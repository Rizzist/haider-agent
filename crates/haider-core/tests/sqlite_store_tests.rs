//! Oracle tests for the async-to-synchronous real-store seam.
#![allow(clippy::expect_used)] // test diagnostics should identify the failed boundary

use async_trait::async_trait;
use haider_core::{
    CommittedRange, HarnessActor, HarnessConfig, SqliteStoreHandle, StoreHandle, SubmitTurn,
    TurnOutcome,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use haider_tools::CasSink;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const SESSION: &str = "sqlite-store-session";

fn config(worker_generation: u64) -> HarnessConfig {
    HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("sqlite-store-device"),
        4,
        worker_generation,
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
async fn append_completion_is_held_until_commit_and_precedes_broadcast() {
    let root = tempfile::tempdir().expect("temporary profile");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("real store opens");
    let gated = Arc::new(GatedStore::new(store.clone()));
    let (actor, handle) =
        HarnessActor::new(config(store.worker_generation()), provider(), gated.clone());
    let actor = tokio::spawn(actor.run());
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("prove commit before publish"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(1), gated.append_committed.notified())
        .await
        .expect("first append commits and reaches the gate");
    assert!(
        timeout(Duration::from_millis(50), subscriber.recv())
            .await
            .is_err(),
        "an envelope was broadcast before append returned"
    );
    let first_durable = store
        .read(&SessionId::new(SESSION), 0, 1)
        .await
        .expect("held append is already durable");
    assert_eq!(first_durable.len(), 1);

    gated.release.notify_one();
    let first_broadcast = timeout(Duration::from_secs(1), subscriber.recv())
        .await
        .expect("broadcast arrives after append is released")
        .expect("broadcast remains open");
    assert_eq!(
        serde_json::to_vec(&first_broadcast).expect("broadcast envelope serializes"),
        serde_json::to_vec(&first_durable[0]).expect("durable envelope serializes")
    );

    let mut received = vec![first_broadcast];
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
    store
        .close()
        .await
        .expect("store closes off runtime workers");
}

#[tokio::test]
async fn real_store_reopen_replays_the_identical_envelope_bytes() {
    let root = tempfile::tempdir().expect("temporary profile");
    let expected = {
        let store = SqliteStoreHandle::open(root.path())
            .await
            .expect("real store opens");
        let actor_store: Arc<dyn StoreHandle> = Arc::new(store.clone());
        let (actor, handle) =
            HarnessActor::new(config(store.worker_generation()), provider(), actor_store);
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
        store
            .close()
            .await
            .expect("store closes off runtime workers");
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
    reopened
        .close()
        .await
        .expect("reopened store closes off runtime workers");
}

#[tokio::test]
async fn durable_generation_prevents_same_millisecond_restart_id_collisions() {
    const FROZEN_START_MS: u64 = 1_234_567;

    let root = tempfile::tempdir().expect("temporary profile");
    let first_store = SqliteStoreHandle::open(root.path())
        .await
        .expect("first store opens");
    let first_generation = first_store.worker_generation();
    let first_actor_store: Arc<dyn StoreHandle> = Arc::new(first_store.clone());
    let (first_actor, first_handle) = HarnessActor::new(
        config(first_generation).with_started_at_ms(FROZEN_START_MS),
        provider(),
        first_actor_store,
    );
    let first_actor = tokio::spawn(first_actor.run());
    let first_outcome = first_handle
        .submit_turn(SubmitTurn::new("first process"))
        .await
        .expect("first turn accepted")
        .wait()
        .await
        .expect("first turn reports an outcome");
    assert_eq!(first_outcome.state, RunState::Done);
    finish_actor(first_handle, first_actor).await;
    first_store
        .close()
        .await
        .expect("first store closes cleanly");

    let second_store = SqliteStoreHandle::open(root.path())
        .await
        .expect("second store opens");
    let second_generation = second_store.worker_generation();
    assert!(second_generation > first_generation);
    let second_actor_store: Arc<dyn StoreHandle> = Arc::new(second_store.clone());
    let (second_actor, second_handle) = HarnessActor::new(
        config(second_generation).with_started_at_ms(FROZEN_START_MS),
        provider(),
        second_actor_store,
    );
    let second_actor = tokio::spawn(second_actor.run());
    let second_outcome = second_handle
        .submit_turn(SubmitTurn::new("restarted process"))
        .await
        .expect("restarted turn accepted")
        .wait()
        .await
        .expect("restarted turn reports an outcome");
    assert_eq!(
        second_outcome.state,
        RunState::Done,
        "same-ms restart collided with a durable event id"
    );

    let replay = second_store
        .read(&SessionId::new(SESSION), 0, usize::MAX)
        .await
        .expect("both turns replay");
    let event_ids = replay
        .iter()
        .map(|envelope| envelope.event_id.to_string())
        .collect::<HashSet<_>>();
    assert_eq!(event_ids.len(), replay.len());
    assert_eq!(
        replay
            .iter()
            .map(|envelope| envelope.worker_generation)
            .collect::<HashSet<_>>(),
        HashSet::from([first_generation, second_generation])
    );
    assert_eq!(
        replay
            .iter()
            .map(|envelope| envelope.seq)
            .collect::<Vec<_>>(),
        (1..=replay.len() as u64).collect::<Vec<_>>()
    );

    finish_actor(second_handle, second_actor).await;
    second_store
        .close()
        .await
        .expect("second store closes cleanly");
}

#[tokio::test]
async fn explicit_close_releases_lock_for_immediate_reopen() {
    let root = tempfile::tempdir().expect("temporary profile");
    let first = SqliteStoreHandle::open(root.path())
        .await
        .expect("first handle opens");

    timeout(Duration::from_secs(1), first.close())
        .await
        .expect("close completes")
        .expect("close succeeds");
    let reopened = timeout(Duration::from_secs(1), SqliteStoreHandle::open(root.path()))
        .await
        .expect("immediate reopen completes")
        .expect("profile lock was released");
    reopened.close().await.expect("reopened handle closes");
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
    let reopened = SqliteStoreHandle::open(root.path())
        .await
        .expect("profile lock is released with its handle");
    reopened.close().await.expect("reopened handle closes");
}

#[tokio::test]
async fn real_store_is_a_durable_tool_result_cas_sink() {
    let root = tempfile::tempdir().expect("temporary profile");
    let mut store = SqliteStoreHandle::open(root.path())
        .await
        .expect("real store opens");
    let bytes = b"complete bounded tool result";

    let artifact = CasSink::put(&mut store, bytes)
        .await
        .expect("tool CAS bridge stores result");
    assert_eq!(
        store.get(&artifact).await.expect("stored result reads"),
        bytes
    );
    assert!(
        store
            .verify(&artifact)
            .await
            .expect("stored result verifies")
    );

    store.close().await.expect("store closes");
}

struct GatedStore {
    inner: SqliteStoreHandle,
    hold_first: AtomicBool,
    append_committed: Notify,
    release: Notify,
}

impl GatedStore {
    fn new(inner: SqliteStoreHandle) -> Self {
        Self {
            inner,
            hold_first: AtomicBool::new(true),
            append_committed: Notify::new(),
            release: Notify::new(),
        }
    }
}

#[async_trait]
impl StoreHandle for GatedStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let range = self.inner.append(envelopes).await?;
        if self.hold_first.swap(false, Ordering::SeqCst) {
            self.append_committed.notify_one();
            self.release.notified().await;
        }
        Ok(range)
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.read(session_id, since_seq, limit).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.inner.latest_seq(session_id).await
    }
}
