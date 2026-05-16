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
    pub(crate) min_sld_version: Option<String>,
    #[serde(default)]
    pub(crate) skip_linkers: Vec<LinkerKind>,
    #[serde(default)]
    pub(crate) extra_flags: Vec<String>,
    #[serde(default)]
    pub(crate) sld_extra_flags: Vec<String>,
    /// Paths relative to the save-dir to mutate before each timed run.
    #[serde(default)]
    pub(crate) mutate_files: Vec<Mutation>,
    /// Strings that must appear in sld's incremental log after each timed sld run.
    #[serde(default)]
    pub(crate) expect_sld_log: Vec<String>,
    /// Whether every timed run must produce output bytes that differ from the warmup output.
    #[serde(default)]
    pub(crate) expect_output_change: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum Mutation {
    AppendZero(String),
    ElfSection {
        path: String,
        section: String,
        #[serde(default)]
        grow: u64,
    },
    FirstElfSection {
        section: String,
        #[serde(default)]
        grow: u64,
    },
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
sld_extra_flags = ["--incremental"]
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
sld_extra_flags = ["--incremental"]
mutate_files = [{ path = "changed.o", section = ".data" }]
"#,
        )
        .unwrap();

        let bench = config.benches.get("changed-incremental").unwrap();
        assert_eq!(
            bench.mutate_files,
            [super::Mutation::ElfSection {
                path: "changed.o".to_owned(),
                section: ".data".to_owned(),
                grow: 0,
            }]
        );
    }

    #[test]
    fn parses_incremental_elf_section_growth_mutation() {
        let config: Config = toml::from_str(
            r#"
name = "test"

[bench.changed-incremental]
save = "large"
sld_extra_flags = ["--incremental"]
mutate_files = [{ path = "changed.o", section = ".data", grow = 1 }]
"#,
        )
        .unwrap();

        let bench = config.benches.get("changed-incremental").unwrap();
        assert_eq!(
            bench.mutate_files,
            [super::Mutation::ElfSection {
                path: "changed.o".to_owned(),
                section: ".data".to_owned(),
                grow: 1,
            }]
        );
    }

    #[test]
    fn parses_incremental_first_elf_section_mutation() {
        let config: Config = toml::from_str(
            r#"
name = "test"

[bench.changed-incremental]
save = "large"
sld_extra_flags = ["--incremental"]
mutate_files = [{ section = ".data" }]
"#,
        )
        .unwrap();

        let bench = config.benches.get("changed-incremental").unwrap();
        assert_eq!(
            bench.mutate_files,
            [super::Mutation::FirstElfSection {
                section: ".data".to_owned(),
                grow: 0,
            }]
        );
    }

    #[test]
    fn parses_incremental_first_elf_section_growth_mutation() {
        let config: Config = toml::from_str(
            r#"
name = "test"

[bench.changed-incremental]
save = "large"
sld_extra_flags = ["--incremental"]
mutate_files = [{ section = ".data", grow = 1 }]
"#,
        )
        .unwrap();

        let bench = config.benches.get("changed-incremental").unwrap();
        assert_eq!(
            bench.mutate_files,
            [super::Mutation::FirstElfSection {
                section: ".data".to_owned(),
                grow: 1,
            }]
        );
    }

    #[test]
    fn parses_incremental_log_expectations() {
        let config: Config = toml::from_str(
            r#"
name = "test"

[bench.changed-incremental]
save = "large"
sld_extra_flags = ["--incremental"]
expect_sld_log = ["patched ", "before loading inputs"]
"#,
        )
        .unwrap();

        let bench = config.benches.get("changed-incremental").unwrap();
        assert_eq!(
            bench.expect_sld_log,
            ["patched ".to_owned(), "before loading inputs".to_owned()]
        );
    }

    #[test]
    fn parses_incremental_output_change_expectation() {
        let config: Config = toml::from_str(
            r#"
name = "test"

[bench.changed-incremental]
save = "large"
sld_extra_flags = ["--incremental"]
mutate_files = [{ path = "changed.o", section = ".data" }]
expect_output_change = true
"#,
        )
        .unwrap();

        let bench = config.benches.get("changed-incremental").unwrap();
        assert!(bench.expect_output_change);
    }

    #[test]
    fn checked_in_incremental_linux_config_parses() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let config_path = manifest_dir
            .parent()
            .unwrap()
            .join("incremental-linux.toml");

        Config::load(&config_path).unwrap();
    }

    #[test]
    fn checked_in_macos_config_parses() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let config_path = manifest_dir.parent().unwrap().join("macos.toml");

        Config::load(&config_path).unwrap();
    }
}
