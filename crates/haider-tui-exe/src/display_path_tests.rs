#![allow(clippy::unwrap_used)]

#[cfg(unix)]
use super::invalid_utf8_workspace_error;
use super::{apply_cwd_for_path, apply_launch_origin_for_path, seed_display_dirs};
use haider_tui::app::AppModel;
use haider_tui::live::workspace_display_path_with_homes;
use std::path::{Path, PathBuf};

#[test]
fn live_demo_and_origin_use_identical_path_projection() {
    for (path, homes) in [
        ("/Users/me/x", vec!["/Users/me"]),
        ("/Users/bob/x", vec!["/Users/me"]),
        ("/users/bo\u{202e}b/x", vec!["/Users/me"]),
        ("/opt/a\u{202e}b\nx", vec!["/Users/me"]),
        ("/Users/me/../bob/x", vec!["/Users/me"]),
        (r"\\?\C:\Users\bob\x", vec!["H:/work", "C:/Users/alice"]),
        ("C:/Users/alice/x", vec!["H:/work", "C:/Users/alice"]),
    ] {
        let homes = homes.into_iter().map(PathBuf::from).collect::<Vec<_>>();
        let expected = workspace_display_path_with_homes(path, &homes);
        let mut live = AppModel::new();
        apply_cwd_for_path(&mut live, Path::new(path), &homes);
        assert_eq!(live.launcher_dir, expected, "live {path}");
        assert_eq!(live.session_dir, expected, "live {path}");
        assert_eq!(
            live.cwd,
            Path::new(path).display().to_string(),
            "real cwd {path}"
        );
        assert!(
            !live.cwd.contains("<user>"),
            "real cwd was redacted: {path}"
        );
        let mut demo = AppModel::new();
        seed_display_dirs(&mut demo, Path::new(path), &homes);
        assert_eq!(demo.launcher_dir, expected, "demo {path}");
        assert_eq!(demo.session_dir, expected, "demo {path}");
        let mut origin_model = AppModel::new();
        apply_launch_origin_for_path(&mut origin_model, Some(Path::new(path)), &homes);
        let origin = origin_model.launch_origin_path.unwrap();
        assert_eq!(
            origin.display.as_deref(),
            Some(expected.as_str()),
            "origin {path}"
        );
        let expected_kind = if expected.starts_with('~') {
            haider_protocol::session::LaunchOriginPathKindV1::HomeRelative
        } else if expected.contains("<user>") {
            haider_protocol::session::LaunchOriginPathKindV1::Redacted
        } else {
            haider_protocol::session::LaunchOriginPathKindV1::Absolute
        };
        assert_eq!(origin.kind, expected_kind, "origin kind {path}");
    }
}

#[cfg(unix)]
#[test]
fn invalid_utf8_workspace_error_uses_the_escaped_display_projection() {
    use std::os::unix::ffi::OsStringExt;
    let path = PathBuf::from(std::ffi::OsString::from_vec(b"/opt/a\xff\nb".to_vec()));
    let message = invalid_utf8_workspace_error(&path);
    assert!(message.contains("/opt/a\u{fffd}\\u{000a}b"), "{message}");
    assert!(!message.contains('\n'));
}
