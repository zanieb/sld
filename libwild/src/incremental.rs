use crate::error::Context as _;
use crate::error::Result;
use crate::input_data::FileLoader;
use crate::input_data::InputRef;
use crate::platform;
use crate::timing_phase;
use hashbrown::HashMap;
use hashbrown::HashSet;
use memmap2::MmapOptions;
use object::Object as _;
use object::ObjectSection as _;
use object::ObjectSymbol as _;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const STATE_VERSION: &str = "wild-incremental-state-v11";
const STATE_VERSION_V10: &str = "wild-incremental-state-v10";
const STATE_VERSION_V9: &str = "wild-incremental-state-v9";
const STATE_VERSION_V8: &str = "wild-incremental-state-v8";
const STATE_VERSION_V7: &str = "wild-incremental-state-v7";
const STATE_VERSION_V6: &str = "wild-incremental-state-v6";
const STATE_VERSION_V5: &str = "wild-incremental-state-v5";
const STATE_VERSION_V4: &str = "wild-incremental-state-v4";
const STATE_VERSION_V3: &str = "wild-incremental-state-v3";
const STATE_VERSION_V2: &str = "wild-incremental-state-v2";
const STATE_VERSION_V1: &str = "wild-incremental-state-v1";
const INDEX_FILE: &str = "index";
const LOG_FILE: &str = "log";
const GLOBAL_LOG_FILE: &str = "incremental.log";
const USER_STATE_DIR_ENV: &str = "WILD_STATE_DIR";
const INPUT_SNAPSHOT_DIR: &str = "input-files";
const BUILD_ID_HASH_FILE: &str = "build-id-hash";
const SECTIONS_FILE: &str = "sections";
const BUILD_ID_HASH_GROUP_CHUNKS: usize = 64;
const BUILD_ID_HASH_GROUP_LEN: usize = blake3::CHUNK_LEN * BUILD_ID_HASH_GROUP_CHUNKS;
const ABSENT_FIELD: &str = "-";

pub(crate) struct PreparedState {
    mode: IncrementalMode,
    current: CurrentState,
    reusable_inputs: HashSet<String>,
    previous_sections: HashSet<SectionRecord>,
    current_sections: Mutex<Vec<SectionRecord>>,
    reused_sections: AtomicUsize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IncrementalMode {
    Disabled,
    Reuse,
    Relink {
        reason: String,
        can_reuse_unchanged_sections: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CurrentState {
    state_dir: PathBuf,
    args_hash: String,
    input_files: Vec<FileState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedState {
    args_hash: String,
    output: FileContentState,
    build_id_hashes: Option<BuildIdHashState>,
    input_files: Vec<FileState>,
    sections: Vec<SectionRecord>,
    sections_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildIdHashState {
    output_len: u64,
    nodes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileState {
    path: String,
    content: FileContentState,
    patch: Option<FilePatchState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilePatchState {
    fingerprint: String,
    sections: Vec<FilePatchSectionState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilePatchSectionState {
    section_index: u32,
    section_name: Option<String>,
    input_size: u64,
    output_offset: u64,
    output_size: u64,
}

#[derive(Debug, Clone, Eq)]
struct FileContentState {
    len: u64,
    hash: String,
    identity: Option<FileIdentity>,
}

#[derive(Debug, Clone)]
struct FileIdentity {
    len: u64,
    dev: u64,
    ino: u64,
    modified_sec: i64,
    modified_nsec: i64,
    changed_sec: i64,
    changed_nsec: i64,
}

impl PartialEq for FileIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && self.dev == other.dev
            && self.ino == other.ino
            && self.modified_sec == other.modified_sec
            && self.modified_nsec == other.modified_nsec
    }
}

impl Eq for FileIdentity {}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SectionRecord {
    input_file: String,
    input: String,
    section_index: u32,
    output_offset: u64,
    size: u64,
}

pub(crate) fn maybe_prepare(
    args: &impl platform::Args,
    file_loader: &FileLoader<'_>,
) -> Result<PreparedState> {
    if !args.common().incremental {
        return Ok(PreparedState {
            mode: IncrementalMode::Disabled,
            current: CurrentState {
                state_dir: state_dir_for_output(args.output()),
                args_hash: String::new(),
                input_files: Vec::new(),
            },
            reusable_inputs: HashSet::new(),
            previous_sections: HashSet::new(),
            current_sections: Mutex::new(Vec::new()),
            reused_sections: AtomicUsize::new(0),
        });
    }

    timing_phase!("Prepare incremental link");

    let state_dir = state_dir_for_output(args.output());
    let previous = PersistedState::read(&state_dir);
    let current = CurrentState::new(
        args,
        file_loader,
        previous.as_ref().ok().and_then(|p| p.as_ref()),
    );
    let (mode, previous) = match previous {
        Ok(Some(previous)) => (
            classify_incremental_mode(args.output(), &current, &previous),
            Some(previous),
        ),
        Ok(None) => (
            IncrementalMode::Relink {
                reason: "no previous incremental state".to_owned(),
                can_reuse_unchanged_sections: false,
            },
            None,
        ),
        Err(error) => (
            IncrementalMode::Relink {
                reason: format!("could not read previous incremental state: {error:?}"),
                can_reuse_unchanged_sections: false,
            },
            None,
        ),
    };

    current.log_mode(&mode)?;

    let reusable_inputs = previous
        .as_ref()
        .map(|previous| reusable_input_files(&current.input_files, &previous.input_files))
        .unwrap_or_default();
    let previous_sections = previous
        .as_ref()
        .map(|previous| previous.sections.iter().cloned().collect())
        .unwrap_or_default();

    Ok(PreparedState {
        mode,
        current,
        reusable_inputs,
        previous_sections,
        current_sections: Mutex::new(Vec::new()),
        reused_sections: AtomicUsize::new(0),
    })
}

pub(crate) fn maybe_reuse_output_before_loading(args: &impl platform::Args) -> Result<bool> {
    if !args.common().incremental
        || args.should_write_trace_file()
        || args.common().save_dir.is_active()
    {
        return Ok(false);
    }
    if args
        .dependency_file()
        .is_some_and(|dependency_file| !dependency_file.exists())
    {
        return Ok(false);
    }

    timing_phase!("Check incremental fast path");

    let state_dir = state_dir_for_output(args.output());
    let Some(mut previous) = PersistedState::read_metadata(&state_dir).unwrap_or_default() else {
        return Ok(false);
    };

    if previous.args_hash != args_hash(args) {
        return Ok(false);
    }
    if !previous.output.identity_matches_path(args.output())? {
        return Ok(false);
    }

    let mut changed_inputs = Vec::new();
    let mut rewritten_inputs = Vec::new();
    for (index, input) in previous.input_files.iter().enumerate() {
        let path = decode_path(&input.path)?;
        if input.content.identity_matches_path(&path)? {
            continue;
        }
        if input_content_matches_snapshot(&state_dir, input, &path)? {
            rewritten_inputs.push((index, path));
            continue;
        }
        changed_inputs.push((index, path));
    }

    if !rewritten_inputs.is_empty() {
        snapshot_input_paths(
            &state_dir,
            rewritten_inputs.iter().map(|(_, path)| path.as_path()),
        )?;
        for (input_index, path) in &rewritten_inputs {
            previous.input_files[*input_index].content =
                FileContentState::from_path_identity_only(path).with_context(|| {
                    format!(
                        "Failed to record rewritten incremental input `{}`",
                        path.display()
                    )
                })?;
        }
        refresh_input_file_identities(&mut previous.input_files);
    }

    if !changed_inputs.is_empty() {
        match patch_changed_inputs(args, &state_dir, previous, &changed_inputs)? {
            ChangedInputPatchResult::Patched => return Ok(true),
            ChangedInputPatchResult::Unsupported(reason) => {
                append_log(
                    &state_dir,
                    &format!("changed-input patch unavailable before loading inputs: {reason}"),
                )?;
            }
        }
        return Ok(false);
    }

    if !rewritten_inputs.is_empty() {
        previous.write_metadata_update(&state_dir)?;
        append_log(
            &state_dir,
            &format!(
                "updated {} rewritten input file{} before loading inputs",
                rewritten_inputs.len(),
                if rewritten_inputs.len() == 1 { "" } else { "s" }
            ),
        )?;
    }
    append_log(&state_dir, "reused existing output before loading inputs")?;
    Ok(true)
}

enum ChangedInputPatchResult {
    Patched,
    Unsupported(String),
}

fn patch_changed_inputs(
    args: &impl platform::Args,
    state_dir: &Path,
    previous: PersistedState,
    changed_inputs: &[(usize, PathBuf)],
) -> Result<ChangedInputPatchResult> {
    timing_phase!("Patch changed incremental inputs");

    let mut patches = Vec::new();
    let mut patched_section_count = 0;
    let mut input_files = previous.input_files.clone();
    for (input_index, path) in changed_inputs {
        let input = &previous.input_files[*input_index];
        let Some(previous_patch) = input.patch.as_ref() else {
            return Ok(ChangedInputPatchResult::Unsupported(format!(
                "missing patch metadata for `{}`",
                path.display()
            )));
        };
        if previous_patch.sections.is_empty() {
            return Ok(ChangedInputPatchResult::Unsupported(format!(
                "no patchable sections recorded for `{}`",
                path.display()
            )));
        }
        let patch_section_indexes = previous_patch
            .sections
            .iter()
            .map(|section| section.section_index)
            .collect::<HashSet<_>>();
        if patch_section_indexes.len() != previous_patch.sections.len() {
            return Ok(ChangedInputPatchResult::Unsupported(format!(
                "duplicate patchable section metadata for `{}`",
                path.display()
            )));
        }
        let sections = previous_patch
            .sections
            .iter()
            .map(|section| PatchSection {
                section_index: section.section_index,
                section_name: section.section_name.clone(),
                input_size: section.input_size,
                output_offset: section.output_offset,
                output_size: section.output_size,
            })
            .collect::<Vec<_>>();

        let bytes = std::fs::read(path).with_context(|| {
            format!(
                "Failed to read changed incremental input `{}`",
                path.display()
            )
        })?;
        let matched_sections = match match_patch_sections(state_dir, input, &bytes, &sections)? {
            Some(matched_sections) => matched_sections,
            None if sections
                .iter()
                .any(|section| section.section_name.is_none()) =>
            {
                return Ok(ChangedInputPatchResult::Unsupported(format!(
                    "could not match anonymous patch sections in `{}`",
                    path.display()
                )));
            }
            None => sections
                .iter()
                .cloned()
                .map(MatchedPatchSection::same)
                .collect(),
        };
        let current_sections = matched_sections
            .iter()
            .map(|section| section.current.clone())
            .collect::<Vec<_>>();

        let fingerprint = patch_fingerprint(&bytes, current_sections.iter().cloned())?;
        if fingerprint.as_deref() != Some(previous_patch.fingerprint.as_str()) {
            return Ok(ChangedInputPatchResult::Unsupported(format!(
                "changed bytes outside patchable sections in `{}`",
                path.display()
            )));
        }

        let patch_sections = changed_patch_sections(state_dir, input, &bytes, &matched_sections)?
            .unwrap_or_else(|| current_sections.clone());
        patched_section_count += patch_sections.len();

        input_files[*input_index].content = FileContentState::from_path_identity_only(path)
            .with_context(|| {
                format!(
                    "Failed to record changed incremental input `{}`",
                    path.display()
                )
            })?;
        input_files[*input_index].patch = Some(previous_patch.clone());

        let Some(section_patches) =
            patch_sections_for_input(&bytes, patch_sections.iter().cloned())?
        else {
            return Ok(ChangedInputPatchResult::Unsupported(format!(
                "changed patchable section size in `{}`",
                path.display()
            )));
        };
        patches.extend(section_patches);
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(args.output())
        .with_context(|| {
            format!(
                "Failed to open output `{}` for incremental patching",
                args.output().display()
            )
        })?;
    let mut output = unsafe { MmapOptions::new().map_mut(&file) }.with_context(|| {
        format!(
            "Failed to mmap output `{}` for incremental patching",
            args.output().display()
        )
    })?;

    let build_id_range = build_id_note_range(&output)?;
    if build_id_range.is_some() && !args.has_incremental_fast_build_id() {
        return Ok(ChangedInputPatchResult::Unsupported(
            "output has a build ID that cannot be updated incrementally".to_owned(),
        ));
    }
    let mut build_id_tree = None;
    let mut build_id_hashes = None;
    if build_id_range.is_some() {
        let Some(previous_hashes) = previous.build_id_hashes.as_ref() else {
            return Ok(ChangedInputPatchResult::Unsupported(
                "missing build ID hash state".to_owned(),
            ));
        };
        let Ok(tree) = read_build_id_hash_tree(state_dir, previous_hashes) else {
            return Ok(ChangedInputPatchResult::Unsupported(
                "could not read build ID hash state".to_owned(),
            ));
        };
        build_id_tree = Some(tree);
        build_id_hashes = Some(previous_hashes.clone());
    }

    let mut patched_ranges = Vec::new();
    for patch in patches {
        let start = patch.output_offset as usize;
        let end = start
            .checked_add(patch.size as usize)
            .context("Incremental patch output range overflow")?;
        let Some(output_range) = output.get_mut(start..end) else {
            return Ok(ChangedInputPatchResult::Unsupported(
                "changed patch output range is out of bounds".to_owned(),
            ));
        };
        if patch.data.len() > output_range.len() {
            return Ok(ChangedInputPatchResult::Unsupported(
                "changed patch data does not fit in the previous output range".to_owned(),
            ));
        }
        let (data_out, padding) = output_range.split_at_mut(patch.data.len());
        data_out.copy_from_slice(&patch.data);
        padding.fill(0);
        patched_ranges.push(start..end);
    }

    let mut flush_ranges = patched_ranges.clone();
    if let Some(range) = build_id_range {
        let previous_hashes = build_id_hashes
            .as_mut()
            .context("Missing incremental build ID hash state")?;
        let tree = build_id_tree
            .as_mut()
            .context("Missing incremental build ID hash tree")?;
        flush_ranges.push(range.clone());
        write_fast_build_id_from_state(&mut output, range, previous_hashes, tree, &patched_ranges)?;
    }

    flush_output_ranges(&output, &flush_ranges, args.output())?;
    drop(output);
    drop(file);

    let output = FileContentState::from_path_identity_only(args.output()).with_context(|| {
        format!(
            "Failed to record patched output `{}` for incremental state",
            args.output().display()
        )
    })?;
    write_build_id_hash_tree(state_dir, build_id_tree.as_deref())?;
    snapshot_input_paths(
        state_dir,
        changed_inputs.iter().map(|(_, path)| path.as_path()),
    )?;
    refresh_input_file_identities(&mut input_files);
    PersistedState {
        args_hash: previous.args_hash,
        output,
        build_id_hashes,
        input_files,
        sections: previous.sections.clone(),
        sections_file: previous.sections_file.clone(),
    }
    .write_metadata_update(state_dir)?;

    append_log(
        state_dir,
        &format!(
            "patched {} changed input file{} before loading inputs",
            changed_inputs.len(),
            if changed_inputs.len() == 1 { "" } else { "s" }
        ),
    )?;
    append_log(
        state_dir,
        &format!("patched {patched_section_count} changed input sections before loading inputs"),
    )?;
    Ok(ChangedInputPatchResult::Patched)
}

struct SectionPatch {
    output_offset: u64,
    size: u64,
    data: Vec<u8>,
}

fn flush_output_ranges(
    output: &memmap2::MmapMut,
    ranges: &[std::ops::Range<usize>],
    output_path: &Path,
) -> Result {
    let mut ranges = ranges
        .iter()
        .filter(|range| !range.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range.start);

    let mut merged = Vec::<std::ops::Range<usize>>::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
            continue;
        }
        merged.push(range);
    }

    for range in merged {
        output
            .flush_range(range.start, range.end - range.start)
            .with_context(|| {
                format!(
                    "Failed to flush incrementally patched output `{}`",
                    output_path.display()
                )
            })?;
    }
    Ok(())
}

#[derive(Clone)]
struct PatchSection {
    section_index: u32,
    section_name: Option<String>,
    input_size: u64,
    output_offset: u64,
    output_size: u64,
}

#[derive(Clone)]
struct MatchedPatchSection {
    previous: PatchSection,
    current: PatchSection,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct SectionReference {
    source_section_name: String,
    relocation_offset: u64,
    relocation_kind: String,
    relocation_encoding: String,
    relocation_size: u8,
    relocation_addend: i64,
}

impl MatchedPatchSection {
    fn same(section: PatchSection) -> Self {
        Self {
            previous: section.clone(),
            current: section,
        }
    }
}

impl PreparedState {
    pub(crate) fn can_reuse_output(&self) -> bool {
        self.mode == IncrementalMode::Reuse
    }

    pub(crate) fn can_reuse_unchanged_sections(&self) -> bool {
        matches!(
            self.mode,
            IncrementalMode::Relink {
                can_reuse_unchanged_sections: true,
                ..
            }
        )
    }

    pub(crate) fn try_reuse_section(
        &self,
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        output_offset: u64,
        size: u64,
        record_for_reuse: bool,
        allow_reuse: bool,
    ) -> bool {
        if self.mode == IncrementalMode::Disabled {
            return false;
        }
        if !record_for_reuse {
            return false;
        }

        let record = SectionRecord::new(input, section_index, output_offset, size);
        self.current_sections.lock().unwrap().push(record.clone());

        if !allow_reuse {
            return false;
        }
        if !self.can_reuse_unchanged_sections() {
            return false;
        }
        if !self.reusable_inputs.contains(&record.input_file) {
            return false;
        }
        if !self.previous_sections.contains(&record) {
            return false;
        }

        self.reused_sections.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub(crate) fn finish(
        &self,
        args: &impl platform::Args,
        file_loader: &FileLoader<'_>,
    ) -> Result {
        if self.mode == IncrementalMode::Disabled {
            return Ok(());
        }

        timing_phase!("Write incremental state");

        let output =
            FileContentState::from_path_identity_only(args.output()).with_context(|| {
                format!(
                    "Failed to record output file `{}` for incremental state",
                    args.output().display()
                )
            })?;
        let output_bytes = std::fs::read(args.output()).with_context(|| {
            format!(
                "Failed to read output `{}` for incremental state",
                args.output().display()
            )
        })?;
        let (build_id_hashes, build_id_tree) = if args.has_incremental_fast_build_id() {
            build_id_hash_state_from_output(&output_bytes)?
        } else {
            (None, None)
        };

        let mut sections = self.current_sections.lock().unwrap().clone();
        if sections.is_empty() && self.mode == IncrementalMode::Reuse {
            sections.extend(self.previous_sections.iter().cloned());
        }
        sections.sort();

        let mut input_files = self.current.input_files.clone();
        record_patch_fingerprints(&mut input_files, file_loader, &sections, &output_bytes)?;
        snapshot_loaded_files(&self.current.state_dir, file_loader)?;
        refresh_input_file_identities(&mut input_files);

        let state = PersistedState {
            args_hash: self.current.args_hash.clone(),
            output,
            build_id_hashes,
            input_files,
            sections,
            sections_file: None,
        };

        write_build_id_hash_tree(&self.current.state_dir, build_id_tree.as_deref())?;
        state.write(&self.current.state_dir)?;
        let reused = self.reused_sections.load(Ordering::Relaxed);
        if reused > 0 {
            append_log(
                &self.current.state_dir,
                &format!("reused {reused} unchanged input sections"),
            )?;
        }
        Ok(())
    }
}

fn classify_incremental_mode(
    output: &Path,
    current: &CurrentState,
    previous: &PersistedState,
) -> IncrementalMode {
    if current.args_hash != previous.args_hash {
        return IncrementalMode::Relink {
            reason: "linker arguments changed".to_owned(),
            can_reuse_unchanged_sections: false,
        };
    }

    if !previous
        .output
        .identity_matches_path(output)
        .unwrap_or(false)
    {
        match FileContentState::from_path(output) {
            Ok(output_state) if output_state == previous.output => {}
            Ok(_) => {
                return IncrementalMode::Relink {
                    reason: "output file changed since previous link".to_owned(),
                    can_reuse_unchanged_sections: false,
                };
            }
            Err(error) => {
                return IncrementalMode::Relink {
                    reason: format!("output file could not be reused: {error:?}"),
                    can_reuse_unchanged_sections: false,
                };
            }
        }
    }

    if current.input_files != previous.input_files {
        return IncrementalMode::Relink {
            reason: describe_input_difference(&current.input_files, &previous.input_files),
            can_reuse_unchanged_sections: true,
        };
    }

    IncrementalMode::Reuse
}

fn describe_input_difference(current: &[FileState], previous: &[FileState]) -> String {
    let previous_by_path = previous
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<HashMap<_, _>>();

    for file in current {
        match previous_by_path.get(file.path.as_str()) {
            None => return format!("input file added: {}", display_hex_path(&file.path)),
            Some(previous) if previous.content != file.content => {
                return format!("input file changed: {}", display_hex_path(&file.path));
            }
            Some(_) => {}
        }
    }

    let current_paths = current
        .iter()
        .map(|file| file.path.as_str())
        .collect::<HashSet<_>>();
    for file in previous {
        if !current_paths.contains(file.path.as_str()) {
            return format!("input file removed: {}", display_hex_path(&file.path));
        }
    }

    "input file set changed".to_owned()
}

fn reusable_input_files(current: &[FileState], previous: &[FileState]) -> HashSet<String> {
    let previous_by_path = previous
        .iter()
        .map(|file| (file.path.as_str(), &file.content))
        .collect::<HashMap<_, _>>();

    current
        .iter()
        .filter(|file| previous_by_path.get(file.path.as_str()) == Some(&&file.content))
        .map(|file| file.path.clone())
        .collect()
}

impl CurrentState {
    fn new(
        args: &impl platform::Args,
        file_loader: &FileLoader<'_>,
        previous: Option<&PersistedState>,
    ) -> Self {
        Self {
            state_dir: state_dir_for_output(args.output()),
            args_hash: args_hash(args),
            input_files: fingerprint_loaded_files(file_loader, previous),
        }
    }

    fn log_mode(&self, mode: &IncrementalMode) -> Result {
        match mode {
            IncrementalMode::Disabled => Ok(()),
            IncrementalMode::Reuse => append_log(&self.state_dir, "reused existing output"),
            IncrementalMode::Relink { reason, .. } => {
                append_log(&self.state_dir, &format!("full relink: {reason}"))
            }
        }
    }
}

impl PersistedState {
    fn read(state_dir: &Path) -> Result<Option<Self>> {
        Self::read_impl(state_dir, true)
    }

    fn read_metadata(state_dir: &Path) -> Result<Option<Self>> {
        Self::read_impl(state_dir, false)
    }

    fn read_impl(state_dir: &Path, load_sections: bool) -> Result<Option<Self>> {
        let path = state_dir.join(INDEX_FILE);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        Ok(Some(Self::parse_with_section_loader(
            &contents,
            |sections_file| {
                if !load_sections {
                    return Ok(None);
                }
                let path = state_dir.join(sections_file);
                std::fs::read_to_string(&path).map(Some).with_context(|| {
                    format!("Failed to read incremental sections `{}`", path.display())
                })
            },
        )?))
    }

    #[cfg(test)]
    fn parse(contents: &str) -> Result<Self> {
        Self::parse_with_section_loader(contents, |_| Ok(None))
    }

    fn parse_with_section_loader(
        contents: &str,
        mut load_sections: impl FnMut(&str) -> Result<Option<String>>,
    ) -> Result<Self> {
        let mut lines = contents.lines().peekable();
        let version = lines.next().context("Missing incremental state header")?;
        if version != STATE_VERSION
            && version != STATE_VERSION_V10
            && version != STATE_VERSION_V9
            && version != STATE_VERSION_V8
            && version != STATE_VERSION_V7
            && version != STATE_VERSION_V6
            && version != STATE_VERSION_V5
            && version != STATE_VERSION_V4
            && version != STATE_VERSION_V3
            && version != STATE_VERSION_V2
            && version != STATE_VERSION_V1
        {
            return Err(crate::error!(
                "Unsupported incremental state version `{version}`"
            ));
        }

        let args_hash = parse_prefixed_line(lines.next(), "args")?.to_owned();
        let output = parse_content_line(lines.next(), "output")?;
        let build_id_hashes = if lines
            .peek()
            .is_some_and(|line| line.starts_with("build-id-hash\t"))
        {
            parse_build_id_hash_line(lines.next())?
        } else {
            None
        };

        let input_count: usize = parse_prefixed_line(lines.next(), "inputs")?
            .parse()
            .context("Invalid incremental input count")?;

        let mut input_files = Vec::with_capacity(input_count);
        for _ in 0..input_count {
            let line = lines.next().context("Missing incremental input record")?;
            input_files.push(parse_input_line(line)?);
        }

        let mut sections_file = None;
        let sections = if version == STATE_VERSION
            || version == STATE_VERSION_V10
            || version == STATE_VERSION_V9
            || version == STATE_VERSION_V8
            || version == STATE_VERSION_V7
            || version == STATE_VERSION_V6
            || version == STATE_VERSION_V5
        {
            let first_line = lines
                .next()
                .context("Missing incremental section input count")?;
            if first_line.starts_with("sections-file\t") {
                let file = parse_prefixed_line(Some(first_line), "sections-file")?.to_owned();
                let sections = load_sections(&file)?
                    .map(|contents| parse_compact_sections_block(contents.lines()))
                    .transpose()?
                    .unwrap_or_default();
                sections_file = Some(file);
                sections
            } else {
                parse_compact_sections_block(std::iter::once(first_line).chain(&mut lines))?
            }
        } else if version == STATE_VERSION_V4
            || version == STATE_VERSION_V3
            || version == STATE_VERSION_V2
        {
            let section_count: usize = parse_prefixed_line(lines.next(), "sections")?
                .parse()
                .context("Invalid incremental section count")?;
            let mut sections = Vec::with_capacity(section_count);
            for _ in 0..section_count {
                let line = lines.next().context("Missing incremental section record")?;
                sections.push(parse_section_line(line)?);
            }
            sections
        } else {
            Vec::new()
        };

        if lines.next().is_some() {
            return Err(crate::error!("Unexpected trailing incremental state data"));
        }

        Ok(Self {
            args_hash,
            output,
            build_id_hashes,
            input_files,
            sections,
            sections_file,
        })
    }

    fn write(&self, state_dir: &Path) -> Result {
        self.write_sections(state_dir)?;
        self.write_index(state_dir)
    }

    fn write_metadata_update(&self, state_dir: &Path) -> Result {
        if self.sections_file.is_some() {
            self.write_index(state_dir)
        } else {
            self.write(state_dir)
        }
    }

    fn write_index(&self, state_dir: &Path) -> Result {
        std::fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "Failed to create incremental state directory `{}`",
                state_dir.display()
            )
        })?;

        let path = state_dir.join(INDEX_FILE);
        let tmp_path = state_dir.join(format!("{INDEX_FILE}.tmp"));
        std::fs::write(&tmp_path, self.render_index()).with_context(|| {
            format!("Failed to write incremental state `{}`", tmp_path.display())
        })?;
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to install incremental state `{}`", path.display()))?;
        Ok(())
    }

    fn write_sections(&self, state_dir: &Path) -> Result {
        std::fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "Failed to create incremental state directory `{}`",
                state_dir.display()
            )
        })?;

        let path = state_dir.join(SECTIONS_FILE);
        let tmp_path = state_dir.join(format!("{SECTIONS_FILE}.tmp"));
        std::fs::write(&tmp_path, self.render_sections()).with_context(|| {
            format!(
                "Failed to write incremental sections `{}`",
                tmp_path.display()
            )
        })?;
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "Failed to install incremental sections `{}`",
                path.display()
            )
        })?;
        Ok(())
    }

    fn render_index(&self) -> String {
        let mut out = self.render_header_and_inputs();
        writeln!(&mut out, "sections-file\t{SECTIONS_FILE}").unwrap();
        out
    }

    #[cfg(test)]
    fn render(&self) -> String {
        let mut out = self.render_header_and_inputs();
        out.push_str(&self.render_sections());
        out
    }

    fn render_header_and_inputs(&self) -> String {
        let mut out = String::new();
        writeln!(&mut out, "{STATE_VERSION}").unwrap();
        writeln!(&mut out, "args\t{}", self.args_hash).unwrap();
        writeln!(
            &mut out,
            "output\t{}\t{}\t{}",
            self.output.len,
            self.output.hash,
            self.output.render_identity()
        )
        .unwrap();
        writeln!(
            &mut out,
            "build-id-hash\t{}",
            self.build_id_hashes
                .as_ref()
                .map(render_build_id_hash_state)
                .unwrap_or_else(|| ABSENT_FIELD.to_owned())
        )
        .unwrap();
        writeln!(&mut out, "inputs\t{}", self.input_files.len()).unwrap();
        for input in &self.input_files {
            writeln!(
                &mut out,
                "input\t{}\t{}\t{}\t{}\t{}\t{}",
                input.path,
                input.content.len,
                input.content.hash,
                input.content.render_identity(),
                input
                    .patch
                    .as_ref()
                    .map_or(ABSENT_FIELD, |patch| patch.fingerprint.as_str()),
                input
                    .patch
                    .as_ref()
                    .map(render_patch_sections)
                    .unwrap_or_else(|| ABSENT_FIELD.to_owned())
            )
            .unwrap();
        }
        out
    }

    fn render_sections(&self) -> String {
        let mut out = String::new();

        let mut section_inputs = Vec::new();
        let mut section_input_ids = HashMap::new();
        for section in &self.sections {
            let key = (section.input_file.as_str(), section.input.as_str());
            if !section_input_ids.contains_key(&key) {
                let index = section_inputs.len();
                section_input_ids.insert(key, index);
                section_inputs.push(key);
            }
        }

        writeln!(&mut out, "section-inputs\t{}", section_inputs.len()).unwrap();
        for (input_file, input) in section_inputs {
            writeln!(&mut out, "section-input\t{input_file}\t{input}").unwrap();
        }

        writeln!(&mut out, "sections\t{}", self.sections.len()).unwrap();
        for section in &self.sections {
            let section_input_id =
                section_input_ids[&(section.input_file.as_str(), section.input.as_str())];
            writeln!(
                &mut out,
                "section\t{}\t{}\t{}\t{}",
                section_input_id, section.section_index, section.output_offset, section.size
            )
            .unwrap();
        }
        out
    }
}

impl PartialEq for FileContentState {
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        if !self.hash.is_empty() && !other.hash.is_empty() {
            return self.hash == other.hash;
        }
        self.identity.is_some() && self.identity == other.identity
    }
}

impl SectionRecord {
    fn new(
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        output_offset: u64,
        size: u64,
    ) -> Self {
        Self {
            input_file: encode_path(&input.file.filename),
            input: encode_input_ref(input),
            section_index: section_index.0 as u32,
            output_offset,
            size,
        }
    }
}

impl FileContentState {
    fn from_path_identity_only(path: &Path) -> Result<Self> {
        let Some(identity) = FileIdentity::from_path(path)? else {
            return Self::from_path(path);
        };
        Ok(Self {
            len: identity.len,
            hash: String::new(),
            identity: Some(identity),
        })
    }

    fn from_path(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("Failed to read `{}`", path.display()))?;
        let mut state = Self::from_bytes(&bytes);
        state.identity = FileIdentity::from_path(path).ok().flatten();
        Ok(state)
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            len: bytes.len() as u64,
            hash: hash_bytes(bytes),
            identity: None,
        }
    }

    fn from_input_file(
        input_file: &crate::input_data::InputFile,
        previous: Option<&FileContentState>,
    ) -> Self {
        let identity = FileIdentity::from_path(&input_file.filename).ok().flatten();
        if let (Some(identity), Some(previous)) = (identity.as_ref(), previous)
            && previous.identity.as_ref() == Some(identity)
        {
            let mut state = previous.clone();
            state.identity = Some(identity.clone());
            return state;
        }

        if let Some(identity) = identity.as_ref()
            && previous.is_none_or(|previous| previous.hash.is_empty())
        {
            return Self {
                len: identity.len,
                hash: String::new(),
                identity: Some(identity.clone()),
            };
        }

        let mut state = Self::from_bytes(input_file.data());
        state.identity = identity;
        state
    }

    fn identity_matches_path(&self, path: &Path) -> Result<bool> {
        let Some(previous) = self.identity.as_ref() else {
            return Ok(false);
        };
        Ok(FileIdentity::from_path(path)?.as_ref() == Some(previous))
    }

    fn render_identity(&self) -> String {
        self.identity
            .as_ref()
            .map_or_else(|| "-".to_owned(), FileIdentity::render)
    }
}

impl FileIdentity {
    fn from_path(path: &Path) -> Result<Option<Self>> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("Failed to read metadata for `{}`", path.display()))?;
        #[cfg(unix)]
        {
            Ok(Some(Self::from_metadata(&metadata)))
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Ok(None)
        }
    }

    #[cfg(unix)]
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            modified_sec: metadata.mtime(),
            modified_nsec: metadata.mtime_nsec(),
            changed_sec: metadata.ctime(),
            changed_nsec: metadata.ctime_nsec(),
        }
    }

    fn render(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            self.len,
            self.dev,
            self.ino,
            self.modified_sec,
            self.modified_nsec,
            self.changed_sec,
            self.changed_nsec
        )
    }

    fn parse(value: &str) -> Result<Option<Self>> {
        if value == "-" {
            return Ok(None);
        }

        let mut parts = value.split(':');
        let mut next = |field| {
            parts
                .next()
                .with_context(|| format!("Malformed incremental file identity `{field}`"))
        };
        let identity = Self {
            len: next("len")?
                .parse()
                .context("Invalid incremental file identity length")?,
            dev: next("dev")?
                .parse()
                .context("Invalid incremental file identity device")?,
            ino: next("ino")?
                .parse()
                .context("Invalid incremental file identity inode")?,
            modified_sec: next("mtime")?
                .parse()
                .context("Invalid incremental file identity mtime")?,
            modified_nsec: next("mtime_nsec")?
                .parse()
                .context("Invalid incremental file identity mtime_nsec")?,
            changed_sec: next("ctime")?
                .parse()
                .context("Invalid incremental file identity ctime")?,
            changed_nsec: next("ctime_nsec")?
                .parse()
                .context("Invalid incremental file identity ctime_nsec")?,
        };
        if parts.next().is_some() {
            return Err(crate::error!("Malformed incremental file identity"));
        }
        Ok(Some(identity))
    }
}

fn record_patch_fingerprints(
    input_files: &mut [FileState],
    file_loader: &FileLoader<'_>,
    sections: &[SectionRecord],
    output: &[u8],
) -> Result {
    let mut sections_by_file = HashMap::<&str, Vec<&SectionRecord>>::new();
    for section in sections {
        if section.input == section.input_file {
            sections_by_file
                .entry(section.input_file.as_str())
                .or_default()
                .push(section);
        }
    }

    if sections_by_file.is_empty() {
        return Ok(());
    }

    let loaded_by_path = file_loader
        .loaded_files
        .iter()
        .map(|file| (encode_path(&file.filename), *file))
        .collect::<HashMap<_, _>>();

    for input in input_files {
        let Some(sections) = sections_by_file.get(input.path.as_str()) else {
            input.patch = None;
            continue;
        };
        let Some(input_file) = loaded_by_path.get(&input.path) else {
            input.patch = None;
            continue;
        };
        let patch_sections = direct_copy_patch_sections(input_file.data(), output, sections)?;
        input.patch = patch_fingerprint(input_file.data(), patch_sections.iter().cloned())?.map(
            |fingerprint| FilePatchState {
                fingerprint,
                sections: patch_sections
                    .iter()
                    .map(|section| FilePatchSectionState {
                        section_index: section.section_index,
                        section_name: section.section_name.clone(),
                        input_size: section.input_size,
                        output_offset: section.output_offset,
                        output_size: section.output_size,
                    })
                    .collect(),
            },
        );
    }

    Ok(())
}

fn direct_copy_patch_sections<'a>(
    bytes: &[u8],
    output: &[u8],
    sections: &[&'a SectionRecord],
) -> Result<Vec<PatchSection>> {
    let file =
        object::File::parse(bytes).context("Failed to parse incremental patch candidate input")?;
    let mut patch_sections = Vec::new();
    for record in sections {
        let section = file
            .section_by_index(object::SectionIndex(record.section_index as usize))
            .context("Missing incremental patch candidate section")?;
        if !section_allows_direct_patching(&section) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read incremental patch candidate section data")?;
        if data.len() > record.size as usize {
            continue;
        }
        let start = record.output_offset as usize;
        let end = start
            .checked_add(record.size as usize)
            .context("Incremental patch output range overflow")?;
        let Some(output_range) = output.get(start..end) else {
            continue;
        };
        let (data_out, padding) = output_range.split_at(data.len());
        if data_out == data && padding.iter().all(|byte| *byte == 0) {
            patch_sections.push(PatchSection {
                section_index: record.section_index,
                section_name: patch_section_name_for_matching(&section),
                input_size: data.len() as u64,
                output_offset: record.output_offset,
                output_size: record.size,
            });
        }
    }
    Ok(patch_sections)
}

fn section_flags_allow_patching(flags: object::SectionFlags) -> bool {
    let object::SectionFlags::Elf { sh_flags } = flags else {
        return false;
    };
    let allocated = sh_flags & u64::from(object::elf::SHF_ALLOC) != 0;
    let content_ordered = sh_flags & u64::from(object::elf::SHF_MERGE) != 0;
    allocated && !content_ordered
}

fn section_allows_direct_patching<'data>(section: &impl object::ObjectSection<'data>) -> bool {
    section_flags_allow_patching(section.flags())
        && section
            .name()
            .ok()
            .is_none_or(section_name_allows_direct_patching)
}

fn section_name_allows_direct_patching(name: &str) -> bool {
    !matches!(name, ".init" | ".fini")
        && !name.starts_with(".init_array")
        && !name.starts_with(".fini_array")
        && !name.starts_with(".preinit_array")
        && !name.starts_with(".ctors")
        && !name.starts_with(".dtors")
}

fn patch_section_name_for_matching<'data>(
    section: &impl object::ObjectSection<'data>,
) -> Option<String> {
    let name = section.name().ok()?;
    section_name_is_stable_for_patch_matching(name).then(|| name.to_owned())
}

fn section_name_is_stable_for_patch_matching(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..L")
        && !name.contains(".L__")
        && !name.contains("__unnamed_")
}

fn patch_fingerprint(
    bytes: &[u8],
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<String>> {
    let Some(ranges) = patch_ranges(bytes, sections)? else {
        return Ok(None);
    };

    let mut hasher = blake3::Hasher::new();
    let mut position = 0;
    for range in ranges {
        hasher.update(&bytes[position..range.start]);
        update_hash_with_zeroes(&mut hasher, range.end - range.start);
        position = range.end;
    }
    hasher.update(&bytes[position..]);
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

fn match_patch_sections(
    state_dir: &Path,
    previous_input: &FileState,
    current_bytes: &[u8],
    sections: &[PatchSection],
) -> Result<Option<Vec<MatchedPatchSection>>> {
    let snapshot = input_snapshot_path_for_encoded_path(state_dir, &previous_input.path);
    if !previous_input
        .content
        .identity_matches_path(&snapshot)
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let previous_bytes = match std::fs::read(&snapshot) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let previous_file =
        object::File::parse(&*previous_bytes).context("Failed to parse previous patch input")?;
    let current_file =
        object::File::parse(current_bytes).context("Failed to parse current patch input")?;
    let previous_references = section_reference_map(&previous_file)?;
    let current_references = section_reference_map(&current_file)?;

    let mut matched_sections = Vec::new();
    for section in sections.iter().cloned() {
        let Some(previous_index) = patch_section_index(&previous_file, &section)? else {
            return Ok(None);
        };
        let Some(current_index) = match_current_patch_section_index(
            &current_file,
            &section,
            previous_index,
            &previous_references,
            &current_references,
        )?
        else {
            return Ok(None);
        };

        let mut previous = section.clone();
        previous.section_index = previous_index.0 as u32;
        let mut current = section;
        current.section_index = current_index.0 as u32;
        matched_sections.push(MatchedPatchSection { previous, current });
    }

    Ok(Some(matched_sections))
}

fn match_current_patch_section_index(
    current_file: &object::File<'_>,
    patch_section: &PatchSection,
    previous_index: object::SectionIndex,
    previous_references: &HashMap<object::SectionIndex, Vec<SectionReference>>,
    current_references: &HashMap<object::SectionIndex, Vec<SectionReference>>,
) -> Result<Option<object::SectionIndex>> {
    if patch_section.section_name.is_some() {
        return patch_section_index(current_file, patch_section);
    }

    let Some(previous_signature) = previous_references.get(&previous_index) else {
        return Ok(None);
    };
    Ok(match_section_by_references(
        previous_signature,
        current_references,
    ))
}

fn match_section_by_references(
    previous_signature: &[SectionReference],
    current_references: &HashMap<object::SectionIndex, Vec<SectionReference>>,
) -> Option<object::SectionIndex> {
    if previous_signature.is_empty() {
        return None;
    }

    let mut matches = current_references
        .iter()
        .filter_map(|(index, signature)| (signature == previous_signature).then_some(*index));
    let index = matches.next()?;
    matches.next().is_none().then_some(index)
}

fn section_reference_map(
    file: &object::File<'_>,
) -> Result<HashMap<object::SectionIndex, Vec<SectionReference>>> {
    let mut references = HashMap::<object::SectionIndex, Vec<SectionReference>>::new();
    for section in file.sections() {
        let Some(source_section_name) = patch_section_name_for_matching(&section) else {
            continue;
        };
        for (relocation_offset, relocation) in section.relocations() {
            let Some(target_section) = relocation_target_section(file, relocation.target())? else {
                continue;
            };
            references
                .entry(target_section)
                .or_default()
                .push(SectionReference {
                    source_section_name: source_section_name.clone(),
                    relocation_offset,
                    relocation_kind: format!("{:?}", relocation.kind()),
                    relocation_encoding: format!("{:?}", relocation.encoding()),
                    relocation_size: relocation.size(),
                    relocation_addend: relocation.addend(),
                });
        }
    }
    for signature in references.values_mut() {
        signature.sort();
    }
    Ok(references)
}

fn relocation_target_section(
    file: &object::File<'_>,
    target: object::RelocationTarget,
) -> Result<Option<object::SectionIndex>> {
    match target {
        object::RelocationTarget::Section(section) => Ok(Some(section)),
        object::RelocationTarget::Symbol(symbol) => {
            Ok(file.symbol_by_index(symbol)?.section_index())
        }
        object::RelocationTarget::Absolute => Ok(None),
        _ => Ok(None),
    }
}

fn changed_patch_sections(
    state_dir: &Path,
    previous_input: &FileState,
    current_bytes: &[u8],
    sections: &[MatchedPatchSection],
) -> Result<Option<Vec<PatchSection>>> {
    let snapshot = input_snapshot_path_for_encoded_path(state_dir, &previous_input.path);
    if !previous_input
        .content
        .identity_matches_path(&snapshot)
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let previous_bytes = match std::fs::read(&snapshot) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let previous_file =
        object::File::parse(&*previous_bytes).context("Failed to parse previous patch input")?;
    let current_file =
        object::File::parse(current_bytes).context("Failed to parse current patch input")?;
    let mut changed_sections = Vec::new();

    for patch_section in sections {
        let previous_section = previous_file
            .section_by_index(object::SectionIndex(
                patch_section.previous.section_index as usize,
            ))
            .context("Missing previous incremental patch section")?;
        let current_section = current_file
            .section_by_index(object::SectionIndex(
                patch_section.current.section_index as usize,
            ))
            .context("Missing current incremental patch section")?;
        let previous_data = previous_section
            .data()
            .context("Failed to read previous incremental patch section data")?;
        let current_data = current_section
            .data()
            .context("Failed to read current incremental patch section data")?;
        if previous_data != current_data {
            changed_sections.push(patch_section.current.clone());
        }
    }

    Ok(Some(changed_sections))
}

fn patch_section_index(
    file: &object::File<'_>,
    patch_section: &PatchSection,
) -> Result<Option<object::SectionIndex>> {
    let Some(name) = patch_section.section_name.as_deref() else {
        return Ok(Some(object::SectionIndex(
            patch_section.section_index as usize,
        )));
    };

    let mut matches = file
        .sections()
        .filter_map(|section| (section.name().ok() == Some(name)).then(|| section.index()));
    let Some(index) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Ok(None);
    }
    Ok(Some(index))
}

fn patch_sections_for_input(
    bytes: &[u8],
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<Vec<SectionPatch>>> {
    let file = object::File::parse(bytes).context("Failed to parse changed incremental input")?;
    let mut patches = Vec::new();
    for patch_section in sections {
        let Some(section_index) = patch_section_index(&file, &patch_section)? else {
            return Ok(None);
        };
        let section = file
            .section_by_index(section_index)
            .context("Missing changed incremental input section")?;
        let data = section
            .data()
            .context("Failed to read changed incremental input section data")?;
        if data.len() as u64 != patch_section.input_size
            || data.len() > patch_section.output_size as usize
        {
            return Ok(None);
        }
        patches.push(SectionPatch {
            output_offset: patch_section.output_offset,
            size: patch_section.output_size,
            data: data.to_owned(),
        });
    }
    Ok(Some(patches))
}

fn patch_ranges(
    bytes: &[u8],
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<Vec<std::ops::Range<usize>>>> {
    let file = object::File::parse(bytes).context("Failed to parse incremental patch input")?;
    let mut ranges = Vec::new();
    for patch_section in sections {
        let Some(section_index) = patch_section_index(&file, &patch_section)? else {
            return Ok(None);
        };
        let section = file
            .section_by_index(section_index)
            .context("Missing incremental patch input section")?;
        let Some((offset, size)) = section.file_range() else {
            return Ok(None);
        };
        if size != patch_section.input_size || size > patch_section.output_size {
            return Ok(None);
        }
        let start = offset as usize;
        let end = start
            .checked_add(size as usize)
            .context("Incremental patch input range overflow")?;
        if end > bytes.len() {
            return Ok(None);
        }
        ranges.push(start..end);
    }

    ranges.sort_by_key(|range| range.start);
    let mut previous_end = 0;
    for range in &ranges {
        if range.start < previous_end {
            return Ok(None);
        }
        previous_end = range.end;
    }

    if ranges.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ranges))
    }
}

fn update_hash_with_zeroes(hasher: &mut blake3::Hasher, mut len: usize) {
    const ZEROES: [u8; 4096] = [0; 4096];
    while len > 0 {
        let chunk_len = len.min(ZEROES.len());
        hasher.update(&ZEROES[..chunk_len]);
        len -= chunk_len;
    }
}

fn build_id_note_range(bytes: &[u8]) -> Result<Option<std::ops::Range<usize>>> {
    let file =
        object::File::parse(bytes).context("Failed to parse output for build ID patching")?;
    for section in file.sections() {
        if section.name_bytes()? != b".note.gnu.build-id" {
            continue;
        }
        let Some((offset, size)) = section.file_range() else {
            return Ok(None);
        };
        let start = offset as usize;
        let end = start
            .checked_add(size as usize)
            .context("Incremental build ID range overflow")?;
        if end > bytes.len() {
            return Ok(None);
        }
        return Ok(Some(start..end));
    }
    Ok(None)
}

fn write_fast_build_id_from_state(
    output: &mut [u8],
    range: std::ops::Range<usize>,
    state: &mut BuildIdHashState,
    tree: &mut [[u8; blake3::OUT_LEN]],
    changed_ranges: &[std::ops::Range<usize>],
) -> Result {
    validate_fast_build_id_range(&range)?;
    output[range.clone()].fill(0);
    let mut hash_ranges = changed_ranges.to_owned();
    hash_ranges.push(range.clone());
    let changed_chunks = touched_build_id_chunks(&hash_ranges, output.len())?;
    if !update_build_id_hash_tree(state, tree, output, &range, &changed_chunks)? {
        return Err(crate::error!(
            "Incremental build ID hash state is incompatible with the output"
        ));
    }
    let build_id = build_id_from_hash_tree(state, tree)?;
    write_fast_build_id_note(output, range, &build_id);
    Ok(())
}

fn validate_fast_build_id_range(range: &std::ops::Range<usize>) -> Result {
    const GNU_NOTE_NAME: &[u8] = b"GNU\0";
    let expected_len = 12 + GNU_NOTE_NAME.len() + blake3::OUT_LEN;
    if range.end - range.start != expected_len {
        return Err(crate::error!(
            "Incremental patching only supports fast 32-byte build IDs"
        ));
    }
    Ok(())
}

fn write_fast_build_id_note(
    output: &mut [u8],
    range: std::ops::Range<usize>,
    build_id: &blake3::Hash,
) {
    const GNU_NOTE_NAME: &[u8] = b"GNU\0";
    let note = &mut output[range];
    note[0..4].copy_from_slice(&(GNU_NOTE_NAME.len() as u32).to_le_bytes());
    note[4..8].copy_from_slice(&(blake3::OUT_LEN as u32).to_le_bytes());
    note[8..12].copy_from_slice(&object::elf::NT_GNU_BUILD_ID.to_le_bytes());
    note[12..16].copy_from_slice(GNU_NOTE_NAME);
    note[16..].copy_from_slice(build_id.as_bytes());
}

fn build_id_hash_state_from_output(
    bytes: &[u8],
) -> Result<(Option<BuildIdHashState>, Option<Vec<[u8; blake3::OUT_LEN]>>)> {
    let Some(range) = build_id_note_range(&bytes)? else {
        return Ok((None, None));
    };
    validate_fast_build_id_range(&range)?;
    let Some(nodes) = build_id_hash_node_count(bytes.len()) else {
        return Ok((None, None));
    };
    let mut tree = Vec::with_capacity(nodes);
    let left_len = blake3::hazmat::left_subtree_len(bytes.len() as u64) as usize;
    build_id_subtree_hash(bytes, 0, left_len, &range, &mut tree);
    build_id_subtree_hash(bytes, left_len, bytes.len() - left_len, &range, &mut tree);
    debug_assert_eq!(tree.len(), nodes);
    Ok((
        Some(BuildIdHashState {
            output_len: bytes.len() as u64,
            nodes,
        }),
        Some(tree),
    ))
}

fn build_id_hash_node_count(len: usize) -> Option<usize> {
    if len <= BUILD_ID_HASH_GROUP_LEN {
        return None;
    }
    let left_len = blake3::hazmat::left_subtree_len(len as u64) as usize;
    Some(build_id_subtree_node_count(left_len) + build_id_subtree_node_count(len - left_len))
}

fn build_id_subtree_node_count(len: usize) -> usize {
    2 * len.div_ceil(BUILD_ID_HASH_GROUP_LEN) - 1
}

fn build_id_subtree_hash(
    bytes: &[u8],
    start: usize,
    len: usize,
    zero_range: &std::ops::Range<usize>,
    tree: &mut Vec<[u8; blake3::OUT_LEN]>,
) -> [u8; blake3::OUT_LEN] {
    let index = tree.len();
    tree.push([0; blake3::OUT_LEN]);
    let hash = if len <= BUILD_ID_HASH_GROUP_LEN {
        build_id_leaf_hash(bytes, start, len, zero_range)
    } else {
        let left_len = blake3::hazmat::left_subtree_len(len as u64) as usize;
        let left = build_id_subtree_hash(bytes, start, left_len, zero_range, tree);
        let right =
            build_id_subtree_hash(bytes, start + left_len, len - left_len, zero_range, tree);
        blake3::hazmat::merge_subtrees_non_root(&left, &right, blake3::hazmat::Mode::Hash)
    };
    tree[index] = hash;
    hash
}

fn update_build_id_hash_tree(
    state: &mut BuildIdHashState,
    tree: &mut [[u8; blake3::OUT_LEN]],
    output: &[u8],
    zero_range: &std::ops::Range<usize>,
    changed_chunks: &[usize],
) -> Result<bool> {
    if state.output_len != output.len() as u64 {
        return Ok(false);
    }
    if Some(state.nodes) != build_id_hash_node_count(output.len()) {
        return Ok(false);
    }
    if tree.len() != state.nodes {
        return Ok(false);
    }
    if output.len() <= BUILD_ID_HASH_GROUP_LEN {
        return Ok(false);
    }
    let left_len = blake3::hazmat::left_subtree_len(output.len() as u64) as usize;
    update_build_id_subtree_hash(tree, 0, output, 0, left_len, zero_range, changed_chunks);
    let right_index = build_id_subtree_node_count(left_len);
    update_build_id_subtree_hash(
        tree,
        right_index,
        output,
        left_len,
        output.len() - left_len,
        zero_range,
        changed_chunks,
    );
    Ok(true)
}

fn update_build_id_subtree_hash(
    tree: &mut [[u8; blake3::OUT_LEN]],
    index: usize,
    output: &[u8],
    start: usize,
    len: usize,
    zero_range: &std::ops::Range<usize>,
    changed_chunks: &[usize],
) -> bool {
    if !touched_chunks_overlap(changed_chunks, start, len) {
        return false;
    }
    if len <= BUILD_ID_HASH_GROUP_LEN {
        tree[index] = build_id_leaf_hash(output, start, len, zero_range);
        return true;
    }

    let left_len = blake3::hazmat::left_subtree_len(len as u64) as usize;
    let left_index = index + 1;
    let right_index = left_index + build_id_subtree_node_count(left_len);
    let left_changed = update_build_id_subtree_hash(
        tree,
        left_index,
        output,
        start,
        left_len,
        zero_range,
        changed_chunks,
    );
    let right_changed = update_build_id_subtree_hash(
        tree,
        right_index,
        output,
        start + left_len,
        len - left_len,
        zero_range,
        changed_chunks,
    );
    if left_changed || right_changed {
        tree[index] = blake3::hazmat::merge_subtrees_non_root(
            &tree[left_index],
            &tree[right_index],
            blake3::hazmat::Mode::Hash,
        );
    }
    left_changed || right_changed
}

fn build_id_leaf_hash(
    bytes: &[u8],
    start: usize,
    len: usize,
    zero_range: &std::ops::Range<usize>,
) -> [u8; blake3::OUT_LEN] {
    let end = start + len;
    let chunk = &bytes[start..end];
    if let Some(overlap) = intersect_ranges(start..end, zero_range.clone()) {
        let mut zeroed = chunk.to_vec();
        zeroed[overlap.start - start..overlap.end - start].fill(0);
        build_id_leaf_hash_bytes(&zeroed, start)
    } else {
        build_id_leaf_hash_bytes(chunk, start)
    }
}

fn build_id_leaf_hash_bytes(bytes: &[u8], start: usize) -> [u8; blake3::OUT_LEN] {
    use blake3::hazmat::HasherExt as _;
    blake3::Hasher::new()
        .set_input_offset(start as u64)
        .update(bytes)
        .finalize_non_root()
}

fn build_id_from_hash_tree(
    state: &BuildIdHashState,
    tree: &[[u8; blake3::OUT_LEN]],
) -> Result<blake3::Hash> {
    let len = usize::try_from(state.output_len)
        .context("Incremental build ID hash output length is too large")?;
    if Some(state.nodes) != build_id_hash_node_count(len) {
        return Err(crate::error!(
            "Incremental build ID hash state does not match output length"
        ));
    }
    if tree.len() != state.nodes {
        return Err(crate::error!(
            "Incremental build ID hash tree size does not match state"
        ));
    }
    let left_len = blake3::hazmat::left_subtree_len(len as u64);
    let right_index = build_id_subtree_node_count(left_len as usize);
    Ok(blake3::hazmat::merge_subtrees_root(
        &tree[0],
        &tree[right_index],
        blake3::hazmat::Mode::Hash,
    ))
}

fn touched_build_id_chunks(
    ranges: &[std::ops::Range<usize>],
    output_len: usize,
) -> Result<Vec<usize>> {
    let mut chunks = Vec::new();
    for range in ranges {
        if range.start > range.end || range.end > output_len {
            return Err(crate::error!("Incremental build ID patch range is invalid"));
        }
        if range.is_empty() {
            continue;
        }
        let first = range.start / BUILD_ID_HASH_GROUP_LEN;
        let last = (range.end - 1) / BUILD_ID_HASH_GROUP_LEN;
        chunks.extend(first..=last);
    }
    chunks.sort_unstable();
    chunks.dedup();
    Ok(chunks)
}

fn touched_chunks_overlap(chunks: &[usize], start: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let first = start / BUILD_ID_HASH_GROUP_LEN;
    let last = (start + len - 1) / BUILD_ID_HASH_GROUP_LEN;
    chunks.iter().any(|chunk| (first..=last).contains(chunk))
}

fn intersect_ranges(
    left: std::ops::Range<usize>,
    right: std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let start = left.start.max(right.start);
    let end = left.end.min(right.end);
    (start < end).then_some(start..end)
}

fn read_build_id_hash_tree(
    state_dir: &Path,
    state: &BuildIdHashState,
) -> Result<Vec<[u8; blake3::OUT_LEN]>> {
    let len = usize::try_from(state.output_len)
        .context("Incremental build ID hash output length is too large")?;
    if Some(state.nodes) != build_id_hash_node_count(len) {
        return Err(crate::error!(
            "Incremental build ID hash state does not match output length"
        ));
    }
    let path = state_dir.join(BUILD_ID_HASH_FILE);
    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "Failed to read incremental build ID hash `{}`",
            path.display()
        )
    })?;
    let expected_len = state.nodes * blake3::OUT_LEN;
    if bytes.len() != expected_len {
        return Err(crate::error!(
            "Incremental build ID hash tree has {} bytes, expected {expected_len}",
            bytes.len()
        ));
    }
    Ok(bytes
        .chunks_exact(blake3::OUT_LEN)
        .map(|chunk| chunk.try_into().unwrap())
        .collect())
}

fn write_build_id_hash_tree(state_dir: &Path, tree: Option<&[[u8; blake3::OUT_LEN]]>) -> Result {
    let path = state_dir.join(BUILD_ID_HASH_FILE);
    let Some(tree) = tree else {
        let _ = std::fs::remove_file(path);
        return Ok(());
    };
    std::fs::create_dir_all(state_dir).with_context(|| {
        format!(
            "Failed to create incremental state directory `{}`",
            state_dir.display()
        )
    })?;
    let tmp_path = state_dir.join(format!("{BUILD_ID_HASH_FILE}.tmp"));
    let mut bytes = Vec::with_capacity(tree.len() * blake3::OUT_LEN);
    for node in tree {
        bytes.extend_from_slice(node);
    }
    std::fs::write(&tmp_path, bytes).with_context(|| {
        format!(
            "Failed to write incremental build ID hash `{}`",
            tmp_path.display()
        )
    })?;
    let _ = std::fs::remove_file(&path);
    std::fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "Failed to install incremental build ID hash `{}`",
            path.display()
        )
    })?;
    Ok(())
}

fn fingerprint_loaded_files(
    file_loader: &FileLoader<'_>,
    previous: Option<&PersistedState>,
) -> Vec<FileState> {
    let previous_by_path = previous.map(|previous| {
        previous
            .input_files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect::<HashMap<_, _>>()
    });

    let mut files = file_loader
        .loaded_files
        .iter()
        .map(|input_file| {
            let path = encode_path(&input_file.filename);
            let previous = previous_by_path
                .as_ref()
                .and_then(|previous| previous.get(path.as_str()).copied());
            let content =
                FileContentState::from_input_file(input_file, previous.map(|file| &file.content));
            let patch = previous
                .filter(|previous| previous.content == content)
                .and_then(|previous| previous.patch.clone());
            FileState {
                path,
                content,
                patch,
            }
        })
        .collect::<Vec<_>>();

    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn parse_prefixed_line<'a>(line: Option<&'a str>, expected_prefix: &str) -> Result<&'a str> {
    let line = line.context("Missing incremental state line")?;
    let (prefix, rest) = line
        .split_once('\t')
        .context("Malformed incremental state line")?;
    if prefix != expected_prefix {
        return Err(crate::error!(
            "Expected incremental state line `{expected_prefix}`, got `{prefix}`"
        ));
    }
    Ok(rest)
}

fn parse_content_line(line: Option<&str>, expected_prefix: &str) -> Result<FileContentState> {
    let rest = parse_prefixed_line(line, expected_prefix)?;
    let mut parts = rest.split('\t');
    let len = parts
        .next()
        .context("Malformed incremental content length")?;
    let hash = parts.next().context("Malformed incremental content hash")?;
    let identity = parts.next().map(FileIdentity::parse).transpose()?.flatten();
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental content record"));
    }
    Ok(FileContentState {
        len: len.parse().context("Invalid incremental content length")?,
        hash: hash.to_owned(),
        identity,
    })
}

fn parse_build_id_hash_line(line: Option<&str>) -> Result<Option<BuildIdHashState>> {
    let rest = parse_prefixed_line(line, "build-id-hash")?;
    if rest == ABSENT_FIELD {
        return Ok(None);
    }
    let mut parts = rest.split('\t');
    let output_len = parts
        .next()
        .context("Malformed incremental build ID hash output length")?
        .parse()
        .context("Invalid incremental build ID hash output length")?;
    let nodes = parts
        .next()
        .context("Malformed incremental build ID hash node count")?
        .parse()
        .context("Invalid incremental build ID hash node count")?;
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental build ID hash record"));
    }
    if nodes == 0 {
        return Err(crate::error!("Missing incremental build ID hash nodes"));
    }
    Ok(Some(BuildIdHashState { output_len, nodes }))
}

fn parse_compact_sections_block<'a>(
    mut lines: impl Iterator<Item = &'a str>,
) -> Result<Vec<SectionRecord>> {
    let section_input_count: usize = parse_prefixed_line(lines.next(), "section-inputs")?
        .parse()
        .context("Invalid incremental section input count")?;
    let mut section_inputs = Vec::with_capacity(section_input_count);
    for _ in 0..section_input_count {
        let line = lines
            .next()
            .context("Missing incremental section input record")?;
        section_inputs.push(parse_section_input_line(line)?);
    }

    let section_count: usize = parse_prefixed_line(lines.next(), "sections")?
        .parse()
        .context("Invalid incremental section count")?;
    let mut sections = Vec::with_capacity(section_count);
    for _ in 0..section_count {
        let line = lines.next().context("Missing incremental section record")?;
        sections.push(parse_compact_section_line(line, &section_inputs)?);
    }
    if lines.next().is_some() {
        return Err(crate::error!(
            "Unexpected trailing incremental section data"
        ));
    }
    Ok(sections)
}

fn parse_input_line(line: &str) -> Result<FileState> {
    let rest = parse_prefixed_line(Some(line), "input")?;
    let mut parts = rest.split('\t');
    let path = parts
        .next()
        .context("Malformed incremental input path")?
        .to_owned();
    let len = parts
        .next()
        .context("Malformed incremental input length")?
        .parse()
        .context("Invalid incremental input length")?;
    let hash = parts
        .next()
        .context("Malformed incremental input hash")?
        .to_owned();
    let identity = parts.next().map(FileIdentity::parse).transpose()?.flatten();
    let patch_fingerprint = parts
        .next()
        .filter(|fingerprint| *fingerprint != ABSENT_FIELD);
    let patch_sections = parts.next().filter(|sections| *sections != ABSENT_FIELD);
    let patch = patch_fingerprint
        .zip(patch_sections)
        .map(|(fingerprint, sections)| {
            parse_patch_sections(sections).map(|sections| FilePatchState {
                fingerprint: fingerprint.to_owned(),
                sections,
            })
        })
        .transpose()?;
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental input record"));
    }
    Ok(FileState {
        path,
        content: FileContentState {
            len,
            hash,
            identity,
        },
        patch,
    })
}

fn render_patch_sections(patch: &FilePatchState) -> String {
    patch
        .sections
        .iter()
        .map(|section| {
            format!(
                "{}:{}:{}:{}:{}",
                section.section_index,
                section.input_size,
                section.output_offset,
                section.output_size,
                section
                    .section_name
                    .as_ref()
                    .map(|name| hex::encode(name.as_bytes()))
                    .unwrap_or_else(|| ABSENT_FIELD.to_owned())
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn render_build_id_hash_state(state: &BuildIdHashState) -> String {
    format!("{}\t{}", state.output_len, state.nodes)
}

fn parse_patch_sections(sections: &str) -> Result<Vec<FilePatchSectionState>> {
    if sections.is_empty() {
        return Ok(Vec::new());
    }
    let mut parsed = Vec::new();
    for section in sections.split(',') {
        let parts = section.split(':').collect::<Vec<_>>();
        if parts.len() != 4 && parts.len() != 5 {
            return Ok(Vec::new());
        }
        let section_name = parts
            .get(4)
            .copied()
            .filter(|name| *name != ABSENT_FIELD)
            .map(|name| {
                let bytes =
                    hex::decode(name).context("Malformed incremental patch section name")?;
                String::from_utf8(bytes).context("Invalid incremental patch section name")
            })
            .transpose()?;
        parsed.push(FilePatchSectionState {
            section_index: parts[0]
                .parse()
                .context("Invalid incremental patch section index")?,
            section_name,
            input_size: parts[1]
                .parse()
                .context("Invalid incremental patch section input size")?,
            output_offset: parts[2]
                .parse()
                .context("Invalid incremental patch section output offset")?,
            output_size: parts[3]
                .parse()
                .context("Invalid incremental patch section output size")?,
        });
    }
    Ok(parsed)
}

fn parse_section_line(line: &str) -> Result<SectionRecord> {
    let rest = parse_prefixed_line(Some(line), "section")?;
    let mut parts = rest.split('\t');
    let input_file = parts
        .next()
        .context("Malformed incremental section input file")?
        .to_owned();
    let input = parts
        .next()
        .context("Malformed incremental section input")?
        .to_owned();
    let section_index = parts
        .next()
        .context("Malformed incremental section index")?
        .parse()
        .context("Invalid incremental section index")?;
    let output_offset = parts
        .next()
        .context("Malformed incremental section output offset")?
        .parse()
        .context("Invalid incremental section output offset")?;
    let size = parts
        .next()
        .context("Malformed incremental section size")?
        .parse()
        .context("Invalid incremental section size")?;
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental section record"));
    }
    Ok(SectionRecord {
        input_file,
        input,
        section_index,
        output_offset,
        size,
    })
}

fn parse_section_input_line(line: &str) -> Result<(String, String)> {
    let rest = parse_prefixed_line(Some(line), "section-input")?;
    let mut parts = rest.split('\t');
    let input_file = parts
        .next()
        .context("Malformed incremental section input file")?
        .to_owned();
    let input = parts
        .next()
        .context("Malformed incremental section input")?
        .to_owned();
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental section input record"));
    }
    Ok((input_file, input))
}

fn parse_compact_section_line(
    line: &str,
    section_inputs: &[(String, String)],
) -> Result<SectionRecord> {
    let rest = parse_prefixed_line(Some(line), "section")?;
    let mut parts = rest.split('\t');
    let section_input_id: usize = parts
        .next()
        .context("Malformed incremental section input index")?
        .parse()
        .context("Invalid incremental section input index")?;
    let section_index = parts
        .next()
        .context("Malformed incremental section index")?
        .parse()
        .context("Invalid incremental section index")?;
    let output_offset = parts
        .next()
        .context("Malformed incremental section output offset")?
        .parse()
        .context("Invalid incremental section output offset")?;
    let size = parts
        .next()
        .context("Malformed incremental section size")?
        .parse()
        .context("Invalid incremental section size")?;
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental section record"));
    }
    let (input_file, input) = section_inputs
        .get(section_input_id)
        .context("Incremental section input index out of bounds")?;
    Ok(SectionRecord {
        input_file: input_file.clone(),
        input: input.clone(),
        section_index,
        output_offset,
        size,
    })
}

fn snapshot_loaded_files(state_dir: &Path, file_loader: &FileLoader<'_>) -> Result<usize> {
    snapshot_input_paths(
        state_dir,
        file_loader
            .loaded_files
            .iter()
            .map(|input_file| input_file.filename.as_path()),
    )
}

fn input_content_matches_snapshot(
    state_dir: &Path,
    previous_input: &FileState,
    current_path: &Path,
) -> Result<bool> {
    let snapshot = input_snapshot_path_for_encoded_path(state_dir, &previous_input.path);
    if !previous_input
        .content
        .identity_matches_path(&snapshot)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    files_equal(&snapshot, current_path)
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let left = match std::fs::read(left) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let right = match std::fs::read(right) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(left == right)
}

fn refresh_input_file_identities(input_files: &mut [FileState]) {
    for input in input_files {
        let Ok(path) = decode_path(&input.path) else {
            continue;
        };
        let Ok(Some(identity)) = FileIdentity::from_path(&path) else {
            continue;
        };
        input.content.len = identity.len;
        input.content.identity = Some(identity);
    }
}

fn snapshot_input_paths<'a>(
    state_dir: &Path,
    paths: impl IntoIterator<Item = &'a Path>,
) -> Result<usize> {
    let mut seen = HashSet::new();
    let mut snapshotted = 0;
    for path in paths {
        if !seen.insert(encode_path(path)) {
            continue;
        }
        if snapshot_input_path(state_dir, path)? {
            snapshotted += 1;
        }
    }
    Ok(snapshotted)
}

fn snapshot_input_path(state_dir: &Path, path: &Path) -> Result<bool> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    if !metadata.is_file() || metadata.permissions().readonly() {
        return Ok(false);
    }

    let snapshot_dir = input_snapshot_dir(state_dir);
    std::fs::create_dir_all(&snapshot_dir).with_context(|| {
        format!(
            "Failed to create incremental input snapshot directory `{}`",
            snapshot_dir.display()
        )
    })?;

    let target = input_snapshot_path(state_dir, path);
    let tmp = target.with_file_name(format!(
        "{}.{}.tmp",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("input"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);

    if std::fs::hard_link(path, &tmp).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }

    let _ = std::fs::remove_file(&target);
    std::fs::rename(&tmp, &target).with_context(|| {
        format!(
            "Failed to install incremental input snapshot `{}`",
            target.display()
        )
    })?;
    Ok(true)
}

fn input_snapshot_path(state_dir: &Path, path: &Path) -> PathBuf {
    input_snapshot_path_for_encoded_path(state_dir, &encode_path(path))
}

fn input_snapshot_path_for_encoded_path(state_dir: &Path, encoded_path: &str) -> PathBuf {
    input_snapshot_dir(state_dir).join(hash_text(encoded_path))
}

fn input_snapshot_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(INPUT_SNAPSHOT_DIR)
}

fn append_log(state_dir: &Path, message: &str) -> Result {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join(LOG_FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open incremental log `{}`", path.display()))?;
    writeln!(file, "{message}")?;
    let _ = append_global_log(state_dir, message);
    Ok(())
}

fn append_global_log(state_dir: &Path, message: &str) -> Result {
    let Some(log_dir) = user_state_dir() else {
        return Ok(());
    };
    append_global_log_to(&log_dir, state_dir, message)
}

fn append_global_log_to(log_dir: &Path, state_dir: &Path, message: &str) -> Result {
    std::fs::create_dir_all(log_dir)?;
    let path = global_log_path_in(log_dir);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open incremental global log `{}`", path.display()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    writeln!(file, "{timestamp}\t{}\t{message}", state_dir.display())?;
    Ok(())
}

pub(crate) fn print_global_log(mut writer: impl std::io::Write) -> Result {
    let Some(log_dir) = user_state_dir() else {
        return Ok(());
    };
    print_global_log_from(&log_dir, &mut writer)
}

fn print_global_log_from(log_dir: &Path, writer: &mut impl std::io::Write) -> Result {
    let path = global_log_path_in(log_dir);
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read incremental log `{}`", path.display()));
        }
    };
    writer
        .write_all(&contents)
        .with_context(|| format!("Failed to write incremental log `{}`", path.display()))?;
    Ok(())
}

fn global_log_path_in(log_dir: &Path) -> PathBuf {
    log_dir.join(GLOBAL_LOG_FILE)
}

fn user_state_dir() -> Option<PathBuf> {
    user_state_dir_from_env(|name| std::env::var_os(name))
}

fn user_state_dir_from_env(mut env: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(path) = env(USER_STATE_DIR_ENV) {
        return Some(PathBuf::from(path));
    }

    #[cfg(target_os = "macos")]
    {
        env("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("wild")
        })
    }

    #[cfg(not(target_os = "macos"))]
    {
        if let Some(path) = env("XDG_STATE_HOME") {
            return Some(PathBuf::from(path).join("wild"));
        }
        env("HOME").map(|home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("wild")
        })
    }
}

fn state_dir_for_output(output: &Path) -> PathBuf {
    append_suffix(output, ".incr")
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

fn encode_path(path: &Path) -> String {
    hex::encode(path.as_os_str().as_encoded_bytes())
}

#[cfg(unix)]
fn decode_path(path: &str) -> Result<PathBuf> {
    let bytes = hex::decode(path).context("Malformed incremental path encoding")?;
    Ok(std::ffi::OsString::from_vec(bytes).into())
}

#[cfg(not(unix))]
fn decode_path(path: &str) -> Result<PathBuf> {
    let bytes = hex::decode(path).context("Malformed incremental path encoding")?;
    Ok(String::from_utf8_lossy(&bytes).into_owned().into())
}

fn encode_input_ref(input: InputRef<'_>) -> String {
    let mut bytes = input.file.filename.as_os_str().as_encoded_bytes().to_vec();
    if let Some(entry) = input.entry {
        bytes.push(0);
        bytes.extend_from_slice(entry.identifier.as_slice());
        bytes.push(0);
        bytes.extend_from_slice(entry.start_offset.to_string().as_bytes());
        bytes.push(b':');
        bytes.extend_from_slice(entry.end_offset.to_string().as_bytes());
    }
    hex::encode(bytes)
}

fn display_hex_path(path: &str) -> String {
    let bytes = hex::decode(path).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hash_text(text: &str) -> String {
    hash_bytes(text.as_bytes())
}

fn args_hash(args: &impl platform::Args) -> String {
    hash_text(&format!("{args:?}"))
}

fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(args_hash: &str, output: &[u8], inputs: &[(&str, &[u8])]) -> PersistedState {
        PersistedState {
            args_hash: args_hash.to_owned(),
            output: FileContentState::from_bytes(output),
            build_id_hashes: None,
            input_files: inputs
                .iter()
                .map(|(path, bytes)| FileState {
                    path: hex::encode(path),
                    content: FileContentState::from_bytes(bytes),
                    patch: None,
                })
                .collect(),
            sections: Vec::new(),
            sections_file: None,
        }
    }

    fn section_record(
        input: &str,
        section_index: u32,
        output_offset: u64,
        size: u64,
    ) -> SectionRecord {
        SectionRecord {
            input_file: hex::encode(input),
            input: hex::encode(input),
            section_index,
            output_offset,
            size,
        }
    }

    fn section_reference(source_section_name: &str, relocation_offset: u64) -> SectionReference {
        SectionReference {
            source_section_name: source_section_name.to_owned(),
            relocation_offset,
            relocation_kind: "Absolute".to_owned(),
            relocation_encoding: "Generic".to_owned(),
            relocation_size: 64,
            relocation_addend: 0,
        }
    }

    fn render_legacy_state(state: &PersistedState, version: &str) -> String {
        let mut out = String::new();
        writeln!(&mut out, "{version}").unwrap();
        writeln!(&mut out, "args\t{}", state.args_hash).unwrap();
        writeln!(
            &mut out,
            "output\t{}\t{}\t{}",
            state.output.len,
            state.output.hash,
            state.output.render_identity()
        )
        .unwrap();
        writeln!(&mut out, "inputs\t{}", state.input_files.len()).unwrap();
        for input in &state.input_files {
            writeln!(
                &mut out,
                "input\t{}\t{}\t{}\t{}",
                input.path,
                input.content.len,
                input.content.hash,
                input.content.render_identity()
            )
            .unwrap();
        }
        writeln!(&mut out, "sections\t{}", state.sections.len()).unwrap();
        for section in &state.sections {
            writeln!(
                &mut out,
                "section\t{}\t{}\t{}\t{}\t{}",
                section.input_file,
                section.input,
                section.section_index,
                section.output_offset,
                section.size
            )
            .unwrap();
        }
        out
    }

    fn render_v8_state(state: &PersistedState) -> String {
        state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V8, 1)
            .lines()
            .filter(|line| !line.starts_with("build-id-hash\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            })
    }

    fn identity(len: u64, dev: u64, ino: u64, modified_sec: i64, changed_sec: i64) -> FileIdentity {
        FileIdentity {
            len,
            dev,
            ino,
            modified_sec,
            modified_nsec: 0,
            changed_sec,
            changed_nsec: 0,
        }
    }

    #[test]
    fn state_dir_appends_suffix() {
        assert_eq!(
            state_dir_for_output(Path::new("target/debug/app")),
            Path::new("target/debug/app.incr")
        );
        assert_eq!(
            state_dir_for_output(Path::new("target/debug/app.so")),
            Path::new("target/debug/app.so.incr")
        );
    }

    #[test]
    fn user_state_dir_uses_override() {
        let dir = user_state_dir_from_env(|name| {
            (name == USER_STATE_DIR_ENV).then(|| OsString::from("/tmp/wild-state"))
        });

        assert_eq!(dir, Some(PathBuf::from("/tmp/wild-state")));
    }

    #[test]
    fn user_state_dir_uses_platform_default() {
        let dir = user_state_dir_from_env(|name| match name {
            "HOME" => Some(OsString::from("/home/wild")),
            _ => None,
        })
        .unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!(
            dir,
            PathBuf::from("/home/wild")
                .join("Library")
                .join("Application Support")
                .join("wild")
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            dir,
            PathBuf::from("/home/wild")
                .join(".local")
                .join("state")
                .join("wild")
        );
    }

    #[test]
    fn user_state_dir_prefers_xdg_state_home_on_non_macos() {
        let dir = user_state_dir_from_env(|name| match name {
            "HOME" => Some(OsString::from("/home/wild")),
            "XDG_STATE_HOME" => Some(OsString::from("/state")),
            _ => None,
        })
        .unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!(
            dir,
            PathBuf::from("/home/wild")
                .join("Library")
                .join("Application Support")
                .join("wild")
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(dir, PathBuf::from("/state").join("wild"));
    }

    #[test]
    fn global_log_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        append_global_log_to(
            dir.path(),
            Path::new("target/debug/app.incr"),
            "full relink: no previous incremental state",
        )
        .unwrap();

        let mut out = Vec::new();
        print_global_log_from(dir.path(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();

        assert!(
            out.contains("\ttarget/debug/app.incr\tfull relink: no previous incremental state\n")
        );
    }

    #[test]
    fn input_snapshot_path_is_stable_for_input_path() {
        let state_dir = Path::new("target/debug/app.incr");
        assert_eq!(
            input_snapshot_path(state_dir, Path::new("obj/main.o")),
            input_snapshot_path(state_dir, Path::new("obj/main.o"))
        );
        assert_ne!(
            input_snapshot_path(state_dir, Path::new("obj/main.o")),
            input_snapshot_path(state_dir, Path::new("obj/other.o"))
        );
    }

    #[test]
    fn input_snapshots_are_hard_links() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("app.incr");
        let input = dir.path().join("input.o");
        std::fs::write(&input, b"object").unwrap();
        let mut input_files = vec![FileState {
            path: encode_path(&input),
            content: FileContentState::from_path_identity_only(&input).unwrap(),
            patch: None,
        }];

        assert_eq!(
            snapshot_input_paths(&state_dir, [input.as_path()]).unwrap(),
            1
        );
        refresh_input_file_identities(&mut input_files);

        let snapshot = input_snapshot_path(&state_dir, &input);
        assert_eq!(std::fs::read(&snapshot).unwrap(), b"object");
        assert!(
            input_files[0]
                .content
                .identity_matches_path(&input)
                .unwrap()
        );

        #[cfg(unix)]
        {
            let input_metadata = std::fs::metadata(&input).unwrap();
            let snapshot_metadata = std::fs::metadata(&snapshot).unwrap();
            assert_eq!(input_metadata.dev(), snapshot_metadata.dev());
            assert_eq!(input_metadata.ino(), snapshot_metadata.ino());
        }
    }

    #[test]
    fn input_snapshots_deduplicate_paths() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("app.incr");
        let input = dir.path().join("input.o");
        std::fs::write(&input, b"object").unwrap();

        assert_eq!(
            snapshot_input_paths(&state_dir, [input.as_path(), input.as_path()]).unwrap(),
            1
        );
    }

    #[test]
    fn input_snapshot_matches_rewritten_file_with_same_content() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("app.incr");
        let input = dir.path().join("input.o");
        std::fs::write(&input, b"object").unwrap();

        snapshot_input_paths(&state_dir, [input.as_path()]).unwrap();
        let mut previous = FileState {
            path: encode_path(&input),
            content: FileContentState::from_path_identity_only(&input).unwrap(),
            patch: None,
        };
        refresh_input_file_identities(std::slice::from_mut(&mut previous));

        let replacement = dir.path().join("replacement.o");
        std::fs::write(&replacement, b"object").unwrap();
        std::fs::rename(&replacement, &input).unwrap();

        assert!(!previous.content.identity_matches_path(&input).unwrap());
        assert!(input_content_matches_snapshot(&state_dir, &previous, &input).unwrap());
    }

    #[test]
    fn input_snapshot_rejects_rewritten_file_with_changed_content() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("app.incr");
        let input = dir.path().join("input.o");
        std::fs::write(&input, b"object").unwrap();

        snapshot_input_paths(&state_dir, [input.as_path()]).unwrap();
        let mut previous = FileState {
            path: encode_path(&input),
            content: FileContentState::from_path_identity_only(&input).unwrap(),
            patch: None,
        };
        refresh_input_file_identities(std::slice::from_mut(&mut previous));

        let replacement = dir.path().join("replacement.o");
        std::fs::write(&replacement, b"changed").unwrap();
        std::fs::rename(&replacement, &input).unwrap();

        assert!(!input_content_matches_snapshot(&state_dir, &previous, &input).unwrap());
    }

    #[test]
    fn changed_patch_sections_identifies_changed_section() {
        let Ok(current_exe) = std::env::current_exe() else {
            return;
        };
        let Ok(bytes) = std::fs::read(&current_exe) else {
            return;
        };
        let Ok(object) = object::File::parse(&*bytes) else {
            return;
        };
        let Some(section) = object.section_by_name(".data") else {
            return;
        };
        let Some((offset, size)) = section.file_range() else {
            return;
        };
        if size == 0 {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("app.incr");
        let input = dir.path().join("input.o");
        std::fs::write(&input, &bytes).unwrap();
        snapshot_input_paths(&state_dir, [input.as_path()]).unwrap();
        let snapshot = input_snapshot_path(&state_dir, &input);
        let previous = FileState {
            path: encode_path(&input),
            content: FileContentState::from_path_identity_only(&snapshot).unwrap(),
            patch: None,
        };
        let mut current = bytes.clone();
        current[offset as usize] ^= 1;
        let patch_section = PatchSection {
            section_index: section.index().0 as u32,
            section_name: section.name().ok().map(str::to_owned),
            input_size: size,
            output_offset: 64,
            output_size: size,
        };

        assert_eq!(
            changed_patch_sections(
                &state_dir,
                &previous,
                &current,
                &[MatchedPatchSection::same(patch_section)]
            )
            .unwrap()
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn reference_matching_resolves_unique_anonymous_section() {
        let signature = vec![section_reference(".text.foo", 12)];
        let current_references = HashMap::from([
            (
                object::SectionIndex(3),
                vec![section_reference(".text.bar", 4)],
            ),
            (object::SectionIndex(7), signature.clone()),
        ]);

        assert_eq!(
            match_section_by_references(&signature, &current_references),
            Some(object::SectionIndex(7))
        );
    }

    #[test]
    fn reference_matching_rejects_ambiguous_anonymous_section() {
        let signature = vec![section_reference(".text.foo", 12)];
        let current_references = HashMap::from([
            (object::SectionIndex(3), signature.clone()),
            (object::SectionIndex(7), signature.clone()),
        ]);

        assert_eq!(
            match_section_by_references(&signature, &current_references),
            None
        );
    }

    #[test]
    fn patch_sections_for_input_rejects_section_size_changes() {
        let Ok(current_exe) = std::env::current_exe() else {
            return;
        };
        let Ok(bytes) = std::fs::read(&current_exe) else {
            return;
        };
        let Ok(object) = object::File::parse(&*bytes) else {
            return;
        };
        let Some(section) = object
            .sections()
            .find(|section| section.file_range().is_some_and(|(_, size)| size > 0))
        else {
            return;
        };
        let Some((_, size)) = section.file_range() else {
            return;
        };
        let patch_section = PatchSection {
            section_index: section.index().0 as u32,
            section_name: section.name().ok().map(str::to_owned),
            input_size: size + 1,
            output_offset: 64,
            output_size: size + 1,
        };

        assert!(
            patch_sections_for_input(&bytes, [patch_section])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn patch_sections_for_input_resolves_unique_section_names() {
        let Ok(current_exe) = std::env::current_exe() else {
            return;
        };
        let Ok(bytes) = std::fs::read(&current_exe) else {
            return;
        };
        let Ok(object) = object::File::parse(&*bytes) else {
            return;
        };
        let mut selected = None;
        for section in object.sections() {
            let Ok(name) = section.name() else {
                continue;
            };
            let Some((_, size)) = section.file_range() else {
                continue;
            };
            if size == 0
                || object
                    .sections()
                    .filter(|s| s.name().ok() == Some(name))
                    .count()
                    != 1
            {
                continue;
            }
            selected = Some((name.to_owned(), size));
            break;
        }
        let Some((section_name, size)) = selected else {
            return;
        };
        let patch_section = PatchSection {
            section_index: u32::MAX,
            section_name: Some(section_name),
            input_size: size,
            output_offset: 64,
            output_size: size,
        };

        assert!(
            patch_ranges(&bytes, [patch_section.clone()])
                .unwrap()
                .is_some()
        );
        assert!(
            patch_sections_for_input(&bytes, [patch_section])
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn generated_local_section_names_are_not_stable_for_patch_matching() {
        assert!(section_name_is_stable_for_patch_matching(".text.symbol"));
        assert!(section_name_is_stable_for_patch_matching(".data.my_static"));
        assert!(!section_name_is_stable_for_patch_matching(""));
        assert!(!section_name_is_stable_for_patch_matching(
            ".rodata..L__unnamed_75"
        ));
        assert!(!section_name_is_stable_for_patch_matching(
            ".data.rel.ro..L__unnamed_12"
        ));
    }

    #[test]
    fn strictly_ordered_or_no_gap_sections_are_not_directly_patchable() {
        assert!(section_name_allows_direct_patching(".text.foo"));
        assert!(section_name_allows_direct_patching(".data.foo"));
        assert!(!section_name_allows_direct_patching(".init"));
        assert!(!section_name_allows_direct_patching(".fini"));
        assert!(!section_name_allows_direct_patching(".init_array"));
        assert!(!section_name_allows_direct_patching(".init_array.100"));
        assert!(!section_name_allows_direct_patching(".fini_array"));
        assert!(!section_name_allows_direct_patching(".preinit_array"));
        assert!(!section_name_allows_direct_patching(".ctors"));
        assert!(!section_name_allows_direct_patching(".dtors"));
    }

    #[test]
    fn persisted_state_round_trips() {
        let mut state = state("args", b"output", &[("a.o", b"a"), ("b.o", b"bbb")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        assert_eq!(PersistedState::parse(&state.render()).unwrap(), state);
    }

    #[test]
    fn persisted_state_round_trips_patch_metadata() {
        let mut state = state("args", b"output", &[("a.o", b"a"), ("b.o", b"bbb")]);
        state.input_files[0].patch = Some(FilePatchState {
            fingerprint: "patch-hash".to_owned(),
            sections: vec![
                FilePatchSectionState {
                    section_index: 1,
                    section_name: Some(".text.foo".to_owned()),
                    input_size: 4,
                    output_offset: 100,
                    output_size: 4,
                },
                FilePatchSectionState {
                    section_index: 3,
                    section_name: Some(".data".to_owned()),
                    input_size: 8,
                    output_offset: 112,
                    output_size: 12,
                },
                FilePatchSectionState {
                    section_index: 5,
                    section_name: None,
                    input_size: 16,
                    output_offset: 128,
                    output_size: 16,
                },
            ],
        });
        state.sections.push(section_record("a.o", 1, 100, 12));

        let rendered = state.render();

        assert!(rendered.contains(&format!(
            "\tpatch-hash\t1:4:100:4:{},3:8:112:12:{},5:16:128:16:-\n",
            hex::encode(".text.foo"),
            hex::encode(".data")
        )));
        assert_eq!(PersistedState::parse(&rendered).unwrap(), state);
    }

    #[test]
    fn v10_patch_metadata_without_section_names_is_accepted() {
        let line = format!(
            "input\t{}\t1\t{}\t-\tpatch-hash\t1:4:100:4,3:8:112:12",
            hex::encode("a.o"),
            hash_bytes(b"a")
        );

        let parsed = parse_input_line(&line).unwrap();
        let sections = parsed.patch.unwrap().sections;

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].section_name, None);
        assert_eq!(sections[1].section_name, None);
    }

    #[test]
    fn old_patch_section_metadata_cannot_patch_changed_inputs() {
        let line = format!(
            "input\t{}\t1\t{}\t-\told-patch-hash\t1:4,3:8",
            hex::encode("a.o"),
            hash_bytes(b"a")
        );

        let parsed = parse_input_line(&line).unwrap();

        assert_eq!(parsed.patch.unwrap().sections, Vec::new());
    }

    #[test]
    fn persisted_state_round_trips_build_id_hashes() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        let output_len = 5 * BUILD_ID_HASH_GROUP_LEN + 100;
        let nodes = build_id_hash_node_count(output_len).unwrap();
        state.build_id_hashes = Some(BuildIdHashState {
            output_len: output_len as u64,
            nodes,
        });

        let rendered = state.render();

        assert!(rendered.contains(&format!("\nbuild-id-hash\t{output_len}\t{nodes}\n")));
        assert_eq!(PersistedState::parse(&rendered).unwrap(), state);
    }

    #[test]
    fn old_patch_fingerprint_without_section_list_is_ignored() {
        let line = format!(
            "input\t{}\t1\t{}\t-\told-patch-hash",
            hex::encode("a.o"),
            hash_bytes(b"a")
        );

        let parsed = parse_input_line(&line).unwrap();

        assert!(parsed.patch.is_none());
    }

    #[test]
    fn compact_state_interns_repeated_section_inputs() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        state.sections.push(section_record("a.o", 2, 112, 8));

        let rendered = state.render();

        assert!(rendered.contains("\nsection-inputs\t1\n"));
        assert!(rendered.contains("\nsection\t0\t1\t100\t12\n"));
        assert!(rendered.contains("\nsection\t0\t2\t112\t8\n"));
        assert_eq!(PersistedState::parse(&rendered).unwrap(), state);
    }

    #[test]
    fn metadata_update_writes_sections_for_inline_legacy_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        assert!(state.sections_file.is_none());

        state.write_metadata_update(dir.path()).unwrap();

        assert!(dir.path().join(SECTIONS_FILE).exists());
        assert_eq!(
            PersistedState::read(dir.path()).unwrap().unwrap().sections,
            state.sections
        );
    }

    #[test]
    fn previous_state_version_is_accepted_without_sections() {
        let state = state("args", b"output", &[("a.o", b"a")]);
        let rendered = render_legacy_state(&state, STATE_VERSION_V1)
            .split_once("\nsections")
            .unwrap()
            .0
            .to_owned();
        let parsed = PersistedState::parse(&format!("{rendered}\n")).unwrap();
        assert!(parsed.sections.is_empty());
    }

    #[test]
    fn v2_state_version_is_accepted_with_sections() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        let rendered = render_legacy_state(&state, STATE_VERSION_V2);
        assert_eq!(PersistedState::parse(&rendered).unwrap().sections.len(), 1);
    }

    #[test]
    fn v3_state_version_is_accepted_with_sections() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        let rendered = render_legacy_state(&state, STATE_VERSION_V3);
        assert_eq!(PersistedState::parse(&rendered).unwrap().sections.len(), 1);
    }

    #[test]
    fn v4_state_version_is_accepted_with_sections() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        let rendered = render_legacy_state(&state, STATE_VERSION_V4);
        assert_eq!(PersistedState::parse(&rendered).unwrap().sections.len(), 1);
    }

    #[test]
    fn v5_state_version_is_accepted_with_compact_sections() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        let rendered = render_v8_state(&state).replacen(STATE_VERSION_V8, STATE_VERSION_V5, 1);
        assert_eq!(PersistedState::parse(&rendered).unwrap().sections.len(), 1);
    }

    #[test]
    fn v8_state_version_is_accepted_without_build_id_hashes() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        let rendered = render_v8_state(&state);
        assert_eq!(PersistedState::parse(&rendered).unwrap().sections.len(), 1);
    }

    #[test]
    fn corrupt_state_is_rejected() {
        assert!(PersistedState::parse("not-wild\n").is_err());
    }

    #[test]
    fn content_hash_detects_same_length_changes() {
        let first = FileContentState::from_bytes(b"abcd");
        let second = FileContentState::from_bytes(b"wxyz");
        assert_eq!(first.len, second.len);
        assert_ne!(first, second);
    }

    #[test]
    fn file_identity_does_not_affect_content_equality() {
        let first = FileContentState {
            identity: Some(identity(4, 1, 2, 3, 5)),
            ..FileContentState::from_bytes(b"abcd")
        };
        let second = FileContentState {
            identity: Some(identity(4, 10, 20, 30, 50)),
            ..FileContentState::from_bytes(b"abcd")
        };

        assert_eq!(first, second);
    }

    #[test]
    fn file_identity_compares_content_when_hash_is_absent() {
        let first = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 3, 5)),
        };
        let same = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 3, 5)),
        };
        let changed = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 4, 5)),
        };

        assert_eq!(first, same);
        assert_ne!(first, changed);
    }

    #[test]
    fn file_identity_ignores_changed_time() {
        let first = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 3, 5)),
        };
        let same = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 3, 6)),
        };

        assert_eq!(first, same);
    }

    #[test]
    fn missing_hash_does_not_match_missing_identity() {
        let first = FileContentState {
            len: 4,
            hash: String::new(),
            identity: None,
        };
        let second = FileContentState {
            len: 4,
            hash: String::new(),
            identity: None,
        };

        assert_ne!(first, second);
    }

    #[test]
    fn file_identity_matches_current_file_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.o");
        std::fs::write(&path, b"abcd").unwrap();
        let content = FileContentState::from_path(&path).unwrap();

        assert!(content.identity_matches_path(&path).unwrap());

        std::fs::write(&path, b"abcde").unwrap();
        assert!(!content.identity_matches_path(&path).unwrap());
    }

    #[test]
    fn classifies_reusable_state() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            input_files: previous.input_files.clone(),
        };

        assert_eq!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Reuse
        );
    }

    #[test]
    fn classifies_reusable_state_from_output_identity_without_hash() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let mut previous = state("args", b"stale", &[("a.o", b"a")]);
        previous.output = FileContentState::from_path_identity_only(&output).unwrap();
        assert!(previous.output.hash.is_empty());
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            input_files: previous.input_files.clone(),
        };

        assert_eq!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Reuse
        );
    }

    #[test]
    fn changed_args_force_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "new-args".to_owned(),
            input_files: previous.input_files.clone(),
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: false,
            } if reason == "linker arguments changed"
        ));
    }

    #[test]
    fn changed_input_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            input_files: state("args", b"output", &[("a.o", b"b")]).input_files,
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: true,
            } if reason.contains("input file changed")
        ));
    }

    #[test]
    fn missing_output_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            input_files: previous.input_files.clone(),
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: false,
            } if reason.contains("output file could not be reused")
        ));
    }

    #[test]
    fn reusable_inputs_only_include_unchanged_files() {
        let previous = state("args", b"output", &[("a.o", b"a"), ("b.o", b"b")]);
        let current = state("args", b"output", &[("a.o", b"a"), ("b.o", b"changed")]);

        let reusable = reusable_input_files(&current.input_files, &previous.input_files);

        assert!(reusable.contains(&hex::encode("a.o")));
        assert!(!reusable.contains(&hex::encode("b.o")));
    }

    #[test]
    fn patchable_sections_are_allocated_but_not_mergeable() {
        let data = object::SectionFlags::Elf {
            sh_flags: u64::from(object::elf::SHF_ALLOC | object::elf::SHF_WRITE),
        };
        let text = object::SectionFlags::Elf {
            sh_flags: u64::from(object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR),
        };
        let rodata = object::SectionFlags::Elf {
            sh_flags: u64::from(object::elf::SHF_ALLOC),
        };
        let mergeable = object::SectionFlags::Elf {
            sh_flags: u64::from(
                object::elf::SHF_ALLOC | object::elf::SHF_WRITE | object::elf::SHF_MERGE,
            ),
        };
        let non_alloc = object::SectionFlags::Elf {
            sh_flags: u64::from(object::elf::SHF_WRITE),
        };

        assert!(section_flags_allow_patching(data));
        assert!(section_flags_allow_patching(text));
        assert!(section_flags_allow_patching(rodata));
        assert!(!section_flags_allow_patching(mergeable));
        assert!(!section_flags_allow_patching(non_alloc));
        assert!(!section_flags_allow_patching(object::SectionFlags::None));
    }

    #[test]
    fn build_id_hash_tree_matches_full_hash() {
        for len in [
            BUILD_ID_HASH_GROUP_LEN + 1,
            2 * BUILD_ID_HASH_GROUP_LEN,
            2 * BUILD_ID_HASH_GROUP_LEN + 17,
            5 * BUILD_ID_HASH_GROUP_LEN + 100,
        ] {
            let output = (0..len).map(|i| (i % 251) as u8).collect::<Vec<_>>();
            let build_id_range = 100..148;
            let nodes = build_id_hash_node_count(output.len()).unwrap();
            let mut tree = Vec::with_capacity(nodes);
            let left_len = blake3::hazmat::left_subtree_len(output.len() as u64) as usize;
            build_id_subtree_hash(&output, 0, left_len, &build_id_range, &mut tree);
            build_id_subtree_hash(
                &output,
                left_len,
                output.len() - left_len,
                &build_id_range,
                &mut tree,
            );
            let state = BuildIdHashState {
                output_len: output.len() as u64,
                nodes,
            };
            let mut expected = output;
            expected[build_id_range].fill(0);

            assert_eq!(tree.len(), nodes);
            assert_eq!(
                build_id_from_hash_tree(&state, &tree).unwrap(),
                blake3::hash(&expected)
            );
        }
    }

    #[test]
    fn build_id_hash_tree_updates_changed_chunks() {
        let mut output = (0..5 * BUILD_ID_HASH_GROUP_LEN + 100)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        let build_id_range = 1500..1548;
        let nodes = build_id_hash_node_count(output.len()).unwrap();
        let mut tree = Vec::with_capacity(nodes);
        let left_len = blake3::hazmat::left_subtree_len(output.len() as u64) as usize;
        build_id_subtree_hash(&output, 0, left_len, &build_id_range, &mut tree);
        build_id_subtree_hash(
            &output,
            left_len,
            output.len() - left_len,
            &build_id_range,
            &mut tree,
        );
        let mut state = BuildIdHashState {
            output_len: output.len() as u64,
            nodes,
        };

        let changed_range = 2 * BUILD_ID_HASH_GROUP_LEN + 100..2 * BUILD_ID_HASH_GROUP_LEN + 110;
        output[changed_range.clone()].copy_from_slice(b"0123456789");
        let changed_chunks = touched_build_id_chunks(&[changed_range], output.len()).unwrap();
        update_build_id_hash_tree(
            &mut state,
            &mut tree,
            &output,
            &build_id_range,
            &changed_chunks,
        )
        .unwrap();
        let mut expected = output;
        expected[build_id_range].fill(0);

        assert_eq!(
            build_id_from_hash_tree(&state, &tree).unwrap(),
            blake3::hash(&expected)
        );
    }

    #[test]
    fn try_reuse_section_requires_unchanged_input_and_matching_record() {
        let mut input_file = crate::input_data::InputFile::for_testing();
        input_file.filename = PathBuf::from("a.o");
        let input = InputRef {
            file: &input_file,
            entry: None,
        };
        let record = SectionRecord::new(input, object::SectionIndex(3), 64, 16);
        let state = PreparedState {
            mode: IncrementalMode::Relink {
                reason: "input file changed: b.o".to_owned(),
                can_reuse_unchanged_sections: true,
            },
            current: CurrentState {
                state_dir: PathBuf::new(),
                args_hash: "args".to_owned(),
                input_files: Vec::new(),
            },
            reusable_inputs: [encode_path(Path::new("a.o"))].into_iter().collect(),
            previous_sections: [record].into_iter().collect(),
            current_sections: Mutex::new(Vec::new()),
            reused_sections: AtomicUsize::new(0),
        };

        assert!(state.try_reuse_section(input, object::SectionIndex(3), 64, 16, true, true));
        assert!(!state.try_reuse_section(input, object::SectionIndex(3), 80, 16, true, true));
        assert_eq!(state.reused_sections.load(Ordering::Relaxed), 1);
        assert_eq!(state.current_sections.lock().unwrap().len(), 2);
    }

    #[test]
    fn try_reuse_section_skips_non_reusable_records() {
        let mut input_file = crate::input_data::InputFile::for_testing();
        input_file.filename = PathBuf::from("a.o");
        let input = InputRef {
            file: &input_file,
            entry: None,
        };
        let state = PreparedState {
            mode: IncrementalMode::Relink {
                reason: "no previous incremental state".to_owned(),
                can_reuse_unchanged_sections: false,
            },
            current: CurrentState {
                state_dir: PathBuf::new(),
                args_hash: "args".to_owned(),
                input_files: Vec::new(),
            },
            reusable_inputs: HashSet::new(),
            previous_sections: HashSet::new(),
            current_sections: Mutex::new(Vec::new()),
            reused_sections: AtomicUsize::new(0),
        };

        assert!(!state.try_reuse_section(input, object::SectionIndex(3), 64, 16, false, true));
        assert!(state.current_sections.lock().unwrap().is_empty());
    }
}
