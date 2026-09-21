use super::*;

fn fixture_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "haider-platform-golden-{name}-{}-{}",
        std::process::id(),
        line!()
    ));
    fs::create_dir_all(root.join("tests/fixtures")).expect("create fixture tree");
    fs::create_dir_all(root.join("src")).expect("create source tree");
    root
}

#[test]
fn generic_platform_shell_fixture_is_rejected() {
    let root = fixture_root("shell");
    fs::write(
        root.join("tests/fixtures/tools.json"),
        r#"{"shell":"/bin/zsh"}"#,
    )
    .expect("write fixture");
    let result = audit(&root);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(result.sensitive, 1);
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn sibling_without_target_selecting_consumer_is_rejected() {
    let root = fixture_root("unselected");
    fs::write(
        root.join("tests/fixtures/tools.json"),
        r#"{"shell":"/bin/sh"}"#,
    )
    .expect("write generic fixture");
    fs::write(
        root.join("tests/fixtures/tools.windows.json"),
        r#"{"shell":"PowerShell"}"#,
    )
    .expect("write qualified fixture");
    let result = audit(&root);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(result.violations.len(), 1);
    assert!(result.violations[0].contains("no source consumer selects"));
}

#[test]
fn target_selected_fixture_family_is_accepted() {
    let root = fixture_root("selected");
    fs::write(
        root.join("tests/fixtures/tools.json"),
        r#"{"shell":"/bin/sh"}"#,
    )
    .expect("write generic fixture");
    fs::write(
        root.join("tests/fixtures/tools.windows.json"),
        r#"{"shell":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"}"#,
    )
    .expect("write qualified fixture");
    fs::write(
        root.join("src/consumer.rs"),
        r#"if cfg!(windows) { include_str!("../tests/fixtures/tools.windows.json") } else { include_str!("../tests/fixtures/tools.json") }"#,
    )
    .expect("write consumer");
    let result = audit(&root);
    let _ = fs::remove_dir_all(&root);
    assert!(result.violations.is_empty(), "{:?}", result.violations);
    assert_eq!(result.sensitive, 2);
    assert_eq!(result.parameterized, 2);
}

#[test]
fn windows_path_in_generic_fixture_is_rejected() {
    let root = fixture_root("windows-path");
    fs::write(
        root.join("tests/fixtures/result.txt"),
        r"installed at C:\Program Files\Haider",
    )
    .expect("write fixture");
    let result = audit(&root);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(result.violations.len(), 1);
    assert!(result.violations[0].contains("Windows path separator"));
    assert!(contains_windows_path(r#"{"path":"D:/Haider/cache"}"#));
    assert!(contains_windows_path(r#"{"path":"\\\\server\\share"}"#));
}

#[test]
fn escaped_json_punctuation_is_not_a_windows_path() {
    let root = fixture_root("escaped-json");
    fs::write(
        root.join("tests/fixtures/result.json"),
        r#"{"detail":"Authorization:Bearer secret; path:\"quoted\""}"#,
    )
    .expect("write fixture");
    let result = audit(&root);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(result.sensitive, 0);
    assert!(result.violations.is_empty());
}
