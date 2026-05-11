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

const STATE_VERSION: &str = "wild-incremental-state-v16";
const STATE_VERSION_V15: &str = "wild-incremental-state-v15";
const STATE_VERSION_V14: &str = "wild-incremental-state-v14";
const STATE_VERSION_V13: &str = "wild-incremental-state-v13";
const STATE_VERSION_V12: &str = "wild-incremental-state-v12";
const STATE_VERSION_V11: &str = "wild-incremental-state-v11";
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
const UPDATE_MARKER_FILE: &str = "update-in-progress";
const SECTIONS_FILE: &str = "sections";
const SECTIONS_FILE_PREFIX: &str = "sections-";
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
    link_options_hash: String,
    wild_version: String,
    input_files: Vec<FileState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedState {
    args_hash: String,
    link_options_hash: Option<String>,
    wild_version: Option<String>,
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
    tree_hash: Option<String>,
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
    input: String,
    section_index: u32,
    section_name: Option<String>,
    input_size: u64,
    output_offset: u64,
    output_size: u64,
    data_hash: Option<String>,
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
                link_options_hash: String::new(),
                wild_version: String::new(),
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
    if mode_needs_previous_sections(&mode) {
        match PersistedState::read(&state_dir) {
            Ok(Some(previous)) => {
                previous_sections = previous.sections.iter().cloned().collect();
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
        current_sections: Mutex::new(Vec::new()),
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
    let current_wild_version = wild_version(args);
    if wild_version_relink_reason(previous.wild_version.as_deref(), &current_wild_version).is_some()
    {
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
        refresh_input_file_identities_at_indices(
            &mut previous.input_files,
            rewritten_inputs.iter().map(|(input_index, _)| *input_index),
        );
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

    if let Some(reason) = input_identity_mismatch_reason(&previous.input_files)? {
        append_log(
            &state_dir,
            &format!("incremental fast path unavailable before loading inputs: {reason}"),
        )?;
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

    let mut previous = previous;
    let mut patches = Vec::new();
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

        let (fingerprint, matched_sections, current_sections, resolved_patches) = {
            let input = &previous.input_files[*input_index];
            if !archive_members_match_snapshot(state_dir, input, &bytes)? {
                return Ok(ChangedInputPatchResult::Unsupported(format!(
                    "archive members changed in `{}`",
                    path.display()
                )));
            }

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

            let Some(fingerprint) = patch_fingerprint(
                &bytes,
                input.path.as_str(),
                current_sections.iter().cloned(),
            )?
            else {
                return Ok(ChangedInputPatchResult::Unsupported(format!(
                    "could not resolve patchable sections in `{}`",
                    path.display()
                )));
            };
            if fingerprint != previous_patch.fingerprint {
                return Ok(ChangedInputPatchResult::Unsupported(format!(
                    "changed bytes outside patchable sections in `{}`",
                    path.display()
                )));
            }

            let patch_sections = if let Some(changed_sections) = matched_changed_sections {
                changed_sections
            } else {
                changed_patch_sections(state_dir, input, &bytes, &matched_sections)?
                    .unwrap_or_else(|| current_sections.clone())
            };
            patched_section_count += patch_sections.len();

            let Some(resolved_patches) =
                resolved_patch_sections_for_input(&bytes, input.path.as_str(), patch_sections)?
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

            (
                fingerprint,
                matched_sections,
                current_sections,
                resolved_patches,
            )
        };

        let sections_changed = update_section_records_for_matched_patches(
            previous.input_files[*input_index].path.as_str(),
            &matched_sections,
            &mut previous.sections,
        );
        if sections_changed {
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
        });
        patches.extend(resolved_patches.into_iter().map(|resolved| resolved.patch));
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
            return Ok(ChangedInputPatchResult::Unsupported(
                "changed patch output range is out of bounds".to_owned(),
            ));
        };
        if patch.data.len() > output_range.len() {
            return Ok(ChangedInputPatchResult::Unsupported(
                "changed patch data does not fit in the previous output range".to_owned(),
            ));
        }
        for preserve_range in &patch.preserve_ranges {
            let Some(data_range) = patch.data.get_mut(preserve_range.clone()) else {
                return Ok(ChangedInputPatchResult::Unsupported(
                    "changed patch preserve range is out of bounds".to_owned(),
                ));
            };
            let Some(previous_range) = output_range.get(preserve_range.clone()) else {
                return Ok(ChangedInputPatchResult::Unsupported(
                    "changed patch preserve range is out of bounds".to_owned(),
                ));
            };
            data_range.copy_from_slice(previous_range);
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
    if let Some(reason) = input_identity_mismatch_reason(&previous.input_files)? {
        return Ok(ChangedInputPatchResult::Unsupported(reason));
    }
    PersistedState {
        args_hash: previous.args_hash,
        link_options_hash: previous.link_options_hash,
        wild_version: previous.wild_version,
        output,
        build_id_hashes,
        input_files: previous.input_files,
        sections: previous.sections,
        sections_file: previous.sections_file,
    }
    .write_metadata_update(state_dir)?;
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
    if previous_patch.sections.is_empty() {
        return Err(format!(
            "no patchable sections recorded for `{}`",
            path.display()
        ));
    }
    let patch_section_keys = previous_patch
        .sections
        .iter()
        .map(|section| (section.input.as_str(), section.section_index))
        .collect::<HashSet<_>>();
    if patch_section_keys.len() != previous_patch.sections.len() {
        return Err(format!(
            "duplicate patchable section metadata for `{}`",
            path.display()
        ));
    }
    Ok(PreviousPatchState {
        fingerprint: previous_patch.fingerprint.clone(),
        sections: previous_patch
            .sections
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
    preserve_ranges: Vec<std::ops::Range<usize>>,
}

struct ResolvedSectionPatch {
    section: PatchSection,
    patch: SectionPatch,
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

    record.input = current.input.clone();
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
            link_options_hash: Some(self.current.link_options_hash.clone()),
            wild_version: Some(self.current.wild_version.clone()),
            output,
            build_id_hashes,
            input_files,
            sections,
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
        wild_version_relink_reason(previous.wild_version.as_deref(), &current.wild_version)
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
            wild_version: wild_version(args),
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
                read_sections_sidecar(state_dir, sections_file).map(Some)
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
        let wild_version = if lines
            .peek()
            .is_some_and(|line| line.starts_with("wild-version\t"))
        {
            Some(parse_prefixed_line(lines.next(), "wild-version")?.to_owned())
        } else {
            None
        };
        if version == STATE_VERSION && wild_version.is_none() {
            return Err(crate::error!("Missing Wild version in incremental state"));
        }
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
            link_options_hash,
            wild_version,
            output,
            build_id_hashes,
            input_files,
            sections,
            sections_file,
        })
    }

    fn write(&self, state_dir: &Path) -> Result {
        let sections = self.render_sections();
        let sections_file = section_sidecar_file_name(&sections);
        self.write_sections(state_dir, &sections_file, &sections)?;

        let mut state = self.clone();
        state.sections_file = Some(sections_file);
        state.write_index(state_dir)
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

    fn render_index(&self) -> String {
        let mut out = self.render_header_and_inputs();
        writeln!(
            &mut out,
            "sections-file\t{}",
            self.sections_file.as_deref().unwrap_or(SECTIONS_FILE)
        )
        .unwrap();
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
        if let Some(version) = &self.wild_version {
            writeln!(&mut out, "wild-version\t{version}").unwrap();
        }
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

fn record_patch_fingerprints(
    input_files: &mut [FileState],
    file_loader: &FileLoader<'_>,
    sections: &[SectionRecord],
    output: &[u8],
) -> Result {
    let mut sections_by_file = HashMap::<&str, Vec<&SectionRecord>>::new();
    for section in sections {
        sections_by_file
            .entry(section.input_file.as_str())
            .or_default()
            .push(section);
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
        let patch_sections =
            direct_copy_patch_sections(input_file.data(), input.path.as_str(), output, sections)?;
        input.patch = patch_fingerprint(
            input_file.data(),
            input.path.as_str(),
            patch_sections.iter().cloned(),
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
        });
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
) -> Result<Vec<PatchSection>> {
    let mut patch_sections = Vec::new();

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
                .section_by_index(object::SectionIndex(record.section_index as usize))
                .context("Missing incremental patch candidate section")?;
            let data = section
                .data()
                .context("Failed to read incremental patch candidate section data")?;
            let Some(preserve_ranges) = section_direct_patch_preserve_ranges(&section, data.len())
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
                    input: record.input.clone(),
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

fn section_flags_allow_patching(flags: object::SectionFlags) -> bool {
    let object::SectionFlags::Elf { sh_flags } = flags else {
        return false;
    };
    // Sections that Wild actually merges are written by the merge-strings path, so they don't
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
) -> Option<Vec<std::ops::Range<usize>>> {
    if !section_flags_allow_patching(section.flags())
        || !section
            .name()
            .ok()
            .is_none_or(|name| section_name_allows_direct_patching(name.as_bytes()))
    {
        return None;
    }

    relocation_preserve_ranges(section, section_data_len)
}

fn relocation_preserve_ranges<'data>(
    section: &impl object::ObjectSection<'data>,
    section_data_len: usize,
) -> Option<Vec<std::ops::Range<usize>>> {
    let mut ranges = Vec::<std::ops::Range<usize>>::new();
    for (offset, relocation) in section.relocations() {
        if relocation.kind() == object::RelocationKind::None {
            continue;
        }
        if relocation.has_implicit_addend()
            || relocation.kind() != object::RelocationKind::Absolute
            || relocation.encoding() != object::RelocationEncoding::Generic
            || relocation.size() == 0
            || relocation.size() % 8 != 0
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
    let snapshot = input_snapshot_path_for_encoded_path(state_dir, &previous_input.path);
    if !previous_input
        .content
        .identity_matches_snapshot_path(&snapshot)
        .unwrap_or(false)
    {
        return Ok(false);
    }

    let previous_bytes = match std::fs::read(&snapshot) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
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

fn patch_fingerprint(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<String>> {
    let Some(ranges) = patch_ranges(bytes, input_file_path, sections)? else {
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

    let Some(current_sections) =
        resolve_current_patch_sections(current_bytes, input_file_path, sections.iter().cloned())?
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
    let snapshot = input_snapshot_path_for_encoded_path(state_dir, &previous_input.path);
    if !previous_input
        .content
        .identity_matches_snapshot_path(&snapshot)
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let previous_bytes = match std::fs::read(&snapshot) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
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
    let snapshot = input_snapshot_path_for_encoded_path(state_dir, &previous_input.path);
    if !previous_input
        .content
        .identity_matches_snapshot_path(&snapshot)
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let previous_bytes = match std::fs::read(&snapshot) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
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

fn resolve_current_patch_sections(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<Vec<PatchSection>>> {
    Ok(
        resolved_patch_sections_for_input(bytes, input_file_path, sections)?.map(|patches| {
            patches
                .into_iter()
                .map(|resolved| resolved.section)
                .collect()
        }),
    )
}

fn resolved_patch_sections_for_input(
    bytes: &[u8],
    input_file_path: &str,
    sections: impl IntoIterator<Item = PatchSection>,
) -> Result<Option<Vec<ResolvedSectionPatch>>> {
    let sections = sections.into_iter().collect::<Vec<_>>();
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
            let Some(preserve_ranges) = section_direct_patch_preserve_ranges(&section, data.len())
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
                    preserve_ranges,
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
    state.tree_hash = Some(build_id_hash_tree_hash(tree));
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
            parse_patch_sections(&path, sections).map(|sections| FilePatchState {
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
                "{}:{}:{}:{}:{}:{}:{}",
                section.input,
                section.section_index,
                section.input_size,
                section.output_offset,
                section.output_size,
                section
                    .section_name
                    .as_ref()
                    .map(|name| hex::encode(name.as_bytes()))
                    .unwrap_or_else(|| ABSENT_FIELD.to_owned()),
                section.data_hash.as_deref().unwrap_or(ABSENT_FIELD)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
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
        .identity_matches_snapshot_path(&snapshot)
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
    Ok(Some((
        bytes,
        FileContentState {
            len: identity.len,
            hash: String::new(),
            identity: Some(identity),
        },
    )))
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

fn section_sidecar_file_name(contents: &str) -> String {
    format!("{SECTIONS_FILE_PREFIX}{}", hash_text(contents))
}

fn args_hash(args: &impl platform::Args) -> String {
    hash_text(&format!("{args:?}"))
}

fn link_options_hash(args: &impl platform::Args) -> String {
    hash_text(&args.incremental_link_options())
}

fn wild_version(args: &impl platform::Args) -> String {
    args.common().version.to_string()
}

fn wild_version_relink_reason<'a>(previous: Option<&'a str>, current: &str) -> Option<&'a str> {
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
            wild_version: Some("wild-test".to_owned()),
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
            .filter(|line| {
                !line.starts_with("build-id-hash\t") && !line.starts_with("wild-version\t")
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
        let snapshot = input_snapshot_path(&state_dir, &input);
        let previous = FileState {
            path: encode_path(&input),
            content: FileContentState::from_path_identity_only(&snapshot).unwrap(),
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

    #[test]
    fn match_patch_sections_records_current_section_size_after_growth() {
        let bytes = growable_data_elf();
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
                input_file: input_file.clone(),
                input: input_ref.clone(),
                section_index: 3,
                output_offset: 64,
                size: 16,
            },
            SectionRecord {
                input_file: unrelated_input,
                input: input_ref.clone(),
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

        let resolved = resolve_current_patch_sections(&bytes, &input_ref, [patch_section])
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
                || section_direct_patch_preserve_ranges(&section, data.len()).is_none()
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
        let snapshot = input_snapshot_path(&state_dir, &input);
        let mut member_ref = input.as_os_str().as_encoded_bytes().to_vec();
        member_ref.push(0);
        member_ref.extend_from_slice(b"member.o");
        member_ref.push(0);
        member_ref.extend_from_slice(b"8:14");
        let previous = FileState {
            path: encode_path(&input),
            content: FileContentState::from_path_identity_only(&snapshot).unwrap(),
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
        assert_eq!(PersistedState::parse(&state.render()).unwrap(), state);
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
            }),
        }];
        let sections = vec![section_record("a.o", 1, 100, 4)];

        record_patch_fingerprints(&mut input_files, &file_loader, &sections, b"").unwrap();

        assert_eq!(
            input_files[0].patch.as_ref().unwrap().fingerprint,
            "patch-hash"
        );
    }

    #[test]
    fn record_patch_fingerprints_clears_stale_patch_without_loaded_input() {
        let arena = colosseum::sync::Arena::new();
        let file_loader = FileLoader::new(&arena);
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
            }),
        }];
        let sections = vec![section_record("a.o", 1, 108, 4)];

        record_patch_fingerprints(&mut input_files, &file_loader, &sections, b"").unwrap();

        assert!(input_files[0].patch.is_none());
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
        assert_eq!(sections[0].input, hex::encode("a.o"));
        assert_eq!(sections[1].input, hex::encode("a.o"));
        assert_eq!(sections[0].section_name, None);
        assert_eq!(sections[1].section_name, None);
    }

    #[test]
    fn v12_state_version_is_accepted_without_wild_version() {
        let mut state = state("args", b"output", &[("a.o", b"a")]);
        state.sections.push(section_record("a.o", 1, 100, 12));
        let rendered = state
            .render()
            .replacen(STATE_VERSION, STATE_VERSION_V12, 1)
            .lines()
            .filter(|line| !line.starts_with("wild-version\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let parsed = PersistedState::parse(&rendered).unwrap();

        assert_eq!(parsed.sections.len(), 1);
        assert!(parsed.wild_version.is_none());
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
    fn current_state_version_requires_wild_version() {
        let rendered = state("args", b"output", &[("a.o", b"a")])
            .render()
            .lines()
            .filter(|line| !line.starts_with("wild-version\t"))
            .fold(String::new(), |mut out, line| {
                writeln!(&mut out, "{line}").unwrap();
                out
            });

        let error = PersistedState::parse(&rendered).unwrap_err();

        assert!(error.to_string().contains("Missing Wild version"));
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

        let parsed = parse_input_line(&line).unwrap();

        assert_eq!(parsed.patch.unwrap().sections, Vec::new());
    }

    #[test]
    fn changed_input_patch_rejects_missing_metadata_before_reading_changed_input() {
        let dir = tempfile::tempdir().unwrap();
        let missing_input = dir.path().join("missing.o");
        let previous = PersistedState {
            args_hash: "args".to_owned(),
            link_options_hash: Some("args".to_owned()),
            wild_version: Some("wild-test".to_owned()),
            output: FileContentState::from_bytes(b"output"),
            build_id_hashes: None,
            input_files: vec![FileState {
                path: encode_path(&missing_input),
                content: FileContentState::from_bytes(b"previous"),
                patch: None,
            }],
            sections: Vec::new(),
            sections_file: None,
        };

        let result = patch_changed_inputs(
            &crate::args::elf::ElfArgs::default(),
            dir.path(),
            previous,
            &[(0, missing_input)],
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
    fn stable_identity_read_records_matching_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.o");
        std::fs::write(&path, b"abcd").unwrap();

        let (bytes, content) = read_file_with_stable_identity(&path).unwrap().unwrap();

        assert_eq!(bytes, b"abcd");
        assert_eq!(content, FileContentState::from_path(&path).unwrap());
    }

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
            wild_version: "wild-test".to_owned(),
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
            link_options_hash: "args".to_owned(),
            wild_version: "wild-test".to_owned(),
            input_files: previous.input_files.clone(),
        };

        assert_eq!(
            classify_incremental_mode(&output, &current, &previous),
            IncrementalMode::Reuse
        );
    }

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
            wild_version: "wild-test".to_owned(),
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
            wild_version: "wild-test".to_owned(),
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
    fn changed_wild_version_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            wild_version: "new-wild".to_owned(),
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

    #[test]
    fn missing_wild_version_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let mut previous = state("args", b"output", &[("a.o", b"a")]);
        previous.wild_version = None;
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            wild_version: "wild-test".to_owned(),
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
            wild_version: "wild-test".to_owned(),
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
    fn changed_input_list_keeps_unchanged_section_reuse_available() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"output").unwrap();

        let previous = state("old-exact-args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "new-exact-args".to_owned(),
            link_options_hash: "old-exact-args".to_owned(),
            wild_version: "wild-test".to_owned(),
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
            wild_version: "wild-test".to_owned(),
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

    #[test]
    fn missing_output_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            wild_version: "wild-test".to_owned(),
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
    fn changed_output_forces_initial_link() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::write(&output, b"changed").unwrap();
        let previous = state("args", b"output", &[("a.o", b"a")]);
        let current = CurrentState {
            state_dir: dir.path().join("out.incr"),
            args_hash: "args".to_owned(),
            link_options_hash: "args".to_owned(),
            wild_version: "wild-test".to_owned(),
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
        assert_eq!(
            state.tree_hash.as_deref(),
            Some(build_id_hash_tree_hash(&tree).as_str())
        );
    }

    #[test]
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
                wild_version: "wild-test".to_owned(),
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
                link_options_hash: "args".to_owned(),
                wild_version: "wild-test".to_owned(),
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

    #[test]
    fn patch_output_ranges_must_not_overlap() {
        let patch = |output_offset, size| SectionPatch {
            output_offset,
            size,
            data: Vec::new(),
            preserve_ranges: Vec::new(),
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
