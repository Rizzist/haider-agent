#![allow(clippy::expect_used)]

use haider_store::{
    ACCOUNT_REMOVE_METHOD, ACCOUNT_SET_DEFAULT_MODEL_METHOD, ErrorCode, ManagementClaim,
    PROVIDER_REMOVE_METHOD, Store,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PublicReceipt {
    value: String,
}

/// Committed replay is the first branch inside the same transaction that
/// performs expected-revision CAS.
///
/// MUTATION CHECK: move the expected-revision block in
/// `claim_management_receipt_in_transaction` above
/// `lookup_command_response`. Expected runtime failure: the replay assertion
/// below receives `revision_conflict` instead of revision one after the
/// counter advances to two.
#[test]
fn committed_management_replay_precedes_expected_revision_cas() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let request = r#"{"expected_revision":0,"model":"model-b","provider":"anthropic"}"#;

    assert!(matches!(
        store
            .management_claim_receipt::<PublicReceipt>(
                "set-default-1",
                ACCOUNT_SET_DEFAULT_MODEL_METHOD,
                "set-default-digest",
                request,
                None,
                Some(0),
            )
            .expect("fresh claim"),
        ManagementClaim::Fresh
    ));
    assert_eq!(
        store
            .finalize_management_receipt(
                "set-default-1",
                ACCOUNT_SET_DEFAULT_MODEL_METHOD,
                &PublicReceipt {
                    value: "model-b".into(),
                },
            )
            .expect("finalize"),
        1
    );
    assert_eq!(
        store.advance_management_revision().expect("later mutation"),
        2
    );

    match store
        .management_claim_receipt::<PublicReceipt>(
            "set-default-1",
            ACCOUNT_SET_DEFAULT_MODEL_METHOD,
            "set-default-digest",
            request,
            None,
            Some(0),
        )
        .expect("committed replay ignores stale expectation")
    {
        ManagementClaim::Committed { response, revision } => {
            assert_eq!(revision, 1);
            assert_eq!(response.value, "model-b");
        }
        other => panic!("expected committed replay, got {other:?}"),
    }

    let error = store
        .management_claim_receipt::<PublicReceipt>(
            "set-default-2",
            ACCOUNT_SET_DEFAULT_MODEL_METHOD,
            "set-default-digest-2",
            r#"{"expected_revision":0,"model":"model-c","provider":"anthropic"}"#,
            None,
            Some(0),
        )
        .expect_err("genuinely new stale command must conflict");
    assert_eq!(error.code, ErrorCode::RevisionConflict);
    assert!(error.retryable);
    assert_eq!(
        error.details,
        Some(serde_json::json!({
            "expected_revision": 0,
            "current_revision": 2,
        }))
    );
}

/// Remove's receipt and reservation are one claim transaction, while its
/// reservation release and revision are one finalization transaction.
///
/// MUTATION CHECK: delete the `DELETE FROM account_alias_reservations` step
/// from `finalize_account_remove_receipt` while still committing the receipt.
/// Expected runtime failure: `reserved_account_aliases()` remains
/// `["work"]` after the successful finalization instead of becoming empty.
#[test]
fn account_remove_reservation_survives_pending_and_releases_with_finalization() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let request = r#"{"alias":"work","expected_revision":0}"#;
    let recovery = r#"{"provider":"anthropic","was_active":true}"#;

    assert!(matches!(
        store
            .account_remove_claim_receipt::<PublicReceipt>(
                "remove-1",
                "remove-digest",
                request,
                recovery,
                Some(0),
                "work",
                "anthropic",
                true,
            )
            .expect("claim remove"),
        ManagementClaim::Fresh
    ));
    assert_eq!(
        store.reserved_account_aliases().expect("reservations"),
        vec!["work"]
    );
    let pending = store.account_remove_receipts().expect("pending removes");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].receipt.method, ACCOUNT_REMOVE_METHOD);
    assert_eq!(pending[0].alias, "work");
    assert_eq!(pending[0].provider, "anthropic");
    assert!(pending[0].was_active);

    let busy = store
        .account_remove_claim_receipt::<PublicReceipt>(
            "remove-2",
            "remove-digest-2",
            request,
            recovery,
            Some(0),
            "work",
            "anthropic",
            true,
        )
        .expect_err("another command cannot acquire the alias reservation");
    assert_eq!(busy.code, ErrorCode::Busy);
    assert!(busy.retryable);

    assert_eq!(
        store
            .finalize_account_remove_receipt(
                "remove-1",
                &PublicReceipt {
                    value: "work".into(),
                },
            )
            .expect("finalize remove"),
        1
    );
    assert!(
        store
            .reserved_account_aliases()
            .expect("released reservations")
            .is_empty()
    );
    assert!(
        store
            .account_remove_receipts()
            .expect("no pending removes")
            .is_empty()
    );

    assert!(matches!(
        store
            .account_remove_claim_receipt::<PublicReceipt>(
                "remove-1",
                "remove-digest",
                request,
                recovery,
                Some(0),
                "work",
                "anthropic",
                true,
            )
            .expect("committed remove replay"),
        ManagementClaim::Committed { revision: 1, .. }
    ));
}

/// Provider removal commits its receipt, revision, and discovered-model cache
/// deletion in one transaction.
///
/// MUTATION CHECK: delete the `DELETE FROM provider_models` statement or move
/// it outside the receipt transaction. Expected RUNTIME failure: the injected
/// deletion fault no longer rolls the receipt/revision back, or the cache row
/// remains readable after the successful retry.
#[test]
fn provider_remove_finalization_deletes_model_cache_and_replays() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let request = r#"{"expected_revision":0,"provider":"custom"}"#;
    store
        .put_provider_models("custom", r#"[{"slug":"model-a"}]"#, Some("etag-a"), 41)
        .expect("seed provider cache");
    assert!(matches!(
        store
            .management_claim_receipt::<PublicReceipt>(
                "remove-provider-1",
                PROVIDER_REMOVE_METHOD,
                "remove-provider-digest",
                request,
                None,
                Some(0),
            )
            .expect("claim provider removal"),
        ManagementClaim::Fresh
    ));
    let connection = Connection::open(store.database_path()).expect("inspection connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_provider_remove_cache_delete
             BEFORE DELETE ON provider_models
             WHEN OLD.provider = 'custom'
             BEGIN
                 SELECT RAISE(ABORT, 'injected provider cache delete failure');
             END;",
        )
        .expect("install delete-failure trigger");
    store
        .finalize_provider_remove_receipt(
            "remove-provider-1",
            "custom",
            &PublicReceipt {
                value: "custom".into(),
            },
        )
        .expect_err("cache deletion failure rolls back finalization");
    assert_eq!(
        store.management_revision().expect("rolled-back revision"),
        0
    );
    assert!(
        store
            .provider_models("custom")
            .expect("cache read")
            .is_some()
    );
    assert_eq!(
        store
            .management_receipts(PROVIDER_REMOVE_METHOD)
            .expect("pending receipt")[0]
            .state,
        "pending"
    );
    connection
        .execute_batch("DROP TRIGGER reject_provider_remove_cache_delete;")
        .expect("remove delete-failure trigger");
    drop(connection);
    assert_eq!(
        store
            .finalize_provider_remove_receipt(
                "remove-provider-1",
                "custom",
                &PublicReceipt {
                    value: "custom".into(),
                },
            )
            .expect("finalize provider removal"),
        1
    );
    assert_eq!(store.management_revision().expect("revision"), 1);
    assert_eq!(store.provider_models("custom").expect("cache read"), None);
    assert!(matches!(
        store
            .management_claim_receipt::<PublicReceipt>(
                "remove-provider-1",
                PROVIDER_REMOVE_METHOD,
                "remove-provider-digest",
                request,
                None,
                Some(0),
            )
            .expect("replay provider removal"),
        ManagementClaim::Committed { revision: 1, .. }
    ));
}
