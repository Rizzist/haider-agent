#![allow(clippy::expect_used)] // test diagnostics identify the exact revision boundary

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_store::{AccountAddReceiptResponse, LoginReceiptResponse, Store};
use rusqlite::Connection;

fn descriptor(alias: &str, auth_method: AuthMethod) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: "anthropic".into(),
        base_url: None,
        auth_method,
        identity: "revision fixture".into(),
        status: CredentialStatus::Ok,
        active: true,
    }
}

/// Receipt finalization and revision allocation are one SQLite commit.
///
/// MUTATION CHECK: commit `finalize_command_receipt` before incrementing
/// `profile_meta.management_revision` in a second transaction. Expected
/// runtime failure: the injected revision trigger returns an error but the
/// receipt query observes `committed` instead of the required `pending`.
#[test]
fn management_receipt_and_revision_roll_back_together() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    assert_eq!(store.management_revision().expect("initial revision"), 0);
    assert!(matches!(
        store
            .login_claim_receipt("atomic-login", "digest-atomic", "{}")
            .expect("claim"),
        haider_store::LoginClaim::Fresh
    ));

    let connection = Connection::open(store.database_path()).expect("inspection connection");
    connection
        .execute_batch(
            "CREATE TRIGGER abort_management_receipt_revision
             BEFORE UPDATE OF final_revision ON command_receipts
             WHEN NEW.final_revision IS NOT NULL
             BEGIN
               SELECT RAISE(ABORT, 'injected final revision failure');
             END;",
        )
        .expect("install trigger");

    let error = store
        .finalize_login_receipt(
            "atomic-login",
            &LoginReceiptResponse {
                descriptor: descriptor("atomic-login", AuthMethod::ApiKey),
            },
        )
        .expect_err("trigger must abort finalization");
    assert!(error.message.contains("injected final revision failure"));
    assert_eq!(
        store.management_revision().expect("rolled back revision"),
        0
    );
    let (state, final_revision): (String, Option<i64>) = connection
        .query_row(
            "SELECT state, final_revision
             FROM command_receipts
             WHERE command_id = 'atomic-login'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("receipt row");
    assert_eq!(state, "pending");
    assert_eq!(final_revision, None);

    connection
        .execute_batch("DROP TRIGGER abort_management_receipt_revision;")
        .expect("drop trigger");
    let revision = store
        .finalize_login_receipt(
            "atomic-login",
            &LoginReceiptResponse {
                descriptor: descriptor("atomic-login", AuthMethod::ApiKey),
            },
        )
        .expect("finalize after trigger removal");
    assert_eq!(revision, 1);
    assert_eq!(store.management_revision().expect("committed revision"), 1);
}

/// The one counter covers both account receipt families and repairs an old
/// committed receipt only once.
///
/// MUTATION CHECK: replace the loaded `final_revision` with `None` in
/// `ensure_committed_management_revision`, bypassing its replay fast path.
/// Expected runtime failure: the second ensure returns the missing-revision
/// claim error instead of replaying revision three.
#[test]
fn management_revision_is_monotonic_durable_and_missing_markers_advance_once() {
    let root = tempfile::tempdir().expect("tempdir");
    let database_path = {
        let store = Store::open(root.path()).expect("store");
        store
            .login_claim_receipt("login-1", "digest-login", "{}")
            .expect("login claim");
        let login_revision = store
            .finalize_login_receipt(
                "login-1",
                &LoginReceiptResponse {
                    descriptor: descriptor("login-1", AuthMethod::ApiKey),
                },
            )
            .expect("login finalize");
        assert_eq!(login_revision, 1);

        store
            .account_add_claim_receipt("oauth-1", "digest-oauth", "{}")
            .expect("OAuth claim");
        let oauth_revision = store
            .finalize_account_add_receipt(
                "oauth-1",
                &AccountAddReceiptResponse {
                    descriptor: descriptor("oauth-1", AuthMethod::OAuth),
                },
            )
            .expect("OAuth finalize");
        assert_eq!(oauth_revision, 2);
        assert_eq!(store.management_revision().expect("revision"), 2);

        let connection = Connection::open(store.database_path()).expect("inspection connection");
        connection
            .execute(
                "UPDATE command_receipts
                 SET final_revision = NULL
                 WHERE command_id = 'login-1'",
                [],
            )
            .expect("simulate pre-v6 committed receipt");
        let repaired = store
            .ensure_committed_management_revision("login-1", "account.login_api")
            .expect("repair missing revision");
        assert_eq!(repaired, 3);
        let replayed = store
            .ensure_committed_management_revision("login-1", "account.login_api")
            .expect("idempotent repair replay");
        assert_eq!(replayed, repaired);

        let standalone = store
            .advance_management_revision()
            .expect("actor-owned transition");
        assert_eq!(standalone, 4);
        assert_eq!(store.management_revision().expect("final revision"), 4);
        store.database_path().to_path_buf()
    };

    let reopened = Store::open(root.path()).expect("reopen");
    assert_eq!(reopened.management_revision().expect("durable revision"), 4);
    let connection = Connection::open(database_path).expect("inspection connection");
    let receipt_revision: i64 = connection
        .query_row(
            "SELECT final_revision FROM command_receipts WHERE command_id = 'login-1'",
            [],
            |row| row.get(0),
        )
        .expect("receipt revision");
    assert_eq!(receipt_revision, 3);
}
