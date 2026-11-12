#![allow(clippy::unwrap_used)]
//! Origin redaction tests (`docs/design/dated-workspace-v1.md` §4, O6).

use std::path::Path;

use super::{MaskPath, sanitize_with_canonicalizers};
use super::{
    ORIGIN_DISPLAY_MAX_BYTES, OriginPathKind, OriginSanitizeError, SanitizedOriginPath,
    canonicalize_existing_prefix_with, canonicalize_masking_prefix_with,
    display_homes_from_profile, sanitize_origin_path, sanitize_origin_path_with_homes,
    sanitize_with, usable_home,
};

fn projected(path: &str, homes: &[&str]) -> (OriginPathKind, String) {
    let homes = homes.iter().map(Path::new).collect::<Vec<_>>();
    let value = if path.starts_with(r"\\") || path.starts_with("//") {
        sanitize_with(Some(Path::new(path)), &homes, &|_| None).unwrap()
    } else {
        sanitize_origin_path_with_homes(Some(Path::new(path)), &homes).unwrap()
    };
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
        ("/Volumes/Work/Users/bob/x", "/Volumes/Work/Users/<user>/x"),
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
        // A real server called device is indistinguishable from the internal
        // device spelling, so it takes the conservative account mask.
        (
            r"\\device\share\proj\Users\bob\x",
            "//device/share/proj/Users/<user>/x",
        ),
        (r"\\srv\Users\bob\x", "//srv/Users/<user>/x"),
        (r"\\?\UNC\srv\Users\bob\x", "//srv/Users/<user>/x"),
        (r"\\wsl$\Ubuntu\home\bob\x", "//wsl$/Ubuntu/home/<user>/x"),
        (
            r"\\wsl.localhost\Ubuntu\home\bob\x",
            "//wsl.localhost/Ubuntu/home/<user>/x",
        ),
        (r"\\WSL$\Ubuntu\HOME\bob\x", "//WSL$/Ubuntu/HOME/<user>/x"),
        ("/mnt/c/Users/bob/proj", "/mnt/c/Users/<user>/proj"),
        ("/usr/home/bob/x", "/usr/home/<user>/x"),
        ("/export/home/bob/x", "/export/home/<user>/x"),
        (r"C:\Users.\bob\x", "C:/Users./<user>/x"),
        (r"C:\Users \bob\x", "C:/Users /<user>/x"),
        (
            r"C:\Documents and Settings\bob\x",
            "C:/Documents and Settings/<user>/x",
        ),
        (r"C:\msys64\home\bob\x", "C:/msys64/home/<user>/x"),
        ("//srv/share/Users/bob/x", "//srv/share/Users/<user>/x"),
        (r"\\srv\\share\\Users\\bob\\x", "//srv/share/Users/<user>/x"),
        ("/Users/张伟/x", "/Users/<user>/x"),
        ("/Users/%62ob/x", "/Users/<user>/x"),
        ("/Users/bo b/x", "/Users/<user>/x"),
        ("/Users/bo\nb/x", "/Users/<user>/x"),
        ("/Users/bo\u{202e}b/x", "/Users/<user>/x"),
        ("/Users/bob//x/", "/Users/<user>/x"),
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
    #[cfg(unix)]
    assert_eq!(
        projected("//Users//bob//x", &[]),
        (OriginPathKind::Redacted, "/Users/<user>/x".into())
    );
    #[cfg(unix)]
    for spelling in ["///Users/bob/x", "////Users/bob/x"] {
        assert_eq!(
            projected(spelling, &[]),
            (OriginPathKind::Redacted, "/Users/<user>/x".into())
        );
    }
    #[cfg(windows)]
    assert_eq!(
        projected(r"\\Users\bob\x", &[]),
        (OriginPathKind::Absolute, "//Users/bob/x".into())
    );
    #[cfg(windows)]
    assert_eq!(
        projected("//Users//bob//x", &[]),
        (OriginPathKind::Absolute, "//Users/bob/x".into())
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
        projected(r"C:\Users.\alice\x", &[r"C:\Users\alice"]),
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
    let (path, existing, resolved) = if cfg!(windows) {
        (
            Path::new("C:/root/missing/leaf"),
            Path::new("C:/root"),
            Path::new("C:/canonical/missing/leaf"),
        )
    } else {
        (
            Path::new("/root/missing/leaf"),
            Path::new("/root"),
            Path::new("/canonical/missing/leaf"),
        )
    };
    let canonicalize = |candidate: &Path| {
        if candidate == existing {
            Ok(resolved.parent().unwrap().parent().unwrap().to_path_buf())
        } else {
            Err(Error::from(ErrorKind::NotFound))
        }
    };
    assert_eq!(
        canonicalize_existing_prefix_with(path, &canonicalize, cfg!(windows)),
        Some(resolved.to_path_buf())
    );
    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::NotADirectory,
        ErrorKind::Other,
    ] {
        assert_eq!(
            canonicalize_existing_prefix_with(path, &|_| Err(Error::from(kind)), cfg!(windows)),
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
    let denied_child = |candidate: &Path| {
        if candidate == existing {
            Ok(resolved.parent().unwrap().parent().unwrap().to_path_buf())
        } else {
            Err(Error::from(ErrorKind::PermissionDenied))
        }
    };
    assert_eq!(
        canonicalize_existing_prefix_with(path, &denied_child, cfg!(windows)),
        None
    );
    let mask_walk = canonicalize_masking_prefix_with(path, &denied_child, cfg!(windows));
    assert_eq!(mask_walk.resolved, Some(resolved.to_path_buf()));
    assert!(mask_walk.denied);
    assert_eq!(
        canonicalize_existing_prefix_with(
            Path::new("relative/path"),
            &|_| panic!("relative walk"),
            cfg!(windows)
        ),
        None
    );
}

#[cfg(unix)]
#[test]
fn inaccessible_alias_uses_only_its_resolved_ancestor_for_masking() {
    use std::io::{Error, ErrorKind};
    let path = Path::new("/tmp/alias/bob/private/x");
    let resolved = sanitize_with_canonicalizers(Some(path), &[], &|_| None, &|candidate| {
        canonicalize_masking_prefix_with(
            candidate,
            &|probe| {
                if probe == Path::new("/tmp/alias") {
                    Ok(Path::new("/Users").to_path_buf())
                } else {
                    Err(Error::from(ErrorKind::PermissionDenied))
                }
            },
            cfg!(windows),
        )
    })
    .unwrap();
    assert_eq!(resolved.kind, OriginPathKind::Redacted);
    assert_eq!(resolved.display.as_deref(), Some("/Users/<user>/private/x"));
}

#[cfg(unix)]
#[test]
fn aliased_multiuser_home_cannot_collapse_another_account() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("usersalias");
    symlink("/Users", &alias).unwrap();
    assert!(!usable_home(&alias));
    let input = alias.join("bob/x");
    let projected = sanitize_with_canonicalizers(
        Some(&input),
        &[&alias],
        &|candidate| {
            if candidate == alias {
                Some(Path::new("/Users").to_path_buf())
            } else {
                None
            }
        },
        &|_| MaskPath {
            resolved: Some(Path::new("/Users/bob/x").to_path_buf()),
            denied: false,
        },
    )
    .unwrap();
    assert_eq!(projected.display.as_deref(), Some("/Users/<user>/x"));
}

#[cfg(unix)]
#[test]
fn denied_alias_without_a_known_account_root_declines_its_label() {
    let path = Path::new("/tmp/bob/private");
    let result = sanitize_with_canonicalizers(Some(path), &[], &|_| None, &|_| MaskPath {
        resolved: Some(path.to_path_buf()),
        denied: true,
    })
    .unwrap();
    assert_eq!(result.kind, OriginPathKind::Redacted);
    assert_eq!(result.display.as_deref(), Some("<path unavailable>"));
}

#[test]
fn named_pipes_and_literal_unix_question_mark_dirs_keep_their_spelling() {
    let pipe = r"\\.\pipe\haider-0123";
    assert_eq!(projected(pipe, &[]).1, pipe);
    let upper_pipe = r"\\.\PIPE\Haider-0123";
    assert_eq!(projected(upper_pipe, &[]).1, upper_pipe);
    assert_eq!(
        projected("\\\\.\\pipe\\haider-\n0123", &[]).1,
        "\\\\.\\pipe\\haider-\\u{000a}0123"
    );
    #[cfg(unix)]
    for literal in ["/??/x", "//?/x", "/??/Users/bob", "//?/Users/bob"] {
        assert_eq!(projected(literal, &[]).1, literal);
    }
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
fn interior_account_alias_cannot_be_shortened_as_own_home_or_workspace() {
    use std::os::unix::fs::symlink;
    let accounts = if cfg!(target_os = "macos") {
        "/Users"
    } else {
        "/home"
    };
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("accounts");
    symlink(accounts, &alias).unwrap();
    for path in [
        alias.join("display-fixture-other-account/missing/file.rs"),
        alias
            .join("..")
            .join(accounts.trim_start_matches('/'))
            .join("display-fixture-other-account/missing/file.rs"),
    ] {
        let result = sanitize_origin_path(Some(&path), Some(temp.path())).unwrap();
        assert_eq!(result.kind, OriginPathKind::Redacted, "{path:?}");
        assert_eq!(
            result.display,
            Some(format!("{accounts}/<user>/missing/file.rs")),
            "{path:?}"
        );
        assert_eq!(super::workspace_relative_display(&path, temp.path()), None);
    }
}

#[test]
fn redirected_home_alias_uses_the_canonical_account_remainder() {
    let home = Path::new("H:/");
    let profile = Path::new("C:/Users/alice");
    for (account, kind, expected) in [
        ("alice", OriginPathKind::HomeRelative, "~/<user>/private"),
        (
            "bob",
            OriginPathKind::Redacted,
            "//srv/Users/<user>/private",
        ),
    ] {
        let path = home.join(format!("account-alias/{account}/private"));
        let resolved = Path::new("//srv/Users").join(format!("{account}/private"));
        let result = sanitize_with_canonicalizers(
            Some(&path),
            &[home, profile],
            &|candidate| {
                if candidate == path {
                    Some(resolved.clone())
                } else if candidate == home {
                    Some(Path::new("//srv/Users").to_path_buf())
                } else {
                    None
                }
            },
            &|_| MaskPath {
                resolved: None,
                denied: false,
            },
        )
        .unwrap();
        assert_eq!(result.kind, kind, "{account}");
        assert_eq!(result.display.as_deref(), Some(expected), "{account}");
    }
}

#[test]
fn canonical_windows_account_names_are_not_win32_aliases() {
    let home = Path::new("C:/Users/alice");
    for account in ["alice.", "alice "] {
        let path = home.join(format!("project/account-alias/{account}/private"));
        let resolved = Path::new("C:/Users").join(format!("{account}/private"));
        let result = sanitize_with_canonicalizers(
            Some(&path),
            &[home],
            &|candidate| {
                if candidate == path {
                    Some(resolved.clone())
                } else if candidate == home {
                    Some(home.to_path_buf())
                } else {
                    None
                }
            },
            &|_| MaskPath {
                resolved: None,
                denied: false,
            },
        )
        .unwrap();
        assert_eq!(result.kind, OriginPathKind::Redacted, "{account:?}");
        assert_eq!(result.display.as_deref(), Some("C:/Users/<user>/private"));
    }
    // Ordinary requested Win32 spelling remains an alias for the own home
    // when there is no conflicting canonical identity.
    assert_eq!(
        super::display_path_with_homes(Path::new("C:/Users/alice./file.rs"), &[home.to_path_buf()]),
        "~/file.rs"
    );
}

#[test]
fn wsl_account_case_does_not_supply_an_own_home_identity() {
    let home = Path::new("//wsl$/Ubuntu/home/alice");
    for (account, expected) in [
        ("alice", "~/notes"),
        ("ALICE", "//WSL$/Ubuntu/home/<user>/notes"),
    ] {
        let path = Path::new("//WSL$/Ubuntu/home").join(account).join("notes");
        let projection =
            sanitize_with_canonicalizers(Some(&path), &[home], &|_| None, &|_| MaskPath {
                resolved: None,
                denied: false,
            })
            .unwrap();
        assert_eq!(projection.display.as_deref(), Some(expected));
    }
    // An interior alias to the case-distinct account must not restore that
    // account name through the requested own-home remainder.
    let path = home.join("project/alias/ALICE/notes");
    let other = Path::new("//wsl$/Ubuntu/home/ALICE/notes");
    let projection = sanitize_with_canonicalizers(
        Some(&path),
        &[home],
        &|candidate| {
            if candidate == path {
                Some(other.to_path_buf())
            } else if candidate == home {
                Some(home.to_path_buf())
            } else {
                None
            }
        },
        &|_| MaskPath {
            resolved: None,
            denied: false,
        },
    )
    .unwrap();
    assert_eq!(projection.kind, OriginPathKind::Redacted);
    assert_eq!(
        projection.display.as_deref(),
        Some("//wsl$/Ubuntu/home/<user>/notes")
    );
    assert_eq!(
        super::display_path_with_homes(
            Path::new("c:/USERS/ALICE/notes"),
            &["C:/Users/alice".into()]
        ),
        "~/notes"
    );
}

#[test]
fn redirected_wsl_home_keeps_case_distinct_accounts_separate() {
    let home = Path::new("H:/");
    let profile = Path::new("C:/Users/alice");
    for (account, expected) in [
        ("alice", "~/<user>/notes"),
        ("ALICE", "//wsl.localhost/Ubuntu/home/<user>/notes"),
    ] {
        let path = home.join("account-alias").join(account).join("notes");
        let root = Path::new("//wsl.localhost/Ubuntu/home");
        let resolved = root.join(account).join("notes");
        let projection = sanitize_with_canonicalizers(
            Some(&path),
            &[home, profile],
            &|candidate| {
                if candidate == path {
                    Some(resolved.clone())
                } else if candidate == home {
                    Some(root.to_path_buf())
                } else {
                    None
                }
            },
            &|_| MaskPath {
                resolved: None,
                denied: false,
            },
        )
        .unwrap();
        assert_eq!(projection.display.as_deref(), Some(expected));
    }
}

#[test]
fn missing_windows_suffix_uses_requested_namespace_naming() {
    use std::io::{Error, ErrorKind};
    let ordinary = std::env::temp_dir().join("winpath-prefix-fixture");
    let namespace = Path::new("//?/C:/Users");
    for (root, windows, expected) in [
        (ordinary.as_path(), true, Path::new("C:/Users/alice/notes")),
        (
            ordinary.as_path(),
            false,
            Path::new("C:/Users/alice. /notes. "),
        ),
        (namespace, true, Path::new("C:/Users/alice. /notes. ")),
    ] {
        let path = root.join("alice. ").join("notes. ");
        let resolve = |candidate: &Path| {
            if candidate == root {
                Ok(Path::new("C:/Users").to_path_buf())
            } else {
                Err(Error::from(ErrorKind::NotFound))
            }
        };
        assert_eq!(
            super::canonicalize_existing_prefix_with(&path, &resolve, windows).as_deref(),
            Some(expected)
        );
        let masked = super::canonicalize_masking_prefix_with(&path, &resolve, windows);
        assert!(!masked.denied);
        assert_eq!(masked.resolved.as_deref(), Some(expected));
        if windows {
            let raw = if root == namespace {
                Path::new("//?/C:/Users/alice. /notes. ")
            } else {
                Path::new("C:/Users/alice. /notes. ")
            };
            let home = Path::new("C:/Users/alice");
            let projection = sanitize_with_canonicalizers(
                Some(raw),
                &[home],
                &|candidate| {
                    if candidate == raw {
                        Some(expected.to_path_buf())
                    } else if candidate == home {
                        Some(home.to_path_buf())
                    } else {
                        None
                    }
                },
                &|_| MaskPath {
                    resolved: None,
                    denied: false,
                },
            )
            .unwrap();
            let wanted = if root == namespace {
                "C:/Users/<user>/notes. "
            } else {
                "~/notes. "
            };
            assert_eq!(projection.display.as_deref(), Some(wanted));
        }
    }
    // A missing current-directory alias supplies no new directory identity.
    let path = ordinary.join(" . ").join("notes");
    let resolve = |candidate: &Path| {
        if candidate == ordinary {
            Ok(ordinary.clone())
        } else {
            Err(Error::from(ErrorKind::NotFound))
        }
    };
    assert!(super::canonicalize_existing_prefix_with(&path, &resolve, true).is_none());
    assert!(
        super::canonicalize_masking_prefix_with(&path, &resolve, true)
            .resolved
            .is_none()
    );
}

#[test]
fn missing_windows_parent_alias_cannot_supply_a_canonical_identity() {
    use std::io::{Error, ErrorKind};
    // The resolver is injected; the host-native root only supplies portable
    // Path traversal. Win32 component semantics are selected explicitly.
    // Namespace suffixes preserve literal names instead of using Win32
    // trimming. The parent-alias guard must also protect those requests.
    for root in [
        std::env::temp_dir(),
        Path::new("//?/C:/Users").to_path_buf(),
        Path::new("//./C:/Users").to_path_buf(),
    ] {
        for component in [".. ", "...", " . . "] {
            let path = root.join("missing").join(component).join("Users/bob/leaf");
            let resolve = |candidate: &Path| {
                if candidate == root {
                    Ok(root.join("canonical"))
                } else {
                    Err(Error::from(ErrorKind::NotFound))
                }
            };
            assert_eq!(
                canonicalize_existing_prefix_with(&path, &resolve, true),
                None,
                "{component}"
            );
            assert_eq!(
                canonicalize_masking_prefix_with(&path, &resolve, true).resolved,
                None,
                "{component}"
            );
            // POSIX permits these literal directory names; they remain a tail.
            assert!(
                canonicalize_existing_prefix_with(&path, &resolve, false).is_some(),
                "{component}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn denied_interior_account_alias_keeps_the_resolved_account_mask() {
    let home = Path::new("C:/Users/alice");
    let path = home.join("accounts/bob/private");
    let result = sanitize_with_canonicalizers(Some(&path), &[home], &|_| None, &|_| MaskPath {
        resolved: Some(Path::new("D:/Users/bob/private").to_path_buf()),
        denied: true,
    })
    .unwrap();
    assert_eq!(result.kind, OriginPathKind::Redacted);
    assert_eq!(result.display.as_deref(), Some("D:/Users/<user>/private"));
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

#[cfg(all(unix, target_os = "macos"))]
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
///
/// This exercises real temporary-directory and canonicalization semantics,
/// so a Windows-home simulation on Unix must skip the Unix entry point.
fn assert_missing_leaf_beneath_differently_spelled_own_home_stays_home_relative() {
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
fn missing_leaf_beneath_differently_spelled_own_home_stays_home_relative() {
    assert_missing_leaf_beneath_differently_spelled_own_home_stays_home_relative();
}

#[cfg(windows)]
#[test]
fn missing_leaf_beneath_differently_spelled_own_home_stays_home_relative() {
    assert_missing_leaf_beneath_differently_spelled_own_home_stays_home_relative();
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

#[test]
fn redirected_home_uses_the_actual_account_component() {
    for (home, profile, path) in [
        (r"\\srv\Users", "C:/Users/alice", r"\\srv\Users\bob\alice\x"),
        ("//srv/Users", "C:/Users/alice", "//srv/Users/bob/alice/x"),
        (
            r"\\?\UNC\srv\Users",
            "C:/Users/alice",
            r"\\?\UNC\srv\Users\bob\alice\x",
        ),
        (r"\\SRV\USERS", "C:/Users/alice", r"\\srv\users\BOB\ALICE\x"),
        (
            r"\\srv\Users. ",
            "C:/Users/alice",
            r"\\srv\Users. \bob\alice\x",
        ),
        ("H:/", "C:/Users/home", "H:/msys64/home/bob/x"),
        (
            r"\\?\Volume{abc}\",
            "C:/Users/alice",
            r"\\?\Volume{abc}\cache\alice\Users\bob\x",
        ),
        (
            r"\\?\Volume{abc}\cache\alice",
            "C:/Users/alice",
            r"\\?\Volume{abc}\cache\alice\Users\bob\x",
        ),
        ("H:/", "C:/Users/alice/nested", "H:/Users/bob/nested/x"),
    ] {
        let actual = projected(path, &[home, profile]).1;
        assert!(
            !actual.to_ascii_lowercase().contains("bob"),
            "{home} {profile} {path}: {actual}"
        );
    }
    for (profile, path, expected) in [
        ("C:/Users/alice", "H:/Users./alice/x", "~/Users./<user>/x"),
        ("C:/Users/alice", "H:/Users /alice/x", "~/Users /<user>/x"),
        ("C:/Users/alice.", "H:/Users/alice/x", "~/Users/<user>/x"),
        (
            "C:/Users/alice",
            "H:/Documents and Settings/alice/x",
            "~/Documents and Settings/<user>/x",
        ),
        (
            "C:/Users/alice",
            "H:/msys64/home/alice/x",
            "~/msys64/home/<user>/x",
        ),
    ] {
        assert_eq!(projected(path, &["H:/", profile]).1, expected);
    }
}

#[test]
fn account_roots_share_home_and_workspace_validation() {
    for (root, path) in [
        (r"\\srv\share\msys64", r"\\srv\share\msys64\home\bob\x"),
        (r"\\srv\msys64\home", r"\\srv\msys64\home\bob\x"),
        (
            r"\\srv\share\Documents and Settings",
            r"\\srv\share\Documents and Settings\bob\x",
        ),
        (
            r"\\srv\Documents and Settings",
            r"\\srv\Documents and Settings\bob\x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\msys64",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\msys64\home\bob\x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\msys64\home",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\msys64\home\bob\x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Documents and Settings",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Documents and Settings\bob\x",
        ),
        ("C:/msys64", "C:/msys64/home/bob/x"),
        ("C:/msys64/home", "C:/msys64/home/bob/x"),
        (r"c:\MSYS64\HOME", r"C:\msys64\home\BOB\x"),
        (r"C:\msys64. \home. ", r"C:\msys64\home\bob\x"),
        (r"\\?\C:\msys64\home", r"C:\msys64\home\bob\x"),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Users\bob\x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Users",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Users\bob\x",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\Users\bob\x",
        ),
        (
            r"\??\GLOBALROOT\Device\HarddiskVolume3\Users",
            r"\??\GLOBALROOT\Device\HarddiskVolume3\Users\bob\x",
        ),
        (
            r"\\?\globalroot\device\harddiskvolume3\USERS",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Users\BOB\x",
        ),
        (
            r"\\.\GLOBALROOT\Device\HarddiskVolume3\Users",
            r"\\.\GLOBALROOT\Device\HarddiskVolume3\Users\bob\x",
        ),
        (r"\\?\Volume{abc}\", r"\\?\Volume{abc}\Users\bob\x"),
        (r"\\srv\Users", r"\\srv\Users\bob\x"),
    ] {
        assert!(!usable_home(Path::new(root)), "home {root}");
        assert!(
            super::is_multiuser_workspace_root(Path::new(root)),
            "workspace {root}"
        );
        assert!(
            !projected(path, &[root])
                .1
                .to_ascii_lowercase()
                .contains("bob"),
            "{root} {path}"
        );
        assert_eq!(
            super::workspace_relative_display(Path::new(path), Path::new(root)),
            None,
            "{root} {path}"
        );
        assert_eq!(
            super::workspace_relative_display(Path::new(root), Path::new(root)).as_deref(),
            Some("."),
            "bare parent {root}"
        );
        assert!(
            !projected(path, &["D:/safe", root])
                .1
                .to_ascii_lowercase()
                .contains("bob")
        );
    }
    for (root, path) in [
        (
            "C:/msys64/home/alice/project",
            "C:/msys64/home/alice/project/src/x.rs",
        ),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Users\bob\project",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Users\bob\project\src\x.rs",
        ),
        (r"\\srv\alice\project", r"\\srv\alice\project\src\x.rs"),
        ("D:/safe", "D:/safe/src/x.rs"),
        (
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\project",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\project\src\x.rs",
        ),
    ] {
        assert!(usable_home(Path::new(root)), "{root}");
        assert!(
            !super::is_multiuser_workspace_root(Path::new(root)),
            "{root}"
        );
        assert_eq!(projected(path, &[root]).1, "~/src/x.rs");
        assert_eq!(
            super::workspace_relative_display(Path::new(path), Path::new(root)).as_deref(),
            Some("src/x.rs")
        );
    }
}

#[test]
fn dot_device_namespace_is_not_a_traversal_component() {
    for (root, path) in [
        (r"\\.\C:\w", r"\\.\C:\w\src\x.rs"),
        (r"\\.\C:\w", r"c:\W\src\x.rs"),
        (r"C:\w", r"\\.\c:\W\src\x.rs"),
        (r"\\.\UNC\srv\share\w", r"\\srv\share\w\src\x.rs"),
        (
            r"\\.\GLOBALROOT\Device\HarddiskVolume3\Users\alice\project",
            r"\\.\GLOBALROOT\Device\HarddiskVolume3\Users\alice\project\src\x.rs",
        ),
    ] {
        assert_eq!(
            super::workspace_relative_display(Path::new(path), Path::new(root)).as_deref(),
            Some("src/x.rs"),
            "{root} {path}"
        );
    }
    for path in [
        r"\\.\C:\w\..\Users\bob\x",
        r"\\.\C:\w\sub\.. \x",
        r"\\.\C:\w\.\x",
    ] {
        assert_eq!(
            super::workspace_relative_display(Path::new(path), Path::new(r"\\.\C:\w")),
            None
        );
    }
}

#[cfg(windows)]
#[test]
fn forward_dot_device_namespace_keeps_windows_workspace_labels() {
    for (root, path) in [
        ("//./C:/w", "//./C:/w/src/x.rs"),
        ("//./C:/w", r"c:\W\src\x.rs"),
        (r"C:\w", "//./c:/W/src/x.rs"),
        ("//./UNC/srv/share/w", r"\\srv\share\w\src\x.rs"),
        (
            "//./GLOBALROOT/Device/HarddiskVolume3/Users/alice/project",
            "//./GLOBALROOT/Device/HarddiskVolume3/Users/alice/project/src/x.rs",
        ),
    ] {
        assert_eq!(
            super::workspace_relative_display(Path::new(path), Path::new(root)).as_deref(),
            Some("src/x.rs"),
            "{root} {path}"
        );
    }
    for path in [
        "//./C:/w/../Users/bob/x",
        "//./C:/w/sub/.. /x",
        "//./C:/w/./x",
    ] {
        assert_eq!(
            super::workspace_relative_display(Path::new(path), Path::new("//./C:/w")),
            None,
            "the namespace dot is allowed, actual traversal is not: {path}"
        );
    }
}

#[test]
fn canonical_case_alias_requires_matching_directory_identity() -> std::io::Result<()> {
    let source = super::DisplayPath::parse(Path::new("//srv/Users/ALICE/private"));
    let home = super::DisplayPath::parse(Path::new("//srv/Users/alice"));
    let identity = |left: &Path, right: &Path| {
        assert_eq!(left, Path::new("//srv/Users/ALICE"));
        assert_eq!(right, Path::new("//srv/Users/alice"));
        Some(false)
    };
    assert_eq!(
        source.canonical_relative_to_with_identity(&home, &identity),
        None
    );
    assert_eq!(
        source.canonical_account_identity(0, &home, &identity),
        Some(false)
    );
    let other_drive = super::DisplayPath::parse(Path::new("C:/Users/alice"));
    assert_eq!(
        source.canonical_account_identity(0, &other_drive, &|_, _| panic!("unrelated roots")),
        None
    );
    for same in [Some(true), None] {
        assert_eq!(
            source.canonical_relative_to_with_identity(&home, &|_, _| same),
            Some(vec!["private".to_owned()])
        );
    }
    let temp = tempfile::tempdir()?;
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    std::fs::create_dir(&first)?;
    std::fs::create_dir(&second)?;
    let file = temp.path().join("file");
    std::fs::write(&file, b"fixture")?;
    assert_eq!(super::directory_identity(&file, &file), Some(false));
    assert_eq!(super::directory_identity(&first, &first), Some(true));
    assert_eq!(super::directory_identity(&first, &second), Some(false));
    assert_eq!(
        super::directory_identity(&first, &temp.path().join("missing")),
        Some(false)
    );
    assert_eq!(
        super::directory_identity(
            &temp.path().join("missing"),
            &temp.path().join("also-missing")
        ),
        None
    );
    Ok(())
}
