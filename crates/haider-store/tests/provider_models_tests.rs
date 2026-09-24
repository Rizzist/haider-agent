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
        "the migration leaves old rows present but no v2 key reads them"
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
/// The digests were computed independently from the e8fd8aaf derivation
/// (`"{provider}\0{alias}\0{auth:?}\0{issuer}\0{scope_kind}\0{scope}"`, plus
/// `"\0{captured_at}"` for API keys, BLAKE3-hex under `account-v2:`).
///
/// MUTATION CHECK: rename an `auth_method_tag` spelling, reorder the key
/// material, or drop the API-key intake epoch. Expected runtime failure: one
/// of the pinned `account-v2:` digests differs.
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
            "account-v2:6ab0f3cf9a175581fcfa0f4f7836e961c3c5a2efabd78bcf476d2553bd1b5320",
        ),
        (
            "anthropic-oauth",
            descriptor(
                "claude",
                AuthMethod::OAuth,
                identity(None, Some("a@example.test"), 3),
            ),
            "account-v2:e491e14e5ddca8b4895248e20cb0f43685be44749ce7c7863d6a7bcf91cf4f0a",
        ),
        (
            "gemini",
            descriptor("gemini-key", AuthMethod::ApiKey, identity(None, None, 7)),
            "account-v2:9697064daf2c69eefc438c6868ea8d18177d9c7520ceabb8ea521c7edf4e2e2d",
        ),
        (
            "gemini",
            descriptor("gemini-key", AuthMethod::ApiKey, None),
            "account-v2:d3006aea71942e00ae8ba8a5727c67e76129631665a731e51943ec9809946ad2",
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
    assert_eq!(
        ProviderModelCacheKey::public("haider-code").as_str(),
        "haider-code",
        "credential-free catalogs keep their pre-973 provider-ID row key"
    );
}
