use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const TARGET_TOKENS: [&str; 7] = [
    "android", "linux", "macos", "unix", "windows", "win32", "darwin",
];
const SHELL_MARKERS: [&str; 6] = [
    "/bin/bash",
    "/bin/sh",
    "/bin/zsh",
    "cmd.exe",
    "powershell",
    "system32",
];
const SOURCE_EXTENSIONS: [&str; 7] = ["rs", "kt", "kts", "py", "sh", "bash", "zsh"];

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Audit {
    pub(crate) scanned: usize,
    pub(crate) sensitive: usize,
    pub(crate) parameterized: usize,
    pub(crate) parameterized_files: Vec<String>,
    pub(crate) violations: Vec<String>,
}

pub(crate) fn check(root: &Path) -> ExitCode {
    let audit = audit(root);
    for finding in &audit.parameterized_files {
        println!("platform-goldens: ok — {finding}");
    }
    for violation in &audit.violations {
        eprintln!("platform-goldens: FAIL — {violation}");
    }
    println!(
        "platform-goldens: {} fixture/golden files scanned, {} platform-sensitive, {} target-parameterized",
        audit.scanned, audit.sensitive, audit.parameterized
    );
    if audit.violations.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub(crate) fn audit(root: &Path) -> Audit {
    let mut all_files = Vec::new();
    collect_files(root, &mut all_files);
    let fixture_files = all_files
        .iter()
        .filter(|path| is_fixture_or_golden(path, root))
        .cloned()
        .collect::<Vec<_>>();
    let source_files = all_files
        .iter()
        .filter(|path| {
            path.extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| SOURCE_EXTENSIONS.contains(&extension))
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut result = Audit {
        scanned: fixture_files.len(),
        ..Audit::default()
    };

    for path in fixture_files {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let markers = sensitive_markers(&content);
        if markers.is_empty() {
            continue;
        }
        result.sensitive += 1;
        if target_qualified(&path) || content_parameterized(&content) {
            result.parameterized += 1;
            result.parameterized_files.push(format!(
                "{} ({})",
                path.strip_prefix(root).unwrap_or(&path).display(),
                markers.iter().copied().collect::<Vec<_>>().join(", ")
            ));
            continue;
        }

        let siblings = qualified_siblings(&path);
        if siblings
            .iter()
            .any(|sibling| consumer_selects(&path, sibling, &source_files))
        {
            result.parameterized += 1;
            result.parameterized_files.push(format!(
                "{} ({}, target-selected sibling family)",
                path.strip_prefix(root).unwrap_or(&path).display(),
                markers.iter().copied().collect::<Vec<_>>().join(", ")
            ));
            continue;
        }

        let relative = path.strip_prefix(root).unwrap_or(&path).display();
        let reason = if siblings.is_empty() {
            "no target-qualified sibling".to_owned()
        } else {
            "target-qualified sibling exists, but no source consumer selects both variants by target"
                .to_owned()
        };
        result.violations.push(format!(
            "{relative} contains {} ({reason})",
            markers.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    result
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            if name != "target"
                && name != ".git"
                && name != "node_modules"
                && !name.to_string_lossy().starts_with(".race-")
            {
                collect_files(&path, out);
            }
        } else {
            out.push(path);
        }
    }
}

fn is_fixture_or_golden(path: &Path, root: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("fixtures" | "goldens" | "snapshots")
        )
    }) || path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "golden" | "snap"))
}

fn sensitive_markers(content: &str) -> BTreeSet<&'static str> {
    let lower = content.to_ascii_lowercase();
    let mut found = BTreeSet::new();
    if SHELL_MARKERS.iter().any(|marker| lower.contains(marker)) {
        found.insert("platform shell wording");
    }
    if contains_windows_path(content) {
        found.insert("Windows path separator");
    }
    let inventory = [
        "\"tools\"",
        "\"tool_sets\"",
        "\"tools_allowed\"",
        "\"inventory\"",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    let platform_name = TARGET_TOKENS.iter().any(|target| lower.contains(target));
    if inventory && platform_name {
        found.insert("per-OS tool inventory");
    }
    found
}

fn contains_windows_path(content: &str) -> bool {
    let bytes = content.as_bytes();
    (0..bytes.len().saturating_sub(3)).any(|index| {
        let boundary = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
        boundary
            && bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && matches!(bytes[index + 2], b'\\' | b'/')
            // JSON's escaped quote (`:\"`) is punctuation, not a path.
            && bytes[index + 3] != b'"'
    }) || content
        .lines()
        .any(|line| line.trim_start().starts_with("\\\\"))
        || content.contains("\"\\\\\\\\")
}

fn target_qualified(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| filename_tokens(name).any(|token| TARGET_TOKENS.contains(&token)))
}

fn filename_tokens(name: &str) -> impl Iterator<Item = &str> {
    name.split(['.', '_', '-'])
}

fn content_parameterized(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    [
        "{{target}}",
        "{{target_os}}",
        "${target_os}",
        "%target_os%",
        "<target_os>",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn qualified_siblings(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let Some(generic_name) = path.file_name().and_then(OsStr::to_str) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| candidate != path && target_qualified(candidate))
        .filter(|candidate| {
            candidate
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| remove_target_token(name) == generic_name)
        })
        .collect()
}

fn remove_target_token(name: &str) -> String {
    for target in TARGET_TOKENS {
        for separator in ['.', '_', '-'] {
            let needle = format!("{separator}{target}");
            if name.contains(&needle) {
                return name.replacen(&needle, "", 1);
            }
        }
    }
    name.to_owned()
}

fn consumer_selects(generic: &Path, qualified: &Path, sources: &[PathBuf]) -> bool {
    let Some(generic_name) = generic.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let Some(qualified_name) = qualified.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    sources.iter().any(|source| {
        fs::read_to_string(source).is_ok_and(|content| {
            let lower = content.to_ascii_lowercase();
            content.contains(generic_name)
                && content.contains(qualified_name)
                && [
                    "cfg!(windows)",
                    "cfg!(target_os",
                    "#[cfg(windows)]",
                    "#[cfg(target_os",
                    "target_os",
                    "os.name",
                    "sys.platform",
                ]
                .iter()
                .any(|marker| lower.contains(marker))
        })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "platform_goldens_tests.rs"]
mod tests;
