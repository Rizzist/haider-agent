#![allow(clippy::expect_used)]

use std::fs;

use super::*;

#[test]
fn enrollment_is_canonical_idempotent_and_path_stable() {
    let profile = tempfile::tempdir().expect("profile");
    let root = tempfile::tempdir().expect("root");
    let mut registry = CredentialSourceRegistry::load(profile.path()).expect("registry");
    let first = registry
        .enroll(CredentialSourceKind::CodexHome, root.path(), Some("Work"))
        .expect("first");
    let second = registry
        .enroll(
            CredentialSourceKind::CodexHome,
            root.path(),
            Some("ignored"),
        )
        .expect("second");
    assert_eq!(first.id, second.id);
    assert_eq!(registry.records().len(), 1);
    assert_eq!(
        first.root,
        fs::canonicalize(root.path()).expect("canonical")
    );

    let reloaded = CredentialSourceRegistry::load(profile.path()).expect("reload");
    assert_eq!(reloaded.records(), registry.records());
}

#[test]
fn operator_enrollment_rejects_missing_roots() {
    let profile = tempfile::tempdir().expect("profile");
    let mut registry = CredentialSourceRegistry::load(profile.path()).expect("registry");
    let error = registry
        .enroll(
            CredentialSourceKind::ClaudeFile,
            profile.path().join("missing"),
            None,
        )
        .expect_err("missing");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(registry.records().is_empty());
}

#[test]
fn source_identity_does_not_include_scan_or_token_state() {
    let profile = tempfile::tempdir().expect("profile");
    let root = tempfile::tempdir().expect("root");
    let mut registry = CredentialSourceRegistry::load(profile.path()).expect("registry");
    let mut source = registry
        .enroll(CredentialSourceKind::CodexHome, root.path(), None)
        .expect("enroll");
    let id = source.id.clone();
    source.health = CredentialSourceHealth::Ready;
    source.last_refreshed_at_ms = Some(42);
    source.account_alias = Some("codex-abcd".into());
    registry.update(source).expect("update");
    assert_eq!(registry.records()[0].id, id);
}

#[test]
fn explicit_reenrollment_revives_a_tombstone_without_changing_identity() {
    let profile = tempfile::tempdir().expect("profile");
    let root = tempfile::tempdir().expect("root");
    let mut registry = CredentialSourceRegistry::load(profile.path()).expect("registry");
    let mut source = registry
        .enroll(CredentialSourceKind::CodexHome, root.path(), None)
        .expect("enroll");
    let id = source.id.clone();
    source.enabled = false;
    source.health = CredentialSourceHealth::SourceGone;
    source.account_alias = Some("codex-abcd".into());
    registry.update(source).expect("tombstone");

    let revived = registry
        .enroll(CredentialSourceKind::CodexHome, root.path(), Some("Back"))
        .expect("reenroll");
    assert_eq!(revived.id, id);
    assert!(revived.enabled);
    assert_eq!(revived.health, CredentialSourceHealth::Pending);
    assert_eq!(revived.account_alias.as_deref(), Some("codex-abcd"));
    assert_eq!(revived.label, "Back");
}

#[test]
fn automatic_default_discovery_does_not_revive_an_operator_tombstone() {
    let profile = tempfile::tempdir().expect("profile");
    let root = tempfile::tempdir().expect("root");
    let mut registry = CredentialSourceRegistry::load(profile.path()).expect("registry");
    let mut source = registry
        .ensure_default(
            CredentialSourceKind::CodexHome,
            root.path(),
            "Default Codex",
        )
        .expect("default");
    source.enabled = false;
    source.health = CredentialSourceHealth::SourceGone;
    registry.update(source.clone()).expect("tombstone");

    let discovered = registry
        .ensure_default(
            CredentialSourceKind::CodexHome,
            root.path(),
            "Default Codex",
        )
        .expect("rediscover default");
    assert_eq!(discovered.id, source.id);
    assert!(!discovered.enabled);
}
