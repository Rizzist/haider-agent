//! Oracle tests for the async-to-synchronous real-store seam.
#![allow(clippy::expect_used)] // test diagnostics should identify the failed boundary

use haider_core::{
    HarnessActor, HarnessConfig, SqliteStoreHandle, StoreHandle, SubmitTurn, TurnOutcome,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const SESSION: &str = "sqlite-store-session";

fn config() -> HarnessConfig {
    HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("sqlite-store-device"),
        4,
        2,
    )
}

fn provider() -> Arc<FakeProvider> {
    Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "durable response".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]))
}

fn typed(envelope: &RawEnvelope) -> EventPayload {
    serde_json::from_value(envelope.payload.clone()).expect("known event payload")
}

async fn finish_actor(handle: haider_core::HarnessHandle, actor: JoinHandle<()>) {
    drop(handle);
    timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor stops after its handle is dropped")
        .expect("actor task succeeds");
}

#[tokio::test]
async fn real_store_actor_turn_is_durable_before_every_broadcast() {
    let root = tempfile::tempdir().expect("temporary profile");
    let store = Arc::new(
        SqliteStoreHandle::open(root.path())
            .await
            .expect("real store opens"),
    );
    let (actor, handle) = HarnessActor::new(config(), provider(), store.clone());
    let actor = tokio::spawn(actor.run());
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("prove commit before publish"))
        .await
        .expect("turn accepted");

    let mut received = Vec::new();
    loop {
        let envelope = timeout(Duration::from_secs(1), subscriber.recv())
            .await
            .expect("broadcast arrives")
            .expect("broadcast remains open");
        let durable = store
            .read(&SessionId::new(SESSION), envelope.seq - 1, 1)
            .await
            .expect("journal is readable at broadcast time");
        assert_eq!(durable.len(), 1);
        assert_eq!(
            serde_json::to_vec(&durable[0]).expect("durable envelope serializes"),
            serde_json::to_vec(&envelope).expect("broadcast envelope serializes")
        );
        let terminal = matches!(
            typed(&envelope),
            EventPayload::RunState(state) if state.is_terminal()
        );
        received.push(envelope);
        if terminal {
            break;
        }
    }

    let outcome = timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("turn terminates")
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(
        received.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=received.len() as u64).collect::<Vec<_>>()
    );
    finish_actor(handle, actor).await;
}

#[tokio::test]
async fn real_store_reopen_replays_the_identical_envelope_bytes() {
    let root = tempfile::tempdir().expect("temporary profile");
    let expected = {
        let store = Arc::new(
            SqliteStoreHandle::open(root.path())
                .await
                .expect("real store opens"),
        );
        let (actor, handle) = HarnessActor::new(config(), provider(), store.clone());
        let actor = tokio::spawn(actor.run());
        let outcome: TurnOutcome = handle
            .submit_turn(SubmitTurn::new("persist this turn"))
            .await
            .expect("turn accepted")
            .wait()
            .await
            .expect("turn outcome");
        assert_eq!(outcome.state, RunState::Done);
        let envelopes = store
            .read(&SessionId::new(SESSION), 0, usize::MAX)
            .await
            .expect("committed turn reads");
        let bytes = envelopes
            .iter()
            .map(|envelope| serde_json::to_vec(envelope).expect("envelope serializes"))
            .collect::<Vec<_>>();
        finish_actor(handle, actor).await;
        drop(store);
        bytes
    };

    let reopened = SqliteStoreHandle::open(root.path())
        .await
        .expect("profile reopens");
    let replayed = reopened
        .read(&SessionId::new(SESSION), 0, usize::MAX)
        .await
        .expect("reopened journal reads")
        .iter()
        .map(|envelope| serde_json::to_vec(envelope).expect("replayed envelope serializes"))
        .collect::<Vec<_>>();

    assert!(!expected.is_empty());
    assert_eq!(replayed, expected);
}

#[tokio::test]
async fn real_store_profile_lock_is_exclusive_across_handles() {
    let root = tempfile::tempdir().expect("temporary profile");
    let first = SqliteStoreHandle::open(root.path())
        .await
        .expect("first handle opens");

    let error = match SqliteStoreHandle::open(root.path()).await {
        Ok(_) => panic!("second handle unexpectedly acquired the profile lock"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::StoreLocked);
    assert!(error.retryable);

    drop(first);
    SqliteStoreHandle::open(root.path())
        .await
        .expect("profile lock is released with its handle");
}
