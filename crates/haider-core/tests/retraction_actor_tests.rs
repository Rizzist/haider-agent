#![allow(clippy::expect_used)]
//! Deterministic actor scheduling tests. The store adapter supplies the
//! retraction arbitration contract, independently pinned by SQLite store tests.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use haider_core::{
    CommittedRange, HarnessActor, HarnessConfig, MemoryStore, ProviderBudgetGuard,
    ProviderBudgetGuardError, ProviderBudgetPermit, StoreHandle, SubmitCommittedTurn,
};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::HaiderError;
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::provider::{CapabilityDoc, StreamEvent};
use haider_protocol::state::RunState;
use haider_provider::{Message, Provider, ProviderError, ProviderStream, TurnRequest};
use tokio::sync::{Notify, mpsc};

#[derive(Clone, Copy)]
enum Gate {
    Streaming,
    FirstResponse,
}

struct ArbitrationStore {
    inner: MemoryStore,
    gate: Gate,
    gate_taken: AtomicBool,
    entered: Notify,
    release: Notify,
    retracted: AtomicBool,
}

impl ArbitrationStore {
    fn new(gate: Gate) -> Self {
        Self {
            inner: MemoryStore::new(),
            gate,
            gate_taken: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
            retracted: AtomicBool::new(false),
        }
    }

    async fn accept_retraction(&self, session: &SessionId) {
        let mut fact = self
            .inner
            .events(session)
            .await
            .last()
            .expect("actor started")
            .clone();
        fact.event_id = EventId::new("accepted-retraction");
        fact.payload = serde_json::json!({
            "type":"prompt_retracted", "prompt_seq":1,
            "prompt_node_id":"accepted-prompt-node", "text":"accepted prompt", "attachments":[],
        })
        .into();
        self.inner
            .append(&mut [fact])
            .await
            .expect("retraction fact commits");
        self.retracted.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl StoreHandle for ArbitrationStore {
    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&haider_protocol::ids::BranchId>,
    ) -> Result<Vec<haider_protocol::branch::BranchDescriptor>, HaiderError> {
        self.inner.branch_lineage(session_id, branch_id).await
    }

    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let gate = envelopes.iter().any(|event| match self.gate {
            Gate::Streaming => {
                event.payload["type"] == "run_state" && event.payload["state"] == "streaming"
            }
            Gate::FirstResponse => event.payload["type"] == "response_started",
        }) && !self.gate_taken.swap(true, Ordering::SeqCst);
        if gate && matches!(self.gate, Gate::Streaming) {
            let committed = self.inner.append(envelopes).await?;
            self.entered.notify_one();
            self.release.notified().await;
            return Ok(committed);
        }
        if gate {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self.retracted.load(Ordering::SeqCst) {
            for event in envelopes.iter_mut() {
                if event.payload["type"] == "response_started" {
                    let mut payload: serde_json::Value = event.payload.clone().into();
                    payload["type"] = "response_delta_discarded".into();
                    payload["prompt_seq"] = 1.into();
                    event.payload = payload.into();
                } else if event.payload["type"] == "run_state"
                    && event.payload["state"] == "cancelled"
                {
                    let mut payload: serde_json::Value = event.payload.clone().into();
                    payload["reason"] = "retracted".into();
                    event.payload = payload.into();
                }
            }
        }
        self.inner.append(envelopes).await
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

struct ControlledProvider {
    receiver: Mutex<Option<mpsc::Receiver<Result<StreamEvent, ProviderError>>>>,
    opened: AtomicUsize,
}

#[async_trait]
impl Provider for ControlledProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        haider_provider::FakeProvider::new(Vec::new())
            .capabilities()
            .await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .receiver
            .lock()
            .expect("provider receiver")
            .take()
            .expect("one request")
            .into())
    }
}

#[derive(Debug, Default)]
struct Budget {
    admitted: AtomicUsize,
    active: Arc<AtomicUsize>,
    settled: Mutex<Vec<bool>>,
    usage_updates: AtomicUsize,
}

struct Permit(Arc<AtomicUsize>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ProviderBudgetGuard for Budget {
    async fn before_request(
        &self,
        _run_id: &RunId,
        _provider: &str,
        _request: &TurnRequest,
        _projected_input_tokens: u64,
    ) -> Result<ProviderBudgetPermit, ProviderBudgetGuardError> {
        self.admitted.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderBudgetPermit::new(Permit(Arc::clone(&self.active))))
    }

    async fn after_usage(&self, _run_id: &RunId) -> Result<(), ProviderBudgetGuardError> {
        self.usage_updates.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn after_route_interruption(
        &self,
        _run_id: &RunId,
    ) -> Result<(), ProviderBudgetGuardError> {
        panic!("retraction must not become a retryable route interruption")
    }

    async fn after_request(
        &self,
        _run_id: &RunId,
        _provider: &str,
        _model: &str,
        usage_reported: bool,
    ) -> Result<(), ProviderBudgetGuardError> {
        self.settled
            .lock()
            .expect("budget settlements")
            .push(usage_reported);
        Ok(())
    }
}

async fn cancellation_race(gate: Gate) {
    let session = SessionId::new("retraction-actor-session");
    let run_id = RunId::new("retraction-actor-run");
    let store = Arc::new(ArbitrationStore::new(gate));
    let budget = Arc::new(Budget::default());
    let (sender, receiver) = mpsc::channel(4);
    let provider = Arc::new(ControlledProvider {
        receiver: Mutex::new(Some(receiver)),
        opened: AtomicUsize::new(0),
    });
    let mut config = HarnessConfig::for_session(session.clone(), DeviceId::new("test"), 1, 1);
    config.prompt_retraction_enabled = true;
    config.provider_budget_guard = Some(budget.clone());
    let actor = HarnessActor::spawn(config, provider.clone(), store.clone());
    let turn = actor
        .submit_committed_turn(SubmitCommittedTurn {
            run_id,
            messages: vec![Message::user_text("accepted prompt")],
        })
        .await
        .expect("actor accepts committed turn");
    if matches!(gate, Gate::FirstResponse) {
        sender
            .send(Ok(StreamEvent::TextDelta {
                text: "losing response".into(),
            }))
            .await
            .expect("first response queued");
    }
    store.entered.notified().await;
    store.accept_retraction(&session).await;
    if matches!(gate, Gate::Streaming) {
        sender
            .send(Ok(StreamEvent::TextDelta {
                text: "losing response".into(),
            }))
            .await
            .expect("first response ready at cancellation tie");
    }
    // The actor is still suspended in the gated store call. Both cancellation
    // and the provider queue become ready before it can poll either branch.
    turn.cancel();
    store.release.notify_one();
    let outcome = turn.wait().await.expect("cancelled turn settles");
    assert_eq!(outcome.state, RunState::Cancelled);
    let events = store.inner.events(&session).await;
    let discarded = events
        .iter()
        .filter(|event| event.payload["type"] == "response_delta_discarded")
        .collect::<Vec<_>>();
    assert_eq!(
        discarded.len(),
        1,
        "the losing already-observed response leaves one durable discarded fact"
    );
    assert_eq!(discarded[0].payload["delta"]["text"], "losing response");
    assert!(
        discarded[0].seq
            > events
                .iter()
                .find(|event| event.payload["type"] == "prompt_retracted")
                .expect("retraction fact")
                .seq
    );
    assert!(
        !events
            .iter()
            .any(|event| event.payload["type"] == "response_started")
    );
    assert!(
        !events
            .iter()
            .any(|event| event.payload["type"] == "item"
                && event.payload["delta"]["delta"] == "text")
    );
    assert!(
        !events
            .iter()
            .any(|event| event.payload["item"]["item"] == "agent_message")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.payload["state"] == "cancelled")
            .count(),
        1
    );
    assert_eq!(provider.opened.load(Ordering::SeqCst), 1);
    assert_eq!(budget.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(*budget.settled.lock().expect("settlements"), [false]);
    assert_eq!(
        budget.active.load(Ordering::SeqCst),
        0,
        "request permit released exactly once"
    );
    assert_eq!(
        budget.usage_updates.load(Ordering::SeqCst),
        0,
        "no usage is invented for discarded content"
    );
    assert!(sender.is_closed(), "cancelled provider receiver is dropped");
    actor.stop().await.expect("actor stops");
}

/// MUTATION: returning immediately from the biased cancellation branch loses
/// an already-buffered first response and fails the discarded-fact assertion.
#[tokio::test]
async fn retraction_cancellation_tie_discards_the_already_ready_first_delta() {
    cancellation_race(Gate::Streaming).await;
}

/// MUTATION: publishing a first response after its store append loses to the
/// retraction receipt creates visible text and violates the same assertion.
#[tokio::test]
async fn retraction_wins_while_the_first_response_boundary_is_waiting_to_commit() {
    cancellation_race(Gate::FirstResponse).await;
}
