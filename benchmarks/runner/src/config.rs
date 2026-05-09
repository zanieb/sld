use crate::LinkerKind;
use crate::Result;
use anyhow::Context as _;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) name: String,

    #[serde(default, rename = "bench")]
    pub(crate) benches: BTreeMap<String, BenchConfig>,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BenchConfig {
    /// Name of the save-dir to run. Defaults to the benchmark name.
    pub(crate) save: Option<String>,
    #[serde(default)]
    pub(crate) skip: bool,
    pub(crate) min_wild_version: Option<String>,
    #[serde(default)]
    pub(crate) skip_linkers: Vec<LinkerKind>,
    #[serde(default)]
    pub(crate) extra_flags: Vec<String>,
    #[serde(default)]
    pub(crate) wild_extra_flags: Vec<String>,
    /// Paths relative to the save-dir to mutate before each timed run.
    #[serde(default)]
    pub(crate) mutate_files: Vec<Mutation>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum Mutation {
    AppendZero(String),
    ElfSectionByte { path: String, section: String },
}

impl Mutation {
    pub(crate) fn path(&self) -> &str {
        match self {
            Mutation::AppendZero(path) => path,
            Mutation::ElfSectionByte { path, .. } => path,
        }
    }
}

impl Config {
    pub(crate) fn load(config_path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read `{}`", config_path.display()))?;

        toml::from_str(&contents)
            .with_context(|| format!("Failed to parse `{}`", config_path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn parses_incremental_mutation_files() {
        let config: Config = toml::from_str(
            r#"
name = "test"

[bench.changed-incremental]
save = "large"
wild_extra_flags = ["--incremental"]
mutate_files = ["changed.o"]
"#,
        )
        .unwrap();

        let bench = config.benches.get("changed-incremental").unwrap();
        assert_eq!(
            bench.mutate_files,
            [super::Mutation::AppendZero("changed.o".to_owned())]
        );
    }

    #[test]
    fn parses_incremental_elf_section_mutation() {
        let config: Config = toml::from_str(
            r#"
name = "test"

[bench.changed-incremental]
save = "large"
wild_extra_flags = ["--incremental"]
mutate_files = [{ path = "changed.o", section = ".data" }]
"#,
        )
        .unwrap();

        let bench = config.benches.get("changed-incremental").unwrap();
        assert_eq!(
            bench.mutate_files,
            [super::Mutation::ElfSectionByte {
                path: "changed.o".to_owned(),
                section: ".data".to_owned(),
            }]
        );
    }
}
