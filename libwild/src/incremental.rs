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

const STATE_VERSION: &str = "wild-incremental-state-v7";
const STATE_VERSION_V6: &str = "wild-incremental-state-v6";
const STATE_VERSION_V5: &str = "wild-incremental-state-v5";
const STATE_VERSION_V4: &str = "wild-incremental-state-v4";
const STATE_VERSION_V3: &str = "wild-incremental-state-v3";
const STATE_VERSION_V2: &str = "wild-incremental-state-v2";
const STATE_VERSION_V1: &str = "wild-incremental-state-v1";
const INDEX_FILE: &str = "index";
const LOG_FILE: &str = "log";
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
    input_files: Vec<FileState>,
    sections: Vec<SectionRecord>,
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
    sections: Vec<u32>,
}

#[derive(Debug, Clone, Eq)]
struct FileContentState {
    len: u64,
    hash: String,
    identity: Option<FileIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    dev: u64,
    ino: u64,
    modified_sec: i64,
    modified_nsec: i64,
    changed_sec: i64,
    changed_nsec: i64,
}

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
    let Some(previous) = PersistedState::read(&state_dir).unwrap_or_default() else {
        return Ok(false);
    };

    if previous.args_hash != args_hash(args) {
        return Ok(false);
    }
    if !previous.output.identity_matches_path(args.output())? {
        return Ok(false);
    }

    let mut changed_inputs = Vec::new();
    for (index, input) in previous.input_files.iter().enumerate() {
        let path = decode_path(&input.path)?;
        if input.content.identity_matches_path(&path)? {
            continue;
        }
        changed_inputs.push((index, path));
    }

    if !changed_inputs.is_empty() {
        if patch_changed_inputs(args, &state_dir, previous, &changed_inputs)? {
            return Ok(true);
        }
        return Ok(false);
    }

    append_log(&state_dir, "reused existing output before loading inputs")?;
    Ok(true)
}

fn patch_changed_inputs(
    args: &impl platform::Args,
    state_dir: &Path,
    previous: PersistedState,
    changed_inputs: &[(usize, PathBuf)],
) -> Result<bool> {
    timing_phase!("Patch changed incremental inputs");

    let sections_by_file = previous
        .sections
        .iter()
        .filter(|section| section.input == section.input_file)
        .fold(
            HashMap::<&str, Vec<&SectionRecord>>::new(),
            |mut sections, section| {
                sections
                    .entry(section.input_file.as_str())
                    .or_default()
                    .push(section);
                sections
            },
        );

    let mut patches = Vec::new();
    let mut input_files = previous.input_files.clone();
    for (input_index, path) in changed_inputs {
        let input = &previous.input_files[*input_index];
        let Some(previous_patch) = input.patch.as_ref() else {
            return Ok(false);
        };
        if previous_patch.sections.is_empty() {
            return Ok(false);
        }
        let Some(all_sections) = sections_by_file.get(input.path.as_str()) else {
            return Ok(false);
        };
        let patch_section_indexes = previous_patch
            .sections
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let sections = all_sections
            .iter()
            .copied()
            .filter(|section| patch_section_indexes.contains(&section.section_index))
            .collect::<Vec<_>>();
        if sections.len() != previous_patch.sections.len() {
            return Ok(false);
        }

        let bytes = std::fs::read(path).with_context(|| {
            format!(
                "Failed to read changed incremental input `{}`",
                path.display()
            )
        })?;
        let fingerprint = patch_fingerprint(&bytes, sections.iter().copied())?;
        if fingerprint.as_deref() != Some(previous_patch.fingerprint.as_str()) {
            return Ok(false);
        }

        let content = FileContentState::from_path_identity_only(path).with_context(|| {
            format!(
                "Failed to record changed incremental input `{}`",
                path.display()
            )
        })?;
        input_files[*input_index].content = content;
        input_files[*input_index].patch = Some(previous_patch.clone());

        patches.extend(patch_sections(&bytes, sections.iter().copied())?);
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
        return Ok(false);
    }

    for patch in patches {
        let start = patch.output_offset as usize;
        let end = start
            .checked_add(patch.size as usize)
            .context("Incremental patch output range overflow")?;
        let output_range = output
            .get_mut(start..end)
            .context("Incremental patch output range out of bounds")?;
        if patch.data.len() > output_range.len() {
            return Ok(false);
        }
        let (data_out, padding) = output_range.split_at_mut(patch.data.len());
        data_out.copy_from_slice(&patch.data);
        padding.fill(0);
    }

    if let Some(range) = build_id_range {
        write_fast_build_id(&mut output, range)?;
    }

    output.flush().with_context(|| {
        format!(
            "Failed to flush incrementally patched output `{}`",
            args.output().display()
        )
    })?;
    drop(output);
    drop(file);

    let output = FileContentState::from_path_identity_only(args.output()).with_context(|| {
        format!(
            "Failed to record patched output `{}` for incremental state",
            args.output().display()
        )
    })?;
    PersistedState {
        args_hash: previous.args_hash,
        output,
        input_files,
        sections: previous.sections.clone(),
    }
    .write(state_dir)?;

    append_log(
        state_dir,
        &format!(
            "patched {} changed input file{} before loading inputs",
            changed_inputs.len(),
            if changed_inputs.len() == 1 { "" } else { "s" }
        ),
    )?;
    Ok(true)
}

struct SectionPatch {
    output_offset: u64,
    size: u64,
    data: Vec<u8>,
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
        _file_loader: &FileLoader<'_>,
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

        let mut sections = self.current_sections.lock().unwrap().clone();
        if sections.is_empty() && self.mode == IncrementalMode::Reuse {
            sections.extend(self.previous_sections.iter().cloned());
        }
        sections.sort();

        let mut input_files = self.current.input_files.clone();
        record_patch_fingerprints(&mut input_files, _file_loader, &sections, args.output())?;

        let state = PersistedState {
            args_hash: self.current.args_hash.clone(),
            output,
            input_files,
            sections,
        };

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
        let path = state_dir.join(INDEX_FILE);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        Ok(Some(Self::parse(&contents)?))
    }

    fn parse(contents: &str) -> Result<Self> {
        let mut lines = contents.lines();
        let version = lines.next().context("Missing incremental state header")?;
        if version != STATE_VERSION
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

        let input_count: usize = parse_prefixed_line(lines.next(), "inputs")?
            .parse()
            .context("Invalid incremental input count")?;

        let mut input_files = Vec::with_capacity(input_count);
        for _ in 0..input_count {
            let line = lines.next().context("Missing incremental input record")?;
            input_files.push(parse_input_line(line)?);
        }

        let sections = if version == STATE_VERSION
            || version == STATE_VERSION_V6
            || version == STATE_VERSION_V5
        {
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
            sections
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
            input_files,
            sections,
        })
    }

    fn write(&self, state_dir: &Path) -> Result {
        std::fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "Failed to create incremental state directory `{}`",
                state_dir.display()
            )
        })?;

        let path = state_dir.join(INDEX_FILE);
        let tmp_path = state_dir.join(format!("{INDEX_FILE}.tmp"));
        std::fs::write(&tmp_path, self.render()).with_context(|| {
            format!("Failed to write incremental state `{}`", tmp_path.display())
        })?;
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to install incremental state `{}`", path.display()))?;
        Ok(())
    }

    fn render(&self) -> String {
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
    output_path: &Path,
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

    let output = std::fs::read(output_path).with_context(|| {
        format!(
            "Failed to read output `{}` for incremental patch fingerprints",
            output_path.display()
        )
    })?;

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
        let patch_sections = direct_copy_patch_sections(input_file.data(), &output, sections)?;
        input.patch = patch_fingerprint(input_file.data(), patch_sections.iter().copied())?.map(
            |fingerprint| FilePatchState {
                fingerprint,
                sections: patch_sections
                    .iter()
                    .map(|section| section.section_index)
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
) -> Result<Vec<&'a SectionRecord>> {
    let file =
        object::File::parse(bytes).context("Failed to parse incremental patch candidate input")?;
    let mut patch_sections = Vec::new();
    for record in sections {
        let section = file
            .section_by_index(object::SectionIndex(record.section_index as usize))
            .context("Missing incremental patch candidate section")?;
        if !section_flags_allow_patching(section.flags()) {
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
            patch_sections.push(*record);
        }
    }
    Ok(patch_sections)
}

fn section_flags_allow_patching(flags: object::SectionFlags) -> bool {
    let object::SectionFlags::Elf { sh_flags } = flags else {
        return false;
    };
    let patchable_kind =
        sh_flags & u64::from(object::elf::SHF_WRITE | object::elf::SHF_EXECINSTR) != 0;
    let content_ordered = sh_flags & u64::from(object::elf::SHF_MERGE) != 0;
    patchable_kind && !content_ordered
}

fn patch_fingerprint<'a>(
    bytes: &[u8],
    sections: impl IntoIterator<Item = &'a SectionRecord>,
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

fn patch_sections<'a>(
    bytes: &[u8],
    sections: impl IntoIterator<Item = &'a SectionRecord>,
) -> Result<Vec<SectionPatch>> {
    let file = object::File::parse(bytes).context("Failed to parse changed incremental input")?;
    sections
        .into_iter()
        .map(|record| {
            let section = file
                .section_by_index(object::SectionIndex(record.section_index as usize))
                .context("Missing changed incremental input section")?;
            let data = section
                .data()
                .context("Failed to read changed incremental input section data")?;
            if data.len() > record.size as usize {
                return Err(crate::error!(
                    "Changed incremental input section grew beyond previous output allocation"
                ));
            }
            Ok(SectionPatch {
                output_offset: record.output_offset,
                size: record.size,
                data: data.to_owned(),
            })
        })
        .collect()
}

fn patch_ranges<'a>(
    bytes: &[u8],
    sections: impl IntoIterator<Item = &'a SectionRecord>,
) -> Result<Option<Vec<std::ops::Range<usize>>>> {
    let file = object::File::parse(bytes).context("Failed to parse incremental patch input")?;
    let mut ranges = Vec::new();
    for record in sections {
        let section = file
            .section_by_index(object::SectionIndex(record.section_index as usize))
            .context("Missing incremental patch input section")?;
        let Some((offset, size)) = section.file_range() else {
            return Ok(None);
        };
        if size > record.size {
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

fn write_fast_build_id(output: &mut [u8], range: std::ops::Range<usize>) -> Result {
    const GNU_NOTE_NAME: &[u8] = b"GNU\0";
    let expected_len = 12 + GNU_NOTE_NAME.len() + blake3::OUT_LEN;
    if range.end - range.start != expected_len {
        return Err(crate::error!(
            "Incremental patching only supports fast 32-byte build IDs"
        ));
    }

    output[range.clone()].fill(0);
    let build_id = blake3::Hasher::new().update_rayon(output).finalize();
    let note = &mut output[range];
    note[0..4].copy_from_slice(&(GNU_NOTE_NAME.len() as u32).to_le_bytes());
    note[4..8].copy_from_slice(&(blake3::OUT_LEN as u32).to_le_bytes());
    note[8..12].copy_from_slice(&object::elf::NT_GNU_BUILD_ID.to_le_bytes());
    note[12..16].copy_from_slice(GNU_NOTE_NAME);
    note[16..].copy_from_slice(build_id.as_bytes());
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
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_patch_sections(sections: &str) -> Result<Vec<u32>> {
    if sections.is_empty() {
        return Ok(Vec::new());
    }
    sections
        .split(',')
        .map(|section| {
            section
                .parse()
                .context("Invalid incremental patch section index")
        })
        .collect()
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

fn append_log(state_dir: &Path, message: &str) -> Result {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join(LOG_FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open incremental log `{}`", path.display()))?;
    writeln!(file, "{message}")?;
    Ok(())
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
            input_files: inputs
                .iter()
                .map(|(path, bytes)| FileState {
                    path: hex::encode(path),
                    content: FileContentState::from_bytes(bytes),
                    patch: None,
                })
                .collect(),
            sections: Vec::new(),
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
            sections: vec![1, 3, 5],
        });
        state.sections.push(section_record("a.o", 1, 100, 12));

        let rendered = state.render();

        assert!(rendered.contains("\tpatch-hash\t1,3,5\n"));
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
        let rendered = state.render().replacen(STATE_VERSION, STATE_VERSION_V5, 1);
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
            identity: Some(identity(4, 1, 2, 3, 6)),
        };

        assert_eq!(first, same);
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
    fn patchable_sections_are_writable_or_executable_but_not_mergeable() {
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

        assert!(section_flags_allow_patching(data));
        assert!(section_flags_allow_patching(text));
        assert!(!section_flags_allow_patching(rodata));
        assert!(!section_flags_allow_patching(mergeable));
        assert!(!section_flags_allow_patching(object::SectionFlags::None));
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
