//! Private session-hub accounting tests.

#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{AgentId, BranchId, EventId, RunId};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Notify, mpsc, watch};

/// MUTATION CHECK: remove any owned ID charge from
/// `envelope_weight_bytes` (for example `branch_id`). Expected failure: the
/// estimator falls below the explicit fixed-value-plus-owned-strings size.
#[test]
fn envelope_weight_charges_every_large_owned_id_string() {
    let large = |label: &str| format!("{label}-{}", "x".repeat(16 * 1024));
    let envelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(large("event")),
        seq: 1,
        session_id: SessionId::new(large("session")),
        branch_id: Some(BranchId::new(large("branch"))),
        run_id: Some(RunId::new(large("run"))),
        agent_id: Some(AgentId::new(large("agent"))),
        device_id: DeviceId::new(large("device")),
        authority_epoch: 2,
        worker_generation: 3,
        causation_id: Some(EventId::new(large("causation"))),
        correlation_id: Some(EventId::new(large("correlation"))),
        committed_at_ms: 4,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::Value::Null,
    };
    let owned_string_bytes = envelope
        .event_id
        .as_str()
        .len()
        .saturating_add(envelope.session_id.as_str().len())
        .saturating_add(
            envelope
                .branch_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .run_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .agent_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(envelope.device_id.as_str().len())
        .saturating_add(
            envelope
                .causation_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .correlation_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        );
    let real_owned_lower_bound =
        std::mem::size_of::<RawEnvelope>().saturating_add(owned_string_bytes);

    assert!(
        envelope_weight_bytes(&envelope) >= real_owned_lower_bound,
        "every variable-length envelope field must be charged"
    );
}

struct AbortQueueSink {
    state: Mutex<AbortQueueState>,
    changed: Notify,
    pause_next_fire: AtomicBool,
    fired_reached: Notify,
    fired_release: Notify,
}

struct AbortQueueState {
    queue: VecDeque<WireFrame>,
    tickets: VecDeque<Weak<Notify>>,
}

impl AbortQueueState {
    fn prune_dead_tickets(&mut self) {
        while self
            .tickets
            .front()
            .is_some_and(|ticket| ticket.strong_count() == 0)
        {
            self.tickets.pop_front();
        }
    }

    fn ticket_is_head(&mut self, ticket: &AdmissionTicket) -> bool {
        self.prune_dead_tickets();
        self.tickets
            .front()
            .is_some_and(|head| Weak::ptr_eq(head, &Arc::downgrade(ticket)))
    }

    fn fire_head(&mut self) {
        self.prune_dead_tickets();
        if let Some(ticket) = self.tickets.front().and_then(Weak::upgrade) {
            ticket.notify_one();
        }
    }

    fn remove_ticket(&mut self, ticket: &AdmissionTicket) -> bool {
        self.prune_dead_tickets();
        let was_head = self.ticket_is_head(ticket);
        let token = Arc::downgrade(ticket);
        self.tickets
            .retain(|candidate| !Weak::ptr_eq(candidate, &token));
        self.prune_dead_tickets();
        was_head
    }
}

impl AbortQueueSink {
    fn new() -> Self {
        Self {
            state: Mutex::new(AbortQueueState {
                queue: VecDeque::new(),
                tickets: VecDeque::new(),
            }),
            changed: Notify::new(),
            pause_next_fire: AtomicBool::new(true),
            fired_reached: Notify::new(),
            fired_release: Notify::new(),
        }
    }

    fn offer_with_ticket(
        &self,
        frame: &WireFrame,
        ticket: Option<&AdmissionTicket>,
    ) -> SendAdmission {
        let mut state = self.state.lock().expect("abort queue state");
        state.prune_dead_tickets();
        let caller_may_admit =
            state.tickets.is_empty() || ticket.is_some_and(|ticket| state.ticket_is_head(ticket));
        if !caller_may_admit || !state.queue.is_empty() {
            return SendAdmission::Busy;
        }
        if let Some(ticket) = ticket
            && state.ticket_is_head(ticket)
        {
            state.tickets.pop_front();
        }
        state.queue.push_back(frame.clone());
        SendAdmission::Sent
    }

    async fn wait_for_tickets(&self, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let changed = self.changed.notified();
                if self.state.lock().expect("abort queue state").tickets.len() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("waiters park deterministically");
    }

    fn pop(&self) -> WireFrame {
        let mut state = self.state.lock().expect("abort queue state");
        let frame = state.queue.pop_front().expect("queued frame");
        state.fire_head();
        frame
    }
}

impl FrameSink for AbortQueueSink {
    fn try_send(&self, _frame: WireFrame) -> Result<(), FrameSendError> {
        Ok(())
    }

    fn offer(&self, _attachment_id: &AttachmentId, frame: &WireFrame) -> SendAdmission {
        self.offer_with_ticket(frame, None)
    }

    fn offer_ticketed(
        &self,
        _attachment_id: &AttachmentId,
        frame: &WireFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        self.offer_with_ticket(frame, Some(ticket))
    }

    fn drain_ticket(&self) -> Option<AdmissionTicket> {
        let ticket = Arc::new(Notify::new());
        self.state
            .lock()
            .expect("abort queue state")
            .tickets
            .push_back(Arc::downgrade(&ticket));
        self.changed.notify_waiters();
        Some(ticket)
    }

    fn cancel_ticket(&self, ticket: &AdmissionTicket) {
        let mut state = self.state.lock().expect("abort queue state");
        if state.remove_ticket(ticket) {
            state.fire_head();
        }
    }

    fn ticket_fired_test_gate(
        &self,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> {
        self.pause_next_fire
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| {
                Box::pin(async {
                    self.fired_reached.notify_one();
                    self.fired_release.notified().await;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>
            })
    }
}

fn caught_up(attachment_id: &AttachmentId, seq: u64) -> WireFrame {
    WireFrame::AttachCaughtUp {
        attachment_id: attachment_id.clone(),
        high_water_seq: seq,
    }
}

fn caught_up_seq(frame: WireFrame) -> u64 {
    let WireFrame::AttachCaughtUp { high_water_seq, .. } = frame else {
        panic!("expected caught-up frame");
    };
    high_water_seq
}

fn spawn_delivery(
    hub: SessionHub,
    sink: Arc<dyn FrameSink>,
    attachment_id: AttachmentId,
    seq: u64,
) -> tokio::task::JoinHandle<FrameDelivery> {
    tokio::spawn(async move {
        let (lag_sender, mut lagged) = watch::channel::<Option<u64>>(None);
        let (cancel_sender, mut cancel) = watch::channel(false);
        let keep_senders_alive = (lag_sender, cancel_sender);
        let result = deliver_frame(
            &hub,
            &sink,
            &attachment_id,
            &caught_up(&attachment_id, seq),
            &mut lagged,
            &mut cancel,
        )
        .await;
        drop(keep_senders_alive);
        result
    })
}

/// Capacity one: actual `deliver_frame` tasks A and B park, A's ticket fires,
/// and A is raw-aborted at the controlled fired-before-reoffer await. Fresh C
/// then joins; B must be admitted first and C after it without a wedge.
///
/// MUTATION CHECK: revert BOTH the `AdmissionTicketGuard` wiring/drop cleanup
/// and the connection outbox's dead-head successor firing. Expected failure:
/// B's timeout expires after C prunes dead A without waking the exposed head.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn aborting_deliver_frame_before_reoffer_keeps_fifo_admission_live() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let attachment_id = AttachmentId::new("abort-before-reoffer");
    let session_id = SessionId::new("abort-before-reoffer");
    let (commands, _command_receiver) = mpsc::channel(1);
    let (owner_cancel, _owner_cancel_receiver) = watch::channel(false);
    lock(&hub.inner.attachments)
        .expect("attachments lock")
        .insert(
            attachment_id.clone(),
            AttachmentOwner {
                connection_id: "abort-test".into(),
                session_id,
                mode: AttachMode::View,
                actor: SessionActorHandle { commands },
                cancel: owner_cancel,
            },
        );

    let sink_impl = Arc::new(AbortQueueSink::new());
    let sink: Arc<dyn FrameSink> = sink_impl.clone();
    assert!(matches!(
        sink.offer(&attachment_id, &caught_up(&attachment_id, 0)),
        SendAdmission::Sent
    ));

    let first = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 1);
    sink_impl.wait_for_tickets(1).await;

    let second = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 2);
    sink_impl.wait_for_tickets(2).await;

    assert_eq!(caught_up_seq(sink_impl.pop()), 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sink_impl.fired_reached.notified(),
    )
    .await
    .expect("A reaches the fired-before-reoffer await");
    first.abort();
    let abort_error = match first.await {
        Err(error) => error,
        Ok(_) => panic!("raw abort must cancel A"),
    };
    assert!(
        abort_error.is_cancelled(),
        "A must be dropped inside deliver_frame"
    );

    let fresh = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 3);

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), second)
            .await
            .expect("B is admitted after A abort")
            .expect("B task joins"),
        FrameDelivery::Delivered
    ));
    assert_eq!(caught_up_seq(sink_impl.pop()), 2);
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), fresh)
            .await
            .expect("C is admitted after B")
            .expect("C task joins"),
        FrameDelivery::Delivered
    ));
    assert_eq!(caught_up_seq(sink_impl.pop()), 3);

    lock(&hub.inner.attachments)
        .expect("attachments lock")
        .remove(&attachment_id);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}
