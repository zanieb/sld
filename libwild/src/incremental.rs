use crate::error::Context as _;
use crate::error::Result;
use crate::input_data::FileLoader;
use crate::input_data::InputRef;
use crate::platform;
use crate::timing_phase;
use hashbrown::HashMap;
use hashbrown::HashSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

const STATE_VERSION: &str = "wild-incremental-state-v2";
const PREVIOUS_STATE_VERSION: &str = "wild-incremental-state-v1";
const INDEX_FILE: &str = "index";
const LOG_FILE: &str = "log";

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileContentState {
    len: u64,
    hash: String,
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

    let current = CurrentState::new(args, file_loader);
    let previous = PersistedState::read(&current.state_dir);
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
        allow_reuse: bool,
    ) -> bool {
        if self.mode == IncrementalMode::Disabled {
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

        let output = FileContentState::from_path(args.output()).with_context(|| {
            format!(
                "Failed to fingerprint output file `{}` for incremental state",
                args.output().display()
            )
        })?;

        let mut sections = self.current_sections.lock().unwrap().clone();
        sections.sort();

        let state = PersistedState {
            args_hash: self.current.args_hash.clone(),
            output,
            input_files: self.current.input_files.clone(),
            sections,
        };

        state.write(&self.current.state_dir)?;
        snapshot_input_files(&self.current.state_dir, file_loader);
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

fn snapshot_input_files(state_dir: &Path, file_loader: &FileLoader<'_>) {
    let snapshots_dir = state_dir.join("inputs");
    let tmp_dir = state_dir.join("inputs.tmp");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    if std::fs::create_dir_all(&tmp_dir).is_err() {
        return;
    }

    for (index, input_file) in file_loader.loaded_files.iter().enumerate() {
        let snapshot_path = tmp_dir.join(format!("{index}"));
        let _ = std::fs::hard_link(&input_file.filename, snapshot_path);
    }

    let _ = std::fs::remove_dir_all(&snapshots_dir);
    let _ = std::fs::rename(tmp_dir, snapshots_dir);
}

impl CurrentState {
    fn new(args: &impl platform::Args, file_loader: &FileLoader<'_>) -> Self {
        Self {
            state_dir: state_dir_for_output(args.output()),
            args_hash: hash_text(&format!("{args:?}")),
            input_files: fingerprint_loaded_files(file_loader),
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
        if version != STATE_VERSION && version != PREVIOUS_STATE_VERSION {
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

        let sections = if version == STATE_VERSION {
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
            "output\t{}\t{}",
            self.output.len, self.output.hash
        )
        .unwrap();
        writeln!(&mut out, "inputs\t{}", self.input_files.len()).unwrap();
        for input in &self.input_files {
            writeln!(
                &mut out,
                "input\t{}\t{}\t{}",
                input.path, input.content.len, input.content.hash
            )
            .unwrap();
        }
        writeln!(&mut out, "sections\t{}", self.sections.len()).unwrap();
        for section in &self.sections {
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
    fn from_path(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("Failed to read `{}`", path.display()))?;
        Ok(Self::from_bytes(&bytes))
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            len: bytes.len() as u64,
            hash: hash_bytes(bytes),
        }
    }
}

fn fingerprint_loaded_files(file_loader: &FileLoader<'_>) -> Vec<FileState> {
    let mut files = file_loader
        .loaded_files
        .iter()
        .map(|input_file| FileState {
            path: encode_path(&input_file.filename),
            content: FileContentState::from_bytes(input_file.data()),
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
    let (len, hash) = rest
        .split_once('\t')
        .context("Malformed incremental content record")?;
    Ok(FileContentState {
        len: len.parse().context("Invalid incremental content length")?,
        hash: hash.to_owned(),
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
    if parts.next().is_some() {
        return Err(crate::error!("Malformed incremental input record"));
    }
    Ok(FileState {
        path,
        content: FileContentState { len, hash },
    })
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
    fn previous_state_version_is_accepted_without_sections() {
        let state = state("args", b"output", &[("a.o", b"a")]);
        let rendered = state
            .render()
            .replace(STATE_VERSION, PREVIOUS_STATE_VERSION)
            .split_once("\nsections")
            .unwrap()
            .0
            .to_owned();
        let parsed = PersistedState::parse(&format!("{rendered}\n")).unwrap();
        assert!(parsed.sections.is_empty());
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

        assert!(state.try_reuse_section(input, object::SectionIndex(3), 64, 16, true));
        assert!(!state.try_reuse_section(input, object::SectionIndex(3), 80, 16, true));
        assert_eq!(state.reused_sections.load(Ordering::Relaxed), 1);
        assert_eq!(state.current_sections.lock().unwrap().len(), 2);
    }
}
