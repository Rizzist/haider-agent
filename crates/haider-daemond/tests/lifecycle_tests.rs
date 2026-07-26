#![allow(clippy::expect_used)] // integration failures should name the exact lifecycle boundary

use haider_daemon::{DaemonConfig, DaemonError, DaemonState, ShutdownOutcome, spawn};
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectIntent, EffectOutcome, EffectPhase};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EffectId, EventId, SessionId};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, Hello, LifecyclePhase, ProtocolError, RequestBody,
    RequestId, ResponseBody, WIRE_PROTOCOL_VERSION, WireFrame, uds_codec,
};
use haider_store::{EventStore, Store};
use rustix::process::{Pid, Signal, kill_process};
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Barrier};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const DEADLINE: Duration = Duration::from_secs(10);

fn test_root() -> tempfile::TempDir {
    #[cfg(target_os = "macos")]
    const SHORT_TMP_ROOT: &str = "/private/tmp";
    #[cfg(not(target_os = "macos"))]
    const SHORT_TMP_ROOT: &str = "/tmp";

    tempfile::Builder::new()
        .prefix("w3b1-")
        .tempdir_in(SHORT_TMP_ROOT)
        .expect("short temporary root")
}

struct TestClient {
    stream: UnixStream,
    decoder: uds_codec::Decoder,
    pending: VecDeque<WireFrame>,
}

impl TestClient {
    async fn connect(path: &Path, frame_limit: usize) -> std::io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path).await?,
            decoder: uds_codec::Decoder::new(frame_limit),
            pending: VecDeque::new(),
        })
    }

    async fn send(&mut self, frame: &WireFrame, limit: usize) {
        let bytes = uds_codec::encode(frame, limit).expect("test frame encodes");
        self.stream.write_all(&bytes).await.expect("frame writes");
    }

    async fn receive(&mut self) -> WireFrame {
        if let Some(frame) = self.pending.pop_front() {
            return frame;
        }
        loop {
            let mut bytes = [0_u8; 8 * 1024];
            let read = self.stream.read(&mut bytes).await.expect("frame reads");
            assert_ne!(read, 0, "connection closed before a frame arrived");
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "server sent an invalid frame");
            self.pending.extend(batch.frames);
            if let Some(frame) = self.pending.pop_front() {
                return frame;
            }
        }
    }

    async fn expect_eof(&mut self) {
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(DEADLINE, self.stream.read(&mut byte))
            .await
            .expect("EOF deadline")
            .expect("EOF read");
        assert_eq!(read, 0);
    }
}

struct ManagedChild {
    child: Child,
    finished: bool,
}

impl ManagedChild {
    fn new(child: Child) -> Self {
        Self {
            child,
            finished: false,
        }
    }

    fn signal(&self, signal: Signal) {
        kill_process(Pid::from_child(&self.child), signal).expect("send child signal");
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        let status = self.child.try_wait().expect("poll child");
        if status.is_some() {
            self.finished = true;
        }
        status
    }

    async fn wait(&mut self) -> ExitStatus {
        let status = tokio::time::timeout(DEADLINE, async {
            loop {
                if let Some(status) = self.try_wait() {
                    return status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child exit deadline");
        self.finished = true;
        status
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn test_config(root: &tempfile::TempDir, profile: &str) -> DaemonConfig {
    DaemonConfig::new(
        profile,
        root.path().join("store"),
        root.path().join("runtime"),
    )
}

fn child_command(config: &DaemonConfig) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_haiderd"));
    command
        .arg("--profile")
        .arg(&config.profile_id)
        .arg("--store-dir")
        .arg(&config.store_dir)
        .arg("--runtime-dir")
        .arg(&config.runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn hello(protocol_min: u32, protocol_max: u32, max_receive_frame: u32) -> WireFrame {
    WireFrame::Hello(Hello {
        protocol_min,
        protocol_max,
        client_name: "daemon-test".into(),
        client_version: "test".into(),
        client_instance_id: "test-client".into(),
        client_kind: ClientKind::Headless,
        capabilities_requested: CapabilitySet::from([Capability::View, Capability::Control]),
        max_receive_frame,
    })
}

async fn handshake(path: &Path, frame_limit: usize) -> TestClient {
    let mut client = TestClient::connect(path, frame_limit)
        .await
        .expect("connect daemon");
    client
        .send(
            &hello(
                WIRE_PROTOCOL_VERSION,
                WIRE_PROTOCOL_VERSION,
                u32::try_from(frame_limit).expect("test frame limit fits"),
            ),
            frame_limit,
        )
        .await;
    match tokio::time::timeout(DEADLINE, client.receive())
        .await
        .expect("handshake deadline")
    {
        WireFrame::Welcome(welcome) => {
            assert_eq!(welcome.lifecycle_phase, LifecyclePhase::Ready);
        }
        frame => panic!("expected Welcome, got {frame:?}"),
    }
    client
}

async fn poll_process_ready(config: &DaemonConfig) -> TestClient {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(mut client) =
                TestClient::connect(&config.endpoint_path(), config.frame_limit).await
            {
                client
                    .send(
                        &hello(
                            WIRE_PROTOCOL_VERSION,
                            WIRE_PROTOCOL_VERSION,
                            u32::try_from(config.frame_limit).expect("frame limit fits"),
                        ),
                        config.frame_limit,
                    )
                    .await;
                if let Ok(WireFrame::Welcome(welcome)) =
                    tokio::time::timeout(Duration::from_millis(250), client.receive()).await
                    && welcome.lifecycle_phase == LifecyclePhase::Ready
                {
                    return client;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("daemon readiness deadline")
}

async fn wait_for_state(
    mut readiness: haider_daemon::Readiness,
    predicate: impl Fn(&DaemonState) -> bool,
) -> DaemonState {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let current = readiness.current();
            if predicate(&current) {
                return current;
            }
            if readiness.changed().await.is_none() {
                panic!(
                    "readiness closed in terminal state {:?}",
                    readiness.current()
                );
            }
        }
    })
    .await
    .expect("readiness state deadline")
}

fn raw_event(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    payload: EventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("seed-device"),
        authority_epoch: 9,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Pruned,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

fn unknown_outcomes(events: &[RawEnvelope], effect: &EffectId) -> usize {
    events
        .iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).ok())
        .filter(|payload| {
            matches!(
                payload,
                EventPayload::Effect(EffectPhase::Outcome {
                    effect: found,
                    outcome: EffectOutcome::Unknown,
                }) if found == effect
            )
        })
        .count()
}

#[tokio::test]
async fn cold_start_socket_missing_serves_handshake_ping_and_stub_with_private_modes() {
    let root = test_root();
    let config = test_config(&root, "cold-start");
    assert!(!config.endpoint_path().exists());
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    assert_eq!(
        fs::metadata(&config.runtime_dir)
            .expect("runtime metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(config.endpoint_path())
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut client = handshake(&config.endpoint_path(), config.frame_limit).await;
    client
        .send(&WireFrame::Ping { nonce: 77 }, config.frame_limit)
        .await;
    assert_eq!(client.receive().await, WireFrame::Pong { nonce: 77 });
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("stub"),
                body: RequestBody::SessionList {
                    cursor: None,
                    limit: 10,
                },
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        client.receive().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == "not_found"
    ));

    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
    assert!(!config.endpoint_path().exists());
}

#[tokio::test]
async fn stale_pid_reuse_is_diagnostic_only_and_does_not_block_start() {
    let root = test_root();
    let config = test_config(&root, "stale-pid");
    fs::create_dir_all(&config.store_dir).expect("store root");
    fs::write(
        config.store_dir.join("lock"),
        format!("pid={}\ncreated_at_ms=1\n", std::process::id()),
    )
    .expect("stale diagnostics");

    let task = spawn(config);
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;
    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

#[tokio::test]
async fn already_running_error_carries_incumbent_diagnostics() {
    let root = test_root();
    let config = test_config(&root, "diagnostics");
    let incumbent = spawn(config.clone());
    wait_for_state(incumbent.readiness(), |state| *state == DaemonState::Ready).await;

    let loser = spawn(config);
    match loser.join().await {
        Err(DaemonError::AlreadyRunning { diagnostics }) => {
            assert_eq!(diagnostics.profile_id, "diagnostics");
            assert!(
                diagnostics
                    .lock_contents
                    .as_deref()
                    .is_some_and(|contents| contents.contains("pid="))
            );
        }
        result => panic!("expected typed AlreadyRunning, got {result:?}"),
    }
    incumbent.shutdown_handle().request("test complete");
    assert_eq!(
        incumbent.join().await.expect("incumbent joins"),
        ShutdownOutcome::Graceful
    );
}

#[tokio::test]
async fn simultaneous_start_n_processes_has_one_winner_and_clean_losers() {
    let root = test_root();
    let config = test_config(&root, "simultaneous");
    let starters = 6;
    let barrier = Arc::new(Barrier::new(starters));
    let mut threads = Vec::new();
    for _ in 0..starters {
        let barrier = Arc::clone(&barrier);
        let child_config = config.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            child_command(&child_config).spawn().expect("spawn daemon")
        }));
    }
    let mut children = threads
        .into_iter()
        .map(|thread| ManagedChild::new(thread.join().expect("starter thread")))
        .collect::<Vec<_>>();
    let _client = poll_process_ready(&config).await;

    let statuses = tokio::time::timeout(DEADLINE, async {
        loop {
            let statuses = children
                .iter_mut()
                .map(ManagedChild::try_wait)
                .collect::<Vec<_>>();
            if statuses.iter().filter(|status| status.is_some()).count() == starters - 1 {
                return statuses;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("loser exit deadline");
    let alive = statuses
        .iter()
        .enumerate()
        .filter_map(|(index, status)| status.is_none().then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(alive.len(), 1);
    for status in statuses.into_iter().flatten() {
        assert_eq!(status.code(), Some(75));
    }
    children[alive[0]].signal(Signal::TERM);
    assert!(children[alive[0]].wait().await.success());
}

#[tokio::test]
async fn successor_socket_deletion_guard_preserves_replacement_identity() {
    let root = test_root();
    let config = test_config(&root, "successor");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let socket_path = config.endpoint_path();
    fs::remove_file(&socket_path).expect("unlink old rendezvous node");
    let successor = StdUnixListener::bind(&socket_path).expect("bind successor node");
    let successor_metadata = fs::symlink_metadata(&socket_path).expect("successor metadata");
    task.shutdown_handle().request("controlled handover");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
    let after = fs::symlink_metadata(&socket_path).expect("successor node remains");
    assert_eq!(
        (after.dev(), after.ino()),
        (successor_metadata.dev(), successor_metadata.ino())
    );
    drop(successor);
    fs::remove_file(socket_path).expect("remove successor");
}

#[tokio::test]
async fn failed_listener_startup_publishes_failed_and_releases_profile_lock() {
    let root = test_root();
    let config = test_config(&root, "bind-failure");
    fs::create_dir_all(&config.runtime_dir).expect("runtime directory");
    symlink(
        root.path().join("missing-parent/socket"),
        config.endpoint_path(),
    )
    .expect("dangling endpoint symlink");
    let task = spawn(config.clone());
    let failed = wait_for_state(task.readiness(), |state| {
        matches!(state, DaemonState::Failed { .. })
    })
    .await;
    assert!(matches!(failed, DaemonState::Failed { .. }));
    let error = task.join().await.expect_err("listener bind must fail");
    assert!(
        matches!(
            &error,
            DaemonError::Io {
                operation: "bind Unix socket",
                ..
            }
        ),
        "failure must originate at the listener bind syscall, got {error:?}"
    );

    let store = Store::open(&config.store_dir).expect("failed daemon released profile lock");
    drop(store);
}

#[tokio::test]
async fn abrupt_death_kill_9_leaves_recoverable_socket_and_next_start_serves() {
    let root = test_root();
    let config = test_config(&root, "abrupt");
    let mut first = ManagedChild::new(child_command(&config).spawn().expect("first daemon"));
    let _client = poll_process_ready(&config).await;
    first.signal(Signal::KILL);
    assert!(!first.wait().await.success());
    assert!(config.endpoint_path().exists());

    let mut recovered =
        ManagedChild::new(child_command(&config).spawn().expect("recovered daemon"));
    let mut client = poll_process_ready(&config).await;
    client
        .send(&WireFrame::Ping { nonce: 9 }, config.frame_limit)
        .await;
    assert_eq!(client.receive().await, WireFrame::Pong { nonce: 9 });
    recovered.signal(Signal::TERM);
    assert!(recovered.wait().await.success());
}

#[tokio::test]
async fn handshake_version_mismatch_returns_fatal_rejection() {
    let root = test_root();
    let config = test_config(&root, "version-mismatch");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
    client
        .send(
            &hello(
                WIRE_PROTOCOL_VERSION + 1,
                WIRE_PROTOCOL_VERSION + 1,
                u32::try_from(config.frame_limit).expect("limit fits"),
            ),
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        client.receive().await,
        WireFrame::ProtocolError(ProtocolError {
            ref code,
            fatal: true,
            ..
        }) if code == "protocol_version_mismatch"
    ));
    client.expect_eof().await;
    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

#[tokio::test]
async fn oversize_frame_is_rejected_at_connection_layer_before_body_allocation() {
    let root = test_root();
    let mut config = test_config(&root, "oversize");
    config.frame_limit = 1_024;
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
    let announced = u32::try_from(config.frame_limit + 1).expect("limit fits");
    client
        .stream
        .write_all(&announced.to_be_bytes())
        .await
        .expect("write oversized prefix");
    assert!(matches!(
        client.receive().await,
        WireFrame::ProtocolError(ProtocolError { fatal: true, .. })
    ));
    client.expect_eof().await;
    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

#[tokio::test]
async fn client_max_receive_frame_is_enforced_on_welcome() {
    let root = test_root();
    let config = test_config(&root, "client-frame-limit");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
    client
        .send(
            &hello(WIRE_PROTOCOL_VERSION, WIRE_PROTOCOL_VERSION, 1),
            config.frame_limit,
        )
        .await;
    client.expect_eof().await;
    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

#[tokio::test]
async fn drain_notifies_every_open_connection_before_close() {
    let root = test_root();
    let config = test_config(&root, "drain-notify");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;
    let mut first = handshake(&config.endpoint_path(), config.frame_limit).await;
    let mut second = handshake(&config.endpoint_path(), config.frame_limit).await;

    task.shutdown_handle().request("maintenance");
    for client in [&mut first, &mut second] {
        assert!(matches!(
            client.receive().await,
            WireFrame::ServerDraining {
                ref reason,
                deadline_unix_ms,
                ..
            } if reason == "maintenance" && deadline_unix_ms > 0
        ));
        client.expect_eof().await;
    }
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

#[tokio::test]
async fn second_signal_request_selects_immediate_forced_termination_path() {
    let root = test_root();
    let config = test_config(&root, "forced");
    let task = spawn(config);
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;
    let shutdown = task.shutdown_handle();
    shutdown.request("SIGTERM");
    shutdown.request("SIGTERM");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Forced
    );
}

#[tokio::test]
async fn first_signal_before_startup_drains_without_advertising_ready() {
    let root = test_root();
    let config = test_config(&root, "drain-before-startup");
    let task = spawn(config.clone());
    assert_eq!(
        task.shutdown_handle().request("startup cancelled"),
        haider_daemon::ShutdownDisposition::DrainStarted
    );
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
    assert!(!config.endpoint_path().exists());
    let store = Store::open(&config.store_dir).expect("startup drain released profile lock");
    assert_eq!(
        store.worker_generation(),
        1,
        "queued shutdown must not consume a worker generation"
    );
    drop(store);
}

#[tokio::test]
async fn second_signal_before_startup_prevents_ready_and_forces_termination() {
    let root = test_root();
    let config = test_config(&root, "forced-before-startup");
    let task = spawn(config.clone());
    let shutdown = task.shutdown_handle();
    assert_eq!(
        shutdown.request("first startup signal"),
        haider_daemon::ShutdownDisposition::DrainStarted
    );
    assert_eq!(
        shutdown.request("second startup signal"),
        haider_daemon::ShutdownDisposition::Forced
    );
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Forced
    );
    assert!(!config.endpoint_path().exists());
    let store = Store::open(&config.store_dir).expect("forced startup released profile lock");
    assert_eq!(
        store.worker_generation(),
        1,
        "queued force must not consume a worker generation"
    );
    drop(store);
}

#[tokio::test]
async fn second_os_signal_terminates_the_daemon_through_the_forced_exit_path() {
    let root = test_root();
    let config = test_config(&root, "forced-signals");
    let mut child = ManagedChild::new(child_command(&config).spawn().expect("daemon process"));
    let _client = poll_process_ready(&config).await;
    child.signal(Signal::TERM);
    child.signal(Signal::INT);
    assert_eq!(child.wait().await.code(), Some(130));
}

#[tokio::test]
async fn reconcile_before_ready_marks_unknown_exactly_once_and_never_retries_effect() {
    let root = test_root();
    let config = test_config(&root, "reconcile");
    let session = SessionId::new("session-recovery");
    let pending = EffectId::new("effect-pending");
    let completed = EffectId::new("effect-completed");
    {
        let store = Store::open(&config.store_dir).expect("seed store");
        let generation = store.worker_generation();
        let intent = |effect: EffectId| EffectIntent {
            effect,
            class: EffectClass::Network {
                host: "example.invalid".into(),
            },
            summary: "non-idempotent request".into(),
            args_digest: "blake3:seed".into(),
            workspace_revision: None,
        };
        let mut events = vec![
            raw_event(
                &session,
                "seed-intent-pending",
                generation,
                EventPayload::Effect(EffectPhase::Intent(intent(pending.clone()))),
            ),
            raw_event(
                &session,
                "seed-dispatch-pending",
                generation,
                EventPayload::Effect(EffectPhase::Dispatched {
                    effect: pending.clone(),
                }),
            ),
            raw_event(
                &session,
                "seed-intent-complete",
                generation,
                EventPayload::Effect(EffectPhase::Intent(intent(completed.clone()))),
            ),
            raw_event(
                &session,
                "seed-dispatch-complete",
                generation,
                EventPayload::Effect(EffectPhase::Dispatched {
                    effect: completed.clone(),
                }),
            ),
            raw_event(
                &session,
                "seed-outcome-complete",
                generation,
                EventPayload::Effect(EffectPhase::Outcome {
                    effect: completed.clone(),
                    outcome: EffectOutcome::Ok,
                }),
            ),
        ];
        store.append(&mut events).expect("seed crash window");
    }

    for _ in 0..2 {
        let task = spawn(config.clone());
        wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;
        let _client = handshake(&config.endpoint_path(), config.frame_limit).await;
        task.shutdown_handle().request("restart test");
        assert_eq!(
            task.join().await.expect("daemon joins"),
            ShutdownOutcome::Graceful
        );
    }

    let store = Store::open(&config.store_dir).expect("inspect recovered store");
    let events = store
        .journal_replay(&session)
        .expect("replay recovered session");
    assert_eq!(unknown_outcomes(&events, &pending), 1);
    assert_eq!(unknown_outcomes(&events, &completed), 0);
    let pending_nonterminal_phases = events
        .iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).ok())
        .filter(|payload| match payload {
            EventPayload::Effect(EffectPhase::Intent(intent)) => {
                intent.effect.as_str() == pending.as_str()
            }
            EventPayload::Effect(EffectPhase::Dispatched { effect }) => {
                effect.as_str() == pending.as_str()
            }
            _ => false,
        })
        .count();
    assert_eq!(
        pending_nonterminal_phases, 2,
        "startup appended only the unknown outcome and never retried the effect"
    );
    assert_ne!(events.last().expect("recovery event").worker_generation, 0);
}
