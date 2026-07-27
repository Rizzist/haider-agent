//! W3b1 acceptance matrix (brief deliverable 7; d1 report R19/R22 named
//! cases). Traceability, matrix case -> test:
//!
//! - simultaneous-start            -> `simultaneous_start_n_processes_has_one_winner_and_clean_losers`
//! - loser diagnostics (R1)        -> `already_running_error_carries_incumbent_diagnostics`
//! - stale-PID-reuse               -> `stale_pid_reuse_is_diagnostic_only_and_does_not_block_start`
//! - cold-start socket-missing     -> `cold_start_socket_missing_serves_handshake_ping_and_stub_with_private_modes`
//! - successor-socket-deletion     -> `successor_socket_deletion_guard_preserves_replacement_identity`
//! - failed-listener-startup       -> `failed_listener_startup_publishes_failed_and_releases_profile_lock`
//! - abrupt-death (kill -9)        -> `abrupt_death_kill_9_leaves_recoverable_socket_and_next_start_serves`
//! - version-mismatch rejection    -> `handshake_version_mismatch_returns_fatal_rejection`
//! - oversize frame                -> `oversize_frame_is_rejected_at_connection_layer_before_body_allocation`
//! - client frame-limit honored    -> `client_max_receive_frame_is_enforced_on_welcome`
//! - drain-notifies-connections    -> `drain_notifies_every_open_connection_before_close`
//! - second-signal termination     -> `second_signal_request_selects_immediate_forced_termination_path`
//! - second OS signal, end to end  -> `second_os_signal_terminates_the_daemon_through_the_forced_exit_path`
//! - shutdown before startup       -> `first_signal_before_startup_drains_without_advertising_ready`
//! - forced shutdown before startup -> `second_signal_before_startup_prevents_ready_and_forces_termination`
//! - reconcile-before-ready (R16)  -> `reconcile_before_ready_marks_unknown_exactly_once_and_never_retries_effect`
//!
//! Efficiency-rider follow-ups (report §2.5, R12/R17), same matrix discipline:
//!
//! - connection admission cap      -> `connection_admission_cap_rejects_over_limit_peers_and_readmits_a_freed_slot`
//! - queued-byte budget            -> `outbound_byte_budget_refuses_a_frame_the_connection_cannot_hold`
//! - reserved drain notice         -> `reserved_drain_notice_survives_an_exhausted_outbound_byte_budget`
//!
//! Review round 1 closures (R3/R17/R22):
//!
//! - blocked writer, deadline path -> `never_reading_client_is_cut_at_the_drain_deadline_and_releases_everything`
//! - blocked writer, forced path   -> `forced_shutdown_aborts_a_blocked_writer_instead_of_detaching_it`
//! - deadline covers finalization  -> `drain_deadline_covers_the_finalization_tail`
//! - replacement around cleanup    -> `endpoint_replacement_around_cleanup_is_never_deleted`
//! - live foreign endpoint         -> `live_foreign_endpoint_is_refused_and_left_intact`
//! - over-limit drain reason       -> `drain_reason_is_truncated_to_fit_a_small_client_frame_limit`
//! - capability downscoping        -> `view_only_connection_is_denied_the_control_frame`
//! - pre-Hello slot exhaustion     -> `silent_peer_is_closed_at_the_handshake_deadline_and_frees_its_slot`
//! - duplicate Hello               -> `duplicate_hello_after_handshake_is_a_fatal_unexpected_frame`
//!
//! The bind → identity window has no test because it has no window: the socket
//! is created under a private name, `statat`-ed there, and renamed into place,
//! so no replacement can be adopted as this daemon's own node.
//!
//! All cases use a real UDS in a tempdir runtime dir and poll readiness
//! states — no sleeps as synchronization. Where only the OS can answer (a
//! child's exit status, a socket appearing), the loop still polls a real
//! condition against a deadline; [`POLL_BACKOFF`] just keeps that poll from
//! spinning a core.

#![allow(clippy::expect_used)] // integration failures should name the exact lifecycle boundary

use haider_daemon::{DaemonConfig, DaemonError, DaemonState, ShutdownOutcome, spawn};
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectIntent, EffectOutcome, EffectPhase};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EffectId, EventId, MenuId, SessionId};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, CommandId, Hello, LifecyclePhase, ProtocolError,
    RequestBody, RequestId, ResponseBody, WIRE_PROTOCOL_VERSION, WireFrame, uds_codec,
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
/// Interval between polls of an OS-only condition (child exit, endpoint
/// appearing, a freed connection slot). The deadline is the synchronization;
/// this only stops the poll loop from burning a core on `yield_now`.
const POLL_BACKOFF: Duration = Duration::from_millis(5);

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

    /// Best-effort send for retry loops: a rejected connection may already be
    /// closed by the time the test writes.
    async fn try_send(&mut self, frame: &WireFrame, limit: usize) -> bool {
        let bytes = uds_codec::encode(frame, limit).expect("test frame encodes");
        self.stream.write_all(&bytes).await.is_ok()
    }

    async fn receive(&mut self) -> WireFrame {
        self.try_receive()
            .await
            .expect("connection closed before a frame arrived")
    }

    /// Next frame, or `None` when the daemon closed the connection first.
    async fn try_receive(&mut self) -> Option<WireFrame> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        loop {
            let mut bytes = [0_u8; 8 * 1024];
            let read = self.stream.read(&mut bytes).await.expect("frame reads");
            if read == 0 {
                return None;
            }
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "server sent an invalid frame");
            self.pending.extend(batch.frames);
            if let Some(frame) = self.pending.pop_front() {
                return Some(frame);
            }
        }
    }

    /// Reads at least `at_least` raw bytes into the decoder without waiting for
    /// a whole frame, leaving a large reply deliberately mid-write (its bytes
    /// still charged against the connection's queued-byte budget).
    async fn absorb_at_least(&mut self, at_least: usize) {
        let mut absorbed = 0;
        while absorbed < at_least {
            let mut bytes = [0_u8; 8 * 1024];
            let read = tokio::time::timeout(DEADLINE, self.stream.read(&mut bytes))
                .await
                .expect("partial read deadline")
                .expect("partial read");
            assert_ne!(read, 0, "connection closed before the reply started");
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "server sent an invalid frame");
            self.pending.extend(batch.frames);
            absorbed += read;
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

    /// Reads until EOF and reports how many COMPLETE frames the daemon managed
    /// to put on the wire. Used to prove a blocked writer was cut off rather
    /// than left running: a writer that survived its connection would keep
    /// feeding the reader until its whole pending frame arrived.
    async fn frames_until_eof(&mut self) -> usize {
        let mut frames = self.pending.len();
        loop {
            let mut bytes = [0_u8; 16 * 1024];
            let read = tokio::time::timeout(DEADLINE, self.stream.read(&mut bytes))
                .await
                .expect("EOF deadline")
                .expect("EOF read");
            if read == 0 {
                return frames;
            }
            frames += self.decoder.push(&bytes[..read]).frames.len();
        }
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
                tokio::time::sleep(POLL_BACKOFF).await;
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

/// The one control frame this lane accepts, optionally correlated so the
/// daemon can answer it with a `Response` (additive `MenuAnswer.request_id`).
fn menu_answer(request_id: Option<&str>) -> WireFrame {
    WireFrame::MenuAnswer {
        request_id: request_id.map(RequestId::new),
        command_id: CommandId::new("command-test"),
        session_id: SessionId::new("session-test"),
        menu_id: MenuId::new("menu-test"),
        request_seq: 1,
        worker_generation: 1,
        option_key: "approve".into(),
        option_index: 0,
        input: None,
    }
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
            tokio::time::sleep(POLL_BACKOFF).await;
        }
    })
    .await
    .expect("daemon readiness deadline")
}

/// Waits for the profile lock to be free, bounded by the usual deadline.
///
/// A drain that overruns its deadline stops WAITING on store work; the work
/// itself is a blocking SQLite call that cannot be cancelled and releases the
/// lock as soon as it returns. The law being pinned is that the lock is never
/// leaked — not that the OS hands it back on the same instruction.
async fn poll_store_release(config: &DaemonConfig) {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(store) = Store::open(&config.store_dir) {
                drop(store);
                return;
            }
            tokio::time::sleep(POLL_BACKOFF).await;
        }
    })
    .await
    .expect("profile lock release deadline");
}

/// Bounded-retry connect + handshake for the admission cap: a freed connection
/// slot becomes observable only once the previous connection's task has ended,
/// which no client-visible event announces.
async fn poll_admission(config: &DaemonConfig) -> TestClient {
    let announced = u32::try_from(config.frame_limit).expect("frame limit fits");
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(mut client) =
                TestClient::connect(&config.endpoint_path(), config.frame_limit).await
                && client
                    .try_send(
                        &hello(WIRE_PROTOCOL_VERSION, WIRE_PROTOCOL_VERSION, announced),
                        config.frame_limit,
                    )
                    .await
                && let Some(WireFrame::Welcome(_)) = client.try_receive().await
            {
                return client;
            }
            tokio::time::sleep(POLL_BACKOFF).await;
        }
    })
    .await
    .expect("connection readmission deadline")
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
            tokio::time::sleep(POLL_BACKOFF).await;
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
    // The daemon no longer binds onto the public name (it binds under a
    // private name and renames), so an unusable node there is refused by the
    // endpoint ownership guard — earlier than the old bind syscall failure,
    // and without ever creating a socket.
    assert!(
        matches!(&error, DaemonError::Endpoint { .. }),
        "failure must originate at the endpoint ownership guard, got {error:?}"
    );
    assert!(
        fs::symlink_metadata(config.endpoint_path()).is_ok(),
        "a squatting node must be refused, never removed"
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
async fn connection_admission_cap_rejects_over_limit_peers_and_readmits_a_freed_slot() {
    let root = test_root();
    let mut config = test_config(&root, "admission-cap");
    config.max_connections = 2;
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    // N = cap connections are served normally.
    let _first = handshake(&config.endpoint_path(), config.frame_limit).await;
    let second = handshake(&config.endpoint_path(), config.frame_limit).await;

    // The cap+1'th peer is answered typed, then closed — no handshake, no task.
    let mut refused = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect over the cap");
    assert!(
        matches!(
            refused.receive().await,
            WireFrame::ProtocolError(ProtocolError {
                ref code,
                fatal: true,
                ..
            }) if code == "overloaded"
        ),
        "over-limit peer must receive the fatal overloaded rejection"
    );
    refused.expect_eof().await;

    // A freed slot is readmitted; the cap bounds concurrency, not lifetime use.
    drop(second);
    let _readmitted = poll_admission(&config).await;

    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

#[tokio::test]
async fn outbound_byte_budget_refuses_a_frame_the_connection_cannot_hold() {
    let root = test_root();
    // The Welcome embeds the profile id, so a profile id larger than the whole
    // per-connection byte budget makes the first reply unqueueable — the byte
    // charge happens before the enqueue, so the connection dies there.
    let profile = format!("byte-budget-{}", "p".repeat(8 * 1024));
    let mut config = test_config(&root, &profile);
    config.outbound_queued_bytes = 4 * 1024;
    assert!(
        config.outbound_queued_bytes < config.profile_id.len(),
        "the reply this test expects to be refused must exceed the budget"
    );
    assert!(
        config.outbound_queue_capacity > 1,
        "the frame-count bound must not be what fires here"
    );
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
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
    assert!(
        tokio::time::timeout(DEADLINE, client.try_receive())
            .await
            .expect("budget rejection deadline")
            .is_none(),
        "an over-budget reply must close the connection, never stall the daemon"
    );

    // The daemon itself is unharmed: the bound is per connection.
    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
    assert!(!config.endpoint_path().exists());
}

#[tokio::test]
async fn reserved_drain_notice_survives_an_exhausted_outbound_byte_budget() {
    let root = test_root();
    // A half-megabyte profile id makes the Welcome larger than any socket
    // buffer, so it is still mid-write — and still charged — when the drain
    // fires. Budget = frame limit leaves less headroom than the (deliberately
    // long-reasoned) ServerDraining frame needs, so only a reserve outside the
    // ordinary budget can deliver it. W3b2's real responses are the traffic
    // this budget exists for; the Welcome is the one large daemon-authored
    // frame a W3b1 test can size.
    let profile = format!("drain-reserve-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    config.frame_limit = config.profile_id.len() + 1_024;
    config.outbound_queued_bytes = config.frame_limit;
    let reason = format!("maintenance-{}", "r".repeat(4 * 1024));
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
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
    // Enough bytes to prove the Welcome write started, far too few to finish it.
    client.absorb_at_least(8 * 1024).await;

    task.shutdown_handle().request(&reason);
    let welcome = client.receive().await;
    assert!(
        matches!(welcome, WireFrame::Welcome(_)),
        "the queued reply must still be delivered, got {welcome:?}"
    );
    let notice = client.receive().await;
    assert!(
        matches!(
            notice,
            WireFrame::ServerDraining {
                reason: ref notice_reason,
                deadline_unix_ms,
                ..
            } if *notice_reason == reason && deadline_unix_ms > 0
        ),
        "the reserved drain notice must arrive last, got {notice:?}"
    );
    client.expect_eof().await;

    // Pin the counterfactual: had the notice shared the ordinary budget with
    // the charged-but-unwritten Welcome, it could not have been queued at all.
    let welcome_bytes = uds_codec::encode(&welcome, config.frame_limit)
        .expect("re-encode welcome")
        .len();
    let notice_bytes = uds_codec::encode(&notice, config.frame_limit)
        .expect("re-encode notice")
        .len();
    assert!(
        welcome_bytes <= config.outbound_queued_bytes,
        "the queued reply itself must fit the budget ({welcome_bytes} bytes)"
    );
    assert!(
        welcome_bytes + notice_bytes > config.outbound_queued_bytes,
        "budget must be too small for reply + notice, else the reserve is untested \
         ({welcome_bytes} + {notice_bytes} bytes)"
    );

    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

/// A client that stops reading leaves a large reply mid-write. The barrier
/// deadline must cut that writer, not wait behind it: nothing may outlive the
/// deadline — not the writer task, not the socket, not the profile lock.
#[tokio::test]
async fn never_reading_client_is_cut_at_the_drain_deadline_and_releases_everything() {
    let root = test_root();
    let profile = format!("never-reads-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    config.frame_limit = config.profile_id.len() + 1_024;
    config.outbound_queued_bytes = config.frame_limit;
    config.drain_timeout = Duration::from_millis(250);
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
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
    // Enough to prove the reply write started; the client then goes silent, so
    // the daemon's writer is parked inside write_all from here on.
    client.absorb_at_least(8 * 1024).await;

    task.shutdown_handle().request("deadline");
    // The barrier must end by itself, and honestly: a connection that never
    // received its ServerDraining is not a graceful drain.
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Forced
    );
    assert!(
        !config.endpoint_path().exists(),
        "socket outlived the barrier"
    );
    poll_store_release(&config).await;

    let delivered = client.frames_until_eof().await;
    assert_eq!(
        delivered, 0,
        "the blocked writer was left running: it completed its frame after the barrier"
    );
}

/// The forced path aborts connection tasks outright. The writer must die with
/// its connection: joining it may not hand its handle away, or the abort finds
/// nothing to cancel and the writer (plus its socket and payload) survives
/// endpoint cleanup and the profile-lock release.
#[tokio::test]
async fn forced_shutdown_aborts_a_blocked_writer_instead_of_detaching_it() {
    let root = test_root();
    let profile = format!("forced-writer-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    config.frame_limit = config.profile_id.len() + 1_024;
    config.outbound_queued_bytes = config.frame_limit;
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
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
    client.absorb_at_least(8 * 1024).await;

    let shutdown = task.shutdown_handle();
    shutdown.request("drain");
    // Let the connection task reach its writer join: that await is exactly
    // where the abort must still find an abortable writer handle. The drain
    // timeout is the default 5s, so the writer is parked in write_all — its own
    // deadline is nowhere near — when the force arrives.
    wait_for_state(task.readiness(), |state| {
        matches!(state, DaemonState::Draining { .. })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown.request("force");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Forced
    );
    assert!(!config.endpoint_path().exists());
    poll_store_release(&config).await;

    let delivered = client.frames_until_eof().await;
    assert_eq!(
        delivered, 0,
        "the writer was detached rather than aborted: it completed its frame after the abort"
    );
}

/// The advertised deadline covers the finalization tail too (flush, socket
/// removal, store close), and an overrun is reported as forced.
#[tokio::test]
async fn drain_deadline_covers_the_finalization_tail() {
    let root = test_root();
    let mut config = test_config(&root, "finalization-deadline");
    // Expired before finalization can begin: any barrier step that finds the
    // deadline already gone must take the forced path rather than block on.
    config.drain_timeout = Duration::from_nanos(1);
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    task.shutdown_handle().request("expired deadline");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Forced,
        "an overrun finalization must never be reported as a graceful drain"
    );
    // Whatever the deadline did to the store work, the rendezvous node is gone
    // and the profile lock is released rather than leaked.
    assert!(!config.endpoint_path().exists());
    poll_store_release(&config).await;
}

/// R3/R22: a node that appears at the endpoint path around cleanup is never
/// this daemon's to delete, whichever side of the claim it lands on. The
/// bind → identity window is closed by construction (the socket is created
/// under a private name and renamed into place), so only this side needs a
/// racing test.
#[tokio::test]
async fn endpoint_replacement_around_cleanup_is_never_deleted() {
    let root = test_root();
    let config = test_config(&root, "cleanup-replacement");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let socket_path = config.endpoint_path();
    task.shutdown_handle().request("handover");
    // Racing the cleanup deliberately: if we win, the daemon claims a node
    // that is not its own and must restore it; if we lose, we create a node
    // the daemon has already stopped caring about. Both orderings must leave
    // this node in place.
    let _ = fs::remove_file(&socket_path);
    let replacement = StdUnixListener::bind(&socket_path).expect("bind replacement node");
    let replacement_metadata = fs::symlink_metadata(&socket_path).expect("replacement metadata");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
    let after = fs::symlink_metadata(&socket_path).expect("replacement node survives cleanup");
    assert_eq!(
        (after.dev(), after.ino()),
        (replacement_metadata.dev(), replacement_metadata.ino())
    );
    drop(replacement);
    fs::remove_file(socket_path).expect("remove replacement");
}

/// The conservative half of R3: a LIVE endpoint owned by someone else is
/// refused, never unlinked — even though the profile lock was free.
#[tokio::test]
async fn live_foreign_endpoint_is_refused_and_left_intact() {
    let root = test_root();
    let config = test_config(&root, "live-foreign");
    fs::create_dir_all(&config.runtime_dir).expect("runtime directory");
    let foreign = StdUnixListener::bind(config.endpoint_path()).expect("foreign listener");
    let before = fs::symlink_metadata(config.endpoint_path()).expect("foreign metadata");

    let task = spawn(config.clone());
    let error = task
        .join()
        .await
        .expect_err("a live endpoint must refuse startup");
    assert!(
        matches!(&error, DaemonError::Endpoint { .. }),
        "expected an endpoint ownership refusal, got {error:?}"
    );
    let after = fs::symlink_metadata(config.endpoint_path()).expect("foreign node survives");
    assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
    drop(foreign);

    let store = Store::open(&config.store_dir).expect("refused startup released the profile lock");
    drop(store);
}

/// A client whose negotiated frame limit cannot carry the operator's prose
/// still gets its last frame: the reason is truncated, the notice is not.
#[tokio::test]
async fn drain_reason_is_truncated_to_fit_a_small_client_frame_limit() {
    let root = test_root();
    let config = test_config(&root, "small-limit");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let announced = 512;
    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
    client
        .send(
            &hello(WIRE_PROTOCOL_VERSION, WIRE_PROTOCOL_VERSION, announced),
            config.frame_limit,
        )
        .await;
    assert!(matches!(client.receive().await, WireFrame::Welcome(_)));

    let reason = format!("maintenance-{}", "r".repeat(4 * 1024));
    task.shutdown_handle().request(&reason);
    let notice = client.receive().await;
    let WireFrame::ServerDraining {
        reason: notice_reason,
        ..
    } = &notice
    else {
        panic!("expected ServerDraining, got {notice:?}");
    };
    assert!(
        reason.starts_with(notice_reason.as_str()) && !notice_reason.is_empty(),
        "the delivered reason must be a prefix of the operator's reason"
    );
    let encoded = uds_codec::encode(&notice, usize::MAX).expect("re-encode notice");
    assert!(
        encoded.len() <= announced as usize,
        "the notice must respect what the client said it can receive"
    );
    client.expect_eof().await;
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

/// The negotiated grant is retained and enforced: `MenuAnswer` is a control
/// frame, so a view-only connection is denied it — and a correlated answer
/// gets a correlated reply (the additive `MenuAnswer.request_id`).
#[tokio::test]
async fn view_only_connection_is_denied_the_control_frame() {
    let root = test_root();
    let config = test_config(&root, "capabilities");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut viewer = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect viewer");
    let mut request = hello(
        WIRE_PROTOCOL_VERSION,
        WIRE_PROTOCOL_VERSION,
        u32::try_from(config.frame_limit).expect("frame limit fits"),
    );
    if let WireFrame::Hello(hello) = &mut request {
        hello.capabilities_requested = CapabilitySet::from([Capability::View]);
    }
    viewer.send(&request, config.frame_limit).await;
    match viewer.receive().await {
        WireFrame::Welcome(welcome) => assert_eq!(
            welcome.capabilities_granted,
            CapabilitySet::from([Capability::View]),
            "the daemon must never grant a capability the client did not ask for"
        ),
        frame => panic!("expected Welcome, got {frame:?}"),
    }
    viewer
        .send(&menu_answer(Some("menu-request")), config.frame_limit)
        .await;
    assert!(
        matches!(
            viewer.receive().await,
            WireFrame::Response {
                ref request_id,
                body: ResponseBody::Error { ref code, .. },
            } if request_id.as_str() == "menu-request" && code == "capability_denied"
        ),
        "a view-only connection must be denied the control frame, correlated to its request"
    );

    // A control connection gets the W3b2 stub instead, and an uncorrelated
    // answer still gets the connection-level form.
    let mut controller = handshake(&config.endpoint_path(), config.frame_limit).await;
    controller
        .send(&menu_answer(None), config.frame_limit)
        .await;
    assert!(matches!(
        controller.receive().await,
        WireFrame::ProtocolError(ProtocolError { ref code, fatal: false, .. })
            if code == "not_found"
    ));

    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

/// A peer that says nothing must not hold a connection slot: it is closed at
/// the handshake deadline and its slot is immediately reusable.
#[tokio::test]
async fn silent_peer_is_closed_at_the_handshake_deadline_and_frees_its_slot() {
    let root = test_root();
    let mut config = test_config(&root, "silent-peer");
    config.max_connections = 1;
    config.handshake_timeout = Duration::from_millis(150);
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut silent = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect silent peer");
    assert!(
        matches!(
            silent.receive().await,
            WireFrame::ProtocolError(ProtocolError { ref code, fatal: true, .. })
                if code == "handshake_timeout"
        ),
        "a silent peer must be told why it is being closed"
    );
    silent.expect_eof().await;

    // The single slot is free again for a peer that does speak.
    let _speaker = poll_admission(&config).await;
    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

/// A second `Hello` on an established connection is not a handshake — it is a
/// frame no connected client may send.
#[tokio::test]
async fn duplicate_hello_after_handshake_is_a_fatal_unexpected_frame() {
    let root = test_root();
    let config = test_config(&root, "duplicate-hello");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = handshake(&config.endpoint_path(), config.frame_limit).await;
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
    assert!(matches!(
        client.receive().await,
        WireFrame::ProtocolError(ProtocolError { ref code, fatal: true, .. })
            if code == "unexpected_frame"
    ));
    client.expect_eof().await;

    task.shutdown_handle().request("test complete");
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
