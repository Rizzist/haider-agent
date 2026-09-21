use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const CATALOG: &str = "scripts/race-stress-cases.tsv";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) platform: String,
    pub(crate) package: String,
    pub(crate) suite: String,
    pub(crate) test: String,
    pub(crate) source: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CatalogAudit {
    pub(crate) entries: usize,
    pub(crate) named_race_tests: usize,
    pub(crate) violations: Vec<String>,
}

pub(crate) fn check(root: &Path) -> ExitCode {
    let result = audit(root);
    for violation in &result.violations {
        eprintln!("race-catalog: FAIL — {violation}");
    }
    println!(
        "race-catalog: {} entries cover {} explicitly named race/concurrency tests",
        result.entries, result.named_race_tests
    );
    if result.violations.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub(crate) fn audit(root: &Path) -> CatalogAudit {
    let catalog_path = root.join(CATALOG);
    let entries = match fs::read_to_string(&catalog_path) {
        Ok(content) => match parse_catalog(&content) {
            Ok(entries) => entries,
            Err(violations) => {
                return CatalogAudit {
                    violations,
                    ..CatalogAudit::default()
                };
            }
        },
        Err(error) => {
            return CatalogAudit {
                violations: vec![format!("cannot read {CATALOG}: {error}")],
                ..CatalogAudit::default()
            };
        }
    };

    let mut result = CatalogAudit {
        entries: entries.len(),
        ..CatalogAudit::default()
    };
    let discovered = discover_tests(root);
    let discovered_by_source_and_name = discovered
        .iter()
        .map(|(source, name)| (format!("{source}\t{name}"), ()))
        .collect::<BTreeMap<_, _>>();
    let cataloged = entries
        .iter()
        .map(|entry| format!("{}\t{}", entry.source, entry.test))
        .collect::<BTreeSet<_>>();

    for entry in &entries {
        let key = format!("{}\t{}", entry.source, entry.test);
        if !root.join(&entry.source).is_file() {
            result.violations.push(format!(
                "{} names missing source {}",
                entry.id, entry.source
            ));
        } else if !discovered_by_source_and_name.contains_key(&key) {
            result.violations.push(format!(
                "{} does not name a test function '{}' in {}",
                entry.id, entry.test, entry.source
            ));
        } else if let Ok(content) = fs::read_to_string(root.join(&entry.source))
            && let Some(required_platform) = test_target_constraints(&content).get(&entry.test)
            && entry.platform != "all"
            && entry.platform != *required_platform
        {
            result.violations.push(format!(
                "{} catalogs a {required_platform}-only test for {}",
                entry.id, entry.platform
            ));
        }
    }

    let named = discovered
        .into_iter()
        .filter(|(_, name)| is_explicit_race_name(name))
        .collect::<Vec<_>>();
    result.named_race_tests = named.len();
    for (source, name) in named {
        let key = format!("{source}\t{name}");
        if !cataloged.contains(&key) {
            result
                .violations
                .push(format!("uncataloged named race test: {source}::{name}"));
        }
    }
    result
}

pub(crate) fn parse_catalog(content: &str) -> Result<Vec<Entry>, Vec<String>> {
    let mut entries = Vec::new();
    let mut violations = Vec::new();
    let mut ids = BTreeSet::new();
    let mut tests = BTreeSet::new();
    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 6 || fields.iter().any(|field| field.is_empty()) {
            violations.push(format!(
                "{CATALOG}:{line_number} requires six non-empty tab-separated fields"
            ));
            continue;
        }
        let entry = Entry {
            id: fields[0].to_owned(),
            platform: fields[1].to_owned(),
            package: fields[2].to_owned(),
            suite: fields[3].to_owned(),
            test: fields[4].to_owned(),
            source: fields[5].to_owned(),
        };
        if !matches!(entry.platform.as_str(), "linux" | "macos" | "all") {
            violations.push(format!(
                "{CATALOG}:{line_number} has invalid platform '{}'",
                entry.platform
            ));
        }
        if entry.suite != "lib" && entry.suite.strip_prefix("test:").is_none_or(str::is_empty) {
            violations.push(format!(
                "{CATALOG}:{line_number} suite must be 'lib' or 'test:<target>'"
            ));
        }
        if !ids.insert(entry.id.clone()) {
            violations.push(format!(
                "{CATALOG}:{line_number} duplicates id {}",
                entry.id
            ));
        }
        let test_key = format!("{}\t{}", entry.source, entry.test);
        if !tests.insert(test_key) {
            violations.push(format!(
                "{CATALOG}:{line_number} duplicates {}::{}",
                entry.source, entry.test
            ));
        }
        entries.push(entry);
    }
    if entries.is_empty() {
        violations.push(format!("{CATALOG} has no stress cases"));
    }
    if violations.is_empty() {
        Ok(entries)
    } else {
        Err(violations)
    }
}

fn discover_tests(root: &Path) -> Vec<(String, String)> {
    let crates = root.join("crates");
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    let mut tests = Vec::new();
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        let relative = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        for name in test_functions(&content) {
            tests.push((relative.clone(), name));
        }
    }
    tests
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() != "target" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

pub(crate) fn test_functions(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut test_attribute_seen = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[test]") || trimmed.starts_with("#[tokio::test") {
            test_attribute_seen = true;
            continue;
        }
        if !test_attribute_seen {
            continue;
        }
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        let function = trimmed
            .strip_prefix("async fn ")
            .or_else(|| trimmed.strip_prefix("fn "));
        if let Some(function) = function
            && let Some(name) = function
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .next()
                .filter(|name| !name.is_empty())
        {
            names.push(name.to_owned());
        }
        test_attribute_seen = false;
    }
    names
}

fn is_explicit_race_name(name: &str) -> bool {
    // Subprocess fixtures are implementation details of their owning test,
    // not independent stress candidates (running them directly is a no-op).
    if name.ends_with("_child") {
        return false;
    }
    name.split('_').any(|token| {
        matches!(
            token,
            "race" | "races" | "racing" | "concurrent" | "simultaneous" | "contention"
        ) || token.starts_with("interleav")
    })
}

fn test_target_constraints(content: &str) -> BTreeMap<String, String> {
    let mut constraints = BTreeMap::new();
    let mut attributes = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[") {
            attributes.push(trimmed.replace(' ', ""));
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        let function = trimmed
            .strip_prefix("async fn ")
            .or_else(|| trimmed.strip_prefix("fn "));
        if let Some(function) = function
            && attributes.iter().any(|attribute| {
                attribute.starts_with("#[test]") || attribute.starts_with("#[tokio::test")
            })
            && let Some(name) = function
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .next()
                .filter(|name| !name.is_empty())
        {
            for (platform, target_attribute) in [
                ("linux", "#[cfg(target_os=\"linux\")]"),
                ("macos", "#[cfg(target_os=\"macos\")]"),
            ] {
                if attributes
                    .iter()
                    .any(|attribute| attribute == target_attribute)
                {
                    constraints.insert(name.to_owned(), platform.to_owned());
                }
            }
        }
        attributes.clear();
    }
    constraints
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "race_catalog_tests.rs"]
mod tests;
