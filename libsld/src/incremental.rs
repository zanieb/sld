use crate::archive::ArchiveEntry;
use crate::archive::ArchiveIterator;
use crate::error::Context as _;
use crate::error::Result;
use crate::input_data::FileLoader;
use crate::input_data::InputRef;
use crate::platform;
use crate::timing_phase;
use hashbrown::HashMap;
use hashbrown::HashSet;
use linker_utils::aarch64;
use linker_utils::elf::RelocationKindInfo;
use linker_utils::elf::RelocationSize;
use linker_utils::loongarch64;
use linker_utils::riscv64;
use memmap2::MmapOptions;
use object::Object as _;
use object::ObjectSection as _;
use object::ObjectSymbol as _;
use std::ffi::OsString;
use std::fmt::Write as _;
#[cfg(unix)]
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::hash::Hash as _;
use std::hash::Hasher as _;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const STATE_VERSION: &str = "sld-incremental-state-v29";
const STATE_VERSION_V28: &str = "sld-incremental-state-v28";
const STATE_VERSION_V27: &str = "sld-incremental-state-v27";
const STATE_VERSION_V26: &str = "sld-incremental-state-v26";
const STATE_VERSION_V25: &str = "sld-incremental-state-v25";
const STATE_VERSION_V24: &str = "sld-incremental-state-v24";
const STATE_VERSION_V23: &str = "sld-incremental-state-v23";
const STATE_VERSION_V22: &str = "sld-incremental-state-v22";
const STATE_VERSION_V21: &str = "sld-incremental-state-v21";
const STATE_VERSION_V20: &str = "sld-incremental-state-v20";
const STATE_VERSION_V19: &str = "sld-incremental-state-v19";
const STATE_VERSION_V18: &str = "sld-incremental-state-v18";
const STATE_VERSION_V17: &str = "sld-incremental-state-v17";
const STATE_VERSION_V16: &str = "sld-incremental-state-v16";
const STATE_VERSION_V15: &str = "sld-incremental-state-v15";
const STATE_VERSION_V14: &str = "sld-incremental-state-v14";
const STATE_VERSION_V13: &str = "sld-incremental-state-v13";
const STATE_VERSION_V12: &str = "sld-incremental-state-v12";
const STATE_VERSION_V11: &str = "sld-incremental-state-v11";
const STATE_VERSION_V10: &str = "sld-incremental-state-v10";
const STATE_VERSION_V9: &str = "sld-incremental-state-v9";
const STATE_VERSION_V8: &str = "sld-incremental-state-v8";
const STATE_VERSION_V7: &str = "sld-incremental-state-v7";
const STATE_VERSION_V6: &str = "sld-incremental-state-v6";
const STATE_VERSION_V5: &str = "sld-incremental-state-v5";
const STATE_VERSION_V4: &str = "sld-incremental-state-v4";
const STATE_VERSION_V3: &str = "sld-incremental-state-v3";
const STATE_VERSION_V2: &str = "sld-incremental-state-v2";
const STATE_VERSION_V1: &str = "sld-incremental-state-v1";
const INDEX_FILE: &str = "index";
const LOG_FILE: &str = "log";
const GLOBAL_LOG_FILE: &str = "incremental.log";
const METADATA_UPDATE_FILE: &str = "metadata-update";
const METADATA_UPDATE_VERSION: &str = "sld-incremental-metadata-update-v1";
const USER_STATE_DIR_ENV: &str = "SLD_STATE_DIR";
const INPUT_SNAPSHOT_DIR: &str = "input-files";
const BUILD_ID_HASH_FILE: &str = "build-id-hash";
const UPDATE_MARKER_FILE: &str = "update-in-progress";
const LINK_START_FILE: &str = "link-start";
const SECTIONS_FILE: &str = "sections";
const SECTIONS_FILE_PREFIX: &str = "sections-";
const GENERATED_RELA_DYN_GENERAL: &str = "generated:.rela.dyn.general";
const BUILD_ID_HASH_GROUP_CHUNKS: usize = 64;
const BUILD_ID_HASH_GROUP_LEN: usize = blake3::CHUNK_LEN * BUILD_ID_HASH_GROUP_CHUNKS;
const ABSENT_FIELD: &str = "-";
const RECORD_TEXT_INTERNER_SHARDS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct SharedText(Arc<str>);

impl SharedText {
    fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::ops::Deref for SharedText {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl From<String> for SharedText {
    fn from(value: String) -> Self {
        Self(Arc::<str>::from(value))
    }
}

impl From<&str> for SharedText {
    fn from(value: &str) -> Self {
        Self(Arc::<str>::from(value))
    }
}

impl std::fmt::Display for SharedText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl PartialEq<String> for SharedText {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<&str> for SharedText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

struct RecordTextInterner {
    values: [Mutex<HashMap<String, SharedText>>; RECORD_TEXT_INTERNER_SHARDS],
}

impl Default for RecordTextInterner {
    fn default() -> Self {
        Self {
            values: std::array::from_fn(|_| Mutex::new(HashMap::new())),
        }
    }
}

impl RecordTextInterner {
    fn intern(&self, value: String) -> SharedText {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        let shard = hasher.finish() as usize % RECORD_TEXT_INTERNER_SHARDS;
        let mut values = self.values[shard].lock().unwrap();
        if let Some(existing) = values.get(value.as_str()) {
            return existing.clone();
        }
        let shared = SharedText::from(value.clone());
        values.insert(value, shared.clone());
        shared
    }
}

pub(crate) struct PreparedState {
    mode: IncrementalMode,
    current: CurrentState,
    reusable_inputs: HashSet<String>,
    previous_sections: HashSet<SectionRecord>,
    previous_relocations: Vec<RelocationRecord>,
    previous_fdes: Vec<FdeRecord>,
    previous_dynamic_relocations: Vec<DynamicRelocationRecord>,
    current_sections: Mutex<Vec<SectionRecord>>,
    current_relocations: Mutex<Vec<RelocationRecord>>,
    current_fdes: Mutex<Vec<FdeRecord>>,
    current_dynamic_relocations: Mutex<Vec<DynamicRelocationRecord>>,
    record_texts: RecordTextInterner,
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
    link_options_hash: String,
    input_order_hash: String,
    sld_version: String,
    link_start: Option<FileIdentity>,
    input_files: Vec<FileState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedState {
    args_hash: String,
    link_options_hash: Option<String>,
    input_order_hash: Option<String>,
    sld_version: Option<String>,
    link_start: Option<FileIdentity>,
    output: FileContentState,
    build_id_hashes: Option<BuildIdHashState>,
    input_files: Vec<FileState>,
    sections: Vec<SectionRecord>,
    relocations: Vec<RelocationRecord>,
    fdes: Vec<FdeRecord>,
    dynamic_relocations: Vec<DynamicRelocationRecord>,
    sections_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildIdHashState {
    output_len: u64,
    nodes: usize,
    tree_hash: Option<String>,
}

type BuildIdHashStateAndTree = (Option<BuildIdHashState>, Option<Vec<[u8; blake3::OUT_LEN]>>);

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
    raw_sections: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilePatchSectionState {
    input: String,
    section_index: u32,
    section_name: Option<String>,
    input_size: u64,
    output_offset: u64,
    output_size: u64,
    data_hash: Option<String>,
}

#[derive(Clone, Copy)]
enum PatchSectionReadMode {
    Parse,
    PreserveRaw,
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
            && self.changed_sec == other.changed_sec
            && self.changed_nsec == other.changed_nsec
    }
}

impl Eq for FileIdentity {}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SectionRecord {
    input_file: SharedText,
    input: SharedText,
    section_index: u32,
    output_offset: u64,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RelocationRecord {
    target_symbol_id: u32,
    written_value: Option<u64>,
    target_value: u64,
    target_name: Option<String>,
    target: Option<RelocationTargetRecord>,
    input_file: SharedText,
    input: SharedText,
    section_index: u32,
    relocation_offset: u64,
    output_offset: u64,
    size: u64,
    kind: u32,
    addend: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RelocationTargetRecord {
    input_file: SharedText,
    input: SharedText,
    section_index: u32,
    section_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct FdeRecord {
    input_file: SharedText,
    input: SharedText,
    section_index: u32,
    eh_frame_section_index: u32,
    input_offset: u64,
    output_offset: u64,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DynamicRelocationRecord {
    input_file: SharedText,
    input: SharedText,
    section_index: u32,
    relocation_offset: u64,
    output_offset: u64,
    size: u64,
    output_r_offset: Option<u64>,
    output_r_info: Option<u64>,
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
                link_options_hash: String::new(),
                input_order_hash: String::new(),
                sld_version: String::new(),
                link_start: None,
                input_files: Vec::new(),
            },
            reusable_inputs: HashSet::new(),
            previous_sections: HashSet::new(),
            previous_relocations: Vec::new(),
            previous_fdes: Vec::new(),
            previous_dynamic_relocations: Vec::new(),
            current_sections: Mutex::new(Vec::new()),
            current_relocations: Mutex::new(Vec::new()),
            current_fdes: Mutex::new(Vec::new()),
            current_dynamic_relocations: Mutex::new(Vec::new()),
            record_texts: RecordTextInterner::default(),
            reused_sections: AtomicUsize::new(0),
        });
    }

    timing_phase!("Prepare incremental link");

    let state_dir = state_dir_for_output(args.output());
    let previous_metadata = PersistedState::read_metadata(&state_dir);
    let current = CurrentState::new(
        args,
        file_loader,
        previous_metadata.as_ref().ok().and_then(|p| p.as_ref()),
    );
    let (mut mode, previous_metadata) = match previous_metadata {
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

    let mut previous_sections = HashSet::new();
    let mut previous_relocations = Vec::new();
    let mut previous_fdes = Vec::new();
    let mut previous_dynamic_relocations = Vec::new();
    if mode_needs_previous_sections(&mode) {
        match PersistedState::read(&state_dir) {
            Ok(Some(previous)) => {
                previous_sections = previous.sections.iter().cloned().collect();
                previous_relocations = previous.relocations;
                previous_fdes = previous.fdes;
                previous_dynamic_relocations = previous.dynamic_relocations;
            }
            Ok(None) => {
                mode = IncrementalMode::Relink {
                    reason: "no previous incremental state".to_owned(),
                    can_reuse_unchanged_sections: false,
                };
            }
            Err(error) => {
                mode = IncrementalMode::Relink {
                    reason: format!("could not read previous incremental state: {error:?}"),
                    can_reuse_unchanged_sections: false,
                };
            }
        }
    }

    current.log_mode(&mode)?;

    let reusable_inputs = previous_metadata
        .as_ref()
        .map(|previous| reusable_input_files(&current.input_files, &previous.input_files))
        .unwrap_or_default();

    Ok(PreparedState {
        mode,
        current,
        reusable_inputs,
        previous_sections,
        previous_relocations,
        previous_fdes,
        previous_dynamic_relocations,
        current_sections: Mutex::new(Vec::new()),
        current_relocations: Mutex::new(Vec::new()),
        current_fdes: Mutex::new(Vec::new()),
        current_dynamic_relocations: Mutex::new(Vec::new()),
        record_texts: RecordTextInterner::default(),
        reused_sections: AtomicUsize::new(0),
    })
}

fn mode_needs_previous_sections(mode: &IncrementalMode) -> bool {
    matches!(
        mode,
        IncrementalMode::Reuse
            | IncrementalMode::Relink {
                can_reuse_unchanged_sections: true,
                ..
            }
    )
}

pub(crate) fn maybe_reuse_output_before_loading(args: &impl platform::Args) -> Result<bool> {
    if !args.common().incremental {
        return Ok(false);
    }

    let state_dir = state_dir_for_output(args.output());
    let current_link_start = write_link_start_marker(&state_dir)?;

    if args.should_write_trace_file() || args.common().save_dir.is_active() {
        return Ok(false);
    }
    if args
        .dependency_file()
        .is_some_and(|dependency_file| !dependency_file.exists())
    {
        return Ok(false);
    }

    timing_phase!("Check incremental fast path");

    if let Some(reason) = interrupted_update_relink_reason(&state_dir) {
        append_log(
            &state_dir,
            &format!("incremental fast path unavailable before loading inputs: {reason}"),
        )?;
        return Ok(false);
    }

    let Some(mut previous) = PersistedState::read_metadata(&state_dir).unwrap_or_default() else {
        return Ok(false);
    };

    if previous.args_hash != args_hash(args) {
        return Ok(false);
    }
    let current_sld_version = sld_version(args);
    if sld_version_relink_reason(previous.sld_version.as_deref(), &current_sld_version).is_some() {
        return Ok(false);
    }
    if !previous.output.identity_matches_path(args.output())? {
        return Ok(false);
    }

    let mut changed_inputs = Vec::new();
    let mut rewritten_inputs = Vec::new();
    let mut checked_ambiguous_inputs = false;
    for (index, input) in previous.input_files.iter().enumerate() {
        let path = decode_path(&input.path)?;
        if input.content.identity_matches_path(&path)? {
            if !input
                .content
                .identity_is_ambiguous_since(previous.link_start.as_ref())
            {
                continue;
            }
            checked_ambiguous_inputs = true;
            if input_content_matches_snapshot(&state_dir, input, &path)? {
                continue;
            }
            changed_inputs.push((index, path));
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
        refresh_rewritten_input_identities(&mut previous, &rewritten_inputs);
    }

    if !changed_inputs.is_empty() {
        let changed_input_indices = changed_inputs
            .iter()
            .map(|(input_index, _)| *input_index)
            .collect::<HashSet<_>>();
        let metadata_update_input_indices =
            metadata_update_indices_for_inputs(&changed_inputs, &rewritten_inputs);
        if changed_input_indices.iter().any(|index| {
            previous.input_files[*index]
                .patch
                .as_ref()
                .is_some_and(|patch| patch.sections.is_empty() && patch.raw_sections.is_none())
        }) {
            previous.read_patch_metadata_for_input_indices(&state_dir, &changed_input_indices)?;
        }
        let should_filter_records = previous
            .sections_file
            .as_deref()
            .is_some_and(|sections_file| should_filter_sections_sidecar(&state_dir, sections_file));
        let should_retry_with_full_state = should_filter_records;
        let result = if should_filter_records {
            let result = patch_changed_inputs(
                args,
                &state_dir,
                previous,
                current_link_start.clone(),
                false,
                &changed_inputs,
                &metadata_update_input_indices,
            )?;
            if let ChangedInputPatchResult::Unsupported(reason) = result {
                append_log(
                    &state_dir,
                    &format!(
                        "metadata-only changed-input patch unavailable before loading inputs: {reason}"
                    ),
                )?;
                let Some(mut previous) = PersistedState::read_metadata(&state_dir)? else {
                    return Ok(false);
                };
                refresh_rewritten_input_identities(&mut previous, &rewritten_inputs);
                previous
                    .read_patch_metadata_for_input_indices(&state_dir, &changed_input_indices)?;
                let changed_input_files = changed_inputs
                    .iter()
                    .map(|(input_index, _)| previous.input_files[*input_index].path.clone())
                    .collect::<HashSet<_>>();
                previous.read_records_for_input_files(&state_dir, &changed_input_files)?;
                patch_changed_inputs(
                    args,
                    &state_dir,
                    previous,
                    current_link_start.clone(),
                    false,
                    &changed_inputs,
                    &metadata_update_input_indices,
                )?
            } else {
                result
            }
        } else {
            let mut records_complete = previous.sections_file.is_none();
            if previous.sections_file.is_some()
                && let Some(mut full_previous) = PersistedState::read(&state_dir)?
            {
                full_previous.input_files = previous.input_files;
                previous = full_previous;
                records_complete = true;
            }
            patch_changed_inputs(
                args,
                &state_dir,
                previous,
                current_link_start.clone(),
                records_complete,
                &changed_inputs,
                &metadata_update_input_indices,
            )?
        };
        let result = if let ChangedInputPatchResult::Unsupported(reason) = result {
            if should_retry_with_full_state
                && let Some(mut full_previous) = PersistedState::read(&state_dir)?
            {
                refresh_rewritten_input_identities(&mut full_previous, &rewritten_inputs);
                patch_changed_inputs(
                    args,
                    &state_dir,
                    full_previous,
                    current_link_start,
                    true,
                    &changed_inputs,
                    &metadata_update_input_indices,
                )?
            } else {
                ChangedInputPatchResult::Unsupported(reason)
            }
        } else {
            result
        };
        match result {
            ChangedInputPatchResult::Patched => return Ok(true),
            ChangedInputPatchResult::Unsupported(reason) => {
                append_log(
                    &state_dir,
                    &format!("changed-input patch unavailable before loading inputs: {reason}"),
                )?;
            }
            ChangedInputPatchResult::StartedUnsupported(reason) => {
                append_log(
                    &state_dir,
                    &format!(
                        "changed-input patch failed after starting update before loading inputs: {reason}"
                    ),
                )?;
            }
        }
        return Ok(false);
    }

    if let Some(reason) = input_identity_mismatch_reason(&previous.input_files)? {
        append_log(
            &state_dir,
            &format!("incremental fast path unavailable before loading inputs: {reason}"),
        )?;
        return Ok(false);
    }

    if (!rewritten_inputs.is_empty() || checked_ambiguous_inputs)
        && let Some(mut metadata) = PersistedState::read_metadata(&state_dir)?
    {
        refresh_rewritten_input_identities(&mut metadata, &rewritten_inputs);
        metadata.link_start = current_link_start;
        metadata.write_metadata_update(&state_dir)?;
    }
    if !rewritten_inputs.is_empty() {
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

fn metadata_update_indices_for_inputs(
    changed_inputs: &[(usize, PathBuf)],
    rewritten_inputs: &[(usize, PathBuf)],
) -> Vec<usize> {
    let mut indices = changed_inputs
        .iter()
        .chain(rewritten_inputs)
        .map(|(input_index, _)| *input_index)
        .collect::<Vec<_>>();
    indices.sort_unstable();
    indices.dedup();
    indices
}

fn refresh_rewritten_input_identities(
    previous: &mut PersistedState,
    rewritten_inputs: &[(usize, PathBuf)],
) {
    refresh_input_file_identities_at_indices(
        &mut previous.input_files,
        rewritten_inputs.iter().map(|(input_index, _)| *input_index),
    );
}

enum ChangedInputPatchResult {
    Patched,
    Unsupported(String),
    StartedUnsupported(String),
}

fn relocation_target_patches_for_input(
    relocations: &mut [RelocationRecord],
    input: &FileState,
    bytes: &[u8],
) -> Result<std::result::Result<RelocationTargetPatches, String>> {
    let mut input_ranges = Vec::new();
    let mut output_patches = Vec::new();
    let mut output_symbols = Vec::new();
    for relocation in relocations {
        let Some(target) = relocation.target.as_mut() else {
            continue;
        };
        if target.input_file != input.path {
            continue;
        }
        let Some(target_name) = relocation.target_name.as_deref() else {
            continue;
        };
        let Some(input_bytes) = patch_input_bytes(bytes, input.path.as_str(), &target.input)?
        else {
            continue;
        };
        let file = object::File::parse(input_bytes.bytes)
            .context("Failed to parse changed relocation target input")?;
        let Some(current) = symbol_position_by_name(
            input_bytes.bytes,
            input_bytes.file_offset,
            &file,
            target_name,
        )?
        else {
            continue;
        };
        if let Some(value_range) = current.value_range {
            input_ranges.push(value_range);
        }
        if current.section_index.0 as u32 == target.section_index
            && current.section_offset == target.section_offset
        {
            continue;
        }
        if current.section_index.0 as u32 != target.section_index {
            return Ok(Err(format!(
                "relocation target moved in {}",
                display_hex_path(&input.path)
            )));
        }

        let Some(previous_written_value) = relocation.written_value else {
            return Ok(Err(format!(
                "missing written relocation value for target in {}",
                display_hex_path(&input.path)
            )));
        };
        let delta = i128::from(current.section_offset) - i128::from(target.section_offset);
        let Some(written_value) = add_signed_delta_u64(previous_written_value, delta) else {
            return Ok(Err(format!(
                "relocation target patch overflowed in {}",
                display_hex_path(&input.path)
            )));
        };
        let deferred_relocation = deferred_instruction_relocation_patch(
            &file,
            relocation.kind,
            previous_written_value,
            written_value,
        );
        let data = if deferred_relocation.is_some() {
            let Ok(size) = usize::try_from(relocation.size) else {
                return Ok(Err(format!(
                    "unsupported relocation target patch size in {}",
                    display_hex_path(&input.path)
                )));
            };
            vec![0; size]
        } else {
            match relocation.size {
                4 => {
                    let Ok(written_value) = u32::try_from(written_value) else {
                        return Ok(Err(format!(
                            "relocation target patch overflowed in {}",
                            display_hex_path(&input.path)
                        )));
                    };
                    written_value.to_le_bytes().to_vec()
                }
                8 => written_value.to_le_bytes().to_vec(),
                _ => {
                    return Ok(Err(format!(
                        "unsupported relocation target patch size in {}",
                        display_hex_path(&input.path)
                    )));
                }
            }
        };
        let previous_target_value = relocation.target_value;
        let Some(target_value) = add_signed_delta_u64(previous_target_value, delta) else {
            return Ok(Err(format!(
                "relocation target value overflowed in {}",
                display_hex_path(&input.path)
            )));
        };
        output_patches.push(SectionPatch {
            output_offset: relocation.output_offset,
            size: relocation.size,
            data,
            deferred_relocation,
            preserve_ranges: Vec::new(),
            adjustments: Vec::new(),
        });
        relocation.written_value = Some(written_value);
        relocation.target_value = target_value;
        target.section_offset = current.section_offset;
        output_symbols.push(RelocationTargetSymbolPatch {
            target_name: target_name.to_owned(),
            previous_target_value,
            target_value,
        });
    }
    dedup_ranges(&mut input_ranges);
    Ok(Ok(RelocationTargetPatches {
        input_ranges,
        output_patches,
        output_symbols,
    }))
}

fn deferred_instruction_relocation_patch(
    file: &object::File<'_>,
    relocation_kind: u32,
    previous_written_value: u64,
    written_value: u64,
) -> Option<DeferredRelocationPatch> {
    let rel_info = match file.architecture() {
        object::Architecture::Aarch64 => aarch64::relocation_type_from_raw(relocation_kind)?,
        object::Architecture::LoongArch64 => {
            loongarch64::relocation_type_from_raw(relocation_kind)?
        }
        object::Architecture::Riscv64 => riscv64::relocation_type_from_raw(relocation_kind)?,
        _ => return None,
    };
    matches!(rel_info.size, RelocationSize::BitMasking(_)).then_some(DeferredRelocationPatch {
        rel_info,
        previous_written_value,
        written_value,
    })
}

fn dedup_ranges(ranges: &mut Vec<std::ops::Range<usize>>) {
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges.dedup_by(|left, right| left.start == right.start && left.end == right.end);
}

fn add_signed_delta_u64(value: u64, delta: i128) -> Option<u64> {
    let adjusted = i128::from(value).checked_add(delta)?;
    u64::try_from(adjusted).ok()
}

struct SymbolPosition {
    section_index: object::SectionIndex,
    section_offset: u64,
    value_range: Option<std::ops::Range<usize>>,
}

fn symbol_position_by_name(
    bytes: &[u8],
    file_offset: usize,
    file: &object::File<'_>,
    encoded_name: &str,
) -> Result<Option<SymbolPosition>> {
    let name = hex::decode(encoded_name).context("Malformed incremental relocation target name")?;
    for symbol in file.symbols() {
        if symbol.name_bytes()? != name {
            continue;
        }
        let Some(section_index) = symbol.section_index() else {
            return Ok(None);
        };
        let value_range = elf_symbol_value_field_range(bytes, symbol.index())
            .map(|range| file_offset + range.start..file_offset + range.end);
        return Ok(Some(SymbolPosition {
            section_index,
            section_offset: symbol.address(),
            value_range,
        }));
    }
    Ok(None)
}

fn symbol_position_by_name_and_value(
    bytes: &[u8],
    file_offset: usize,
    file: &object::File<'_>,
    encoded_name: &str,
    value: u64,
) -> Result<std::result::Result<Option<SymbolPosition>, String>> {
    let name = hex::decode(encoded_name).context("Malformed incremental relocation target name")?;
    let mut matched_symbol = None;
    for symbol in file.symbols() {
        if symbol.name_bytes()? != name || symbol.address() != value {
            continue;
        }
        if matched_symbol.is_some() {
            return Ok(Err(
                "ambiguous output symbol for incremental value patch".to_owned()
            ));
        }
        let Some(section_index) = symbol.section_index() else {
            return Ok(Ok(None));
        };
        let value_range = elf_symbol_value_field_range(bytes, symbol.index())
            .map(|range| file_offset + range.start..file_offset + range.end);
        matched_symbol = Some(SymbolPosition {
            section_index,
            section_offset: symbol.address(),
            value_range,
        });
    }
    Ok(Ok(matched_symbol))
}

fn output_symbol_value_patches(
    output: &[u8],
    symbols: &[RelocationTargetSymbolPatch],
) -> Result<std::result::Result<Vec<SectionPatch>, String>> {
    if symbols.is_empty() {
        return Ok(Ok(Vec::new()));
    }

    let file = object::File::parse(output)
        .context("Failed to parse output for incremental symbol value patching")?;
    let mut values_by_previous_name_and_value = HashMap::<(&str, u64), u64>::new();
    for symbol in symbols {
        let key = (symbol.target_name.as_str(), symbol.previous_target_value);
        if let Some(previous) = values_by_previous_name_and_value.insert(key, symbol.target_value)
            && previous != symbol.target_value
        {
            return Ok(Err(
                "conflicting incremental symbol value patches".to_owned()
            ));
        }
    }

    let mut patches = Vec::with_capacity(values_by_previous_name_and_value.len());
    for ((target_name, previous_target_value), target_value) in values_by_previous_name_and_value {
        let symbol = symbol_position_by_name_and_value(
            output,
            0,
            &file,
            target_name,
            previous_target_value,
        )?;
        let symbol = match symbol {
            Ok(Some(symbol)) => symbol,
            Ok(None) => {
                return Ok(Err(
                    "missing output symbol for incremental value patch".to_owned()
                ));
            }
            Err(error) => return Ok(Err(error)),
        };
        let Some(value_range) = symbol.value_range else {
            return Ok(Err(
                "missing output symbol value range for incremental patch".to_owned(),
            ));
        };
        let data = match value_range.len() {
            4 => {
                let Ok(value) = u32::try_from(target_value) else {
                    return Ok(Err(
                        "incremental output symbol value patch overflowed".to_owned()
                    ));
                };
                value.to_le_bytes().to_vec()
            }
            8 => target_value.to_le_bytes().to_vec(),
            _ => {
                return Ok(Err(
                    "unsupported output symbol value size for incremental patch".to_owned(),
                ));
            }
        };
        patches.push(SectionPatch {
            output_offset: value_range.start as u64,
            size: value_range.len() as u64,
            data,
            deferred_relocation: None,
            preserve_ranges: Vec::new(),
            adjustments: Vec::new(),
        });
    }

    Ok(Ok(patches))
}

fn patch_changed_inputs(
    args: &impl platform::Args,
    state_dir: &Path,
    previous: PersistedState,
    current_link_start: Option<FileIdentity>,
    records_complete: bool,
    changed_inputs: &[(usize, PathBuf)],
    metadata_update_input_indices: &[usize],
) -> Result<ChangedInputPatchResult> {
    timing_phase!("Patch changed incremental inputs");

    let mut previous = previous;
    let mut patches = Vec::new();
    let mut output_symbol_patches = Vec::new();
    let mut eh_frame_hdr_changes = Vec::new();
    let mut fde_add_candidates = Vec::new();
    let mut expected_changed_inputs = Vec::new();
    let mut patched_section_count = 0;
    for (input_index, path) in changed_inputs {
        let previous_patch = {
            let input = &previous.input_files[*input_index];
            match patch_sections_from_previous_state(input, path) {
                Ok(previous_patch) => previous_patch,
                Err(reason) => return Ok(ChangedInputPatchResult::Unsupported(reason)),
            }
        };
        let Some((bytes, input_content)) =
            read_file_with_stable_identity(path).with_context(|| {
                format!(
                    "Failed to read changed incremental input `{}`",
                    path.display()
                )
            })?
        else {
            return Ok(ChangedInputPatchResult::Unsupported(format!(
                "changed input changed while being read: {}",
                path.display()
            )));
        };
        expected_changed_inputs.push(ExpectedInputContent::from_bytes(path, &bytes));

        let (
            fingerprint,
            matched_sections,
            current_sections,
            resolved_patches,
            fde_eh_frame_hdr_changes,
            input_fde_add_candidates,
            added_dynamic_relocations,
            removed_dynamic_relocations,
            removed_fdes,
            updated_fdes,
        ) = {
            let input = &previous.input_files[*input_index];
            if !archive_members_match_snapshot(state_dir, input, &bytes)? {
                return Ok(ChangedInputPatchResult::Unsupported(format!(
                    "archive members changed in `{}`",
                    path.display()
                )));
            }
            let previous_snapshot_bytes = read_verified_input_snapshot(state_dir, input)?;
            let relocation_target_patches = match relocation_target_patches_for_input(
                &mut previous.relocations,
                input,
                &bytes,
            )? {
                Ok(patches) => patches,
                Err(reason) => return Ok(ChangedInputPatchResult::Unsupported(reason)),
            };
            output_symbol_patches.extend(relocation_target_patches.output_symbols.iter().cloned());
            let relocation_addend_patches = match relocation_addend_patches_for_input(
                &mut previous.relocations,
                input,
                &bytes,
                previous_snapshot_bytes.as_deref(),
                &previous.dynamic_relocations,
            )? {
                Ok(patches) => patches,
                Err(reason) => return Ok(ChangedInputPatchResult::Unsupported(reason)),
            };

            let matched_patch_sections = if let Some(matched) =
                match_patch_sections_from_current_hashes(
                    &bytes,
                    input.path.as_str(),
                    &previous_patch.sections,
                )? {
                Some(matched)
            } else {
                match_patch_sections(state_dir, input, &bytes, &previous_patch.sections)?
            };

            let (mut matched_sections, matched_changed_sections) = match matched_patch_sections {
                Some(matched_sections) => (
                    matched_sections.sections,
                    Some(matched_sections.changed_sections),
                ),
                None if previous_patch
                    .sections
                    .iter()
                    .any(|section| section.section_name.is_none()) =>
                {
                    return Ok(ChangedInputPatchResult::Unsupported(format!(
                        "could not match anonymous patch sections in `{}`",
                        path.display()
                    )));
                }
                None => (
                    previous_patch
                        .sections
                        .iter()
                        .cloned()
                        .map(MatchedPatchSection::same)
                        .collect(),
                    None,
                ),
            };
            let matched_from_snapshot = matched_changed_sections.is_some();
            let mut current_sections = matched_sections
                .iter()
                .map(|section| section.current.clone())
                .collect::<Vec<_>>();

            let mut dynamic_relocation_patches = dynamic_relocation_patches_for_input(
                &bytes,
                input.path.as_str(),
                previous
                    .dynamic_relocations
                    .iter()
                    .filter(|record| record.input_file == input.path),
            )?;
            if let Some(previous_bytes) = previous_snapshot_bytes.as_deref() {
                dynamic_relocation_patches.extend(added_dynamic_relocation_patches_for_input(
                    &bytes,
                    previous_bytes,
                    input.path.as_str(),
                    &matched_sections,
                    &previous.dynamic_relocations,
                    &previous.sections,
                ));
            }
            let eh_frame_patches = if let Some(previous_bytes) = previous_snapshot_bytes.as_deref()
            {
                fde_relocation_patches_for_input(
                    &bytes,
                    previous_bytes,
                    input.path.as_str(),
                    previous
                        .fdes
                        .iter()
                        .filter(|record| record.input_file == input.path),
                )?
            } else {
                Vec::new()
            };
            let input_fde_add_candidates =
                if let Some(previous_bytes) = previous_snapshot_bytes.as_deref() {
                    added_fde_candidates_for_input(
                        &bytes,
                        previous_bytes,
                        input.path.as_str(),
                        &matched_sections,
                        previous
                            .fdes
                            .iter()
                            .filter(|record| record.input_file == input.path),
                    )?
                } else {
                    Vec::new()
                };
            let Some(fingerprint) = patch_fingerprint_with_extra_ranges(
                &bytes,
                input.path.as_str(),
                current_sections.iter().cloned(),
                dynamic_relocation_patches
                    .iter()
                    .filter_map(|patch| patch.input_range.clone())
                    .chain(relocation_addend_patches.input_ranges.iter().cloned())
                    .chain(relocation_target_patches.input_ranges.iter().cloned())
                    .chain(
                        eh_frame_patches
                            .iter()
                            .flat_map(|patch| patch.input_ranges.iter().map(Clone::clone)),
                    )
                    .chain(
                        input_fde_add_candidates
                            .iter()
                            .flat_map(|candidate| candidate.input_ranges.iter().cloned()),
                    ),
            )?
            else {
                return Ok(ChangedInputPatchResult::Unsupported(format!(
                    "could not resolve patchable sections in `{}`",
                    path.display()
                )));
            };
            if fingerprint != previous_patch.fingerprint {
                let dynamic_relocation_removed = dynamic_relocation_patches
                    .iter()
                    .any(|patch| patch.input_range.is_none());
                let allows_dynamic_relocation_removal = if dynamic_relocation_removed {
                    if let Some(previous_bytes) = previous_snapshot_bytes.as_deref() {
                        object_diff_allows_dynamic_relocation_removal(
                            previous_bytes,
                            &bytes,
                            input.path.as_str(),
                            &matched_sections,
                            &dynamic_relocation_patches,
                        )?
                    } else {
                        false
                    }
                } else {
                    false
                };
                let dynamic_relocation_added = dynamic_relocation_patches
                    .iter()
                    .any(|patch| patch.input_range.is_some());
                let allows_dynamic_relocation_addition = if dynamic_relocation_added {
                    if let Some(previous_bytes) = previous_snapshot_bytes.as_deref() {
                        object_diff_allows_dynamic_relocation_addition(
                            previous_bytes,
                            &bytes,
                            input.path.as_str(),
                            &matched_sections,
                            &dynamic_relocation_patches,
                        )?
                    } else {
                        false
                    }
                } else {
                    false
                };
                let fde_removed = eh_frame_patches.iter().any(|patch| {
                    matches!(patch.eh_frame_hdr_change, Some(EhFrameHdrChange::Remove(_)))
                });
                let allows_fde_removal = if fde_removed {
                    if let Some(previous_bytes) = previous_snapshot_bytes.as_deref() {
                        object_diff_allows_fde_removal(
                            previous_bytes,
                            &bytes,
                            input.path.as_str(),
                            &matched_sections,
                            &eh_frame_patches,
                        )?
                    } else {
                        false
                    }
                } else {
                    false
                };
                let allows_fde_addition = if input_fde_add_candidates.is_empty() {
                    false
                } else {
                    if let Some(previous_bytes) = previous_snapshot_bytes.as_deref() {
                        object_diff_allows_fde_addition(
                            previous_bytes,
                            &bytes,
                            input.path.as_str(),
                            &matched_sections,
                            &input_fde_add_candidates,
                        )?
                    } else {
                        false
                    }
                };
                let metadata_only_fingerprint_matches = !records_complete
                    && previous.sections.is_empty()
                    && previous.relocations.is_empty()
                    && previous.fdes.is_empty()
                    && previous.dynamic_relocations.is_empty()
                    && if let Some(previous_bytes) = previous_snapshot_bytes.as_deref() {
                        patch_fingerprint_matches_previous_without_extra_ranges(
                            previous_bytes,
                            fingerprint.as_str(),
                            input.path.as_str(),
                            &matched_sections,
                        )?
                    } else {
                        false
                    };
                if !allows_dynamic_relocation_removal
                    && !allows_dynamic_relocation_addition
                    && !allows_fde_removal
                    && !allows_fde_addition
                    && !metadata_only_fingerprint_matches
                {
                    return Ok(ChangedInputPatchResult::Unsupported(format!(
                        "changed bytes outside patchable sections in `{}`",
                        path.display()
                    )));
                }
            }

            let patch_sections = if let Some(changed_sections) = matched_changed_sections {
                changed_sections
            } else {
                changed_patch_sections(state_dir, input, &bytes, &matched_sections)?
                    .unwrap_or_else(|| current_sections.clone())
            };
            patched_section_count += patch_sections.len();

            let Some(resolved_patches) =
                resolved_patch_sections_for_input_with_dynamic_relocations(
                    &bytes,
                    input.path.as_str(),
                    patch_sections,
                    dynamic_relocation_patches.iter().map(|patch| &patch.record),
                )?
            else {
                return Ok(ChangedInputPatchResult::Unsupported(format!(
                    "changed patchable section size in `{}`",
                    path.display()
                )));
            };
            if !matched_from_snapshot {
                if resolved_patches.len() == current_sections.len() {
                    current_sections = resolved_patches
                        .iter()
                        .map(|resolved| resolved.section.clone())
                        .collect();
                } else {
                    let Some(resolved_sections) = resolve_current_patch_sections(
                        &bytes,
                        input.path.as_str(),
                        current_sections.iter().cloned(),
                        dynamic_relocation_patches.iter().map(|patch| &patch.record),
                    )?
                    else {
                        return Ok(ChangedInputPatchResult::Unsupported(format!(
                            "changed patchable section size in `{}`",
                            path.display()
                        )));
                    };
                    current_sections = resolved_sections;
                }
            }
            update_matched_patch_current_sections(&mut matched_sections, &current_sections);
            patched_section_count += dynamic_relocation_patches.len();
            patched_section_count += relocation_addend_patches.output_patches.len();
            patched_section_count += eh_frame_patches
                .iter()
                .filter(|patch| patch.patch.is_some())
                .count();
            let eh_frame_hdr_changes = eh_frame_patches
                .iter()
                .filter_map(|patch| patch.eh_frame_hdr_change.clone())
                .collect::<Vec<_>>();
            let removed_dynamic_relocations = dynamic_relocation_patches
                .iter()
                .filter_map(|patch| patch.input_range.is_none().then_some(patch.record.clone()))
                .collect::<HashSet<_>>();
            let added_dynamic_relocations = dynamic_relocation_patches
                .iter()
                .filter_map(|patch| {
                    (patch.input_range.is_some() && patch.is_new).then_some(patch.record.clone())
                })
                .collect::<HashSet<_>>();
            let removed_fdes = eh_frame_hdr_changes
                .iter()
                .filter_map(|change| match change {
                    EhFrameHdrChange::Remove(fde) => Some(fde.clone()),
                    EhFrameHdrChange::Adjust(_) | EhFrameHdrChange::Add(_) => None,
                })
                .collect::<HashSet<_>>();
            let updated_fdes = eh_frame_patches
                .iter()
                .filter_map(|patch| patch.record_update.clone())
                .collect::<Vec<_>>();

            (
                fingerprint,
                matched_sections,
                current_sections,
                resolved_patches
                    .into_iter()
                    .map(|resolved| resolved.patch)
                    .chain(
                        dynamic_relocation_patches
                            .into_iter()
                            .map(|relocation| relocation.patch),
                    )
                    .chain(relocation_addend_patches.output_patches)
                    .chain(relocation_target_patches.output_patches)
                    .chain(eh_frame_patches.into_iter().filter_map(|fde| fde.patch))
                    .collect::<Vec<_>>(),
                eh_frame_hdr_changes,
                input_fde_add_candidates,
                added_dynamic_relocations,
                removed_dynamic_relocations,
                removed_fdes,
                updated_fdes,
            )
        };

        let sections_changed = update_section_records_for_matched_patches(
            previous.input_files[*input_index].path.as_str(),
            &matched_sections,
            &mut previous.sections,
        );
        if sections_changed {
            if !records_complete {
                return Ok(ChangedInputPatchResult::Unsupported(
                    "changed input needs complete section records".to_owned(),
                ));
            }
            previous.sections_file = None;
        }
        if !added_dynamic_relocations.is_empty() {
            if !records_complete {
                return Ok(ChangedInputPatchResult::Unsupported(
                    "changed input needs complete dynamic relocation records".to_owned(),
                ));
            }
            previous
                .dynamic_relocations
                .extend(added_dynamic_relocations);
            previous.dynamic_relocations.sort();
            previous.dynamic_relocations.dedup();
            previous.sections_file = None;
        }
        if !removed_dynamic_relocations.is_empty() {
            if !records_complete {
                return Ok(ChangedInputPatchResult::Unsupported(
                    "changed input needs complete dynamic relocation records".to_owned(),
                ));
            }
            previous.dynamic_relocations.retain(|record| {
                !removed_dynamic_relocations.contains(record)
                    || record.has_restorable_rela_output_info()
            });
            previous.sections_file = None;
        }
        if !removed_fdes.is_empty() {
            if !records_complete {
                return Ok(ChangedInputPatchResult::Unsupported(
                    "changed input needs complete FDE records".to_owned(),
                ));
            }
            previous
                .fdes
                .retain(|record| !removed_fdes.contains(record));
            previous.sections_file = None;
        }
        if !updated_fdes.is_empty() {
            if !records_complete {
                return Ok(ChangedInputPatchResult::Unsupported(
                    "changed input needs complete FDE records".to_owned(),
                ));
            }
            update_fde_records(&mut previous.fdes, updated_fdes);
            previous.sections_file = None;
        }
        previous.input_files[*input_index].content = input_content;
        previous.input_files[*input_index].patch = Some(FilePatchState {
            fingerprint: fingerprint.clone(),
            sections: current_sections
                .iter()
                .map(|section| FilePatchSectionState {
                    input: section.input.clone(),
                    section_index: section.section_index,
                    section_name: section.section_name.clone(),
                    input_size: section.input_size,
                    output_offset: section.output_offset,
                    output_size: section.output_size,
                    data_hash: section.data_hash.clone(),
                })
                .collect(),
            raw_sections: None,
        });
        patches.extend(resolved_patches);
        eh_frame_hdr_changes.extend(fde_eh_frame_hdr_changes);
        fde_add_candidates.extend(input_fde_add_candidates);
    }

    if let Some(reason) = input_content_mismatch_reason(&expected_changed_inputs) {
        return Ok(ChangedInputPatchResult::Unsupported(reason));
    }

    if let Some(reason) = input_identity_mismatch_reason(&previous.input_files)? {
        return Ok(ChangedInputPatchResult::Unsupported(reason));
    }

    if let Some(reason) = patch_output_range_rejection_reason(&patches) {
        return Ok(ChangedInputPatchResult::Unsupported(reason));
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
    match output_symbol_value_patches(&output, &output_symbol_patches)? {
        Ok(symbol_patches) => patches.extend(symbol_patches),
        Err(reason) => return Ok(ChangedInputPatchResult::Unsupported(reason)),
    }
    match fde_add_patches_for_output(&output, &fde_add_candidates, &previous.fdes)? {
        Ok(resolved_fdes) => {
            if !records_complete && !resolved_fdes.is_empty() {
                return Ok(ChangedInputPatchResult::Unsupported(
                    "changed input needs complete FDE records".to_owned(),
                ));
            }
            patched_section_count += resolved_fdes.len();
            for resolved in resolved_fdes {
                previous.fdes.push(resolved.record);
                patches.push(resolved.patch);
                if let Some(change) = resolved.eh_frame_hdr_change {
                    eh_frame_hdr_changes.push(change);
                }
                previous.sections_file = None;
            }
        }
        Err(reason) => return Ok(ChangedInputPatchResult::Unsupported(reason)),
    }
    match eh_frame_hdr_patches_for_fde_changes(&output, &eh_frame_hdr_changes)? {
        Ok(header_patches) => patches.extend(header_patches),
        Err(reason) => return Ok(ChangedInputPatchResult::Unsupported(reason)),
    }
    if let Some(reason) = patch_output_range_rejection_reason(&patches) {
        return Ok(ChangedInputPatchResult::Unsupported(reason));
    }

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

    mark_incremental_update_started(state_dir, "patch changed inputs")?;

    let mut patched_ranges = Vec::new();
    for mut patch in patches {
        let start = patch.output_offset as usize;
        let end = start
            .checked_add(patch.size as usize)
            .context("Incremental patch output range overflow")?;
        let Some(output_range) = output.get_mut(start..end) else {
            return Ok(ChangedInputPatchResult::StartedUnsupported(
                "changed patch output range is out of bounds".to_owned(),
            ));
        };
        if patch.data.len() > output_range.len() {
            return Ok(ChangedInputPatchResult::StartedUnsupported(
                "changed patch data does not fit in the previous output range".to_owned(),
            ));
        }
        if let Some(deferred_relocation) = patch.deferred_relocation
            && let Err(reason) = materialize_deferred_relocation_patch(
                &mut patch.data,
                output_range,
                deferred_relocation,
            )
        {
            return Ok(ChangedInputPatchResult::StartedUnsupported(reason));
        }
        for preserve_range in &patch.preserve_ranges {
            let Some(data_range) = patch.data.get_mut(preserve_range.clone()) else {
                return Ok(ChangedInputPatchResult::StartedUnsupported(
                    "changed patch preserve range is out of bounds".to_owned(),
                ));
            };
            let Some(previous_range) = output_range.get(preserve_range.clone()) else {
                return Ok(ChangedInputPatchResult::StartedUnsupported(
                    "changed patch preserve range is out of bounds".to_owned(),
                ));
            };
            data_range.copy_from_slice(previous_range);
        }
        for adjustment in &patch.adjustments {
            let Some(data_range) = patch.data.get_mut(adjustment.range.clone()) else {
                return Ok(ChangedInputPatchResult::StartedUnsupported(
                    "changed patch adjustment range is out of bounds".to_owned(),
                ));
            };
            if let Err(reason) = apply_addend_delta(data_range, adjustment.addend_delta) {
                return Ok(ChangedInputPatchResult::StartedUnsupported(reason));
            }
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
    refresh_input_file_identities_at_indices(
        &mut previous.input_files,
        changed_inputs.iter().map(|(input_index, _)| *input_index),
    );
    if let Some(reason) = input_content_mismatch_reason(&expected_changed_inputs) {
        return Ok(ChangedInputPatchResult::StartedUnsupported(reason));
    }
    if let Some(reason) = input_identity_mismatch_reason(&previous.input_files)? {
        return Ok(ChangedInputPatchResult::StartedUnsupported(reason));
    }
    PersistedState {
        args_hash: previous.args_hash,
        link_options_hash: previous.link_options_hash,
        input_order_hash: previous.input_order_hash,
        sld_version: previous.sld_version,
        link_start: current_link_start,
        output,
        build_id_hashes,
        input_files: previous.input_files,
        sections: previous.sections,
        relocations: previous.relocations,
        fdes: previous.fdes,
        dynamic_relocations: previous.dynamic_relocations,
        sections_file: previous.sections_file,
    }
    .write_metadata_update_for_inputs(state_dir, metadata_update_input_indices)?;
    clear_incremental_update_marker(state_dir)?;

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

struct PreviousPatchState {
    fingerprint: String,
    sections: Vec<PatchSection>,
}

fn patch_sections_from_previous_state(
    input: &FileState,
    path: &Path,
) -> std::result::Result<PreviousPatchState, String> {
    let Some(previous_patch) = input.patch.as_ref() else {
        return Err(format!("missing patch metadata for `{}`", path.display()));
    };
    let sections = if previous_patch.sections.is_empty() {
        previous_patch
            .raw_sections
            .as_ref()
            .map(|raw| parse_patch_sections(input.path.as_str(), raw))
            .transpose()
            .map_err(|error| format!("{error:?}"))?
            .unwrap_or_default()
    } else {
        previous_patch.sections.clone()
    };
    if sections.is_empty() {
        return Err(format!(
            "no patchable sections recorded for `{}`",
            path.display()
        ));
    }
    let patch_section_keys = sections
        .iter()
        .map(|section| (section.input.as_str(), section.section_index))
        .collect::<HashSet<_>>();
    if patch_section_keys.len() != sections.len() {
        return Err(format!(
            "duplicate patchable section metadata for `{}`",
            path.display()
        ));
    }
    Ok(PreviousPatchState {
        fingerprint: previous_patch.fingerprint.clone(),
        sections: sections
            .iter()
            .map(|section| PatchSection {
                input: section.input.clone(),
                section_index: section.section_index,
                section_name: section.section_name.clone(),
                input_size: section.input_size,
                output_offset: section.output_offset,
                output_size: section.output_size,
                data_hash: section.data_hash.clone(),
            })
            .collect(),
    })
}

struct SectionPatch {
    output_offset: u64,
    size: u64,
    data: Vec<u8>,
    deferred_relocation: Option<DeferredRelocationPatch>,
    preserve_ranges: Vec<std::ops::Range<usize>>,
    adjustments: Vec<PatchAdjustment>,
}

#[derive(Clone, Copy)]
struct DeferredRelocationPatch {
    rel_info: RelocationKindInfo,
    previous_written_value: u64,
    written_value: u64,
}

struct PatchAdjustment {
    range: std::ops::Range<usize>,
    addend_delta: i64,
}

struct ResolvedSectionPatch {
    section: PatchSection,
    patch: SectionPatch,
}

struct DynamicRelocationPatch {
    record: DynamicRelocationRecord,
    input_range: Option<std::ops::Range<usize>>,
    patch: SectionPatch,
    is_new: bool,
}

struct RelocationAddendPatches {
    input_ranges: Vec<std::ops::Range<usize>>,
    output_patches: Vec<SectionPatch>,
}

struct RelocationTargetPatches {
    input_ranges: Vec<std::ops::Range<usize>>,
    output_patches: Vec<SectionPatch>,
    output_symbols: Vec<RelocationTargetSymbolPatch>,
}

#[derive(Clone)]
struct RelocationTargetSymbolPatch {
    target_name: String,
    previous_target_value: u64,
    target_value: u64,
}

struct FdeRelocationPatch {
    input_ranges: Vec<std::ops::Range<usize>>,
    patch: Option<SectionPatch>,
    eh_frame_hdr_change: Option<EhFrameHdrChange>,
    record_update: Option<FdeRecordUpdate>,
}

#[derive(Clone)]
struct FdeRecordUpdate {
    previous: FdeRecord,
    current: FdeRecord,
}

struct FdeAddCandidate {
    input_ranges: Vec<std::ops::Range<usize>>,
    input_file: String,
    input: String,
    target_section_index: u32,
    eh_frame_section_index: u32,
    input_offset: u64,
    target_section_offset: u64,
    target_output_offset: u64,
    fde_data: Vec<u8>,
    pc_begin_range: std::ops::Range<usize>,
    cie_input_offset: u64,
    cie_reference_fde_output_offset: u64,
}

struct ResolvedFdeAdd {
    patch: SectionPatch,
    record: FdeRecord,
    eh_frame_hdr_change: Option<EhFrameHdrChange>,
}

#[derive(Clone)]
enum EhFrameHdrChange {
    Adjust(EhFrameHdrDelta),
    Remove(FdeRecord),
    Add(EhFrameHdrEntryPatch),
}

#[derive(Clone)]
struct EhFrameHdrDelta {
    fde_output_offset: u64,
    frame_ptr_delta: i64,
}

const GENERATED_SECTION_INPUT_FILE: &str = "generated";
const GENERATED_SECTION_INDEX: u32 = 0;

struct ExpectedInputContent {
    path: PathBuf,
    len: u64,
    hash: String,
}

impl ExpectedInputContent {
    fn from_bytes(path: &Path, bytes: &[u8]) -> Self {
        let content = FileContentState::from_bytes(bytes);
        Self {
            path: path.to_owned(),
            len: content.len,
            hash: content.hash,
        }
    }
}

fn patch_output_range_rejection_reason(patches: &[SectionPatch]) -> Option<String> {
    let mut ranges = Vec::with_capacity(patches.len());
    for patch in patches {
        let Ok(start) = usize::try_from(patch.output_offset) else {
            return Some("changed patch output range is out of bounds".to_owned());
        };
        let Some(end) = start.checked_add(patch.size as usize) else {
            return Some("changed patch output range overflow".to_owned());
        };
        ranges.push(start..end);
    }
    ranges.sort_by_key(|range| range.start);

    let mut previous_end = 0;
    for range in ranges {
        if !range.is_empty() && range.start < previous_end {
            return Some("changed patch output ranges overlap".to_owned());
        }
        previous_end = previous_end.max(range.end);
    }
    None
}

fn apply_addend_delta(data: &mut [u8], addend_delta: i64) -> std::result::Result<(), String> {
    match data.len() {
        4 => {
            let value = i64::from(i32::from_le_bytes(data.try_into().unwrap()));
            let adjusted = value
                .checked_add(addend_delta)
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| "changed .eh_frame relocation adjustment overflowed".to_owned())?;
            data.copy_from_slice(&adjusted.to_le_bytes());
            Ok(())
        }
        8 => {
            let value = i64::from_le_bytes(data.try_into().unwrap());
            let adjusted = value
                .checked_add(addend_delta)
                .ok_or_else(|| "changed .eh_frame relocation adjustment overflowed".to_owned())?;
            data.copy_from_slice(&adjusted.to_le_bytes());
            Ok(())
        }
        _ => Err("unsupported .eh_frame relocation field size for incremental patch".to_owned()),
    }
}

fn materialize_deferred_relocation_patch(
    data: &mut [u8],
    previous_output: &[u8],
    deferred_relocation: DeferredRelocationPatch,
) -> std::result::Result<(), String> {
    if data.len() != previous_output.len() {
        return Err("deferred relocation patch output size changed".to_owned());
    }
    let RelocationSize::BitMasking(mask) = deferred_relocation.rel_info.size else {
        return Err("deferred relocation patch is not instruction-shaped".to_owned());
    };
    if mask.instruction.write_windows_size() != data.len() {
        return Err("deferred relocation patch output size changed".to_owned());
    }

    let mut replayed_previous_output = previous_output.to_vec();
    deferred_relocation
        .rel_info
        .write_to_buffer(
            deferred_relocation.previous_written_value,
            &mut replayed_previous_output,
        )
        .map_err(|error| format!("failed to validate deferred relocation patch: {error:#}"))?;
    if replayed_previous_output != previous_output {
        return Err("deferred relocation patch encoding changed".to_owned());
    }

    data.copy_from_slice(previous_output);
    deferred_relocation
        .rel_info
        .write_to_buffer(deferred_relocation.written_value, data)
        .map_err(|error| format!("failed to encode deferred relocation patch: {error:#}"))
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
    input: String,
    section_index: u32,
    section_name: Option<String>,
    input_size: u64,
    output_offset: u64,
    output_size: u64,
    data_hash: Option<String>,
}

#[derive(Clone)]
struct MatchedPatchSection {
    previous: PatchSection,
    current: PatchSection,
}

struct MatchedPatchSections {
    sections: Vec<MatchedPatchSection>,
    changed_sections: Vec<PatchSection>,
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

struct PatchInputBytes<'data> {
    bytes: &'data [u8],
    file_offset: usize,
}

struct ParsedPatchInputRef {
    identifier: Vec<u8>,
    range: std::ops::Range<usize>,
}

enum ArchiveMemberMatch<'data> {
    Unique(PatchInputBytes<'data>),
    Ambiguous,
    Unavailable,
}

impl MatchedPatchSection {
    fn same(section: PatchSection) -> Self {
        Self {
            previous: section.clone(),
            current: section,
        }
    }
}

fn update_section_records_for_matched_patches(
    input_file: &str,
    matched_sections: &[MatchedPatchSection],
    records: &mut [SectionRecord],
) -> bool {
    if matched_sections.len() == 1 {
        return update_section_record_for_matched_patch(input_file, &matched_sections[0], records);
    }

    let updates = matched_sections
        .iter()
        .map(|matched| {
            (
                section_record_update_key(input_file, &matched.previous),
                &matched.current,
            )
        })
        .collect::<HashMap<_, _>>();

    let mut changed = false;
    for record in records {
        let Some(current) = updates.get(&(
            record.input_file.as_str(),
            record.input.as_str(),
            record.section_index,
            record.output_offset,
            record.size,
        )) else {
            continue;
        };

        if update_section_record(record, current) {
            changed = true;
        }
    }
    changed
}

fn update_section_record_for_matched_patch(
    input_file: &str,
    matched: &MatchedPatchSection,
    records: &mut [SectionRecord],
) -> bool {
    let Some(record) = records.iter_mut().find(|record| {
        record.input_file == input_file
            && record.input == matched.previous.input
            && record.section_index == matched.previous.section_index
            && record.output_offset == matched.previous.output_offset
            && record.size == matched.previous.output_size
    }) else {
        return false;
    };

    update_section_record(record, &matched.current)
}

fn section_record_update_key<'a>(
    input_file: &'a str,
    section: &'a PatchSection,
) -> (&'a str, &'a str, u32, u64, u64) {
    (
        input_file,
        section.input.as_str(),
        section.section_index,
        section.output_offset,
        section.output_size,
    )
}

fn update_section_record(record: &mut SectionRecord, current: &PatchSection) -> bool {
    if record.input == current.input
        && record.section_index == current.section_index
        && record.output_offset == current.output_offset
        && record.size == current.output_size
    {
        return false;
    }

    record.input = current.input.clone().into();
    record.section_index = current.section_index;
    record.output_offset = current.output_offset;
    record.size = current.output_size;
    true
}

fn update_matched_patch_current_sections(
    matched_sections: &mut [MatchedPatchSection],
    current_sections: &[PatchSection],
) {
    for (matched, current) in matched_sections.iter_mut().zip(current_sections) {
        matched.current = current.clone();
    }
}

impl PreparedState {
    pub(crate) fn begin_update(&self) -> Result {
        if self.mode == IncrementalMode::Disabled {
            return Ok(());
        }
        mark_incremental_update_started(&self.current.state_dir, "link output")
    }

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

    fn intern_input_texts(&self, input: InputRef<'_>) -> (SharedText, SharedText) {
        (
            self.record_texts.intern(encode_path(&input.file.filename)),
            self.record_texts.intern(encode_input_ref(input)),
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

        let (input_file, input_text) = self.intern_input_texts(input);
        let record = SectionRecord::new_with_texts(
            input_file,
            input_text,
            section_index,
            output_offset,
            size,
        );
        self.current_sections.lock().unwrap().push(record.clone());

        if !allow_reuse {
            return false;
        }
        if !self.can_reuse_unchanged_sections() {
            return false;
        }
        if !self.reusable_inputs.contains(record.input_file.as_str()) {
            return false;
        }
        if !self.previous_sections.contains(&record) {
            return false;
        }

        self.reused_sections.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub(crate) fn record_generated_section(&self, name: &str, output_offset: u64, size: u64) {
        if self.mode == IncrementalMode::Disabled || size == 0 {
            return;
        }
        self.current_sections
            .lock()
            .unwrap()
            .push(generated_section_record(name, output_offset, size));
    }

    pub(crate) fn record_eh_frame_fde(
        &self,
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        eh_frame_section_index: object::SectionIndex,
        input_offset: u64,
        output_offset: u64,
        size: u64,
    ) {
        if self.mode == IncrementalMode::Disabled || size == 0 {
            return;
        }
        let (input_file, input_text) = self.intern_input_texts(input);
        self.current_fdes
            .lock()
            .unwrap()
            .push(FdeRecord::new_with_texts(
                input_file,
                input_text,
                section_index,
                eh_frame_section_index,
                input_offset,
                output_offset,
                size,
            ));
    }

    pub(crate) fn record_relocation(
        &self,
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        target_symbol_id: u32,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
        kind: u32,
        addend: i64,
        written_value: u64,
        target_value: u64,
        target_name: Option<String>,
        target: Option<(InputRef<'_>, object::SectionIndex, u64)>,
    ) {
        if self.mode == IncrementalMode::Disabled || size == 0 {
            return;
        }
        let (input_file, input_text) = self.intern_input_texts(input);
        let target = target.map(|(target_input, target_section_index, section_offset)| {
            let (target_input_file, target_input_text) = self.intern_input_texts(target_input);
            (
                target_input_file,
                target_input_text,
                target_section_index,
                section_offset,
            )
        });
        self.current_relocations
            .lock()
            .unwrap()
            .push(RelocationRecord::new_with_texts(
                input_file,
                input_text,
                section_index,
                target_symbol_id,
                relocation_offset,
                output_offset,
                size,
                kind,
                addend,
                written_value,
                target_value,
                target_name,
                target,
            ));
    }

    pub(crate) fn record_dynamic_relocation_with_output_info(
        &self,
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
        output_info: Option<(u64, u64)>,
    ) {
        if self.mode == IncrementalMode::Disabled || size == 0 {
            return;
        }
        let (input_file, input_text) = self.intern_input_texts(input);
        self.current_dynamic_relocations.lock().unwrap().push(
            DynamicRelocationRecord::new_with_texts(
                input_file,
                input_text,
                section_index,
                relocation_offset,
                output_offset,
                size,
                output_info,
            ),
        );
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
        let output_path = args.output().to_owned();
        let mut output_bytes = LazyOutputBytes::new(|| read_output_bytes(&output_path));
        let (build_id_hashes, build_id_tree) = if args.has_incremental_fast_build_id() {
            build_id_hash_state_from_output(output_bytes.get()?)?
        } else {
            (None, None)
        };

        let mut sections = {
            let mut current_sections = self.current_sections.lock().unwrap();
            std::mem::take(&mut *current_sections)
        };
        if sections.is_empty() && self.mode == IncrementalMode::Reuse {
            sections.extend(self.previous_sections.iter().cloned());
        }
        sections.sort();

        let mut relocations = {
            let mut current_relocations = self.current_relocations.lock().unwrap();
            std::mem::take(&mut *current_relocations)
        };
        if relocations.is_empty() && self.mode == IncrementalMode::Reuse {
            relocations.extend(self.previous_relocations.iter().cloned());
        }
        relocations.sort();

        let mut fdes = {
            let mut current_fdes = self.current_fdes.lock().unwrap();
            std::mem::take(&mut *current_fdes)
        };
        if fdes.is_empty() && self.mode == IncrementalMode::Reuse {
            fdes.extend(self.previous_fdes.iter().cloned());
        }
        fdes.sort();

        let mut dynamic_relocations = {
            let mut current_dynamic_relocations = self.current_dynamic_relocations.lock().unwrap();
            std::mem::take(&mut *current_dynamic_relocations)
        };
        if dynamic_relocations.is_empty() && self.mode == IncrementalMode::Reuse {
            dynamic_relocations.extend(self.previous_dynamic_relocations.iter().cloned());
        }
        dynamic_relocations.sort();

        let mut input_files = self.current.input_files.clone();
        record_patch_fingerprints(
            &mut input_files,
            file_loader,
            &sections,
            &relocations,
            &fdes,
            &dynamic_relocations,
            &mut output_bytes,
        )?;
        snapshot_loaded_files(&self.current.state_dir, file_loader)?;
        refresh_input_file_identities(&mut input_files);

        let state = PersistedState {
            args_hash: self.current.args_hash.clone(),
            link_options_hash: Some(self.current.link_options_hash.clone()),
            input_order_hash: Some(self.current.input_order_hash.clone()),
            sld_version: Some(self.current.sld_version.clone()),
            link_start: self.current.link_start.clone(),
            output,
            build_id_hashes,
            input_files,
            sections,
            relocations,
            fdes,
            dynamic_relocations,
            sections_file: None,
        };

        write_build_id_hash_tree(&self.current.state_dir, build_id_tree.as_deref())?;
        state.write(&self.current.state_dir)?;
        clear_incremental_update_marker(&self.current.state_dir)?;
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
    if let Some(reason) = interrupted_update_relink_reason(&current.state_dir) {
        return IncrementalMode::Relink {
            reason,
            can_reuse_unchanged_sections: false,
        };
    }

    if let Some(reason) =
        sld_version_relink_reason(previous.sld_version.as_deref(), &current.sld_version)
    {
        return IncrementalMode::Relink {
            reason: reason.to_owned(),
            can_reuse_unchanged_sections: false,
        };
    }

    let previous_link_options_hash = previous
        .link_options_hash
        .as_deref()
        .unwrap_or(&previous.args_hash);
    if current.link_options_hash != previous_link_options_hash {
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

    match previous.input_order_hash.as_deref() {
        Some(previous_order) if current.input_order_hash == previous_order => {}
        Some(_) => {
            return IncrementalMode::Relink {
                reason: "input file order changed".to_owned(),
                can_reuse_unchanged_sections: true,
            };
        }
        None => {
            return IncrementalMode::Relink {
                reason: "input file order missing from previous state".to_owned(),
                can_reuse_unchanged_sections: false,
            };
        }
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
            link_options_hash: link_options_hash(args),
            input_order_hash: input_order_hash(file_loader),
            sld_version: sld_version(args),
            link_start: link_start_marker_identity(&state_dir_for_output(args.output())),
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
        Self::read_impl(state_dir, true, PatchSectionReadMode::Parse)
    }

    fn read_metadata(state_dir: &Path) -> Result<Option<Self>> {
        Self::read_impl(state_dir, false, PatchSectionReadMode::PreserveRaw)
    }

    fn read_impl(
        state_dir: &Path,
        load_sections: bool,
        patch_section_mode: PatchSectionReadMode,
    ) -> Result<Option<Self>> {
        let path = state_dir.join(INDEX_FILE);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let mut state =
            Self::parse_with_section_loader(&contents, patch_section_mode, |sections_file| {
                if !load_sections {
                    return Ok(None);
                }
                read_sections_sidecar(state_dir, sections_file).map(Some)
            })?;
        state.apply_metadata_update(state_dir, patch_section_mode)?;
        Ok(Some(state))
    }

    fn read_records_for_input_files(
        &mut self,
        state_dir: &Path,
        input_files: &HashSet<String>,
    ) -> Result {
        let Some(sections_file) = self.sections_file.as_deref() else {
            return Ok(());
        };
        timing_phase!("Read incremental sidecar records");
        let contents = read_sections_sidecar(state_dir, sections_file)?;
        let records = parse_compact_records_block_for_input_files(contents.lines(), input_files)?;
        self.sections = records.sections;
        self.relocations = records.relocations;
        self.fdes = records.fdes;
        self.dynamic_relocations = records.dynamic_relocations;
        Ok(())
    }

    fn read_patch_metadata_for_input_indices(
        &mut self,
        state_dir: &Path,
        input_indices: &HashSet<usize>,
    ) -> Result {
        if input_indices.is_empty() {
            return Ok(());
        }
        let mut remaining = input_indices.clone();
        self.read_patch_metadata_from_update(state_dir, &mut remaining)?;
        if !remaining.is_empty() {
            self.read_patch_metadata_from_index(state_dir, &mut remaining)?;
        }
        Ok(())
    }

    fn read_patch_metadata_from_update(
        &mut self,
        state_dir: &Path,
        input_indices: &mut HashSet<usize>,
    ) -> Result {
        let path = metadata_update_path(state_dir);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut lines = contents.lines();
        let version = lines
            .next()
            .context("Missing incremental metadata update header")?;
        if version != METADATA_UPDATE_VERSION {
            return Err(crate::error!(
                "Unsupported incremental metadata update version `{version}`"
            ));
        }
        let _ = parse_link_start_line(lines.next())?;
        let _ = parse_content_line(lines.next(), "output")?;
        let _ = parse_build_id_hash_line(lines.next())?;
        let input_count: usize = parse_prefixed_line(lines.next(), "inputs")?
            .parse()
            .context("Invalid incremental metadata update input count")?;
        for _ in 0..input_count {
            let rest = parse_prefixed_line(lines.next(), "input")?;
            let (index, input_line) = rest
                .split_once('\t')
                .context("Malformed incremental metadata update input")?;
            let index: usize = index
                .parse()
                .context("Invalid incremental metadata update input index")?;
            if input_indices.remove(&index) {
                let Some(input) = self.input_files.get_mut(index) else {
                    return Err(crate::error!(
                        "Incremental metadata update input index out of bounds"
                    ));
                };
                input.patch = parse_input_line(
                    &format!("input\t{input_line}"),
                    PatchSectionReadMode::PreserveRaw,
                )?
                .patch;
            }
        }
        if lines.next().is_some() {
            return Err(crate::error!(
                "Unexpected trailing incremental metadata update data"
            ));
        }
        Ok(())
    }

    fn read_patch_metadata_from_index(
        &mut self,
        state_dir: &Path,
        input_indices: &mut HashSet<usize>,
    ) -> Result {
        let path = state_dir.join(INDEX_FILE);
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read incremental state `{}`", path.display()))?;
        let mut lines = contents.lines();
        for line in lines.by_ref() {
            if line.starts_with("inputs\t") {
                let input_count: usize = parse_prefixed_line(Some(line), "inputs")?
                    .parse()
                    .context("Invalid incremental input count")?;
                for index in 0..input_count {
                    let line = lines.next().context("Missing incremental input record")?;
                    if input_indices.remove(&index) {
                        let Some(input) = self.input_files.get_mut(index) else {
                            return Err(crate::error!("Incremental input index out of bounds"));
                        };
                        input.patch =
                            parse_input_line(line, PatchSectionReadMode::PreserveRaw)?.patch;
                    }
                    if input_indices.is_empty() {
                        return Ok(());
                    }
                }
                return Ok(());
            }
        }
        Err(crate::error!("Missing incremental input count"))
    }

    #[cfg(test)]
    fn parse(contents: &str) -> Result<Self> {
        Self::parse_with_section_loader(contents, PatchSectionReadMode::Parse, |_| Ok(None))
    }

    fn parse_with_section_loader(
        contents: &str,
        patch_section_mode: PatchSectionReadMode,
        mut load_sections: impl FnMut(&str) -> Result<Option<String>>,
    ) -> Result<Self> {
        let mut lines = contents.lines().peekable();
        let version = lines.next().context("Missing incremental state header")?;
        if version != STATE_VERSION
            && version != STATE_VERSION_V28
            && version != STATE_VERSION_V27
            && version != STATE_VERSION_V26
            && version != STATE_VERSION_V25
            && version != STATE_VERSION_V24
            && version != STATE_VERSION_V23
            && version != STATE_VERSION_V22
            && version != STATE_VERSION_V21
            && version != STATE_VERSION_V20
            && version != STATE_VERSION_V19
            && version != STATE_VERSION_V18
            && version != STATE_VERSION_V17
            && version != STATE_VERSION_V16
            && version != STATE_VERSION_V15
            && version != STATE_VERSION_V14
            && version != STATE_VERSION_V13
            && version != STATE_VERSION_V12
            && version != STATE_VERSION_V11
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
        let link_options_hash = if lines
            .peek()
            .is_some_and(|line| line.starts_with("link-options\t"))
        {
            Some(parse_prefixed_line(lines.next(), "link-options")?.to_owned())
        } else {
            None
        };
        if version == STATE_VERSION && link_options_hash.is_none() {
            return Err(crate::error!(
                "Missing incremental link-options hash in incremental state"
            ));
        }
        let input_order_hash = if lines
            .peek()
            .is_some_and(|line| line.starts_with("input-order\t"))
        {
            Some(parse_prefixed_line(lines.next(), "input-order")?.to_owned())
        } else {
            None
        };
        if version == STATE_VERSION && input_order_hash.is_none() {
            return Err(crate::error!(
                "Missing incremental input-order hash in incremental state"
            ));
        }
        let sld_version = if lines
            .peek()
            .is_some_and(|line| line.starts_with("sld-version\t"))
        {
            Some(parse_prefixed_line(lines.next(), "sld-version")?.to_owned())
        } else {
            None
        };
        if version == STATE_VERSION && sld_version.is_none() {
            return Err(crate::error!("Missing sld version in incremental state"));
        }
        let link_start = if lines
            .peek()
            .is_some_and(|line| line.starts_with("link-start\t"))
        {
            parse_link_start_line(lines.next())?
        } else {
            None
        };
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
            input_files.push(parse_input_line(line, patch_section_mode)?);
        }

        let mut sections_file = None;
        let mut relocations = Vec::new();
        let mut fdes = Vec::new();
        let mut dynamic_relocations = Vec::new();
        let sections = if version == STATE_VERSION
            || version == STATE_VERSION_V28
            || version == STATE_VERSION_V27
            || version == STATE_VERSION_V26
            || version == STATE_VERSION_V25
            || version == STATE_VERSION_V24
            || version == STATE_VERSION_V23
            || version == STATE_VERSION_V22
            || version == STATE_VERSION_V21
            || version == STATE_VERSION_V20
            || version == STATE_VERSION_V19
            || version == STATE_VERSION_V18
            || version == STATE_VERSION_V17
            || version == STATE_VERSION_V16
            || version == STATE_VERSION_V15
            || version == STATE_VERSION_V14
            || version == STATE_VERSION_V13
            || version == STATE_VERSION_V12
            || version == STATE_VERSION_V11
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
                validate_sections_file_name(&file)?;
                let records = load_sections(&file)?
                    .map(|contents| parse_compact_records_block(contents.lines()))
                    .transpose()?
                    .unwrap_or_default();
                sections_file = Some(file);
                relocations = records.relocations;
                fdes = records.fdes;
                dynamic_relocations = records.dynamic_relocations;
                records.sections
            } else {
                let records =
                    parse_compact_records_block(std::iter::once(first_line).chain(&mut lines))?;
                relocations = records.relocations;
                fdes = records.fdes;
                dynamic_relocations = records.dynamic_relocations;
                records.sections
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
            link_options_hash,
            input_order_hash,
            sld_version,
            link_start,
            output,
            build_id_hashes,
            input_files,
            sections,
            relocations,
            fdes,
            dynamic_relocations,
            sections_file,
        })
    }

    fn write(&self, state_dir: &Path) -> Result {
        let sections_file = self.write_sections_streaming(state_dir)?;
        self.write_index_with_sections_file(state_dir, &sections_file)
    }

    fn write_metadata_update(&self, state_dir: &Path) -> Result {
        if self.sections_file.is_some() {
            self.write_index(state_dir)
        } else {
            self.write(state_dir)
        }
    }

    fn write_metadata_update_for_inputs(
        &self,
        state_dir: &Path,
        input_indices: &[usize],
    ) -> Result {
        if self.sections_file.is_none() {
            return self.write(state_dir);
        }
        std::fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "Failed to create incremental state directory `{}`",
                state_dir.display()
            )
        })?;

        let path = metadata_update_path(state_dir);
        let tmp_path = state_dir.join(format!("{METADATA_UPDATE_FILE}.tmp"));
        std::fs::write(&tmp_path, self.render_metadata_update(input_indices)).with_context(
            || {
                format!(
                    "Failed to write incremental metadata update `{}`",
                    tmp_path.display()
                )
            },
        )?;
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "Failed to install incremental metadata update `{}`",
                path.display()
            )
        })?;
        Ok(())
    }

    fn write_index(&self, state_dir: &Path) -> Result {
        let sections_file = self.sections_file.as_deref().unwrap_or(SECTIONS_FILE);
        self.write_index_with_sections_file(state_dir, sections_file)
    }

    fn write_index_with_sections_file(&self, state_dir: &Path, sections_file: &str) -> Result {
        std::fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "Failed to create incremental state directory `{}`",
                state_dir.display()
            )
        })?;

        let path = state_dir.join(INDEX_FILE);
        let tmp_path = state_dir.join(format!("{INDEX_FILE}.tmp"));
        std::fs::write(&tmp_path, self.render_index(sections_file)).with_context(|| {
            format!("Failed to write incremental state `{}`", tmp_path.display())
        })?;
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to install incremental state `{}`", path.display()))?;
        let _ = std::fs::remove_file(metadata_update_path(state_dir));
        Ok(())
    }

    #[cfg(test)]
    fn write_sections(&self, state_dir: &Path, file_name: &str, contents: &str) -> Result {
        std::fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "Failed to create incremental state directory `{}`",
                state_dir.display()
            )
        })?;

        let path = state_dir.join(file_name);
        let tmp_path = state_dir.join(format!("{file_name}.tmp"));
        std::fs::write(&tmp_path, contents).with_context(|| {
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

    fn write_sections_streaming(&self, state_dir: &Path) -> Result<String> {
        std::fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "Failed to create incremental state directory `{}`",
                state_dir.display()
            )
        })?;

        let tmp_path = state_dir.join(format!("{SECTIONS_FILE}.tmp"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .with_context(|| {
                format!(
                    "Failed to create incremental sections `{}`",
                    tmp_path.display()
                )
            })?;
        let mut writer = SectionSidecarWriter::new(file);
        if self.write_rendered_sections(&mut writer).is_err() {
            if let Some(error) = writer.take_error() {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to write incremental sections `{}`",
                        tmp_path.display()
                    )
                });
            }
            return Err(crate::error!(
                "Failed to render incremental sections `{}`",
                tmp_path.display()
            ));
        }
        let hash = writer.finish().with_context(|| {
            format!(
                "Failed to finish incremental sections `{}`",
                tmp_path.display()
            )
        })?;
        let file_name = format!("{SECTIONS_FILE_PREFIX}{hash}");
        let path = state_dir.join(&file_name);
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "Failed to install incremental sections `{}`",
                path.display()
            )
        })?;
        Ok(file_name)
    }

    fn render_index(&self, sections_file: &str) -> String {
        let mut out = self.render_header_and_inputs();
        writeln!(&mut out, "sections-file\t{sections_file}").unwrap();
        out
    }

    fn render_metadata_update(&self, input_indices: &[usize]) -> String {
        let mut out = String::new();
        writeln!(&mut out, "{METADATA_UPDATE_VERSION}").unwrap();
        writeln!(
            &mut out,
            "link-start\t{}",
            self.link_start
                .as_ref()
                .map_or_else(|| ABSENT_FIELD.to_owned(), FileIdentity::render)
        )
        .unwrap();
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
                .map_or_else(|| ABSENT_FIELD.to_owned(), render_build_id_hash_state)
        )
        .unwrap();
        let mut input_indices = input_indices.to_vec();
        input_indices.sort_unstable();
        input_indices.dedup();
        writeln!(&mut out, "inputs\t{}", input_indices.len()).unwrap();
        for index in input_indices {
            writeln!(
                &mut out,
                "input\t{}\t{}",
                index,
                render_input_line_rest(&self.input_files[index])
            )
            .unwrap();
        }
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
        if let Some(hash) = &self.link_options_hash {
            writeln!(&mut out, "link-options\t{hash}").unwrap();
        }
        if let Some(hash) = &self.input_order_hash {
            writeln!(&mut out, "input-order\t{hash}").unwrap();
        }
        if let Some(version) = &self.sld_version {
            writeln!(&mut out, "sld-version\t{version}").unwrap();
        }
        writeln!(
            &mut out,
            "link-start\t{}",
            self.link_start
                .as_ref()
                .map_or_else(|| ABSENT_FIELD.to_owned(), FileIdentity::render)
        )
        .unwrap();
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
                .map_or_else(|| ABSENT_FIELD.to_owned(), render_build_id_hash_state)
        )
        .unwrap();
        writeln!(&mut out, "inputs\t{}", self.input_files.len()).unwrap();
        for input in &self.input_files {
            writeln!(&mut out, "input\t{}", render_input_line_rest(input)).unwrap();
        }
        out
    }

    fn apply_metadata_update(
        &mut self,
        state_dir: &Path,
        patch_section_mode: PatchSectionReadMode,
    ) -> Result {
        let path = metadata_update_path(state_dir);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut lines = contents.lines();
        let version = lines
            .next()
            .context("Missing incremental metadata update header")?;
        if version != METADATA_UPDATE_VERSION {
            return Err(crate::error!(
                "Unsupported incremental metadata update version `{version}`"
            ));
        }
        self.link_start = parse_link_start_line(lines.next())?;
        self.output = parse_content_line(lines.next(), "output")?;
        self.build_id_hashes = parse_build_id_hash_line(lines.next())?;
        let input_count: usize = parse_prefixed_line(lines.next(), "inputs")?
            .parse()
            .context("Invalid incremental metadata update input count")?;
        for _ in 0..input_count {
            let rest = parse_prefixed_line(lines.next(), "input")?;
            let (index, input_line) = rest
                .split_once('\t')
                .context("Malformed incremental metadata update input")?;
            let index: usize = index
                .parse()
                .context("Invalid incremental metadata update input index")?;
            let Some(input) = self.input_files.get_mut(index) else {
                return Err(crate::error!(
                    "Incremental metadata update input index out of bounds"
                ));
            };
            *input = parse_input_line(&format!("input\t{input_line}"), patch_section_mode)?;
        }
        if lines.next().is_some() {
            return Err(crate::error!(
                "Unexpected trailing incremental metadata update data"
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn render_sections(&self) -> String {
        let mut out = String::new();
        self.write_rendered_sections(&mut out)
            .expect("writing incremental sections to String should not fail");
        out
    }

    fn write_rendered_sections(&self, mut out: &mut impl std::fmt::Write) -> std::fmt::Result {
        let mut section_inputs = Vec::new();
        let mut section_input_ids = HashMap::new();
        for section in &self.sections {
            add_section_input(
                &mut section_inputs,
                &mut section_input_ids,
                section.input_file.as_str(),
                section.input.as_str(),
            );
        }
        for relocation in &self.relocations {
            add_section_input(
                &mut section_inputs,
                &mut section_input_ids,
                relocation.input_file.as_str(),
                relocation.input.as_str(),
            );
            if let Some(target) = &relocation.target {
                add_section_input(
                    &mut section_inputs,
                    &mut section_input_ids,
                    target.input_file.as_str(),
                    target.input.as_str(),
                );
            }
        }
        for fde in &self.fdes {
            add_section_input(
                &mut section_inputs,
                &mut section_input_ids,
                fde.input_file.as_str(),
                fde.input.as_str(),
            );
        }
        for relocation in &self.dynamic_relocations {
            add_section_input(
                &mut section_inputs,
                &mut section_input_ids,
                relocation.input_file.as_str(),
                relocation.input.as_str(),
            );
        }

        writeln!(&mut out, "section-inputs\t{}", section_inputs.len())?;
        for (input_file, input) in section_inputs {
            writeln!(&mut out, "section-input\t{input_file}\t{input}")?;
        }

        writeln!(&mut out, "sections\t{}", self.sections.len())?;
        for section in &self.sections {
            let section_input_id =
                section_input_ids[&(section.input_file.as_str(), section.input.as_str())];
            writeln!(
                &mut out,
                "section\t{}\t{}\t{}\t{}",
                section_input_id, section.section_index, section.output_offset, section.size
            )?;
        }
        writeln!(&mut out, "relocs\t{}", self.relocations.len())?;
        for relocation in &self.relocations {
            let section_input_id =
                section_input_ids[&(relocation.input_file.as_str(), relocation.input.as_str())];
            let (target_section_input_id, target_section_index, target_section_offset) =
                relocation.target.as_ref().map_or(
                    (
                        ABSENT_FIELD.to_owned(),
                        ABSENT_FIELD.to_owned(),
                        ABSENT_FIELD.to_owned(),
                    ),
                    |target| {
                        (
                            section_input_ids[&(target.input_file.as_str(), target.input.as_str())]
                                .to_string(),
                            target.section_index.to_string(),
                            target.section_offset.to_string(),
                        )
                    },
                );
            writeln!(
                &mut out,
                "reloc2\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                section_input_id,
                relocation.section_index,
                relocation.target_symbol_id,
                relocation.relocation_offset,
                relocation.output_offset,
                relocation.size,
                relocation.kind,
                relocation.addend,
                relocation
                    .written_value
                    .map_or_else(|| ABSENT_FIELD.to_owned(), |value| value.to_string()),
                relocation.target_value,
                relocation.target_name.as_deref().unwrap_or(ABSENT_FIELD),
                target_section_input_id,
                target_section_index,
                target_section_offset
            )?;
        }
        writeln!(&mut out, "fdes\t{}", self.fdes.len())?;
        for fde in &self.fdes {
            let section_input_id =
                section_input_ids[&(fde.input_file.as_str(), fde.input.as_str())];
            writeln!(
                &mut out,
                "fde\t{}\t{}\t{}\t{}\t{}\t{}",
                section_input_id,
                fde.section_index,
                fde.eh_frame_section_index,
                fde.input_offset,
                fde.output_offset,
                fde.size
            )?;
        }
        writeln!(&mut out, "dynrels\t{}", self.dynamic_relocations.len())?;
        for relocation in &self.dynamic_relocations {
            let section_input_id =
                section_input_ids[&(relocation.input_file.as_str(), relocation.input.as_str())];
            if let (Some(output_r_offset), Some(output_r_info)) =
                (relocation.output_r_offset, relocation.output_r_info)
            {
                writeln!(
                    &mut out,
                    "dynrel\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    section_input_id,
                    relocation.section_index,
                    relocation.relocation_offset,
                    relocation.output_offset,
                    relocation.size,
                    output_r_offset,
                    output_r_info
                )?;
            } else {
                writeln!(
                    &mut out,
                    "dynrel\t{}\t{}\t{}\t{}\t{}",
                    section_input_id,
                    relocation.section_index,
                    relocation.relocation_offset,
                    relocation.output_offset,
                    relocation.size
                )?;
            }
        }
        Ok(())
    }
}

struct SectionSidecarWriter {
    file: std::io::BufWriter<std::fs::File>,
    hasher: blake3::Hasher,
    error: Option<std::io::Error>,
}

impl SectionSidecarWriter {
    fn new(file: std::fs::File) -> Self {
        Self {
            file: std::io::BufWriter::new(file),
            hasher: blake3::Hasher::new(),
            error: None,
        }
    }

    fn take_error(&mut self) -> Option<std::io::Error> {
        self.error.take()
    }

    fn finish(mut self) -> std::io::Result<String> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        self.file.flush()?;
        Ok(self.hasher.finalize().to_hex().to_string())
    }
}

impl std::fmt::Write for SectionSidecarWriter {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        if self.error.is_some() {
            return Err(std::fmt::Error);
        }
        if let Err(error) = self.file.write_all(text.as_bytes()) {
            self.error = Some(error);
            return Err(std::fmt::Error);
        }
        self.hasher.update(text.as_bytes());
        Ok(())
    }
}

fn add_section_input<'a>(
    section_inputs: &mut Vec<(&'a str, &'a str)>,
    section_input_ids: &mut HashMap<(&'a str, &'a str), usize>,
    input_file: &'a str,
    input: &'a str,
) {
    let key = (input_file, input);
    if !section_input_ids.contains_key(&key) {
        let index = section_inputs.len();
        section_input_ids.insert(key, index);
        section_inputs.push(key);
    }
}

fn read_sections_sidecar(state_dir: &Path, file_name: &str) -> Result<String> {
    validate_sections_file_name(file_name)?;
    let path = state_dir.join(file_name);
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read incremental sections `{}`", path.display()))?;
    if file_name.starts_with(SECTIONS_FILE_PREFIX) {
        let expected_name = section_sidecar_file_name(&contents);
        if file_name != expected_name {
            return Err(crate::error!(
                "Incremental sections `{}` do not match their content hash",
                path.display()
            ));
        }
    }
    Ok(contents)
}

fn validate_sections_file_name(file_name: &str) -> Result {
    if file_name == SECTIONS_FILE {
        return Ok(());
    }
    if !file_name.starts_with(SECTIONS_FILE_PREFIX)
        || file_name.contains('/')
        || file_name.contains('\\')
        || Path::new(file_name).is_absolute()
    {
        return Err(crate::error!(
            "Invalid incremental sections sidecar name `{file_name}`"
        ));
    }
    Ok(())
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
    #[cfg(test)]
    fn new(
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        output_offset: u64,
        size: u64,
    ) -> Self {
        Self::new_with_texts(
            encode_path(&input.file.filename).into(),
            encode_input_ref(input).into(),
            section_index,
            output_offset,
            size,
        )
    }

    fn new_with_texts(
        input_file: SharedText,
        input: SharedText,
        section_index: object::SectionIndex,
        output_offset: u64,
        size: u64,
    ) -> Self {
        Self {
            input_file,
            input,
            section_index: section_index.0 as u32,
            output_offset,
            size,
        }
    }
}

impl RelocationRecord {
    #[cfg(test)]
    fn new(
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        target_symbol_id: u32,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
        kind: u32,
        addend: i64,
        written_value: u64,
        target_value: u64,
        target_name: Option<String>,
        target: Option<(InputRef<'_>, object::SectionIndex, u64)>,
    ) -> Self {
        Self::new_with_texts(
            encode_path(&input.file.filename).into(),
            encode_input_ref(input).into(),
            section_index,
            target_symbol_id,
            relocation_offset,
            output_offset,
            size,
            kind,
            addend,
            written_value,
            target_value,
            target_name,
            target.map(|(target_input, target_section_index, section_offset)| {
                (
                    encode_path(&target_input.file.filename).into(),
                    encode_input_ref(target_input).into(),
                    target_section_index,
                    section_offset,
                )
            }),
        )
    }

    fn new_with_texts(
        input_file: SharedText,
        input: SharedText,
        section_index: object::SectionIndex,
        target_symbol_id: u32,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
        kind: u32,
        addend: i64,
        written_value: u64,
        target_value: u64,
        target_name: Option<String>,
        target: Option<(SharedText, SharedText, object::SectionIndex, u64)>,
    ) -> Self {
        Self {
            target_symbol_id,
            written_value: Some(written_value),
            target_value,
            target_name,
            target: target.map(|(input_file, input, section_index, section_offset)| {
                RelocationTargetRecord::new(input_file, input, section_index, section_offset)
            }),
            input_file,
            input,
            section_index: section_index.0 as u32,
            relocation_offset,
            output_offset,
            size,
            kind,
            addend,
        }
    }
}

impl RelocationTargetRecord {
    fn new(
        input_file: SharedText,
        input: SharedText,
        section_index: object::SectionIndex,
        section_offset: u64,
    ) -> Self {
        Self {
            input_file,
            input,
            section_index: section_index.0 as u32,
            section_offset,
        }
    }
}

impl FdeRecord {
    #[cfg(test)]
    fn new(
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        eh_frame_section_index: object::SectionIndex,
        input_offset: u64,
        output_offset: u64,
        size: u64,
    ) -> Self {
        Self::new_with_texts(
            encode_path(&input.file.filename).into(),
            encode_input_ref(input).into(),
            section_index,
            eh_frame_section_index,
            input_offset,
            output_offset,
            size,
        )
    }

    fn new_with_texts(
        input_file: SharedText,
        input: SharedText,
        section_index: object::SectionIndex,
        eh_frame_section_index: object::SectionIndex,
        input_offset: u64,
        output_offset: u64,
        size: u64,
    ) -> Self {
        Self {
            input_file,
            input,
            section_index: section_index.0 as u32,
            eh_frame_section_index: eh_frame_section_index.0 as u32,
            input_offset,
            output_offset,
            size,
        }
    }
}

impl DynamicRelocationRecord {
    #[cfg(test)]
    fn new(
        input: InputRef<'_>,
        section_index: object::SectionIndex,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
        output_info: Option<(u64, u64)>,
    ) -> Self {
        Self::new_with_texts(
            encode_path(&input.file.filename).into(),
            encode_input_ref(input).into(),
            section_index,
            relocation_offset,
            output_offset,
            size,
            output_info,
        )
    }

    fn new_with_texts(
        input_file: SharedText,
        input: SharedText,
        section_index: object::SectionIndex,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
        output_info: Option<(u64, u64)>,
    ) -> Self {
        let (output_r_offset, output_r_info) = output_info
            .map_or((None, None), |(r_offset, r_info)| {
                (Some(r_offset), Some(r_info))
            });
        Self {
            input_file,
            input,
            section_index: section_index.0 as u32,
            relocation_offset,
            output_offset,
            size,
            output_r_offset,
            output_r_info,
        }
    }

    fn has_restorable_rela_output_info(&self) -> bool {
        self.size == crate::elf::RELA_ENTRY_SIZE
            && self.output_r_offset.is_some()
            && self.output_r_info.is_some()
    }
}

fn generated_section_record(name: &str, output_offset: u64, size: u64) -> SectionRecord {
    SectionRecord {
        input_file: GENERATED_SECTION_INPUT_FILE.into(),
        input: name.into(),
        section_index: GENERATED_SECTION_INDEX,
        output_offset,
        size,
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

    fn identity_is_ambiguous_since(&self, link_start: Option<&FileIdentity>) -> bool {
        self.identity
            .as_ref()
            .zip(link_start)
            .is_some_and(|(identity, link_start)| identity.may_have_changed_since(link_start))
    }

    fn identity_matches_snapshot_path(&self, path: &Path) -> Result<bool> {
        let Some(previous) = self.identity.as_ref() else {
            return Ok(false);
        };
        // Hard-link snapshots can have ctime changes from link-count updates while still being the
        // saved snapshot content.
        Ok(FileIdentity::from_path(path)?
            .as_ref()
            .is_some_and(|current| previous.matches_snapshot_identity(current)))
    }

    fn render_identity(&self) -> String {
        self.identity
            .as_ref()
            .map_or_else(|| "-".to_owned(), FileIdentity::render)
    }
}

impl FileIdentity {
    fn may_have_changed_since(&self, lower_bound: &Self) -> bool {
        timestamp_on_or_after(
            self.modified_sec,
            self.modified_nsec,
            lower_bound.modified_sec,
            lower_bound.modified_nsec,
        ) || timestamp_on_or_after(
            self.changed_sec,
            self.changed_nsec,
            lower_bound.modified_sec,
            lower_bound.modified_nsec,
        )
    }

    fn matches_snapshot_identity(&self, other: &Self) -> bool {
        self.len == other.len
            && self.dev == other.dev
            && self.ino == other.ino
            && self.modified_sec == other.modified_sec
            && self.modified_nsec == other.modified_nsec
    }

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

fn timestamp_on_or_after(sec: i64, _nsec: i64, lower_sec: i64, _lower_nsec: i64) -> bool {
    // Some filesystems report coarse timestamps or fail to advance nanoseconds for rapid
    // rewrites. Treat the whole filesystem second as ambiguous once it overlaps the link start.
    sec >= lower_sec
}

struct LazyOutputBytes<F> {
    bytes: Option<memmap2::Mmap>,
    load: Option<F>,
}

impl<F> LazyOutputBytes<F>
where
    F: FnOnce() -> Result<memmap2::Mmap>,
{
    fn new(load: F) -> Self {
        Self {
            bytes: None,
            load: Some(load),
        }
    }

    fn get(&mut self) -> Result<&[u8]> {
        if self.bytes.is_none() {
            let load = self
                .load
                .take()
                .context("Incremental output bytes were already consumed")?;
            self.bytes = Some(load()?);
        }
        Ok(self.bytes.as_deref().unwrap_or_default())
    }
}

fn read_output_bytes(path: &Path) -> Result<memmap2::Mmap> {
    let file = OpenOptions::new().read(true).open(path).with_context(|| {
        format!(
            "Failed to read output `{}` for incremental state",
            path.display()
        )
    })?;
    unsafe { MmapOptions::new().map(&file) }.with_context(|| {
        format!(
            "Failed to mmap output `{}` for incremental state",
            path.display()
        )
    })
}

fn record_patch_fingerprints<F>(
    input_files: &mut [FileState],
    file_loader: &FileLoader<'_>,
    sections: &[SectionRecord],
    relocations: &[RelocationRecord],
    fdes: &[FdeRecord],
    dynamic_relocations: &[DynamicRelocationRecord],
    output: &mut LazyOutputBytes<F>,
) -> Result
where
    F: FnOnce() -> Result<memmap2::Mmap>,
{
    let mut sections_by_file = HashMap::<&str, Vec<&SectionRecord>>::new();
    for section in sections {
        sections_by_file
            .entry(section.input_file.as_str())
            .or_default()
            .push(section);
    }

    let mut dynamic_relocations_by_file = HashMap::<&str, Vec<&DynamicRelocationRecord>>::new();
    for relocation in dynamic_relocations {
        dynamic_relocations_by_file
            .entry(relocation.input_file.as_str())
            .or_default()
            .push(relocation);
    }

    let mut relocations_by_file = HashMap::<&str, Vec<&RelocationRecord>>::new();
    for relocation in relocations {
        relocations_by_file
            .entry(relocation.input_file.as_str())
            .or_default()
            .push(relocation);
    }

    let mut relocation_targets_by_file = HashMap::<&str, Vec<&RelocationRecord>>::new();
    for relocation in relocations {
        if let Some(target) = &relocation.target {
            relocation_targets_by_file
                .entry(target.input_file.as_str())
                .or_default()
                .push(relocation);
        }
    }

    let mut fdes_by_file = HashMap::<&str, Vec<&FdeRecord>>::new();
    for fde in fdes {
        fdes_by_file
            .entry(fde.input_file.as_str())
            .or_default()
            .push(fde);
    }

    if sections_by_file.is_empty()
        && relocations_by_file.is_empty()
        && relocation_targets_by_file.is_empty()
        && dynamic_relocations_by_file.is_empty()
        && fdes_by_file.is_empty()
    {
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
        let input_dynamic_relocations = dynamic_relocations_by_file.get(input.path.as_str());
        let input_relocations = relocations_by_file.get(input.path.as_str());
        let input_relocation_targets = relocation_targets_by_file.get(input.path.as_str());
        let input_fdes = fdes_by_file.get(input.path.as_str());
        if input
            .patch
            .as_ref()
            .is_some_and(|patch| patch_state_matches_section_records(patch, sections))
        {
            continue;
        }
        let Some(input_file) = loaded_by_path.get(&input.path) else {
            input.patch = None;
            continue;
        };
        let patch_sections = direct_copy_patch_sections(
            input_file.data(),
            input.path.as_str(),
            output.get()?,
            sections,
            input_dynamic_relocations
                .into_iter()
                .flat_map(|relocations| relocations.iter().copied()),
        )?;
        let dynamic_relocation_patches = dynamic_relocation_patches_for_input(
            input_file.data(),
            input.path.as_str(),
            input_dynamic_relocations
                .into_iter()
                .flat_map(|relocations| relocations.iter().copied()),
        )?;
        let relocation_addend_ranges = relocation_addend_ranges_for_input(
            input_file.data(),
            input.path.as_str(),
            input_relocations
                .into_iter()
                .flat_map(|relocations| relocations.iter().copied()),
        )?;
        let relocation_target_ranges = relocation_target_ranges_for_input(
            input_file.data(),
            input.path.as_str(),
            input_relocation_targets
                .into_iter()
                .flat_map(|relocations| relocations.iter().copied()),
        )?;
        let fde_relocation_ranges = fde_patch_input_ranges_for_input(
            input_file.data(),
            input.path.as_str(),
            input_fdes
                .into_iter()
                .flat_map(|records| records.iter().copied()),
        )?;
        let patch = patch_fingerprint_with_extra_ranges(
            input_file.data(),
            input.path.as_str(),
            patch_sections.iter().cloned(),
            dynamic_relocation_patches
                .iter()
                .filter_map(|patch| patch.input_range.clone())
                .chain(relocation_addend_ranges)
                .chain(relocation_target_ranges)
                .chain(fde_relocation_ranges),
        )?
        .map(|fingerprint| FilePatchState {
            fingerprint,
            sections: patch_sections
                .iter()
                .map(|section| FilePatchSectionState {
                    input: section.input.clone(),
                    section_index: section.section_index,
                    section_name: section.section_name.clone(),
                    input_size: section.input_size,
                    output_offset: section.output_offset,
                    output_size: section.output_size,
                    data_hash: section.data_hash.clone(),
                })
                .collect(),
            raw_sections: None,
        });
        if patch.is_some() && input.content.hash.is_empty() {
            input.content.hash = hash_bytes(input_file.data());
        }
        input.patch = patch;
    }

    Ok(())
}

fn patch_state_matches_section_records(
    patch: &FilePatchState,
    sections: &[&SectionRecord],
) -> bool {
    if patch.sections.is_empty() || patch.sections.len() != sections.len() {
        return false;
    }

    let mut patch_sections = patch
        .sections
        .iter()
        .map(|section| {
            (
                section.input.as_str(),
                section.section_index,
                section.output_offset,
                section.output_size,
            )
        })
        .collect::<Vec<_>>();
    patch_sections.sort();

    let mut section_records = sections
        .iter()
        .map(|section| {
            (
                section.input.as_str(),
                section.section_index,
                section.output_offset,
                section.size,
            )
        })
        .collect::<Vec<_>>();
    section_records.sort();

    patch_sections == section_records
}

fn direct_copy_patch_sections<'a>(
    bytes: &[u8],
    input_file_path: &str,
    output: &[u8],
    sections: &[&'a SectionRecord],
    dynamic_relocations: impl IntoIterator<Item = &'a DynamicRelocationRecord>,
) -> Result<Vec<PatchSection>> {
    let mut patch_sections = Vec::new();
    let dynamic_relocation_offsets =
        dynamic_relocation_offsets_by_input_section(dynamic_relocations);

    let mut sections_by_input = HashMap::<&str, Vec<&SectionRecord>>::new();
    for record in sections {
        sections_by_input
            .entry(record.input.as_str())
            .or_default()
            .push(record);
    }

    for (input_ref, records) in sections_by_input {
        let Some(input_bytes) = patch_input_bytes(bytes, input_file_path, input_ref)? else {
            continue;
        };
        let file = object::File::parse(input_bytes.bytes)
            .context("Failed to parse incremental patch candidate input")?;
        for record in records {
            let section = file
                .section_by_index(patch_section_object_index(&file, record.section_index)?)
                .context("Missing incremental patch candidate section")?;
            let data = section
                .data()
                .context("Failed to read incremental patch candidate section data")?;
            let dynamic_relocations =
                dynamic_relocation_offsets.get(&(input_ref, record.section_index));
            let Some(preserve_ranges) =
                section_direct_patch_preserve_ranges(&section, data.len(), dynamic_relocations)
            else {
                continue;
            };
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
            if patchable_bytes_match(data_out, data, &preserve_ranges)
                && padding.iter().all(|byte| *byte == 0)
            {
                patch_sections.push(PatchSection {
                    input: record.input.to_string(),
                    section_index: record.section_index,
                    section_name: patch_section_name_for_matching(&section),
                    input_size: data.len() as u64,
                    output_offset: record.output_offset,
                    output_size: record.size,
                    data_hash: Some(hash_bytes(data)),
                });
            }
        }
    }
    Ok(patch_sections)
}

fn dynamic_relocation_offsets_by_input_section<'a>(
    dynamic_relocations: impl IntoIterator<Item = &'a DynamicRelocationRecord>,
) -> HashMap<(&'a str, u32), HashSet<u64>> {
    let mut offsets = HashMap::<(&str, u32), HashSet<u64>>::new();
    for relocation in dynamic_relocations {
        offsets
            .entry((relocation.input.as_str(), relocation.section_index))
            .or_default()
            .insert(relocation.relocation_offset);
    }
    offsets
}

fn section_flags_allow_patching(flags: object::SectionFlags) -> bool {
    let object::SectionFlags::Elf { sh_flags } = flags else {
        return false;
    };
    // Sections that sld actually merges are written by the merge-strings path, so they don't
    // produce direct-copy patch records. Merge-flagged sections that reach this point were copied
    // directly, for example under --no-string-merge.
    sh_flags & u64::from(object::elf::SHF_ALLOC) != 0
}

pub(crate) fn section_name_allows_direct_patching(name: &[u8]) -> bool {
    !matches!(name, b".init" | b".fini")
        && !name.starts_with(b".eh_frame")
        && !name.starts_with(b".init_array")
        && !name.starts_with(b".fini_array")
        && !name.starts_with(b".preinit_array")
        && !name.starts_with(b".ctors")
        && !name.starts_with(b".dtors")
}

pub(crate) fn section_name_allows_incremental_padding(name: &[u8]) -> bool {
    name.starts_with(b".") && section_name_allows_direct_patching(name)
}

fn section_direct_patch_preserve_ranges<'data>(
    section: &impl object::ObjectSection<'data>,
    section_data_len: usize,
    dynamic_relocation_offsets: Option<&HashSet<u64>>,
) -> Option<Vec<std::ops::Range<usize>>> {
    if !section_flags_allow_patching(section.flags())
        || !section
            .name()
            .ok()
            .is_none_or(|name| section_name_allows_direct_patching(name.as_bytes()))
    {
        return None;
    }

    relocation_preserve_ranges(section, section_data_len, dynamic_relocation_offsets)
}

fn relocation_preserve_ranges<'data>(
    section: &impl object::ObjectSection<'data>,
    section_data_len: usize,
    dynamic_relocation_offsets: Option<&HashSet<u64>>,
) -> Option<Vec<std::ops::Range<usize>>> {
    let mut ranges = Vec::<std::ops::Range<usize>>::new();
    for (offset, relocation) in section.relocations() {
        if relocation.kind() == object::RelocationKind::None {
            continue;
        }
        let is_recorded_dynamic_relocation =
            dynamic_relocation_offsets.is_some_and(|offsets| offsets.contains(&offset));
        let generic_explicit_relocation = !relocation.has_implicit_addend()
            && relocation.encoding() == object::RelocationEncoding::Generic
            && relocation.size() != 0
            && relocation.size() % 8 == 0;
        if !generic_explicit_relocation
            || (!is_recorded_dynamic_relocation
                && relocation.kind() != object::RelocationKind::Absolute)
        {
            return None;
        }
        let start = usize::try_from(offset).ok()?;
        let len = usize::from(relocation.size() / 8);
        let end = start.checked_add(len)?;
        if end > section_data_len {
            return None;
        }
        ranges.push(start..end);
    }
    ranges.sort_by_key(|range| range.start);
    let mut previous_end = 0;
    for range in &ranges {
        if range.start < previous_end {
            return None;
        }
        previous_end = range.end;
    }
    Some(ranges)
}

fn patchable_bytes_match(
    output: &[u8],
    input: &[u8],
    preserve_ranges: &[std::ops::Range<usize>],
) -> bool {
    if output.len() != input.len() {
        return false;
    }
    let mut position = 0;
    for range in preserve_ranges {
        if output[position..range.start] != input[position..range.start] {
            return false;
        }
        position = range.end;
    }
    output[position..] == input[position..]
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

fn patch_input_bytes<'data>(
    bytes: &'data [u8],
    input_file_path: &str,
    input_ref: &str,
) -> Result<Option<PatchInputBytes<'data>>> {
    let Some(parsed) = parse_patch_input_ref(input_file_path, input_ref)? else {
        return Ok(Some(PatchInputBytes {
            bytes,
            file_offset: 0,
        }));
    };
    if parsed.range.is_empty() {
        return Ok(None);
    }

    match patch_archive_member_bytes(bytes, &parsed.identifier)? {
        ArchiveMemberMatch::Unique(member) => return Ok(Some(member)),
        ArchiveMemberMatch::Ambiguous => return Ok(None),
        ArchiveMemberMatch::Unavailable => {}
    }

    let Some(input_bytes) = bytes.get(parsed.range.clone()) else {
        return Ok(None);
    };
    Ok(Some(PatchInputBytes {
        bytes: input_bytes,
        file_offset: parsed.range.start,
    }))
}

#[cfg(test)]
fn patch_input_range(
    input_file_path: &str,
    input_ref: &str,
) -> Result<Option<std::ops::Range<usize>>> {
    Ok(parse_patch_input_ref(input_file_path, input_ref)?.map(|input_ref| input_ref.range))
}

fn parse_patch_input_ref(
    input_file_path: &str,
    input_ref: &str,
) -> Result<Option<ParsedPatchInputRef>> {
    let input_file_path_bytes =
        hex::decode(input_file_path).context("Malformed incremental input file path")?;
    let input_ref_bytes = hex::decode(input_ref).context("Malformed incremental input ref")?;
    if input_ref_bytes == input_file_path_bytes {
        return Ok(None);
    }

    let Some(rest) = input_ref_bytes
        .strip_prefix(input_file_path_bytes.as_slice())
        .and_then(|rest| rest.strip_prefix(&[0]))
    else {
        return Ok(Some(ParsedPatchInputRef {
            identifier: Vec::new(),
            range: 0..0,
        }));
    };
    let Some(separator) = rest.iter().position(|byte| *byte == 0) else {
        return Ok(Some(ParsedPatchInputRef {
            identifier: Vec::new(),
            range: 0..0,
        }));
    };
    let identifier = rest[..separator].to_vec();
    let range_bytes = &rest[separator + 1..];
    let range =
        std::str::from_utf8(range_bytes).context("Malformed incremental archive member range")?;
    let Some((start, end)) = range.split_once(':') else {
        return Ok(Some(ParsedPatchInputRef {
            identifier: Vec::new(),
            range: 0..0,
        }));
    };
    let start = start
        .parse()
        .context("Invalid incremental archive member start offset")?;
    let end = end
        .parse()
        .context("Invalid incremental archive member end offset")?;
    if start > end {
        return Ok(Some(ParsedPatchInputRef {
            identifier: Vec::new(),
            range: 0..0,
        }));
    }
    Ok(Some(ParsedPatchInputRef {
        identifier,
        range: start..end,
    }))
}

fn patch_archive_member_bytes<'data>(
    bytes: &'data [u8],
    identifier: &[u8],
) -> Result<ArchiveMemberMatch<'data>> {
    if identifier.is_empty() {
        return Ok(ArchiveMemberMatch::Unavailable);
    }
    let Ok(archive) = ArchiveIterator::from_archive_bytes(bytes) else {
        return Ok(ArchiveMemberMatch::Unavailable);
    };
    let mut matched = None;
    for entry in archive {
        match entry? {
            ArchiveEntry::Regular(content) if content.ident.as_slice() == identifier => {
                let member = PatchInputBytes {
                    bytes: content.entry_data,
                    file_offset: content.data_offset,
                };
                if matched.replace(member).is_some() {
                    return Ok(ArchiveMemberMatch::Ambiguous);
                }
            }
            ArchiveEntry::Regular(_) | ArchiveEntry::Thin(_) => {}
        }
    }
    Ok(matched.map_or(ArchiveMemberMatch::Unavailable, ArchiveMemberMatch::Unique))
}

fn archive_members_match_snapshot(
    state_dir: &Path,
    previous_input: &FileState,
    current_bytes: &[u8],
) -> Result<bool> {
    let current_members = archive_member_identifiers(current_bytes)?;
    if current_members.is_none() && !patch_state_references_archive_member(previous_input) {
        return Ok(true);
    }
    let Some(previous_bytes) = read_verified_input_snapshot(state_dir, previous_input)? else {
        return Ok(false);
    };
    Ok(archive_member_identifiers(&previous_bytes)? == current_members)
}

fn patch_state_references_archive_member(previous_input: &FileState) -> bool {
    previous_input.patch.as_ref().is_some_and(|patch| {
        patch
            .sections
            .iter()
            .any(|section| section.input != previous_input.path)
    })
}

fn archive_member_identifiers(bytes: &[u8]) -> Result<Option<Vec<Vec<u8>>>> {
    let Ok(archive) = ArchiveIterator::from_archive_bytes(bytes) else {
        return Ok(None);
    };
    let mut identifiers = Vec::new();
    for entry in archive {
        match entry? {
            ArchiveEntry::Regular(content) => identifiers.push(content.ident.as_slice().to_vec()),
            ArchiveEntry::Thin(entry) => identifiers.push(entry.ident.as_slice().to_vec()),
        }
    }
    Ok(Some(identifiers))
}

#[cfg(test)]
fn patch_fingerprint(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<String>> {
    patch_fingerprint_with_extra_ranges(bytes, input_file_path, sections, std::iter::empty())
}

fn patch_fingerprint_with_extra_ranges(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
    extra_ranges: impl IntoIterator<Item = std::ops::Range<usize>>,
) -> Result<Option<String>> {
    let Some(mut ranges) = patch_ranges(bytes, input_file_path, sections)? else {
        return Ok(None);
    };
    ranges.extend(extra_ranges);
    dedup_ranges(&mut ranges);
    let Some(ranges) = normalize_patch_ranges(ranges, bytes.len()) else {
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

fn patch_fingerprint_matches_previous_without_extra_ranges(
    previous_bytes: &[u8],
    current_fingerprint: &str,
    input_file_path: &str,
    matched_sections: &[MatchedPatchSection],
) -> Result<bool> {
    Ok(patch_fingerprint_with_extra_ranges(
        previous_bytes,
        input_file_path,
        matched_sections
            .iter()
            .map(|section| section.previous.clone()),
        std::iter::empty(),
    )?
    .as_deref()
        == Some(current_fingerprint))
}

fn normalize_patch_ranges(
    mut ranges: Vec<std::ops::Range<usize>>,
    bytes_len: usize,
) -> Option<Vec<std::ops::Range<usize>>> {
    ranges.sort_by_key(|range| range.start);
    let mut previous_end = 0;
    for range in &ranges {
        if range.start > range.end || range.end > bytes_len || range.start < previous_end {
            return None;
        }
        previous_end = range.end;
    }

    (!ranges.is_empty()).then_some(ranges)
}

fn match_patch_sections_from_current_hashes(
    current_bytes: &[u8],
    input_file_path: &str,
    sections: &[PatchSection],
) -> Result<Option<MatchedPatchSections>> {
    if sections.is_empty()
        || sections
            .iter()
            .any(|section| section.section_name.is_none() || section.data_hash.is_none())
    {
        return Ok(None);
    }

    let Some(current_sections) = resolve_current_patch_sections(
        current_bytes,
        input_file_path,
        sections.iter().cloned(),
        std::iter::empty(),
    )?
    else {
        return Ok(None);
    };

    let mut matched_sections = Vec::with_capacity(sections.len());
    let mut changed_sections = Vec::new();
    for (previous, current) in sections.iter().cloned().zip(current_sections) {
        if previous.data_hash != current.data_hash {
            changed_sections.push(current.clone());
        }
        matched_sections.push(MatchedPatchSection { previous, current });
    }

    Ok(Some(MatchedPatchSections {
        sections: matched_sections,
        changed_sections,
    }))
}

fn match_patch_sections(
    state_dir: &Path,
    previous_input: &FileState,
    current_bytes: &[u8],
    sections: &[PatchSection],
) -> Result<Option<MatchedPatchSections>> {
    let Some(previous_bytes) = read_verified_input_snapshot(state_dir, previous_input)? else {
        return Ok(None);
    };

    let mut sections_by_input = HashMap::<&str, Vec<(usize, &PatchSection)>>::new();
    for (section_index, section) in sections.iter().enumerate() {
        sections_by_input
            .entry(section.input.as_str())
            .or_default()
            .push((section_index, section));
    }

    let mut matched_sections = vec![None; sections.len()];
    let mut changed_sections = Vec::new();
    for (input_ref, sections) in sections_by_input {
        let Some(previous_input_bytes) =
            patch_input_bytes(&previous_bytes, previous_input.path.as_str(), input_ref)?
        else {
            return Ok(None);
        };
        let Some(current_input_bytes) =
            patch_input_bytes(current_bytes, previous_input.path.as_str(), input_ref)?
        else {
            return Ok(None);
        };
        let previous_file = object::File::parse(previous_input_bytes.bytes)
            .context("Failed to parse previous patch input")?;
        let current_file = object::File::parse(current_input_bytes.bytes)
            .context("Failed to parse current patch input")?;
        let previous_references = section_reference_map(&previous_file)?;
        let current_references = section_reference_map(&current_file)?;

        for (matched_index, section) in sections {
            let Some(previous_index) = patch_section_index(&previous_file, section)? else {
                return Ok(None);
            };
            let Some(current_index) = match_current_patch_section_index(
                &current_file,
                section,
                previous_index,
                &previous_references,
                &current_references,
            )?
            else {
                return Ok(None);
            };

            let mut previous = section.clone();
            previous.section_index = previous_index.0 as u32;
            let mut current = section.clone();
            current.section_index = current_index.0 as u32;

            let previous_section = previous_file
                .section_by_index(previous_index)
                .context("Missing previous incremental patch section")?;
            let current_section = current_file
                .section_by_index(current_index)
                .context("Missing current incremental patch section")?;
            let previous_data = previous_section
                .data()
                .context("Failed to read previous incremental patch section data")?;
            let current_data = current_section
                .data()
                .context("Failed to read current incremental patch section data")?;
            previous.input_size = previous_data.len() as u64;
            current.input_size = current_data.len() as u64;
            current.data_hash = Some(hash_bytes(current_data));
            if previous_data != current_data {
                changed_sections.push(current.clone());
            }

            matched_sections[matched_index] = Some(MatchedPatchSection { previous, current });
        }
    }

    Ok(Some(MatchedPatchSections {
        sections: matched_sections
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .context("Missing matched incremental patch section")?,
        changed_sections,
    }))
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
    let Some(previous_bytes) = read_verified_input_snapshot(state_dir, previous_input)? else {
        return Ok(None);
    };

    let mut changed_sections = Vec::new();

    let mut sections_by_input = HashMap::<&str, Vec<&MatchedPatchSection>>::new();
    for section in sections {
        sections_by_input
            .entry(section.current.input.as_str())
            .or_default()
            .push(section);
    }

    for (input_ref, sections) in sections_by_input {
        let Some(previous_input_bytes) =
            patch_input_bytes(&previous_bytes, previous_input.path.as_str(), input_ref)?
        else {
            return Ok(None);
        };
        let Some(current_input_bytes) =
            patch_input_bytes(current_bytes, previous_input.path.as_str(), input_ref)?
        else {
            return Ok(None);
        };
        let previous_file = object::File::parse(previous_input_bytes.bytes)
            .context("Failed to parse previous patch input")?;
        let current_file = object::File::parse(current_input_bytes.bytes)
            .context("Failed to parse current patch input")?;

        for patch_section in sections {
            let Some(previous_index) =
                patch_section_index(&previous_file, &patch_section.previous)?
            else {
                return Ok(None);
            };
            let Some(current_index) = patch_section_index(&current_file, &patch_section.current)?
            else {
                return Ok(None);
            };
            let previous_section = previous_file
                .section_by_index(previous_index)
                .context("Missing previous incremental patch section")?;
            let current_section = current_file
                .section_by_index(current_index)
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
    }

    Ok(Some(changed_sections))
}

fn patch_section_index(
    file: &object::File<'_>,
    patch_section: &PatchSection,
) -> Result<Option<object::SectionIndex>> {
    let Some(name) = patch_section.section_name.as_deref() else {
        return patch_section_object_index(file, patch_section.section_index).map(Some);
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

#[cfg(test)]
fn patch_sections_for_input(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<Vec<SectionPatch>>> {
    Ok(
        resolved_patch_sections_for_input(bytes, input_file_path, sections)?
            .map(|patches| patches.into_iter().map(|resolved| resolved.patch).collect()),
    )
}

fn resolve_current_patch_sections<'a>(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
    dynamic_relocations: impl IntoIterator<Item = &'a DynamicRelocationRecord>,
) -> Result<Option<Vec<PatchSection>>> {
    Ok(resolved_patch_sections_for_input_with_dynamic_relocations(
        bytes,
        input_file_path,
        sections,
        dynamic_relocations,
    )?
    .map(|patches| {
        patches
            .into_iter()
            .map(|resolved| resolved.section)
            .collect()
    }))
}

#[cfg(test)]
fn resolved_patch_sections_for_input(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<Vec<ResolvedSectionPatch>>> {
    resolved_patch_sections_for_input_with_dynamic_relocations(
        bytes,
        input_file_path,
        sections,
        std::iter::empty(),
    )
}

fn resolved_patch_sections_for_input_with_dynamic_relocations<'a>(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
    dynamic_relocations: impl IntoIterator<Item = &'a DynamicRelocationRecord>,
) -> Result<Option<Vec<ResolvedSectionPatch>>> {
    let sections = sections.into_iter().collect::<Vec<_>>();
    let dynamic_relocation_offsets =
        dynamic_relocation_offsets_by_input_section(dynamic_relocations);
    let mut patches = std::iter::repeat_with(|| None)
        .take(sections.len())
        .collect::<Vec<_>>();
    let mut sections_by_input = HashMap::<&str, Vec<usize>>::new();
    for (section_index, section) in sections.iter().enumerate() {
        sections_by_input
            .entry(section.input.as_str())
            .or_default()
            .push(section_index);
    }

    for (input_ref, section_indices) in sections_by_input {
        let Some(input_bytes) = patch_input_bytes(bytes, input_file_path, input_ref)? else {
            return Ok(None);
        };
        let file = object::File::parse(input_bytes.bytes)
            .context("Failed to parse changed incremental input")?;
        for stored_section_index in section_indices {
            let patch_section = &sections[stored_section_index];
            let Some(section_index) = patch_section_index(&file, patch_section)? else {
                return Ok(None);
            };
            let section = file
                .section_by_index(section_index)
                .context("Missing changed incremental input section")?;
            let data = section
                .data()
                .context("Failed to read changed incremental input section data")?;
            let dynamic_relocations =
                dynamic_relocation_offsets.get(&(input_ref, patch_section.section_index));
            let Some(preserve_ranges) =
                section_direct_patch_preserve_ranges(&section, data.len(), dynamic_relocations)
            else {
                return Ok(None);
            };
            if data.len() > patch_section.output_size as usize {
                return Ok(None);
            }
            let mut resolved_section = patch_section.clone();
            resolved_section.section_index = section_index.0 as u32;
            resolved_section.input_size = data.len() as u64;
            resolved_section.data_hash = Some(hash_bytes(data));
            patches[stored_section_index] = Some(ResolvedSectionPatch {
                section: resolved_section,
                patch: SectionPatch {
                    output_offset: patch_section.output_offset,
                    size: patch_section.output_size,
                    data: data.to_owned(),
                    deferred_relocation: None,
                    preserve_ranges,
                    adjustments: Vec::new(),
                },
            });
        }
    }
    Ok(Some(
        patches
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .context("Missing resolved incremental patch section")?,
    ))
}

fn dynamic_relocation_patches_for_input<'a>(
    bytes: &[u8],
    input_file_path: &str,
    records: impl IntoIterator<Item = &'a DynamicRelocationRecord>,
) -> Result<Vec<DynamicRelocationPatch>> {
    let records = records.into_iter().collect::<Vec<_>>();
    let mut patches = Vec::new();
    let mut records_by_input = HashMap::<&str, Vec<&DynamicRelocationRecord>>::new();
    for record in records {
        records_by_input
            .entry(record.input.as_str())
            .or_default()
            .push(record);
    }

    for (input_ref, records) in records_by_input {
        let Some(input_bytes) = patch_input_bytes(bytes, input_file_path, input_ref)? else {
            continue;
        };
        patches.extend(dynamic_relocation_patches_for_input_bytes(
            input_bytes.bytes,
            input_bytes.file_offset,
            records,
        )?);
    }

    Ok(patches)
}

fn added_dynamic_relocation_patches_for_input(
    current_bytes: &[u8],
    previous_bytes: &[u8],
    input_file_path: &str,
    matched_sections: &[MatchedPatchSection],
    previous_dynamic_relocations: &[DynamicRelocationRecord],
    previous_sections: &[SectionRecord],
) -> Vec<DynamicRelocationPatch> {
    if matched_sections.iter().any(|section| {
        section.previous.input != input_file_path || section.current.input != input_file_path
    }) {
        return Vec::new();
    }

    let Some(current_section_headers) = elf_section_headers(current_bytes) else {
        return Vec::new();
    };
    let Some(previous_section_headers) = elf_section_headers(previous_bytes) else {
        return Vec::new();
    };
    let free_slots = free_dynamic_relocation_output_slots(
        previous_sections,
        previous_dynamic_relocations
            .iter()
            .filter(|record| record.size == crate::elf::RELA_ENTRY_SIZE),
    );
    if free_slots.is_empty() {
        return Vec::new();
    }

    let previous_records = previous_dynamic_relocations
        .iter()
        .filter(|record| {
            record.input_file == input_file_path
                && record.input == input_file_path
                && record.has_restorable_rela_output_info()
        })
        .collect::<Vec<_>>();
    if previous_records.is_empty() {
        return Vec::new();
    }

    let mut patches = Vec::new();
    let mut next_free_slot = 0;
    for matched in matched_sections {
        let previous_entries = rela_entries_for_section(
            previous_bytes,
            &previous_section_headers,
            matched.previous.section_index,
        )
        .unwrap_or_default();
        let current_entries = rela_entries_for_section(
            current_bytes,
            &current_section_headers,
            matched.current.section_index,
        )
        .unwrap_or_default();
        let previous_offsets = previous_entries
            .iter()
            .map(|entry| entry.offset)
            .collect::<HashSet<_>>();
        for entry in current_entries
            .iter()
            .filter(|entry| !previous_offsets.contains(&entry.offset))
        {
            let Some(reference) = previous_records.iter().find_map(|record| {
                (record.section_index == matched.previous.section_index).then(|| {
                    previous_entries
                        .iter()
                        .find(|previous| {
                            previous.offset == record.relocation_offset
                                && previous.info == entry.info
                        })
                        .map(|_| *record)
                })?
            }) else {
                continue;
            };
            let Some(output_offset) = free_slots.get(next_free_slot).copied() else {
                return patches;
            };
            next_free_slot += 1;

            let delta = i128::from(entry.offset) - i128::from(reference.relocation_offset);
            let Some(output_r_offset) =
                add_signed_delta_u64(reference.output_r_offset.unwrap(), delta)
            else {
                continue;
            };
            let output_r_info = reference.output_r_info.unwrap();
            let mut data = vec![0; crate::elf::RELA_ENTRY_SIZE as usize];
            data[0..8].copy_from_slice(&output_r_offset.to_le_bytes());
            data[8..16].copy_from_slice(&output_r_info.to_le_bytes());
            data[16..24].copy_from_slice(&entry.addend.to_le_bytes());
            patches.push(DynamicRelocationPatch {
                record: DynamicRelocationRecord {
                    input_file: input_file_path.to_owned().into(),
                    input: input_file_path.to_owned().into(),
                    section_index: matched.current.section_index,
                    relocation_offset: entry.offset,
                    output_offset,
                    size: crate::elf::RELA_ENTRY_SIZE,
                    output_r_offset: Some(output_r_offset),
                    output_r_info: Some(output_r_info),
                },
                input_range: Some(entry.addend_range.clone()),
                patch: SectionPatch {
                    output_offset,
                    size: crate::elf::RELA_ENTRY_SIZE,
                    data,
                    deferred_relocation: None,
                    preserve_ranges: Vec::new(),
                    adjustments: Vec::new(),
                },
                is_new: true,
            });
        }
    }

    patches
}

fn free_dynamic_relocation_output_slots<'a>(
    sections: &[SectionRecord],
    dynamic_relocations: impl IntoIterator<Item = &'a DynamicRelocationRecord>,
) -> Vec<u64> {
    let used = dynamic_relocations
        .into_iter()
        .map(|record| record.output_offset)
        .collect::<HashSet<_>>();
    let mut slots = Vec::new();
    for section in sections {
        if section.input_file != GENERATED_SECTION_INPUT_FILE
            || section.input != GENERATED_RELA_DYN_GENERAL
        {
            continue;
        }
        let mut output_offset = section.output_offset;
        let Some(end) = section.output_offset.checked_add(section.size) else {
            continue;
        };
        while output_offset
            .checked_add(crate::elf::RELA_ENTRY_SIZE)
            .is_some_and(|slot_end| slot_end <= end)
        {
            if !used.contains(&output_offset) {
                slots.push(output_offset);
            }
            output_offset += crate::elf::RELA_ENTRY_SIZE;
        }
    }
    slots.sort_unstable();
    slots
}

fn relocation_addend_patches_for_input(
    relocations: &mut [RelocationRecord],
    input: &FileState,
    bytes: &[u8],
    previous_bytes: Option<&[u8]>,
    dynamic_relocations: &[DynamicRelocationRecord],
) -> Result<std::result::Result<RelocationAddendPatches, String>> {
    let mut input_ranges = Vec::new();
    let mut output_patches = Vec::new();
    let dynamic_relocation_keys = dynamic_relocations
        .iter()
        .filter(|relocation| relocation.input_file == input.path)
        .map(|relocation| {
            (
                relocation.input.as_str(),
                relocation.section_index,
                relocation.relocation_offset,
            )
        })
        .collect::<HashSet<_>>();
    let mut relocations_by_input = HashMap::<String, Vec<usize>>::new();
    for (relocation_index, relocation) in relocations.iter().enumerate() {
        if relocation.input_file == input.path {
            relocations_by_input
                .entry(relocation.input.to_string())
                .or_default()
                .push(relocation_index);
        }
    }

    for (input_ref, relocation_indices) in relocations_by_input {
        let Some(input_bytes) = patch_input_bytes(bytes, input.path.as_str(), &input_ref)? else {
            continue;
        };
        let previous_input_bytes = if let Some(previous_bytes) = previous_bytes {
            patch_input_bytes(previous_bytes, input.path.as_str(), &input_ref)?
        } else {
            None
        };
        let Some(section_headers) = elf_section_headers(input_bytes.bytes) else {
            continue;
        };
        let previous_section_headers = previous_input_bytes
            .as_ref()
            .and_then(|input_bytes| elf_section_headers(input_bytes.bytes));
        let mut entries_by_section = HashMap::<u32, Vec<RelaPatchEntry>>::new();
        let mut previous_entries_by_section = HashMap::<u32, Vec<RelaPatchEntry>>::new();
        for relocation_index in relocation_indices {
            let section_index = relocations[relocation_index].section_index;
            let relocation_offset = relocations[relocation_index].relocation_offset;
            let entries = entries_by_section.entry(section_index).or_insert_with(|| {
                rela_entries_for_section(input_bytes.bytes, &section_headers, section_index)
                    .unwrap_or_default()
            });
            let Some(entry) = entries
                .iter()
                .find(|entry| entry.offset == relocation_offset)
            else {
                continue;
            };
            input_ranges.push(
                input_bytes.file_offset + entry.addend_range.start
                    ..input_bytes.file_offset + entry.addend_range.end,
            );
            let relocation = &mut relocations[relocation_index];
            if entry.addend == relocation.addend {
                continue;
            }
            let raw_addend_unchanged = previous_input_bytes
                .as_ref()
                .zip(previous_section_headers.as_ref())
                .is_some_and(|(previous_input_bytes, previous_section_headers)| {
                    let previous_entries = previous_entries_by_section
                        .entry(section_index)
                        .or_insert_with(|| {
                            rela_entries_for_section(
                                previous_input_bytes.bytes,
                                previous_section_headers,
                                section_index,
                            )
                            .unwrap_or_default()
                        });
                    previous_entries
                        .iter()
                        .find(|previous_entry| previous_entry.offset == relocation_offset)
                        .is_some_and(|previous_entry| previous_entry.addend == entry.addend)
                });
            if raw_addend_unchanged {
                continue;
            }
            if dynamic_relocation_keys.contains(&(
                input_ref.as_str(),
                section_index,
                relocation_offset,
            )) {
                relocation.addend = entry.addend;
                continue;
            }
            let Some(written_value) = relocation.written_value else {
                return Ok(Err(format!(
                    "missing written relocation value for addend change in {}",
                    display_hex_path(&input.path)
                )));
            };
            if relocation.size != 8 {
                return Ok(Err(format!(
                    "unsupported relocation addend patch size in {}",
                    display_hex_path(&input.path)
                )));
            }
            let delta = i128::from(entry.addend) - i128::from(relocation.addend);
            let Some(written_value) = add_signed_delta_u64(written_value, delta) else {
                return Ok(Err(format!(
                    "relocation addend patch overflowed in {}",
                    display_hex_path(&input.path)
                )));
            };
            output_patches.push(SectionPatch {
                output_offset: relocation.output_offset,
                size: relocation.size,
                data: written_value.to_le_bytes().to_vec(),
                deferred_relocation: None,
                preserve_ranges: Vec::new(),
                adjustments: Vec::new(),
            });
            relocation.written_value = Some(written_value);
            relocation.addend = entry.addend;
        }
    }
    dedup_ranges(&mut input_ranges);
    Ok(Ok(RelocationAddendPatches {
        input_ranges,
        output_patches,
    }))
}

fn relocation_addend_ranges_for_input<'a>(
    bytes: &[u8],
    input_file_path: &str,
    records: impl IntoIterator<Item = &'a RelocationRecord>,
) -> Result<Vec<std::ops::Range<usize>>> {
    let mut ranges = Vec::new();
    let mut records_by_input = HashMap::<&str, Vec<&RelocationRecord>>::new();
    for record in records {
        records_by_input
            .entry(record.input.as_str())
            .or_default()
            .push(record);
    }

    for (input_ref, records) in records_by_input {
        let Some(input_bytes) = patch_input_bytes(bytes, input_file_path, input_ref)? else {
            continue;
        };
        let Some(section_headers) = elf_section_headers(input_bytes.bytes) else {
            continue;
        };
        let mut entries_by_section = HashMap::<u32, Vec<RelaPatchEntry>>::new();
        for record in records {
            let entries = entries_by_section
                .entry(record.section_index)
                .or_insert_with(|| {
                    rela_entries_for_section(
                        input_bytes.bytes,
                        &section_headers,
                        record.section_index,
                    )
                    .unwrap_or_default()
                });
            let Some(entry) = entries
                .iter()
                .find(|entry| entry.offset == record.relocation_offset)
            else {
                continue;
            };
            ranges.push(
                input_bytes.file_offset + entry.addend_range.start
                    ..input_bytes.file_offset + entry.addend_range.end,
            );
        }
    }

    dedup_ranges(&mut ranges);
    Ok(ranges)
}

fn relocation_target_ranges_for_input<'a>(
    bytes: &[u8],
    input_file_path: &str,
    records: impl IntoIterator<Item = &'a RelocationRecord>,
) -> Result<Vec<std::ops::Range<usize>>> {
    let mut ranges = Vec::new();
    let mut records_by_input = HashMap::<&str, Vec<&RelocationRecord>>::new();
    for record in records {
        let Some(target) = &record.target else {
            continue;
        };
        records_by_input
            .entry(target.input.as_str())
            .or_default()
            .push(record);
    }

    for (input_ref, records) in records_by_input {
        let Some(input_bytes) = patch_input_bytes(bytes, input_file_path, input_ref)? else {
            continue;
        };
        let file = object::File::parse(input_bytes.bytes)
            .context("Failed to parse incremental relocation target input")?;
        let mut seen_names = HashSet::new();
        for record in records {
            let Some(target_name) = record.target_name.as_deref() else {
                continue;
            };
            if !seen_names.insert(target_name) {
                continue;
            }
            let Some(symbol) = symbol_position_by_name(
                input_bytes.bytes,
                input_bytes.file_offset,
                &file,
                target_name,
            )?
            else {
                continue;
            };
            if let Some(value_range) = symbol.value_range {
                ranges.push(value_range);
            }
        }
    }

    dedup_ranges(&mut ranges);
    Ok(ranges)
}

fn dynamic_relocation_patches_for_input_bytes(
    bytes: &[u8],
    file_offset: usize,
    records: Vec<&DynamicRelocationRecord>,
) -> Result<Vec<DynamicRelocationPatch>> {
    let Some(section_headers) = elf_section_headers(bytes) else {
        return Ok(Vec::new());
    };

    let mut patches = Vec::new();
    for record in records {
        if record.size != crate::elf::RELA_ENTRY_SIZE {
            continue;
        }
        let Some(entry_range) = dynamic_relocation_entry_range(bytes, &section_headers, record)?
        else {
            patches.push(DynamicRelocationPatch {
                record: record.clone(),
                input_range: None,
                patch: SectionPatch {
                    output_offset: record.output_offset,
                    size: record.size,
                    data: vec![0; record.size as usize],
                    deferred_relocation: None,
                    preserve_ranges: Vec::new(),
                    adjustments: Vec::new(),
                },
                is_new: false,
            });
            continue;
        };
        let addend_start = entry_range.start + 16;
        let addend_end = addend_start + 8;
        let Some(addend) = bytes.get(addend_start..addend_end) else {
            continue;
        };
        let mut data = vec![0; record.size as usize];
        let preserve_ranges = if let (Some(output_r_offset), Some(output_r_info)) =
            (record.output_r_offset, record.output_r_info)
        {
            data[0..8].copy_from_slice(&output_r_offset.to_le_bytes());
            data[8..16].copy_from_slice(&output_r_info.to_le_bytes());
            Vec::new()
        } else {
            std::iter::once(0..16).collect()
        };
        data[16..24].copy_from_slice(addend);
        patches.push(DynamicRelocationPatch {
            record: record.clone(),
            input_range: Some(file_offset + addend_start..file_offset + addend_end),
            patch: SectionPatch {
                output_offset: record.output_offset,
                size: record.size,
                data,
                deferred_relocation: None,
                preserve_ranges,
                adjustments: Vec::new(),
            },
            is_new: false,
        });
    }
    Ok(patches)
}

fn object_diff_allows_dynamic_relocation_removal(
    previous_bytes: &[u8],
    current_bytes: &[u8],
    input_file_path: &str,
    matched_sections: &[MatchedPatchSection],
    dynamic_relocation_patches: &[DynamicRelocationPatch],
) -> Result<bool> {
    if dynamic_relocation_patches.is_empty()
        || dynamic_relocation_patches
            .iter()
            .all(|patch| patch.input_range.is_some())
        || dynamic_relocation_patches.iter().any(|patch| {
            patch.record.input_file != input_file_path || patch.record.input != input_file_path
        })
        || matched_sections.iter().any(|section| {
            section.previous.input != input_file_path
                || section.current.input != input_file_path
                || section.previous.section_index != section.current.section_index
        })
    {
        return Ok(false);
    }

    let Some(current_section_headers) = elf_section_headers(current_bytes) else {
        return Ok(false);
    };
    let mut kept_offsets_by_section = HashMap::<u32, HashSet<u64>>::new();
    let mut section_indices = HashSet::new();
    for patch in dynamic_relocation_patches {
        section_indices.insert(patch.record.section_index);
        if patch.input_range.is_some() {
            kept_offsets_by_section
                .entry(patch.record.section_index)
                .or_default()
                .insert(patch.record.relocation_offset);
        }
    }
    for section_index in section_indices {
        let kept_offsets = kept_offsets_by_section
            .get(&section_index)
            .cloned()
            .unwrap_or_default();
        let current_entries =
            rela_entries_for_section(current_bytes, &current_section_headers, section_index)
                .unwrap_or_default();
        if current_entries
            .iter()
            .any(|entry| !kept_offsets.contains(&entry.offset))
        {
            return Ok(false);
        }
    }

    let previous_file = object::File::parse(previous_bytes)
        .context("Failed to parse previous dynamic relocation removal input")?;
    let current_file = object::File::parse(current_bytes)
        .context("Failed to parse current dynamic relocation removal input")?;
    let previous_patch_indices = matched_sections
        .iter()
        .map(|section| object::SectionIndex(section.previous.section_index as usize))
        .collect::<HashSet<_>>();
    let current_patch_indices = matched_sections
        .iter()
        .map(|section| object::SectionIndex(section.current.section_index as usize))
        .collect::<HashSet<_>>();

    let mut previous_sections_by_name = HashMap::<String, Vec<Vec<u8>>>::new();
    for section in previous_file.sections() {
        if previous_patch_indices.contains(&section.index()) {
            continue;
        }
        let name = section.name().unwrap_or_default();
        if section_name_is_metadata_for_dynamic_relocation_removal(name) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read previous dynamic relocation removal section data")?;
        previous_sections_by_name
            .entry(name.to_owned())
            .or_default()
            .push(data.to_vec());
    }

    let mut current_sections_by_name = HashMap::<String, Vec<Vec<u8>>>::new();
    for section in current_file.sections() {
        if current_patch_indices.contains(&section.index()) {
            continue;
        }
        let name = section.name().unwrap_or_default();
        if section_name_is_metadata_for_dynamic_relocation_removal(name) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read current dynamic relocation removal section data")?;
        current_sections_by_name
            .entry(name.to_owned())
            .or_default()
            .push(data.to_vec());
    }

    Ok(previous_sections_by_name == current_sections_by_name)
}

fn object_diff_allows_dynamic_relocation_addition(
    previous_bytes: &[u8],
    current_bytes: &[u8],
    input_file_path: &str,
    matched_sections: &[MatchedPatchSection],
    dynamic_relocation_patches: &[DynamicRelocationPatch],
) -> Result<bool> {
    if dynamic_relocation_patches.is_empty()
        || dynamic_relocation_patches
            .iter()
            .all(|patch| patch.input_range.is_none())
        || dynamic_relocation_patches
            .iter()
            .any(|patch| patch.input_range.is_none())
        || dynamic_relocation_patches.iter().any(|patch| {
            patch.record.input_file != input_file_path
                || patch.record.input != input_file_path
                || !patch.record.has_restorable_rela_output_info()
        })
        || matched_sections.iter().any(|section| {
            section.previous.input != input_file_path || section.current.input != input_file_path
        })
    {
        return Ok(false);
    }

    let Some(previous_section_headers) = elf_section_headers(previous_bytes) else {
        return Ok(false);
    };
    let Some(current_section_headers) = elf_section_headers(current_bytes) else {
        return Ok(false);
    };
    let mut any_added = false;
    let section_indices = dynamic_relocation_patches
        .iter()
        .map(|patch| patch.record.section_index)
        .collect::<HashSet<_>>();
    for section_index in section_indices {
        let previous_entries =
            rela_entries_for_section(previous_bytes, &previous_section_headers, section_index)
                .unwrap_or_default();
        let current_entries =
            rela_entries_for_section(current_bytes, &current_section_headers, section_index)
                .unwrap_or_default();
        let previous_offsets = previous_entries
            .iter()
            .map(|entry| entry.offset)
            .collect::<HashSet<_>>();
        let current_offsets = current_entries
            .iter()
            .map(|entry| entry.offset)
            .collect::<HashSet<_>>();
        if !previous_offsets.is_subset(&current_offsets) {
            return Ok(false);
        }
        let added_offsets = dynamic_relocation_patches
            .iter()
            .filter(|patch| {
                patch.record.section_index == section_index
                    && !previous_offsets.contains(&patch.record.relocation_offset)
            })
            .map(|patch| patch.record.relocation_offset)
            .collect::<HashSet<_>>();
        any_added |= !added_offsets.is_empty();
        if current_offsets
            .difference(&previous_offsets)
            .any(|offset| !added_offsets.contains(offset))
        {
            return Ok(false);
        }
    }
    if !any_added {
        return Ok(false);
    }

    let previous_file = object::File::parse(previous_bytes)
        .context("Failed to parse previous dynamic relocation addition input")?;
    let current_file = object::File::parse(current_bytes)
        .context("Failed to parse current dynamic relocation addition input")?;
    let previous_patch_indices = matched_sections
        .iter()
        .map(|section| object::SectionIndex(section.previous.section_index as usize))
        .collect::<HashSet<_>>();
    let current_patch_indices = matched_sections
        .iter()
        .map(|section| object::SectionIndex(section.current.section_index as usize))
        .collect::<HashSet<_>>();

    let mut previous_sections_by_name = HashMap::<String, Vec<Vec<u8>>>::new();
    for section in previous_file.sections() {
        if previous_patch_indices.contains(&section.index()) {
            continue;
        }
        let name = section.name().unwrap_or_default();
        if section_name_is_metadata_for_dynamic_relocation_removal(name) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read previous dynamic relocation addition section data")?;
        previous_sections_by_name
            .entry(name.to_owned())
            .or_default()
            .push(data.to_vec());
    }

    let mut current_sections_by_name = HashMap::<String, Vec<Vec<u8>>>::new();
    for section in current_file.sections() {
        if current_patch_indices.contains(&section.index()) {
            continue;
        }
        let name = section.name().unwrap_or_default();
        if section_name_is_metadata_for_dynamic_relocation_removal(name) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read current dynamic relocation addition section data")?;
        current_sections_by_name
            .entry(name.to_owned())
            .or_default()
            .push(data.to_vec());
    }

    Ok(previous_sections_by_name == current_sections_by_name)
}

fn object_diff_allows_fde_removal(
    previous_bytes: &[u8],
    current_bytes: &[u8],
    input_file_path: &str,
    matched_sections: &[MatchedPatchSection],
    eh_frame_patches: &[FdeRelocationPatch],
) -> Result<bool> {
    let removed_fdes = eh_frame_patches
        .iter()
        .filter_map(|patch| match &patch.eh_frame_hdr_change {
            Some(EhFrameHdrChange::Remove(fde)) => Some(fde),
            Some(EhFrameHdrChange::Adjust(_)) | Some(EhFrameHdrChange::Add(_)) => None,
            None => None,
        })
        .collect::<Vec<_>>();
    if removed_fdes.is_empty()
        || eh_frame_patches
            .iter()
            .any(|patch| matches!(patch.eh_frame_hdr_change, Some(EhFrameHdrChange::Add(_))))
        || removed_fdes
            .iter()
            .any(|fde| fde.input_file != input_file_path || fde.input != input_file_path)
        || matched_sections.iter().any(|section| {
            section.previous.input != input_file_path || section.current.input != input_file_path
        })
    {
        return Ok(false);
    }

    let previous_file = object::File::parse(previous_bytes)
        .context("Failed to parse previous FDE removal input")?;
    let current_file =
        object::File::parse(current_bytes).context("Failed to parse current FDE removal input")?;
    let previous_patch_indices = matched_sections
        .iter()
        .map(|section| object::SectionIndex(section.previous.section_index as usize))
        .collect::<HashSet<_>>();
    let current_patch_indices = matched_sections
        .iter()
        .map(|section| object::SectionIndex(section.current.section_index as usize))
        .collect::<HashSet<_>>();
    let previous_removed_target_indices = removed_fdes
        .iter()
        .map(|fde| object::SectionIndex(fde.section_index as usize))
        .collect::<HashSet<_>>();

    let mut previous_sections_by_name = HashMap::<String, Vec<Vec<u8>>>::new();
    for section in previous_file.sections() {
        if previous_patch_indices.contains(&section.index())
            || previous_removed_target_indices.contains(&section.index())
        {
            continue;
        }
        let name = section.name().unwrap_or_default();
        if section_name_is_metadata_for_fde_removal(name) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read previous FDE removal section data")?;
        previous_sections_by_name
            .entry(name.to_owned())
            .or_default()
            .push(data.to_vec());
    }

    let mut current_sections_by_name = HashMap::<String, Vec<Vec<u8>>>::new();
    for section in current_file.sections() {
        if current_patch_indices.contains(&section.index()) {
            continue;
        }
        let name = section.name().unwrap_or_default();
        if section_name_is_metadata_for_fde_removal(name) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read current FDE removal section data")?;
        current_sections_by_name
            .entry(name.to_owned())
            .or_default()
            .push(data.to_vec());
    }

    Ok(previous_sections_by_name == current_sections_by_name)
}

fn object_diff_allows_fde_addition(
    previous_bytes: &[u8],
    current_bytes: &[u8],
    input_file_path: &str,
    matched_sections: &[MatchedPatchSection],
    added_fdes: &[FdeAddCandidate],
) -> Result<bool> {
    if added_fdes.is_empty()
        || added_fdes
            .iter()
            .any(|fde| fde.input_file != input_file_path || fde.input != input_file_path)
        || matched_sections.iter().any(|section| {
            section.previous.input != input_file_path || section.current.input != input_file_path
        })
    {
        return Ok(false);
    }

    let previous_file = object::File::parse(previous_bytes)
        .context("Failed to parse previous FDE addition input")?;
    let current_file =
        object::File::parse(current_bytes).context("Failed to parse current FDE addition input")?;
    let previous_patch_indices = matched_sections
        .iter()
        .map(|section| object::SectionIndex(section.previous.section_index as usize))
        .collect::<HashSet<_>>();
    let current_patch_indices = matched_sections
        .iter()
        .map(|section| object::SectionIndex(section.current.section_index as usize))
        .collect::<HashSet<_>>();

    let mut previous_sections_by_name = HashMap::<String, Vec<Vec<u8>>>::new();
    for section in previous_file.sections() {
        if previous_patch_indices.contains(&section.index()) {
            continue;
        }
        let name = section.name().unwrap_or_default();
        if section_name_is_metadata_for_fde_removal(name) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read previous FDE addition section data")?;
        previous_sections_by_name
            .entry(name.to_owned())
            .or_default()
            .push(data.to_vec());
    }

    let mut current_sections_by_name = HashMap::<String, Vec<Vec<u8>>>::new();
    for section in current_file.sections() {
        if current_patch_indices.contains(&section.index()) {
            continue;
        }
        let name = section.name().unwrap_or_default();
        if section_name_is_metadata_for_fde_removal(name) {
            continue;
        }
        let data = section
            .data()
            .context("Failed to read current FDE addition section data")?;
        current_sections_by_name
            .entry(name.to_owned())
            .or_default()
            .push(data.to_vec());
    }

    Ok(previous_sections_by_name == current_sections_by_name)
}

fn section_name_is_metadata_for_dynamic_relocation_removal(name: &str) -> bool {
    name.is_empty()
        || name.starts_with(".rela")
        || name.starts_with(".rel")
        || matches!(
            name,
            ".symtab" | ".strtab" | ".shstrtab" | ".comment" | ".note.GNU-stack" | ".llvm_addrsig"
        )
}

fn section_name_is_metadata_for_fde_removal(name: &str) -> bool {
    name.is_empty()
        || name.starts_with(".rela")
        || name.starts_with(".rel")
        || name == ".eh_frame"
        || matches!(
            name,
            ".symtab" | ".strtab" | ".shstrtab" | ".comment" | ".note.GNU-stack" | ".llvm_addrsig"
        )
}

fn fde_patch_input_ranges_for_input<'a>(
    bytes: &[u8],
    input_file_path: &str,
    records: impl IntoIterator<Item = &'a FdeRecord>,
) -> Result<Vec<std::ops::Range<usize>>> {
    let mut ranges = Vec::new();
    let mut records_by_input = HashMap::<&str, Vec<&FdeRecord>>::new();
    for record in records {
        records_by_input
            .entry(record.input.as_str())
            .or_default()
            .push(record);
    }

    for (input_ref, records) in records_by_input {
        let Some(input_bytes) = patch_input_bytes(bytes, input_file_path, input_ref)? else {
            continue;
        };
        ranges.extend(fde_patch_input_ranges_for_input_bytes(
            input_bytes.bytes,
            input_bytes.file_offset,
            records,
        )?);
    }
    Ok(ranges)
}

fn fde_patch_input_ranges_for_input_bytes(
    bytes: &[u8],
    file_offset: usize,
    records: Vec<&FdeRecord>,
) -> Result<Vec<std::ops::Range<usize>>> {
    let Some(section_headers) = elf_section_headers(bytes) else {
        return Ok(Vec::new());
    };
    let file = object::File::parse(bytes).context("Failed to parse .eh_frame relocation input")?;
    let mut ranges = Vec::new();
    for record in records {
        let relocation_sizes =
            eh_frame_relocation_sizes(&file, record.eh_frame_section_index, record)?;
        let Some(fde_range) = fde_input_range(bytes.len(), &section_headers, record) else {
            continue;
        };
        let Some(entries) =
            rela_entries_for_section(bytes, &section_headers, record.eh_frame_section_index)
        else {
            continue;
        };
        let mut record_ranges = Vec::with_capacity(1);
        record_ranges.push(file_offset + fde_range.start..file_offset + fde_range.end);
        let mut supported = true;
        for entry in entries
            .into_iter()
            .filter(|entry| fde_contains_relocation(record, entry.offset))
        {
            if !relocation_sizes.contains_key(&entry.offset) {
                supported = false;
                break;
            }
            record_ranges
                .push(file_offset + entry.addend_range.start..file_offset + entry.addend_range.end);
        }
        if supported {
            ranges.extend(record_ranges);
        }
    }
    Ok(ranges)
}

fn fde_relocation_patches_for_input<'a>(
    current_bytes: &[u8],
    previous_bytes: &[u8],
    input_file_path: &str,
    records: impl IntoIterator<Item = &'a FdeRecord>,
) -> Result<Vec<FdeRelocationPatch>> {
    let records = records.into_iter().collect::<Vec<_>>();
    let mut patches = Vec::new();
    let mut records_by_input = HashMap::<&str, Vec<&FdeRecord>>::new();
    for record in records {
        records_by_input
            .entry(record.input.as_str())
            .or_default()
            .push(record);
    }

    for (input_ref, records) in records_by_input {
        let Some(current_input_bytes) =
            patch_input_bytes(current_bytes, input_file_path, input_ref)?
        else {
            continue;
        };
        let Some(previous_input_bytes) =
            patch_input_bytes(previous_bytes, input_file_path, input_ref)?
        else {
            continue;
        };
        patches.extend(fde_relocation_patches_for_input_bytes(
            current_input_bytes.bytes,
            current_input_bytes.file_offset,
            previous_input_bytes.bytes,
            records,
        )?);
    }
    Ok(patches)
}

fn added_fde_candidates_for_input<'a>(
    current_bytes: &[u8],
    previous_bytes: &[u8],
    input_file_path: &str,
    matched_sections: &[MatchedPatchSection],
    records: impl IntoIterator<Item = &'a FdeRecord>,
) -> Result<Vec<FdeAddCandidate>> {
    let records = records.into_iter().collect::<Vec<_>>();
    let mut records_by_input = HashMap::<&str, Vec<&FdeRecord>>::new();
    for record in records {
        records_by_input
            .entry(record.input.as_str())
            .or_default()
            .push(record);
    }

    let mut candidates = Vec::new();
    for (input_ref, records) in records_by_input {
        let Some(current_input_bytes) =
            patch_input_bytes(current_bytes, input_file_path, input_ref)?
        else {
            continue;
        };
        let Some(previous_input_bytes) =
            patch_input_bytes(previous_bytes, input_file_path, input_ref)?
        else {
            continue;
        };
        candidates.extend(added_fde_candidates_for_input_bytes(
            current_input_bytes.bytes,
            current_input_bytes.file_offset,
            previous_input_bytes.bytes,
            input_file_path,
            input_ref,
            matched_sections,
            records,
        )?);
    }
    Ok(candidates)
}

fn added_fde_candidates_for_input_bytes(
    current_bytes: &[u8],
    current_file_offset: usize,
    previous_bytes: &[u8],
    input_file_path: &str,
    input_ref: &str,
    matched_sections: &[MatchedPatchSection],
    records: Vec<&FdeRecord>,
) -> Result<Vec<FdeAddCandidate>> {
    let Some(current_section_headers) = elf_section_headers(current_bytes) else {
        return Ok(Vec::new());
    };
    let current_file =
        object::File::parse(current_bytes).context("Failed to parse current FDE addition input")?;
    let previous_file = object::File::parse(previous_bytes)
        .context("Failed to parse previous FDE addition input")?;
    let current_eh_frame_section_index = current_file
        .section_by_name(".eh_frame")
        .map(|section| section.index().0 as u32);

    let mut matched_current_fdes = HashSet::<(u32, u64)>::new();
    let mut cie_reference_fde_output_offsets = HashMap::<u64, u64>::new();
    for record in records {
        let Some(current_record) = current_fde_record_for_previous_record(
            &previous_file,
            &current_file,
            record,
            current_eh_frame_section_index,
        )?
        else {
            continue;
        };
        let mut current_record = current_record;
        let current_input_offset = fde_input_range_for_target_section_at_offset(
            current_bytes,
            &current_section_headers,
            current_record.eh_frame_section_index,
            current_record.section_index,
            current_record.input_offset,
        )
        .or_else(|| {
            fde_input_range_for_target_section(
                current_bytes,
                &current_section_headers,
                current_record.eh_frame_section_index,
                current_record.section_index,
            )
        })
        .map_or(current_record.input_offset, |(_, input_offset)| {
            input_offset
        });
        current_record.input_offset = current_input_offset;
        matched_current_fdes.insert((current_record.section_index, current_input_offset));
        if let Some(current_fde_range) = fde_input_range(
            current_bytes.len(),
            &current_section_headers,
            &current_record,
        ) && let Some(cie_input_offset) = fde_cie_input_offset(
            &current_bytes[current_fde_range],
            current_record.input_offset,
        ) {
            cie_reference_fde_output_offsets
                .entry(cie_input_offset)
                .or_insert(record.output_offset);
        }
    }

    let target_output_offsets = matched_sections
        .iter()
        .filter(|section| section.current.input == input_ref)
        .map(|section| (section.current.section_index, section.current.output_offset))
        .collect::<HashMap<_, _>>();

    let mut candidates = Vec::new();
    for candidate in fde_candidates_for_input_bytes(current_bytes, current_file_offset)? {
        if matched_current_fdes.contains(&(candidate.target_section_index, candidate.input_offset))
        {
            continue;
        }
        let Some(target_output_offset) = target_output_offsets
            .get(&candidate.target_section_index)
            .copied()
        else {
            continue;
        };
        let Some(cie_reference_fde_output_offset) = cie_reference_fde_output_offsets
            .get(&candidate.cie_input_offset)
            .copied()
        else {
            continue;
        };
        candidates.push(FdeAddCandidate {
            target_output_offset,
            cie_reference_fde_output_offset,
            input_file: input_file_path.to_owned(),
            input: input_ref.to_owned(),
            ..candidate
        });
    }
    Ok(candidates)
}

fn fde_candidates_for_input_bytes(
    bytes: &[u8],
    file_offset: usize,
) -> Result<Vec<FdeAddCandidate>> {
    let Some(section_headers) = elf_section_headers(bytes) else {
        return Ok(Vec::new());
    };
    let file = object::File::parse(bytes).context("Failed to parse FDE addition input")?;
    let Some(eh_frame_section) = file.section_by_name(".eh_frame") else {
        return Ok(Vec::new());
    };
    let eh_frame_section_index = eh_frame_section.index().0 as u32;
    let data = eh_frame_section
        .data()
        .context("Failed to read FDE addition .eh_frame data")?;
    let Some(section_header) = section_headers.get(eh_frame_section_index as usize) else {
        return Ok(Vec::new());
    };
    let section_start = usize::try_from(section_header.sh_offset)
        .context("FDE addition .eh_frame offset is too large")?;
    let relas = rela_entries_for_section(bytes, &section_headers, eh_frame_section_index)
        .unwrap_or_default();

    let mut candidates = Vec::new();
    let mut offset = 0usize;
    while offset + 8 <= data.len() {
        let length = usize::try_from(
            read_u32_le(&data[offset..offset + 4])
                .context("FDE addition .eh_frame entry length is truncated")?,
        )
        .context("FDE addition .eh_frame entry length is too large")?;
        if length == 0 {
            break;
        }
        let Some(size) = length.checked_add(4) else {
            break;
        };
        let Some(next_offset) = offset.checked_add(size) else {
            break;
        };
        if next_offset > data.len() {
            break;
        }
        let entry = &data[offset..next_offset];
        let cie_id =
            read_u32_le(&entry[4..8]).context("FDE addition .eh_frame CIE pointer is truncated")?;
        if cie_id != 0 {
            let input_offset = offset as u64;
            let pc_begin_offset = input_offset + crate::elf::FDE_PC_BEGIN_OFFSET as u64;
            let relas_in_fde = relas
                .iter()
                .filter(|entry| {
                    entry.offset >= input_offset && entry.offset < input_offset + size as u64
                })
                .collect::<Vec<_>>();
            let Some(pc_begin_rela) = relas_in_fde
                .iter()
                .find(|entry| entry.offset == pc_begin_offset)
            else {
                offset = next_offset;
                continue;
            };
            if relas_in_fde.len() != 1 {
                offset = next_offset;
                continue;
            }
            let Some((target_section_index, target_section_offset, pc_begin_range)) =
                fde_pc_begin_target(&file, eh_frame_section.index(), pc_begin_rela)?
            else {
                offset = next_offset;
                continue;
            };
            let cie_pointer_pos = input_offset + 4;
            let Some(cie_input_offset) = cie_pointer_pos.checked_sub(u64::from(cie_id)) else {
                offset = next_offset;
                continue;
            };
            let entry_start = section_start + offset;
            candidates.push(FdeAddCandidate {
                input_ranges: std::iter::once(
                    file_offset + entry_start..file_offset + section_start + next_offset,
                )
                .chain(relas_in_fde.iter().map(|entry| {
                    file_offset + entry.addend_range.start..file_offset + entry.addend_range.end
                }))
                .collect(),
                input_file: String::new(),
                input: String::new(),
                target_section_index: target_section_index.0 as u32,
                eh_frame_section_index,
                input_offset,
                target_section_offset,
                target_output_offset: 0,
                fde_data: entry.to_vec(),
                pc_begin_range,
                cie_input_offset,
                cie_reference_fde_output_offset: 0,
            });
        }
        offset = next_offset;
    }
    Ok(candidates)
}

fn fde_pc_begin_target(
    file: &object::File<'_>,
    eh_frame_section_index: object::SectionIndex,
    entry: &RelaPatchEntry,
) -> Result<Option<(object::SectionIndex, u64, std::ops::Range<usize>)>> {
    let eh_frame = file
        .section_by_index(eh_frame_section_index)
        .context("Missing FDE addition .eh_frame section")?;
    let relocation = eh_frame
        .relocations()
        .find_map(|(offset, relocation)| (offset == entry.offset).then_some(relocation));
    let Some(relocation) = relocation else {
        return Ok(None);
    };
    if relocation.kind() != object::RelocationKind::Relative
        || relocation.encoding() != object::RelocationEncoding::Generic
        || relocation.size() != 32
        || relocation.has_implicit_addend()
    {
        return Ok(None);
    }
    let pc_begin_range = crate::elf::FDE_PC_BEGIN_OFFSET..crate::elf::FDE_PC_BEGIN_OFFSET + 4;

    match relocation.target() {
        object::RelocationTarget::Symbol(symbol_index) => {
            let symbol = file
                .symbol_by_index(symbol_index)
                .context("Missing FDE addition pc-begin symbol")?;
            let Some(section_index) = symbol.section_index() else {
                return Ok(None);
            };
            let Some(offset) = i128::from(symbol.address())
                .checked_add(i128::from(relocation.addend()))
                .and_then(|value| u64::try_from(value).ok())
            else {
                return Ok(None);
            };
            Ok(Some((section_index, offset, pc_begin_range)))
        }
        object::RelocationTarget::Section(section_index) => {
            let Some(offset) = u64::try_from(relocation.addend()).ok() else {
                return Ok(None);
            };
            Ok(Some((section_index, offset, pc_begin_range)))
        }
        _ => Ok(None),
    }
}

fn fde_add_patches_for_output(
    output: &[u8],
    candidates: &[FdeAddCandidate],
    existing_fdes: &[FdeRecord],
) -> Result<std::result::Result<Vec<ResolvedFdeAdd>, String>> {
    if candidates.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let file = object::File::parse(output)
        .context("Failed to parse output for incremental FDE addition")?;
    let Some(eh_frame) = file.section_by_name(".eh_frame") else {
        return Ok(Err(
            "output has no .eh_frame for incremental FDE addition".to_owned()
        ));
    };
    let Some((eh_frame_offset, eh_frame_size)) = eh_frame.file_range() else {
        return Ok(Err("output .eh_frame has no file range".to_owned()));
    };
    let Ok(eh_frame_start) = usize::try_from(eh_frame_offset) else {
        return Ok(Err("output .eh_frame offset is too large".to_owned()));
    };
    let Ok(eh_frame_size) = usize::try_from(eh_frame_size) else {
        return Ok(Err("output .eh_frame size is too large".to_owned()));
    };
    let Some(eh_frame_end) = eh_frame_start.checked_add(eh_frame_size) else {
        return Ok(Err("output .eh_frame range overflowed".to_owned()));
    };
    let Some(eh_frame_bytes) = output.get(eh_frame_start..eh_frame_end) else {
        return Ok(Err("output .eh_frame range is out of bounds".to_owned()));
    };
    let Some(terminator_offset_in_section) = eh_frame_terminator_offset(eh_frame_bytes) else {
        return Ok(Err(
            "output .eh_frame has no terminator for FDE addition".to_owned()
        ));
    };

    let eh_frame_hdr_address = file
        .section_by_name(".eh_frame_hdr")
        .map(|section| section.address());
    let Some(mut output_offset) = eh_frame_offset.checked_add(terminator_offset_in_section as u64)
    else {
        return Ok(Err(
            "output .eh_frame terminator offset overflowed".to_owned()
        ));
    };
    let mut resolved = Vec::new();
    for candidate in candidates {
        let target_section_address =
            match output_address_for_file_offset(&file, candidate.target_output_offset) {
                Some(address) => address,
                None => {
                    return Ok(Err(
                        "could not resolve FDE addition target output address".to_owned()
                    ));
                }
            };
        let Some(target_address) =
            target_section_address.checked_add(candidate.target_section_offset)
        else {
            return Ok(Err(
                "FDE addition target output address overflowed".to_owned()
            ));
        };
        let Some(fde_address) = output_offset
            .checked_sub(eh_frame_offset)
            .and_then(|section_offset| eh_frame.address().checked_add(section_offset))
        else {
            return Ok(Err("FDE addition output address overflowed".to_owned()));
        };
        let output_cie_offset =
            match output_cie_offset_for_fde(output, candidate.cie_reference_fde_output_offset) {
                Some(offset) => offset,
                None => {
                    return Ok(Err(
                        "could not resolve FDE addition CIE output offset".to_owned()
                    ));
                }
            };
        let Some(cie_pointer) = output_offset
            .checked_add(4)
            .and_then(|pointer_pos| pointer_pos.checked_sub(output_cie_offset))
            .and_then(|value| u32::try_from(value).ok())
        else {
            return Ok(Err("FDE addition CIE pointer overflowed".to_owned()));
        };

        let mut data = candidate.fde_data.clone();
        data[4..8].copy_from_slice(&cie_pointer.to_le_bytes());
        let field_address = fde_address + crate::elf::FDE_PC_BEGIN_OFFSET as u64;
        let Some(pc_begin) = i128::from(target_address)
            .checked_sub(i128::from(field_address))
            .and_then(|value| i32::try_from(value).ok())
        else {
            return Ok(Err("FDE addition pc-begin relocation overflowed".to_owned()));
        };
        let Some(pc_begin_range) = data.get_mut(candidate.pc_begin_range.clone()) else {
            return Ok(Err(
                "FDE addition pc-begin range is out of bounds".to_owned()
            ));
        };
        pc_begin_range.copy_from_slice(&pc_begin.to_le_bytes());

        let record = FdeRecord {
            input_file: candidate.input_file.clone().into(),
            input: candidate.input.clone().into(),
            section_index: candidate.target_section_index,
            eh_frame_section_index: candidate.eh_frame_section_index,
            input_offset: candidate.input_offset,
            output_offset,
            size: data.len() as u64,
        };
        let eh_frame_hdr_change = if let Some(eh_frame_hdr_address) = eh_frame_hdr_address {
            let Some(frame_ptr) = i128::from(target_address)
                .checked_sub(i128::from(eh_frame_hdr_address))
                .and_then(|value| i32::try_from(value).ok())
            else {
                return Ok(Err(
                    "FDE addition .eh_frame_hdr frame pointer overflowed".to_owned()
                ));
            };
            let Some(frame_info_ptr) = i128::from(fde_address)
                .checked_sub(i128::from(eh_frame_hdr_address))
                .and_then(|value| i32::try_from(value).ok())
            else {
                return Ok(Err(
                    "FDE addition .eh_frame_hdr frame-info pointer overflowed".to_owned(),
                ));
            };
            Some(EhFrameHdrChange::Add(EhFrameHdrEntryPatch {
                frame_ptr,
                frame_info_ptr,
            }))
        } else {
            None
        };

        resolved.push(ResolvedFdeAdd {
            patch: SectionPatch {
                output_offset,
                size: data.len() as u64,
                data,
                deferred_relocation: None,
                preserve_ranges: Vec::new(),
                adjustments: Vec::new(),
            },
            record,
            eh_frame_hdr_change,
        });
        let Some(next_output_offset) =
            output_offset.checked_add(resolved.last().unwrap().patch.size)
        else {
            return Ok(Err("FDE addition output offset overflowed".to_owned()));
        };
        output_offset = next_output_offset;
    }

    let Some(required_end) = output_offset.checked_add(4) else {
        return Ok(Err("FDE addition terminator offset overflowed".to_owned()));
    };
    if required_end > eh_frame_offset + eh_frame_size as u64 {
        return Ok(Err("no free .eh_frame space for FDE addition".to_owned()));
    }
    if !eh_frame_bytes[terminator_offset_in_section..required_end as usize - eh_frame_start]
        .iter()
        .all(|byte| *byte == 0)
    {
        return Ok(Err("FDE addition .eh_frame space is not empty".to_owned()));
    }

    let mut combined_data = Vec::new();
    for resolved in &resolved {
        combined_data.extend(&resolved.patch.data);
    }
    combined_data.extend(0_u32.to_le_bytes());
    if let Some(first) = resolved.first_mut() {
        first.patch = SectionPatch {
            output_offset: eh_frame_offset + terminator_offset_in_section as u64,
            size: combined_data.len() as u64,
            data: combined_data,
            deferred_relocation: None,
            preserve_ranges: Vec::new(),
            adjustments: Vec::new(),
        };
    }
    for extra in resolved.iter_mut().skip(1) {
        extra.patch = SectionPatch {
            output_offset: extra.record.output_offset,
            size: 0,
            data: Vec::new(),
            deferred_relocation: None,
            preserve_ranges: Vec::new(),
            adjustments: Vec::new(),
        };
    }

    let mut previous = existing_fdes.iter().cloned().collect::<HashSet<_>>();
    for resolved_fde in &resolved {
        if !previous.insert(resolved_fde.record.clone()) {
            return Ok(Err("duplicate incremental FDE addition record".to_owned()));
        }
    }
    Ok(Ok(resolved))
}

fn eh_frame_terminator_offset(eh_frame: &[u8]) -> Option<usize> {
    let mut offset = 0usize;
    while offset + 4 <= eh_frame.len() {
        let length = usize::try_from(read_u32_le(eh_frame.get(offset..offset + 4)?)?).ok()?;
        if length == 0 {
            return Some(offset);
        }
        offset = offset.checked_add(length.checked_add(4)?)?;
    }
    None
}

fn output_cie_offset_for_fde(output: &[u8], fde_output_offset: u64) -> Option<u64> {
    let pointer_pos = usize::try_from(fde_output_offset.checked_add(4)?).ok()?;
    let cie_pointer = u64::from(read_u32_le(output.get(pointer_pos..pointer_pos + 4)?)?);
    fde_output_offset.checked_add(4)?.checked_sub(cie_pointer)
}

fn output_address_for_file_offset(file: &object::File<'_>, file_offset: u64) -> Option<u64> {
    for section in file.sections() {
        let Some((section_offset, section_size)) = section.file_range() else {
            continue;
        };
        if file_offset >= section_offset
            && file_offset < section_offset.saturating_add(section_size)
        {
            return Some(section.address() + (file_offset - section_offset));
        }
    }
    None
}

fn fde_relocation_patches_for_input_bytes(
    current_bytes: &[u8],
    current_file_offset: usize,
    previous_bytes: &[u8],
    records: Vec<&FdeRecord>,
) -> Result<Vec<FdeRelocationPatch>> {
    let Some(current_section_headers) = elf_section_headers(current_bytes) else {
        return Ok(Vec::new());
    };
    let Some(previous_section_headers) = elf_section_headers(previous_bytes) else {
        return Ok(Vec::new());
    };
    let current_file = object::File::parse(current_bytes)
        .context("Failed to parse changed .eh_frame relocation input")?;
    let previous_file = object::File::parse(previous_bytes)
        .context("Failed to parse previous .eh_frame relocation input")?;
    let current_eh_frame_section_index = current_file
        .section_by_name(".eh_frame")
        .map(|section| section.index().0 as u32);

    let mut patches = Vec::new();
    for record in records {
        let Some(current_record) = current_fde_record_for_previous_record(
            &previous_file,
            &current_file,
            record,
            current_eh_frame_section_index,
        )?
        else {
            patches.push(FdeRelocationPatch {
                input_ranges: Vec::new(),
                patch: None,
                eh_frame_hdr_change: Some(EhFrameHdrChange::Remove(record.clone())),
                record_update: None,
            });
            continue;
        };
        let Some((current_fde_range, current_fde_input_offset)) =
            fde_input_range_for_target_section_at_offset(
                current_bytes,
                &current_section_headers,
                current_record.eh_frame_section_index,
                current_record.section_index,
                current_record.input_offset,
            )
            .or_else(|| {
                fde_input_range_for_target_section(
                    current_bytes,
                    &current_section_headers,
                    current_record.eh_frame_section_index,
                    current_record.section_index,
                )
            })
            .or_else(|| {
                fde_input_range(
                    current_bytes.len(),
                    &current_section_headers,
                    &current_record,
                )
                .map(|range| (range, current_record.input_offset))
            })
        else {
            patches.push(FdeRelocationPatch {
                input_ranges: Vec::new(),
                patch: None,
                eh_frame_hdr_change: Some(EhFrameHdrChange::Remove(record.clone())),
                record_update: None,
            });
            continue;
        };
        let mut current_record = current_record;
        current_record.input_offset = current_fde_input_offset;
        let Some(previous_fde_range) =
            fde_input_range(previous_bytes.len(), &previous_section_headers, record)
        else {
            continue;
        };
        let current_fde_data = &current_bytes[current_fde_range.clone()];
        let previous_fde_data = &previous_bytes[previous_fde_range];
        if current_fde_data.get(4..8) != previous_fde_data.get(4..8)
            && !fde_bytes_match_ignoring_cie_pointer(current_fde_data, previous_fde_data)
        {
            patches.push(FdeRelocationPatch {
                input_ranges: Vec::new(),
                patch: None,
                eh_frame_hdr_change: None,
                record_update: None,
            });
            continue;
        }
        let current_entries = rela_entries_for_section(
            current_bytes,
            &current_section_headers,
            current_record.eh_frame_section_index,
        );
        let previous_entries = rela_entries_for_section(
            previous_bytes,
            &previous_section_headers,
            record.eh_frame_section_index,
        );
        let Some(current_entries) = current_entries else {
            continue;
        };
        let Some(previous_entries) = previous_entries else {
            continue;
        };
        let current_entries = current_entries
            .into_iter()
            .filter(|entry| fde_contains_relocation(&current_record, entry.offset))
            .collect::<Vec<_>>();
        let previous_entries = previous_entries
            .into_iter()
            .filter(|entry| fde_contains_relocation(record, entry.offset))
            .collect::<Vec<_>>();
        if current_entries.len() != previous_entries.len() {
            continue;
        }
        let relocation_sizes = eh_frame_relocation_sizes(
            &current_file,
            current_record.eh_frame_section_index,
            &current_record,
        )?;

        let mut input_ranges = Vec::with_capacity(1);
        input_ranges.push(
            current_file_offset + current_fde_range.start
                ..current_file_offset + current_fde_range.end,
        );
        let mut preserve_ranges = Vec::with_capacity(1);
        preserve_ranges.push(4..8);
        let mut adjustments = Vec::new();
        let mut eh_frame_hdr_change = None;
        for (current, previous) in current_entries.iter().zip(&previous_entries) {
            let Some(current_field_offset) =
                current.offset.checked_sub(current_record.input_offset)
            else {
                input_ranges.clear();
                preserve_ranges.clear();
                adjustments.clear();
                eh_frame_hdr_change = None;
                break;
            };
            let Some(previous_field_offset) = previous.offset.checked_sub(record.input_offset)
            else {
                input_ranges.clear();
                preserve_ranges.clear();
                adjustments.clear();
                eh_frame_hdr_change = None;
                break;
            };
            if current_field_offset != previous_field_offset || current.info != previous.info {
                input_ranges.clear();
                preserve_ranges.clear();
                adjustments.clear();
                eh_frame_hdr_change = None;
                break;
            }
            let Some(field_size) = relocation_sizes.get(&current.offset).copied() else {
                input_ranges.clear();
                preserve_ranges.clear();
                adjustments.clear();
                eh_frame_hdr_change = None;
                break;
            };
            input_ranges.push(
                current_file_offset + current.addend_range.start
                    ..current_file_offset + current.addend_range.end,
            );
            let field_start = usize::try_from(current_field_offset)
                .context("Incremental .eh_frame relocation offset is too large")?;
            let field_end = field_start
                .checked_add(usize::from(field_size))
                .context("Incremental .eh_frame relocation range overflow")?;
            if field_end > record.size as usize {
                input_ranges.clear();
                preserve_ranges.clear();
                adjustments.clear();
                eh_frame_hdr_change = None;
                break;
            }
            preserve_ranges.push(field_start..field_end);
            let Some(addend_delta) = current.addend.checked_sub(previous.addend) else {
                input_ranges.clear();
                preserve_ranges.clear();
                adjustments.clear();
                eh_frame_hdr_change = None;
                break;
            };
            if addend_delta == 0 {
                continue;
            }
            if field_start == crate::elf::FDE_PC_BEGIN_OFFSET {
                eh_frame_hdr_change = Some(EhFrameHdrChange::Adjust(EhFrameHdrDelta {
                    fde_output_offset: record.output_offset,
                    frame_ptr_delta: addend_delta,
                }));
            }
            adjustments.push(PatchAdjustment {
                range: field_start..field_end,
                addend_delta,
            });
        }
        preserve_ranges.sort_by_key(|range| (range.start, range.end));
        preserve_ranges.dedup_by(|left, right| left.start == right.start && left.end == right.end);
        let needs_patch = !input_ranges.is_empty()
            && (current_fde_data != previous_fde_data || !adjustments.is_empty());
        let record_update =
            (!input_ranges.is_empty() && current_record != *record).then(|| FdeRecordUpdate {
                previous: record.clone(),
                current: current_record.clone(),
            });
        patches.push(FdeRelocationPatch {
            input_ranges,
            patch: needs_patch.then(|| SectionPatch {
                output_offset: record.output_offset,
                size: record.size,
                data: current_fde_data.to_vec(),
                deferred_relocation: None,
                preserve_ranges,
                adjustments,
            }),
            eh_frame_hdr_change,
            record_update,
        });
    }
    Ok(patches)
}

fn update_fde_records(fdes: &mut Vec<FdeRecord>, updates: Vec<FdeRecordUpdate>) {
    for update in updates {
        if let Some(record) = fdes.iter_mut().find(|record| **record == update.previous) {
            *record = update.current;
        } else {
            fdes.push(update.current);
        }
    }
    fdes.sort();
    fdes.dedup();
}

fn eh_frame_hdr_patches_for_fde_changes(
    output: &[u8],
    changes: &[EhFrameHdrChange],
) -> Result<std::result::Result<Vec<SectionPatch>, String>> {
    if changes.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let has_removal = changes
        .iter()
        .any(|change| matches!(change, EhFrameHdrChange::Remove(_)));

    let file = object::File::parse(output)
        .context("Failed to parse output for incremental .eh_frame_hdr patching")?;
    let Some(eh_frame_hdr) = file.section_by_name(".eh_frame_hdr") else {
        if has_removal {
            return Ok(Err(
                "output has no .eh_frame_hdr for incremental FDE removal".to_owned(),
            ));
        }
        return Ok(Ok(Vec::new()));
    };
    let Some(eh_frame) = file.section_by_name(".eh_frame") else {
        return Ok(Err(
            "output has .eh_frame_hdr but no .eh_frame section".to_owned()
        ));
    };
    let Some((eh_frame_offset, eh_frame_size)) = eh_frame.file_range() else {
        return Ok(Err("output .eh_frame has no file range".to_owned()));
    };
    let Some((eh_frame_hdr_offset, eh_frame_hdr_size)) = eh_frame_hdr.file_range() else {
        return Ok(Err("output .eh_frame_hdr has no file range".to_owned()));
    };

    let header_size = std::mem::size_of::<crate::elf::EhFrameHdr>();
    let entry_size = std::mem::size_of::<crate::elf::EhFrameHdrEntry>();
    let Ok(eh_frame_hdr_start) = usize::try_from(eh_frame_hdr_offset) else {
        return Ok(Err("output .eh_frame_hdr offset is too large".to_owned()));
    };
    let Ok(eh_frame_hdr_size) = usize::try_from(eh_frame_hdr_size) else {
        return Ok(Err("output .eh_frame_hdr size is too large".to_owned()));
    };
    if eh_frame_hdr_size < header_size
        || !(eh_frame_hdr_size - header_size).is_multiple_of(entry_size)
    {
        return Ok(Err("output .eh_frame_hdr has an invalid size".to_owned()));
    }
    let Some(eh_frame_hdr_end) = eh_frame_hdr_start.checked_add(eh_frame_hdr_size) else {
        return Ok(Err("output .eh_frame_hdr range overflowed".to_owned()));
    };
    let Some(eh_frame_hdr_bytes) = output.get(eh_frame_hdr_start..eh_frame_hdr_end) else {
        return Ok(Err("output .eh_frame_hdr range is out of bounds".to_owned()));
    };

    let Some(entry_count_bytes) = eh_frame_hdr_bytes.get(8..12) else {
        return Ok(Err(
            "output .eh_frame_hdr entry count is truncated".to_owned()
        ));
    };
    let Some(entry_count) = read_u32_le(entry_count_bytes) else {
        return Ok(Err(
            "output .eh_frame_hdr entry count is truncated".to_owned()
        ));
    };
    let Ok(entry_count) = usize::try_from(entry_count) else {
        return Ok(Err(
            "output .eh_frame_hdr entry count is too large".to_owned()
        ));
    };
    let entry_capacity = (eh_frame_hdr_size - header_size) / entry_size;
    if entry_count > entry_capacity {
        return Ok(Err(
            "output .eh_frame_hdr entry count exceeds capacity".to_owned()
        ));
    }

    let mut entries = eh_frame_hdr_bytes[header_size..]
        .chunks_exact(entry_size)
        .take(entry_count)
        .map(|entry| {
            let frame_ptr = read_i32_le(&entry[0..4])
                .ok_or_else(|| "output .eh_frame_hdr entry is truncated".to_owned())?;
            let frame_info_ptr = read_i32_le(&entry[4..8])
                .ok_or_else(|| "output .eh_frame_hdr entry is truncated".to_owned())?;
            Ok(EhFrameHdrEntryPatch {
                frame_ptr,
                frame_info_ptr,
            })
        })
        .collect::<std::result::Result<Vec<_>, String>>()?;

    let mut changed_indices = Vec::new();
    let mut removed_indices = Vec::new();
    let mut added_entries = Vec::new();
    for change in changes {
        match change {
            EhFrameHdrChange::Adjust(delta) => {
                let index = match eh_frame_hdr_index_for_fde(
                    &entries,
                    eh_frame_offset,
                    eh_frame_size,
                    eh_frame.address(),
                    eh_frame_hdr.address(),
                    delta.fde_output_offset,
                ) {
                    Ok(index) => index,
                    Err(reason) => return Ok(Err(reason)),
                };
                let Some(adjusted) = i64::from(entries[index].frame_ptr)
                    .checked_add(delta.frame_ptr_delta)
                    .and_then(|value| i32::try_from(value).ok())
                else {
                    return Ok(Err(
                        "changed .eh_frame_hdr frame pointer overflowed".to_owned()
                    ));
                };
                entries[index].frame_ptr = adjusted;
                changed_indices.push(index);
            }
            EhFrameHdrChange::Remove(fde) => {
                let index = match eh_frame_hdr_index_for_fde(
                    &entries,
                    eh_frame_offset,
                    eh_frame_size,
                    eh_frame.address(),
                    eh_frame_hdr.address(),
                    fde.output_offset,
                ) {
                    Ok(index) => index,
                    Err(reason) => return Ok(Err(reason)),
                };
                removed_indices.push(index);
            }
            EhFrameHdrChange::Add(entry) => {
                added_entries.push(entry.clone());
            }
        }
    }

    if !entries
        .windows(2)
        .all(|window| window[0].frame_ptr <= window[1].frame_ptr)
    {
        return Ok(Err(
            "changed .eh_frame_hdr entries would no longer be sorted".to_owned(),
        ));
    }

    changed_indices.sort_unstable();
    changed_indices.dedup();
    removed_indices.sort_unstable();
    removed_indices.dedup();

    if !added_entries.is_empty() {
        for index in removed_indices.iter().rev() {
            entries.remove(*index);
        }
        if entries.len() + added_entries.len() > entry_capacity {
            return Ok(Err(
                "no free .eh_frame_hdr entries for FDE addition".to_owned()
            ));
        }
        entries.extend(added_entries);
        entries.sort_by_key(|entry| entry.frame_ptr);
        if !entries
            .windows(2)
            .all(|window| window[0].frame_ptr <= window[1].frame_ptr)
        {
            return Ok(Err(
                "changed .eh_frame_hdr entries would no longer be sorted".to_owned(),
            ));
        }
        let entry_count = u32::try_from(entries.len())
            .map_err(|_| "changed .eh_frame_hdr entry count overflowed".to_owned())?;
        let mut patches = vec![SectionPatch {
            output_offset: (eh_frame_hdr_start + 8) as u64,
            size: 4,
            data: entry_count.to_le_bytes().to_vec(),
            deferred_relocation: None,
            preserve_ranges: Vec::new(),
            adjustments: Vec::new(),
        }];
        let mut data = Vec::with_capacity(entries.len() * entry_size);
        for entry in &entries {
            data.extend(entry.frame_ptr.to_le_bytes());
            data.extend(entry.frame_info_ptr.to_le_bytes());
        }
        data.resize(entry_capacity * entry_size, 0);
        patches.push(SectionPatch {
            output_offset: (eh_frame_hdr_start + header_size) as u64,
            size: data.len() as u64,
            data,
            deferred_relocation: None,
            preserve_ranges: Vec::new(),
            adjustments: Vec::new(),
        });
        return Ok(Ok(patches));
    }

    if removed_indices.is_empty() {
        return Ok(Ok(changed_indices
            .into_iter()
            .map(|index| SectionPatch {
                output_offset: (eh_frame_hdr_start + header_size + index * entry_size) as u64,
                size: 4,
                data: entries[index].frame_ptr.to_le_bytes().to_vec(),
                deferred_relocation: None,
                preserve_ranges: Vec::new(),
                adjustments: Vec::new(),
            })
            .collect()));
    }

    let original_entry_count = entries.len();
    for index in removed_indices.iter().rev() {
        entries.remove(*index);
    }
    if !entries
        .windows(2)
        .all(|window| window[0].frame_ptr <= window[1].frame_ptr)
    {
        return Ok(Err(
            "changed .eh_frame_hdr entries would no longer be sorted".to_owned(),
        ));
    }

    let first_removed = *removed_indices.first().unwrap();
    let mut patches = changed_indices
        .into_iter()
        .filter(|index| *index < first_removed)
        .map(|index| SectionPatch {
            output_offset: (eh_frame_hdr_start + header_size + index * entry_size) as u64,
            size: 4,
            data: entries[index].frame_ptr.to_le_bytes().to_vec(),
            deferred_relocation: None,
            preserve_ranges: Vec::new(),
            adjustments: Vec::new(),
        })
        .collect::<Vec<_>>();

    let entry_count = u32::try_from(entries.len())
        .map_err(|_| "changed .eh_frame_hdr entry count overflowed".to_owned())?;
    patches.push(SectionPatch {
        output_offset: (eh_frame_hdr_start + 8) as u64,
        size: 4,
        data: entry_count.to_le_bytes().to_vec(),
        deferred_relocation: None,
        preserve_ranges: Vec::new(),
        adjustments: Vec::new(),
    });

    let mut data = Vec::new();
    for entry in &entries[first_removed..] {
        data.extend(entry.frame_ptr.to_le_bytes());
        data.extend(entry.frame_info_ptr.to_le_bytes());
    }
    data.resize((original_entry_count - first_removed) * entry_size, 0);
    patches.push(SectionPatch {
        output_offset: (eh_frame_hdr_start + header_size + first_removed * entry_size) as u64,
        size: data.len() as u64,
        data,
        deferred_relocation: None,
        preserve_ranges: Vec::new(),
        adjustments: Vec::new(),
    });

    Ok(Ok(patches))
}

fn eh_frame_hdr_index_for_fde(
    entries: &[EhFrameHdrEntryPatch],
    eh_frame_offset: u64,
    eh_frame_size: u64,
    eh_frame_address: u64,
    eh_frame_hdr_address: u64,
    fde_output_offset: u64,
) -> std::result::Result<usize, String> {
    let Some(fde_offset_in_section) = fde_output_offset.checked_sub(eh_frame_offset) else {
        return Err("incremental FDE output offset is outside .eh_frame".to_owned());
    };
    if fde_offset_in_section >= eh_frame_size {
        return Err("incremental FDE output offset is outside .eh_frame".to_owned());
    }
    let Some(fde_address) = eh_frame_address.checked_add(fde_offset_in_section) else {
        return Err("incremental FDE address overflowed".to_owned());
    };
    let Ok(frame_info_ptr) =
        i32::try_from(i128::from(fde_address) - i128::from(eh_frame_hdr_address))
    else {
        return Err("incremental .eh_frame_hdr frame-info pointer overflowed".to_owned());
    };
    let mut matching_indices = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| (entry.frame_info_ptr == frame_info_ptr).then_some(index));
    let Some(index) = matching_indices.next() else {
        return Err("could not find .eh_frame_hdr entry for changed FDE".to_owned());
    };
    if matching_indices.next().is_some() {
        return Err("found multiple .eh_frame_hdr entries for changed FDE".to_owned());
    }
    Ok(index)
}

#[derive(Clone)]
struct EhFrameHdrEntryPatch {
    frame_ptr: i32,
    frame_info_ptr: i32,
}

fn eh_frame_relocation_sizes(
    file: &object::File<'_>,
    eh_frame_section_index: u32,
    record: &FdeRecord,
) -> Result<HashMap<u64, u8>> {
    let section = file
        .section_by_index(object::SectionIndex(eh_frame_section_index as usize))
        .context("Missing changed .eh_frame section")?;
    let mut sizes = HashMap::new();
    for (offset, relocation) in section.relocations() {
        if !fde_contains_relocation(record, offset)
            || relocation.has_implicit_addend()
            || relocation.encoding() != object::RelocationEncoding::Generic
            || relocation.size() == 0
            || relocation.size() % 8 != 0
        {
            continue;
        }
        sizes.insert(offset, relocation.size() / 8);
    }
    Ok(sizes)
}

fn current_fde_record_for_previous_record(
    previous_file: &object::File<'_>,
    current_file: &object::File<'_>,
    record: &FdeRecord,
    current_eh_frame_section_index: Option<u32>,
) -> Result<Option<FdeRecord>> {
    let Some(eh_frame_section_index) = current_eh_frame_section_index else {
        return Ok(None);
    };
    let mut current_record = record.clone();
    current_record.eh_frame_section_index = eh_frame_section_index;

    let previous_section = previous_file
        .section_by_index(object::SectionIndex(record.section_index as usize))
        .context("Missing previous FDE target section")?;
    let Ok(section_name) = previous_section.name() else {
        return Ok(Some(current_record));
    };
    let Some(current_section) = current_file.section_by_name(section_name) else {
        return Ok(None);
    };
    current_record.section_index = current_section.index().0 as u32;
    Ok(Some(current_record))
}

fn fde_input_range_for_target_section(
    bytes: &[u8],
    section_headers: &[ElfSectionHeader],
    eh_frame_section_index: u32,
    target_section_index: u32,
) -> Option<(std::ops::Range<usize>, u64)> {
    fde_input_range_for_target_section_matching(
        bytes,
        section_headers,
        eh_frame_section_index,
        target_section_index,
        None,
    )
}

fn fde_input_range_for_target_section_at_offset(
    bytes: &[u8],
    section_headers: &[ElfSectionHeader],
    eh_frame_section_index: u32,
    target_section_index: u32,
    target_input_offset: u64,
) -> Option<(std::ops::Range<usize>, u64)> {
    fde_input_range_for_target_section_matching(
        bytes,
        section_headers,
        eh_frame_section_index,
        target_section_index,
        Some(target_input_offset),
    )
}

fn fde_input_range_for_target_section_matching(
    bytes: &[u8],
    section_headers: &[ElfSectionHeader],
    eh_frame_section_index: u32,
    target_section_index: u32,
    target_input_offset: Option<u64>,
) -> Option<(std::ops::Range<usize>, u64)> {
    let section = section_headers.get(eh_frame_section_index as usize)?;
    let section_start = usize::try_from(section.sh_offset).ok()?;
    let section_size = usize::try_from(section.sh_size).ok()?;
    let section_end = section_start.checked_add(section_size)?;
    let entries = rela_entries_for_section(bytes, section_headers, eh_frame_section_index)?;
    let symbol_section_indices = elf_symbol_section_indices(bytes, section_headers)?;

    let mut entry_offset = 0usize;
    while entry_offset.checked_add(8)? <= section_size {
        let entry_start = section_start.checked_add(entry_offset)?;
        let length =
            usize::try_from(read_u32_le(bytes.get(entry_start..entry_start + 4)?)?).ok()?;
        if length == 0 {
            break;
        }
        let entry_size = length.checked_add(4)?;
        let entry_end_offset = entry_offset.checked_add(entry_size)?;
        let entry_end = section_start.checked_add(entry_end_offset)?;
        if entry_end > section_end {
            break;
        }
        let input_offset = u64::try_from(entry_offset).ok()?;
        if target_input_offset.is_some_and(|target| target != input_offset) {
            entry_offset = entry_end_offset;
            continue;
        }
        let pc_begin_offset = input_offset + crate::elf::FDE_PC_BEGIN_OFFSET as u64;
        let has_target_pc_begin = entries.iter().any(|entry| {
            entry.offset == pc_begin_offset
                && symbol_section_indices
                    .get(&(entry.info >> 32))
                    .is_some_and(|section_index| *section_index == target_section_index)
        });
        if has_target_pc_begin {
            return Some((entry_start..entry_end, input_offset));
        }
        entry_offset = entry_end_offset;
    }
    None
}

fn elf_symbol_section_indices(
    bytes: &[u8],
    section_headers: &[ElfSectionHeader],
) -> Option<HashMap<u64, u32>> {
    let mut sections = HashMap::new();
    for section in section_headers {
        if section.sh_type != u64::from(object::elf::SHT_SYMTAB) || section.sh_entsize < 24 {
            continue;
        }
        let start = usize::try_from(section.sh_offset).ok()?;
        let size = usize::try_from(section.sh_size).ok()?;
        let entsize = usize::try_from(section.sh_entsize).ok()?;
        let end = start.checked_add(size)?;
        let section_bytes = bytes.get(start..end)?;
        for (symbol_index, symbol) in section_bytes.chunks_exact(entsize).enumerate() {
            let section_index = read_u16_le(symbol.get(6..8)?)?;
            sections.insert(u64::try_from(symbol_index).ok()?, u32::from(section_index));
        }
        return Some(sections);
    }
    None
}

fn fde_bytes_match_ignoring_cie_pointer(current: &[u8], previous: &[u8]) -> bool {
    current.len() == previous.len()
        && current.get(..4) == previous.get(..4)
        && current.get(8..) == previous.get(8..)
}

fn fde_cie_input_offset(fde_data: &[u8], fde_input_offset: u64) -> Option<u64> {
    let cie_id = read_u32_le(fde_data.get(4..8)?)?;
    (cie_id != 0).then_some(
        fde_input_offset
            .checked_add(4)?
            .checked_sub(u64::from(cie_id))?,
    )
}

fn fde_contains_relocation(record: &FdeRecord, relocation_offset: u64) -> bool {
    relocation_offset >= record.input_offset
        && relocation_offset < record.input_offset.saturating_add(record.size)
}

fn fde_input_range(
    input_len: usize,
    section_headers: &[ElfSectionHeader],
    record: &FdeRecord,
) -> Option<std::ops::Range<usize>> {
    let section = section_headers.get(record.eh_frame_section_index as usize)?;
    let section_start = usize::try_from(section.sh_offset).ok()?;
    let section_size = usize::try_from(section.sh_size).ok()?;
    let section_end = section_start.checked_add(section_size)?;
    let start = section_start.checked_add(usize::try_from(record.input_offset).ok()?)?;
    let size = usize::try_from(record.size).ok()?;
    let end = start.checked_add(size)?;
    (end <= input_len && end <= section_end && size >= 8).then_some(start..end)
}

struct RelaPatchEntry {
    offset: u64,
    info: u64,
    addend: i64,
    addend_range: std::ops::Range<usize>,
}

fn rela_entries_for_section(
    bytes: &[u8],
    section_headers: &[ElfSectionHeader],
    section_index: u32,
) -> Option<Vec<RelaPatchEntry>> {
    let mut entries = Vec::new();
    for section in section_headers {
        if section.sh_type != u64::from(object::elf::SHT_RELA)
            || section.sh_info != u64::from(section_index)
            || section.sh_entsize != crate::elf::RELA_ENTRY_SIZE
        {
            continue;
        }
        let start = usize::try_from(section.sh_offset).ok()?;
        let size = usize::try_from(section.sh_size).ok()?;
        let end = start.checked_add(size)?;
        let section_bytes = bytes.get(start..end)?;
        for (entry_index, entry) in section_bytes
            .chunks_exact(crate::elf::RELA_ENTRY_SIZE as usize)
            .enumerate()
        {
            let entry_start =
                start.checked_add(entry_index * crate::elf::RELA_ENTRY_SIZE as usize)?;
            let addend_start = entry_start + 16;
            entries.push(RelaPatchEntry {
                offset: read_u64_le(entry.get(0..8)?)?,
                info: read_u64_le(entry.get(8..16)?)?,
                addend: read_i64_le(entry.get(16..24)?)?,
                addend_range: addend_start..addend_start + 8,
            });
        }
    }
    Some(entries)
}

fn dynamic_relocation_entry_range(
    bytes: &[u8],
    section_headers: &[ElfSectionHeader],
    record: &DynamicRelocationRecord,
) -> Result<Option<std::ops::Range<usize>>> {
    for section in section_headers {
        if section.sh_type != u64::from(object::elf::SHT_RELA)
            || section.sh_info != u64::from(record.section_index)
            || section.sh_entsize != crate::elf::RELA_ENTRY_SIZE
        {
            continue;
        }
        let start = usize::try_from(section.sh_offset)
            .context("Incremental dynamic relocation section offset is too large")?;
        let size = usize::try_from(section.sh_size)
            .context("Incremental dynamic relocation section size is too large")?;
        let end = start
            .checked_add(size)
            .context("Incremental dynamic relocation section range overflow")?;
        let Some(section_bytes) = bytes.get(start..end) else {
            continue;
        };
        for (entry_index, entry) in section_bytes
            .chunks_exact(crate::elf::RELA_ENTRY_SIZE as usize)
            .enumerate()
        {
            let Some(offset) = read_u64_le(entry.get(0..8).unwrap_or_default()) else {
                continue;
            };
            if offset == record.relocation_offset {
                let entry_start = start
                    .checked_add(entry_index * crate::elf::RELA_ENTRY_SIZE as usize)
                    .context("Incremental dynamic relocation entry range overflow")?;
                let entry_end = entry_start + crate::elf::RELA_ENTRY_SIZE as usize;
                return Ok(Some(entry_start..entry_end));
            }
        }
    }
    Ok(None)
}

struct ElfSectionHeader {
    sh_type: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_info: u64,
    sh_entsize: u64,
}

fn elf_section_headers(bytes: &[u8]) -> Option<Vec<ElfSectionHeader>> {
    if bytes.len() < 0x34 || bytes.get(0..4)? != b"\x7fELF" || *bytes.get(5)? != 1 {
        return None;
    }

    let (section_header_offset, section_header_size, section_count, class) = match *bytes.get(4)? {
        1 => (
            read_u32_le(bytes.get(0x20..0x24)?)? as usize,
            read_u16_le(bytes.get(0x2e..0x30)?)? as usize,
            read_u16_le(bytes.get(0x30..0x32)?)? as usize,
            1_u8,
        ),
        2 => {
            if bytes.len() < 0x40 {
                return None;
            }
            (
                read_u64_le(bytes.get(0x28..0x30)?)? as usize,
                read_u16_le(bytes.get(0x3a..0x3c)?)? as usize,
                read_u16_le(bytes.get(0x3c..0x3e)?)? as usize,
                2_u8,
            )
        }
        _ => return None,
    };

    let mut sections = Vec::with_capacity(section_count);
    for section_index in 0..section_count {
        let start =
            section_header_offset.checked_add(section_index.checked_mul(section_header_size)?)?;
        let header = bytes.get(start..start.checked_add(section_header_size)?)?;
        let section = match class {
            1 => {
                if header.len() < 40 {
                    return None;
                }
                ElfSectionHeader {
                    sh_type: u64::from(read_u32_le(header.get(4..8)?)?),
                    sh_offset: u64::from(read_u32_le(header.get(16..20)?)?),
                    sh_size: u64::from(read_u32_le(header.get(20..24)?)?),
                    sh_info: u64::from(read_u32_le(header.get(28..32)?)?),
                    sh_entsize: u64::from(read_u32_le(header.get(36..40)?)?),
                }
            }
            2 => {
                if header.len() < 64 {
                    return None;
                }
                ElfSectionHeader {
                    sh_type: u64::from(read_u32_le(header.get(4..8)?)?),
                    sh_offset: read_u64_le(header.get(24..32)?)?,
                    sh_size: read_u64_le(header.get(32..40)?)?,
                    sh_info: u64::from(read_u32_le(header.get(44..48)?)?),
                    sh_entsize: read_u64_le(header.get(56..64)?)?,
                }
            }
            _ => return None,
        };
        sections.push(section);
    }
    Some(sections)
}

fn elf_symbol_value_field_range(
    bytes: &[u8],
    symbol_index: object::SymbolIndex,
) -> Option<std::ops::Range<usize>> {
    if bytes.len() < 0x34 || bytes.get(0..4)? != b"\x7fELF" || *bytes.get(5)? != 1 {
        return None;
    }

    let (entry_size, value_offset, value_size) = match *bytes.get(4)? {
        1 => (16usize, 4usize, 4usize),
        2 => (24usize, 8usize, 8usize),
        _ => return None,
    };
    let symbol_offset = symbol_index.0.checked_mul(entry_size)?;
    let symbol_end = symbol_offset.checked_add(entry_size)?;
    for section in elf_section_headers(bytes)? {
        if section.sh_type != u64::from(object::elf::SHT_SYMTAB)
            || section.sh_entsize != entry_size as u64
            || symbol_end > usize::try_from(section.sh_size).ok()?
        {
            continue;
        }
        let section_start = usize::try_from(section.sh_offset).ok()?;
        let field_start = section_start
            .checked_add(symbol_offset)?
            .checked_add(value_offset)?;
        let field_end = field_start.checked_add(value_size)?;
        if field_end <= bytes.len() {
            return Some(field_start..field_end);
        }
    }
    None
}

fn patch_ranges(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<Vec<std::ops::Range<usize>>>> {
    let mut ranges = Vec::new();
    let sections = sections.into_iter().collect::<Vec<_>>();
    let mut sections_by_input = HashMap::<&str, Vec<&PatchSection>>::new();
    for section in &sections {
        sections_by_input
            .entry(section.input.as_str())
            .or_default()
            .push(section);
    }

    for (input_ref, sections) in sections_by_input {
        let Some(input_bytes) = patch_input_bytes(bytes, input_file_path, input_ref)? else {
            return Ok(None);
        };
        let file = object::File::parse(input_bytes.bytes)
            .context("Failed to parse incremental patch input")?;
        for patch_section in sections {
            let Some(section_index) = patch_section_index(&file, patch_section)? else {
                return Ok(None);
            };
            let section = file
                .section_by_index(section_index)
                .context("Missing incremental patch input section")?;
            let Some((offset, size)) = section.file_range() else {
                return Ok(None);
            };
            if size > patch_section.output_size {
                return Ok(None);
            }
            let start = input_bytes
                .file_offset
                .checked_add(offset as usize)
                .context("Incremental patch input range overflow")?;
            let end = start
                .checked_add(size as usize)
                .context("Incremental patch input range overflow")?;
            if end > bytes.len() {
                return Ok(None);
            }
            ranges.push(start..end);
            if let Some(size_range) =
                elf_section_size_field_range(input_bytes.bytes, section_index.0)
            {
                ranges.push(
                    input_bytes.file_offset + size_range.start
                        ..input_bytes.file_offset + size_range.end,
                );
            }
        }
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

fn patch_section_object_index(
    file: &object::File<'_>,
    section_index: u32,
) -> Result<object::SectionIndex> {
    let section_index = section_index as usize;
    let section_index = match file {
        object::File::MachO32(_) | object::File::MachO64(_) => section_index
            .checked_add(1)
            .context("Mach-O incremental section index overflow")?,
        _ => section_index,
    };
    Ok(object::SectionIndex(section_index))
}

fn update_hash_with_zeroes(hasher: &mut blake3::Hasher, mut len: usize) {
    const ZEROES: [u8; 4096] = [0; 4096];
    while len > 0 {
        let chunk_len = len.min(ZEROES.len());
        hasher.update(&ZEROES[..chunk_len]);
        len -= chunk_len;
    }
}

fn elf_section_size_field_range(
    bytes: &[u8],
    section_index: usize,
) -> Option<std::ops::Range<usize>> {
    if bytes.len() < 0x34 || bytes.get(0..4)? != b"\x7fELF" || *bytes.get(5)? != 1 {
        return None;
    }

    match *bytes.get(4)? {
        1 => {
            let section_header_offset = read_u32_le(bytes.get(0x20..0x24)?)? as usize;
            let section_header_size = read_u16_le(bytes.get(0x2e..0x30)?)? as usize;
            let section_count = read_u16_le(bytes.get(0x30..0x32)?)? as usize;
            elf_section_header_field_range(
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
            elf_section_header_field_range(
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

fn elf_section_header_field_range(
    bytes: &[u8],
    section_index: usize,
    section_header_offset: usize,
    section_header_size: usize,
    section_count: usize,
    field_offset: usize,
    field_size: usize,
) -> Option<std::ops::Range<usize>> {
    if section_index >= section_count || section_header_size < field_offset + field_size {
        return None;
    }
    let section_start =
        section_header_offset.checked_add(section_index.checked_mul(section_header_size)?)?;
    let field_start = section_start.checked_add(field_offset)?;
    let field_end = field_start.checked_add(field_size)?;
    (field_end <= bytes.len()).then_some(field_start..field_end)
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

fn read_i32_le(bytes: &[u8]) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_i64_le(bytes: &[u8]) -> Option<i64> {
    Some(i64::from_le_bytes(bytes.try_into().ok()?))
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
    if !update_build_id_hash_tree(state, tree, output, &range, &changed_chunks) {
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

fn build_id_hash_state_from_output(bytes: &[u8]) -> Result<BuildIdHashStateAndTree> {
    let Some(range) = build_id_note_range(bytes)? else {
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
            tree_hash: Some(build_id_hash_tree_hash(&tree)),
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
) -> bool {
    if state.output_len != output.len() as u64 {
        return false;
    }
    if Some(state.nodes) != build_id_hash_node_count(output.len()) {
        return false;
    }
    if tree.len() != state.nodes {
        return false;
    }
    if output.len() <= BUILD_ID_HASH_GROUP_LEN {
        return false;
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
    state.tree_hash = Some(build_id_hash_tree_hash(tree));
    true
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

fn build_id_hash_tree_hash(tree: &[[u8; blake3::OUT_LEN]]) -> String {
    let mut hasher = blake3::Hasher::new();
    for node in tree {
        hasher.update(node);
    }
    hasher.finalize().to_hex().to_string()
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
    let tree = bytes
        .chunks_exact(blake3::OUT_LEN)
        .map(|chunk| chunk.try_into().unwrap())
        .collect::<Vec<_>>();
    if let Some(expected_hash) = state.tree_hash.as_deref()
        && build_id_hash_tree_hash(&tree) != expected_hash
    {
        return Err(crate::error!(
            "Incremental build ID hash tree does not match its recorded hash"
        ));
    }
    Ok(tree)
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

fn parse_link_start_line(line: Option<&str>) -> Result<Option<FileIdentity>> {
    let rest = parse_prefixed_line(line, "link-start")?;
    FileIdentity::parse(rest)
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
    let tree_hash = parts
        .next()
        .filter(|tree_hash| *tree_hash != ABSENT_FIELD)
        .map(|tree_hash| tree_hash.to_owned());
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental build ID hash record"));
    }
    if nodes == 0 {
        return Err(crate::error!("Missing incremental build ID hash nodes"));
    }
    Ok(Some(BuildIdHashState {
        output_len,
        nodes,
        tree_hash,
    }))
}

#[derive(Default)]
struct CompactRecords {
    sections: Vec<SectionRecord>,
    relocations: Vec<RelocationRecord>,
    fdes: Vec<FdeRecord>,
    dynamic_relocations: Vec<DynamicRelocationRecord>,
}

fn parse_compact_records_block<'a>(
    mut lines: impl Iterator<Item = &'a str>,
) -> Result<CompactRecords> {
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
    let mut relocations = Vec::new();
    let mut fdes = Vec::new();
    let mut dynamic_relocations = Vec::new();
    let mut next_line = lines.next();
    if let Some(line) = next_line
        && line.starts_with("relocs\t")
    {
        let relocation_count: usize = parse_prefixed_line(Some(line), "relocs")?
            .parse()
            .context("Invalid incremental relocation count")?;
        relocations = Vec::with_capacity(relocation_count);
        for _ in 0..relocation_count {
            let line = lines
                .next()
                .context("Missing incremental relocation record")?;
            relocations.push(parse_compact_relocation_line(line, &section_inputs)?);
        }
        next_line = lines.next();
    }
    if let Some(line) = next_line
        && line.starts_with("fdes\t")
    {
        let fde_count: usize = parse_prefixed_line(Some(line), "fdes")?
            .parse()
            .context("Invalid incremental FDE count")?;
        fdes = Vec::with_capacity(fde_count);
        for _ in 0..fde_count {
            let line = lines.next().context("Missing incremental FDE record")?;
            fdes.push(parse_compact_fde_line(line, &section_inputs)?);
        }
        next_line = lines.next();
    }
    if let Some(line) = next_line
        && line.starts_with("dynrels\t")
    {
        let relocation_count: usize = parse_prefixed_line(Some(line), "dynrels")?
            .parse()
            .context("Invalid incremental dynamic relocation count")?;
        dynamic_relocations = Vec::with_capacity(relocation_count);
        for _ in 0..relocation_count {
            let line = lines
                .next()
                .context("Missing incremental dynamic relocation record")?;
            dynamic_relocations.push(parse_compact_dynamic_relocation_line(
                line,
                &section_inputs,
            )?);
        }
        next_line = lines.next();
    }
    if next_line.is_some() || lines.next().is_some() {
        return Err(crate::error!(
            "Unexpected trailing incremental section data"
        ));
    }
    Ok(CompactRecords {
        sections,
        relocations,
        fdes,
        dynamic_relocations,
    })
}

fn parse_compact_records_block_for_input_files<'a>(
    mut lines: impl Iterator<Item = &'a str>,
    input_files: &HashSet<String>,
) -> Result<CompactRecords> {
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
    let mut sections = Vec::new();
    for _ in 0..section_count {
        let line = lines.next().context("Missing incremental section record")?;
        if compact_record_matches_input(line, "section", &section_inputs, input_files)? {
            sections.push(parse_compact_section_line(line, &section_inputs)?);
        }
    }

    let mut relocations = Vec::new();
    let mut fdes = Vec::new();
    let mut dynamic_relocations = Vec::new();
    let mut next_line = lines.next();
    if let Some(line) = next_line
        && line.starts_with("relocs\t")
    {
        let relocation_count: usize = parse_prefixed_line(Some(line), "relocs")?
            .parse()
            .context("Invalid incremental relocation count")?;
        for _ in 0..relocation_count {
            let line = lines
                .next()
                .context("Missing incremental relocation record")?;
            if compact_relocation_record_matches_input(line, &section_inputs, input_files)? {
                relocations.push(parse_compact_relocation_line(line, &section_inputs)?);
            }
        }
        next_line = lines.next();
    }
    if let Some(line) = next_line
        && line.starts_with("fdes\t")
    {
        let fde_count: usize = parse_prefixed_line(Some(line), "fdes")?
            .parse()
            .context("Invalid incremental FDE count")?;
        for _ in 0..fde_count {
            let line = lines.next().context("Missing incremental FDE record")?;
            if compact_record_matches_input(line, "fde", &section_inputs, input_files)? {
                fdes.push(parse_compact_fde_line(line, &section_inputs)?);
            }
        }
        next_line = lines.next();
    }
    if let Some(line) = next_line
        && line.starts_with("dynrels\t")
    {
        let relocation_count: usize = parse_prefixed_line(Some(line), "dynrels")?
            .parse()
            .context("Invalid incremental dynamic relocation count")?;
        for _ in 0..relocation_count {
            let line = lines
                .next()
                .context("Missing incremental dynamic relocation record")?;
            if compact_record_matches_input(line, "dynrel", &section_inputs, input_files)? {
                dynamic_relocations.push(parse_compact_dynamic_relocation_line(
                    line,
                    &section_inputs,
                )?);
            }
        }
        next_line = lines.next();
    }
    if next_line.is_some() || lines.next().is_some() {
        return Err(crate::error!(
            "Unexpected trailing incremental section data"
        ));
    }
    Ok(CompactRecords {
        sections,
        relocations,
        fdes,
        dynamic_relocations,
    })
}

fn compact_record_matches_input(
    line: &str,
    prefix: &str,
    section_inputs: &[(String, String)],
    input_files: &HashSet<String>,
) -> Result<bool> {
    let rest = parse_prefixed_line(Some(line), prefix)?;
    let section_input_id = compact_record_section_input_id(rest, prefix)?;
    let Some((input_file, _)) = section_inputs.get(section_input_id) else {
        return Err(crate::error!(
            "Incremental {prefix} input index out of bounds"
        ));
    };
    Ok(input_files.contains(input_file))
}

fn compact_relocation_record_matches_input(
    line: &str,
    section_inputs: &[(String, String)],
    input_files: &HashSet<String>,
) -> Result<bool> {
    if line.starts_with("reloc2\t") {
        let rest = parse_prefixed_line(Some(line), "reloc2")?;
        if compact_record_matches_input(line, "reloc2", section_inputs, input_files)? {
            return Ok(true);
        }
        let parts = rest.split('\t').collect::<Vec<_>>();
        if parts.len() != 14 || parts[11] == ABSENT_FIELD {
            return Ok(false);
        }
        let target_section_input_id: usize = parts[11]
            .parse()
            .context("Invalid incremental relocation target input index")?;
        let Some((target_input_file, _)) = section_inputs.get(target_section_input_id) else {
            return Err(crate::error!(
                "Incremental relocation target input index out of bounds"
            ));
        };
        return Ok(input_files.contains(target_input_file));
    }

    if compact_record_matches_input(line, "reloc", section_inputs, input_files)? {
        return Ok(true);
    }
    Ok(input_files
        .iter()
        .any(|input_file| line.contains(input_file)))
}

fn compact_record_section_input_id(rest: &str, prefix: &str) -> Result<usize> {
    rest.split('\t')
        .next()
        .context("Malformed incremental record input index")?
        .parse()
        .with_context(|| format!("Invalid incremental {prefix} input index"))
}

fn parse_input_line(line: &str, patch_section_mode: PatchSectionReadMode) -> Result<FileState> {
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
    let patch = match patch_fingerprint.zip(patch_sections) {
        Some((fingerprint, raw_sections)) => {
            let sections = match patch_section_mode {
                PatchSectionReadMode::Parse => parse_patch_sections(&path, raw_sections)?,
                PatchSectionReadMode::PreserveRaw => Vec::new(),
            };
            Some(FilePatchState {
                fingerprint: fingerprint.to_owned(),
                sections,
                raw_sections: matches!(patch_section_mode, PatchSectionReadMode::PreserveRaw)
                    .then(|| raw_sections.to_owned()),
            })
        }
        None => None,
    };
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
    if patch.sections.is_empty()
        && let Some(raw_sections) = &patch.raw_sections
    {
        return raw_sections.clone();
    }

    patch
        .sections
        .iter()
        .map(|section| {
            format!(
                "{}:{}:{}:{}:{}:{}:{}",
                section.input,
                section.section_index,
                section.input_size,
                section.output_offset,
                section.output_size,
                section.section_name.as_ref().map_or_else(
                    || ABSENT_FIELD.to_owned(),
                    |name| hex::encode(name.as_bytes())
                ),
                section.data_hash.as_deref().unwrap_or(ABSENT_FIELD)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn render_input_line_rest(input: &FileState) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
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
            .map_or_else(|| ABSENT_FIELD.to_owned(), render_patch_sections)
    )
}

fn render_build_id_hash_state(state: &BuildIdHashState) -> String {
    format!(
        "{}\t{}\t{}",
        state.output_len,
        state.nodes,
        state.tree_hash.as_deref().unwrap_or(ABSENT_FIELD)
    )
}

fn parse_patch_sections(default_input: &str, sections: &str) -> Result<Vec<FilePatchSectionState>> {
    if sections.is_empty() {
        return Ok(Vec::new());
    }
    let mut parsed = Vec::new();
    for section in sections.split(',') {
        let parts = section.split(':').collect::<Vec<_>>();
        let (input, parts, data_hash) = match parts.len() {
            4 | 5 => (default_input.to_owned(), parts.as_slice(), None),
            6 => (parts[0].to_owned(), &parts[1..], None),
            7 => (
                parts[0].to_owned(),
                &parts[1..6],
                (parts[6] != ABSENT_FIELD).then(|| parts[6].to_owned()),
            ),
            _ => return Ok(Vec::new()),
        };
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
            input,
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
            data_hash,
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
        input_file: input_file.into(),
        input: input.into(),
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
        input_file: input_file.clone().into(),
        input: input.clone().into(),
        section_index,
        output_offset,
        size,
    })
}

fn parse_compact_relocation_line(
    line: &str,
    section_inputs: &[(String, String)],
) -> Result<RelocationRecord> {
    let (is_compact_target, rest) = if line.starts_with("reloc2\t") {
        (true, parse_prefixed_line(Some(line), "reloc2")?)
    } else {
        (false, parse_prefixed_line(Some(line), "reloc")?)
    };
    let parts = rest.split('\t').collect::<Vec<_>>();
    let section_input_id: usize = parts
        .first()
        .copied()
        .context("Malformed incremental relocation input index")?
        .parse()
        .context("Invalid incremental relocation input index")?;
    let section_index = parts
        .get(1)
        .copied()
        .context("Malformed incremental relocation section index")?
        .parse()
        .context("Invalid incremental relocation section index")?;
    let target_symbol_id = parts
        .get(2)
        .copied()
        .context("Malformed incremental relocation target symbol")?
        .parse()
        .context("Invalid incremental relocation target symbol")?;
    let relocation_offset = parts
        .get(3)
        .copied()
        .context("Malformed incremental relocation input offset")?
        .parse()
        .context("Invalid incremental relocation input offset")?;
    let output_offset = parts
        .get(4)
        .copied()
        .context("Malformed incremental relocation output offset")?
        .parse()
        .context("Invalid incremental relocation output offset")?;
    let size = parts
        .get(5)
        .copied()
        .context("Malformed incremental relocation size")?
        .parse()
        .context("Invalid incremental relocation size")?;
    let kind = parts
        .get(6)
        .copied()
        .context("Malformed incremental relocation kind")?
        .parse()
        .context("Invalid incremental relocation kind")?;
    let addend = parts
        .get(7)
        .copied()
        .context("Malformed incremental relocation addend")?
        .parse()
        .context("Invalid incremental relocation addend")?;
    let (written_value, target_value, target_name, target) = if is_compact_target {
        if parts.len() != 14 {
            return Err(crate::error!("Malformed incremental relocation record"));
        }
        let written_value = (parts[8] != ABSENT_FIELD)
            .then(|| {
                parts[8]
                    .parse()
                    .context("Invalid incremental written relocation value")
            })
            .transpose()?;
        let target_value = parts[9]
            .parse()
            .context("Invalid incremental relocation target value")?;
        let target_name = (parts[10] != ABSENT_FIELD).then(|| parts[10].to_owned());
        let target = if parts[11] == ABSENT_FIELD
            && parts[12] == ABSENT_FIELD
            && parts[13] == ABSENT_FIELD
        {
            None
        } else {
            let target_section_input_id: usize = parts[11]
                .parse()
                .context("Invalid incremental relocation target input index")?;
            let (target_input_file, target_input) = section_inputs
                .get(target_section_input_id)
                .context("Incremental relocation target input index out of bounds")?;
            Some(RelocationTargetRecord {
                input_file: target_input_file.clone().into(),
                input: target_input.clone().into(),
                section_index: parts[12]
                    .parse()
                    .context("Invalid incremental relocation target section index")?,
                section_offset: parts[13]
                    .parse()
                    .context("Invalid incremental relocation target section offset")?,
            })
        };
        (written_value, target_value, target_name, target)
    } else {
        match parts.len() {
            8 => (None, 0, None, None),
            10 => {
                let target_value = parts[8]
                    .parse()
                    .context("Invalid incremental relocation target value")?;
                let target_name = (parts[9] != ABSENT_FIELD).then(|| parts[9].to_owned());
                (None, target_value, target_name, None)
            }
            14 => {
                let target_value = parts[8]
                    .parse()
                    .context("Invalid incremental relocation target value")?;
                let target_name = (parts[9] != ABSENT_FIELD).then(|| parts[9].to_owned());
                let target = if parts[10] == ABSENT_FIELD
                    && parts[11] == ABSENT_FIELD
                    && parts[12] == ABSENT_FIELD
                    && parts[13] == ABSENT_FIELD
                {
                    None
                } else {
                    Some(RelocationTargetRecord {
                        input_file: parts[10].to_owned().into(),
                        input: parts[11].to_owned().into(),
                        section_index: parts[12]
                            .parse()
                            .context("Invalid incremental relocation target section index")?,
                        section_offset: parts[13]
                            .parse()
                            .context("Invalid incremental relocation target section offset")?,
                    })
                };
                (None, target_value, target_name, target)
            }
            15 => {
                let written_value = (parts[8] != ABSENT_FIELD)
                    .then(|| {
                        parts[8]
                            .parse()
                            .context("Invalid incremental written relocation value")
                    })
                    .transpose()?;
                let target_value = parts[9]
                    .parse()
                    .context("Invalid incremental relocation target value")?;
                let target_name = (parts[10] != ABSENT_FIELD).then(|| parts[10].to_owned());
                let target = if parts[11] == ABSENT_FIELD
                    && parts[12] == ABSENT_FIELD
                    && parts[13] == ABSENT_FIELD
                    && parts[14] == ABSENT_FIELD
                {
                    None
                } else {
                    Some(RelocationTargetRecord {
                        input_file: parts[11].to_owned().into(),
                        input: parts[12].to_owned().into(),
                        section_index: parts[13]
                            .parse()
                            .context("Invalid incremental relocation target section index")?,
                        section_offset: parts[14]
                            .parse()
                            .context("Invalid incremental relocation target section offset")?,
                    })
                };
                (written_value, target_value, target_name, target)
            }
            _ => return Err(crate::error!("Malformed incremental relocation record")),
        }
    };
    let (input_file, input) = section_inputs
        .get(section_input_id)
        .context("Incremental relocation input index out of bounds")?;
    Ok(RelocationRecord {
        target_symbol_id,
        written_value,
        target_value,
        target_name,
        target,
        input_file: input_file.clone().into(),
        input: input.clone().into(),
        section_index,
        relocation_offset,
        output_offset,
        size,
        kind,
        addend,
    })
}

fn parse_compact_fde_line(line: &str, section_inputs: &[(String, String)]) -> Result<FdeRecord> {
    let rest = parse_prefixed_line(Some(line), "fde")?;
    let mut parts = rest.split('\t');
    let section_input_id: usize = parts
        .next()
        .context("Malformed incremental FDE input index")?
        .parse()
        .context("Invalid incremental FDE input index")?;
    let section_index = parts
        .next()
        .context("Malformed incremental FDE section index")?
        .parse()
        .context("Invalid incremental FDE section index")?;
    let eh_frame_section_index = parts
        .next()
        .context("Malformed incremental FDE .eh_frame section index")?
        .parse()
        .context("Invalid incremental FDE .eh_frame section index")?;
    let input_offset = parts
        .next()
        .context("Malformed incremental FDE input offset")?
        .parse()
        .context("Invalid incremental FDE input offset")?;
    let output_offset = parts
        .next()
        .context("Malformed incremental FDE output offset")?
        .parse()
        .context("Invalid incremental FDE output offset")?;
    let size = parts
        .next()
        .context("Malformed incremental FDE size")?
        .parse()
        .context("Invalid incremental FDE size")?;
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental FDE record"));
    }
    let (input_file, input) = section_inputs
        .get(section_input_id)
        .context("Incremental FDE input index out of bounds")?;
    Ok(FdeRecord {
        input_file: input_file.clone().into(),
        input: input.clone().into(),
        section_index,
        eh_frame_section_index,
        input_offset,
        output_offset,
        size,
    })
}

fn parse_compact_dynamic_relocation_line(
    line: &str,
    section_inputs: &[(String, String)],
) -> Result<DynamicRelocationRecord> {
    let rest = parse_prefixed_line(Some(line), "dynrel")?;
    let parts = rest.split('\t').collect::<Vec<_>>();
    let section_input_id: usize = parts
        .first()
        .copied()
        .context("Malformed incremental dynamic relocation input index")?
        .parse()
        .context("Invalid incremental dynamic relocation input index")?;
    let section_index = parts
        .get(1)
        .copied()
        .context("Malformed incremental dynamic relocation section index")?
        .parse()
        .context("Invalid incremental dynamic relocation section index")?;
    let (relocation_offset, output_offset_index, size_index, output_info_indices) =
        match parts.len() {
            4 => (0, 2, 3, None),
            5 => {
                let relocation_offset = parts[2]
                    .parse()
                    .context("Invalid incremental dynamic relocation input offset")?;
                (relocation_offset, 3, 4, None)
            }
            7 => {
                let relocation_offset = parts[2]
                    .parse()
                    .context("Invalid incremental dynamic relocation input offset")?;
                (relocation_offset, 3, 4, Some((5, 6)))
            }
            _ => {
                return Err(crate::error!(
                    "Malformed incremental dynamic relocation record"
                ));
            }
        };
    let output_offset = parts[output_offset_index]
        .parse()
        .context("Invalid incremental dynamic relocation output offset")?;
    let size = parts[size_index]
        .parse()
        .context("Invalid incremental dynamic relocation size")?;
    let (output_r_offset, output_r_info) =
        if let Some((r_offset_index, r_info_index)) = output_info_indices {
            (
                Some(
                    parts[r_offset_index]
                        .parse()
                        .context("Invalid incremental dynamic relocation output r_offset")?,
                ),
                Some(
                    parts[r_info_index]
                        .parse()
                        .context("Invalid incremental dynamic relocation output r_info")?,
                ),
            )
        } else {
            (None, None)
        };
    let (input_file, input) = section_inputs
        .get(section_input_id)
        .context("Incremental dynamic relocation input index out of bounds")?;
    Ok(DynamicRelocationRecord {
        input_file: input_file.clone().into(),
        input: input.clone().into(),
        section_index,
        relocation_offset,
        output_offset,
        size,
        output_r_offset,
        output_r_info,
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
    files_equal(&snapshot, current_path)
}

fn read_verified_input_snapshot(
    state_dir: &Path,
    previous_input: &FileState,
) -> Result<Option<Vec<u8>>> {
    let snapshot = input_snapshot_path_for_encoded_path(state_dir, &previous_input.path);
    let bytes = match std::fs::read(&snapshot) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !snapshot_bytes_match_previous_content(&previous_input.content, &snapshot, &bytes)? {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn snapshot_bytes_match_previous_content(
    previous: &FileContentState,
    snapshot: &Path,
    bytes: &[u8],
) -> Result<bool> {
    if previous.len != bytes.len() as u64 {
        return Ok(false);
    }
    if !previous.hash.is_empty() {
        return Ok(previous.hash == hash_bytes(bytes));
    }
    previous.identity_matches_snapshot_path(snapshot)
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

fn read_file_with_stable_identity(path: &Path) -> Result<Option<(Vec<u8>, FileContentState)>> {
    let before = FileIdentity::from_path(path)?;
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read `{}`", path.display()))?;
    let after = FileIdentity::from_path(path)?;
    if before != after {
        return Ok(None);
    }
    let Some(identity) = after else {
        let content = FileContentState::from_bytes(&bytes);
        return Ok(Some((bytes, content)));
    };
    if bytes.len() as u64 != identity.len {
        return Ok(None);
    }
    let mut content = FileContentState::from_bytes(&bytes);
    content.identity = Some(identity);
    Ok(Some((bytes, content)))
}

fn input_content_mismatch_reason(expected_inputs: &[ExpectedInputContent]) -> Option<String> {
    for expected in expected_inputs {
        let current = match read_file_with_stable_identity(&expected.path) {
            Ok(Some((bytes, _))) => FileContentState::from_bytes(&bytes),
            Ok(None) => {
                return Some(format!(
                    "input file changed while incremental fast path was running: {}",
                    expected.path.display()
                ));
            }
            Err(error) => {
                return Some(format!(
                    "input file could not be rechecked while incremental fast path was running: {} ({error:?})",
                    expected.path.display()
                ));
            }
        };
        if current.len != expected.len || current.hash != expected.hash {
            return Some(format!(
                "input file changed while incremental fast path was running: {}",
                expected.path.display()
            ));
        }
    }
    None
}

fn input_identity_mismatch_reason(input_files: &[FileState]) -> Result<Option<String>> {
    for input in input_files {
        let path = decode_path(&input.path)?;
        match input.content.identity_matches_path(&path) {
            Ok(true) => {}
            Ok(false) => {
                return Ok(Some(format!(
                    "input file changed while incremental fast path was running: {}",
                    path.display()
                )));
            }
            Err(error) => {
                return Ok(Some(format!(
                    "input file could not be rechecked while incremental fast path was running: {} ({error:?})",
                    path.display()
                )));
            }
        }
    }
    Ok(None)
}

fn refresh_input_file_identities(input_files: &mut [FileState]) {
    for input in input_files {
        refresh_input_file_identity(input);
    }
}

fn refresh_input_file_identities_at_indices(
    input_files: &mut [FileState],
    indices: impl IntoIterator<Item = usize>,
) {
    let mut seen = HashSet::new();
    for index in indices {
        if !seen.insert(index) {
            continue;
        }
        let Some(input) = input_files.get_mut(index) else {
            continue;
        };
        refresh_input_file_identity(input);
    }
}

fn refresh_input_file_identity(input: &mut FileState) {
    let Ok(path) = decode_path(&input.path) else {
        return;
    };
    let Ok(Some(identity)) = FileIdentity::from_path(&path) else {
        return;
    };
    input.content.len = identity.len;
    input.content.identity = Some(identity);
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

    copy_snapshot_bytes(path, &tmp)?;

    let _ = std::fs::remove_file(&target);
    std::fs::rename(&tmp, &target).with_context(|| {
        format!(
            "Failed to install incremental input snapshot `{}`",
            target.display()
        )
    })?;
    Ok(true)
}

fn copy_snapshot_bytes(source: &Path, target: &Path) -> Result {
    if clone_snapshot_bytes(source, target) {
        return Ok(());
    }

    let mut input = std::fs::File::open(source)
        .with_context(|| format!("Failed to read incremental input `{}`", source.display()))?;
    let mut output = std::fs::File::create(target).with_context(|| {
        format!(
            "Failed to create incremental input snapshot `{}`",
            target.display()
        )
    })?;
    std::io::copy(&mut input, &mut output).with_context(|| {
        format!(
            "Failed to copy incremental input snapshot `{}` to `{}`",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn clone_snapshot_bytes(source: &Path, target: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(source) = CString::new(source.as_os_str().as_bytes()) else {
        return false;
    };
    let Ok(target) = CString::new(target.as_os_str().as_bytes()) else {
        return false;
    };

    // `clonefile` creates an APFS copy-on-write clone, so the snapshot keeps
    // copy semantics without paying to duplicate every input byte up front.
    unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) == 0 }
}

#[cfg(not(target_vendor = "apple"))]
fn clone_snapshot_bytes(_source: &Path, _target: &Path) -> bool {
    false
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

fn interrupted_update_relink_reason(state_dir: &Path) -> Option<String> {
    match update_marker_path(state_dir).try_exists() {
        Ok(true) => Some("previous incremental update did not complete".to_owned()),
        Ok(false) => None,
        Err(error) => Some(format!(
            "previous incremental update status could not be checked: {error:?}"
        )),
    }
}

fn mark_incremental_update_started(state_dir: &Path, operation: &str) -> Result {
    std::fs::create_dir_all(state_dir)?;
    let path = update_marker_path(state_dir);
    std::fs::write(&path, format!("{operation}\n")).with_context(|| {
        format!(
            "Failed to write incremental update marker `{}`",
            path.display()
        )
    })
}

fn clear_incremental_update_marker(state_dir: &Path) -> Result {
    let path = update_marker_path(state_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to remove incremental update marker `{}`",
                path.display()
            )
        }),
    }
}

fn write_link_start_marker(state_dir: &Path) -> Result<Option<FileIdentity>> {
    std::fs::create_dir_all(state_dir)?;
    let path = link_start_marker_path(state_dir);
    std::fs::write(&path, b"link started\n").with_context(|| {
        format!(
            "Failed to write incremental link-start marker `{}`",
            path.display()
        )
    })?;
    Ok(link_start_marker_identity(state_dir))
}

fn link_start_marker_identity(state_dir: &Path) -> Option<FileIdentity> {
    FileIdentity::from_path(&link_start_marker_path(state_dir))
        .ok()
        .flatten()
}

fn link_start_marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LINK_START_FILE)
}

fn update_marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(UPDATE_MARKER_FILE)
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

fn metadata_update_path(state_dir: &Path) -> PathBuf {
    state_dir.join(METADATA_UPDATE_FILE)
}

fn should_filter_sections_sidecar(state_dir: &Path, sections_file: &str) -> bool {
    const LARGE_SECTIONS_SIDECAR: u64 = 64 * 1024 * 1024;
    if validate_sections_file_name(sections_file).is_err() {
        return false;
    }
    std::fs::metadata(state_dir.join(sections_file))
        .is_ok_and(|metadata| metadata.len() > LARGE_SECTIONS_SIDECAR)
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
                .join("sld")
        })
    }

    #[cfg(not(target_os = "macos"))]
    {
        if let Some(path) = env("XDG_STATE_HOME") {
            return Some(PathBuf::from(path).join("sld"));
        }
        env("HOME").map(|home| PathBuf::from(home).join(".local").join("state").join("sld"))
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

fn section_sidecar_file_name(contents: &str) -> String {
    format!("{SECTIONS_FILE_PREFIX}{}", hash_text(contents))
}

fn args_hash(args: &impl platform::Args) -> String {
    hash_text(&format!("{args:?}"))
}

fn link_options_hash(args: &impl platform::Args) -> String {
    hash_text(&args.incremental_link_options())
}

fn input_order_hash(file_loader: &FileLoader<'_>) -> String {
    let mut hasher = blake3::Hasher::new();
    for file in &file_loader.loaded_files {
        let path = encode_path(&file.filename);
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn sld_version(args: &impl platform::Args) -> String {
    args.common().version.to_string()
}

fn sld_version_relink_reason<'a>(previous: Option<&'a str>, current: &str) -> Option<&'a str> {
    match previous {
        Some(previous) if previous == current => None,
        Some(_) => Some("linker version changed"),
        None => Some("linker version missing from previous state"),
    }
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
            link_options_hash: Some(args_hash.to_owned()),
            input_order_hash: Some(input_order_hash_for_paths(
                inputs.iter().map(|(path, _)| *path),
            )),
            sld_version: Some("sld-test".to_owned()),
            link_start: None,
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
            relocations: Vec::new(),
            fdes: Vec::new(),
            dynamic_relocations: Vec::new(),
            sections_file: None,
        }
    }

    fn input_order_hash_for_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
        let mut hasher = blake3::Hasher::new();
        for path in paths {
            hasher.update(hex::encode(path).as_bytes());
            hasher.update(&[0]);
        }
        hasher.finalize().to_hex().to_string()
    }

    #[test]
    fn output_symbol_value_patches_match_duplicate_names_by_previous_value() {
        let (output, first_value_range, second_value_range) = duplicate_symbol_name_elf();

        let patches = output_symbol_value_patches(
            &output,
            &[RelocationTargetSymbolPatch {
                target_name: hex::encode(b"duplicate"),
                previous_target_value: 0x200,
                target_value: 0x208,
            }],
        )
        .unwrap()
        .unwrap();

        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].output_offset, second_value_range.start as u64);
        assert_eq!(patches[0].data, 0x208_u64.to_le_bytes());
        assert_eq!(&output[first_value_range], &0x100_u64.to_le_bytes());
        assert_eq!(&output[second_value_range], &0x200_u64.to_le_bytes());
    }

    #[test]
    fn output_symbol_value_patches_reject_missing_output_symbols() {
        let (output, _, _) = duplicate_symbol_name_elf();

        let patches = output_symbol_value_patches(
            &output,
            &[RelocationTargetSymbolPatch {
                target_name: hex::encode(b"missing"),
                previous_target_value: 0x200,
                target_value: 0x208,
            }],
        )
        .unwrap();

        assert!(matches!(
            patches,
            Err(reason) if reason == "missing output symbol for incremental value patch"
        ));
    }

    #[test]
    fn relocation_target_patch_supports_32_bit_output_payloads() {
        let (previous, first_value_range, _) = duplicate_symbol_name_elf();
        let mut current = previous.clone();
        current[first_value_range.clone()].copy_from_slice(&0x108_u64.to_le_bytes());
        let mut state = state("args", b"output", &[("input.o", &previous)]);
        let input = state.input_files.remove(0);
        let relocation = relocation_record(
            "input.o",
            1,
            42,
            Some(0x1000),
            0x2000,
            Some("duplicate"),
            Some(("input.o", 1, 0x100)),
            0,
            300,
            4,
            1,
            0,
        );
        let mut relocations = vec![relocation];

        let patches = relocation_target_patches_for_input(&mut relocations, &input, &current)
            .unwrap()
            .unwrap();

        assert_eq!(patches.input_ranges, vec![first_value_range]);
        assert_eq!(patches.output_patches.len(), 1);
        assert_eq!(patches.output_patches[0].output_offset, 300);
        assert_eq!(patches.output_patches[0].size, 4);
        assert_eq!(
            patches.output_patches[0].data,
            0x1008_u32.to_le_bytes().to_vec()
        );
        assert_eq!(relocations[0].written_value, Some(0x1008));
        assert_eq!(relocations[0].target_value, 0x2008);
        assert_eq!(
            relocations[0]
                .target
                .as_ref()
                .map(|target| target.section_offset),
            Some(0x108)
        );
    }

    #[test]
    fn relocation_target_patch_defers_riscv_instruction_payloads() {
        let (mut previous, first_value_range, _) = duplicate_symbol_name_elf();
        previous[18..20].copy_from_slice(&object::elf::EM_RISCV.to_le_bytes());
        let mut current = previous.clone();
        current[first_value_range.clone()].copy_from_slice(&0x108_u64.to_le_bytes());
        let mut state = state("args", b"output", &[("input.o", &previous)]);
        let input = state.input_files.remove(0);
        let relocation = relocation_record(
            "input.o",
            1,
            42,
            Some(0x1000),
            0x2000,
            Some("duplicate"),
            Some(("input.o", 1, 0x100)),
            0,
            300,
            4,
            object::elf::R_RISCV_JAL,
            0,
        );
        let mut relocations = vec![relocation];

        let patches = relocation_target_patches_for_input(&mut relocations, &input, &current)
            .unwrap()
            .unwrap();

        assert_eq!(patches.output_patches.len(), 1);
        assert!(patches.output_patches[0].deferred_relocation.is_some());
        assert_eq!(patches.output_patches[0].data, vec![0; 4]);
        assert_eq!(relocations[0].written_value, Some(0x1008));
    }

    #[test]
    fn relocation_target_patch_defers_aarch64_instruction_payloads() {
        let (mut previous, first_value_range, _) = duplicate_symbol_name_elf();
        previous[18..20].copy_from_slice(&object::elf::EM_AARCH64.to_le_bytes());
        let mut current = previous.clone();
        current[first_value_range.clone()].copy_from_slice(&0x108_u64.to_le_bytes());
        let mut state = state("args", b"output", &[("input.o", &previous)]);
        let input = state.input_files.remove(0);
        let relocation = relocation_record(
            "input.o",
            1,
            42,
            Some(0x1000),
            0x2000,
            Some("duplicate"),
            Some(("input.o", 1, 0x100)),
            0,
            300,
            4,
            object::elf::R_AARCH64_JUMP26,
            0,
        );
        let mut relocations = vec![relocation];

        let patches = relocation_target_patches_for_input(&mut relocations, &input, &current)
            .unwrap()
            .unwrap();

        assert_eq!(patches.output_patches.len(), 1);
        assert!(patches.output_patches[0].deferred_relocation.is_some());
        assert_eq!(patches.output_patches[0].data, vec![0; 4]);
        assert_eq!(relocations[0].written_value, Some(0x1008));
    }

    #[test]
    fn deferred_riscv_instruction_patches_preserve_non_relocation_bits() {
        let rel_info = riscv64::relocation_type_from_raw(object::elf::R_RISCV_JAL).unwrap();
        let previous_output = 0x0000_006f_u32.to_le_bytes();
        let mut data = vec![0; previous_output.len()];
        let mut expected = previous_output.to_vec();
        rel_info.write_to_buffer(8, &mut expected).unwrap();

        materialize_deferred_relocation_patch(
            &mut data,
            &previous_output,
            DeferredRelocationPatch {
                rel_info,
                previous_written_value: 0,
                written_value: 8,
            },
        )
        .unwrap();

        assert_eq!(data, expected);
        assert_eq!(data[0] & 0x7f, 0x6f);
    }

    #[test]
    fn deferred_riscv_call_patches_cover_both_instruction_words() {
        let rel_info = riscv64::relocation_type_from_raw(object::elf::R_RISCV_CALL_PLT).unwrap();
        let previous_output = [0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00];
        let mut data = vec![0; previous_output.len()];
        let mut expected = previous_output.to_vec();
        rel_info.write_to_buffer(8, &mut expected).unwrap();

        materialize_deferred_relocation_patch(
            &mut data,
            &previous_output,
            DeferredRelocationPatch {
                rel_info,
                previous_written_value: 0,
                written_value: 8,
            },
        )
        .unwrap();

        assert_eq!(data, expected);
    }

    #[test]
    fn deferred_riscv_call_patches_reject_relaxed_output_windows() {
        let rel_info = riscv64::relocation_type_from_raw(object::elf::R_RISCV_CALL_PLT).unwrap();
        let previous_output = 0x0000_006f_u32.to_le_bytes();
        let mut data = vec![0; previous_output.len()];

        let result = materialize_deferred_relocation_patch(
            &mut data,
            &previous_output,
            DeferredRelocationPatch {
                rel_info,
                previous_written_value: 0,
                written_value: 8,
            },
        );

        assert!(matches!(
            result,
            Err(reason) if reason == "deferred relocation patch output size changed"
        ));
    }

    #[test]
    fn relocation_target_patch_rejects_same_offset_section_moves() {
        let (previous, _, _) = duplicate_symbol_name_elf();
        let mut current = previous.clone();
        current[0x7e..0x80].copy_from_slice(&2_u16.to_le_bytes());
        let mut state = state("args", b"output", &[("input.o", &previous)]);
        let input = state.input_files.remove(0);
        let relocation = relocation_record(
            "input.o",
            1,
            42,
            Some(0x1000),
            0x2000,
            Some("duplicate"),
            Some(("input.o", 1, 0x100)),
            0,
            300,
            8,
            1,
            0,
        );
        let mut relocations = vec![relocation];

        let patches =
            relocation_target_patches_for_input(&mut relocations, &input, &current).unwrap();

        assert!(matches!(
            patches,
            Err(reason) if reason == "relocation target moved in input.o"
        ));
    }

    fn duplicate_symbol_name_elf() -> (Vec<u8>, std::ops::Range<usize>, std::ops::Range<usize>) {
        let mut bytes = vec![0; 0x220];
        let shstrtab = b"\0.text\0.symtab\0.strtab\0.shstrtab\0";
        let strtab = b"\0duplicate\0";
        let text_offset = 0x40;
        let symtab_offset = 0x60;
        let strtab_offset = 0xa8;
        let shstrtab_offset = 0xb8;
        let section_headers_offset = 0xe0;
        bytes[strtab_offset..strtab_offset + strtab.len()].copy_from_slice(strtab);
        bytes[shstrtab_offset..shstrtab_offset + shstrtab.len()].copy_from_slice(shstrtab);

        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&object::elf::ET_REL.to_le_bytes());
        bytes[18..20].copy_from_slice(&object::elf::EM_X86_64.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&(section_headers_offset as u64).to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64_u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&5_u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&4_u16.to_le_bytes());

        let write_symbol = |bytes: &mut [u8], index: usize, value: u64| {
            let symbol_offset = symtab_offset + index * 24;
            bytes[symbol_offset..symbol_offset + 4].copy_from_slice(&1_u32.to_le_bytes());
            bytes[symbol_offset + 4] = object::elf::STT_OBJECT;
            bytes[symbol_offset + 6..symbol_offset + 8].copy_from_slice(&1_u16.to_le_bytes());
            bytes[symbol_offset + 8..symbol_offset + 16].copy_from_slice(&value.to_le_bytes());
        };
        write_symbol(&mut bytes, 1, 0x100);
        write_symbol(&mut bytes, 2, 0x200);

        let name_offset = |name: &[u8]| -> u32 {
            shstrtab
                .windows(name.len())
                .position(|window| window == name)
                .unwrap() as u32
        };
        let write_section = |bytes: &mut [u8],
                             index: usize,
                             name: &[u8],
                             ty: u32,
                             flags: u64,
                             offset: u64,
                             size: u64,
                             link: u32,
                             info: u32,
                             align: u64,
                             entsize: u64| {
            let header = section_headers_offset + index * 64;
            bytes[header..header + 4].copy_from_slice(&name_offset(name).to_le_bytes());
            bytes[header + 4..header + 8].copy_from_slice(&ty.to_le_bytes());
            bytes[header + 8..header + 16].copy_from_slice(&flags.to_le_bytes());
            bytes[header + 24..header + 32].copy_from_slice(&offset.to_le_bytes());
            bytes[header + 32..header + 40].copy_from_slice(&size.to_le_bytes());
            bytes[header + 40..header + 44].copy_from_slice(&link.to_le_bytes());
            bytes[header + 44..header + 48].copy_from_slice(&info.to_le_bytes());
            bytes[header + 48..header + 56].copy_from_slice(&align.to_le_bytes());
            bytes[header + 56..header + 64].copy_from_slice(&entsize.to_le_bytes());
        };
        write_section(
            &mut bytes,
            1,
            b".text",
            object::elf::SHT_PROGBITS,
            u64::from(object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR),
            text_offset as u64,
            0x20,
            0,
            0,
            16,
            0,
        );
        write_section(
            &mut bytes,
            2,
            b".symtab",
            object::elf::SHT_SYMTAB,
            0,
            symtab_offset as u64,
            72,
            3,
            3,
            8,
            24,
        );
        write_section(
            &mut bytes,
            3,
            b".strtab",
            object::elf::SHT_STRTAB,
            0,
            strtab_offset as u64,
            strtab.len() as u64,
            0,
            0,
            1,
            0,
        );
        write_section(
            &mut bytes,
            4,
            b".shstrtab",
            object::elf::SHT_STRTAB,
            0,
            shstrtab_offset as u64,
            shstrtab.len() as u64,
            0,
            0,
            1,
            0,
        );

        let first_value_range = symtab_offset + 24 + 8..symtab_offset + 24 + 16;
        let second_value_range = symtab_offset + 48 + 8..symtab_offset + 48 + 16;
        (bytes, first_value_range, second_value_range)
    }

    fn section_record(
        input: &str,
        section_index: u32,
        output_offset: u64,
        size: u64,
    ) -> SectionRecord {
        SectionRecord {
            input_file: hex::encode(input).into(),
            input: hex::encode(input).into(),
            section_index,
            output_offset,
            size,
        }
    }

    fn fde_record(
        input: &str,
        section_index: u32,
        eh_frame_section_index: u32,
        input_offset: u64,
        output_offset: u64,
        size: u64,
    ) -> FdeRecord {
        FdeRecord {
            input_file: hex::encode(input).into(),
            input: hex::encode(input).into(),
            section_index,
            eh_frame_section_index,
            input_offset,
            output_offset,
            size,
        }
    }

    fn dynamic_relocation_record(
        input: &str,
        section_index: u32,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
    ) -> DynamicRelocationRecord {
        DynamicRelocationRecord {
            input_file: hex::encode(input).into(),
            input: hex::encode(input).into(),
            section_index,
            relocation_offset,
            output_offset,
            size,
            output_r_offset: None,
            output_r_info: None,
        }
    }

    fn dynamic_relocation_record_with_output_info(
        input: &str,
        section_index: u32,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
        output_r_offset: u64,
        output_r_info: u64,
    ) -> DynamicRelocationRecord {
        let mut record =
            dynamic_relocation_record(input, section_index, relocation_offset, output_offset, size);
        record.output_r_offset = Some(output_r_offset);
        record.output_r_info = Some(output_r_info);
        record
    }

    fn relocation_record(
        input: &str,
        section_index: u32,
        target_symbol_id: u32,
        written_value: Option<u64>,
        target_value: u64,
        target_name: Option<&str>,
        target: Option<(&str, u32, u64)>,
        relocation_offset: u64,
        output_offset: u64,
        size: u64,
        kind: u32,
        addend: i64,
    ) -> RelocationRecord {
        RelocationRecord {
            target_symbol_id,
            written_value,
            target_value,
            target_name: target_name.map(hex::encode),
            target: target.map(
                |(input, section_index, section_offset)| RelocationTargetRecord {
                    input_file: hex::encode(input).into(),
                    input: hex::encode(input).into(),
                    section_index,
                    section_offset,
                },
            ),
            input_file: hex::encode(input).into(),
            input: hex::encode(input).into(),
            section_index,
            relocation_offset,
            output_offset,
            size,
            kind,
            addend,
        }
    }

    fn current_relocation_as_v25_line(line: &str) -> String {
        let Some(rest) = line.strip_prefix("reloc2\t") else {
            return line.to_owned();
        };
        let fields = rest.split('\t').collect::<Vec<_>>();
        format!(
            "reloc\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            fields[0],
            fields[1],
            fields[2],
            fields[3],
            fields[4],
            fields[5],
            fields[6],
            fields[7],
            fields[9],
            fields[10],
            hex::encode("a.o"),
            hex::encode("a.o"),
            fields[12],
            fields[13],
        )
    }

    fn current_relocation_as_v24_line(line: &str) -> String {
        let Some(rest) = line.strip_prefix("reloc2\t") else {
            return line.to_owned();
        };
        let fields = rest.split('\t').collect::<Vec<_>>();
        format!(
            "reloc\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            fields[0],
            fields[1],
            fields[2],
            fields[3],
            fields[4],
            fields[5],
            fields[6],
            fields[7],
            fields[9],
            fields[10],
        )
    }

    fn current_relocation_as_v23_line(line: &str) -> String {
        let Some(rest) = line.strip_prefix("reloc2\t") else {
            return line.to_owned();
        };
        let fields = rest.split('\t').take(8).collect::<Vec<_>>().join("\t");
        format!("reloc\t{fields}")
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
            .filter(|line| {
                !line.starts_with("build-id-hash\t") && !line.starts_with("sld-version\t")
            })
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

    fn content_hash_with_path_identity(path: &Path, bytes: &[u8]) -> FileContentState {
        let mut content = FileContentState::from_bytes(bytes);
        content.identity = FileIdentity::from_path(path).unwrap();
        content
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
            (name == USER_STATE_DIR_ENV).then(|| OsString::from("/tmp/sld-state"))
        });

        assert_eq!(dir, Some(PathBuf::from("/tmp/sld-state")));
    }

    #[test]
    fn user_state_dir_uses_platform_default() {
        let dir = user_state_dir_from_env(|name| match name {
            "HOME" => Some(OsString::from("/home/sld")),
            _ => None,
        })
        .unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!(
            dir,
            PathBuf::from("/home/sld")
                .join("Library")
                .join("Application Support")
                .join("sld")
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            dir,
            PathBuf::from("/home/sld")
                .join(".local")
                .join("state")
                .join("sld")
        );
    }

    #[test]
    fn user_state_dir_prefers_xdg_state_home_on_non_macos() {
        let dir = user_state_dir_from_env(|name| match name {
            "HOME" => Some(OsString::from("/home/sld")),
            "XDG_STATE_HOME" => Some(OsString::from("/state")),
            _ => None,
        })
        .unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!(
            dir,
            PathBuf::from("/home/sld")
                .join("Library")
                .join("Application Support")
                .join("sld")
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(dir, PathBuf::from("/state").join("sld"));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn input_snapshots_are_isolated_copies() {
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

        std::fs::write(&input, b"changed").unwrap();
        assert_eq!(std::fs::read(&snapshot).unwrap(), b"object");
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn input_identity_refresh_can_target_changed_indices() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.o");
        let second = dir.path().join("second.o");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();

        let mut input_files = vec![
            FileState {
                path: encode_path(&first),
                content: FileContentState::from_bytes(b""),
                patch: None,
            },
            FileState {
                path: encode_path(&second),
                content: FileContentState::from_bytes(b""),
                patch: None,
            },
        ];

        refresh_input_file_identities_at_indices(&mut input_files, [1, 1, 99]);

        assert!(input_files[0].content.identity.is_none());
        assert_eq!(input_files[0].content.len, 0);
        assert!(
            input_files[1]
                .content
                .identity_matches_path(&second)
                .unwrap()
        );
        assert_eq!(input_files[1].content.len, 6);
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
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
        let previous = FileState {
            path: encode_path(&input),
            content: content_hash_with_path_identity(&input, &bytes),
            patch: None,
        };
        let input_ref = encode_path(&input);
        let mut current = bytes.clone();
        current[offset as usize] ^= 1;
        let patch_section = PatchSection {
            input: input_ref,
            section_index: section.index().0 as u32,
            section_name: section.name().ok().map(str::to_owned),
            input_size: size,
            output_offset: 64,
            output_size: size,
            data_hash: None,
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn match_patch_sections_identifies_changed_section() {
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
        let previous = FileState {
            path: encode_path(&input),
            content: content_hash_with_path_identity(&input, &bytes),
            patch: None,
        };
        let input_ref = encode_path(&input);
        let mut current = bytes.clone();
        current[offset as usize] ^= 1;
        let patch_section = PatchSection {
            input: input_ref,
            section_index: section.index().0 as u32,
            section_name: section.name().ok().map(str::to_owned),
            input_size: size,
            output_offset: 64,
            output_size: size,
            data_hash: None,
        };

        let matched = match_patch_sections(&state_dir, &previous, &current, &[patch_section])
            .unwrap()
            .unwrap();

        assert_eq!(matched.sections.len(), 1);
        assert_eq!(matched.changed_sections.len(), 1);
        assert_eq!(
            matched.changed_sections[0].section_index,
            section.index().0 as u32
        );
        assert_eq!(matched.changed_sections[0].input_size, size);
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn match_patch_sections_records_current_section_size_after_growth() {
        let bytes = growable_data_elf();
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("app.incr");
        let input = dir.path().join("input.o");
        std::fs::write(&input, &bytes).unwrap();
        snapshot_input_paths(&state_dir, [input.as_path()]).unwrap();
        let previous = FileState {
            path: encode_path(&input),
            content: content_hash_with_path_identity(&input, &bytes),
            patch: None,
        };
        let input_ref = encode_path(&input);
        let patch_section = PatchSection {
            input: input_ref,
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };
        let mut current = bytes.clone();
        current[0x44] = 5;
        current[0xe0..0xe8].copy_from_slice(&5_u64.to_le_bytes());

        let matched = match_patch_sections(&state_dir, &previous, &current, &[patch_section])
            .unwrap()
            .unwrap();

        assert_eq!(matched.sections.len(), 1);
        assert_eq!(matched.changed_sections.len(), 1);
        assert_eq!(matched.sections[0].current.input_size, 5);
        assert_eq!(matched.changed_sections[0].input_size, 5);
    }

    #[test]
    fn match_patch_sections_uses_current_hashes_for_stable_names() {
        let bytes = growable_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 8,
            data_hash: Some(hash_bytes(&[1, 2, 3, 4])),
        };
        let mut current = bytes.clone();
        current[0x40] = 9;

        let matched =
            match_patch_sections_from_current_hashes(&current, &input_ref, &[patch_section])
                .unwrap()
                .unwrap();

        assert_eq!(matched.sections.len(), 1);
        assert_eq!(matched.sections[0].current.section_index, 1);
        assert_eq!(
            matched.sections[0].current.data_hash.as_deref(),
            Some(hash_bytes(&[9, 2, 3, 4]).as_str())
        );
        assert_eq!(matched.changed_sections.len(), 1);
        assert_eq!(
            matched.changed_sections[0].data_hash.as_deref(),
            Some(hash_bytes(&[9, 2, 3, 4]).as_str())
        );
    }

    #[test]
    fn current_hash_matching_requires_stable_names_and_hashes() {
        let bytes = growable_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let mut patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };

        assert!(
            match_patch_sections_from_current_hashes(
                &bytes,
                &input_ref,
                std::slice::from_ref(&patch_section),
            )
            .unwrap()
            .is_none()
        );

        patch_section.data_hash = Some(hash_bytes(&[1, 2, 3, 4]));
        patch_section.section_name = None;
        assert!(
            match_patch_sections_from_current_hashes(&bytes, &input_ref, &[patch_section])
                .unwrap()
                .is_none()
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
    fn patched_section_records_follow_current_section_identity() {
        let input_file = hex::encode("input.o");
        let input_ref = input_file.clone();
        let unrelated_input = hex::encode("other.o");
        let mut records = vec![
            SectionRecord {
                input_file: input_file.clone().into(),
                input: input_ref.clone().into(),
                section_index: 3,
                output_offset: 64,
                size: 16,
            },
            SectionRecord {
                input_file: unrelated_input.into(),
                input: input_ref.clone().into(),
                section_index: 3,
                output_offset: 64,
                size: 16,
            },
        ];
        let previous = PatchSection {
            input: input_ref.clone(),
            section_index: 3,
            section_name: None,
            input_size: 8,
            output_offset: 64,
            output_size: 16,
            data_hash: None,
        };
        let current = PatchSection {
            input: input_ref.clone(),
            section_index: 7,
            section_name: None,
            input_size: 9,
            output_offset: 64,
            output_size: 16,
            data_hash: None,
        };

        assert!(update_section_records_for_matched_patches(
            &input_file,
            &[MatchedPatchSection { previous, current }],
            &mut records,
        ));

        assert_eq!(records[0].section_index, 7);
        assert_eq!(records[0].size, 16);
        assert_eq!(records[1].section_index, 3);
    }

    #[test]
    fn matched_patch_sections_follow_resolved_current_sections() {
        let input_ref = hex::encode("input.o");
        let previous = PatchSection {
            input: input_ref.clone(),
            section_index: 3,
            section_name: Some(".data.old".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 16,
            data_hash: None,
        };
        let current = PatchSection {
            input: input_ref,
            section_index: 7,
            section_name: Some(".data.old".to_owned()),
            input_size: 9,
            output_offset: 64,
            output_size: 16,
            data_hash: None,
        };
        let mut matched_sections = vec![MatchedPatchSection::same(previous.clone())];

        update_matched_patch_current_sections(&mut matched_sections, &[current.clone()]);

        assert_eq!(
            matched_sections[0].previous.section_index,
            previous.section_index
        );
        assert_eq!(
            matched_sections[0].current.section_index,
            current.section_index
        );
        assert_eq!(matched_sections[0].current.input_size, current.input_size);
    }

    #[test]
    fn patch_sections_for_input_rejects_section_growth_beyond_capacity() {
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
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: section.index().0 as u32,
            section_name: section.name().ok().map(str::to_owned),
            input_size: size,
            output_offset: 64,
            output_size: size - 1,
            data_hash: None,
        };

        assert!(
            patch_sections_for_input(&bytes, &input_ref, [patch_section])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn patch_fingerprint_allows_section_size_growth_within_capacity() {
        let bytes = growable_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };
        let previous_fingerprint = patch_fingerprint(&bytes, &input_ref, [patch_section.clone()])
            .unwrap()
            .unwrap();

        let mut current = bytes.clone();
        current[0x44] = 5;
        current[0xe0..0xe8].copy_from_slice(&5_u64.to_le_bytes());

        assert_eq!(
            patch_fingerprint(&current, &input_ref, [patch_section.clone()])
                .unwrap()
                .unwrap(),
            previous_fingerprint
        );
        let patches = patch_sections_for_input(&current, &input_ref, [patch_section])
            .unwrap()
            .unwrap();

        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].size, 8);
        assert_eq!(patches[0].data, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn patch_fingerprint_rejects_relocation_metadata_changes() {
        let bytes = relocated_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };
        let previous_fingerprint = patch_fingerprint(&bytes, &input_ref, [patch_section.clone()])
            .unwrap()
            .unwrap();

        let mut data_changed = bytes.clone();
        data_changed[0x40] ^= 1;
        assert_eq!(
            patch_fingerprint(&data_changed, &input_ref, [patch_section.clone()])
                .unwrap()
                .unwrap(),
            previous_fingerprint
        );

        let mut relocation_changed = bytes.clone();
        relocation_changed[0x80] ^= 1;
        assert_ne!(
            patch_fingerprint(&relocation_changed, &input_ref, [patch_section])
                .unwrap()
                .unwrap(),
            previous_fingerprint
        );
    }

    #[test]
    fn patch_fingerprint_allows_duplicate_extra_ranges() {
        let bytes = growable_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };

        let fingerprint = patch_fingerprint_with_extra_ranges(
            &bytes,
            &input_ref,
            [patch_section],
            [0x40..0x44, 0x40..0x44],
        )
        .unwrap();

        assert!(fingerprint.is_some());
    }

    #[test]
    fn metadata_only_fingerprint_matches_previous_without_extra_ranges() {
        let bytes = relocated_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };
        let previous_with_extra = patch_fingerprint_with_extra_ranges(
            &bytes,
            &input_ref,
            [patch_section.clone()],
            [0x90..0x98],
        )
        .unwrap()
        .unwrap();

        let mut current = bytes.clone();
        current[0x40] ^= 1;
        let current_without_extra =
            patch_fingerprint(&current, &input_ref, [patch_section.clone()])
                .unwrap()
                .unwrap();
        assert_ne!(previous_with_extra, current_without_extra);

        assert!(
            patch_fingerprint_matches_previous_without_extra_ranges(
                &bytes,
                current_without_extra.as_str(),
                &input_ref,
                &[MatchedPatchSection::same(patch_section)],
            )
            .unwrap()
        );
    }

    #[test]
    fn patch_fingerprint_allows_dynamic_relocation_addend_changes() {
        let bytes = relocated_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };
        let relocation = dynamic_relocation_record("input.o", 1, 4, 300, 24);
        let previous_patches =
            dynamic_relocation_patches_for_input(&bytes, &input_ref, [&relocation]).unwrap();
        let previous_fingerprint = patch_fingerprint_with_extra_ranges(
            &bytes,
            &input_ref,
            [patch_section.clone()],
            previous_patches
                .iter()
                .filter_map(|patch| patch.input_range.clone()),
        )
        .unwrap()
        .unwrap();

        let mut addend_changed = bytes.clone();
        addend_changed[0x90] ^= 1;
        let current_patches =
            dynamic_relocation_patches_for_input(&addend_changed, &input_ref, [&relocation])
                .unwrap();

        assert_eq!(
            patch_fingerprint_with_extra_ranges(
                &addend_changed,
                &input_ref,
                [patch_section],
                current_patches
                    .iter()
                    .filter_map(|patch| patch.input_range.clone()),
            )
            .unwrap()
            .unwrap(),
            previous_fingerprint
        );
        assert_eq!(current_patches.len(), 1);
        assert_eq!(current_patches[0].input_range, Some(0x90..0x98));
        assert_eq!(current_patches[0].patch.output_offset, 300);
        assert_eq!(current_patches[0].patch.size, 24);
        assert_eq!(current_patches[0].patch.preserve_ranges, vec![0..16]);
        assert_eq!(
            &current_patches[0].patch.data[16..24],
            &addend_changed[0x90..0x98]
        );
    }

    #[test]
    fn patch_fingerprint_rejects_dynamic_relocation_offset_changes() {
        let bytes = relocated_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };
        let relocation = dynamic_relocation_record("input.o", 1, 4, 300, 24);
        let previous_patches =
            dynamic_relocation_patches_for_input(&bytes, &input_ref, [&relocation]).unwrap();
        let previous_fingerprint = patch_fingerprint_with_extra_ranges(
            &bytes,
            &input_ref,
            [patch_section.clone()],
            previous_patches
                .iter()
                .filter_map(|patch| patch.input_range.clone()),
        )
        .unwrap()
        .unwrap();

        let mut offset_changed = bytes.clone();
        offset_changed[0x80] ^= 1;
        let current_patches =
            dynamic_relocation_patches_for_input(&offset_changed, &input_ref, [&relocation])
                .unwrap();

        assert_ne!(
            patch_fingerprint_with_extra_ranges(
                &offset_changed,
                &input_ref,
                [patch_section],
                current_patches
                    .iter()
                    .filter_map(|patch| patch.input_range.clone()),
            )
            .unwrap()
            .unwrap(),
            previous_fingerprint
        );
    }

    #[test]
    fn dynamic_relocation_patch_tombstones_missing_relocation_entry() {
        let bytes = growable_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let relocation = dynamic_relocation_record("input.o", 1, 4, 300, 24);

        let patches =
            dynamic_relocation_patches_for_input(&bytes, &input_ref, [&relocation]).unwrap();

        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].record, relocation);
        assert_eq!(patches[0].input_range, None);
        assert_eq!(patches[0].patch.output_offset, 300);
        assert_eq!(patches[0].patch.size, 24);
        assert_eq!(patches[0].patch.data, vec![0; 24]);
        assert!(patches[0].patch.preserve_ranges.is_empty());
    }

    #[test]
    fn dynamic_relocation_patch_restores_recorded_output_info() {
        let bytes = relocated_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let relocation = dynamic_relocation_record_with_output_info(
            "input.o",
            1,
            4,
            300,
            24,
            0x400040,
            0x100000006,
        );

        let patches =
            dynamic_relocation_patches_for_input(&bytes, &input_ref, [&relocation]).unwrap();

        assert_eq!(patches.len(), 1);
        assert_eq!(
            patches[0].patch.preserve_ranges,
            Vec::<std::ops::Range<usize>>::new()
        );
        assert_eq!(&patches[0].patch.data[0..8], &0x400040_u64.to_le_bytes());
        assert_eq!(
            &patches[0].patch.data[8..16],
            &0x100000006_u64.to_le_bytes()
        );
        assert_eq!(&patches[0].patch.data[16..24], &bytes[0x90..0x98]);
    }

    #[test]
    fn dynamic_relocation_patch_uses_free_slot_for_added_relocation() {
        let previous = relocated_data_elf();
        let current = relocated_data_elf_with_added_relocation();
        let input_ref = encode_path(Path::new("input.o"));
        let relocation = dynamic_relocation_record_with_output_info(
            "input.o",
            1,
            4,
            300,
            24,
            0x400040,
            0x100000006,
        );
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };
        let sections = vec![generated_section_record(
            "generated:.rela.dyn.general",
            300,
            48,
        )];

        let patches = added_dynamic_relocation_patches_for_input(
            &current,
            &previous,
            &input_ref,
            &[MatchedPatchSection::same(patch_section.clone())],
            std::slice::from_ref(&relocation),
            &sections,
        );

        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].record.relocation_offset, 6);
        assert_eq!(patches[0].record.output_offset, 324);
        assert_eq!(patches[0].record.output_r_offset, Some(0x400042));
        assert_eq!(patches[0].record.output_r_info, Some(0x100000006));
        assert_eq!(patches[0].input_range, Some(0xa8..0xb0));
        assert_eq!(patches[0].patch.output_offset, 324);
        assert_eq!(&patches[0].patch.data[0..8], &0x400042_u64.to_le_bytes());
        assert_eq!(
            &patches[0].patch.data[8..16],
            &0x100000006_u64.to_le_bytes()
        );
        assert_eq!(&patches[0].patch.data[16..24], &9_i64.to_le_bytes());

        let mut all_patches =
            dynamic_relocation_patches_for_input(&current, &input_ref, [&relocation]).unwrap();
        all_patches.extend(patches);
        assert!(
            object_diff_allows_dynamic_relocation_addition(
                &previous,
                &current,
                &input_ref,
                &[MatchedPatchSection::same(patch_section)],
                &all_patches,
            )
            .unwrap()
        );
    }

    #[test]
    fn object_diff_allows_dynamic_relocation_addition_from_metadata_only_change() {
        let current = relocated_data_elf();
        let mut previous = current.clone();
        let rela_header = 0x100 + 128;
        previous[rela_header + 32..rela_header + 40].copy_from_slice(&0_u64.to_le_bytes());
        let input_ref = encode_path(Path::new("input.o"));
        let relocation = dynamic_relocation_record_with_output_info(
            "input.o",
            1,
            4,
            300,
            24,
            0x400040,
            0x100000006,
        );
        let patches =
            dynamic_relocation_patches_for_input(&current, &input_ref, [&relocation]).unwrap();
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };

        assert!(
            object_diff_allows_dynamic_relocation_addition(
                &previous,
                &current,
                &input_ref,
                &[MatchedPatchSection::same(patch_section)],
                &patches,
            )
            .unwrap()
        );
    }

    #[test]
    fn object_diff_rejects_dynamic_relocation_addition_without_free_slot_info() {
        let current = relocated_data_elf();
        let mut previous = current.clone();
        let rela_header = 0x100 + 128;
        previous[rela_header + 32..rela_header + 40].copy_from_slice(&0_u64.to_le_bytes());
        let input_ref = encode_path(Path::new("input.o"));
        let relocation = dynamic_relocation_record("input.o", 1, 4, 300, 24);
        let patches =
            dynamic_relocation_patches_for_input(&current, &input_ref, [&relocation]).unwrap();
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };

        assert!(
            !object_diff_allows_dynamic_relocation_addition(
                &previous,
                &current,
                &input_ref,
                &[MatchedPatchSection::same(patch_section)],
                &patches,
            )
            .unwrap()
        );
    }

    #[test]
    fn fde_removal_diff_ignores_relocation_metadata_sections() {
        assert!(section_name_is_metadata_for_fde_removal(
            ".rela.text.removed_fde_target"
        ));
        assert!(section_name_is_metadata_for_fde_removal(
            ".rel.text.removed_fde_target"
        ));
        assert!(section_name_is_metadata_for_fde_removal(".rela.eh_frame"));
    }

    #[test]
    fn object_diff_allows_fde_removal_with_surviving_fde_updates() {
        let bytes = eh_frame_relocation_elf(8, -4);
        let input_ref = encode_path(Path::new("input.o"));
        let removed_fde = fde_record("input.o", 2, 2, 0, 300, 16);
        let patches = vec![
            FdeRelocationPatch {
                input_ranges: Vec::new(),
                patch: None,
                eh_frame_hdr_change: Some(EhFrameHdrChange::Remove(removed_fde)),
                record_update: None,
            },
            FdeRelocationPatch {
                input_ranges: Vec::new(),
                patch: Some(SectionPatch {
                    output_offset: 320,
                    size: 16,
                    data: vec![0; 16],
                    deferred_relocation: None,
                    preserve_ranges: Vec::new(),
                    adjustments: Vec::new(),
                }),
                eh_frame_hdr_change: Some(EhFrameHdrChange::Adjust(EhFrameHdrDelta {
                    fde_output_offset: 320,
                    frame_ptr_delta: 4,
                })),
                record_update: None,
            },
        ];

        assert!(object_diff_allows_fde_removal(&bytes, &bytes, &input_ref, &[], &patches).unwrap());
    }

    #[test]
    fn patch_fingerprint_allows_relocation_addend_changes() {
        let bytes = relocated_data_elf();
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 8,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };
        let relocation = relocation_record(
            "input.o",
            1,
            42,
            Some(0x1000),
            0x1000,
            None,
            None,
            4,
            300,
            8,
            1,
            2,
        );
        let previous_ranges =
            relocation_addend_ranges_for_input(&bytes, &input_ref, [&relocation]).unwrap();
        let previous_fingerprint = patch_fingerprint_with_extra_ranges(
            &bytes,
            &input_ref,
            [patch_section.clone()],
            previous_ranges,
        )
        .unwrap()
        .unwrap();

        let mut addend_changed = bytes.clone();
        addend_changed[0x90..0x98].copy_from_slice(&5_i64.to_le_bytes());
        let mut state = state("args", b"output", &[("input.o", &bytes)]);
        let input = state.input_files.remove(0);
        let mut relocations = vec![relocation];
        let patches = relocation_addend_patches_for_input(
            &mut relocations,
            &input,
            &addend_changed,
            None,
            &[],
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            patch_fingerprint_with_extra_ranges(
                &addend_changed,
                &input_ref,
                [patch_section],
                patches.input_ranges.clone(),
            )
            .unwrap()
            .unwrap(),
            previous_fingerprint
        );
        assert_eq!(patches.input_ranges, vec![0x90..0x98]);
        assert_eq!(patches.output_patches.len(), 1);
        assert_eq!(patches.output_patches[0].output_offset, 300);
        assert_eq!(patches.output_patches[0].size, 8);
        assert_eq!(
            patches.output_patches[0].data,
            0x1003_u64.to_le_bytes().to_vec()
        );
        assert_eq!(relocations[0].addend, 5);
        assert_eq!(relocations[0].written_value, Some(0x1003));
    }

    #[test]
    fn dynamic_relocation_addend_does_not_patch_output_data_word() {
        let bytes = relocated_data_elf();
        let mut addend_changed = bytes.clone();
        addend_changed[0x90..0x98].copy_from_slice(&5_i64.to_le_bytes());
        let mut state = state("args", b"output", &[("input.o", &bytes)]);
        let input = state.input_files.remove(0);
        let relocation = relocation_record(
            "input.o",
            1,
            42,
            Some(0x1000),
            0x1000,
            None,
            None,
            4,
            300,
            8,
            1,
            2,
        );
        let dynamic_relocation = dynamic_relocation_record("input.o", 1, 4, 400, 24);
        let mut relocations = vec![relocation];
        let patches = relocation_addend_patches_for_input(
            &mut relocations,
            &input,
            &addend_changed,
            None,
            &[dynamic_relocation],
        )
        .unwrap()
        .unwrap();

        assert_eq!(patches.input_ranges, vec![0x90..0x98]);
        assert!(patches.output_patches.is_empty());
        assert_eq!(relocations[0].addend, 5);
        assert_eq!(relocations[0].written_value, Some(0x1000));
    }

    #[test]
    fn relocation_addend_patch_ignores_unchanged_raw_addends_for_unsupported_sizes() {
        let mut previous = relocated_data_elf();
        previous[0x88..0x90]
            .copy_from_slice(&u64::from(object::elf::R_X86_64_GOTPCREL).to_le_bytes());
        previous[0x90..0x98].copy_from_slice(&(-4_i64).to_le_bytes());
        let current = previous.clone();
        let mut state = state("args", b"output", &[("input.o", &previous)]);
        let input = state.input_files.remove(0);
        let relocation = relocation_record(
            "input.o",
            1,
            42,
            Some(0x1000),
            0x1000,
            None,
            None,
            4,
            300,
            4,
            42,
            0,
        );

        let mut relocations = vec![relocation.clone()];
        let patches = relocation_addend_patches_for_input(
            &mut relocations,
            &input,
            &current,
            Some(&previous),
            &[],
        )
        .unwrap()
        .unwrap();

        assert_eq!(patches.input_ranges, vec![0x90..0x98]);
        assert!(patches.output_patches.is_empty());
        assert_eq!(relocations[0].addend, 0);

        let mut relocations = vec![relocation];
        let err = match relocation_addend_patches_for_input(
            &mut relocations,
            &input,
            &current,
            None,
            &[],
        )
        .unwrap()
        {
            Ok(_) => panic!("unsupported relocation addend unexpectedly patched"),
            Err(err) => err,
        };
        assert!(err.contains("unsupported relocation addend patch size"));
    }

    #[test]
    fn patch_fingerprint_allows_fde_relocation_addend_changes() {
        let previous = eh_frame_relocation_elf(8, -4);
        let current = eh_frame_relocation_elf(8, 2);
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".text".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 4,
            data_hash: None,
        };
        let fde = fde_record("input.o", 1, 2, 0, 300, 16);
        let previous_ranges =
            fde_patch_input_ranges_for_input(&previous, &input_ref, [&fde]).unwrap();
        let previous_fingerprint = patch_fingerprint_with_extra_ranges(
            &previous,
            &input_ref,
            [patch_section.clone()],
            previous_ranges.into_iter(),
        )
        .unwrap()
        .unwrap();
        let current_patches =
            fde_relocation_patches_for_input(&current, &previous, &input_ref, [&fde]).unwrap();

        assert_eq!(
            patch_fingerprint_with_extra_ranges(
                &current,
                &input_ref,
                [patch_section],
                current_patches
                    .iter()
                    .flat_map(|patch| patch.input_ranges.iter().cloned()),
            )
            .unwrap()
            .unwrap(),
            previous_fingerprint
        );
        assert_eq!(current_patches.len(), 1);
        assert_eq!(
            current_patches[0].input_ranges,
            vec![0x48..0x58, 0x58 + 16..0x58 + 24]
        );
        let patch = current_patches[0].patch.as_ref().unwrap();
        assert_eq!(patch.output_offset, 300);
        assert_eq!(patch.size, 16);
        assert_eq!(patch.preserve_ranges, vec![4..8, 8..12]);
        assert_eq!(patch.adjustments.len(), 1);
        assert_eq!(patch.adjustments[0].range, 8..12);
        assert_eq!(patch.adjustments[0].addend_delta, 6);
        assert!(current_patches[0].record_update.is_none());
        assert!(matches!(
            current_patches[0].eh_frame_hdr_change,
            Some(EhFrameHdrChange::Adjust(_))
        ));
    }

    #[test]
    fn fde_relocation_patches_follow_current_eh_frame_section_name() {
        let previous = eh_frame_relocation_elf(8, -4);
        let current = eh_frame_relocation_elf_with_shifted_eh_frame_index(8, -4);
        let input_ref = encode_path(Path::new("input.o"));
        let fde = fde_record("input.o", 1, 2, 0, 300, 16);

        let current_patches =
            fde_relocation_patches_for_input(&current, &previous, &input_ref, [&fde]).unwrap();

        assert_eq!(current_patches.len(), 1);
        assert_eq!(
            current_patches[0].input_ranges,
            vec![0x48..0x58, 0x58 + 16..0x58 + 24]
        );
        assert!(current_patches[0].patch.is_none());
        assert!(current_patches[0].eh_frame_hdr_change.is_none());
    }

    #[test]
    fn fde_relocation_patches_follow_current_fde_offset() {
        let previous = eh_frame_two_fdes_same_section_elf();
        let current = eh_frame_relocation_elf(8, 2);
        let input_ref = encode_path(Path::new("input.o"));
        let fde = fde_record("input.o", 1, 2, 16, 300, 16);

        let current_patches =
            fde_relocation_patches_for_input(&current, &previous, &input_ref, [&fde]).unwrap();

        assert_eq!(current_patches.len(), 1);
        assert_eq!(
            current_patches[0].input_ranges,
            vec![0x48..0x58, 0x58 + 16..0x58 + 24]
        );
        let patch = current_patches[0].patch.as_ref().unwrap();
        assert_eq!(patch.output_offset, 300);
        assert_eq!(patch.size, 16);
        assert_eq!(patch.preserve_ranges, vec![4..8, 8..12]);
        assert_eq!(patch.adjustments.len(), 1);
        assert_eq!(patch.adjustments[0].range, 8..12);
        assert_eq!(patch.adjustments[0].addend_delta, 6);
        let update = current_patches[0].record_update.as_ref().unwrap();
        assert_eq!(update.previous.input_offset, 16);
        assert_eq!(update.current.input_offset, 0);
        assert!(matches!(
            current_patches[0].eh_frame_hdr_change,
            Some(EhFrameHdrChange::Adjust(_))
        ));
    }

    #[test]
    fn updated_fde_records_replace_previous_offsets() {
        let previous = fde_record("input.o", 1, 2, 16, 300, 16);
        let mut current = previous.clone();
        current.input_offset = 0;
        let mut fdes = vec![previous.clone()];

        update_fde_records(
            &mut fdes,
            vec![FdeRecordUpdate {
                previous,
                current: current.clone(),
            }],
        );

        assert_eq!(fdes, vec![current]);
    }

    #[test]
    fn fde_match_can_use_input_offset_with_multiple_fdes_per_section() {
        let bytes = eh_frame_two_fdes_same_section_elf();
        let section_headers = elf_section_headers(&bytes).unwrap();

        assert_eq!(
            fde_input_range_for_target_section(&bytes, &section_headers, 2, 1)
                .unwrap()
                .1,
            0
        );
        assert_eq!(
            fde_input_range_for_target_section_at_offset(&bytes, &section_headers, 2, 1, 16)
                .unwrap()
                .1,
            16
        );
    }

    #[test]
    fn patch_fingerprint_allows_fde_content_changes() {
        let previous = eh_frame_relocation_elf(8, -4);
        let mut current = previous.clone();
        current[0x54] = 8;
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".text".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 4,
            data_hash: None,
        };
        let fde = fde_record("input.o", 1, 2, 0, 300, 16);
        let previous_ranges =
            fde_patch_input_ranges_for_input(&previous, &input_ref, [&fde]).unwrap();
        let previous_fingerprint = patch_fingerprint_with_extra_ranges(
            &previous,
            &input_ref,
            [patch_section.clone()],
            previous_ranges.into_iter(),
        )
        .unwrap()
        .unwrap();
        let current_patches =
            fde_relocation_patches_for_input(&current, &previous, &input_ref, [&fde]).unwrap();

        assert_eq!(
            patch_fingerprint_with_extra_ranges(
                &current,
                &input_ref,
                [patch_section],
                current_patches
                    .iter()
                    .flat_map(|patch| patch.input_ranges.iter().cloned()),
            )
            .unwrap()
            .unwrap(),
            previous_fingerprint
        );
        assert_eq!(current_patches.len(), 1);
        let patch = current_patches[0].patch.as_ref().unwrap();
        assert_eq!(&patch.data[12..16], &[8, 0, 0, 0]);
        assert_eq!(patch.preserve_ranges, vec![4..8, 8..12]);
        assert!(patch.adjustments.is_empty());
        assert!(current_patches[0].eh_frame_hdr_change.is_none());
    }

    #[test]
    fn patch_fingerprint_rejects_fde_relocation_offset_changes() {
        let previous = eh_frame_relocation_elf(8, -4);
        let current = eh_frame_relocation_elf(12, -4);
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".text".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 4,
            data_hash: None,
        };
        let fde = fde_record("input.o", 1, 2, 0, 300, 16);
        let previous_ranges =
            fde_patch_input_ranges_for_input(&previous, &input_ref, [&fde]).unwrap();
        let previous_fingerprint = patch_fingerprint_with_extra_ranges(
            &previous,
            &input_ref,
            [patch_section.clone()],
            previous_ranges.into_iter(),
        )
        .unwrap()
        .unwrap();
        let current_patches =
            fde_relocation_patches_for_input(&current, &previous, &input_ref, [&fde]).unwrap();

        assert_eq!(current_patches.len(), 1);
        assert!(current_patches[0].input_ranges.is_empty());
        assert!(current_patches[0].patch.is_none());
        assert_ne!(
            patch_fingerprint_with_extra_ranges(
                &current,
                &input_ref,
                [patch_section],
                current_patches
                    .iter()
                    .flat_map(|patch| patch.input_ranges.iter().cloned()),
            )
            .unwrap()
            .unwrap(),
            previous_fingerprint
        );
    }

    #[test]
    fn patch_fingerprint_rejects_fde_relocation_field_overflow() {
        let previous = eh_frame_relocation_elf(14, -4);
        let current = eh_frame_relocation_elf(14, 2);
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".text".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 4,
            data_hash: None,
        };
        let fde = fde_record("input.o", 1, 2, 0, 300, 16);
        let previous_ranges =
            fde_patch_input_ranges_for_input(&previous, &input_ref, [&fde]).unwrap();
        let previous_fingerprint = patch_fingerprint_with_extra_ranges(
            &previous,
            &input_ref,
            [patch_section.clone()],
            previous_ranges.into_iter(),
        )
        .unwrap()
        .unwrap();
        let current_patches =
            fde_relocation_patches_for_input(&current, &previous, &input_ref, [&fde]).unwrap();

        assert_eq!(current_patches.len(), 1);
        assert!(current_patches[0].input_ranges.is_empty());
        assert!(current_patches[0].patch.is_none());
        assert_ne!(
            patch_fingerprint_with_extra_ranges(
                &current,
                &input_ref,
                [patch_section],
                current_patches
                    .iter()
                    .flat_map(|patch| patch.input_ranges.iter().cloned()),
            )
            .unwrap()
            .unwrap(),
            previous_fingerprint
        );
    }

    #[test]
    fn eh_frame_hdr_patch_updates_matching_fde_entry() {
        let output = eh_frame_hdr_output_elf(&[(-48, 0x50), (-16, 0x60)]);
        let patches = eh_frame_hdr_patches_for_fde_changes(
            &output,
            &[EhFrameHdrChange::Adjust(EhFrameHdrDelta {
                fde_output_offset: 0x50,
                frame_ptr_delta: 6,
            })],
        )
        .unwrap()
        .unwrap();

        assert_eq!(patches.len(), 1);
        assert_eq!(
            patches[0].output_offset,
            (0x80 + std::mem::size_of::<crate::elf::EhFrameHdr>()) as u64
        );
        assert_eq!(patches[0].size, 4);
        assert_eq!(patches[0].data, (-42_i32).to_le_bytes());
        assert!(patches[0].preserve_ranges.is_empty());
        assert!(patches[0].adjustments.is_empty());
    }

    #[test]
    fn eh_frame_hdr_patch_rejects_unsorted_result() {
        let output = eh_frame_hdr_output_elf(&[(-48, 0x50), (-16, 0x60)]);
        let result = eh_frame_hdr_patches_for_fde_changes(
            &output,
            &[EhFrameHdrChange::Adjust(EhFrameHdrDelta {
                fde_output_offset: 0x50,
                frame_ptr_delta: 40,
            })],
        )
        .unwrap();
        let Err(error) = result else {
            panic!("expected .eh_frame_hdr patch to be rejected");
        };

        assert!(error.contains("would no longer be sorted"));
    }

    #[test]
    fn eh_frame_hdr_patch_rejects_missing_fde_entry() {
        let output = eh_frame_hdr_output_elf(&[(-48, 0x50), (-16, 0x60)]);
        let result = eh_frame_hdr_patches_for_fde_changes(
            &output,
            &[EhFrameHdrChange::Adjust(EhFrameHdrDelta {
                fde_output_offset: 0x70,
                frame_ptr_delta: 6,
            })],
        )
        .unwrap();
        let Err(error) = result else {
            panic!("expected .eh_frame_hdr patch to be rejected");
        };

        assert!(error.contains("could not find .eh_frame_hdr entry"));
    }

    #[test]
    fn eh_frame_hdr_patch_removes_fde_entry() {
        let output = eh_frame_hdr_output_elf(&[(-48, 0x50), (-16, 0x60), (8, 0x70)]);
        let patches = eh_frame_hdr_patches_for_fde_changes(
            &output,
            &[EhFrameHdrChange::Remove(fde_record(
                "input.o", 1, 2, 0, 0x60, 16,
            ))],
        )
        .unwrap()
        .unwrap();

        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].output_offset, 0x88);
        assert_eq!(patches[0].size, 4);
        assert_eq!(patches[0].data, 2_u32.to_le_bytes());
        assert_eq!(
            patches[1].output_offset,
            (0x80 + std::mem::size_of::<crate::elf::EhFrameHdr>() + 8) as u64
        );
        assert_eq!(patches[1].size, 16);
        assert_eq!(
            patches[1].data,
            [
                8_i32.to_le_bytes().as_slice(),
                (-16_i32).to_le_bytes().as_slice(),
                &[0, 0, 0, 0, 0, 0, 0, 0],
            ]
            .concat()
        );
    }

    #[test]
    fn eh_frame_hdr_patch_adds_fde_entry() {
        let output = eh_frame_hdr_output_elf_with_capacity(&[(-48, 0x50), (8, 0x70)], 3);
        let patches = eh_frame_hdr_patches_for_fde_changes(
            &output,
            &[EhFrameHdrChange::Add(EhFrameHdrEntryPatch {
                frame_ptr: -16,
                frame_info_ptr: -32,
            })],
        )
        .unwrap()
        .unwrap();

        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].output_offset, 0x88);
        assert_eq!(patches[0].size, 4);
        assert_eq!(patches[0].data, 3_u32.to_le_bytes());
        assert_eq!(
            patches[1].output_offset,
            (0x80 + std::mem::size_of::<crate::elf::EhFrameHdr>()) as u64
        );
        assert_eq!(patches[1].size, 24);
        assert_eq!(
            patches[1].data,
            [
                (-48_i32).to_le_bytes().as_slice(),
                (-48_i32).to_le_bytes().as_slice(),
                (-16_i32).to_le_bytes().as_slice(),
                (-32_i32).to_le_bytes().as_slice(),
                8_i32.to_le_bytes().as_slice(),
                (-16_i32).to_le_bytes().as_slice(),
            ]
            .concat()
        );
    }

    #[test]
    fn eh_frame_hdr_patch_rejects_added_fde_without_capacity() {
        let output = eh_frame_hdr_output_elf(&[(-48, 0x50), (8, 0x70)]);
        let result = eh_frame_hdr_patches_for_fde_changes(
            &output,
            &[EhFrameHdrChange::Add(EhFrameHdrEntryPatch {
                frame_ptr: -16,
                frame_info_ptr: -32,
            })],
        )
        .unwrap();
        let Err(error) = result else {
            panic!("expected .eh_frame_hdr addition to be rejected");
        };

        assert!(error.contains("no free .eh_frame_hdr entries"));
    }

    #[test]
    fn fde_add_patch_uses_free_eh_frame_tail() {
        let mut output = eh_frame_hdr_output_elf_with_capacity(&[(-48, 0x48)], 2);
        output[0x40..0x48].copy_from_slice(&[
            4, 0, 0, 0, // CIE length
            0, 0, 0, 0, // CIE id
        ]);
        output[0x48..0x58].copy_from_slice(&[
            12, 0, 0, 0, // FDE length
            12, 0, 0, 0, // output CIE pointer
            0, 0, 0, 0, // relocated pc begin
            4, 0, 0, 0, // pc range
        ]);

        let previous = vec![fde_record("input.o", 1, 2, 0, 0x48, 16)];
        let candidate = FdeAddCandidate {
            input_ranges: vec![0..16],
            input_file: "input.o".to_owned(),
            input: "input.o".to_owned(),
            target_section_index: 1,
            eh_frame_section_index: 2,
            input_offset: 16,
            target_section_offset: 0,
            target_output_offset: 0x40,
            fde_data: vec![12, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0],
            pc_begin_range: 8..12,
            cie_input_offset: 0,
            cie_reference_fde_output_offset: 0x48,
        };

        let resolved = fde_add_patches_for_output(&output, &[candidate], &previous)
            .unwrap()
            .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].record.output_offset, 0x58);
        assert_eq!(resolved[0].record.size, 16);
        assert_eq!(resolved[0].patch.output_offset, 0x58);
        assert_eq!(resolved[0].patch.size, 20);
        assert_eq!(&resolved[0].patch.data[4..8], &28_u32.to_le_bytes());
        assert_eq!(&resolved[0].patch.data[8..12], &(-32_i32).to_le_bytes());
        assert_eq!(&resolved[0].patch.data[16..20], &0_u32.to_le_bytes());
        assert!(matches!(
            resolved[0].eh_frame_hdr_change,
            Some(EhFrameHdrChange::Add(EhFrameHdrEntryPatch {
                frame_ptr: -64,
                frame_info_ptr: -40,
            }))
        ));
    }

    #[test]
    fn fde_add_patch_rejects_without_free_eh_frame_tail() {
        let mut output = eh_frame_hdr_output_elf_with_capacity(&[(-48, 0x48)], 2);
        output[0x40..0x48].copy_from_slice(&[
            4, 0, 0, 0, // CIE length
            0, 0, 0, 0, // CIE id
        ]);
        output[0x48..0x58].copy_from_slice(&[
            12, 0, 0, 0, // FDE length
            12, 0, 0, 0, // output CIE pointer
            0, 0, 0, 0, // relocated pc begin
            4, 0, 0, 0, // pc range
        ]);
        output[0x160..0x168].copy_from_slice(&0x1c_u64.to_le_bytes());

        let previous = vec![fde_record("input.o", 1, 2, 0, 0x48, 16)];
        let candidate = FdeAddCandidate {
            input_ranges: vec![0..16],
            input_file: "input.o".to_owned(),
            input: "input.o".to_owned(),
            target_section_index: 1,
            eh_frame_section_index: 2,
            input_offset: 16,
            target_section_offset: 0,
            target_output_offset: 0x40,
            fde_data: vec![12, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0],
            pc_begin_range: 8..12,
            cie_input_offset: 0,
            cie_reference_fde_output_offset: 0x48,
        };

        let result = fde_add_patches_for_output(&output, &[candidate], &previous).unwrap();
        let Err(error) = result else {
            panic!("expected .eh_frame addition to be rejected");
        };

        assert!(error.contains("no free .eh_frame space"));
    }

    #[test]
    fn resolve_current_patch_sections_updates_section_size_after_growth() {
        let mut bytes = growable_data_elf();
        bytes[0x44] = 5;
        bytes[0xe0..0xe8].copy_from_slice(&5_u64.to_le_bytes());
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: 1,
            section_name: Some(".data".to_owned()),
            input_size: 4,
            output_offset: 64,
            output_size: 8,
            data_hash: None,
        };

        let resolved =
            resolve_current_patch_sections(&bytes, &input_ref, [patch_section], std::iter::empty())
                .unwrap()
                .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].section_index, 1);
        assert_eq!(resolved[0].input_size, 5);
        assert_eq!(resolved[0].output_size, 8);
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

    fn relocated_data_elf() -> Vec<u8> {
        let mut bytes = vec![0; 0x220];

        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&1_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&0x100_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64_u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&4_u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&3_u16.to_le_bytes());

        bytes[0x40..0x48].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes[0x80..0x88].copy_from_slice(&4_u64.to_le_bytes());
        bytes[0x88..0x90].copy_from_slice(&1_u64.to_le_bytes());
        bytes[0x90..0x98].copy_from_slice(&2_i64.to_le_bytes());
        bytes[0xa0..0xbc].copy_from_slice(b"\0.data\0.rela.data\0.shstrtab\0");

        let data_header = 0x100 + 64;
        bytes[data_header..data_header + 4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[data_header + 4..data_header + 8].copy_from_slice(&1_u32.to_le_bytes());
        bytes[data_header + 8..data_header + 16].copy_from_slice(&3_u64.to_le_bytes());
        bytes[data_header + 24..data_header + 32].copy_from_slice(&0x40_u64.to_le_bytes());
        bytes[data_header + 32..data_header + 40].copy_from_slice(&8_u64.to_le_bytes());
        bytes[data_header + 48..data_header + 56].copy_from_slice(&8_u64.to_le_bytes());

        let rela_header = 0x100 + 128;
        bytes[rela_header..rela_header + 4].copy_from_slice(&7_u32.to_le_bytes());
        bytes[rela_header + 4..rela_header + 8].copy_from_slice(&4_u32.to_le_bytes());
        bytes[rela_header + 24..rela_header + 32].copy_from_slice(&0x80_u64.to_le_bytes());
        bytes[rela_header + 32..rela_header + 40].copy_from_slice(&24_u64.to_le_bytes());
        bytes[rela_header + 40..rela_header + 44].copy_from_slice(&0_u32.to_le_bytes());
        bytes[rela_header + 44..rela_header + 48].copy_from_slice(&1_u32.to_le_bytes());
        bytes[rela_header + 48..rela_header + 56].copy_from_slice(&8_u64.to_le_bytes());
        bytes[rela_header + 56..rela_header + 64].copy_from_slice(&24_u64.to_le_bytes());

        let shstrtab_header = 0x100 + 192;
        bytes[shstrtab_header..shstrtab_header + 4].copy_from_slice(&18_u32.to_le_bytes());
        bytes[shstrtab_header + 4..shstrtab_header + 8].copy_from_slice(&3_u32.to_le_bytes());
        bytes[shstrtab_header + 24..shstrtab_header + 32].copy_from_slice(&0xa0_u64.to_le_bytes());
        bytes[shstrtab_header + 32..shstrtab_header + 40].copy_from_slice(&28_u64.to_le_bytes());
        bytes[shstrtab_header + 48..shstrtab_header + 56].copy_from_slice(&1_u64.to_le_bytes());

        bytes
    }

    fn relocated_data_elf_with_added_relocation() -> Vec<u8> {
        let mut bytes = relocated_data_elf();
        let shstrtab = bytes[0xa0..0xbc].to_vec();
        bytes[0xa0..0xc0].fill(0);
        bytes[0xc0..0xdc].copy_from_slice(&shstrtab);

        bytes[0x98..0xa0].copy_from_slice(&6_u64.to_le_bytes());
        bytes[0xa0..0xa8].copy_from_slice(&1_u64.to_le_bytes());
        bytes[0xa8..0xb0].copy_from_slice(&9_i64.to_le_bytes());

        let rela_header = 0x100 + 128;
        bytes[rela_header + 32..rela_header + 40].copy_from_slice(&48_u64.to_le_bytes());

        let shstrtab_header = 0x100 + 192;
        bytes[shstrtab_header + 24..shstrtab_header + 32].copy_from_slice(&0xc0_u64.to_le_bytes());

        bytes
    }

    fn eh_frame_hdr_output_elf(entries: &[(i32, u64)]) -> Vec<u8> {
        eh_frame_hdr_output_elf_with_capacity(entries, entries.len())
    }

    fn eh_frame_hdr_output_elf_with_capacity(
        entries: &[(i32, u64)],
        entry_capacity: usize,
    ) -> Vec<u8> {
        assert!(entry_capacity >= entries.len());
        let mut bytes = vec![0; 0x220];
        let shstrtab = b"\0.eh_frame\0.eh_frame_hdr\0.shstrtab\0";
        let shstrtab_offset = 0xd0;
        let eh_frame_offset = 0x40_u64;
        let eh_frame_size = 0x40_u64;
        let eh_frame_address = 0x400040_u64;
        let eh_frame_hdr_offset = 0x80_u64;
        let eh_frame_hdr_address = 0x400080_u64;
        let eh_frame_hdr_size = std::mem::size_of::<crate::elf::EhFrameHdr>() + entry_capacity * 8;
        bytes[shstrtab_offset..shstrtab_offset + shstrtab.len()].copy_from_slice(shstrtab);

        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&object::elf::ET_EXEC.to_le_bytes());
        bytes[18..20].copy_from_slice(&object::elf::EM_X86_64.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&0x100_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64_u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&4_u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&3_u16.to_le_bytes());

        let entry_start =
            eh_frame_hdr_offset as usize + std::mem::size_of::<crate::elf::EhFrameHdr>();
        bytes[eh_frame_hdr_offset as usize + 8..eh_frame_hdr_offset as usize + 12]
            .copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (index, (frame_ptr, fde_output_offset)) in entries.iter().copied().enumerate() {
            let offset = entry_start + index * 8;
            let fde_offset_in_section = fde_output_offset - eh_frame_offset;
            let frame_info_ptr = i32::try_from(
                i128::from(eh_frame_address + fde_offset_in_section)
                    - i128::from(eh_frame_hdr_address),
            )
            .unwrap();
            bytes[offset..offset + 4].copy_from_slice(&frame_ptr.to_le_bytes());
            bytes[offset + 4..offset + 8].copy_from_slice(&frame_info_ptr.to_le_bytes());
        }

        let name_offset = |name: &[u8]| -> u32 {
            shstrtab
                .windows(name.len())
                .position(|window| window == name)
                .unwrap() as u32
        };
        let write_section = |bytes: &mut [u8],
                             index: usize,
                             name: &[u8],
                             ty: u32,
                             flags: u64,
                             address: u64,
                             offset: u64,
                             size: u64,
                             align: u64| {
            let header = 0x100 + index * 64;
            bytes[header..header + 4].copy_from_slice(&name_offset(name).to_le_bytes());
            bytes[header + 4..header + 8].copy_from_slice(&ty.to_le_bytes());
            bytes[header + 8..header + 16].copy_from_slice(&flags.to_le_bytes());
            bytes[header + 16..header + 24].copy_from_slice(&address.to_le_bytes());
            bytes[header + 24..header + 32].copy_from_slice(&offset.to_le_bytes());
            bytes[header + 32..header + 40].copy_from_slice(&size.to_le_bytes());
            bytes[header + 48..header + 56].copy_from_slice(&align.to_le_bytes());
        };
        write_section(
            &mut bytes,
            1,
            b".eh_frame",
            object::elf::SHT_PROGBITS,
            u64::from(object::elf::SHF_ALLOC),
            eh_frame_address,
            eh_frame_offset,
            eh_frame_size,
            8,
        );
        write_section(
            &mut bytes,
            2,
            b".eh_frame_hdr",
            object::elf::SHT_PROGBITS,
            u64::from(object::elf::SHF_ALLOC),
            eh_frame_hdr_address,
            eh_frame_hdr_offset,
            eh_frame_hdr_size as u64,
            4,
        );
        write_section(
            &mut bytes,
            3,
            b".shstrtab",
            object::elf::SHT_STRTAB,
            0,
            0,
            shstrtab_offset as u64,
            shstrtab.len() as u64,
            1,
        );

        bytes
    }

    fn eh_frame_relocation_elf(relocation_offset: u64, addend: i64) -> Vec<u8> {
        let mut bytes = vec![0; 0x288];
        let shstrtab = b"\0.text\0.eh_frame\0.rela.eh_frame\0.symtab\0.strtab\0.shstrtab\0";
        let shstrtab_offset = 0xa8;
        bytes[shstrtab_offset..shstrtab_offset + shstrtab.len()].copy_from_slice(shstrtab);

        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&object::elf::ET_REL.to_le_bytes());
        bytes[18..20].copy_from_slice(&object::elf::EM_X86_64.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&0xc8_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64_u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&7_u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&6_u16.to_le_bytes());

        bytes[0x40..0x44].copy_from_slice(&[0xc3, 0, 0, 0]);
        bytes[0x48..0x58].copy_from_slice(&[
            12, 0, 0, 0, // length
            4, 0, 0, 0, // CIE pointer
            0, 0, 0, 0, // relocated pc begin
            4, 0, 0, 0, // pc range
        ]);
        bytes[0x58..0x60].copy_from_slice(&relocation_offset.to_le_bytes());
        let relocation_info = (1_u64 << 32) | u64::from(object::elf::R_X86_64_PC32);
        bytes[0x60..0x68].copy_from_slice(&relocation_info.to_le_bytes());
        bytes[0x68..0x70].copy_from_slice(&addend.to_le_bytes());

        let symbol_offset = 0x70 + 24;
        bytes[symbol_offset..symbol_offset + 4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[symbol_offset + 4] = object::elf::STT_FUNC;
        bytes[symbol_offset + 6..symbol_offset + 8].copy_from_slice(&1_u16.to_le_bytes());
        bytes[symbol_offset + 16..symbol_offset + 24].copy_from_slice(&4_u64.to_le_bytes());
        bytes[0xa0..0xa6].copy_from_slice(b"\0func\0");

        let name_offset = |name: &[u8]| -> u32 {
            shstrtab
                .windows(name.len())
                .position(|window| window == name)
                .unwrap() as u32
        };
        let write_section = |bytes: &mut [u8],
                             index: usize,
                             name: &[u8],
                             ty: u32,
                             flags: u64,
                             offset: u64,
                             size: u64,
                             link: u32,
                             info: u32,
                             align: u64,
                             entsize: u64| {
            let header = 0xc8 + index * 64;
            bytes[header..header + 4].copy_from_slice(&name_offset(name).to_le_bytes());
            bytes[header + 4..header + 8].copy_from_slice(&ty.to_le_bytes());
            bytes[header + 8..header + 16].copy_from_slice(&flags.to_le_bytes());
            bytes[header + 24..header + 32].copy_from_slice(&offset.to_le_bytes());
            bytes[header + 32..header + 40].copy_from_slice(&size.to_le_bytes());
            bytes[header + 40..header + 44].copy_from_slice(&link.to_le_bytes());
            bytes[header + 44..header + 48].copy_from_slice(&info.to_le_bytes());
            bytes[header + 48..header + 56].copy_from_slice(&align.to_le_bytes());
            bytes[header + 56..header + 64].copy_from_slice(&entsize.to_le_bytes());
        };
        write_section(
            &mut bytes,
            1,
            b".text",
            object::elf::SHT_PROGBITS,
            u64::from(object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR),
            0x40,
            4,
            0,
            0,
            4,
            0,
        );
        write_section(
            &mut bytes,
            2,
            b".eh_frame",
            object::elf::SHT_PROGBITS,
            u64::from(object::elf::SHF_ALLOC),
            0x48,
            16,
            0,
            0,
            8,
            0,
        );
        write_section(
            &mut bytes,
            3,
            b".rela.eh_frame",
            object::elf::SHT_RELA,
            0,
            0x58,
            24,
            4,
            2,
            8,
            crate::elf::RELA_ENTRY_SIZE,
        );
        write_section(
            &mut bytes,
            4,
            b".symtab",
            object::elf::SHT_SYMTAB,
            0,
            0x70,
            48,
            5,
            1,
            8,
            24,
        );
        write_section(
            &mut bytes,
            5,
            b".strtab",
            object::elf::SHT_STRTAB,
            0,
            0xa0,
            6,
            0,
            0,
            1,
            0,
        );
        write_section(
            &mut bytes,
            6,
            b".shstrtab",
            object::elf::SHT_STRTAB,
            0,
            shstrtab_offset as u64,
            shstrtab.len() as u64,
            0,
            0,
            1,
            0,
        );

        bytes
    }

    fn eh_frame_two_fdes_same_section_elf() -> Vec<u8> {
        let mut bytes = vec![0; 0x300];
        let shstrtab = b"\0.text\0.eh_frame\0.rela.eh_frame\0.symtab\0.strtab\0.shstrtab\0";
        let shstrtab_offset = 0xd0;
        bytes[shstrtab_offset..shstrtab_offset + shstrtab.len()].copy_from_slice(shstrtab);

        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&object::elf::ET_REL.to_le_bytes());
        bytes[18..20].copy_from_slice(&object::elf::EM_X86_64.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&0x130_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64_u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&7_u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&6_u16.to_le_bytes());

        bytes[0x40..0x48].copy_from_slice(&[0xc3, 0, 0, 0, 0xc3, 0, 0, 0]);
        bytes[0x48..0x58].copy_from_slice(&[
            12, 0, 0, 0, // length
            4, 0, 0, 0, // CIE pointer
            0, 0, 0, 0, // relocated pc begin
            4, 0, 0, 0, // pc range
        ]);
        bytes[0x58..0x68].copy_from_slice(&[
            12, 0, 0, 0, // length
            4, 0, 0, 0, // CIE pointer
            0, 0, 0, 0, // relocated pc begin
            4, 0, 0, 0, // pc range
        ]);
        let relocation_info = (1_u64 << 32) | u64::from(object::elf::R_X86_64_PC32);
        for (rela_offset, fde_pc_begin) in [(0x68, 8_u64), (0x80, 24_u64)] {
            bytes[rela_offset..rela_offset + 8].copy_from_slice(&fde_pc_begin.to_le_bytes());
            bytes[rela_offset + 8..rela_offset + 16]
                .copy_from_slice(&relocation_info.to_le_bytes());
            bytes[rela_offset + 16..rela_offset + 24].copy_from_slice(&(-4_i64).to_le_bytes());
        }

        let symbol_offset = 0x98 + 24;
        bytes[symbol_offset..symbol_offset + 4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[symbol_offset + 4] = object::elf::STT_FUNC;
        bytes[symbol_offset + 6..symbol_offset + 8].copy_from_slice(&1_u16.to_le_bytes());
        bytes[symbol_offset + 16..symbol_offset + 24].copy_from_slice(&8_u64.to_le_bytes());
        bytes[0xc8..0xce].copy_from_slice(b"\0func\0");

        let name_offset = |name: &[u8]| -> u32 {
            shstrtab
                .windows(name.len())
                .position(|window| window == name)
                .unwrap() as u32
        };
        let write_section = |bytes: &mut [u8],
                             index: usize,
                             name: &[u8],
                             ty: u32,
                             flags: u64,
                             offset: u64,
                             size: u64,
                             link: u32,
                             info: u32,
                             align: u64,
                             entsize: u64| {
            let header = 0x130 + index * 64;
            bytes[header..header + 4].copy_from_slice(&name_offset(name).to_le_bytes());
            bytes[header + 4..header + 8].copy_from_slice(&ty.to_le_bytes());
            bytes[header + 8..header + 16].copy_from_slice(&flags.to_le_bytes());
            bytes[header + 24..header + 32].copy_from_slice(&offset.to_le_bytes());
            bytes[header + 32..header + 40].copy_from_slice(&size.to_le_bytes());
            bytes[header + 40..header + 44].copy_from_slice(&link.to_le_bytes());
            bytes[header + 44..header + 48].copy_from_slice(&info.to_le_bytes());
            bytes[header + 48..header + 56].copy_from_slice(&align.to_le_bytes());
            bytes[header + 56..header + 64].copy_from_slice(&entsize.to_le_bytes());
        };
        write_section(
            &mut bytes,
            1,
            b".text",
            object::elf::SHT_PROGBITS,
            u64::from(object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR),
            0x40,
            8,
            0,
            0,
            4,
            0,
        );
        write_section(
            &mut bytes,
            2,
            b".eh_frame",
            object::elf::SHT_PROGBITS,
            u64::from(object::elf::SHF_ALLOC),
            0x48,
            32,
            0,
            0,
            8,
            0,
        );
        write_section(
            &mut bytes,
            3,
            b".rela.eh_frame",
            object::elf::SHT_RELA,
            0,
            0x68,
            48,
            4,
            2,
            8,
            crate::elf::RELA_ENTRY_SIZE,
        );
        write_section(
            &mut bytes,
            4,
            b".symtab",
            object::elf::SHT_SYMTAB,
            0,
            0x98,
            48,
            5,
            1,
            8,
            24,
        );
        write_section(
            &mut bytes,
            5,
            b".strtab",
            object::elf::SHT_STRTAB,
            0,
            0xc8,
            6,
            0,
            0,
            1,
            0,
        );
        write_section(
            &mut bytes,
            6,
            b".shstrtab",
            object::elf::SHT_STRTAB,
            0,
            shstrtab_offset as u64,
            shstrtab.len() as u64,
            0,
            0,
            1,
            0,
        );

        bytes
    }

    fn eh_frame_relocation_elf_with_shifted_eh_frame_index(
        relocation_offset: u64,
        addend: i64,
    ) -> Vec<u8> {
        let mut bytes = eh_frame_relocation_elf(relocation_offset, addend);
        let eh_frame_header = 0xc8 + 2 * 64;
        let rela_header = 0xc8 + 3 * 64;
        let mut eh_frame = [0; 64];
        let mut rela = [0; 64];
        eh_frame.copy_from_slice(&bytes[eh_frame_header..eh_frame_header + 64]);
        rela.copy_from_slice(&bytes[rela_header..rela_header + 64]);
        bytes[eh_frame_header..eh_frame_header + 64].copy_from_slice(&rela);
        bytes[rela_header..rela_header + 64].copy_from_slice(&eh_frame);
        bytes[eh_frame_header + 44..eh_frame_header + 48].copy_from_slice(&3_u32.to_le_bytes());
        bytes
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
            let Ok(data) = section.data() else {
                continue;
            };
            if size == 0
                || section_direct_patch_preserve_ranges(&section, data.len(), None).is_none()
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
        let input_ref = encode_path(Path::new("input.o"));
        let patch_section = PatchSection {
            input: input_ref.clone(),
            section_index: u32::MAX,
            section_name: Some(section_name),
            input_size: size,
            output_offset: 64,
            output_size: size,
            data_hash: None,
        };

        assert!(
            patch_ranges(&bytes, &input_ref, [patch_section.clone()])
                .unwrap()
                .is_some()
        );
        assert!(
            patch_sections_for_input(&bytes, &input_ref, [patch_section])
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
    fn patch_input_range_decodes_archive_member_offsets() {
        let input_file = hex::encode("libarchive.a");
        let input_ref = hex::encode("libarchive.a\0member.o\012:34");

        assert_eq!(
            patch_input_range(&input_file, &input_ref).unwrap(),
            Some(12..34)
        );
    }

    #[test]
    fn patch_input_range_uses_whole_file_for_direct_inputs() {
        let input_file = hex::encode("main.o");

        assert_eq!(patch_input_range(&input_file, &input_file).unwrap(), None);
    }

    #[test]
    fn patch_input_bytes_finds_archive_member_by_identifier() {
        let mut builder = ar::Builder::new(Vec::new());
        builder
            .append(
                &ar::Header::new(b"padding.o".to_vec(), 4),
                b"xxxx".as_slice(),
            )
            .unwrap();
        builder
            .append(
                &ar::Header::new(b"member.o".to_vec(), 11),
                b"member-data".as_slice(),
            )
            .unwrap();
        let archive = builder.into_inner().unwrap();
        let input_file = hex::encode("libarchive.a");
        let stale_ref = hex::encode("libarchive.a\0member.o\01:5");

        let member = patch_input_bytes(&archive, &input_file, &stale_ref)
            .unwrap()
            .unwrap();

        assert_eq!(member.bytes, b"member-data");
        assert_ne!(member.file_offset, 1);
    }

    #[test]
    fn patch_input_bytes_rejects_ambiguous_archive_member_names() {
        let mut builder = ar::Builder::new(Vec::new());
        builder
            .append(
                &ar::Header::new(b"member.o".to_vec(), 5),
                b"first".as_slice(),
            )
            .unwrap();
        builder
            .append(
                &ar::Header::new(b"member.o".to_vec(), 6),
                b"second".as_slice(),
            )
            .unwrap();
        let archive = builder.into_inner().unwrap();
        let input_file = hex::encode("libarchive.a");
        let input_ref = hex::encode("libarchive.a\0member.o\01:5");

        assert!(
            patch_input_bytes(&archive, &input_file, &input_ref)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn archive_member_identifiers_track_member_set() {
        let mut builder = ar::Builder::new(Vec::new());
        builder
            .append(
                &ar::Header::new(b"first.o".to_vec(), 5),
                b"first".as_slice(),
            )
            .unwrap();
        builder
            .append(
                &ar::Header::new(b"second.o".to_vec(), 6),
                b"second".as_slice(),
            )
            .unwrap();
        let archive = builder.into_inner().unwrap();

        assert_eq!(
            archive_member_identifiers(&archive).unwrap().unwrap(),
            vec![b"first.o".to_vec(), b"second.o".to_vec()]
        );
        assert!(
            archive_member_identifiers(b"not an archive")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    fn archive_member_changes_do_not_match_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("app.incr");
        let input = dir.path().join("libarchive.a");
        let mut previous_builder = ar::Builder::new(Vec::new());
        previous_builder
            .append(
                &ar::Header::new(b"member.o".to_vec(), 6),
                b"member".as_slice(),
            )
            .unwrap();
        let previous_archive = previous_builder.into_inner().unwrap();
        std::fs::write(&input, &previous_archive).unwrap();
        snapshot_input_paths(&state_dir, [input.as_path()]).unwrap();
        let mut member_ref = input.as_os_str().as_encoded_bytes().to_vec();
        member_ref.push(0);
        member_ref.extend_from_slice(b"member.o");
        member_ref.push(0);
        member_ref.extend_from_slice(b"8:14");
        let previous = FileState {
            path: encode_path(&input),
            content: content_hash_with_path_identity(&input, &previous_archive),
            patch: Some(FilePatchState {
                fingerprint: String::new(),
                sections: vec![FilePatchSectionState {
                    input: hex::encode(member_ref),
                    section_index: 0,
                    section_name: None,
                    input_size: 0,
                    output_offset: 0,
                    output_size: 0,
                    data_hash: None,
                }],
                raw_sections: None,
            }),
        };

        let mut current_builder = ar::Builder::new(Vec::new());
        current_builder
            .append(
                &ar::Header::new(b"padding.o".to_vec(), 7),
                b"padding".as_slice(),
            )
            .unwrap();
        current_builder
            .append(
                &ar::Header::new(b"member.o".to_vec(), 6),
                b"member".as_slice(),
            )
            .unwrap();
        let current_archive = current_builder.into_inner().unwrap();

        assert!(!archive_members_match_snapshot(&state_dir, &previous, &current_archive).unwrap());
        assert!(!archive_members_match_snapshot(&state_dir, &previous, b"not an archive").unwrap());
        assert!(archive_members_match_snapshot(&state_dir, &previous, &previous_archive).unwrap());
    }

    #[test]
    fn special_ordered_sections_are_not_directly_patchable() {
        assert!(section_name_allows_direct_patching(b".text.foo"));
        assert!(section_name_allows_direct_patching(b".data.foo"));
        assert!(!section_name_allows_direct_patching(b".eh_frame"));
        assert!(!section_name_allows_direct_patching(b".eh_frame_hdr"));
        assert!(!section_name_allows_direct_patching(b".init"));
        assert!(!section_name_allows_direct_patching(b".fini"));
        assert!(!section_name_allows_direct_patching(b".init_array"));
        assert!(!section_name_allows_direct_patching(b".init_array.100"));
        assert!(!section_name_allows_direct_patching(b".fini_array"));
        assert!(!section_name_allows_direct_patching(b".preinit_array"));
        assert!(!section_name_allows_direct_patching(b".ctors"));
        assert!(!section_name_allows_direct_patching(b".dtors"));
    }

    #[test]
    fn start_stop_sections_are_not_padded() {
        assert!(section_name_allows_incremental_padding(b".text.foo"));
        assert!(section_name_allows_incremental_padding(b".data.foo"));
        assert!(!section_name_allows_incremental_padding(b"foo"));
        assert!(!section_name_allows_incremental_padding(b"bar"));
        assert!(!section_name_allows_incremental_padding(b".init_array"));
        assert!(!section_name_allows_incremental_padding(b".eh_frame"));
    }

    #[test]
    fn patchable_bytes_match_ignores_preserved_relocation_ranges() {
        let input = [1, 2, 3, 4, 5, 6];
        let linked = [1, 9, 9, 4, 8, 6];

        assert!(patchable_bytes_match(&linked, &input, &[1..3, 4..5]));
        assert!(!patchable_bytes_match(&linked, &input, &[1..3]));
        assert!(!patchable_bytes_match(
            &[0, 9, 9, 4, 8, 6],
            &input,
            &[1..3, 4..5]
        ));
    }

    #[test]
    fn persisted_state_round_trips() {
        let mut state = state("args", b"output", &[("a.o", b"a"), ("b.o", b"bbb")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        state.sections.push(generated_section_record(
            "generated:.rela.dyn.general",
            256,
            24,
        ));
        assert_eq!(PersistedState::parse(&state.render()).unwrap(), state);
    }

    #[test]
    fn persisted_state_round_trips_fde_records() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        state.fdes.push(fde_record("a.o", 1, 4, 32, 200, 24));

        let rendered = state.render();

        assert!(rendered.contains("\nfdes\t1\n"));
        assert!(rendered.contains("\nfde\t0\t1\t4\t32\t200\t24\n"));
        assert_eq!(PersistedState::parse(&rendered).unwrap(), state);
    }

    #[test]
    fn persisted_state_round_trips_relocation_records() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        state.relocations.push(relocation_record(
            "a.o",
            1,
            42,
            Some(0x5678),
            0x1234,
            Some("target"),
            Some(("a.o", 2, 16)),
            8,
            300,
            8,
            1,
            -4,
        ));

        let rendered = state.render();

        assert!(rendered.contains("\nrelocs\t1\n"));
        assert!(rendered.contains(
            "\nreloc2\t0\t1\t42\t8\t300\t8\t1\t-4\t22136\t4660\t746172676574\t0\t2\t16\n"
        ));
        assert_eq!(PersistedState::parse(&rendered).unwrap(), state);
    }

    #[test]
    fn v25_state_version_is_accepted_without_written_relocation_value() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.relocations.push(relocation_record(
            "a.o",
            1,
            42,
            Some(0x5678),
            0x1234,
            Some("target"),
            Some(("a.o", 2, 16)),
            8,
            300,
            8,
            1,
            -4,
        ));
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V25, 1)
            .lines()
            .map(current_relocation_as_v25_line)
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.relocations.len(), 1);
        assert_eq!(parsed.relocations[0].written_value, None);
        assert_eq!(parsed.relocations[0].target_value, 0x1234);
        assert_eq!(
            parsed.relocations[0].target_name,
            Some(hex::encode("target"))
        );
        assert_eq!(
            parsed.relocations[0].target,
            Some(RelocationTargetRecord {
                input_file: hex::encode("a.o").into(),
                input: hex::encode("a.o").into(),
                section_index: 2,
                section_offset: 16,
            })
        );
    }

    #[test]
    fn v24_state_version_is_accepted_without_relocation_target_owner() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.relocations.push(relocation_record(
            "a.o",
            1,
            42,
            Some(0x5678),
            0x1234,
            Some("target"),
            Some(("a.o", 2, 16)),
            8,
            300,
            8,
            1,
            -4,
        ));
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V24, 1)
            .lines()
            .map(current_relocation_as_v24_line)
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.relocations.len(), 1);
        assert_eq!(parsed.relocations[0].written_value, None);
        assert_eq!(parsed.relocations[0].target_value, 0x1234);
        assert_eq!(
            parsed.relocations[0].target_name,
            Some(hex::encode("target"))
        );
        assert_eq!(parsed.relocations[0].target, None);
    }

    #[test]
    fn v23_state_version_is_accepted_without_relocation_target_metadata() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.relocations.push(relocation_record(
            "a.o",
            1,
            42,
            Some(0x5678),
            0x1234,
            Some("target"),
            Some(("a.o", 2, 16)),
            8,
            300,
            8,
            1,
            -4,
        ));
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V23, 1)
            .lines()
            .map(current_relocation_as_v23_line)
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.relocations.len(), 1);
        assert_eq!(parsed.relocations[0].written_value, None);
        assert_eq!(parsed.relocations[0].target_value, 0);
        assert_eq!(parsed.relocations[0].target_name, None);
    }

    #[test]
    fn persisted_state_round_trips_dynamic_relocation_records() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        state
            .dynamic_relocations
            .push(dynamic_relocation_record("a.o", 1, 8, 300, 24));
        state
            .dynamic_relocations
            .push(dynamic_relocation_record_with_output_info(
                "a.o",
                1,
                16,
                324,
                24,
                0x400040,
                0x100000006,
            ));

        let rendered = state.render();

        assert!(rendered.contains("\ndynrels\t2\n"));
        assert!(rendered.contains("\ndynrel\t0\t1\t8\t300\t24\n"));
        assert!(rendered.contains("\ndynrel\t0\t1\t16\t324\t24\t4194368\t4294967302\n"));
        assert_eq!(PersistedState::parse(&rendered).unwrap(), state);
    }

    #[test]
    fn v27_state_version_is_accepted_without_dynamic_relocation_output_info() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        state
            .dynamic_relocations
            .push(dynamic_relocation_record("a.o", 1, 8, 300, 24));
        let rendered = state.render().replacen(STATE_VERSION, STATE_VERSION_V27, 1);

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.dynamic_relocations, state.dynamic_relocations);
        assert_eq!(parsed.dynamic_relocations[0].output_r_offset, None);
        assert_eq!(parsed.dynamic_relocations[0].output_r_info, None);
    }

    #[test]
    fn v22_state_version_is_accepted_without_relocation_records() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.fdes.push(fde_record("a.o", 1, 4, 32, 200, 24));
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V22, 1)
            .lines()
            .filter(|line| {
                !line.starts_with("relocs\t")
                    && !line.starts_with("reloc\t")
                    && !line.starts_with("reloc2\t")
            })
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.fdes.len(), 1);
        assert!(parsed.relocations.is_empty());
    }

    #[test]
    fn persisted_state_round_trips_patch_metadata() {
        let mut state = state("args", b"output", &[("a.o", b"a"), ("b.o", b"bbb")]);
        state.input_files[0].patch = Some(FilePatchState {
            fingerprint: "patch-hash".to_owned(),
            sections: vec![
                FilePatchSectionState {
                    input: hex::encode("a.o"),
                    section_index: 1,
                    section_name: Some(".text.foo".to_owned()),
                    input_size: 4,
                    output_offset: 100,
                    output_size: 4,
                    data_hash: Some("text-hash".to_owned()),
                },
                FilePatchSectionState {
                    input: hex::encode("a.o"),
                    section_index: 3,
                    section_name: Some(".data".to_owned()),
                    input_size: 8,
                    output_offset: 112,
                    output_size: 12,
                    data_hash: Some("data-hash".to_owned()),
                },
                FilePatchSectionState {
                    input: hex::encode("a.o"),
                    section_index: 5,
                    section_name: None,
                    input_size: 16,
                    output_offset: 128,
                    output_size: 16,
                    data_hash: None,
                },
            ],
            raw_sections: None,
        });
        state.sections.push(section_record("a.o", 1, 100, 12));

        let rendered = state.render();

        assert!(rendered.contains(&format!(
            "\tpatch-hash\t{}:1:4:100:4:{}:text-hash,{}:3:8:112:12:{}:data-hash,{}:5:16:128:16:-:-\n",
            hex::encode("a.o"),
            hex::encode(".text.foo"),
            hex::encode("a.o"),
            hex::encode(".data"),
            hex::encode("a.o"),
        )));
        assert_eq!(PersistedState::parse(&rendered).unwrap(), state);
    }

    #[test]
    fn metadata_only_input_parse_preserves_raw_patch_sections() {
        let raw_sections = format!(
            "{}:7:11:100:13:{}:data-hash",
            hex::encode("a.o"),
            hex::encode(".text.a")
        );
        let line = format!(
            "input\t{}\t1\t{}\t-\tpatch-hash\t{}",
            hex::encode("a.o"),
            hash_bytes(b"a"),
            raw_sections
        );

        let parsed = parse_input_line(&line, PatchSectionReadMode::PreserveRaw).unwrap();
        let patch = parsed.patch.as_ref().unwrap();

        assert_eq!(patch.fingerprint, "patch-hash");
        assert!(patch.sections.is_empty());
        assert_eq!(patch.raw_sections.as_deref(), Some(raw_sections.as_str()));
        assert_eq!(render_patch_sections(patch), raw_sections);
    }

    #[test]
    fn changed_input_patch_metadata_is_parsed_lazily() {
        let raw_sections = format!(
            "{}:7:11:100:13:{}:data-hash",
            hex::encode("a.o"),
            hex::encode(".text.a")
        );
        let line = format!(
            "input\t{}\t1\t{}\t-\tpatch-hash\t{}",
            hex::encode("a.o"),
            hash_bytes(b"a"),
            raw_sections
        );
        let parsed = parse_input_line(&line, PatchSectionReadMode::PreserveRaw).unwrap();

        let previous = patch_sections_from_previous_state(&parsed, Path::new("a.o")).unwrap();

        assert_eq!(previous.fingerprint, "patch-hash");
        assert_eq!(previous.sections.len(), 1);
        assert_eq!(previous.sections[0].input, hex::encode("a.o"));
        assert_eq!(previous.sections[0].section_index, 7);
        assert_eq!(
            previous.sections[0].section_name.as_deref(),
            Some(".text.a")
        );
        assert_eq!(previous.sections[0].input_size, 11);
        assert_eq!(previous.sections[0].output_offset, 100);
        assert_eq!(previous.sections[0].output_size, 13);
        assert_eq!(previous.sections[0].data_hash.as_deref(), Some("data-hash"));
    }

    #[test]
    fn patch_state_matches_current_section_records() {
        let patch = FilePatchState {
            fingerprint: "patch-hash".to_owned(),
            sections: vec![
                FilePatchSectionState {
                    input: hex::encode("a.o"),
                    section_index: 3,
                    section_name: Some(".text.a".to_owned()),
                    input_size: 4,
                    output_offset: 200,
                    output_size: 8,
                    data_hash: Some("text-hash".to_owned()),
                },
                FilePatchSectionState {
                    input: hex::encode("a.o"),
                    section_index: 1,
                    section_name: Some(".data.a".to_owned()),
                    input_size: 4,
                    output_offset: 100,
                    output_size: 4,
                    data_hash: Some("data-hash".to_owned()),
                },
            ],
            raw_sections: None,
        };
        let first = section_record("a.o", 1, 100, 4);
        let second = section_record("a.o", 3, 200, 8);
        let moved = section_record("a.o", 3, 208, 8);

        assert!(patch_state_matches_section_records(
            &patch,
            &[&second, &first]
        ));
        assert!(!patch_state_matches_section_records(
            &patch,
            &[&first, &moved]
        ));
    }

    #[test]
    fn record_patch_fingerprints_preserves_matching_existing_patch() {
        let arena = colosseum::sync::Arena::new();
        let file_loader = FileLoader::new(&arena);
        let mut output =
            LazyOutputBytes::new(|| panic!("matching patch metadata should not read output bytes"));
        let mut input_files = vec![FileState {
            path: hex::encode("a.o"),
            content: FileContentState::from_bytes(b"a"),
            patch: Some(FilePatchState {
                fingerprint: "patch-hash".to_owned(),
                sections: vec![FilePatchSectionState {
                    input: hex::encode("a.o"),
                    section_index: 1,
                    section_name: Some(".data.a".to_owned()),
                    input_size: 4,
                    output_offset: 100,
                    output_size: 4,
                    data_hash: Some("patch-section-hash".to_owned()),
                }],
                raw_sections: None,
            }),
        }];
        let sections = vec![section_record("a.o", 1, 100, 4)];

        record_patch_fingerprints(
            &mut input_files,
            &file_loader,
            &sections,
            &[],
            &[],
            &[],
            &mut output,
        )
        .unwrap();

        assert_eq!(
            input_files[0].patch.as_ref().unwrap().fingerprint,
            "patch-hash"
        );
    }

    #[test]
    fn record_patch_fingerprints_preserves_matching_existing_patch_with_metadata() {
        let arena = colosseum::sync::Arena::new();
        let file_loader = FileLoader::new(&arena);
        let mut output =
            LazyOutputBytes::new(|| panic!("matching patch metadata should not read output bytes"));
        let mut input_files = vec![FileState {
            path: hex::encode("a.o"),
            content: FileContentState::from_bytes(b"a"),
            patch: Some(FilePatchState {
                fingerprint: "patch-hash".to_owned(),
                sections: vec![FilePatchSectionState {
                    input: hex::encode("a.o"),
                    section_index: 1,
                    section_name: Some(".data.a".to_owned()),
                    input_size: 4,
                    output_offset: 100,
                    output_size: 4,
                    data_hash: Some("patch-section-hash".to_owned()),
                }],
                raw_sections: None,
            }),
        }];
        let sections = vec![section_record("a.o", 1, 100, 4)];
        let relocations = vec![relocation_record(
            "a.o",
            1,
            42,
            Some(0x1000),
            0x1000,
            Some("target"),
            Some(("a.o", 1, 0)),
            0,
            100,
            8,
            1,
            0,
        )];
        let fdes = vec![fde_record("a.o", 1, 2, 0, 200, 24)];
        let dynamic_relocations = vec![dynamic_relocation_record("a.o", 1, 0, 300, 24)];

        record_patch_fingerprints(
            &mut input_files,
            &file_loader,
            &sections,
            &relocations,
            &fdes,
            &dynamic_relocations,
            &mut output,
        )
        .unwrap();

        assert_eq!(
            input_files[0].patch.as_ref().unwrap().fingerprint,
            "patch-hash"
        );
    }

    #[test]
    fn record_patch_fingerprints_clears_stale_patch_without_loaded_input() {
        let arena = colosseum::sync::Arena::new();
        let file_loader = FileLoader::new(&arena);
        let mut output =
            LazyOutputBytes::new(|| panic!("missing loaded input should not read output bytes"));
        let mut input_files = vec![FileState {
            path: hex::encode("a.o"),
            content: FileContentState::from_bytes(b"a"),
            patch: Some(FilePatchState {
                fingerprint: "patch-hash".to_owned(),
                sections: vec![FilePatchSectionState {
                    input: hex::encode("a.o"),
                    section_index: 1,
                    section_name: Some(".data.a".to_owned()),
                    input_size: 4,
                    output_offset: 100,
                    output_size: 4,
                    data_hash: Some("patch-section-hash".to_owned()),
                }],
                raw_sections: None,
            }),
        }];
        let sections = vec![section_record("a.o", 1, 108, 4)];

        record_patch_fingerprints(
            &mut input_files,
            &file_loader,
            &sections,
            &[],
            &[],
            &[],
            &mut output,
        )
        .unwrap();

        assert!(input_files[0].patch.is_none());
    }

    #[test]
    fn v10_patch_metadata_without_section_names_is_accepted() {
        let line = format!(
            "input\t{}\t1\t{}\t-\tpatch-hash\t1:4:100:4,3:8:112:12",
            hex::encode("a.o"),
            hash_bytes(b"a")
        );

        let parsed = parse_input_line(&line, PatchSectionReadMode::Parse).unwrap();
        let sections = parsed.patch.unwrap().sections;

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].input, hex::encode("a.o"));
        assert_eq!(sections[1].input, hex::encode("a.o"));
        assert_eq!(sections[0].section_name, None);
        assert_eq!(sections[1].section_name, None);
    }

    #[test]
    fn v12_state_version_is_accepted_without_sld_version() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V12, 1)
            .lines()
            .filter(|line| !line.starts_with("sld-version\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.sections.len(), 1);
        assert!(parsed.sld_version.is_none());
    }

    #[test]
    fn v13_patch_metadata_is_accepted_without_section_hashes() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.input_files[0].patch = Some(FilePatchState {
            fingerprint: "patch-hash".to_owned(),
            sections: vec![FilePatchSectionState {
                input: hex::encode("a.o"),
                section_index: 1,
                section_name: Some(".data".to_owned()),
                input_size: 4,
                output_offset: 100,
                output_size: 8,
                data_hash: Some("section-hash".to_owned()),
            }],
            raw_sections: None,
        });
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V13, 1)
            .replace(":section-hash", "");

        let parsed = PersistedState::parse(&rendered).unwrap();
        let patch = parsed.input_files[0].patch.as_ref().unwrap();

        assert_eq!(patch.sections.len(), 1);
        assert_eq!(patch.sections[0].section_name.as_deref(), Some(".data"));
        assert_eq!(patch.sections[0].data_hash, None);
    }

    #[test]
    fn current_state_version_requires_sld_version() {
        let rendered = state("args", b"output", &[("a.o", b"a")])
            .render()
            .lines()
            .filter(|line| !line.starts_with("sld-version\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let error = PersistedState::parse(&rendered).unwrap_err();

        assert!(error.to_string().contains("Missing sld version"));
    }

    #[test]
    fn current_state_version_requires_link_options_hash() {
        let rendered = state("args", b"output", &[("a.o", b"a")])
            .render()
            .lines()
            .filter(|line| !line.starts_with("link-options\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let error = PersistedState::parse(&rendered).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Missing incremental link-options")
        );
    }

    #[test]
    fn current_state_version_requires_input_order_hash() {
        let rendered = state("args", b"output", &[("a.o", b"a")])
            .render()
            .lines()
            .filter(|line| !line.starts_with("input-order\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let error = PersistedState::parse(&rendered).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Missing incremental input-order")
        );
    }

    #[test]
    fn v16_state_version_is_accepted_without_input_order_hash() {
        let rendered = state("args", b"output", &[("a.o", b"a")])
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V16, 1)
            .lines()
            .filter(|line| !line.starts_with("input-order\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.input_order_hash, None);
    }

    #[test]
    fn v18_state_version_is_accepted_without_dynamic_relocation_records() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.fdes.push(fde_record("a.o", 1, 4, 32, 200, 24));
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V18, 1)
            .lines()
            .filter(|line| !line.starts_with("dynrels\t") && !line.starts_with("dynrel\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.fdes.len(), 1);
        assert!(parsed.dynamic_relocations.is_empty());
    }

    #[test]
    fn v19_state_version_is_accepted_without_dynamic_relocation_offsets() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state
            .dynamic_relocations
            .push(dynamic_relocation_record("a.o", 1, 0, 300, 24));
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V19, 1)
            .replace("\ndynrel\t0\t1\t0\t300\t24\n", "\ndynrel\t0\t1\t300\t24\n");

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.dynamic_relocations, state.dynamic_relocations);
    }

    #[test]
    fn v15_state_version_is_accepted_without_link_options_hash() {
        let rendered = state("args", b"output", &[("a.o", b"a")])
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V15, 1)
            .lines()
            .filter(|line| !line.starts_with("link-options\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.link_options_hash, None);
    }

    #[test]
    fn old_patch_section_metadata_cannot_patch_changed_inputs() {
        let line = format!(
            "input\t{}\t1\t{}\t-\told-patch-hash\t1:4,3:8",
            hex::encode("a.o"),
            hash_bytes(b"a")
        );

        let parsed = parse_input_line(&line, PatchSectionReadMode::Parse).unwrap();

        assert_eq!(parsed.patch.unwrap().sections, Vec::new());
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn changed_input_patch_rejects_missing_metadata_before_reading_changed_input() {
        let dir = tempfile::tempdir().unwrap();
        let missing_input = dir.path().join("missing.o");
        let previous = PersistedState {
            args_hash: "args".to_owned(),
            link_options_hash: Some("args".to_owned()),
            input_order_hash: Some(input_order_hash_for_paths([missing_input
                .to_str()
                .unwrap()])),
            sld_version: Some("sld-test".to_owned()),
            link_start: None,
            output: FileContentState::from_bytes(b"output"),
            build_id_hashes: None,
            input_files: vec![FileState {
                path: encode_path(&missing_input),
                content: FileContentState::from_bytes(b"previous"),
                patch: None,
            }],
            sections: Vec::new(),
            relocations: Vec::new(),
            fdes: Vec::new(),
            dynamic_relocations: Vec::new(),
            sections_file: None,
        };

        let result = patch_changed_inputs(
            &crate::args::elf::ElfArgs::default(),
            dir.path(),
            previous,
            None,
            true,
            &[(0, missing_input)],
            &[0],
        )
        .unwrap();

        let ChangedInputPatchResult::Unsupported(reason) = result else {
            panic!("changed input was unexpectedly patched");
        };
        assert!(reason.contains("missing patch metadata"));
    }

    #[test]
    fn persisted_state_round_trips_build_id_hashes() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        let output_len = 5 * BUILD_ID_HASH_GROUP_LEN + 100;
        let nodes = build_id_hash_node_count(output_len).unwrap();
        state.build_id_hashes = Some(BuildIdHashState {
            output_len: output_len as u64,
            nodes,
            tree_hash: Some("tree-hash".to_owned()),
        });

        let rendered = state.render();

        assert!(rendered.contains(&format!(
            "\nbuild-id-hash\t{output_len}\t{nodes}\ttree-hash\n"
        )));
        assert_eq!(PersistedState::parse(&rendered).unwrap(), state);
    }

    #[test]
    fn legacy_build_id_hashes_are_accepted_without_tree_hash() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        let output_len = 5 * BUILD_ID_HASH_GROUP_LEN + 100;
        let nodes = build_id_hash_node_count(output_len).unwrap();
        state.build_id_hashes = Some(BuildIdHashState {
            output_len: output_len as u64,
            nodes,
            tree_hash: Some("tree-hash".to_owned()),
        });
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V14, 1)
            .replace("\ttree-hash\n", "\n");

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.build_id_hashes.unwrap().tree_hash, None);
    }

    #[test]
    fn old_patch_fingerprint_without_section_list_is_ignored() {
        let line = format!(
            "input\t{}\t1\t{}\t-\told-patch-hash",
            hex::encode("a.o"),
            hash_bytes(b"a")
        );

        let parsed = parse_input_line(&line, PatchSectionReadMode::Parse).unwrap();

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
    fn record_text_interner_reuses_allocations() {
        let interner = RecordTextInterner::default();
        let first = interner.intern("same-input".to_owned());
        let second = interner.intern("same-input".to_owned());

        assert!(Arc::ptr_eq(&first.0, &second.0));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn read_metadata_skips_missing_sections_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        state.write(dir.path()).unwrap();
        let sections_file = PersistedState::read_metadata(dir.path())
            .unwrap()
            .unwrap()
            .sections_file
            .unwrap();
        std::fs::remove_file(dir.path().join(sections_file)).unwrap();

        let metadata = PersistedState::read_metadata(dir.path()).unwrap().unwrap();
        assert!(metadata.sections.is_empty());
        assert!(PersistedState::read(dir.path()).is_err());
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn read_metadata_preserves_patch_sections_without_parsing_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.input_files[0].patch = Some(FilePatchState {
            fingerprint: "patch-hash".to_owned(),
            sections: vec![FilePatchSectionState {
                input: hex::encode("a.o"),
                section_index: 1,
                section_name: Some(".text.a".to_owned()),
                input_size: 4,
                output_offset: 100,
                output_size: 8,
                data_hash: Some("section-hash".to_owned()),
            }],
            raw_sections: None,
        });
        let raw_sections = render_patch_sections(state.input_files[0].patch.as_ref().unwrap());
        state.sections.push(section_record("a.o", 1, 100, 8));
        state.write(dir.path()).unwrap();

        let metadata = PersistedState::read_metadata(dir.path()).unwrap().unwrap();
        let patch = metadata.input_files[0].patch.as_ref().unwrap();

        assert!(metadata.sections.is_empty());
        assert!(patch.sections.is_empty());
        assert_eq!(patch.raw_sections.as_deref(), Some(raw_sections.as_str()));
        assert!(metadata.render().contains(&raw_sections));

        let full = PersistedState::read(dir.path()).unwrap().unwrap();
        let full_patch = full.input_files[0].patch.as_ref().unwrap();
        assert_eq!(full_patch.sections.len(), 1);
        assert_eq!(full_patch.raw_sections, None);
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn metadata_update_overlay_updates_only_changed_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state("args", b"output", &[("a.o", b"a"), ("b.o", b"b")]);
        state.input_files[0].patch = Some(FilePatchState {
            fingerprint: "old-patch-hash".to_owned(),
            sections: vec![FilePatchSectionState {
                input: hex::encode("a.o"),
                section_index: 1,
                section_name: Some(".text.a".to_owned()),
                input_size: 4,
                output_offset: 100,
                output_size: 8,
                data_hash: Some("old-section-hash".to_owned()),
            }],
            raw_sections: None,
        });
        state.sections.push(section_record("a.o", 1, 100, 8));
        state.write(dir.path()).unwrap();
        let base_index = std::fs::read_to_string(dir.path().join(INDEX_FILE)).unwrap();
        let mut updated = PersistedState::read_metadata(dir.path()).unwrap().unwrap();
        updated.link_start = Some(FileIdentity {
            len: 0,
            dev: 1,
            ino: 2,
            modified_sec: 3,
            modified_nsec: 4,
            changed_sec: 5,
            changed_nsec: 6,
        });
        updated.output = FileContentState::from_bytes(b"new-output");
        updated.input_files[0].content = FileContentState::from_bytes(b"aa");
        updated.input_files[0].patch = Some(FilePatchState {
            fingerprint: "new-patch-hash".to_owned(),
            sections: vec![FilePatchSectionState {
                input: hex::encode("a.o"),
                section_index: 1,
                section_name: Some(".text.a".to_owned()),
                input_size: 4,
                output_offset: 100,
                output_size: 8,
                data_hash: Some("new-section-hash".to_owned()),
            }],
            raw_sections: None,
        });

        updated
            .write_metadata_update_for_inputs(dir.path(), &[0])
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(INDEX_FILE)).unwrap(),
            base_index
        );
        assert!(metadata_update_path(dir.path()).exists());
        let metadata = PersistedState::read_metadata(dir.path()).unwrap().unwrap();
        assert_eq!(metadata.output, updated.output);
        assert_eq!(
            metadata.input_files[0].content,
            updated.input_files[0].content
        );
        assert_eq!(
            metadata.input_files[0].patch.as_ref().unwrap().fingerprint,
            "new-patch-hash"
        );
        assert_eq!(metadata.input_files[1], state.input_files[1]);

        metadata.write_index(dir.path()).unwrap();
        assert!(!metadata_update_path(dir.path()).exists());
    }

    #[test]
    fn metadata_update_indices_include_changed_and_rewritten_inputs() {
        let changed_inputs = vec![
            (2, PathBuf::from("changed-c.o")),
            (0, PathBuf::from("changed-a.o")),
        ];
        let rewritten_inputs = vec![
            (1, PathBuf::from("rewritten-b.o")),
            (2, PathBuf::from("rewritten-c.o")),
        ];

        assert_eq!(
            metadata_update_indices_for_inputs(&changed_inputs, &rewritten_inputs),
            vec![0, 1, 2]
        );
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn reloaded_metadata_refreshes_rewritten_input_identities() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("rewritten.o");
        std::fs::write(&input, b"same").unwrap();
        let input_path = input.to_str().unwrap();
        let mut state = state("args", b"output", &[(input_path, b"same")]);
        state.input_files[0].content = FileContentState {
            len: 4,
            hash: hash_bytes(b"same"),
            identity: Some(identity(4, 1, 2, 3, 4)),
        };
        state.write(dir.path()).unwrap();
        let mut metadata = PersistedState::read_metadata(dir.path()).unwrap().unwrap();

        assert!(
            input_identity_mismatch_reason(&metadata.input_files)
                .unwrap()
                .is_some()
        );

        refresh_rewritten_input_identities(&mut metadata, &[(0, input.clone())]);

        assert_eq!(
            input_identity_mismatch_reason(&metadata.input_files).unwrap(),
            None
        );
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn read_records_for_input_files_filters_sections_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state("args", b"output", &[("a.o", b"a"), ("b.o", b"b")]);
        state.sections.push(section_record("a.o", 1, 100, 8));
        state.sections.push(section_record("b.o", 1, 200, 8));
        state.relocations.push(relocation_record(
            "a.o",
            1,
            4,
            Some(0x1000),
            0x1000,
            Some("target"),
            None,
            0,
            100,
            8,
            1,
            0,
        ));
        state.relocations.push(relocation_record(
            "b.o",
            1,
            4,
            Some(0x2000),
            0x2000,
            Some("target"),
            None,
            0,
            200,
            8,
            1,
            0,
        ));
        state.fdes.push(fde_record("a.o", 1, 2, 0, 300, 24));
        state.fdes.push(fde_record("b.o", 1, 2, 0, 400, 24));
        state
            .dynamic_relocations
            .push(dynamic_relocation_record("a.o", 1, 0, 500, 24));
        state
            .dynamic_relocations
            .push(dynamic_relocation_record("b.o", 1, 0, 600, 24));
        state.write(dir.path()).unwrap();
        let mut metadata = PersistedState::read_metadata(dir.path()).unwrap().unwrap();
        let input_files = [hex::encode("a.o")].into_iter().collect::<HashSet<_>>();

        metadata
            .read_records_for_input_files(dir.path(), &input_files)
            .unwrap();

        assert_eq!(metadata.sections.len(), 1);
        assert_eq!(metadata.relocations.len(), 1);
        assert_eq!(metadata.fdes.len(), 1);
        assert_eq!(metadata.dynamic_relocations.len(), 1);
        assert!(metadata.sections_file.is_some());
        assert!(
            metadata
                .sections
                .iter()
                .all(|record| record.input_file == hex::encode("a.o"))
        );
        assert!(
            metadata
                .relocations
                .iter()
                .all(|record| record.input_file == hex::encode("a.o"))
        );
        assert!(
            metadata
                .fdes
                .iter()
                .all(|record| record.input_file == hex::encode("a.o"))
        );
        assert!(
            metadata
                .dynamic_relocations
                .iter()
                .all(|record| record.input_file == hex::encode("a.o"))
        );
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn read_records_for_input_files_validates_sections_sidecar_hash() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 8));
        state.write(dir.path()).unwrap();
        let sections_file = PersistedState::read_metadata(dir.path())
            .unwrap()
            .unwrap()
            .sections_file
            .unwrap();
        std::fs::write(
            dir.path().join(&sections_file),
            "section-inputs\t0\nsections\t0\n",
        )
        .unwrap();
        let mut metadata = PersistedState::read_metadata(dir.path()).unwrap().unwrap();
        let input_files = [hex::encode("a.o")].into_iter().collect::<HashSet<_>>();

        let error = metadata
            .read_records_for_input_files(dir.path(), &input_files)
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("do not match their content hash")
        );
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn hashed_sections_sidecar_must_match_contents() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        state.write(dir.path()).unwrap();
        let sections_file = PersistedState::read_metadata(dir.path())
            .unwrap()
            .unwrap()
            .sections_file
            .unwrap();
        std::fs::write(
            dir.path().join(&sections_file),
            "section-inputs\t0\nsections\t0\n",
        )
        .unwrap();

        let error = PersistedState::read(dir.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("do not match their content hash")
        );
    }

    #[test]
    fn sections_sidecar_name_must_stay_in_state_dir() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        let rendered = format!(
            "{}sections-file\t../sections\n",
            state.render_header_and_inputs()
        );

        let error = PersistedState::parse(&rendered).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Invalid incremental sections sidecar name")
        );
    }

    #[test]
    fn previous_sections_are_only_needed_for_reuse_capable_modes() {
        assert!(!mode_needs_previous_sections(&IncrementalMode::Disabled));
        assert!(mode_needs_previous_sections(&IncrementalMode::Reuse));
        assert!(mode_needs_previous_sections(&IncrementalMode::Relink {
            reason: "input file changed".to_owned(),
            can_reuse_unchanged_sections: true,
        }));
        assert!(!mode_needs_previous_sections(&IncrementalMode::Relink {
            reason: "linker arguments changed".to_owned(),
            can_reuse_unchanged_sections: false,
        }));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn metadata_update_writes_sections_for_inline_legacy_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        assert!(state.sections_file.is_none());

        state.write_metadata_update(dir.path()).unwrap();

        let sections_file = section_sidecar_file_name(&state.render_sections());
        assert!(dir.path().join(&sections_file).exists());
        let index = std::fs::read_to_string(dir.path().join(INDEX_FILE)).unwrap();
        assert!(index.contains(&format!("\nsections-file\t{sections_file}\n")));
        assert_eq!(
            PersistedState::read(dir.path()).unwrap().unwrap().sections,
            state.sections
        );
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn section_sidecars_are_not_replaced_before_index_update() {
        let dir = tempfile::tempdir().unwrap();
        let mut old_state = state("old-args", b"old-output", &[("a.o", b"a")]);
        old_state.sections.push(section_record("a.o", 1, 100, 12));
        old_state.write(dir.path()).unwrap();
        let old_sections_file = PersistedState::read(dir.path())
            .unwrap()
            .unwrap()
            .sections_file
            .unwrap();

        let mut new_state = state("new-args", b"new-output", &[("b.o", b"b")]);
        new_state.sections.push(section_record("b.o", 7, 900, 16));
        let new_sections = new_state.render_sections();
        let new_sections_file = section_sidecar_file_name(&new_sections);
        new_state
            .write_sections(dir.path(), &new_sections_file, &new_sections)
            .unwrap();

        let read_after_torn_write = PersistedState::read(dir.path()).unwrap().unwrap();
        assert_eq!(read_after_torn_write.sections, old_state.sections);
        assert_eq!(
            read_after_torn_write.sections_file.as_deref(),
            Some(old_sections_file.as_str())
        );
        assert!(dir.path().join(new_sections_file).exists());
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
        assert!(PersistedState::parse("not-sld\n").is_err());
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
    fn file_identity_compares_changed_time() {
        let first = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 3, 5)),
        };
        let changed = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 3, 6)),
        };

        assert_ne!(first, changed);
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
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
    fn file_identity_is_ambiguous_when_timestamp_overlaps_link_start() {
        let link_start = identity(0, 1, 2, 10, 10);
        let before = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 9, 9)),
        };
        let same_tick = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 10, 9)),
        };
        let changed_same_tick = FileContentState {
            len: 4,
            hash: String::new(),
            identity: Some(identity(4, 1, 2, 9, 10)),
        };

        assert!(!before.identity_is_ambiguous_since(Some(&link_start)));
        assert!(same_tick.identity_is_ambiguous_since(Some(&link_start)));
        assert!(changed_same_tick.identity_is_ambiguous_since(Some(&link_start)));
        assert!(!same_tick.identity_is_ambiguous_since(None));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn stable_identity_read_records_matching_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.o");
        std::fs::write(&path, b"abcd").unwrap();

        let (bytes, content) = read_file_with_stable_identity(&path).unwrap().unwrap();

        assert_eq!(bytes, b"abcd");
        assert_eq!(content, FileContentState::from_path(&path).unwrap());
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn input_identity_mismatch_reason_rechecks_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.o");
        std::fs::write(&path, b"abcd").unwrap();
        let input = FileState {
            path: encode_path(&path),
            content: FileContentState::from_path_identity_only(&path).unwrap(),
            patch: None,
        };

        assert!(
            input_identity_mismatch_reason(std::slice::from_ref(&input))
                .unwrap()
                .is_none()
        );

        std::fs::write(&path, b"abcde").unwrap();
        let reason = input_identity_mismatch_reason(&[input]).unwrap().unwrap();

        assert!(reason.contains("input file changed while incremental fast path was running"));
        assert!(reason.contains("input.o"));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn input_content_mismatch_reason_rechecks_changed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.o");
        std::fs::write(&path, b"abcd").unwrap();
        let expected = ExpectedInputContent::from_bytes(&path, b"abcd");

        assert!(input_content_mismatch_reason(std::slice::from_ref(&expected)).is_none());

        std::fs::write(&path, b"wxyz").unwrap();
        let reason = input_content_mismatch_reason(&[expected]).unwrap();

        assert!(reason.contains("input file changed while incremental fast path was running"));
        assert!(reason.contains("input.o"));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn preloading_check_records_link_start_before_depfile_exists() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        let mut args = crate::args::elf::ElfArgs::default();
        args.common.incremental = true;
        args.output = Arc::from(output.as_path());
        args.dependency_file = Some(dir.path().join("out.d"));

        assert!(!maybe_reuse_output_before_loading(&args).unwrap());

        let state_dir = state_dir_for_output(&args.output);
        assert!(link_start_marker_identity(&state_dir).is_some());
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn classifies_reusable_state() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: previous.input_files.clone(),
        };

        assert_eq!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Reuse
        );
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
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
            link_options_hash: "args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: previous.input_files.clone(),
        };

        assert_eq!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Reuse
        );
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn interrupted_update_marker_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        let state_dir = dir.path().join("out.incr");
        std::fs::write(&output, b"output").unwrap();
        mark_incremental_update_started(&state_dir, "test").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: state_dir.clone(),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: previous.input_files.clone(),
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: false,
            } if reason == "previous incremental update did not complete"
        ));

        clear_incremental_update_marker(&state_dir).unwrap();
        assert!(!update_marker_path(&state_dir).exists());
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn changed_args_force_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "new-args".to_owned(),
            link_options_hash: "new-args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "sld-test".to_owned(),
            link_start: None,
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn changed_sld_version_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "new-sld".to_owned(),
            link_start: None,
            input_files: previous.input_files.clone(),
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: false,
            } if reason == "linker version changed"
        ));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn missing_sld_version_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let mut previous = state("args", b"output", &[("a.o", b"a")]);
        previous.sld_version = None;
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: previous.input_files.clone(),
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: false,
            } if reason == "linker version missing from previous state"
        ));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn changed_input_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "sld-test".to_owned(),
            link_start: None,
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn changed_input_list_keeps_unchanged_section_reuse_available() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("old-exact-args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "new-exact-args".to_owned(),
            link_options_hash: "old-exact-args".to_owned(),
            input_order_hash: input_order_hash_for_paths(["a.o", "b.o"]),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: state("new-exact-args", b"output", &[("a.o", b"a"), ("b.o", b"b")])
                .input_files,
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: true,
            } if reason.contains("input file added")
        ));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn removed_input_list_keeps_unchanged_section_reuse_available() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("old-exact-args", b"output", &[("a.o", b"a"), ("b.o", b"b")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "new-exact-args".to_owned(),
            link_options_hash: "old-exact-args".to_owned(),
            input_order_hash: input_order_hash_for_paths(["a.o"]),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: state("new-exact-args", b"output", &[("a.o", b"a")]).input_files,
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: true,
            } if reason.contains("input file removed")
        ));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn reordered_input_list_keeps_unchanged_section_reuse_available() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("old-exact-args", b"output", &[("a.o", b"a"), ("b.o", b"b")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "new-exact-args".to_owned(),
            link_options_hash: "old-exact-args".to_owned(),
            input_order_hash: input_order_hash_for_paths(["b.o", "a.o"]),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: previous.input_files.clone(),
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: true,
            } if reason == "input file order changed"
        ));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn missing_input_order_hash_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let mut previous = state("args", b"output", &[("a.o", b"a")]);
        previous.input_order_hash = None;
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            input_order_hash: input_order_hash_for_paths(["a.o"]),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: previous.input_files.clone(),
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: false,
            } if reason == "input file order missing from previous state"
        ));
    }

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn missing_output_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "sld-test".to_owned(),
            link_start: None,
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

    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    #[test]
    fn changed_output_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"changed").unwrap();
        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            input_order_hash: previous.input_order_hash.clone().unwrap(),
            sld_version: "sld-test".to_owned(),
            link_start: None,
            input_files: previous.input_files.clone(),
        };

        assert!(matches!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Relink {
                reason,
                can_reuse_unchanged_sections: false,
            } if reason == "output file changed since previous link"
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
    fn patchable_sections_must_be_allocated() {
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
        assert!(section_flags_allow_patching(mergeable));
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
                tree_hash: Some(build_id_hash_tree_hash(&tree)),
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
            tree_hash: Some(build_id_hash_tree_hash(&tree)),
        };

        let changed_range = 2 * BUILD_ID_HASH_GROUP_LEN + 100..2 * BUILD_ID_HASH_GROUP_LEN + 110;
        output[changed_range.clone()].copy_from_slice(b"0123456789");
        let changed_chunks = touched_build_id_chunks(&[changed_range], output.len()).unwrap();
        assert!(update_build_id_hash_tree(
            &mut state,
            &mut tree,
            &output,
            &build_id_range,
            &changed_chunks,
        ));
        let mut expected = output;
        expected[build_id_range].fill(0);

        assert_eq!(
            build_id_from_hash_tree(&state, &tree).unwrap(),
            blake3::hash(&expected)
        );
        assert_eq!(
            state.tree_hash.as_deref(),
            Some(build_id_hash_tree_hash(&tree).as_str())
        );
    }

    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    fn build_id_hash_tree_must_match_state_hash() {
        let dir = tempfile::tempdir().unwrap();
        let output_len = 5 * BUILD_ID_HASH_GROUP_LEN + 100;
        let nodes = build_id_hash_node_count(output_len).unwrap();
        let tree = vec![[1; blake3::OUT_LEN]; nodes];
        write_build_id_hash_tree(dir.path(), Some(&tree)).unwrap();
        let state = BuildIdHashState {
            output_len: output_len as u64,
            nodes,
            tree_hash: Some("wrong-hash".to_owned()),
        };

        let error = read_build_id_hash_tree(dir.path(), &state).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not match its recorded hash")
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
                link_options_hash: "args".to_owned(),
                input_order_hash: String::new(),
                sld_version: "sld-test".to_owned(),
                link_start: None,
                input_files: Vec::new(),
            },
            reusable_inputs: [encode_path(Path::new("a.o"))].into_iter().collect(),
            previous_sections: [record].into_iter().collect(),
            previous_relocations: Vec::new(),
            previous_fdes: Vec::new(),
            previous_dynamic_relocations: Vec::new(),
            current_sections: Mutex::new(Vec::new()),
            current_relocations: Mutex::new(Vec::new()),
            current_fdes: Mutex::new(Vec::new()),
            current_dynamic_relocations: Mutex::new(Vec::new()),
            record_texts: RecordTextInterner::default(),
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
                link_options_hash: "args".to_owned(),
                input_order_hash: String::new(),
                sld_version: "sld-test".to_owned(),
                link_start: None,
                input_files: Vec::new(),
            },
            reusable_inputs: HashSet::new(),
            previous_sections: HashSet::new(),
            previous_relocations: Vec::new(),
            previous_fdes: Vec::new(),
            previous_dynamic_relocations: Vec::new(),
            current_sections: Mutex::new(Vec::new()),
            current_relocations: Mutex::new(Vec::new()),
            current_fdes: Mutex::new(Vec::new()),
            current_dynamic_relocations: Mutex::new(Vec::new()),
            record_texts: RecordTextInterner::default(),
            reused_sections: AtomicUsize::new(0),
        };

        assert!(!state.try_reuse_section(input, object::SectionIndex(3), 64, 16, false, true));
        assert!(state.current_sections.lock().unwrap().is_empty());
    }

    #[test]
    fn record_generated_section_records_non_empty_ranges() {
        let state = PreparedState {
            mode: IncrementalMode::Relink {
                reason: "no previous incremental state".to_owned(),
                can_reuse_unchanged_sections: false,
            },
            current: CurrentState {
                state_dir: PathBuf::new(),
                args_hash: "args".to_owned(),
                link_options_hash: "args".to_owned(),
                input_order_hash: String::new(),
                sld_version: "sld-test".to_owned(),
                link_start: None,
                input_files: Vec::new(),
            },
            reusable_inputs: HashSet::new(),
            previous_sections: HashSet::new(),
            previous_relocations: Vec::new(),
            previous_fdes: Vec::new(),
            previous_dynamic_relocations: Vec::new(),
            current_sections: Mutex::new(Vec::new()),
            current_relocations: Mutex::new(Vec::new()),
            current_fdes: Mutex::new(Vec::new()),
            current_dynamic_relocations: Mutex::new(Vec::new()),
            record_texts: RecordTextInterner::default(),
            reused_sections: AtomicUsize::new(0),
        };

        state.record_generated_section("generated:.rela.dyn.general", 256, 24);
        state.record_generated_section("generated:.relr.dyn", 512, 0);

        assert_eq!(
            *state.current_sections.lock().unwrap(),
            vec![generated_section_record(
                "generated:.rela.dyn.general",
                256,
                24
            )]
        );
    }

    #[test]
    fn record_eh_frame_fde_records_non_empty_ranges() {
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
                link_options_hash: "args".to_owned(),
                input_order_hash: String::new(),
                sld_version: "sld-test".to_owned(),
                link_start: None,
                input_files: Vec::new(),
            },
            reusable_inputs: HashSet::new(),
            previous_sections: HashSet::new(),
            previous_relocations: Vec::new(),
            previous_fdes: Vec::new(),
            previous_dynamic_relocations: Vec::new(),
            current_sections: Mutex::new(Vec::new()),
            current_relocations: Mutex::new(Vec::new()),
            current_fdes: Mutex::new(Vec::new()),
            current_dynamic_relocations: Mutex::new(Vec::new()),
            record_texts: RecordTextInterner::default(),
            reused_sections: AtomicUsize::new(0),
        };

        state.record_eh_frame_fde(
            input,
            object::SectionIndex(3),
            object::SectionIndex(5),
            32,
            256,
            24,
        );
        state.record_eh_frame_fde(
            input,
            object::SectionIndex(3),
            object::SectionIndex(5),
            56,
            280,
            0,
        );

        assert_eq!(
            *state.current_fdes.lock().unwrap(),
            vec![FdeRecord::new(
                input,
                object::SectionIndex(3),
                object::SectionIndex(5),
                32,
                256,
                24
            )]
        );
    }

    #[test]
    fn record_relocation_records_non_empty_ranges() {
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
                link_options_hash: "args".to_owned(),
                input_order_hash: String::new(),
                sld_version: "sld-test".to_owned(),
                link_start: None,
                input_files: Vec::new(),
            },
            reusable_inputs: HashSet::new(),
            previous_sections: HashSet::new(),
            previous_relocations: Vec::new(),
            previous_fdes: Vec::new(),
            previous_dynamic_relocations: Vec::new(),
            current_sections: Mutex::new(Vec::new()),
            current_relocations: Mutex::new(Vec::new()),
            current_fdes: Mutex::new(Vec::new()),
            current_dynamic_relocations: Mutex::new(Vec::new()),
            record_texts: RecordTextInterner::default(),
            reused_sections: AtomicUsize::new(0),
        };

        state.record_relocation(
            input,
            object::SectionIndex(3),
            42,
            8,
            256,
            4,
            2,
            -16,
            0x5678,
            0x1234,
            Some(hex::encode("target")),
            Some((input, object::SectionIndex(7), 32)),
        );
        state.record_relocation(
            input,
            object::SectionIndex(3),
            43,
            16,
            280,
            0,
            2,
            0,
            0,
            0x5678,
            None,
            None,
        );

        assert_eq!(
            *state.current_relocations.lock().unwrap(),
            vec![RelocationRecord::new(
                input,
                object::SectionIndex(3),
                42,
                8,
                256,
                4,
                2,
                -16,
                0x5678,
                0x1234,
                Some(hex::encode("target")),
                Some((input, object::SectionIndex(7), 32))
            )]
        );
    }

    #[test]
    fn record_dynamic_relocation_records_non_empty_ranges() {
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
                link_options_hash: "args".to_owned(),
                input_order_hash: String::new(),
                sld_version: "sld-test".to_owned(),
                link_start: None,
                input_files: Vec::new(),
            },
            reusable_inputs: HashSet::new(),
            previous_sections: HashSet::new(),
            previous_relocations: Vec::new(),
            previous_fdes: Vec::new(),
            previous_dynamic_relocations: Vec::new(),
            current_sections: Mutex::new(Vec::new()),
            current_relocations: Mutex::new(Vec::new()),
            current_fdes: Mutex::new(Vec::new()),
            current_dynamic_relocations: Mutex::new(Vec::new()),
            record_texts: RecordTextInterner::default(),
            reused_sections: AtomicUsize::new(0),
        };

        state.record_dynamic_relocation_with_output_info(
            input,
            object::SectionIndex(3),
            8,
            256,
            24,
            None,
        );
        state.record_dynamic_relocation_with_output_info(
            input,
            object::SectionIndex(3),
            16,
            280,
            0,
            None,
        );

        assert_eq!(
            *state.current_dynamic_relocations.lock().unwrap(),
            vec![DynamicRelocationRecord::new(
                input,
                object::SectionIndex(3),
                8,
                256,
                24,
                None
            )]
        );
    }

    #[test]
    fn patch_output_ranges_must_not_overlap() {
        let patch = |output_offset, size| SectionPatch {
            output_offset,
            size,
            data: Vec::new(),
            deferred_relocation: None,
            preserve_ranges: Vec::new(),
            adjustments: Vec::new(),
        };

        assert!(patch_output_range_rejection_reason(&[patch(16, 8), patch(24, 8)]).is_none());
        assert!(patch_output_range_rejection_reason(&[patch(24, 8), patch(16, 8)]).is_none());
        assert_eq!(
            patch_output_range_rejection_reason(&[patch(16, 8), patch(23, 8)]).as_deref(),
            Some("changed patch output ranges overlap")
        );
        assert_eq!(
            patch_output_range_rejection_reason(&[patch(usize::MAX as u64, 8)]).as_deref(),
            Some("changed patch output range overflow")
        );
    }
}
