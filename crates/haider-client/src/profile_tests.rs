#![allow(clippy::expect_used)]

//! Unit tests for the shared profile resolver (workspace rule: tests
//! live in `*_tests.rs` files, never inline).

use super::*;

fn env_for(dir: &Path) -> ProfileEnv {
    ProfileEnv {
        profile_dir: Some(dir.to_path_buf()),
        home: None,
        user_profile: None,
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    }
}

fn home_test_root() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("haider-home-")
            .tempdir_in("/tmp")
            .unwrap_or_else(|error| panic!("short HOME tempdir: {error}"))
    }
    #[cfg(windows)]
    {
        tempfile::tempdir().unwrap_or_else(|error| panic!("HOME tempdir: {error}"))
    }
}

// MUTATION CHECK (R8 one-resolver law): drop the version tag or the
// canonicalization step from the profile-id derivation. Expected failure:
// the determinism/path-scoping assertions below (two resolutions of one
// path must agree; two paths must differ).
#[test]
fn profile_id_is_deterministic_and_path_scoped() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let first = root.path().join("one");
    let second = root.path().join("two");
    let a1 = resolve_profile(&env_for(&first)).unwrap_or_else(|error| panic!("{error}"));
    let a2 = resolve_profile(&env_for(&first)).unwrap_or_else(|error| panic!("{error}"));
    let b = resolve_profile(&env_for(&second)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(a1.profile_id, a2.profile_id);
    assert_ne!(a1.profile_id, b.profile_id);
    assert_eq!(a1.profile_id.len(), 64);
    assert!(
        a1.profile_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    // The store directory was created and canonicalized.
    assert!(a1.store_dir.is_dir());
    // Every profile owns a distinct runtime directory and endpoint.
    assert_ne!(a1.runtime_dir, b.runtime_dir);
    assert_ne!(a1.endpoint_path, b.endpoint_path);
    // Unix rendezvous through a filesystem socket below that directory;
    // Windows rendezvous through a profile-hashed named-pipe address.
    #[cfg(unix)]
    {
        assert!(a1.endpoint_path.starts_with(&a1.runtime_dir));
        assert!(b.endpoint_path.starts_with(&b.runtime_dir));
    }
}

#[test]
fn default_home_store_dir_is_preserved() {
    let root = home_test_root();
    let env = ProfileEnv {
        profile_dir: None,
        home: Some(root.path().to_path_buf()),
        user_profile: None,
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    };
    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert!(profile.store_dir.ends_with(".haider/dev-profile"));
    assert!(profile.runtime_dir.starts_with(root.path()));
    assert!(
        profile
            .runtime_dir
            .starts_with(root.path().join(".haider/runtime"))
    );
    assert!(profile.endpoint_path.starts_with(&profile.runtime_dir));
}

/// MUTATION CHECK (hermetic HOME law): restore a temporary-directory choice
/// ahead of HOME. Expected failure: the runtime leaves the only directory the
/// caller supplied as its machine-user home.
#[test]
fn private_home_contains_store_runtime_and_endpoint() {
    let root = home_test_root();
    let private_home = root.path().join("private-home");
    let env = ProfileEnv {
        profile_dir: None,
        home: Some(private_home.clone()),
        user_profile: Some(private_home.clone()),
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    };

    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    let canonical_home = private_home
        .canonicalize()
        .unwrap_or_else(|error| panic!("canonicalize private HOME: {error}"));
    assert!(profile.store_dir.starts_with(canonical_home));
    assert!(profile.runtime_dir.starts_with(&private_home));
    assert!(profile.endpoint_path.starts_with(&private_home));
}

/// A deep private HOME must fail before bind with an actionable typed error;
/// silently selecting `/tmp` would violate the caller's isolation boundary.
#[test]
#[cfg(unix)]
fn overlong_private_home_is_a_typed_error_instead_of_a_tmp_escape() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let private_home = root.path().join("h".repeat(100));
    let env = ProfileEnv {
        profile_dir: None,
        home: Some(private_home),
        user_profile: None,
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    };

    let error = resolve_profile(&env).expect_err("an overlong HOME endpoint must fail early");
    assert!(matches!(
        error,
        ProfileError::RuntimeEndpoint {
            source: haider_platform::EndpointError::AddressTooLong { .. }
        }
    ));
    assert!(error.to_string().contains(RUNTIME_DIR_ENV));
}

#[test]
fn missing_store_dir_and_home_is_a_typed_error() {
    let error = resolve_profile(&ProfileEnv::default());
    assert!(matches!(error, Err(ProfileError::NoStoreDir)));
}

#[test]
fn explicit_profile_dir_precedes_every_home_variable() {
    let root = home_test_root();
    let explicit = root.path().join("explicit");
    let mut env = env_for(&explicit);
    env.user_profile = Some(root.path().join("user-profile"));
    env.home = Some(root.path().join("home"));

    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        profile.store_dir,
        explicit
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonicalize explicit profile: {error}"))
    );
}

#[cfg(windows)]
#[test]
fn userprofile_only_resolves_on_windows() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let user_profile = root.path().join("user-profile");
    let env = ProfileEnv {
        profile_dir: None,
        home: None,
        user_profile: Some(user_profile.clone()),
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    };

    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        profile.store_dir,
        user_profile
            .join(".haider/dev-profile")
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonicalize USERPROFILE store: {error}"))
    );
}

#[cfg(windows)]
#[test]
fn userprofile_precedes_home_on_windows() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let user_profile = root.path().join("user-profile");
    let env = ProfileEnv {
        profile_dir: None,
        home: Some(root.path().join("home")),
        user_profile: Some(user_profile.clone()),
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    };

    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        profile.store_dir,
        user_profile
            .join(".haider/dev-profile")
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonicalize USERPROFILE store: {error}"))
    );
}

#[cfg(unix)]
#[test]
fn home_remains_authoritative_and_userprofile_is_ignored_on_unix() {
    let root = home_test_root();
    let home = root.path().join("home");
    let env = ProfileEnv {
        profile_dir: None,
        home: Some(home.clone()),
        user_profile: Some(root.path().join("user-profile")),
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    };

    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        profile.store_dir,
        home.join(".haider/dev-profile")
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonicalize HOME store: {error}"))
    );

    let userprofile_only = ProfileEnv { home: None, ..env };
    assert!(matches!(
        resolve_profile(&userprofile_only),
        Err(ProfileError::NoStoreDir)
    ));
}

#[test]
fn capture_reads_userprofile_from_a_scrubbed_environment() {
    const CHILD_USERPROFILE_BASENAME: &str = "haider-capture-userprofile-only";
    let captured_user_profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let is_scrubbed_child = std::env::var_os("HOME").is_none()
        && captured_user_profile
            .as_deref()
            .is_some_and(|path| path.ends_with(CHILD_USERPROFILE_BASENAME));
    if is_scrubbed_child {
        let captured = ProfileEnv::capture();
        assert_eq!(captured.profile_dir, None);
        assert_eq!(captured.home, None);
        assert_eq!(captured.user_profile, captured_user_profile);
        #[cfg(windows)]
        assert!(resolve_profile(&captured).is_ok());
        #[cfg(not(windows))]
        assert!(matches!(
            resolve_profile(&captured),
            Err(ProfileError::NoStoreDir)
        ));
        return;
    }

    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let user_profile = root.path().join(CHILD_USERPROFILE_BASENAME);
    let output = std::process::Command::new(
        std::env::current_exe().unwrap_or_else(|error| panic!("current test executable: {error}")),
    )
    .arg("capture_reads_userprofile_from_a_scrubbed_environment")
    .arg("--nocapture")
    .env_clear()
    .env("USERPROFILE", user_profile)
    .output()
    .unwrap_or_else(|error| panic!("run scrubbed capture child: {error}"));
    assert!(
        output.status.success(),
        "scrubbed capture child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn model_precedence_is_env_then_config_then_packaged() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut env = env_for(root.path());

    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(profile.default_model, PACKAGED_DEFAULT_MODEL);
    assert_eq!(profile.default_provider, DEFAULT_PROVIDER);
    assert_eq!(profile.default_max_tokens, DEFAULT_MAX_TOKENS);

    std::fs::write(
        root.path().join(PROFILE_CONFIG_FILE),
        r#"{"default_model":"claude-config-model"}"#,
    )
    .unwrap_or_else(|error| panic!("write config: {error}"));
    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(profile.default_model, "claude-config-model");

    env.model = Some("claude-env-model".into());
    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(profile.default_model, "claude-env-model");
}

#[test]
fn malformed_profile_config_is_loud() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    std::fs::write(root.path().join(PROFILE_CONFIG_FILE), "{not json")
        .unwrap_or_else(|error| panic!("write config: {error}"));
    let error = resolve_profile(&env_for(root.path()));
    assert!(matches!(error, Err(ProfileError::InvalidConfig { .. })));
}

// MUTATION CHECK (R8/D1 short-private-directory rule): change endpoint
// derivation so it escapes the selected runtime directory. Expected failure:
// the containment or fixed-length socket-name assertion below fails.
#[test]
fn endpoint_stays_inside_the_resolved_runtime_directory() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let profile = resolve_profile(&env_for(root.path())).unwrap_or_else(|error| panic!("{error}"));
    #[cfg(unix)]
    {
        assert!(profile.endpoint_path.starts_with(&profile.runtime_dir));
        let name = profile
            .endpoint_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert_eq!(name, "h.sock");
    }
    #[cfg(windows)]
    {
        let endpoint = profile.endpoint_path.to_string_lossy();
        assert!(endpoint.starts_with(r"\\.\pipe\haider-"));
        // Fixed-length pipe name: prefix + 32 hex.
        assert_eq!(endpoint.len(), r"\\.\pipe\haider-".len() + 32);
    }
}

/// MUTATION CHECK (TMPDIR portability): restore the unconditional
/// `/tmp/haider-<uid>` return in `runtime_dir`. Expected failure: the left
/// path starts with `/tmp`, not the private TMPDIR fixture.
#[test]
#[cfg(unix)]
fn runtime_dir_honors_a_verified_private_tmpdir() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let tmpdir = root.path().join("private-tmp");
    std::fs::create_dir(&tmpdir).unwrap_or_else(|error| panic!("create TMPDIR: {error}"));
    std::fs::set_permissions(&tmpdir, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("chmod TMPDIR: {error}"));
    let runtime_env = RuntimeEnv {
        tmpdir: Some(tmpdir.clone()),
        prefix: None,
    };

    assert_eq!(
        runtime_dir(&env_for(root.path()), &runtime_env, "profile-123"),
        tmpdir.join("haider").join("profile-123")
    );
}

/// MUTATION CHECK (TMPDIR trust boundary): accept `TMPDIR` without calling
/// `verified_owner_private`. Expected failure: the left path uses the 0755
/// fixture instead of the per-UID `/tmp` fallback.
#[test]
#[cfg(unix)]
fn runtime_dir_refuses_a_non_private_tmpdir_and_falls_back() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let tmpdir = root.path().join("shared-tmp");
    std::fs::create_dir(&tmpdir).unwrap_or_else(|error| panic!("create TMPDIR: {error}"));
    std::fs::set_permissions(&tmpdir, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("chmod TMPDIR: {error}"));
    let runtime_env = RuntimeEnv {
        tmpdir: Some(tmpdir),
        prefix: None,
    };

    assert_eq!(
        runtime_dir(&env_for(root.path()), &runtime_env, "profile-123"),
        PathBuf::from("/tmp")
            .join(format!("haider-{}", effective_uid()))
            .join("profile-123")
    );
}

#[test]
#[cfg(unix)]
fn runtime_dir_uses_a_verified_prefix_tmp_when_tmpdir_is_unavailable() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let prefix = root.path().join("prefix");
    let prefix_tmp = prefix.join("tmp");
    std::fs::create_dir_all(&prefix_tmp)
        .unwrap_or_else(|error| panic!("create PREFIX/tmp: {error}"));
    std::fs::set_permissions(&prefix_tmp, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("chmod PREFIX/tmp: {error}"));
    let runtime_env = RuntimeEnv {
        tmpdir: None,
        prefix: Some(prefix),
    };

    assert_eq!(
        runtime_dir(&env_for(root.path()), &runtime_env, "profile-123"),
        prefix_tmp.join("haider").join("profile-123")
    );
}

#[test]
fn runtime_override_is_a_root_and_never_collapses_profiles() {
    let root = home_test_root();
    let runtime_root = root.path().join("gate-runtime");
    let mut first_env = env_for(&root.path().join("first"));
    first_env.runtime_dir = Some(runtime_root.clone());
    let mut second_env = env_for(&root.path().join("second"));
    second_env.runtime_dir = Some(runtime_root.clone());

    let first = resolve_profile(&first_env).unwrap_or_else(|error| panic!("{error}"));
    let second = resolve_profile(&second_env).unwrap_or_else(|error| panic!("{error}"));

    assert!(first.runtime_dir.starts_with(&runtime_root));
    assert!(second.runtime_dir.starts_with(&runtime_root));
    assert_ne!(first.runtime_dir, second.runtime_dir);
    assert_eq!(
        first
            .runtime_dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::len),
        Some(RUNTIME_PROFILE_ID_CHARS)
    );
}
