#![allow(clippy::expect_used)]

use haider_store::{CachedModels, Store};
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
