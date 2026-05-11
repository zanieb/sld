// TODO
#![allow(unused_variables)]
#![allow(unused)]

use crate::alignment::Alignment;
use crate::alignment::MACHO_PAGE_ALIGNMENT;
use crate::args::ArgumentParser;
use crate::args::CommonArgs;
use crate::args::FILES_PER_GROUP_ENV;
use crate::args::FileWriteMode;
use crate::args::Modifiers;
use crate::args::REFERENCE_LINKER_ENV;
use crate::args::RelocationModel;
use crate::args::VersionMode;
use crate::ensure;
use crate::error::Context;
use crate::error::Result;
use crate::platform;
use crate::platform::Args as _;
use crate::save_dir::SaveDir;
use jobserver::Client;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug)]
pub struct MachOArgs {
    pub(crate) common: super::CommonArgs,

    pub(crate) output: Arc<Path>,
    pub(crate) lib_search_path: Vec<Box<Path>>,
    pub(crate) extra_dylib_paths: Vec<Vec<u8>>,
    pub(crate) sysroot: Option<PathBuf>,
    pub(crate) relocation_model: RelocationModel,
    pub(crate) should_output_executable: bool,
    pub(crate) is_dynamiclib: bool,
    pub(crate) should_adhoc_codesign: bool,
    pub(crate) dead_strip: bool,
    pub(crate) platform_version: MachOPlatformVersion,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct MachOPlatformVersion {
    pub(crate) platform: u32,
    pub(crate) minimum_os: u32,
    pub(crate) sdk: u32,
}

impl MachOArgs {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            common: CommonArgs::from_env()?,
            ..Default::default()
        })
    }
}

impl Default for MachOArgs {
    fn default() -> Self {
        Self {
            common: CommonArgs::default(),

            // TODO: move to CommonArgs
            relocation_model: RelocationModel::NonRelocatable,
            should_output_executable: true,
            is_dynamiclib: false,
            output: Arc::from(Path::new("a.out")),
            lib_search_path: Vec::new(),
            extra_dylib_paths: Vec::new(),
            sysroot: None,
            should_adhoc_codesign: cfg!(target_os = "macos"),
            dead_strip: false,
            platform_version: MachOPlatformVersion {
                platform: object::macho::PLATFORM_MACOS,
                minimum_os: encode_macho_version(11, 0, 0),
                sdk: encode_macho_version(11, 0, 0),
            },
        }
    }
}

struct MachOIncrementalLinkOptions<'a>(&'a MachOArgs);

impl fmt::Debug for MachOIncrementalLinkOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let args = self.0;
        let common = args.common.incremental_link_options();
        f.debug_struct("MachOArgs")
            .field("common", &common)
            .field("output", &args.output)
            .field("relocation_model", &args.relocation_model)
            .finish()
    }
}

impl platform::Args for MachOArgs {
    fn parse<S, I>(&mut self, input: I) -> Result
    where
        S: AsRef<str>,
        I: Iterator<Item = S>,
    {
        parse(self, input)
    }

    fn should_strip_debug(&self) -> bool {
        false
    }

    fn should_strip_all(&self) -> bool {
        false
    }

    fn entry_symbol_name<'a>(&'a self, linker_script_entry: Option<&'a [u8]>) -> &'a [u8] {
        // TODO: probably add option
        b"_main"
    }

    fn lib_search_path(&self) -> &[Box<std::path::Path>] {
        &self.lib_search_path
    }

    fn output(&self) -> &std::sync::Arc<std::path::Path> {
        &self.output
    }

    fn common(&self) -> &crate::args::CommonArgs {
        &self.common
    }

    fn common_mut(&mut self) -> &mut crate::args::CommonArgs {
        &mut self.common
    }

    fn incremental_link_options(&self) -> String {
        format!("{:?}", MachOIncrementalLinkOptions(self))
    }

    fn should_export_all_dynamic_symbols(&self) -> bool {
        false
    }

    fn should_export_dynamic(&self, lib_name: &[u8]) -> bool {
        false
    }

    fn sysroot(&self) -> Option<&Path> {
        self.sysroot.as_deref()
    }

    fn loadable_segment_alignment(&self) -> crate::alignment::Alignment {
        MACHO_PAGE_ALIGNMENT
    }

    fn should_merge_sections(&self) -> bool {
        // TODO
        true
    }

    fn relocation_model(&self) -> crate::args::RelocationModel {
        self.relocation_model
    }

    fn should_output_executable(&self) -> bool {
        self.should_output_executable && !self.is_dynamiclib
    }

    fn should_allow_object_undefined(&self, _output_kind: crate::output_kind::OutputKind) -> bool {
        // Mach-O links against libSystem by default. We currently model undefined external
        // references as libSystem chained imports.
        true
    }
}

impl MachOArgs {
    fn add_dylib_path(&mut self, path: impl Into<Vec<u8>>) {
        let path = path.into();
        if !self
            .extra_dylib_paths
            .iter()
            .any(|existing| existing == &path)
        {
            self.extra_dylib_paths.push(path);
        }
    }

    fn add_framework(&mut self, framework: &str) {
        self.add_dylib_path(framework_dylib_path(framework));
    }

    fn add_linked_library(&mut self, library: &str) -> Result {
        match library {
            "System" | "c" | "m" => {}
            "objc" => self.add_dylib_path(b"/usr/lib/libobjc.A.dylib".to_vec()),
            "iconv" => self.add_dylib_path(b"/usr/lib/libiconv.2.dylib".to_vec()),
            "c++" => self.add_dylib_path(b"/usr/lib/libc++.1.dylib".to_vec()),
            "z" => self.add_dylib_path(b"/usr/lib/libz.1.dylib".to_vec()),
            _ => {
                self.warn_unsupported(&format!("-l{library}"))?;
            }
        }
        Ok(())
    }
}

// Parse the supplied input arguments, which should not include the program name.
pub(crate) fn parse<S: AsRef<str>, I: Iterator<Item = S>>(
    args: &mut MachOArgs,
    mut input: I,
) -> Result {
    let mut modifier_stack = vec![Modifiers::default()];

    let arg_parser = setup_argument_parser();
    while let Some(arg) = input.next() {
        let arg = arg.as_ref();

        if handle_ld64_multi_arg(args, arg, &mut input)? {
            continue;
        }

        arg_parser.handle_argument(args, &mut modifier_stack, arg, &mut input)?;
    }

    if !args.common.unrecognized_options.is_empty() {
        let options_list = args.common.unrecognized_options.join(", ");
        crate::bail!("unrecognized option(s): {}", options_list);
    }

    Ok(())
}

fn setup_argument_parser() -> ArgumentParser<MachOArgs> {
    let mut parser = ArgumentParser::<MachOArgs>::new();

    parser
        .declare_with_param()
        .prefix("L")
        .help("Add directory to library search path")
        .execute(|args, _modifier_stack, value| {
            let path = Path::new(value);
            let dir = args
                .sysroot
                .as_ref()
                .filter(|_| path.is_absolute())
                .and_then(|sysroot| path.strip_prefix("/").ok().map(|p| sysroot.join(p)))
                .unwrap_or_else(|| path.to_owned());
            args.common_mut().save_dir.handle_file(value);
            args.lib_search_path.push(dir.into_boxed_path());
            Ok(())
        });

    parser
        .declare_with_param()
        .prefix("l")
        .help("Link with library")
        .execute(|args, _modifier_stack, value| {
            args.add_linked_library(value)?;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("output")
        .short("o")
        .help("Set the output filename")
        .execute(|args, _modifier_stack, value| {
            args.output = Arc::from(Path::new(value));
            Ok(())
        });
    parser
        .declare_with_optional_param()
        .long("time")
        .help("Show timing information")
        .execute(|args, _modifier_stack, value| {
            args.common.time_phase_options = match value {
                Some(v) => Some(super::parse_time_phase_options(v)?),
                None => Some(Vec::new()),
            };
            Ok(())
        });
    parser
        .declare()
        .long("incremental")
        .help("Enable incremental linking")
        .execute(|args, _modifier_stack| {
            args.common.incremental = true;
            Ok(())
        });
    parser
        .declare()
        .long("no-incremental")
        .help("Disable incremental linking")
        .execute(|args, _modifier_stack| {
            args.common.incremental = false;
            Ok(())
        });
    parser
        .declare_with_param()
        .long("incremental-padding-percent")
        .help("Add this percentage of extra capacity after patchable input sections")
        .execute(|args, _modifier_stack, value| {
            args.common.incremental_padding_percent = value.parse()?;
            Ok(())
        });

    parser
        .declare()
        .long("help")
        .help("Show this help message")
        .execute(|_args, _modifier_stack| {
            use std::io::Write;

            let parser = setup_argument_parser();
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", parser.generate_help())?;
            std::process::exit(0);
        });

    parser
        .declare()
        .long("version")
        .help("Show version information and exit")
        .execute(|args, _modifier_stack| {
            args.common.version_mode = VersionMode::ExitAfterPrint;
            Ok(())
        });

    parser
        .declare()
        .short("v")
        .help("Print version and continue linking")
        .execute(|args, _modifier_stack| {
            args.common.version_mode = VersionMode::Verbose;
            Ok(())
        });

    parser
        .declare()
        .long("demangle")
        .help("Enable symbol demangling")
        .execute(|args, _modifier_stack| {
            args.common.demangle = true;
            Ok(())
        });

    parser
        .declare()
        .long("no_demangle")
        .long("no-demangle")
        .help("Disable symbol demangling")
        .execute(|args, _modifier_stack| {
            args.common.demangle = false;
            Ok(())
        });

    parser
        .declare()
        .long("dynamic")
        .help("Write a dynamic executable")
        .execute(|_args, _modifier_stack| Ok(()));

    parser
        .declare_with_param()
        .long("arch")
        .help("Set target architecture")
        .execute(|_args, _modifier_stack, value| {
            ensure!(
                matches!(value, "arm64" | "aarch64"),
                "Only arm64 Mach-O output is currently supported"
            );
            Ok(())
        });

    parser
        .declare_with_param()
        .long("syslibroot")
        .help("Set SDK root")
        .execute(|args, _modifier_stack, value| {
            args.sysroot = Some(PathBuf::from(value));
            Ok(())
        });

    parser
        .declare_with_param()
        .long("lto_library")
        .help("Set LTO library path")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().save_dir.handle_file(value);
            Ok(())
        });

    parser
        .declare()
        .long("no_deduplicate")
        .help("Disable deduplication")
        .execute(|_args, _modifier_stack| Ok(()));

    parser
        .declare()
        .long("adhoc_codesign")
        .long("adhoc-codesign")
        .help("Ad-hoc sign the output executable")
        .execute(|args, _modifier_stack| {
            args.should_adhoc_codesign = true;
            Ok(())
        });

    parser
        .declare()
        .long("no_adhoc_codesign")
        .long("no-adhoc-codesign")
        .help("Do not ad-hoc sign the output executable")
        .execute(|args, _modifier_stack| {
            args.should_adhoc_codesign = false;
            Ok(())
        });

    parser
        .declare()
        .long("validate-output")
        .execute(|args, _modifier_stack| {
            args.common_mut().validate_output = true;
            Ok(())
        });

    parser
        .declare()
        .long("write-layout")
        .execute(|args, _modifier_stack| {
            args.common_mut().write_layout = true;
            Ok(())
        });

    parser
        .declare()
        .long("write-trace")
        .execute(|args, _modifier_stack| {
            args.common_mut().write_trace = true;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("sym-info")
        .help("Show symbol information. Accepts symbol name or ID.")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().sym_info = Some(value.to_owned());
            Ok(())
        });

    parser
        .declare()
        .long("no-fork")
        .execute(|args, _modifier_stack| {
            args.common_mut().should_fork = false;
            Ok(())
        });

    parser
        .declare()
        .long("update-in-place")
        .execute(|args, _modifier_stack| {
            args.common_mut().file_write_mode = Some(FileWriteMode::UpdateInPlace);
            Ok(())
        });

    parser
        .declare()
        .long("no-update-in-place")
        .execute(|args, _modifier_stack| {
            args.common_mut().file_write_mode = Some(FileWriteMode::UnlinkAndReplace);
            Ok(())
        });

    parser
        .declare_with_param()
        .long("threads")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().num_threads = Some(NonZeroUsize::try_from(value.parse::<usize>()?)?);
            Ok(())
        });

    parser
}

fn handle_ld64_multi_arg<S: AsRef<str>, I: Iterator<Item = S>>(
    args: &mut MachOArgs,
    arg: &str,
    input: &mut I,
) -> Result<bool> {
    if let Some(minimum_os) = arg
        .strip_prefix("-mmacosx-version-min=")
        .or_else(|| arg.strip_prefix("--macosx-version-min="))
    {
        args.platform_version.minimum_os = parse_macho_version(minimum_os)?;
        return Ok(true);
    }

    match arg {
        "-flavor" | "--flavor" => {
            let flavor = input.next().context("-flavor requires an argument")?;
            ensure!(
                matches!(flavor.as_ref(), "darwin" | "ld64"),
                "Mach-O parser cannot handle -flavor {}",
                flavor.as_ref()
            );
            Ok(true)
        }
        "-platform_version" | "--platform_version" => {
            let platform = input
                .next()
                .context("-platform_version requires a platform name")?;
            let minimum_os = input
                .next()
                .context("-platform_version requires a minimum OS version")?;
            let sdk = input
                .next()
                .context("-platform_version requires an SDK version")?;
            args.platform_version = MachOPlatformVersion {
                platform: parse_macho_platform(platform.as_ref())?,
                minimum_os: parse_macho_version(minimum_os.as_ref())?,
                sdk: parse_macho_version(sdk.as_ref())?,
            };
            Ok(true)
        }
        "-macosx_version_min" | "--macosx_version_min" => {
            let minimum_os = input
                .next()
                .context("-macosx_version_min requires a minimum OS version")?;
            args.platform_version.minimum_os = parse_macho_version(minimum_os.as_ref())?;
            Ok(true)
        }
        "-mllvm" | "--mllvm" => {
            input.next().context("-mllvm requires an argument")?;
            Ok(true)
        }
        "-undefined" | "--undefined" => {
            let value = input.next().context("-undefined requires an argument")?;
            args.warn_unsupported(&format!("-undefined {}", value.as_ref()))?;
            Ok(true)
        }
        "-framework" | "--framework" => {
            let framework = input.next().context("-framework requires an argument")?;
            args.add_framework(framework.as_ref());
            Ok(true)
        }
        "-weak_framework" | "--weak_framework" => {
            let framework = input
                .next()
                .context("-weak_framework requires an argument")?;
            args.add_framework(framework.as_ref());
            Ok(true)
        }
        "-dynamiclib" | "--dynamiclib" | "-dylib" | "--dylib" => {
            args.is_dynamiclib = true;
            args.should_output_executable = false;
            Ok(true)
        }
        "-install_name" | "--install_name" => {
            let value = input.next().context("-install_name requires an argument")?;
            args.warn_unsupported(&format!("-install_name {}", value.as_ref()))?;
            Ok(true)
        }
        "-Wl,-exported_symbols_list" => {
            input
                .next()
                .context("-Wl,-exported_symbols_list requires an argument")?;
            Ok(true)
        }
        "-ObjC" | "-nodefaultlibs" => Ok(true),
        "-dead_strip" | "--dead_strip" | "-Wl,-dead_strip" => {
            args.dead_strip = true;
            Ok(true)
        }
        _ if arg.starts_with("-Wl,") => handle_wl_arg(args, arg),
        _ => Ok(false),
    }
}

fn handle_wl_arg(args: &mut MachOArgs, arg: &str) -> Result<bool> {
    let Some(rest) = arg.strip_prefix("-Wl,") else {
        return Ok(false);
    };
    let mut values = rest.split(',');
    while let Some(value) = values.next() {
        match value {
            "-framework" => {
                let framework = values
                    .next()
                    .context("-Wl,-framework requires an argument")?;
                args.add_framework(framework);
            }
            "-weak_framework" => {
                let framework = values
                    .next()
                    .context("-Wl,-weak_framework requires an argument")?;
                args.add_framework(framework);
            }
            _ if value.starts_with("-l") && value.len() > 2 => {
                args.add_linked_library(&value[2..])?
            }
            "-dead_strip" => {
                args.dead_strip = true;
            }
            _ => {}
        }
    }
    Ok(true)
}

fn parse_macho_platform(platform: &str) -> Result<u32> {
    match platform {
        "macos" => Ok(object::macho::PLATFORM_MACOS),
        other => crate::bail!("unsupported Mach-O platform `{other}`"),
    }
}

fn parse_macho_version(version: &str) -> Result<u32> {
    let mut components = version.split('.');
    let major = parse_macho_version_component(version, components.next(), u16::MAX.into())?;
    let minor = parse_macho_version_component(version, components.next(), u8::MAX.into())?;
    let patch = parse_macho_version_component(version, components.next(), u8::MAX.into())?;
    ensure!(
        components.next().is_none(),
        "Mach-O version `{version}` has too many components"
    );
    Ok(encode_macho_version(major, minor, patch))
}

fn parse_macho_version_component(version: &str, component: Option<&str>, max: u32) -> Result<u32> {
    let Some(component) = component else {
        return Ok(0);
    };
    let value = component
        .parse::<u32>()
        .with_context(|| format!("invalid Mach-O version `{version}`"))?;
    ensure!(
        value <= max,
        "Mach-O version `{version}` component `{component}` is too large"
    );
    Ok(value)
}

const fn encode_macho_version(major: u32, minor: u32, patch: u32) -> u32 {
    (major << 16) | (minor << 8) | patch
}

fn framework_dylib_path(framework: &str) -> Vec<u8> {
    let framework = canonical_framework_name(framework);
    let version = match framework {
        "AppKit" | "Foundation" => "C",
        _ => "A",
    };
    format!("/System/Library/Frameworks/{framework}.framework/Versions/{version}/{framework}")
        .into_bytes()
}

fn canonical_framework_name(framework: &str) -> &str {
    match framework {
        "Appkit" | "appkit" => "AppKit",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use crate::platform::Args as _;

    use super::*;

    #[test]
    fn dynamiclib_disables_executable_output() {
        let mut args = MachOArgs::default();

        parse(&mut args, ["-dynamiclib"].into_iter()).unwrap();

        assert!(args.is_dynamiclib);
        assert!(!args.should_output_executable);
        assert!(!args.should_output_executable());
    }
}
