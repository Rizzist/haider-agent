#![allow(clippy::expect_used)]

use haider_protocol::credential::{
    AccountIdentity, AuthMethod, CredentialDescriptor, CredentialStatus,
};
use haider_protocol::ids::CredentialAlias;
use haider_store::{CachedModels, ProviderModelCacheKey, Store};
use rusqlite::Connection;

/// The cache is durable, provider-scoped, and a refresh replaces every
/// mutable field.
///
/// MUTATION CHECK: replace the `ON CONFLICT(provider) DO UPDATE SET ...`
/// clause in `Store::put_provider_models` with `DO NOTHING`. Expected runtime
/// failure: the final read returns the first catalog, ETag, and timestamp
/// instead of the replacement provenance.
/// Verified by revert on 2026-07-30.
#[test]
fn provider_model_cache_is_durable_provider_scoped_and_replaced() {
    let root = tempfile::tempdir().expect("tempdir");
    let first_json = r#"[{"slug":"frontier-a","source":"provider-fixture-one"}]"#;
    let other_json = r#"[{"slug":"frontier-b","source":"provider-fixture-two"}]"#;
    let replacement_json = r#"[{"slug":"frontier-c","source":"provider-fixture-replacement"}]"#;

    {
        let store = Store::open(root.path()).expect("open store");
        assert_eq!(
            store.provider_models("provider-one").expect("empty read"),
            None
        );
        store
            .put_provider_models("provider-one", first_json, Some(r#"W/"first""#), 101)
            .expect("put first catalog");
        store
            .put_provider_models("provider-two", other_json, None, 202)
            .expect("put second provider");
    }

    let store = Store::open(root.path()).expect("reopen store");
    assert_eq!(
        store.provider_models("provider-one").expect("durable read"),
        Some(CachedModels {
            models_json: first_json.to_owned(),
            etag: Some(r#"W/"first""#.to_owned()),
            fetched_at_ms: 101,
        })
    );
    assert_eq!(
        store.provider_models("provider-two").expect("scoped read"),
        Some(CachedModels {
            models_json: other_json.to_owned(),
            etag: None,
            fetched_at_ms: 202,
        })
    );

    store
        .put_provider_models(
            "provider-one",
            replacement_json,
            Some(r#"W/"replacement""#),
            303,
        )
        .expect("replace catalog");
    assert_eq!(
        store
            .provider_models("provider-one")
            .expect("replacement read"),
        Some(CachedModels {
            models_json: replacement_json.to_owned(),
            etag: Some(r#"W/"replacement""#.to_owned()),
            fetched_at_ms: 303,
        })
    );
    assert_eq!(
        store
            .provider_models("provider-two")
            .expect("isolation read"),
        Some(CachedModels {
            models_json: other_json.to_owned(),
            etag: None,
            fetched_at_ms: 202,
        }),
        "replacing one provider must not alter another provider's provenance"
    );
}

#[test]
fn authenticated_catalog_cache_round_trips_by_provider_and_account() {
    let root = tempfile::tempdir().expect("tempdir");
    let descriptor = |alias: &str, account_id: &str| CredentialDescriptor {
        incarnation: 0,
        credential_digest: None,
        alias: CredentialAlias::new(alias),
        provider: "openai-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "same display".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: Some(AccountIdentity {
            email: None,
            display_name: None,
            account_id: Some(account_id.into()),
            plan: None,
            issuer: Some("issuer".into()),
            captured_at: 1,
            verified: false,
        }),
        created_at_ms: None,
    };
    let a = ProviderModelCacheKey::for_account(
        "openai-oauth",
        &descriptor("shared-alias", "account-a"),
    );
    let b = ProviderModelCacheKey::for_account(
        "openai-oauth",
        &descriptor("shared-alias", "account-b"),
    );
    let other = ProviderModelCacheKey::for_account(
        "anthropic-oauth",
        &descriptor("shared-alias", "account-a"),
    );
    assert_ne!(
        a, b,
        "a reused alias must not reuse another identity's catalog"
    );
    let mut api_key = descriptor("same-api-alias", "ignored");
    api_key.auth_method = AuthMethod::ApiKey;
    api_key
        .account_identity
        .as_mut()
        .expect("identity")
        .account_id = None;
    let first_api_key = ProviderModelCacheKey::for_account("gemini", &api_key);
    api_key
        .account_identity
        .as_mut()
        .expect("identity")
        .captured_at = 2;
    assert_eq!(
        first_api_key,
        ProviderModelCacheKey::for_account("gemini", &api_key),
        "timestamps are never cache identity"
    );
    api_key.incarnation += 1;
    assert_ne!(
        first_api_key,
        ProviderModelCacheKey::for_account("gemini", &api_key),
        "replacing an API key must change its cache even if the display identity is unchanged"
    );
    {
        let store = Store::open(root.path()).expect("open store");
        store
            .put_provider_models(
                "account:12:openai-oauth:shared-alias",
                "[\"legacy\"]",
                None,
                5,
            )
            .expect("put old alias-only catalog");
        store
            .put_provider_models(a.as_str(), "[\"a\"]", Some("etag-a"), 10)
            .expect("put A");
        store
            .put_provider_models(b.as_str(), "[\"b\"]", Some("etag-b"), 20)
            .expect("put B");
        store
            .put_provider_models(other.as_str(), "[\"other\"]", None, 30)
            .expect("put other");
    }
    let store = Store::open(root.path()).expect("reopen store");
    assert!(
        store
            .provider_models("account:12:openai-oauth:shared-alias")
            .expect("old alias key")
            .is_some(),
        "the migration leaves old rows present but no v3 key reads them"
    );
    assert_eq!(
        store.provider_models("openai-oauth").expect("legacy key"),
        None
    );
    assert_eq!(
        store
            .provider_models(a.as_str())
            .expect("A read")
            .expect("A cache exists")
            .models_json,
        "[\"a\"]"
    );
    assert_eq!(
        store
            .provider_models(b.as_str())
            .expect("B read")
            .expect("B cache exists")
            .models_json,
        "[\"b\"]"
    );
    assert_eq!(
        store
            .provider_models(other.as_str())
            .expect("other read")
            .expect("other cache exists")
            .models_json,
        "[\"other\"]"
    );
    store
        .put_provider_models(a.as_str(), "[\"a-new\"]", None, 40)
        .expect("replace A");
    assert_eq!(
        store
            .provider_models(a.as_str())
            .expect("A replacement")
            .expect("replacement A cache exists")
            .models_json,
        "[\"a-new\"]"
    );
    assert_eq!(
        store
            .provider_models(b.as_str())
            .expect("B preserved")
            .expect("preserved B cache exists")
            .models_json,
        "[\"b\"]"
    );
}

#[test]
fn catalog_prune_bounds_replaced_and_removed_accounts_and_unread_legacy_rows() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let live = "account-v3:live";
    for key in [
        live,
        "account-v3:replaced",
        "account-v2:replaced",
        "account-v2:removed-custom-provider",
        "account:12:openai-oauth:old",
        "openai-oauth",
        "public-provider",
    ] {
        store
            .put_provider_models(key, "[]", None, 1)
            .expect("seed row");
    }
    store
        .prune_provider_model_caches(&[live.into()], &["openai-oauth".into()])
        .expect("prune");
    for key in [
        "account-v3:replaced",
        "account-v2:replaced",
        "account-v2:removed-custom-provider",
        "account:12:openai-oauth:old",
        "openai-oauth",
    ] {
        assert!(
            store.provider_models(key).expect("pruned read").is_none(),
            "{key}"
        );
    }
    for key in [live, "public-provider"] {
        assert!(
            store.provider_models(key).expect("retained read").is_some(),
            "{key}"
        );
    }
    store
        .prune_provider_model_caches(&[], &[])
        .expect("remove last account");
    assert!(store.provider_models(live).expect("removed read").is_none());
}

#[test]
fn v2_account_catalog_rows_are_pruned_after_v3_migration() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    store
        .put_provider_models("account-v2:legacy", "[]", None, 1)
        .expect("legacy row");
    store
        .put_provider_models("account-v3:current", "[]", None, 1)
        .expect("current row");
    store
        .prune_provider_model_caches(&["account-v3:current".into()], &[])
        .expect("prune");
    assert!(
        store
            .provider_models("account-v2:legacy")
            .expect("pruned legacy read")
            .is_none()
    );
    assert!(
        store
            .provider_models("account-v3:current")
            .expect("retained current read")
            .is_some()
    );
}

/// The production migration rejects impossible pre-epoch timestamps.
///
/// MUTATION CHECK: delete `CHECK (fetched_at_ms >= 0)` from migration v8.
/// Expected runtime failure: the direct negative-timestamp insert succeeds
/// and the row-count assertion observes one invalid cache row.
/// Verified by revert on 2026-07-30.
#[test]
fn provider_model_cache_schema_rejects_negative_fetch_timestamp() {
    let root = tempfile::tempdir().expect("tempdir");
    let database_path = {
        let store = Store::open(root.path()).expect("open store");
        store.database_path().to_path_buf()
    };
    let connection = Connection::open(database_path).expect("inspection connection");
    let result = connection.execute(
        "INSERT INTO provider_models(provider, models_json, fetched_at_ms)
         VALUES ('negative-time', '[]', -1)",
        [],
    );
    assert!(result.is_err(), "schema must reject a negative timestamp");
    let rows: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_models WHERE provider = 'negative-time'",
            [],
            |row| row.get(0),
        )
        .expect("count invalid rows");
    assert_eq!(rows, 0);
}

/// A catalog publication cannot become durable unless its matching
/// management revision does too.
///
/// MUTATION CHECK: commit the catalog upsert in
/// `put_provider_models_and_advance_management_revision` before advancing
/// the revision in a second transaction. Expected runtime failure: the
/// injected revision trigger still returns an error, but the final cache
/// read observes the partially committed catalog instead of `None`.
/// Verified by revert on 2026-07-30.
#[test]
fn provider_model_catalog_and_management_revision_roll_back_together() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let connection = Connection::open(store.database_path()).expect("inspection connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_model_revision
             BEFORE UPDATE OF management_revision ON profile_meta
             BEGIN
                 SELECT RAISE(ABORT, 'injected management revision failure');
             END;",
        )
        .expect("install revision failure");

    let result = store.put_provider_models_and_advance_management_revision(
        "provider-atomic",
        r#"[{"slug":"frontier-atomic","source":"provider-fixture"}]"#,
        Some(r#"W/"atomic""#),
        404,
    );
    assert!(result.is_err(), "injected revision failure must surface");
    assert_eq!(
        store
            .provider_models("provider-atomic")
            .expect("cache after rollback"),
        None,
        "catalog and revision must roll back as one transaction"
    );
    assert_eq!(
        store
            .management_revision()
            .expect("revision after rollback"),
        0
    );
}

/// Account-scoped cache keys are durable SQLite row keys: a same-identity
/// restart or re-login must derive byte-identical keys across releases.
///
/// The v3 digests were computed independently with Python blake3 from the
/// provider, alias, auth, incarnation, origin, issuer, account ID, and email
/// fields, length-prefixed as little-endian u64 values. Timestamps and display
/// labels are absent.
///
/// MUTATION CHECK: rename an `auth_method_tag` spelling, reorder the key
/// material, or drop the incarnation. Expected failure: a pinned v3 digest.
#[test]
fn account_scoped_cache_key_derivation_is_pinned() {
    let identity = |account_id: Option<&str>, email: Option<&str>, captured_at: u64| {
        Some(AccountIdentity {
            email: email.map(Into::into),
            display_name: Some("ignored display name".into()),
            account_id: account_id.map(Into::into),
            plan: Some("ignored plan".into()),
            issuer: Some("https://issuer.example".into()),
            captured_at,
            verified: true,
        })
    };
    let descriptor =
        |alias: &str, auth_method: AuthMethod, account_identity| CredentialDescriptor {
            incarnation: 42,
            credential_digest: None,
            alias: CredentialAlias::new(alias),
            provider: "ignored-descriptor-provider".into(),
            base_url: None,
            auth_method,
            identity: "display identity".into(),
            status: CredentialStatus::Ok,
            active: true,
            label: Some("ignored label".into()),
            account_identity,
            created_at_ms: Some(1),
        };
    let cases = [
        (
            "openai-oauth",
            descriptor(
                "codex",
                AuthMethod::OAuth,
                identity(Some("acct-1"), Some("a@example.test"), 3),
            ),
            "account-v3:e0c2a37947fd25a05ebabc981037c9d1d18fc1f716c8b7ec5fbbd40835d54c5d",
        ),
        (
            "anthropic-oauth",
            descriptor(
                "claude",
                AuthMethod::OAuth,
                identity(None, Some("a@example.test"), 3),
            ),
            "account-v3:6645ed7e42c70cdca9243a176e65cb8f1af2122ab538270dbd0bb668d479ffc2",
        ),
        (
            "gemini",
            descriptor("gemini-key", AuthMethod::ApiKey, identity(None, None, 7)),
            "account-v3:18dd388b7a94c920ad1e747e616fc8d99d81f28838c00185fe62ad0c286b3a74",
        ),
        (
            "gemini",
            descriptor("gemini-key", AuthMethod::ApiKey, None),
            "account-v3:ee6b660d20db7f39a1dd7448195d70c25eed31b10e5dd0453de5733282f7e6e9",
        ),
    ];
    for (provider, descriptor, expected) in cases {
        assert_eq!(
            ProviderModelCacheKey::for_account(provider, &descriptor).as_str(),
            expected,
            "{provider}/{}",
            descriptor.alias.as_str()
        );
    }
    // A source-derived identity remains scoped by its durable incarnation.
    let poor = |captured_at: u64| {
        let mut descriptor = descriptor(
            "kimi-code-home",
            AuthMethod::OAuth,
            Some(AccountIdentity {
                email: None,
                display_name: Some("Kimi Code subscription".into()),
                account_id: Some("kimi:0123456789abcdef".into()),
                plan: None,
                issuer: Some("https://issuer.example".into()),
                captured_at,
                verified: false,
            }),
        );
        descriptor.provider = "kimi-oauth".into();
        descriptor
    };
    assert_eq!(
        ProviderModelCacheKey::for_account("kimi-oauth", &poor(5)).as_str(),
        "account-v3:7252e0017b41641ba7a615eeb7e7bc29dfeeff133097e74a46bcf38c122e531e"
    );
    assert_eq!(
        ProviderModelCacheKey::for_account("kimi-oauth", &poor(5)),
        ProviderModelCacheKey::for_account("kimi-oauth", &poor(6)),
        "a timestamp change alone cannot change the catalog coordinate"
    );
    let mut new_incarnation = poor(5);
    new_incarnation.incarnation += 1;
    assert_ne!(
        ProviderModelCacheKey::for_account("kimi-oauth", &poor(5)),
        ProviderModelCacheKey::for_account("kimi-oauth", &new_incarnation),
    );
    assert_eq!(
        ProviderModelCacheKey::for_account(
            "kimi-oauth",
            &descriptor("kimi-code-home", AuthMethod::OAuth, None)
        )
        .as_str(),
        "account-v3:3ece8d45f26267409d42680faa1e89a05dccc7aab46cf1379f3ec649c4cfd6a1",
        "OAuth without identity still carries the incarnation"
    );
    // A known identity (email or verified ID token) keeps one key across
    // re-logins: the epoch is not part of it.
    let known = |captured_at: u64, verified: bool, email: Option<&str>| {
        descriptor(
            "codex",
            AuthMethod::OAuth,
            Some(AccountIdentity {
                email: email.map(Into::into),
                display_name: None,
                account_id: Some("acct-1".into()),
                plan: None,
                issuer: Some("https://issuer.example".into()),
                captured_at,
                verified,
            }),
        )
    };
    assert_eq!(
        ProviderModelCacheKey::for_account(
            "openai-oauth",
            &known(1, false, Some("a@example.test"))
        ),
        ProviderModelCacheKey::for_account(
            "openai-oauth",
            &known(2, false, Some("a@example.test"))
        )
    );
    assert_eq!(
        ProviderModelCacheKey::for_account("openai-oauth", &known(1, true, None)),
        ProviderModelCacheKey::for_account("openai-oauth", &known(2, true, None))
    );
    assert_eq!(
        ProviderModelCacheKey::public("haider-code").as_str(),
        "haider-code",
        "credential-free catalogs keep their pre-973 provider-ID row key"
    );
}

#[test]
fn openai_user_coordinate_and_workspace_email_both_scope_catalog_keys() {
    let descriptor = |id: &str, email: Option<&str>, captured_at: u64| CredentialDescriptor {
        incarnation: 81,
        credential_digest: None,
        alias: CredentialAlias::new("team-user"),
        provider: "openai-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "same display".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: Some(AccountIdentity {
            email: email.map(str::to_owned),
            display_name: None,
            account_id: Some(id.into()),
            plan: None,
            issuer: Some("https://auth.openai.com".into()),
            captured_at,
            verified: true,
        }),
        created_at_ms: None,
    };
    let user1 = descriptor("user:6:user-1;workspace:6:acct-1", None, 1);
    let user2 = descriptor("user:6:user-2;workspace:6:acct-1", None, 1);
    let key = |d: &CredentialDescriptor| ProviderModelCacheKey::for_account("openai-oauth", d);
    assert_ne!(key(&user1), key(&user2));
    assert_eq!(
        key(&user1),
        key(&descriptor("user:6:user-1;workspace:6:acct-1", None, 2))
    );
    assert_eq!(
        key(&user2),
        key(&descriptor("user:6:user-2;workspace:6:acct-1", None, 2))
    );
    assert_ne!(
        key(&descriptor("acct-1", Some("one@example.test"), 1)),
        key(&descriptor("acct-1", Some("two@example.test"), 1)),
    );
}

#[test]
fn account_scope_components_are_length_prefixed_and_byte_exact() {
    let descriptor = |account_id: &str, email: &str| CredentialDescriptor {
        incarnation: 9,
        credential_digest: None,
        alias: CredentialAlias::new("alias"),
        provider: "openai-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "display".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: Some(AccountIdentity {
            email: Some(email.into()),
            display_name: None,
            account_id: Some(account_id.into()),
            plan: None,
            issuer: Some("issuer".into()),
            captured_at: 0,
            verified: false,
        }),
        created_at_ms: None,
    };
    let key =
        |id, email| ProviderModelCacheKey::for_account("openai-oauth", &descriptor(id, email));
    assert_ne!(key("a\0b", "c"), key("a", "b\0c"));
    assert_ne!(key("User", "a@example.test"), key("user", "a@example.test"));
    assert_ne!(
        key("user", "a@example.test"),
        key("user", " a@example.test")
    );
    let mut renamed = descriptor("", "");
    renamed.identity = "renamed display label".into();
    assert_eq!(
        ProviderModelCacheKey::for_account("openai-oauth", &descriptor("", "")),
        ProviderModelCacheKey::for_account("openai-oauth", &renamed),
        "a display label is never account proof",
    );
}
