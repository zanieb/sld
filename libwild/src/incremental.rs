use crate::error::Context as _;
use crate::error::Result;
use crate::input_data::FileLoader;
use crate::platform;
use crate::timing_phase;
use hashbrown::HashMap;
use hashbrown::HashSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

const STATE_VERSION: &str = "wild-incremental-state-v1";
const INDEX_FILE: &str = "index";
const LOG_FILE: &str = "log";

pub(crate) struct PreparedState {
    mode: IncrementalMode,
    current: CurrentState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IncrementalMode {
    Disabled,
    Reuse,
    Initial { reason: String },
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
        });
    }

    timing_phase!("Prepare incremental link");

    let current = CurrentState::new(args, file_loader);
    let previous = PersistedState::read(&current.state_dir);
    let mode = match previous {
        Ok(Some(previous)) => classify_incremental_mode(args.output(), &current, &previous),
        Ok(None) => IncrementalMode::Initial {
            reason: "no previous incremental state".to_owned(),
        },
        Err(error) => IncrementalMode::Initial {
            reason: format!("could not read previous incremental state: {error:?}"),
        },
    };

    current.log_mode(&mode)?;

    Ok(PreparedState { mode, current })
}

impl PreparedState {
    pub(crate) fn can_reuse_output(&self) -> bool {
        self.mode == IncrementalMode::Reuse
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

        let state = PersistedState {
            args_hash: self.current.args_hash.clone(),
            output,
            input_files: self.current.input_files.clone(),
        };

        state.write(&self.current.state_dir)?;
        snapshot_input_files(&self.current.state_dir, file_loader);
        Ok(())
    }
}

fn classify_incremental_mode(
    output: &Path,
    current: &CurrentState,
    previous: &PersistedState,
) -> IncrementalMode {
    if current.args_hash != previous.args_hash {
        return IncrementalMode::Initial {
            reason: "linker arguments changed".to_owned(),
        };
    }

    match FileContentState::from_path(output) {
        Ok(output_state) if output_state == previous.output => {}
        Ok(_) => {
            return IncrementalMode::Initial {
                reason: "output file changed since previous link".to_owned(),
            };
        }
        Err(error) => {
            return IncrementalMode::Initial {
                reason: format!("output file could not be reused: {error:?}"),
            };
        }
    }

    if current.input_files != previous.input_files {
        return IncrementalMode::Initial {
            reason: describe_input_difference(&current.input_files, &previous.input_files),
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
            IncrementalMode::Initial { reason } => {
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
        if version != STATE_VERSION {
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

        if lines.next().is_some() {
            return Err(crate::error!("Unexpected trailing incremental state data"));
        }

        Ok(Self {
            args_hash,
            output,
            input_files,
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
        out
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
        let state = state("args", b"output", &[("a.o", b"a"), ("b.o", b"bbb")]);
        assert_eq!(PersistedState::parse(&state.render()).unwrap(), state);
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
            IncrementalMode::Initial { reason } if reason == "linker arguments changed"
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
            IncrementalMode::Initial { reason } if reason.contains("input file changed")
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
            IncrementalMode::Initial { reason } if reason.contains("output file could not be reused")
        ));
    }
}
