//! Unit tests for the shared profile resolver (workspace rule: tests
//! live in `*_tests.rs` files, never inline).

use super::*;

fn env_for(dir: &Path) -> ProfileEnv {
    ProfileEnv {
        profile_dir: Some(dir.to_path_buf()),
        home: None,
        model: None,
        xdg_runtime_dir: None,
    }
}

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
    // Same runtime dir, distinct sockets per profile.
    assert_eq!(a1.runtime_dir, b.runtime_dir);
    assert_ne!(a1.endpoint_path, b.endpoint_path);
}

#[test]
fn default_home_store_dir_is_preserved() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let env = ProfileEnv {
        profile_dir: None,
        home: Some(root.path().to_path_buf()),
        model: None,
        xdg_runtime_dir: None,
    };
    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    assert!(profile.store_dir.ends_with(".haider/dev-profile"));
}

#[test]
fn missing_store_dir_and_home_is_a_typed_error() {
    let error = resolve_profile(&ProfileEnv::default());
    assert!(matches!(error, Err(ProfileError::NoStoreDir)));
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

#[test]
fn runtime_dir_is_never_env_overridable_and_defaults_to_tmp_uid() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut env = env_for(root.path());
    // Even a hostile XDG value on macOS must not move the runtime dir;
    // on Linux only a VERIFIED owner-private dir may.
    env.xdg_runtime_dir = Some(PathBuf::from("/definitely/not/private"));
    let profile = resolve_profile(&env).unwrap_or_else(|error| panic!("{error}"));
    let expected = PathBuf::from("/tmp").join(format!("haider-{}", effective_uid()));
    assert_eq!(profile.runtime_dir, expected);
    assert!(profile.endpoint_path.starts_with(&expected));
    let name = profile
        .endpoint_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    assert!(name.starts_with("haider-") && name.ends_with(".sock"));
    // Fixed-length socket name: "haider-" + 32 hex + ".sock".
    assert_eq!(name.len(), "haider-".len() + 32 + ".sock".len());
}
