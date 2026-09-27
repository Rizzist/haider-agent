#![allow(clippy::unwrap_used)]
//! Origin redaction tests (`docs/design/dated-workspace-v1.md` §4, O6).

use std::path::Path;

use super::{
    ORIGIN_DISPLAY_MAX_BYTES, OriginPathKind, OriginSanitizeError, SanitizedOriginPath,
    canonicalize_existing_prefix_with, display_homes_from_profile, sanitize_origin_path,
    sanitize_origin_path_with_homes, sanitize_with, usable_home,
};

fn projected(path: &str, homes: &[&str]) -> (OriginPathKind, String) {
    let homes = homes.iter().map(Path::new).collect::<Vec<_>>();
    let value = sanitize_origin_path_with_homes(Some(Path::new(path)), &homes).unwrap();
    (value.kind, value.display.unwrap())
}

#[test]
fn invalid_homes_fall_through_to_mask_and_valid_fallback() {
    for invalid in [
        "",
        ".",
        "/",
        "C:/",
        r"\\?\C:\",
        "\\\\?\\",
        "\\\\.\\",
        r"\\?\Volume{abc}\",
        "relative/home",
        "/Users",
        "/home",
        "/var",
        "/var/home",
        "/private",
        "/private/var/home",
        "/System/Volumes/Data/Users",
        "C:/Users",
        r"\\srv\share\Users",
    ] {
        assert!(!usable_home(Path::new(invalid)), "{invalid}");
        assert_eq!(
            projected("/Users/bob/secret", &[invalid]),
            (OriginPathKind::Redacted, "/Users/<user>/secret".into())
        );
        assert_eq!(
            projected("C:/Users/alice/x", &[invalid, "C:/Users/alice"]),
            (OriginPathKind::HomeRelative, "~/x".into())
        );
    }
    assert!(!usable_home(Path::new("H:/")));
    assert_eq!(projected("H:/work/x", &["H:/"]).1, "H:/work/x");
}

#[test]
fn windows_home_resolution_keeps_profile_with_redirected_or_invalid_home() {
    let mut profile = crate::profile::ProfileEnv {
        profile_dir: None,
        home: Some("H:/work".into()),
        user_profile: Some("C:/Users/alice".into()),
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    };
    let homes = display_homes_from_profile(&profile, true);
    assert_eq!(
        homes,
        vec![
            std::path::PathBuf::from("H:/work"),
            std::path::PathBuf::from("C:/Users/alice")
        ]
    );
    profile.home = Some("".into());
    assert_eq!(
        display_homes_from_profile(&profile, true),
        vec![std::path::PathBuf::from("C:/Users/alice")]
    );
    profile.home = None;
    assert_eq!(
        display_homes_from_profile(&profile, true),
        vec![std::path::PathBuf::from("C:/Users/alice")]
    );
    profile.home = Some(".".into());
    assert_eq!(
        display_homes_from_profile(&profile, true),
        vec![std::path::PathBuf::from("C:/Users/alice")]
    );
    profile.home = Some("/Users/me".into());
    assert_eq!(
        display_homes_from_profile(&profile, true),
        vec![std::path::PathBuf::from("C:/Users/alice")]
    );
    profile.home = Some("H:/".into());
    assert_eq!(
        display_homes_from_profile(&profile, true),
        vec![
            std::path::PathBuf::from("H:/"),
            std::path::PathBuf::from("C:/Users/alice")
        ]
    );
    assert_eq!(
        projected("H:/work/x", &["H:/", "C:/Users/alice"]),
        (OriginPathKind::HomeRelative, "~/work/x".into())
    );
    assert_eq!(
        projected("H:/Users/bob/x", &["H:/", "C:/Users/alice"]),
        (OriginPathKind::Redacted, "H:/Users/<user>/x".into())
    );
    assert_eq!(
        projected("H:/Users/alice/x", &["H:/", "C:/Users/alice"]),
        (OriginPathKind::HomeRelative, "~/Users/<user>/x".into())
    );
    assert_eq!(
        projected("C:/Users/alice/x", &["H:/", "C:/Users/alice"]),
        (OriginPathKind::HomeRelative, "~/x".into())
    );
    assert_eq!(
        projected(r"\\srv\share\work\x", &[r"\\srv\share", "C:/Users/alice"]),
        (OriginPathKind::HomeRelative, "~/work/x".into())
    );
    assert_eq!(
        projected(
            r"\\srv\share\Users\alice\x",
            &[r"\\srv\share", "C:/Users/alice"]
        ),
        (OriginPathKind::HomeRelative, "~/Users/<user>/x".into())
    );
    assert_eq!(
        projected(r"\\srv\c$\Users\bob\x", &[r"\\srv\c$", "C:/Users/alice"]),
        (OriginPathKind::Redacted, "//srv/c$/Users/<user>/x".into())
    );
    profile.home = Some(r"\\srv\share".into());
    assert_eq!(
        display_homes_from_profile(&profile, true),
        vec![
            std::path::PathBuf::from(r"\\srv\share"),
            std::path::PathBuf::from("C:/Users/alice")
        ]
    );
    for admin in [r"\\srv\c$", r"\\srv\ADMIN$", r"\\srv\IPC$"] {
        profile.home = Some(admin.into());
        assert_eq!(
            display_homes_from_profile(&profile, true),
            vec![std::path::PathBuf::from("C:/Users/alice")],
            "{admin}"
        );
    }
    profile.home = Some("H:/".into());
    profile.user_profile = Some("D:/Profiles/alice".into());
    assert_eq!(
        display_homes_from_profile(&profile, true),
        vec![
            std::path::PathBuf::from("H:/"),
            std::path::PathBuf::from("D:/Profiles/alice")
        ]
    );
    assert_eq!(
        projected("D:/Profiles/alice/x", &["H:/", "D:/Profiles/alice"]).1,
        "~/x"
    );
    assert_eq!(
        projected("H:/work/x", &["H:/", "D:/Profiles/alice"]).1,
        "~/work/x"
    );
    assert!(display_homes_from_profile(&profile, false).is_empty());
}

#[cfg(unix)]
#[test]
fn lexical_parent_and_dot_are_resolved_before_both_checks() {
    assert_eq!(
        projected("/Users/me/../bob/x", &["/Users/me"]),
        (OriginPathKind::Redacted, "/Users/<user>/x".into())
    );
    assert_eq!(
        projected("/Users/me/./x", &["/Users/me"]),
        (OriginPathKind::HomeRelative, "~/x".into())
    );
    assert_eq!(
        projected("/Users/bob/../me/x", &["/Users/me"]),
        (OriginPathKind::HomeRelative, "~/x".into())
    );
}

#[cfg(unix)]
#[test]
fn concurrent_projections_keep_home_candidates_isolated() {
    std::thread::scope(|scope| {
        for own in ["alice", "bob", "me"] {
            scope.spawn(move || {
                let home = format!("/Users/{own}");
                let own_path = format!("{home}/work");
                let other_path = if own == "bob" {
                    "/Users/alice/work"
                } else {
                    "/Users/bob/work"
                };
                for _ in 0..32 {
                    assert_eq!(projected(&own_path, &[&home]).1, "~/work");
                    assert_eq!(projected(other_path, &[&home]).1, "/Users/<user>/work");
                }
            });
        }
    });
}

#[test]
fn every_known_other_user_root_and_case_are_masked() {
    for (input, expected) in [
        ("/users/bob/x", "/users/<user>/x"),
        ("/USERS/bob/x", "/USERS/<user>/x"),
        (
            "/System/Volumes/Data/Users/bob/x",
            "/System/Volumes/Data/Users/<user>/x",
        ),
        (
            "/system/volumes/data/users/bob/x",
            "/system/volumes/data/users/<user>/x",
        ),
        ("/var/home/bob/x", "/var/home/<user>/x"),
        ("/private/var/home/bob/x", "/private/var/home/<user>/x"),
        ("C:/users/bob/x", "C:/users/<user>/x"),
        (r"C:\Users\bob\x", "C:/Users/<user>/x"),
        ("d:/USERS/bob/x", "d:/USERS/<user>/x"),
        (r"\\?\C:\Users\bob\x", "C:/Users/<user>/x"),
        (r"\\.\C:\Users\bob\x", "C:/Users/<user>/x"),
        (r"\??\C:\Users\bob\x", "C:/Users/<user>/x"),
        (r"\\srv\c$\Users\bob\x", "//srv/c$/Users/<user>/x"),
        (r"\\?\UNC\srv\c$\Users\bob\x", "//srv/c$/Users/<user>/x"),
        (r"\\?\unc\SRV\C$\users\bob\x", "//SRV/C$/users/<user>/x"),
        (
            r"\\?\Volume{abc}\Users\bob\x",
            "//device/Volume{abc}/Users/<user>/x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\Users\bob\x",
            "//device/GLOBALROOT/Device/HarddiskVolume1/Users/<user>/x",
        ),
        (r"\\srv\share\Users\bob\x", "//srv/share/Users/<user>/x"),
        ("//srv/share/Users/bob/x", "//srv/share/Users/<user>/x"),
        (r"\\srv\\share\\Users\\bob\\x", "//srv/share/Users/<user>/x"),
        ("/Users/张伟/x", "/Users/<user>/x"),
        ("/Users/%62ob/x", "/Users/<user>/x"),
        ("/Users/bo b/x", "/Users/<user>/x"),
        ("/Users/bo\nb/x", "/Users/<user>/x"),
        ("/Users/bo\u{202e}b/x", "/Users/<user>/x"),
        ("/Users/bob//x/", "/Users/<user>/x"),
        ("//Users//bob//x", "/Users/<user>/x"),
    ] {
        assert_eq!(
            projected(input, &[]),
            (OriginPathKind::Redacted, expected.into()),
            "{input}"
        );
    }
    #[cfg(unix)]
    assert_eq!(
        projected("//users/ME/x", &["/Users/me"]),
        (OriginPathKind::Redacted, "/users/<user>/x".into())
    );
    // A bare root has no account component to conceal.
    assert_eq!(
        projected("/Users", &[]),
        (OriginPathKind::Absolute, "/Users".into())
    );
    assert_eq!(
        projected("/home", &[]),
        (OriginPathKind::Absolute, "/home".into())
    );
    // An actual UNC server called Users is not the local Users directory.
    assert_eq!(
        projected(r"\\Users\bob\proj", &[]),
        (OriginPathKind::Absolute, "//Users/bob/proj".into())
    );
}

#[test]
fn verbatim_plain_unc_and_redirected_homes_share_one_projection() {
    for (input, expected) in [
        (r"\\?\D:\x", "D:/x"),
        (r"\\?\UNC\srv\share\x", "//srv/share/x"),
        (
            r"\\?\Volume{abc}\projects\x",
            "//device/Volume{abc}/projects/x",
        ),
        (r"\\?\C:\Users\alice\x", "~/x"),
        ("c:/users/ALICE/x", "~/x"),
        ("C:/Users/alice//x/", "~/x"),
        ("H:/work/x", "~/x"),
    ] {
        let homes = ["H:/work", "C:/Users/alice"];
        let expected_kind = if expected.starts_with('~') {
            OriginPathKind::HomeRelative
        } else {
            OriginPathKind::Absolute
        };
        assert_eq!(
            projected(input, &homes),
            (expected_kind, expected.into()),
            "{input}"
        );
    }
    // The longer matching home wins when HOME and USERPROFILE nest.
    assert_eq!(
        projected(
            "C:/Users/alice/work/x",
            &["C:/Users/alice", "C:/Users/alice/work"]
        ),
        (OriginPathKind::HomeRelative, "~/x".into())
    );
    assert_eq!(
        projected(
            r"\\srv\share\Users\alice\x",
            &[r"\\srv\share\Users\alice", "C:/Users/alice"]
        ),
        (OriginPathKind::HomeRelative, "~/x".into())
    );
    assert_eq!(
        projected("C:/Users/alice/x", &[r"\\?\C:\Users\alice"]),
        (OriginPathKind::HomeRelative, "~/x".into())
    );
    assert_eq!(
        projected(
            r"\\?\Volume{abc}\Users\alice\x",
            &[r"\\?\Volume{abc}\Users\alice"]
        ),
        (OriginPathKind::HomeRelative, "~/x".into())
    );
}

#[test]
fn relative_path_never_uses_process_cwd_for_canonical_identity() {
    let relative = Path::new("RealHome/newdir/x");
    let home = Path::new("/tmp/RealHome");
    let result = sanitize_with(Some(relative), &[home], &|_| {
        panic!("relative input was canonicalized")
    })
    .unwrap();
    assert_eq!(result.kind, OriginPathKind::Absolute);
    assert_eq!(result.display.as_deref(), Some("RealHome/newdir/x"));
    assert_eq!(projected("a/../b", &["/Users/me"]).1, "a/../b");
    assert_eq!(projected("", &["/Users/me"]).1, "");
}

#[test]
fn ancestor_walk_continues_only_for_not_found() {
    use std::io::{Error, ErrorKind};
    let path = Path::new("/root/missing/leaf");
    let canonicalize = |candidate: &Path| {
        if candidate == Path::new("/root") {
            Ok(Path::new("/canonical").to_path_buf())
        } else {
            Err(Error::from(ErrorKind::NotFound))
        }
    };
    assert_eq!(
        canonicalize_existing_prefix_with(path, &canonicalize),
        Some(Path::new("/canonical/missing/leaf").to_path_buf())
    );
    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::NotADirectory,
        ErrorKind::Other,
    ] {
        assert_eq!(
            canonicalize_existing_prefix_with(path, &|_| Err(Error::from(kind))),
            None,
            "{kind:?}"
        );
        let display = sanitize_with(
            Some(Path::new("/Users/bob/private")),
            &[Path::new("/Users/me")],
            &|_| None,
        )
        .unwrap();
        assert_eq!(
            display.display.as_deref(),
            Some("/Users/<user>/private"),
            "{kind:?} must fall back to lexical masking"
        );
    }
    assert_eq!(
        canonicalize_existing_prefix_with(Path::new("relative/path"), &|_| panic!("relative walk")),
        None
    );
}

#[cfg(unix)]
#[test]
fn symlink_loop_does_not_prevent_lexical_own_home_display() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    symlink("loop", temp.path().join("loop")).unwrap();
    let value = sanitize_origin_path(Some(&temp.path().join("loop/x")), Some(temp.path())).unwrap();
    assert_eq!(value.kind, OriginPathKind::HomeRelative);
    assert_eq!(value.display.as_deref(), Some("~/loop/x"));
}

#[cfg(unix)]
#[test]
fn missing_parent_tail_declines_and_unrelated_absolute_paths_stay_visible() {
    for path in ["/tmp/workspace-8", "/opt/x", "/opt/a b", "D:/proj"] {
        assert_eq!(projected(path, &["/Users/me"]).1, path);
    }
    assert_eq!(
        projected("/Users/me2/x", &["/Users/me"]).1,
        "/Users/<user>/x"
    );
    assert_eq!(projected("/Users/me/x", &["/Users/me/"]).1, "~/x");
    assert_eq!(projected("/Users/me", &["/Users/me"]).1, "~");
    assert_eq!(projected("/Users/me/x", &[]).1, "/Users/<user>/x");
    assert_eq!(
        projected("/Users/me/x", &["/missing-home"]).1,
        "/Users/<user>/x"
    );
    assert_eq!(projected("/missing-home/x", &["/missing-home"]).1, "~/x");
}

#[test]
fn simulated_windows_short_name_and_verbatim_home_resolve_to_owner() {
    let home = Path::new("C:/Users/alice");
    let path = Path::new(r"\\?\C:\Users\ALICE~1\missing\x");
    let result = sanitize_with(Some(path), &[home], &|p| {
        let raw = p.to_string_lossy();
        if raw.contains("ALICE~1") {
            Some(Path::new("C:/Users/alice/missing/x").to_path_buf())
        } else {
            Some(Path::new("C:/Users/alice").to_path_buf())
        }
    })
    .unwrap();
    // Lexical comparison cannot expand the short name on a non-Windows host;
    // the injected canonicalizer models the Windows filesystem result.
    assert_eq!(result.display.as_deref(), Some("~/missing/x"));
    let short_home = Path::new(r"C:\Users\ALICE~1");
    let long_path = Path::new("C:/Users/alice/missing/x");
    let reverse = sanitize_with(Some(long_path), &[short_home], &|candidate| {
        if candidate == short_home {
            Some(Path::new("C:/Users/alice").to_path_buf())
        } else {
            Some(long_path.to_path_buf())
        }
    })
    .unwrap();
    assert_eq!(reverse.display.as_deref(), Some("~/missing/x"));
}

#[test]
fn canonical_alias_to_another_home_masks_the_resolved_account() {
    let alias = Path::new("/alias/work");
    let value = sanitize_with(Some(alias), &[], &|path| {
        assert_eq!(path, alias);
        Some(Path::new("/Users/bob/work").to_path_buf())
    })
    .unwrap();
    assert_eq!(value.kind, OriginPathKind::Redacted);
    assert_eq!(value.display.as_deref(), Some("/Users/<user>/work"));
}

#[cfg(unix)]
#[test]
fn firmlink_spellings_keep_own_existing_and_missing_leaf() {
    let home = Path::new("/private/var/home/me");
    for path in ["/var/home/me/existing", "/var/home/me/missing/leaf"] {
        let result = sanitize_with(Some(Path::new(path)), &[home], &|candidate| {
            let text = candidate.to_string_lossy();
            if text.starts_with("/private/var/home/") {
                Some(candidate.to_path_buf())
            } else {
                Some(Path::new(&text.replace("/var/home/", "/private/var/home/")).to_path_buf())
            }
        })
        .unwrap();
        assert_eq!(result.kind, OriginPathKind::HomeRelative, "{path}");
        assert!(
            result
                .display
                .as_deref()
                .is_some_and(|display| display.starts_with("~/")),
            "{path}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apfs_case_alias_keeps_existing_and_missing_own_paths() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("MiXeDHome");
    std::fs::create_dir(&home).unwrap();
    let case_alias = temp.path().join("mixedhome");
    if case_alias.canonicalize().is_err() {
        return; // Case-sensitive APFS volume: this alias does not exist.
    }
    assert_eq!(
        sanitize_origin_path(Some(&home), Some(&case_alias))
            .unwrap()
            .display
            .as_deref(),
        Some("~")
    );
    assert_eq!(
        sanitize_origin_path(Some(&home.join("missing/leaf")), Some(&case_alias))
            .unwrap()
            .display
            .as_deref(),
        Some("~/missing/leaf")
    );
}

#[cfg(windows)]
#[test]
fn native_windows_verbatim_and_userprofile_forms() {
    assert_eq!(
        projected(r"C:\Users\alice", &[r"C:\Users\alice"]),
        (OriginPathKind::HomeRelative, "~".into())
    );
    assert_eq!(
        projected(r"\\?\C:\Users\alice\x", &[r"C:\Users\alice"]),
        (OriginPathKind::HomeRelative, "~/x".into())
    );
    assert_eq!(
        projected(r"\\srv\share\Users\bob\x", &[]),
        (
            OriginPathKind::Redacted,
            "//srv/share/Users/<user>/x".into()
        )
    );
    assert_eq!(projected(r"dir\file", &[]).1, "dir/file");
    assert_eq!(
        projected(r"C:\Users\alice\..\bob\x", &[r"C:\Users\alice"]),
        (OriginPathKind::Redacted, "C:/Users/<user>/x".into())
    );
    assert_eq!(
        projected(r"C:\Users\alice\Documents\x", &[r"C:\Users\alice"]),
        (OriginPathKind::HomeRelative, "~/Documents/x".into())
    );
    assert_eq!(
        projected("D:/proj", &[r"C:\Users\alice"]),
        (OriginPathKind::Absolute, "D:/proj".into())
    );
}

#[cfg(unix)]
fn own_home() -> &'static Path {
    Path::new("/Users/alice")
}

#[cfg(windows)]
fn own_home() -> &'static Path {
    Path::new(r"C:\Users\alice")
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

#[cfg(unix)]
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

/// A terminal `..` in a not-yet-existing tail has no reliable identity, so
/// the projection declines it instead of guessing a home-relative label.
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

#[cfg(unix)]
#[test]
fn unix_colon_and_backslash_filename_stays_distinct_from_windows_drive_path() {
    assert_eq!(
        projected("a:b\\c", &[]).1,
        "a:b\\u{005c}c",
        "a Unix relative filename is not a Windows drive"
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_account_segment_is_masked_before_lossy_display() {
    use std::os::unix::ffi::OsStringExt;
    let raw = std::ffi::OsString::from_vec(b"/Users/bo\xffb/x".to_vec());
    let value = sanitize_origin_path(Some(Path::new(&raw)), None).unwrap();
    assert_eq!(value.kind, OriginPathKind::Redacted);
    assert_eq!(value.display.as_deref(), Some("/Users/<user>/x"));
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
