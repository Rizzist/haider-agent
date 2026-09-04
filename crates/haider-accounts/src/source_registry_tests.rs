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

/// Growing the kind enum must not perturb an already-durable source id.
/// The ids below were minted by the pre-Grok/Kimi registry and are pinned as
/// literals so a reordered enum, a renamed `as_str`, or a changed hash
/// domain is caught instead of silently re-identifying every enrolled root.
#[test]
fn existing_codex_and_claude_source_ids_survive_new_kinds_byte_identical() {
    let codex = Path::new("/home/golden/.codex");
    let claude = Path::new("/home/golden/.claude");
    assert_eq!(
        source_id(CredentialSourceKind::CodexHome, codex),
        "src1_7775556b658441dd7feb28f4997d982c19e8754dffd1636db6b28b3dada60e65"
    );
    assert_eq!(
        source_id(CredentialSourceKind::ClaudeFile, claude),
        "src1_ce37ac3c6ccba2b706cf54c6fcce581aff6ec1667284fc7c9040cb63da34f53d"
    );
    // The new kinds are distinct coordinates even over an identical root.
    let ids = [
        source_id(CredentialSourceKind::CodexHome, codex),
        source_id(CredentialSourceKind::ClaudeFile, codex),
        source_id(CredentialSourceKind::GrokHome, codex),
        source_id(CredentialSourceKind::KimiCodeHome, codex),
    ];
    let unique = ids.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), ids.len(), "every kind is its own coordinate");
}

/// The relative credential path is joined onto the root verbatim, so a
/// NESTED path (Kimi's `credentials/kimi-code.json`) needs no new machinery.
#[test]
fn nested_credential_relative_paths_resolve_under_the_enrolled_root() {
    let profile = tempfile::tempdir().expect("profile");
    let root = tempfile::tempdir().expect("root");
    let mut registry = CredentialSourceRegistry::load(profile.path()).expect("registry");
    let record = registry
        .ensure_default(
            CredentialSourceKind::KimiCodeHome,
            root.path(),
            "Kimi Code default",
        )
        .expect("enroll kimi root");
    assert_eq!(
        record.credential_path(),
        root.path().join("credentials").join("kimi-code.json")
    );
    let grok = registry
        .ensure_default(CredentialSourceKind::GrokHome, root.path(), "Grok default")
        .expect("enroll grok root");
    assert_eq!(grok.credential_path(), root.path().join("auth.json"));
}

/// Every kind maps to exactly one origin refresh owner, and enrollment
/// records that owner durably. A new kind without an owner cannot compile.
#[test]
fn every_source_kind_records_its_origin_refresh_owner() {
    let profile = tempfile::tempdir().expect("profile");
    let root = tempfile::tempdir().expect("root");
    let mut registry = CredentialSourceRegistry::load(profile.path()).expect("registry");
    for (kind, owner) in [
        (CredentialSourceKind::CodexHome, "codex"),
        (CredentialSourceKind::ClaudeFile, "claude_code"),
        (CredentialSourceKind::GrokHome, "grok_cli"),
        (CredentialSourceKind::KimiCodeHome, "kimi_cli"),
    ] {
        let record = registry
            .enroll(kind, root.path(), Some("Origin"))
            .expect("enroll kind");
        assert_eq!(record.refresh_owner, kind.refresh_owner());
        assert_eq!(record.refresh_owner.as_str(), owner);
        assert_eq!(record.kind.as_str(), kind.as_str());
    }
    assert_eq!(registry.records().len(), 4);
}
