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
