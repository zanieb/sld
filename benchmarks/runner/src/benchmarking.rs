use crate::BatchResult;
use crate::BenchArgs;
use crate::Benchmark;
use crate::BenchmarkResult;
use crate::Benchmarks;
use crate::Bin;
use crate::LinkerKind;
use crate::Result;
use crate::Run;
use crate::config::Config;
use crate::config::Mutation;
use anyhow::Context as _;
use anyhow::bail;
use object::Object as _;
use object::ObjectSection as _;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::Read as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Instant;
use wait4::Wait4 as _;

pub(crate) fn run_bench(args: &BenchArgs, config: &Config) -> Result {
    if !args.allow_non_tmpfs {
        check_tmpfs(args)?;
    }

    let bins = args
        .binaries
        .iter()
        .enumerate()
        .map(|(i, bin_path)| Bin::new(bin_path, i as u32))
        .collect::<Result<Vec<Bin>>>()?;

    let benchmarks = find_benchmarks(args, config)?;

    let benchmarks = filter_benchmarks_by_sld_version(benchmarks, &bins);

    println!("Binaries:");
    for bin in &bins {
        println!("  {bin}");
    }

    println!("Benchmarks:");
    for bench in &benchmarks {
        println!("  {bench}");
    }

    if !args.no_verify {
        verify(&bins, &benchmarks, args)?;
    }

    let results = run(&bins, &benchmarks, args)?;

    let output_path = crate::default_result_path(config, &args.output);

    std::fs::write(&output_path, postcard::to_stdvec(&results)?)
        .with_context(|| format!("Failed to write `{}`", output_path.display()))?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn check_tmpfs(args: &BenchArgs) -> Result {
    let tmpfile = std::path::absolute(&args.tmp)?;
    let tmpdir = tmpfile.parent().unwrap();

    let output = Command::new("stat")
        .arg("-f")
        .arg("-c")
        .arg("%T")
        .arg(tmpdir)
        .output()
        .context("Failed to run `stat`")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if !stdout.contains("tmpfs") {
        bail!(
            "{} uses filesystem {}, but we need tmpfs for reliable benchmarking. \
            Set --tmp to something else or pass --allow-non-tmpfs to ignore",
            tmpdir.display(),
            stdout.trim(),
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn check_tmpfs(_args: &BenchArgs) -> Result {
    Ok(())
}

fn run(bins: &[Bin], benchmarks: &[Benchmark], args: &BenchArgs) -> Result<Benchmarks> {
    let mut out = Vec::new();
    let start = Instant::now();

    for (bench_index, bench) in benchmarks.iter().enumerate() {
        let bench_start = Instant::now();
        let message = format!(
            "Benchmark {} of {}: {bench}",
            bench_index + 1,
            benchmarks.len()
        );

        let progress_bar = indicatif::ProgressBar::new(
            (args.num_batches * args.batch_size * bins.len() as u32) as u64,
        )
        .with_style(indicatif::ProgressStyle::with_template(
            "{msg} {spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}]",
        )?)
        .with_message(message.clone());

        if bins.is_empty() {
            bail!("Need at least one binary");
        }
        let mut baseline_outputs = Vec::new();
        for bin in bins {
            let warmup_flags = extra_flags_for_run(bin, bench, false);
            let warmup_run = run_once(bin, bench, args, &warmup_flags, false, false)?;
            let baseline_output = if bench.config.expect_output_change && warmup_run.is_some() {
                let output_path = output_path_for_bin(args.tmp.as_path(), bin);
                Some(std::fs::read(&output_path).with_context(|| {
                    format!(
                        "Failed to read warmup output `{}` for output-change verification",
                        output_path.display()
                    )
                })?)
            } else {
                None
            };
            baseline_outputs.push(baseline_output);
        }

        let mut bench_results = Vec::new();
        for batch_num in 0..args.num_batches {
            let mut batch_runs = vec![Vec::new(); bins.len()];
            for group in timed_run_groups(bins.len(), args.batch_size) {
                mutate_inputs(bench)?;
                for bin_index in group.bin_indexes {
                    let bin = &bins[bin_index];
                    let measure_memory = !args.no_mem && batch_num == 0;
                    let extra_flags = extra_flags_for_run(bin, bench, measure_memory);

                    if let Some(run) =
                        run_once(bin, bench, args, &extra_flags, true, measure_memory)?
                    {
                        if let Some(baseline_output) = baseline_outputs
                            .get(bin_index)
                            .and_then(|baseline| baseline.as_deref())
                        {
                            verify_output_changed(bin, bench, args, baseline_output)?;
                        }
                        batch_runs[bin_index].push(run);
                    }
                    progress_bar.inc(1);
                }
            }
            for (bin, runs) in bins.iter().zip(batch_runs) {
                bench_results.push(BatchResult {
                    bin: bin.clone(),
                    runs,
                })
            }
        }
        bench_results.sort_by_key(|b| b.bin.index);
        let r = BenchmarkResult {
            config: bench.clone(),
            batches: bench_results,
        };
        out.push(r);
        progress_bar.finish_and_clear();
        println!("{message}: done in {} s", bench_start.elapsed().as_secs());
    }

    let elapsed = start.elapsed();
    println!(
        "All done in {}h {}m {}s",
        elapsed.as_secs() / 3600,
        (elapsed.as_secs() / 60) % 60,
        elapsed.as_secs() % 60
    );

    Ok(Benchmarks { benchmarks: out })
}

#[derive(Debug, PartialEq, Eq)]
struct TimedRunGroup {
    bin_indexes: Vec<usize>,
}

fn timed_run_groups(num_bins: usize, batch_size: u32) -> Vec<TimedRunGroup> {
    (0..batch_size)
        .map(|_| TimedRunGroup {
            bin_indexes: (0..num_bins).collect(),
        })
        .collect()
}

fn extra_flags_for_run(bin: &Bin, bench: &Benchmark, measure_memory: bool) -> Vec<String> {
    let mut extra_flags = bench.config.extra_flags.clone();
    if bin.identifier.kind == LinkerKind::Sld {
        extra_flags.extend(bench.config.sld_extra_flags.clone());
    }
    if measure_memory {
        extra_flags.push("--no-fork".to_owned());
    }
    extra_flags
}

fn mutate_inputs(bench: &Benchmark) -> Result {
    if bench.config.mutate_files.is_empty() {
        return Ok(());
    }

    let save_dir = bench
        .path
        .parent()
        .with_context(|| format!("Benchmark path `{}` has no parent", bench.path.display()))?;

    for mutation in &bench.config.mutate_files {
        match mutation {
            Mutation::AppendZero(relative_path) => {
                ensure_relative_path(relative_path)?;
                append_zero(&save_dir.join(relative_path))?;
            }
            Mutation::ElfSection {
                path: relative_path,
                section,
                grow,
            } => {
                ensure_relative_path(relative_path)?;
                mutate_elf_section(&save_dir.join(relative_path), section, *grow)?;
            }
            Mutation::FirstElfSection { section, grow } => {
                let (path, section) = find_first_relocatable_elf_with_section(save_dir, section)?;
                mutate_elf_section(&path, &section, *grow)?;
            }
        }
    }

    Ok(())
}

fn mutate_elf_section(path: &Path, section: &str, grow: u64) -> Result {
    if grow == 0 {
        mutate_elf_section_byte(path, section)
    } else {
        grow_elf_section(path, section, grow)
    }
}

fn find_first_relocatable_elf_with_section(
    save_dir: &Path,
    section_selector: &str,
) -> Result<(std::path::PathBuf, String)> {
    let mut dirs = VecDeque::from([save_dir.to_owned()]);

    while let Some(dir) = dirs.pop_front() {
        let mut entries = std::fs::read_dir(&dir)
            .with_context(|| format!("Failed to read benchmark save-dir `{}`", dir.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("Failed to read benchmark save-dir `{}`", dir.display()))?;
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().with_context(|| {
                format!(
                    "Failed to read benchmark save-dir entry `{}`",
                    path.display()
                )
            })?;
            if file_type.is_dir() {
                if entry.file_name().to_string_lossy().ends_with(".incr") {
                    continue;
                }
                dirs.push_back(path);
                continue;
            }
            if file_type.is_file()
                && let Some(section) = relocatable_elf_section_name(&path, section_selector)?
            {
                return Ok((path, section));
            }
        }
    }

    bail!(
        "Could not find a relocatable ELF input with section `{section_selector}` under `{}`",
        save_dir.display()
    )
}

fn relocatable_elf_section_name(path: &Path, section_selector: &str) -> Result<Option<String>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read benchmark input `{}`", path.display()))?;
    let Ok(object) = object::File::parse(&*bytes) else {
        return Ok(None);
    };
    if object.kind() != object::ObjectKind::Relocatable {
        return Ok(None);
    }
    for section in object.sections() {
        let Ok(name) = section.name() else {
            continue;
        };
        if !section_selector_matches(section_selector, name) {
            continue;
        }
        let Some((_, size)) = section.file_range() else {
            continue;
        };
        if size > 0 {
            return Ok(Some(name.to_owned()));
        }
    }
    Ok(None)
}

fn section_selector_matches(selector: &str, section_name: &str) -> bool {
    selector
        .strip_suffix('*')
        .map_or(section_name == selector, |prefix| {
            section_name.starts_with(prefix)
        })
}

fn append_zero(path: &Path) -> Result {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open mutation input `{}`", path.display()))?;
    file.write_all(&[0])
        .with_context(|| format!("Failed to mutate input `{}`", path.display()))?;
    Ok(())
}

fn mutate_elf_section_byte(path: &Path, section_name: &str) -> Result {
    let mut bytes =
        std::fs::read(path).with_context(|| format!("Failed to read `{}`", path.display()))?;
    let object = object::File::parse(&*bytes)
        .with_context(|| format!("Failed to parse ELF mutation input `{}`", path.display()))?;
    let section = object.section_by_name(section_name).with_context(|| {
        format!(
            "Mutation input `{}` does not contain section `{section_name}`",
            path.display()
        )
    })?;
    let (start, size) = section.file_range().with_context(|| {
        format!(
            "Mutation section `{section_name}` in `{}` has no file range",
            path.display()
        )
    })?;
    if size == 0 {
        bail!(
            "Mutation section `{section_name}` in `{}` is empty",
            path.display()
        );
    }
    let byte = bytes
        .get_mut(start as usize)
        .with_context(|| format!("Mutation section `{section_name}` starts past end of file"))?;
    *byte = byte.wrapping_add(1);
    std::fs::write(path, bytes)
        .with_context(|| format!("Failed to write mutation input `{}`", path.display()))?;
    Ok(())
}

fn grow_elf_section(path: &Path, section_name: &str, growth: u64) -> Result {
    if growth == 0 {
        bail!("ELF section growth mutation must grow by at least one byte");
    }

    let mut bytes =
        std::fs::read(path).with_context(|| format!("Failed to read `{}`", path.display()))?;
    let (offset, size, limit, size_field) = {
        let object = object::File::parse(&*bytes)
            .with_context(|| format!("Failed to parse ELF mutation input `{}`", path.display()))?;
        let section = object.section_by_name(section_name).with_context(|| {
            format!(
                "Mutation input `{}` does not contain section `{section_name}`",
                path.display()
            )
        })?;
        let (offset, size) = section.file_range().with_context(|| {
            format!(
                "Mutation section `{section_name}` in `{}` has no file range",
                path.display()
            )
        })?;
        let section_index = section.index().0;
        let size_field = elf_section_size_field(&bytes, section_index).with_context(|| {
            format!(
                "Mutation section `{section_name}` in `{}` has no ELF size field",
                path.display()
            )
        })?;
        let limit = section_growth_limit(&object, section_index, offset, bytes.len());
        (offset, size, limit, size_field)
    };

    let new_size = size
        .checked_add(growth)
        .context("ELF section growth mutation overflowed section size")?;
    let new_end_offset = offset
        .checked_add(new_size)
        .context("ELF section growth mutation overflowed file offset")?;
    if new_end_offset > limit {
        bail!(
            "Mutation section `{section_name}` in `{}` cannot grow by {growth} bytes without moving later object data",
            path.display()
        );
    }

    let start = usize::try_from(offset).context("ELF section offset is too large")?;
    let old_end = start
        .checked_add(usize::try_from(size).context("ELF section size is too large")?)
        .context("ELF section end offset overflowed")?;
    let new_end = start
        .checked_add(usize::try_from(new_size).context("ELF section size is too large")?)
        .context("ELF section end offset overflowed")?;
    for (index, byte) in bytes[old_end..new_end].iter_mut().enumerate() {
        *byte = 0x80_u8.wrapping_add(index as u8);
    }
    size_field.write(&mut bytes, new_size)?;

    std::fs::write(path, bytes)
        .with_context(|| format!("Failed to write mutation input `{}`", path.display()))?;
    Ok(())
}

#[derive(Clone)]
struct ElfSizeField {
    range: std::ops::Range<usize>,
    width: usize,
}

impl ElfSizeField {
    fn write(self, bytes: &mut [u8], value: u64) -> Result {
        match self.width {
            4 => {
                let value = u32::try_from(value).context("ELF32 section size overflow")?;
                bytes[self.range].copy_from_slice(&value.to_le_bytes());
            }
            8 => bytes[self.range].copy_from_slice(&value.to_le_bytes()),
            _ => bail!("Unsupported ELF section size width {}", self.width),
        }
        Ok(())
    }
}

fn section_growth_limit(
    object: &object::File<'_>,
    section_index: usize,
    offset: u64,
    file_len: usize,
) -> u64 {
    let mut limit = elf_section_table_offset(object).unwrap_or(file_len as u64);
    for section in object.sections() {
        if section.index().0 == section_index {
            continue;
        }
        let Some((next_offset, _)) = section.file_range() else {
            continue;
        };
        if next_offset > offset {
            limit = limit.min(next_offset);
        }
    }
    limit
}

fn elf_section_table_offset(object: &object::File<'_>) -> Option<u64> {
    match object {
        object::File::Elf32(file) => Some(u64::from(file.elf_header().e_shoff.get(file.endian()))),
        object::File::Elf64(file) => Some(file.elf_header().e_shoff.get(file.endian())),
        _ => None,
    }
}

fn elf_section_size_field(bytes: &[u8], section_index: usize) -> Option<ElfSizeField> {
    if bytes.len() < 0x34 || bytes.get(0..4)? != b"\x7fELF" || *bytes.get(5)? != 1 {
        return None;
    }

    match *bytes.get(4)? {
        1 => {
            let section_header_offset = read_u32_le(bytes.get(0x20..0x24)?)? as usize;
            let section_header_size = read_u16_le(bytes.get(0x2e..0x30)?)? as usize;
            let section_count = read_u16_le(bytes.get(0x30..0x32)?)? as usize;
            elf_section_header_field(
                bytes,
                section_index,
                section_header_offset,
                section_header_size,
                section_count,
                0x14,
                4,
            )
        }
        2 => {
            if bytes.len() < 0x40 {
                return None;
            }
            let section_header_offset = read_u64_le(bytes.get(0x28..0x30)?)? as usize;
            let section_header_size = read_u16_le(bytes.get(0x3a..0x3c)?)? as usize;
            let section_count = read_u16_le(bytes.get(0x3c..0x3e)?)? as usize;
            elf_section_header_field(
                bytes,
                section_index,
                section_header_offset,
                section_header_size,
                section_count,
                0x20,
                8,
            )
        }
        _ => None,
    }
}

fn elf_section_header_field(
    bytes: &[u8],
    section_index: usize,
    section_header_offset: usize,
    section_header_size: usize,
    section_count: usize,
    field_offset: usize,
    field_size: usize,
) -> Option<ElfSizeField> {
    if section_index >= section_count || section_header_size < field_offset + field_size {
        return None;
    }
    let section_start =
        section_header_offset.checked_add(section_index.checked_mul(section_header_size)?)?;
    let field_start = section_start.checked_add(field_offset)?;
    let field_end = field_start.checked_add(field_size)?;
    (field_end <= bytes.len()).then_some(ElfSizeField {
        range: field_start..field_end,
        width: field_size,
    })
}

fn read_u16_le(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u32_le(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u64_le(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn ensure_relative_path(path: &str) -> Result {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!(
            "Benchmark mutation paths must be relative to the save-dir: `{}`",
            path.display()
        );
    }
    Ok(())
}

/// Runs each benchmark once with each linker.
fn verify(bins: &[Bin], benchmarks: &[Benchmark], args: &BenchArgs) -> Result {
    let mut success = true;
    for bench in benchmarks {
        println!("Verifying: {bench}");
        for bin in bins {
            if let Err(error) = run_once(bin, bench, args, &[], false, false) {
                eprintln!("{error}");
                success = false;
            }
        }
    }

    if !success {
        bail!("One or more benchmark/linker combinations failed");
    }

    Ok(())
}

fn run_once(
    bin: &Bin,
    bench: &Benchmark,
    args: &BenchArgs,
    extra_flags: &[String],
    check_sld_log: bool,
    measure_memory: bool,
) -> Result<Option<Run>> {
    if !bench.supports_bin(bin) {
        return Ok(None);
    }

    let output_path = output_path_for_bin(args.tmp.as_path(), bin);
    let linker = linker_invocation(args, bin)?;
    let mut command = Command::new(&bench.path);
    command
        .env("OUT", output_path.as_os_str())
        .arg(&linker.path);
    for (key, value) in linker.env {
        command.env(key, value);
    }
    for arg in extra_flags {
        if bin.identifier.kind.supports_arg(arg) {
            command.arg(arg);
        }
    }

    let (mut pipe_read, pipe_write) = std::io::pipe()?;
    command
        .stderr(pipe_write.try_clone()?)
        .stdout(pipe_write)
        .stdin(Stdio::null());

    let start = Instant::now();

    let mut child = command
        .spawn()
        .with_context(|| format!("Failed to run {command:?}"))?;

    // Ensure we're not holding any copies of the write-end of the pipe in the parent process,
    // otherwise the read below won't terminate.
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());

    let mut text_out = String::new();
    pipe_read.read_to_string(&mut text_out)?;

    let pid = child.id();

    let res_use = child.wait4()?;

    let elapsed = start.elapsed();

    if !res_use.status.success() {
        bail!("Error returned from {command:?}\n{text_out}",)
    }

    // Make sure that the linker runs without warning. Specifically what we care about is that the
    // linker is being invoked without any flags that it doesn't properly support, since that might
    // be unfair to other linkers that do support that option.
    if text_out.contains("WARN") {
        bail!("Command produced warnings: {command:?}\n{text_out}");
    }

    if should_verify_sld_incremental_log(bin, bench, check_sld_log) {
        verify_sld_incremental_log(&output_path, &bench.config.expect_sld_log)?;
    }

    // However long we took to run, sleep for half of that. If the linker forked on startup, then
    // this gives the subprocess a chance to shutdown in the background before we run the next
    // command.
    std::thread::sleep(elapsed / 2);

    Ok(Some(Run {
        pid,
        extra_flags: extra_flags.to_vec(),
        measure_memory,
        elapsed,
        max_rss: res_use.rusage.maxrss,
        stime: res_use.rusage.stime,
        utime: res_use.rusage.utime,
    }))
}

struct LinkerInvocation {
    path: PathBuf,
    env: Vec<(&'static str, OsString)>,
}

fn linker_invocation(args: &BenchArgs, bin: &Bin) -> Result<LinkerInvocation> {
    if bin.identifier.kind == LinkerKind::AppleClang {
        let path = apple_clang_wrapper_path(args, bin);
        ensure_apple_clang_wrapper(&path)?;
        return Ok(LinkerInvocation {
            path,
            env: vec![("SLD_BENCH_REAL_LINKER", bin.path.as_os_str().to_owned())],
        });
    }

    Ok(LinkerInvocation {
        path: bin.path.clone(),
        env: Vec::new(),
    })
}

fn apple_clang_wrapper_path(args: &BenchArgs, bin: &Bin) -> PathBuf {
    let mut path = args.tmp.clone();
    let mut file_name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("linker-benchmark-out"));
    file_name.push(format!(".apple-clang-wrapper.{}", bin.index));
    path.set_file_name(file_name);
    path
}

fn ensure_apple_clang_wrapper(path: &Path) -> Result {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create `{}`", parent.display()))?;
    }

    std::fs::write(path, APPLE_CLANG_WRAPPER)
        .with_context(|| format!("Failed to write `{}`", path.display()))?;
    make_executable(path)
}

const APPLE_CLANG_WRAPPER: &str = r#"#!/usr/bin/env bash
set -euo pipefail

if [ -z "${SLD_BENCH_REAL_LINKER:-}" ]; then
  echo "SLD_BENCH_REAL_LINKER is not set" >&2
  exit 1
fi

ARGS=()
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-flavor" ] && [ "${2:-}" = "darwin" ]; then
    shift 2
    continue
  fi

  if [ "$1" = "--no-fork" ]; then
    shift
    continue
  fi

  ARGS+=("$1")
  shift
done

exec "$SLD_BENCH_REAL_LINKER" "${ARGS[@]}"
"#;

#[cfg(unix)]
fn make_executable(path: &Path) -> Result {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = std::fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for `{}`", path.display()))?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("Failed to chmod `{}`", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result {
    Ok(())
}

fn output_path_for_bin(tmp: &Path, bin: &Bin) -> std::path::PathBuf {
    let suffix = format!(".{}", bin.index);
    let mut path = tmp.to_owned();
    let mut file_name = path
        .file_name()
        .map(|name| name.to_owned())
        .unwrap_or_else(|| "linker-benchmark-out".into());
    file_name.push(suffix);
    path.set_file_name(file_name);
    path
}

fn should_verify_sld_incremental_log(bin: &Bin, bench: &Benchmark, check_sld_log: bool) -> bool {
    check_sld_log
        && bin.identifier.kind == LinkerKind::Sld
        && !bench.config.expect_sld_log.is_empty()
}

fn verify_sld_incremental_log(output_path: &Path, expected: &[String]) -> Result {
    let path = incremental_log_path(output_path);
    let log = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read sld incremental log `{}`", path.display()))?;
    for expected in expected {
        if !log.contains(expected) {
            bail!(
                "sld incremental log `{}` did not contain expected text `{expected}`.\nLog:\n{log}",
                path.display()
            );
        }
    }
    Ok(())
}

fn verify_output_changed(
    bin: &Bin,
    bench: &Benchmark,
    args: &BenchArgs,
    baseline_output: &[u8],
) -> Result {
    let output_path = output_path_for_bin(args.tmp.as_path(), bin);
    let output = std::fs::read(&output_path).with_context(|| {
        format!(
            "Failed to read output `{}` for output-change verification",
            output_path.display()
        )
    })?;
    if output == baseline_output {
        bail!(
            "Benchmark `{}` with linker `{}` did not change output after mutation",
            bench.name,
            bin.identifier.kind
        );
    }
    Ok(())
}

fn incremental_log_path(output_path: &Path) -> std::path::PathBuf {
    let mut state_dir = output_path.as_os_str().to_owned();
    state_dir.push(".incr");
    std::path::PathBuf::from(state_dir).join("log")
}

fn find_benchmarks(args: &BenchArgs, config: &Config) -> Result<Vec<Benchmark>> {
    let dir = args.saves.as_path();

    let mut benchmarks = Vec::new();

    let mut available: BTreeSet<String> = std::fs::read_dir(dir)
        .with_context(|| format!("Save dir doesn't exist `{}`", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_owned()))
        .collect();

    for (name, config) in &config.benches {
        let save_name = config.save.as_deref().unwrap_or(name);
        available.remove(save_name);
        if !config.skip {
            benchmarks.push(Benchmark::new(
                name.clone(),
                dir.join(save_name),
                config.clone(),
            )?);
        }
    }

    if !available.is_empty() {
        let mut config_snippet = String::new();
        for a in available {
            config_snippet += &format!("[bench.{a}]\n\n");
        }
        bail!("Config doesn't list some benchmarks. Please add:\n{config_snippet}");
    }

    if !args.benches.is_empty() {
        let keep: HashSet<&str> = args.benches.iter().map(|n| n.as_str()).collect();
        benchmarks.retain(|b| keep.contains(b.name.as_str()));
    }

    Ok(benchmarks)
}

/// Filter benchmarks to just those that have at least one supported sld version.
fn filter_benchmarks_by_sld_version(benchmarks: Vec<Benchmark>, bins: &[Bin]) -> Vec<Benchmark> {
    let Some(maximum_sld_version) = bins
        .iter()
        .filter(|&bin| bin.identifier.kind == LinkerKind::Sld)
        .map(|bin| &bin.identifier.effective_version)
        .max()
    else {
        return benchmarks;
    };

    benchmarks
        .into_iter()
        .filter(|bench| {
            if !bench.supports_sld_version(maximum_sld_version) {
                println!("Skipping benchmark {bench} due to minimum version requirement");
                false
            } else {
                true
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::ensure_apple_clang_wrapper;
    use super::ensure_relative_path;
    use super::find_first_relocatable_elf_with_section;
    use super::grow_elf_section;
    use super::incremental_log_path;
    use super::make_executable;
    use super::mutate_elf_section_byte;
    use super::mutate_inputs;
    use super::output_path_for_bin;
    use super::should_verify_sld_incremental_log;
    use super::verify_output_changed;
    use super::verify_sld_incremental_log;
    use crate::BenchArgs;
    use crate::Benchmark;
    use crate::Bin;
    use crate::LinkerIdentifier;
    use crate::LinkerKind;
    use crate::config::BenchConfig;
    use crate::config::Mutation;
    use object::Object as _;
    use object::ObjectSection as _;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn mutation_paths_must_be_save_dir_relative() {
        assert!(ensure_relative_path("objects/main.o").is_ok());
        assert!(ensure_relative_path("../main.o").is_err());
        assert!(ensure_relative_path("/tmp/main.o").is_err());
    }

    #[test]
    fn append_zero_mutation_changes_configured_input() {
        let dir = tempfile::tempdir().unwrap();
        let save_dir = dir.path().join("save");
        std::fs::create_dir(&save_dir).unwrap();
        let input = save_dir.join("changed.o");
        std::fs::write(&input, b"abc").unwrap();
        let bench = Benchmark {
            name: "append".to_owned(),
            path: save_dir.join("run-with"),
            config: BenchConfig {
                mutate_files: vec![Mutation::AppendZero("changed.o".to_owned())],
                ..BenchConfig::default()
            },
        };

        mutate_inputs(&bench).unwrap();

        assert_eq!(std::fs::read(&input).unwrap(), b"abc\0");
    }

    #[test]
    fn elf_section_byte_mutation_changes_section_contents() {
        let Ok(current_exe) = std::env::current_exe() else {
            return;
        };
        let Ok(bytes) = std::fs::read(&current_exe) else {
            return;
        };
        let Ok(object) = object::File::parse(&*bytes) else {
            return;
        };
        if object.section_by_name(".data").is_none() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current-exe");
        std::fs::write(&path, &bytes).unwrap();

        mutate_elf_section_byte(&path, ".data").unwrap();

        assert_ne!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn elf_section_byte_mutation_does_not_toggle_to_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("changed.o");
        std::fs::write(&path, growable_data_elf()).unwrap();

        mutate_elf_section_byte(&path, ".data").unwrap();
        mutate_elf_section_byte(&path, ".data").unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let object = object::File::parse(&*bytes).unwrap();
        assert_eq!(
            object.section_by_name(".data").unwrap().data().unwrap(),
            &[3, 2, 3, 4]
        );
    }

    #[test]
    fn elf_section_growth_mutation_grows_section_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growable.o");
        std::fs::write(&path, growable_data_elf()).unwrap();

        grow_elf_section(&path, ".data", 1).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let object = object::File::parse(&*bytes).unwrap();
        let data = object.section_by_name(".data").unwrap().data().unwrap();
        assert_eq!(data, &[1, 2, 3, 4, 0x80]);
    }

    #[test]
    fn elf_section_growth_mutation_changes_configured_input() {
        let dir = tempfile::tempdir().unwrap();
        let save_dir = dir.path().join("save");
        std::fs::create_dir(&save_dir).unwrap();
        let input = save_dir.join("changed.o");
        std::fs::write(&input, growable_data_elf()).unwrap();
        let bench = Benchmark {
            name: "grow".to_owned(),
            path: save_dir.join("run-with"),
            config: BenchConfig {
                mutate_files: vec![Mutation::ElfSection {
                    path: "changed.o".to_owned(),
                    section: ".data".to_owned(),
                    grow: 1,
                }],
                ..BenchConfig::default()
            },
        };

        mutate_inputs(&bench).unwrap();

        let bytes = std::fs::read(&input).unwrap();
        let object = object::File::parse(&*bytes).unwrap();
        assert_eq!(
            object.section_by_name(".data").unwrap().data().unwrap(),
            &[1, 2, 3, 4, 0x80]
        );
    }

    #[test]
    fn first_elf_section_mutation_finds_deterministic_input() {
        let dir = tempfile::tempdir().unwrap();
        let save_dir = dir.path().join("save");
        let nested = save_dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(save_dir.join("run-with"), b"#!/bin/sh\n").unwrap();
        std::fs::write(save_dir.join("not-object"), b"abc").unwrap();
        let input = nested.join("changed.o");
        std::fs::write(&input, growable_data_elf()).unwrap();
        let bench = Benchmark {
            name: "first-elf-section".to_owned(),
            path: save_dir.join("run-with"),
            config: BenchConfig {
                mutate_files: vec![Mutation::FirstElfSection {
                    section: ".data".to_owned(),
                    grow: 0,
                }],
                ..BenchConfig::default()
            },
        };

        assert_eq!(
            find_first_relocatable_elf_with_section(&save_dir, ".data").unwrap(),
            (input, ".data".to_owned())
        );

        mutate_inputs(&bench).unwrap();

        let bytes = std::fs::read(nested.join("changed.o")).unwrap();
        let object = object::File::parse(&*bytes).unwrap();
        assert_eq!(
            object.section_by_name(".data").unwrap().data().unwrap(),
            &[2, 2, 3, 4]
        );
    }

    #[test]
    fn first_elf_section_mutation_can_match_section_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let save_dir = dir.path().join("save");
        std::fs::create_dir_all(&save_dir).unwrap();
        let input = save_dir.join("changed.o");
        std::fs::write(&input, growable_data_elf()).unwrap();

        assert_eq!(
            find_first_relocatable_elf_with_section(&save_dir, ".dat*").unwrap(),
            (input, ".data".to_owned())
        );
    }

    #[test]
    fn sld_incremental_log_expectations_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        let log_path = incremental_log_path(&output);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(
            &log_path,
            "full relink: no previous incremental state\npatched 1 changed input file before loading inputs\n",
        )
        .unwrap();

        verify_sld_incremental_log(
            &output,
            &[
                "patched 1 changed input file".to_owned(),
                "before loading inputs".to_owned(),
            ],
        )
        .unwrap();

        let error = verify_sld_incremental_log(&output, &["reused existing output".to_owned()])
            .unwrap_err();

        assert!(error.to_string().contains("did not contain expected text"));
    }

    #[test]
    fn sld_incremental_log_expectations_skip_warmup() {
        let sld = Bin {
            index: 0,
            path: PathBuf::from("/bin/sld"),
            identifier: LinkerIdentifier {
                kind: LinkerKind::Sld,
                version: "sld 0.0.0".to_owned(),
                variant: None,
                hash: None,
                effective_version: vec![0, 0, 0],
            },
        };
        let mold = Bin {
            index: 1,
            path: PathBuf::from("/bin/mold"),
            identifier: LinkerIdentifier {
                kind: LinkerKind::Mold,
                version: "mold 0.0.0".to_owned(),
                variant: None,
                hash: None,
                effective_version: vec![0, 0, 0],
            },
        };
        let bench = Benchmark {
            name: "incremental".to_owned(),
            path: PathBuf::from("/tmp/save/run-with"),
            config: BenchConfig {
                expect_sld_log: vec!["reused existing output".to_owned()],
                ..BenchConfig::default()
            },
        };

        assert!(!should_verify_sld_incremental_log(&sld, &bench, false));
        assert!(should_verify_sld_incremental_log(&sld, &bench, true));
        assert!(!should_verify_sld_incremental_log(&mold, &bench, true));
    }

    #[test]
    fn output_change_expectation_requires_changed_output() {
        let dir = tempfile::tempdir().unwrap();
        let args = BenchArgs {
            config: PathBuf::from("benchmarks/test.toml"),
            saves: dir.path().join("saves"),
            no_verify: false,
            no_check_system: true,
            allow_non_tmpfs: true,
            tmp: dir.path().join("out"),
            batch_size: 1,
            num_batches: 1,
            no_mem: true,
            benches: Vec::new(),
            output: None,
            binaries: Vec::new(),
        };
        let bin = Bin {
            index: 0,
            path: PathBuf::from("/bin/sld"),
            identifier: LinkerIdentifier {
                kind: LinkerKind::Sld,
                version: "sld 0.0.0".to_owned(),
                variant: None,
                hash: None,
                effective_version: vec![0, 0, 0],
            },
        };
        let bench = Benchmark {
            name: "changed-incremental".to_owned(),
            path: PathBuf::from("/tmp/save/run-with"),
            config: BenchConfig {
                expect_output_change: true,
                ..BenchConfig::default()
            },
        };
        let output = output_path_for_bin(args.tmp.as_path(), &bin);

        std::fs::write(&output, b"baseline").unwrap();
        let error = verify_output_changed(&bin, &bench, &args, b"baseline").unwrap_err();
        assert!(error.to_string().contains("did not change output"));

        std::fs::write(&output, b"changed").unwrap();
        verify_output_changed(&bin, &bench, &args, b"baseline").unwrap();
    }

    #[test]
    fn benchmark_output_paths_are_isolated_by_linker() {
        let bin = Bin {
            index: 7,
            path: PathBuf::from("/bin/sld"),
            identifier: LinkerIdentifier {
                kind: LinkerKind::Sld,
                version: "sld 0.0.0".to_owned(),
                variant: None,
                hash: None,
                effective_version: vec![0, 0, 0],
            },
        };

        assert_eq!(
            output_path_for_bin(Path::new("/tmp/linker-benchmark-out"), &bin),
            PathBuf::from("/tmp/linker-benchmark-out.7")
        );
    }

    #[cfg(unix)]
    #[test]
    fn apple_clang_wrapper_strips_sld_darwin_flavor() {
        let dir = tempfile::tempdir().unwrap();
        let real_linker = dir.path().join("real-linker");
        std::fs::write(
            &real_linker,
            "#!/usr/bin/env bash\nfor ARG in \"$@\"; do printf '<%s>\\n' \"$ARG\"; done\n",
        )
        .unwrap();
        make_executable(&real_linker).unwrap();

        let wrapper = dir.path().join("apple-clang-wrapper");
        ensure_apple_clang_wrapper(&wrapper).unwrap();

        let output = Command::new(&wrapper)
            .env("SLD_BENCH_REAL_LINKER", &real_linker)
            .args([
                "input.o",
                "-flavor",
                "darwin",
                "--no-fork",
                "-flavor",
                "gnu",
            ])
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "<input.o>\n<-flavor>\n<gnu>\n"
        );
    }

    #[test]
    fn timed_run_groups_share_each_mutation_across_bins() {
        assert_eq!(
            super::timed_run_groups(3, 2),
            [
                super::TimedRunGroup {
                    bin_indexes: vec![0, 1, 2]
                },
                super::TimedRunGroup {
                    bin_indexes: vec![0, 1, 2]
                },
            ]
        );
    }

    fn growable_data_elf() -> Vec<u8> {
        let mut bytes = vec![0; 0x140];

        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&1_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&0x80_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64_u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&3_u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&2_u16.to_le_bytes());

        bytes[0x40..0x44].copy_from_slice(&[1, 2, 3, 4]);
        bytes[0x48..0x59].copy_from_slice(b"\0.data\0.shstrtab\0");

        let data_header = 0x80 + 64;
        bytes[data_header..data_header + 4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[data_header + 4..data_header + 8].copy_from_slice(&1_u32.to_le_bytes());
        bytes[data_header + 8..data_header + 16].copy_from_slice(&3_u64.to_le_bytes());
        bytes[data_header + 24..data_header + 32].copy_from_slice(&0x40_u64.to_le_bytes());
        bytes[data_header + 32..data_header + 40].copy_from_slice(&4_u64.to_le_bytes());
        bytes[data_header + 48..data_header + 56].copy_from_slice(&8_u64.to_le_bytes());

        let shstrtab_header = 0x80 + 128;
        bytes[shstrtab_header..shstrtab_header + 4].copy_from_slice(&7_u32.to_le_bytes());
        bytes[shstrtab_header + 4..shstrtab_header + 8].copy_from_slice(&3_u32.to_le_bytes());
        bytes[shstrtab_header + 24..shstrtab_header + 32].copy_from_slice(&0x48_u64.to_le_bytes());
        bytes[shstrtab_header + 32..shstrtab_header + 40].copy_from_slice(&17_u64.to_le_bytes());
        bytes[shstrtab_header + 48..shstrtab_header + 56].copy_from_slice(&1_u64.to_le_bytes());

        bytes
    }
}
