//! W5f-4 — the file vault is the default secret store: round-trip, atomic
//! replace, alias recovery, restrictive permissions, and honest absence.
#![allow(clippy::expect_used)]

use haider_protocol::ids::CredentialAlias;

use crate::file_vault::FileVault;
use crate::vault::Vault;

fn temp_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// MUTATION CHECK: make `put`'s write non-atomic by writing straight to the
/// target (skip the temp+rename). This test still passes on the happy path;
/// the property it PINS is the round-trip and byte-exactness — the mutation
/// that matters here is `resolve` returning the wrong bytes.
/// Expected runtime failure if `resolve` reads a different alias's file:
/// the secret below comes back wrong.
/// Verified by revert on 2026-07-30.
#[test]
fn put_then_resolve_returns_the_exact_secret() {
    let root = temp_root();
    let vault = FileVault::new(root.path().join("vault"));
    let alias = CredentialAlias::new("openai-oauth");
    vault.put(&alias, b"secret-bytes-0").expect("put");
    let handle = vault.resolve(&alias).expect("resolve");
    assert_eq!(handle.expose_secret(), b"secret-bytes-0");

    // Replace is last-writer-wins and torn-write-free.
    vault
        .put(&alias, b"secret-bytes-1-longer")
        .expect("replace");
    assert_eq!(
        vault.resolve(&alias).expect("resolve").expose_secret(),
        b"secret-bytes-1-longer"
    );
}

/// MUTATION CHECK: make `resolve` return `Ok` with empty bytes for a missing
/// alias instead of `Err`. Expected runtime failure: the assertion that a
/// never-stored alias is an error fails.
/// Verified by revert on 2026-07-30.
#[test]
fn resolving_a_missing_alias_is_an_error() {
    let root = temp_root();
    let vault = FileVault::new(root.path().join("vault"));
    assert!(vault.resolve(&CredentialAlias::new("nope")).is_err());
    // A missing root lists as empty, never an error.
    assert!(vault.list().expect("list empty").is_empty());
}

/// MUTATION CHECK: make `list` return the raw filenames (hex) instead of
/// decoding them. Expected runtime failure: the recovered alias no longer
/// equals the stored one.
/// Verified by revert on 2026-07-30.
#[test]
fn list_recovers_aliases_in_lexical_order() {
    let root = temp_root();
    let vault = FileVault::new(root.path().join("vault"));
    for alias in ["anthropic-oauth", "openai-oauth", "openai-oauth-2"] {
        vault.put(&CredentialAlias::new(alias), b"x").expect("put");
    }
    let listed: Vec<String> = vault
        .list()
        .expect("list")
        .into_iter()
        .map(|alias| alias.as_str().to_owned())
        .collect();
    assert_eq!(
        listed,
        vec![
            "anthropic-oauth".to_owned(),
            "openai-oauth".to_owned(),
            "openai-oauth-2".to_owned(),
        ]
    );

    vault
        .delete(&CredentialAlias::new("openai-oauth"))
        .expect("delete");
    let after: Vec<String> = vault
        .list()
        .expect("list")
        .into_iter()
        .map(|alias| alias.as_str().to_owned())
        .collect();
    assert_eq!(
        after,
        vec!["anthropic-oauth".to_owned(), "openai-oauth-2".to_owned()]
    );
    // Deleting an absent alias succeeds.
    vault
        .delete(&CredentialAlias::new("ghost"))
        .expect("idempotent delete");
}

/// MUTATION CHECK: drop the `set_permissions(0o700)` / `mode(0o600)` calls.
/// Expected runtime failure: the directory or file is world-readable, so
/// the mode assertions below fail — a secret store must not be group/other
/// readable.
/// Verified by revert on 2026-07-30.
#[cfg(unix)]
#[test]
fn the_vault_directory_and_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_root();
    let dir = root.path().join("vault");
    let vault = FileVault::new(&dir);
    vault
        .put(&CredentialAlias::new("openai-oauth"), b"s")
        .expect("put");

    let dir_mode = std::fs::metadata(&dir)
        .expect("dir meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "vault dir must be owner-only: {dir_mode:o}"
    );

    let file = dir.join(format!("{}.vault", hex::encode("openai-oauth")));
    let file_mode = std::fs::metadata(&file)
        .expect("file meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        file_mode, 0o600,
        "vault file must be owner-only: {file_mode:o}"
    );
}

/// Non-vault strays in the directory are ignored by `list`, never an error.
#[test]
fn list_ignores_non_vault_files() {
    let root = temp_root();
    let dir = root.path().join("vault");
    let vault = FileVault::new(&dir);
    vault
        .put(&CredentialAlias::new("openai-oauth"), b"s")
        .expect("put");
    std::fs::write(dir.join("README.txt"), b"not a secret").expect("stray");
    std::fs::write(dir.join("zz.vault"), b"bad hex stem").expect("bad hex");
    let listed = vault.list().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].as_str(), "openai-oauth");
}

/// MUTATION CHECK: replace the file-backed lease with a per-instance or
/// process-local gate. Expected RUNTIME failure: two independently opened
/// vaults both enter the same rotating-refresh critical section.
#[test]
fn independent_file_vaults_share_the_refresh_rotation_lease() {
    let root = temp_root();
    let directory = root.path().join("vault");
    let first = FileVault::new(&directory);
    let second = FileVault::new(&directory);
    let alias = CredentialAlias::new("kimi-oauth-work");

    let lease = first
        .try_refresh_lock(&alias)
        .expect("first lock attempt")
        .expect("first vault owns the lease");
    assert!(
        second
            .try_refresh_lock(&alias)
            .expect("contended lock attempt")
            .is_none(),
        "an independently opened vault must observe the same OS lease"
    );
    drop(lease);
    assert!(
        second
            .try_refresh_lock(&alias)
            .expect("released lock attempt")
            .is_some(),
        "dropping the lease releases it for another daemon"
    );
}
