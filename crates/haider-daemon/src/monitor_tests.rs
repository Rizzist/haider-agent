#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use haider_core::{BranchCreateCommand, SessionCreateCommand, SqliteStoreHandle};
use haider_protocol::EventPayload;
use haider_protocol::ids::DeviceId;
use haider_protocol::state::{RunState, WaitReason};
use std::sync::atomic::AtomicUsize;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::{oneshot as tokio_oneshot, watch as tokio_watch};
use tokio::time::timeout;

fn registration(filter: Option<MonitorFilter>) -> MonitorRegistration {
    MonitorRegistration {
        monitor_id: "monitor-test".into(),
        owner_session_id: SessionId::new("session-monitor-test"),
        source: MonitorSource::Sms,
        filter,
        action: MonitorAction {
            report: true,
            follow_up: None,
        },
        occurrence: MonitorOccurrence::Every,
        created_at_ms: 1,
        start_sequence: 0,
        expires_at_ms: None,
        branch_id: None,
        agent_id: None,
    }
}

fn sms(address: &str, body: &str) -> MonitorEvent {
    MonitorEvent {
        sequence: 1,
        observed_at_ms: 2,
        payload: MonitorEventPayload::Sms(SmsIncomingEvent {
            address: address.into(),
            body: body.into(),
            received_at_ms: 3,
        }),
    }
}

fn test_report(report_id: &str, body: &str, status: MonitorReportStatus) -> MonitorReport {
    MonitorReport {
        report_id: report_id.into(),
        monitor_id: "monitor-test".into(),
        session_id: SessionId::new("session-monitor-test"),
        branch_id: None,
        agent_id: None,
        source: MonitorSourceKind::Sms,
        status,
        events: vec![sms("+1", body)],
        coalesced_count: 1,
        omitted_count: 0,
        action: MonitorAction {
            report: true,
            follow_up: None,
        },
    }
}

#[test]
fn filter_matching_is_typed_case_aware_and_source_scoped() {
    let body = registration(Some(MonitorFilter {
        field: MonitorFilterField::Body,
        operator: MonitorFilterOperator::Contains,
        value: "DEPLOY".into(),
        case_sensitive: false,
    }));
    assert!(monitor_matches(&body, &sms("+1", "deploy complete")));
    assert!(!monitor_matches(&body, &sms("+1", "build running")));

    let address = registration(Some(MonitorFilter {
        field: MonitorFilterField::Address,
        operator: MonitorFilterOperator::Equals,
        value: "+1555".into(),
        case_sensitive: true,
    }));
    assert!(monitor_matches(&address, &sms("+1555", "hello")));
    assert!(!monitor_matches(&address, &sms("+1666", "hello")));
}

#[test]
fn event_time_fences_registration_and_inclusive_expiry() {
    let mut watch = registration(None);
    watch.created_at_ms = 10;
    watch.start_sequence = 5;
    watch.expires_at_ms = Some(20);

    let mut before_registration = sms("+1", "before");
    before_registration.sequence = 5;
    before_registration.observed_at_ms = 10;
    assert!(!monitor_matches(&watch, &before_registration));

    let mut after_registration = before_registration.clone();
    after_registration.sequence = 6;
    assert!(monitor_matches(&watch, &after_registration));

    let mut at_expiry = after_registration.clone();
    at_expiry.observed_at_ms = 20;
    assert!(monitor_matches(&watch, &at_expiry));

    let mut after_expiry = at_expiry;
    after_expiry.observed_at_ms = 21;
    assert!(!monitor_matches(&watch, &after_expiry));
}

#[tokio::test]
async fn source_publish_seam_is_instance_scoped_and_bounded() {
    let first = MonitorSourceHub::new();
    let second = MonitorSourceHub::new();
    let mut subscription = first.subscribe(MonitorSourceKind::Sms);
    let receipt = publish_sms_incoming(&first, "+1555", "wake", 10).unwrap();
    assert_eq!(receipt.subscriber_count, 1);
    let event = subscription.recv().await.unwrap();
    assert_eq!(event.sequence, 1);
    assert!(matches!(
        event.payload,
        MonitorEventPayload::Sms(SmsIncomingEvent {
            ref address,
            ref body,
            received_at_ms: 10,
        }) if address == "+1555" && body == "wake"
    ));

    let other = publish_sms_incoming(&second, "+1555", "isolated", 11).unwrap();
    assert_eq!(other.subscriber_count, 0);
    assert!(publish_sms_incoming(&first, "x", &"b".repeat(MAX_SMS_BODY_BYTES + 1), 1).is_err());
}

#[test]
fn journal_fold_registers_lists_and_removes() {
    let registry = MonitorRegistry::default();
    let session = SessionId::new("session-monitor-test");
    let watch = registration(None);
    registry.install(session.clone(), BTreeMap::new(), BTreeMap::new());
    registry.insert(&session, watch.clone());
    assert_eq!(registry.snapshot(&session), vec![watch]);
    assert!(registry.remove(&session, "monitor-test").is_some());
    assert!(registry.snapshot(&session).is_empty());
}

#[test]
fn durable_queue_order_beats_equal_or_rollback_wall_clock_on_adoption() {
    let session = SessionId::new("session-monitor-test");
    let active = PendingMonitorReport {
        report: test_report("z-active", "first", MonitorReportStatus::Matched),
        terminal_reason: None,
        queue_order: 7,
        queued_at_ms: 100,
    };
    let follow_up = PendingMonitorReport {
        report: test_report("a-follow-up", "second", MonitorReportStatus::RateLimited),
        terminal_reason: Some(MonitorRemovalReason::RateLimited),
        queue_order: 8,
        // A rolled-back wall clock and lexically earlier id must not let
        // the terminal follow-up overtake the active report on restart.
        queued_at_ms: 1,
    };
    let mut pending = BTreeMap::new();
    pending.insert(active.report.report_id.clone(), active.clone());
    pending.insert(follow_up.report.report_id.clone(), follow_up.clone());
    assert_eq!(oldest_pending_per_monitor(&pending), vec![active.clone()]);
    let mut equal_time = follow_up.clone();
    equal_time.queued_at_ms = active.queued_at_ms;
    pending.insert(equal_time.report.report_id.clone(), equal_time);
    assert_eq!(oldest_pending_per_monitor(&pending), vec![active.clone()]);
    pending.insert(follow_up.report.report_id.clone(), follow_up.clone());

    let registry = MonitorRegistry::default();
    registry.install(session.clone(), BTreeMap::new(), pending);
    assert_eq!(
        registry.pending_for_monitor(&session, "monitor-test"),
        vec![active, follow_up]
    );
}

#[test]
fn pre_durable_retry_queue_is_bounded_and_coalesces_its_follow_up() {
    let watch = registration(None);
    let mut queue = EnqueueRetryQueue::default();
    for (id, body) in [
        ("retry-first", "first"),
        ("retry-second", "second"),
        ("retry-third", "third"),
    ] {
        queue.push(EnqueueRetryItem {
            registration: watch.clone(),
            report: test_report(id, body, MonitorReportStatus::Matched),
            terminal_reason: None,
            wait_for_source_sequence: None,
        });
    }
    assert_eq!(queue.items.len(), 2);
    assert_eq!(queue.items[0].report.report_id, "retry-first");
    assert_eq!(queue.items[1].report.report_id, "retry-second");
    assert_eq!(queue.items[1].report.coalesced_count, 2);
    assert!(queue.items[1].report.events.iter().any(|event| {
        matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "third")
    }));

    queue.push(EnqueueRetryItem {
        registration: watch,
        report: test_report("retry-terminal", "stop", MonitorReportStatus::RateLimited),
        terminal_reason: Some(MonitorRemovalReason::RateLimited),
        wait_for_source_sequence: None,
    });
    assert_eq!(queue.items.len(), 2);
    assert_eq!(
        queue.items[1].terminal_reason,
        Some(MonitorRemovalReason::RateLimited)
    );
    assert_eq!(
        queue.items[1].report.status,
        MonitorReportStatus::RateLimited
    );
    assert_eq!(queue.items[1].report.coalesced_count, 3);
}

#[test]
fn report_event_and_prompt_bounds_are_explicit() {
    let mut events = Vec::new();
    for sequence in 0..(MAX_MONITOR_REPORT_EVENTS + 3) {
        let mut event = sms("+1", &"x".repeat(MAX_REPORT_BODY_CHARS + 10));
        event.sequence = sequence as u64;
        events.push(event);
    }
    let watch = registration(None);
    let session = SessionId::new("session-report");
    let coalesced_count = events.len();
    let omitted_count = events.len().saturating_sub(MAX_MONITOR_REPORT_EVENTS);
    events.truncate(MAX_MONITOR_REPORT_EVENTS);
    let report = MonitorReport {
        report_id: "report".into(),
        monitor_id: watch.monitor_id,
        session_id: session,
        branch_id: None,
        agent_id: None,
        source: MonitorSourceKind::Sms,
        status: MonitorReportStatus::Matched,
        events,
        coalesced_count,
        omitted_count,
        action: watch.action,
    };
    assert_eq!(report.events.len(), MAX_MONITOR_REPORT_EVENTS);
    assert_eq!(report.omitted_count, 3);
    assert!(report.prompt_text().contains("monitor_event"));
}

#[test]
fn client_availability_is_exhaustive_and_does_not_invent_adapters() {
    use haider_rpc::{
        MonitorSourceAvailabilityStateWire as Availability, MonitorSourceKindWire as Source,
        MonitorSourceUnavailableReasonWire as Reason,
    };

    assert_eq!(
        monitor_source_availability(),
        vec![
            haider_rpc::MonitorSourceAvailabilityWire {
                source: Source::Sms,
                availability: Availability::Available,
            },
            haider_rpc::MonitorSourceAvailabilityWire {
                source: Source::Process,
                availability: Availability::Unavailable {
                    reason: Reason::AdapterInactive,
                },
            },
            haider_rpc::MonitorSourceAvailabilityWire {
                source: Source::File,
                availability: Availability::Unavailable {
                    reason: Reason::AdapterInactive,
                },
            },
            haider_rpc::MonitorSourceAvailabilityWire {
                source: Source::Poll,
                availability: Availability::Unavailable {
                    reason: Reason::AdapterInactive,
                },
            },
            haider_rpc::MonitorSourceAvailabilityWire {
                source: Source::Timer,
                availability: Availability::Unavailable {
                    reason: Reason::AdapterInactive,
                },
            },
        ]
    );
    assert_eq!(monitor_control_policy().list, haider_rpc::Capability::View);
    assert_eq!(monitor_control_policy().watch, haider_rpc::Capability::View);
    assert_eq!(
        monitor_control_policy().register,
        haider_rpc::Capability::Control
    );
    assert!(monitor_control_policy().register_requires_control_attachment);
    assert_eq!(
        monitor_control_policy().remove,
        haider_rpc::Capability::Control
    );
    assert!(monitor_control_policy().remove_requires_control_attachment);

    assert!(matches!(
        monitor_store_rejection(MonitorError::StoreUnavailable {
            message: "transient".into(),
            retryable: true,
        }),
        haider_rpc::MonitorControlRejectionWire::StoreUnavailable {
            retryable: true,
            ..
        }
    ));
    assert!(matches!(
        monitor_store_rejection(MonitorError::Store("invalid receipt shape".into())),
        haider_rpc::MonitorControlRejectionWire::StoreUnavailable {
            retryable: false,
            ..
        }
    ));
}

#[test]
fn client_delivery_projects_durable_report_with_cursor_and_dedupe() {
    let session = SessionId::new("session-delivery-projection");
    let mut report = test_report(
        "report-projection",
        "bounded body",
        MonitorReportStatus::Matched,
    );
    report.session_id = session.clone();
    report.coalesced_count = 5;
    report.omitted_count = 4;
    let pending = PendingMonitorReport {
        report,
        terminal_reason: None,
        queue_order: 11,
        queued_at_ms: 17,
    };
    let mut envelope = monitor_envelope(
        &session,
        None,
        None,
        None,
        "monitor-report-projection",
        DeviceId::new("monitor-report-device"),
        9,
        MonitorJournalEvent::MonitorReportPending { pending }
            .to_value()
            .expect("encode monitor report"),
    );
    envelope.seq = 41;

    let delivery = monitor_delivery_report(&envelope).expect("monitor delivery projection");
    assert_eq!(delivery.report_id, "report-projection");
    assert_eq!(delivery.session_id, session);
    assert_eq!(delivery.cursor, 41);
    assert_eq!(delivery.coalesced_count, 5);
    assert_eq!(delivery.omitted_count, 4);
    assert_eq!(delivery.events.len(), 1);
    assert_eq!(delivery.dedupe.report_key, "report-projection");
    assert!(
        delivery
            .dedupe
            .delivery_key
            .starts_with("monitor-delivery-")
    );

    envelope.session_id = SessionId::new("fork-copy");
    assert!(monitor_delivery_report(&envelope).is_none());
}

struct MonitorWorld {
    store: SqliteStoreHandle,
    hub: SessionHub,
    session: SessionId,
    run: RunId,
    lease: HubStoreHandle,
    _root: tempfile::TempDir,
}

impl MonitorWorld {
    async fn new(label: &str) -> Self {
        let root = tempfile::tempdir().expect("temporary monitor profile");
        let store = SqliteStoreHandle::open(root.path())
            .await
            .expect("monitor store");
        let hub = SessionHub::new(
            store.clone(),
            crate::session_hub::SessionHubConfig::default(),
        )
        .expect("monitor hub");
        // Production activates after installing WorkerManager. These
        // subsystem tests intentionally exercise the sink seam directly.
        hub.inner_monitor().activate(hub.downgrade());
        let session = SessionId::new(format!("monitor-session-{label}"));
        let device = DeviceId::new(format!("monitor-device-{label}"));
        let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
            .to_string_lossy()
            .into_owned();
        hub.create_internal_session(SessionCreateCommand {
            command_id: format!("monitor-create-{label}"),
            request_digest: format!("monitor-create-{label}-digest"),
            request_json: format!(r#"{{"session":"{label}"}}"#),
            session_id: session.clone(),
            cwd,
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("monitor-created-{label}")),
            device_id: device.clone(),
        })
        .await
        .expect("create monitor session");
        let run = RunId::new(format!("monitor-tool-run-{label}"));
        hub.accept_internal_turn(TurnAcceptCommand {
            command_id: format!("monitor-tool-submit-{label}"),
            request_digest: format!("monitor-tool-submit-{label}-digest"),
            request_json: format!(r#"{{"turn":"{label}"}}"#),
            session_id: session.clone(),
            worker_generation: store.worker_generation(),
            run_id: run.clone(),
            agent_id: None,
            branch_id: None,
            text: "register a monitor".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new(format!("monitor-tool-queued-{label}")),
            user_event_id: EventId::new(format!("monitor-tool-user-{label}")),
            active_event_id: EventId::new(format!("monitor-tool-active-{label}")),
            device_id: device,
        })
        .await
        .expect("accept monitor tool turn");
        let lease = hub
            .acquire_worker_lease(session.clone())
            .await
            .expect("monitor worker lease");
        Self {
            store,
            hub,
            session,
            run,
            lease,
            _root: root,
        }
    }

    fn coordinates(&self, call: &str) -> MonitorToolCoordinates {
        MonitorToolCoordinates {
            run_id: self.run.clone(),
            branch_id: None,
            agent_id: None,
            call_id: call.to_owned(),
            device_id: DeviceId::new(format!("monitor-call-{call}")),
        }
    }

    async fn execute(&self, call: &str, request: MonitorRequest) -> BoundedResult {
        self.hub
            .execute_monitor_tool(&self.lease, self.coordinates(call), request)
            .await
            .expect("execute monitor tool")
    }

    async fn register(
        &self,
        call: &str,
        filter: Option<MonitorFilter>,
        occurrence: MonitorOccurrence,
    ) -> String {
        self.register_with_lifetime(call, filter, occurrence, MonitorLifetime::Session)
            .await
    }

    async fn register_with_lifetime(
        &self,
        call: &str,
        filter: Option<MonitorFilter>,
        occurrence: MonitorOccurrence,
        lifetime: MonitorLifetime,
    ) -> String {
        let result = self
            .execute(
                call,
                MonitorRequest::Register {
                    source: MonitorSource::Sms,
                    filter,
                    action: MonitorAction {
                        report: true,
                        follow_up: Some("react to this SMS".into()),
                    },
                    occurrence,
                    lifetime,
                },
            )
            .await;
        assert_eq!(result.status, ToolResultStatus::Completed);
        serde_json::from_str::<serde_json::Value>(&result.preview)
            .expect("registration preview")
            .get("monitor_id")
            .and_then(serde_json::Value::as_str)
            .expect("monitor id")
            .to_owned()
    }

    async fn wait_for_count(&self, expected: usize) -> BoundedResult {
        timeout(Duration::from_secs(3), async {
            loop {
                let result = self.execute("list-poll", MonitorRequest::List).await;
                let count = serde_json::from_str::<serde_json::Value>(&result.preview)
                    .expect("monitor list preview")
                    .get("count")
                    .and_then(serde_json::Value::as_u64)
                    .expect("monitor list count") as usize;
                if count == expected {
                    break result;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("monitor count did not converge")
    }

    fn install_canonical_test_sink(&self, sink: Arc<dyn MonitorDeliverySink>) {
        *self
            .hub
            .inner_monitor()
            .inner
            .sink
            .write()
            .unwrap_or_else(PoisonError::into_inner) = sink;
    }
}

struct CapturingSink {
    reports: tokio_mpsc::UnboundedSender<MonitorReport>,
}

struct FailOnceSink {
    attempts: Arc<AtomicUsize>,
    reports: tokio_mpsc::UnboundedSender<MonitorReport>,
}

struct GatedSink {
    reports: tokio_mpsc::UnboundedSender<MonitorReport>,
    started: StdMutex<Option<tokio_oneshot::Sender<()>>>,
    release: tokio_watch::Receiver<bool>,
}

#[async_trait]
impl MonitorDeliverySink for FailOnceSink {
    async fn deliver(
        &self,
        _session: &SessionId,
        report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(MonitorError::Delivery("injected first failure".into()));
        }
        self.reports
            .send(report)
            .map_err(|_| MonitorError::Delivery("test report receiver was dropped".into()))?;
        Ok(MonitorDeliveryReceipt {
            durable: true,
            handed_off: true,
            disposition: "captured",
        })
    }
}

#[async_trait]
impl MonitorDeliverySink for CapturingSink {
    async fn deliver(
        &self,
        _session: &SessionId,
        report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError> {
        self.reports
            .send(report)
            .map_err(|_| MonitorError::Delivery("test report receiver was dropped".into()))?;
        Ok(MonitorDeliveryReceipt {
            durable: true,
            handed_off: true,
            disposition: "captured",
        })
    }
}

#[async_trait]
impl MonitorDeliverySink for GatedSink {
    async fn deliver(
        &self,
        _session: &SessionId,
        report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError> {
        self.reports
            .send(report)
            .map_err(|_| MonitorError::Delivery("test report receiver was dropped".into()))?;
        if let Some(started) = self
            .started
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = started.send(());
        }
        let mut release = self.release.clone();
        while !*release.borrow() {
            release
                .changed()
                .await
                .map_err(|_| MonitorError::Delivery("test delivery gate was dropped".into()))?;
        }
        Ok(MonitorDeliveryReceipt {
            durable: true,
            handed_off: true,
            disposition: "captured",
        })
    }
}

#[tokio::test]
async fn register_list_remove_and_durable_readoption() {
    let world = MonitorWorld::new("crud").await;
    let monitor_id = world
        .register("register", None, MonitorOccurrence::Every)
        .await;
    let replayed_id = world
        .register("register", None, MonitorOccurrence::Every)
        .await;
    assert_eq!(replayed_id, monitor_id);
    assert!(world.wait_for_count(1).await.preview.contains(&monitor_id));
    let altered = world
        .hub
        .execute_monitor_tool(
            &world.lease,
            world.coordinates("register"),
            MonitorRequest::Register {
                source: MonitorSource::Sms,
                filter: None,
                action: MonitorAction {
                    report: true,
                    follow_up: Some("different replay".into()),
                },
                occurrence: MonitorOccurrence::Once,
                lifetime: MonitorLifetime::Session,
            },
        )
        .await
        .expect_err("changed replay must conflict");
    assert!(altered.to_string().contains("different arguments"));
    let listed = world.execute("list", MonitorRequest::List).await;
    assert!(listed.preview.contains(&monitor_id));

    // Drop only the projection; the next list must fold the durable facts.
    world
        .hub
        .inner_monitor()
        .inner
        .registry
        .forget_session(&world.session);
    let readopted = world.execute("readopt", MonitorRequest::List).await;
    assert!(readopted.preview.contains(&monitor_id));

    let removed = world
        .execute(
            "remove",
            MonitorRequest::Remove {
                monitor_id: monitor_id.clone(),
            },
        )
        .await;
    assert!(removed.preview.contains("removed"));
    let replayed_remove = world
        .execute(
            "remove",
            MonitorRequest::Remove {
                monitor_id: monitor_id.clone(),
            },
        )
        .await;
    assert_eq!(replayed_remove, removed);
    let empty = world.execute("empty", MonitorRequest::List).await;
    assert!(empty.preview.contains(r#""count":0"#));
}

/// MUTATION CHECK: scope a command receipt to one session/method or skip
/// the session-local recovery receipt. Expected runtime failure: replay
/// diverges or the cross-method reuse stops returning CommandConflict.
#[tokio::test]
async fn client_control_reuses_registry_and_replays_typed_receipts() {
    let world = MonitorWorld::new("client-control").await;
    let generation = world.hub.worker_generation();
    let request = MonitorClientRegistrationRequest {
        command_id: haider_rpc::CommandId::new("monitor-client-register"),
        session_id: world.session.clone(),
        worker_generation: generation,
        source: haider_rpc::MonitorSourceWire::Sms,
        filter: None,
        action: haider_rpc::MonitorActionWire {
            report: true,
            follow_up: Some("react to this SMS".into()),
        },
        occurrence: haider_rpc::MonitorOccurrenceWire::Every,
        lifetime: haider_rpc::MonitorLifetimeWire::Session,
    };
    let receipt = world
        .hub
        .inner_monitor()
        .client_register(&world.hub, request.clone())
        .await;
    let monitor_id = match &receipt.outcome {
        haider_rpc::MonitorRegisterOutcomeWire::Registered { monitor } => {
            monitor.monitor_id.clone()
        }
        other => panic!("expected registered receipt, got {other:?}"),
    };
    let replay = world
        .hub
        .inner_monitor()
        .client_register(&world.hub, request.clone())
        .await;
    assert_eq!(replay, receipt);

    let mut cross_session_request = request;
    cross_session_request.session_id = SessionId::new("monitor-session-other");
    let cross_session = world
        .hub
        .inner_monitor()
        .client_register(&world.hub, cross_session_request)
        .await;
    assert!(matches!(
        cross_session.outcome,
        haider_rpc::MonitorRegisterOutcomeWire::Rejected {
            rejection: haider_rpc::MonitorControlRejectionWire::CommandConflict
        }
    ));

    let cross_method = world
        .hub
        .inner_monitor()
        .client_remove(
            &world.hub,
            haider_rpc::CommandId::new("monitor-client-register"),
            world.session.clone(),
            generation,
            monitor_id.clone(),
        )
        .await;
    assert!(matches!(
        cross_method.outcome,
        haider_rpc::MonitorRemoveOutcomeWire::Rejected {
            rejection: haider_rpc::MonitorControlRejectionWire::CommandConflict
        }
    ));

    let listed = world
        .hub
        .inner_monitor()
        .client_list(&world.hub, world.session.clone())
        .await;
    match listed.outcome {
        haider_rpc::MonitorListOutcomeWire::Listed { monitors } => {
            assert_eq!(monitors.len(), 1);
            assert_eq!(monitors[0].monitor_id, monitor_id);
        }
        other => panic!("expected monitor list, got {other:?}"),
    }

    let removed = world
        .hub
        .inner_monitor()
        .client_remove(
            &world.hub,
            haider_rpc::CommandId::new("monitor-client-remove"),
            world.session.clone(),
            generation,
            monitor_id.clone(),
        )
        .await;
    assert_eq!(
        removed.outcome,
        haider_rpc::MonitorRemoveOutcomeWire::Removed {
            monitor_id: monitor_id.clone(),
        }
    );
    let replayed_remove = world
        .hub
        .inner_monitor()
        .client_remove(
            &world.hub,
            haider_rpc::CommandId::new("monitor-client-remove"),
            world.session.clone(),
            generation,
            monitor_id,
        )
        .await;
    assert_eq!(replayed_remove, removed);
}

#[tokio::test]
async fn durable_registry_is_rebuilt_after_store_reopen() {
    let world = MonitorWorld::new("reopen").await;
    let monitor_id = world
        .register("register", None, MonitorOccurrence::Every)
        .await;
    let MonitorWorld {
        store,
        hub,
        session,
        run: _,
        lease,
        _root: root,
    } = world;
    hub.inner_monitor()
        .shutdown()
        .await
        .expect("shutdown first monitor service");
    drop(lease);
    drop(hub);
    store.close().await.expect("close first monitor store");

    let reopened_store = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen monitor store");
    let reopened_hub = SessionHub::new(
        reopened_store.clone(),
        crate::session_hub::SessionHubConfig::default(),
    )
    .expect("reopened monitor hub");
    reopened_hub
        .inner_monitor()
        .activate(reopened_hub.downgrade());
    reopened_hub
        .inner_monitor()
        .adopt_session(&reopened_hub, &session)
        .await
        .expect("adopt reopened monitor registry");
    assert_eq!(
        reopened_hub
            .inner_monitor()
            .inner
            .registry
            .snapshot(&session)
            .first()
            .map(|registration| registration.monitor_id.as_str()),
        Some(monitor_id.as_str())
    );
    reopened_hub
        .inner_monitor()
        .shutdown()
        .await
        .expect("shutdown reopened monitor service");
    drop(reopened_hub);
    reopened_store
        .close()
        .await
        .expect("close reopened monitor store");
}

#[tokio::test]
async fn failed_durable_delete_rollback_restores_registry_timeout_and_pending_delivery() {
    let world = MonitorWorld::new("delete-rollback").await;
    let monitor_id = world
        .register_with_lifetime(
            "register",
            None,
            MonitorOccurrence::Every,
            MonitorLifetime::Timeout { timeout_ms: 60_000 },
        )
        .await;
    let (blocked_reports, mut blocked_report) = tokio_mpsc::unbounded_channel();
    let (started, delivery_started) = tokio_oneshot::channel();
    let (_release, release_gate) = tokio_watch::channel(false);
    world.install_canonical_test_sink(Arc::new(GatedSink {
        reports: blocked_reports,
        started: StdMutex::new(Some(started)),
        release: release_gate,
    }));
    publish_sms_incoming(
        &world.hub.monitor_source_hub(),
        "+1",
        "pending across failed delete",
        1,
    )
    .expect("publish pending delete event");
    timeout(Duration::from_secs(3), delivery_started)
        .await
        .expect("pending delivery did not start")
        .expect("pending delivery start sender dropped");
    let original = blocked_report.recv().await.expect("blocked pending report");

    // This is the exact monitor transaction around SessionHub's durable
    // delete call: forget first, then restore because that call failed.
    // The SQLite store remains intact, as it does on a failed delete.
    world
        .hub
        .inner_monitor()
        .forget_session(&world.hub, &world.session)
        .await
        .expect("forget monitors before simulated delete failure");
    assert!(
        world
            .hub
            .inner_monitor()
            .inner
            .registry
            .get(&world.session, &monitor_id)
            .is_none()
    );

    let (restored_reports, mut restored_report) = tokio_mpsc::unbounded_channel();
    world.install_canonical_test_sink(Arc::new(CapturingSink {
        reports: restored_reports,
    }));
    world
        .hub
        .inner_monitor()
        .restore_session(&world.hub, &world.session)
        .await
        .expect("restore monitors after simulated delete failure");
    assert!(
        world
            .hub
            .inner_monitor()
            .inner
            .registry
            .get(&world.session, &monitor_id)
            .is_some()
    );
    assert!(
        world
            .hub
            .inner_monitor()
            .inner
            .timeout_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&(world.session.clone(), monitor_id.clone()))
    );
    let restored = timeout(Duration::from_secs(3), restored_report.recv())
        .await
        .expect("restored pending delivery timeout")
        .expect("restored pending delivery");
    assert_eq!(restored.report_id, original.report_id);
    assert!(restored.events.iter().any(|event| {
        matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "pending across failed delete")
    }));
}

#[tokio::test]
async fn published_sms_wakes_a_normal_durable_turn_through_default_sink() {
    let world = MonitorWorld::new("wake").await;
    let monitor_id = world
        .register("register", None, MonitorOccurrence::Every)
        .await;
    let receipt = publish_sms_incoming(
        &world.hub.monitor_source_hub(),
        "+15551212",
        "deploy completed",
        99,
    )
    .expect("publish SMS monitor event");
    assert_eq!(receipt.subscriber_count, 1);

    timeout(Duration::from_secs(3), async {
        loop {
            let events = world
                .store
                .read(&world.session, 0, 512)
                .await
                .expect("read monitor wake journal");
            if events.into_iter().any(|event| {
                serde_json::from_value::<EventPayload>(event.payload).is_ok_and(|payload| {
                    matches!(
                        payload,
                        EventPayload::UserMessage { text, .. }
                            if text.contains("monitor_event") && text.contains(&monitor_id)
                    )
                })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("monitor wake was not durably accepted");
}

#[tokio::test]
async fn named_branch_waiting_run_is_woken_as_a_subturn() {
    let world = MonitorWorld::new("named-waiting").await;
    let mut done = [monitor_envelope(
        &world.session,
        Some(&world.run),
        None,
        None,
        "monitor-named-source-done",
        DeviceId::new("monitor-named-device"),
        world.store.worker_generation(),
        serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("encode source done"),
    )];
    world
        .lease
        .append(&mut done)
        .await
        .expect("finish source run");
    let source_events = world
        .store
        .read(&world.session, 0, 128)
        .await
        .expect("read source history");
    let (fork_node_id, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            (event.run_id.as_ref() == Some(&world.run)).then_some((node.node, event.seq))
        })
        .expect("source fork node");
    let branch_id = BranchId::new("monitor-named-branch");
    let branch_request = r#"{"fork":"monitor-source"}"#.to_owned();
    world
        .store
        .create_branch(BranchCreateCommand {
            command_id: "monitor-create-named-branch".into(),
            request_digest: blake3::hash(branch_request.as_bytes()).to_hex().to_string(),
            request_json: branch_request,
            session_id: world.session.clone(),
            worker_generation: world.store.worker_generation(),
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id,
            fork_seq,
            name: Some("Monitor branch".into()),
            event_id: EventId::new("monitor-named-branch-created"),
            device_id: DeviceId::new("monitor-named-device"),
        })
        .await
        .expect("create monitor branch");
    let branch_run = RunId::new("monitor-named-branch-run");
    world
        .hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "monitor-accept-named-run".into(),
            request_digest: "monitor-accept-named-run-digest".into(),
            request_json: r#"{"turn":"named"}"#.into(),
            session_id: world.session.clone(),
            worker_generation: world.store.worker_generation(),
            run_id: branch_run.clone(),
            agent_id: None,
            branch_id: Some(branch_id.clone()),
            text: "work on named branch".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("monitor-named-queued"),
            user_event_id: EventId::new("monitor-named-user"),
            active_event_id: EventId::new("monitor-named-active"),
            device_id: DeviceId::new("monitor-named-device"),
        })
        .await
        .expect("accept named run");
    let branch_lease = world
        .hub
        .acquire_worker_lease(world.session.clone())
        .await
        .expect("named branch lease");
    let mut waiting = [monitor_envelope(
        &world.session,
        Some(&branch_run),
        Some(&branch_id),
        None,
        "monitor-named-waiting",
        DeviceId::new("monitor-named-device"),
        world.store.worker_generation(),
        serde_json::to_value(EventPayload::RunState(RunState::Waiting {
            reason: WaitReason::Dependency,
        }))
        .expect("encode waiting state"),
    )];
    branch_lease
        .append(&mut waiting)
        .await
        .expect("park named run");
    let report = MonitorReport {
        report_id: "monitor-report-named-waiting".into(),
        monitor_id: "monitor-named".into(),
        session_id: world.session.clone(),
        branch_id: Some(branch_id.clone()),
        agent_id: None,
        source: MonitorSourceKind::Sms,
        status: MonitorReportStatus::Matched,
        events: vec![sms("+1", "wake named")],
        coalesced_count: 1,
        omitted_count: 0,
        action: MonitorAction {
            report: true,
            follow_up: None,
        },
    };
    let error = world
        .hub
        .wake_monitor_report(report)
        .await
        .expect_err("missing manager must retain the subturn for retry");
    assert!(
        error
            .to_string()
            .contains("could not reach the worker manager")
    );
    let events = world
        .store
        .read(&world.session, 0, 256)
        .await
        .expect("read named wake");
    assert!(events.into_iter().any(|event| {
        event.run_id.as_ref() == Some(&branch_run)
            && serde_json::from_value::<EventPayload>(event.payload).is_ok_and(|payload| {
                matches!(
                    payload,
                    EventPayload::UserMessage { text, mode, .. }
                        if mode == DeliveryMode::Subturn && text.contains("monitor_event")
                )
            })
    }));
}

#[tokio::test]
async fn matching_bursts_coalesce_and_a_firehose_auto_stops() {
    let world = MonitorWorld::new("rate").await;
    world
        .register("register", None, MonitorOccurrence::Every)
        .await;
    let (reports, mut received) = tokio_mpsc::unbounded_channel();
    world.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
    let sources = world.hub.monitor_source_hub();
    for index in 0..3 {
        publish_sms_incoming(&sources, "+1", &format!("burst-{index}"), index)
            .expect("publish coalesced SMS");
    }
    let first = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("coalesced report timeout")
        .expect("coalesced report");
    assert_eq!(first.status, MonitorReportStatus::Matched);
    assert_eq!(first.coalesced_count, 3);

    for index in 0..62 {
        publish_sms_incoming(&sources, "+1", &format!("firehose-{index}"), 100 + index)
            .expect("publish firehose SMS");
    }
    let stopped = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("rate-limit report timeout")
        .expect("rate-limit report");
    assert_eq!(stopped.status, MonitorReportStatus::RateLimited);
    assert_eq!(stopped.coalesced_count, 62);
    let empty = world.wait_for_count(0).await;
    assert!(empty.preview.contains(r#""count":0"#));
}

#[tokio::test]
async fn failed_delivery_retries_the_same_durable_report() {
    let world = MonitorWorld::new("delivery-retry").await;
    let monitor_id = world
        .register("register", None, MonitorOccurrence::Every)
        .await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let (reports, mut received) = tokio_mpsc::unbounded_channel();
    world.install_canonical_test_sink(Arc::new(FailOnceSink {
        attempts: Arc::clone(&attempts),
        reports,
    }));
    publish_sms_incoming(&world.hub.monitor_source_hub(), "+1", "retry me", 1)
        .expect("publish retry event");
    let report = timeout(Duration::from_secs(4), received.recv())
        .await
        .expect("retried report timeout")
        .expect("retried report");
    assert_eq!(report.monitor_id, monitor_id);
    assert_eq!(report.coalesced_count, 1);
    assert!(attempts.load(Ordering::SeqCst) >= 2);
    timeout(Duration::from_secs(3), async {
        loop {
            if world
                .hub
                .inner_monitor()
                .inner
                .registry
                .pending_summary(&world.session, &monitor_id)
                .0
                == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delivered outbox report was not acknowledged");
    world.wait_for_count(1).await;
}

#[tokio::test]
async fn stalled_delivery_retains_a_bounded_follow_up_occurrence() {
    let world = MonitorWorld::new("delivery-follow-up").await;
    let monitor_id = world
        .register("register", None, MonitorOccurrence::Every)
        .await;
    let (reports, mut received) = tokio_mpsc::unbounded_channel();
    let (started, delivery_started) = tokio_oneshot::channel();
    let (release, release_gate) = tokio_watch::channel(false);
    world.install_canonical_test_sink(Arc::new(GatedSink {
        reports,
        started: StdMutex::new(Some(started)),
        release: release_gate,
    }));
    let sources = world.hub.monitor_source_hub();
    publish_sms_incoming(&sources, "+1", "first occurrence", 1).expect("publish first occurrence");
    timeout(Duration::from_secs(3), delivery_started)
        .await
        .expect("first delivery did not start")
        .expect("first delivery start sender dropped");

    // This event lands after the source coalescing window while the first
    // durable delivery remains blocked. It must occupy the bounded
    // follow-up slot rather than disappear as AlreadyPending.
    publish_sms_incoming(&sources, "+1", "second occurrence", 2)
        .expect("publish second occurrence");
    timeout(Duration::from_secs(3), async {
        loop {
            if world
                .hub
                .inner_monitor()
                .inner
                .registry
                .pending_summary(&world.session, &monitor_id)
                .0
                == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("follow-up occurrence did not enter the durable outbox");

    release.send_replace(true);
    let first = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("first report timeout")
        .expect("first report");
    let second = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("follow-up report timeout")
        .expect("follow-up report");
    assert!(first.events.iter().any(|event| {
        matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "first occurrence")
    }));
    assert!(second.events.iter().any(|event| {
        matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "second occurrence")
    }));
    timeout(Duration::from_secs(3), async {
        loop {
            if world
                .hub
                .inner_monitor()
                .inner
                .registry
                .pending_summary(&world.session, &monitor_id)
                .0
                == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("follow-up report was not acknowledged");
}

#[tokio::test]
async fn pre_expiry_event_is_classified_before_timeout_via_source_watermark() {
    let world = MonitorWorld::new("expiry-watermark").await;
    let monitor_id = world
        .register("register", None, MonitorOccurrence::Every)
        .await;
    let sources = world.hub.monitor_source_hub();
    let mut registration = world
        .hub
        .inner_monitor()
        .inner
        .registry
        .get(&world.session, &monitor_id)
        .expect("registered monitor");
    registration.created_at_ms = now_ms();
    registration.start_sequence = sources.current_sequence();
    registration.expires_at_ms = Some(now_ms().saturating_add(100));
    world
        .hub
        .inner_monitor()
        .inner
        .registry
        .insert(&world.session, registration.clone());
    world.hub.inner_monitor().schedule_timeout(
        world.hub.downgrade(),
        world.session.clone(),
        registration,
    );
    let (reports, mut received) = tokio_mpsc::unbounded_channel();
    world.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
    publish_sms_incoming(&sources, "+1", "just before expiry", 1)
        .expect("publish pre-expiry event");

    // Source classification intentionally waits 250ms, past the 100ms
    // deadline. The timeout worker's explicit source watermark must keep
    // the earlier event in front of the terminal report.
    let matched = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("matched report timeout")
        .expect("matched report");
    let timed_out = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("timeout report timeout")
        .expect("timeout report");
    assert_eq!(matched.status, MonitorReportStatus::Matched);
    assert!(matched.events.iter().any(|event| {
        matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "just before expiry")
    }));
    assert_eq!(timed_out.status, MonitorReportStatus::TimedOut);
}

#[tokio::test]
async fn once_stops_after_delivery_while_every_remains_active() {
    let world = MonitorWorld::new("occurrence").await;
    let once = world.register("once", None, MonitorOccurrence::Once).await;
    let every = world
        .register("every", None, MonitorOccurrence::Every)
        .await;
    let (reports, mut received) = tokio_mpsc::unbounded_channel();
    world.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
    publish_sms_incoming(&world.hub.monitor_source_hub(), "+1", "event", 1)
        .expect("publish occurrence event");
    let first = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("first occurrence report timeout")
        .expect("first occurrence report");
    let second = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("second occurrence report timeout")
        .expect("second occurrence report");
    assert!(
        [first.monitor_id, second.monitor_id]
            .iter()
            .any(|monitor| monitor == &once)
    );
    let remaining = world.wait_for_count(1).await;
    assert!(remaining.preview.contains(&every));
    assert!(!remaining.preview.contains(&once));
}

#[tokio::test]
async fn timeout_reports_and_stops_while_session_lifetime_persists() {
    let timed = MonitorWorld::new("timeout").await;
    let (reports, mut received) = tokio_mpsc::unbounded_channel();
    timed.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
    timed
        .register_with_lifetime(
            "timed",
            None,
            MonitorOccurrence::Every,
            MonitorLifetime::Timeout { timeout_ms: 100 },
        )
        .await;
    let report = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("timeout report wait")
        .expect("timeout report");
    assert_eq!(report.status, MonitorReportStatus::TimedOut);
    assert_eq!(report.coalesced_count, 0);
    timed.wait_for_count(0).await;

    let persistent = MonitorWorld::new("session-lifetime").await;
    let monitor_id = persistent
        .register("persistent", None, MonitorOccurrence::Every)
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        persistent
            .wait_for_count(1)
            .await
            .preview
            .contains(&monitor_id)
    );
}

#[tokio::test]
async fn bounded_registry_and_filter_matching() {
    let world = MonitorWorld::new("bounds-filter").await;
    let filter = MonitorFilter {
        field: MonitorFilterField::Body,
        operator: MonitorFilterOperator::Contains,
        value: "ship".into(),
        case_sensitive: false,
    };
    world
        .register("filtered", Some(filter), MonitorOccurrence::Every)
        .await;
    let (reports, mut received) = tokio_mpsc::unbounded_channel();
    world.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
    let sources = world.hub.monitor_source_hub();
    publish_sms_incoming(&sources, "+1", "ignore", 1).expect("publish mismatch");
    tokio::time::sleep(MONITOR_COALESCE_WINDOW + Duration::from_millis(50)).await;
    assert!(received.try_recv().is_err());
    publish_sms_incoming(&sources, "+1", "SHIP it", 2).expect("publish match");
    let matched = timeout(Duration::from_secs(3), received.recv())
        .await
        .expect("filter match timeout")
        .expect("filter match report");
    assert_eq!(matched.coalesced_count, 1);

    for index in 1..MAX_MONITORS_PER_SESSION {
        world
            .register(&format!("fill-{index}"), None, MonitorOccurrence::Every)
            .await;
    }
    let overflow = world
        .execute(
            "overflow",
            MonitorRequest::Register {
                source: MonitorSource::Sms,
                filter: None,
                action: MonitorAction {
                    report: true,
                    follow_up: None,
                },
                occurrence: MonitorOccurrence::Every,
                lifetime: MonitorLifetime::Session,
            },
        )
        .await;
    assert_eq!(overflow.status, ToolResultStatus::Rejected);
    assert!(overflow.preview.contains("limit_reached"));
}
