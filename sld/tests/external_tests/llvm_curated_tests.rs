use crate::Result;
use crate::external_tests::external_linker_name;
use crate::external_tests::run_external_test;
use crate::external_tests::should_not_ignore_tests;
use crate::external_tests::using_third_party_linker;
use libsld::error::Context;
use libtest_mimic::Failed;
use libtest_mimic::Trial;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Output;

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    skipped_groups: HashMap<String, SkippedGroup>,
}

#[derive(Deserialize)]
struct SkippedGroup {
    tests: Vec<String>,
}

#[derive(Copy, Clone)]
struct Suite {
    name: &'static str,
    display_name: &'static str,
    prefix: &'static str,
    relative_test_dir: &'static str,
    skip_list_file: &'static str,
}

const LLVM_ELF: Suite = Suite {
    name: "llvm_elf",
    display_name: "LLVM ELF",
    prefix: "external_test_suites/llvm-project/ELF",
    relative_test_dir: "../external_test_suites/llvm-project/ELF/test",
    skip_list_file: "llvm_elf_skip_tests.toml",
};

const LLVM_MACHO: Suite = Suite {
    name: "llvm_macho",
    display_name: "LLVM Mach-O",
    prefix: "external_test_suites/llvm-project/MachO",
    relative_test_dir: "../external_test_suites/llvm-project/MachO/test",
    skip_list_file: "llvm_macho_skip_tests.toml",
};

pub(crate) fn collect_elf_tests(tests: &mut Vec<Trial>, filter: &crate::Filter) -> Result {
    collect_suite_tests(LLVM_ELF, tests, filter)
}

pub(crate) fn collect_macho_tests(tests: &mut Vec<Trial>, filter: &crate::Filter) -> Result {
    collect_suite_tests(LLVM_MACHO, tests, filter)
}

fn collect_suite_tests(suite: Suite, tests: &mut Vec<Trial>, filter: &crate::Filter) -> Result {
    if filter.excludes(suite.prefix) {
        return Ok(());
    }

    let third_party = using_third_party_linker();
    let linker_name = external_linker_name();
    let skip_tests = load_skip_tests_config(suite)?;
    let test_dir_path = crate::base_dir().join(suite.relative_test_dir);
    let dir = fs::read_dir(&test_dir_path)
        .with_context(|| format!("Failed to read directory {}", test_dir_path.display()))?;

    for ent in dir {
        let ent = ent?;
        let path = ent.path();
        if path.extension().is_none_or(|ext| ext != "sh") {
            continue;
        }

        let file_name =
            String::from_utf8_lossy(path.file_name().unwrap().as_encoded_bytes()).to_string();
        let name = if third_party {
            format!("{}/test/{file_name}[{linker_name}]", suite.prefix)
        } else {
            format!("{}/test/{file_name}", suite.prefix)
        };

        if !should_skip_test(suite, &skip_tests, &path) && !should_skip_by_local_config(&path) {
            tests.push(Trial::test(name, move || {
                check_test_regression(suite, path).map_err(|e| Failed::from(e.to_string()))
            }));
        } else if should_skip_test_by_toml(suite, &skip_tests, &path)
            && !should_skip_by_local_config(&path)
        {
            tests.push(Trial::test(format!("{name}/expect_failure"), move || {
                verify_skipped_test_still_fails(suite, path)
                    .map_err(|e| Failed::from(e.to_string()))
            }));
        }
    }

    Ok(())
}

fn check_test_regression(suite: Suite, test: PathBuf) -> Result {
    let output = run_external_test(&test, &[])?;
    if !output.status.success() {
        let error_message = format!(
            "{} test `{}` failed with status: {}\nOutput:\n{}",
            suite.display_name,
            test.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout)
        );
        return Err(error_message.into());
    }

    Ok(())
}

fn verify_skipped_test_still_fails(suite: Suite, test: PathBuf) -> Result {
    let output = run_external_test(&test, &[])?;
    if output.status.success() {
        if test_was_skipped(&output) {
            return Ok(());
        }

        let linker = external_linker_name();
        let message = if using_third_party_linker() {
            format!(
                "{} test `{}` is in the skip list (fails with sld) but passes with '{linker}'. This indicates the failure may be sld-specific.",
                suite.display_name,
                test.display()
            )
        } else {
            format!(
                "{} test `{}` is in skip list but now passes. Should be removed from skip list.",
                suite.display_name,
                test.display()
            )
        };
        return Err(message.into());
    }

    Ok(())
}

fn test_was_skipped(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim_end().ends_with(" skipped"))
}

fn load_skip_tests_config(suite: Suite) -> Result<Vec<String>> {
    let skip_tests_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("external_tests")
        .join(suite.skip_list_file);

    let content = fs::read_to_string(&skip_tests_path)
        .with_context(|| format!("Failed to read {}", skip_tests_path.display()))?;
    let config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", skip_tests_path.display()))?;

    Ok(config
        .skipped_groups
        .into_values()
        .flat_map(|group| group.tests)
        .collect())
}

fn should_skip_test(suite: Suite, skip_tests: &[String], path: &Path) -> bool {
    should_skip_test_by_toml(suite, skip_tests, path)
}

fn should_skip_test_by_toml(suite: Suite, skip_tests: &[String], path: &Path) -> bool {
    if should_not_ignore_tests(suite.name) {
        return false;
    }

    let file_name = path
        .file_name()
        .expect("Must be a valid filename")
        .to_str()
        .expect("Expected valid string name");

    skip_tests.iter().any(|test| test == file_name)
}

/// Returns whether the user's test-config.toml says to skip a particular test. If this returns
/// true, then we skip both the positive and negative versions of the test.
fn should_skip_by_local_config(path: &Path) -> bool {
    if let Ok(config) = crate::read_test_config()
        && let Some(name) = path.file_name().and_then(|name| name.to_str())
        && config.ignore_external_tests.iter().any(|n| n == name)
    {
        true
    } else {
        false
    }
}
