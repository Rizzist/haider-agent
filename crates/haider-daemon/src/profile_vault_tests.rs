//! R10 pins for [`ProfileVault`]: profile isolation over one shared backing
//! vault, legacy raw-key fallback, and dual-key deletion.
#![allow(clippy::expect_used)]

use super::*;
use haider_accounts::MemoryVault;

fn vaults() -> (Arc<MemoryVault>, ProfileVault, ProfileVault) {
    let backing = Arc::new(MemoryVault::new());
    let profile_a = ProfileVault::new(backing.clone() as Arc<dyn Vault>, "profile-a");
    let profile_b = ProfileVault::new(backing.clone() as Arc<dyn Vault>, "profile-b");
    (backing, profile_a, profile_b)
}

/// MUTATION CHECK (R10): make `ProfileVault::scoped` return the alias
/// unchanged (identity mapping). Expected runtime failure: profile B's `put`
/// under the same global alias overwrites profile A's secret, so the first
/// assertion below reads `B_SECRET` instead of `A_SECRET`; the delete
/// assertion then reports profile A's item gone after B's remove.
/// Verified by revert on 2026-07-30.
#[test]
fn same_global_alias_in_two_profiles_never_collides() {
    let (_backing, profile_a, profile_b) = vaults();
    let alias = CredentialAlias::new("work");

    profile_a.put(&alias, b"A_SECRET").expect("a put");
    profile_b.put(&alias, b"B_SECRET").expect("b put");

    assert_eq!(
        profile_a.resolve(&alias).expect("a resolve").expose_secret(),
        b"A_SECRET",
        "profile B's login must not clobber profile A's secret"
    );
    assert_eq!(
        profile_b.resolve(&alias).expect("b resolve").expose_secret(),
        b"B_SECRET"
    );

    profile_b.delete(&alias).expect("b delete");
    assert_eq!(
        profile_a.resolve(&alias).expect("a survives").expose_secret(),
        b"A_SECRET",
        "a remove in profile B must not delete profile A's item"
    );
    assert!(profile_b.resolve(&alias).is_err());
}

/// MUTATION CHECK: remove the raw-key fallback arm from
/// `ProfileVault::resolve`. Expected runtime failure: the legacy item stored
/// under the raw physical alias resolves as `CredentialMissing`.
/// Verified by revert on 2026-07-30.
#[test]
fn legacy_raw_items_resolve_and_delete_through_the_scoped_vault() {
    let (backing, profile_a, _profile_b) = vaults();
    // Pre-scoping install: the item lives under the raw (old physical) alias.
    let legacy = CredentialAlias::new("anthropic-0f3a2b1c4d5e6f70");
    backing.put(&legacy, b"LEGACY_SECRET").expect("legacy put");

    assert_eq!(
        profile_a
            .resolve(&legacy)
            .expect("legacy fallback")
            .expose_secret(),
        b"LEGACY_SECRET"
    );

    // Deletion clears the legacy key too (dual-key delete).
    profile_a.delete(&legacy).expect("delete");
    assert!(backing.resolve(&legacy).is_err(), "raw item must be gone");
}

#[test]
fn list_shows_own_and_legacy_items_but_never_other_profiles() {
    let (backing, profile_a, profile_b) = vaults();
    profile_a
        .put(&CredentialAlias::new("work"), b"a")
        .expect("a put");
    profile_b
        .put(&CredentialAlias::new("home"), b"b")
        .expect("b put");
    backing
        .put(&CredentialAlias::new("anthropic-legacyhash"), b"l")
        .expect("legacy put");

    let listed = profile_a.list().expect("list");
    let names: Vec<&str> = listed.iter().map(CredentialAlias::as_str).collect();
    assert!(names.contains(&"work"));
    assert!(names.contains(&"anthropic-legacyhash"));
    assert!(
        !names.iter().any(|name| name.contains("home")),
        "another profile's scoped item leaked into list: {names:?}"
    );
}

#[test]
fn scoped_key_format_is_the_durable_v1_contract() {
    let alias = CredentialAlias::new("work");
    let scoped = scoped_vault_alias("profile-a", &alias);
    let (prefix, rest) = scoped
        .as_str()
        .split_once("::")
        .expect("scoped key has one :: separator");
    assert_eq!(prefix.len(), 16, "fixed-length profile hash");
    assert!(prefix.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(rest, "work");
    // Deterministic: the same profile + alias always addresses the same item.
    assert_eq!(
        scoped_vault_alias("profile-a", &alias).as_str(),
        scoped.as_str()
    );
    assert_ne!(
        scoped_vault_alias("profile-b", &alias).as_str(),
        scoped.as_str()
    );
}
