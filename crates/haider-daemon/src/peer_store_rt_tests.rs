#![allow(clippy::expect_used)]
//! Isolated store-dispatch counter: run this exact test alone with --nocapture.
//! The same fixture runs against the pre-lane tree without peer wire changes.

use super::*;
use std::sync::atomic::AtomicUsize;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

struct StoreRoundTripCounter(Arc<AtomicUsize>);

#[derive(Default)]
struct StoreEventMessage(bool);

impl Visit for StoreEventMessage {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}").contains("store blocking operation completed");
        }
    }
}

impl Subscriber for StoreRoundTripCounter {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "haider.store"
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}

    fn event(&self, event: &Event<'_>) {
        if event.metadata().target() != "haider.store" {
            return;
        }
        let mut message = StoreEventMessage::default();
        event.record(&mut message);
        if message.0 {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn peer_store_round_trip_baseline_probe() {
    // A workspace test run may have unrelated store callers on other threads.
    // Isolate only this counter fixture so its measured RTs remain attributable
    // to peers without changing production instrumentation or test scheduling.
    if std::env::var_os("HAIDER_PEER_STORE_RT_PROBE").is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["peer_store_round_trip_baseline_probe", "--nocapture"])
            .env("HAIDER_PEER_STORE_RT_PROBE", "1")
            .output()
            .expect("isolated store-counter process");
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    // Global installation deliberately includes spawn_blocking's worker
    // threads. A thread-local subscriber silently misses every store RT.
    let count = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(StoreRoundTripCounter(Arc::clone(&count)))
        .expect("run peer_store_round_trip_baseline_probe in an isolated test process");
    let root = tempfile::tempdir().expect("peer count profile");
    let store = SqliteStoreHandle::open(root.path().join("store"))
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    const AGENTS: usize = 3;
    for index in 0..AGENTS {
        let session = SessionId::new(format!("store-count-{index}"));
        hub.create_internal_session(SessionCreateCommand {
            command_id: format!("create-{index}"),
            request_digest: format!("create-digest-{index}"),
            request_json: "{}".into(),
            session_id: session,
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 1_024,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "peer-store-counter".into(),
            event_id: EventId::new(format!("created-{index}")),
            device_id: DeviceId::new("peer-store-counter-device"),
        })
        .await
        .expect("resident peer");
    }
    let runtime = root.path().join("runtime");
    let _runtime = haider_platform::prepare_runtime_directory(&runtime).expect("private runtime");
    let service = crate::peer::PeerService::start(runtime, &hub)
        .await
        .expect("peer service");
    tokio::task::yield_now().await;
    let before = count.load(Ordering::Relaxed);
    tokio::time::advance(std::time::Duration::from_millis(1_200)).await;
    tokio::task::yield_now().await;
    let quiet = count.load(Ordering::Relaxed) - before;
    let before = count.load(Ordering::Relaxed);
    service.list().await.expect("explicit roster refresh");
    let explicit_list = count.load(Ordering::Relaxed) - before;
    eprintln!(
        "PEER_STORE_RT agents={AGENTS} quiet_ms=1200 quiet_rts={quiet} explicit_list_rts={explicit_list}"
    );
    assert_eq!(
        quiet, 0,
        "quiet peers must not schedule store round trips at 500 ms"
    );
    assert!(
        explicit_list > 0,
        "counter must observe real store work on a deliberate refresh"
    );
    service.shutdown().await;
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}
