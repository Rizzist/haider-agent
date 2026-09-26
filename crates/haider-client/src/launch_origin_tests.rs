#![allow(clippy::unwrap_used)]
//! Origin redaction tests (`docs/design/dated-workspace-v1.md` §4, O6).

use std::path::Path;

use super::{
    ORIGIN_DISPLAY_MAX_BYTES, OriginPathKind, OriginSanitizeError, SanitizedOriginPath,
    sanitize_origin_path,
};

fn own_home() -> &'static Path {
    Path::new("/Users/alice")
}

#[test]
fn unavailable_when_no_path_captured() {
    assert_eq!(
        sanitize_origin_path(None, Some(own_home())).unwrap(),
        SanitizedOriginPath {
            kind: OriginPathKind::Unavailable,
            display: None,
        }
    );
}

#[test]
fn own_home_becomes_tilde() {
    let sanitized = sanitize_origin_path(Some(own_home()), Some(own_home())).unwrap();
    assert_eq!(sanitized.kind, OriginPathKind::HomeRelative);
    assert_eq!(sanitized.display.as_deref(), Some("~"));

    let sanitized = sanitize_origin_path(
        Some(Path::new("/Users/alice/Documents/x")),
        Some(own_home()),
    )
    .unwrap();
    assert_eq!(sanitized.kind, OriginPathKind::HomeRelative);
    assert_eq!(sanitized.display.as_deref(), Some("~/Documents/x"));
}

/// Exact component prefix: `/Users/alice2` is NOT inside `/Users/alice`.
#[test]
fn sibling_user_prefix_is_not_own_home() {
    let sanitized =
        sanitize_origin_path(Some(Path::new("/Users/alice2/notes")), Some(own_home())).unwrap();
    assert_eq!(sanitized.kind, OriginPathKind::Redacted);
    assert_eq!(sanitized.display.as_deref(), Some("/Users/<user>/notes"));

    let homoglyph = sanitize_origin_path(
        Some(Path::new("/Users/\u{430}lice/notes")),
        Some(own_home()),
    )
    .unwrap();
    assert_eq!(homoglyph.kind, OriginPathKind::Redacted);
    assert_eq!(homoglyph.display.as_deref(), Some("/Users/<user>/notes"));
}

#[cfg(unix)]
#[test]
fn canonical_path_beneath_symlinked_own_home_stays_home_relative() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let real_home = temp.path().join("real-home");
    let real_project = real_home.join("projects").join("haider");
    std::fs::create_dir_all(&real_project).unwrap();
    let linked_home = temp.path().join("linked-home");
    symlink(&real_home, &linked_home).unwrap();
    let captured = real_project.canonicalize().unwrap();

    let sanitized = sanitize_origin_path(Some(&captured), Some(&linked_home)).unwrap();
    assert_eq!(sanitized.kind, OriginPathKind::HomeRelative);
    assert_eq!(sanitized.display.as_deref(), Some("~/projects/haider"));
}

/// A dated leaf is previewed before its first write creates it. When HOME and
/// the leaf are spelled differently (macOS `/var` vs `/private/var`, Windows
/// `C:\` vs `\\?\C:\`, or a Windows 8.3 `RUNNER~1` short name), the
/// canonical comparison must still recognise the own home via the nearest
/// existing ancestor instead of masking the owner as another user.
#[test]
fn missing_leaf_beneath_differently_spelled_own_home_stays_home_relative() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let leaf = temp
        .path()
        .join("Haider")
        .join("1448-04-07")
        .join("s-0123456789abcdef");
    assert!(!leaf.exists());

    let sanitized = sanitize_origin_path(Some(&leaf), Some(&home)).unwrap();
    assert_eq!(sanitized.kind, OriginPathKind::HomeRelative);
    assert_eq!(
        sanitized.display.as_deref(),
        Some("~/Haider/1448-04-07/s-0123456789abcdef")
    );
    assert!(!leaf.exists(), "sanitising must never create the leaf");
}

#[cfg(unix)]
#[test]
fn missing_leaf_beneath_symlinked_own_home_stays_home_relative() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let real_home = temp.path().join("real-home");
    std::fs::create_dir_all(real_home.join("Documents")).unwrap();
    let linked_home = temp.path().join("linked-home");
    symlink(&real_home, &linked_home).unwrap();
    let leaf = real_home
        .canonicalize()
        .unwrap()
        .join("Documents")
        .join("Haider")
        .join("s-1");

    let sanitized = sanitize_origin_path(Some(&leaf), Some(&linked_home)).unwrap();
    assert_eq!(sanitized.kind, OriginPathKind::HomeRelative);
    assert_eq!(sanitized.display.as_deref(), Some("~/Documents/Haider/s-1"));
}

/// A `..` in the not-yet-existing tail cannot be resolved lexically, so the
/// canonical fallback declines and the path is shown as given.
#[cfg(unix)]
#[test]
fn missing_tail_with_parent_component_is_not_resolved() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let real_home = temp.path().join("real-home");
    std::fs::create_dir_all(&real_home).unwrap();
    let linked_home = temp.path().join("linked-home");
    symlink(&real_home, &linked_home).unwrap();
    let path = real_home.canonicalize().unwrap().join("missing").join("..");

    let sanitized = sanitize_origin_path(Some(&path), Some(&linked_home)).unwrap();
    assert_ne!(sanitized.kind, OriginPathKind::HomeRelative);
}

/// The Windows other-user mask is string-level on the `/`-normalised display,
/// so its forward-slash form is exercised on every host.
#[test]
fn windows_drive_user_homes_are_masked_on_every_host() {
    for (input, expected) in [
        (
            "C:/Users/RUNNER~1/AppData/Local/Temp/x",
            "C:/Users/<user>/AppData/Local/Temp/x",
        ),
        ("D:/Users/bob", "D:/Users/<user>"),
    ] {
        let sanitized = sanitize_origin_path(Some(Path::new(input)), Some(own_home())).unwrap();
        assert_eq!(sanitized.kind, OriginPathKind::Redacted, "for {input}");
        assert_eq!(sanitized.display.as_deref(), Some(expected), "for {input}");
    }
}

/// Native Windows spelling: backslash separators are normalised to `/` and
/// the user component (long or 8.3 short name) is masked.
#[cfg(windows)]
#[test]
fn windows_backslash_user_homes_are_masked_and_normalised() {
    let sanitized = sanitize_origin_path(
        Some(Path::new(r"C:\Users\RUNNER~1\AppData\Local\Temp\x")),
        Some(Path::new(r"C:\Users\nobody-here")),
    )
    .unwrap();
    assert_eq!(sanitized.kind, OriginPathKind::Redacted);
    assert_eq!(
        sanitized.display.as_deref(),
        Some("C:/Users/<user>/AppData/Local/Temp/x")
    );
}

#[test]
fn other_user_homes_are_masked() {
    for (input, expected) in [
        ("/Users/bob/project", "/Users/<user>/project"),
        ("/home/carol", "/home/<user>"),
        ("/home/carol/deep/dir", "/home/<user>/deep/dir"),
    ] {
        let sanitized = sanitize_origin_path(Some(Path::new(input)), Some(own_home())).unwrap();
        assert_eq!(sanitized.kind, OriginPathKind::Redacted, "for {input}");
        assert_eq!(sanitized.display.as_deref(), Some(expected), "for {input}");
    }
}

#[test]
fn non_home_absolute_paths_stay_absolute() {
    let sanitized =
        sanitize_origin_path(Some(Path::new("/opt/data/run")), Some(own_home())).unwrap();
    assert_eq!(sanitized.kind, OriginPathKind::Absolute);
    assert_eq!(sanitized.display.as_deref(), Some("/opt/data/run"));
}

#[test]
fn control_and_bidi_characters_are_escaped() {
    let tricky = "/opt/a\u{061c}b\u{202e}c\nd\u{1b}[31m";
    let sanitized = sanitize_origin_path(Some(Path::new(tricky)), Some(own_home())).unwrap();
    let display = sanitized.display.unwrap();
    assert!(!display.contains('\u{202e}'), "bidi control leaked");
    assert!(!display.contains('\u{061c}'), "Arabic Letter Mark leaked");
    assert!(!display.contains('\n'), "newline leaked");
    assert!(!display.contains('\u{1b}'), "escape leaked");
    assert!(display.contains("\\u{202e}"));
    assert!(display.contains("\\u{061c}"));
    assert!(display.contains("\\u{001b}"));
}

#[test]
fn distinct_paths_stay_distinct_after_escaping() {
    let a = sanitize_origin_path(Some(Path::new("/opt/a\u{1b}b")), None).unwrap();
    let b = sanitize_origin_path(Some(Path::new("/opt/a\\u{001b}b")), None).unwrap();
    assert_ne!(a.display, b.display);
}

#[test]
fn over_limit_display_is_rejected_not_truncated() {
    let long = format!("/opt/{}", "x".repeat(ORIGIN_DISPLAY_MAX_BYTES));
    assert!(matches!(
        sanitize_origin_path(Some(Path::new(&long)), Some(own_home())),
        Err(OriginSanitizeError::DisplayTooLong(_))
    ));
}

#[test]
fn wire_kind_vocabulary_is_pinned() {
    assert_eq!(OriginPathKind::HomeRelative.as_wire(), "home_relative");
    assert_eq!(OriginPathKind::Absolute.as_wire(), "absolute");
    assert_eq!(OriginPathKind::Redacted.as_wire(), "redacted");
    assert_eq!(OriginPathKind::Unavailable.as_wire(), "unavailable");
}
