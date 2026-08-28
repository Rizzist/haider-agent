#![cfg(unix)]
//! W3b1 acceptance matrix — EXHAUSTIVE. Every `#[tokio::test]` in this file
//! appears below; a case added without a row here is a gap in the matrix, not
//! a shortcut. Cases are grouped by the law they defend (d1 report
//! R1/R2/R3/R12/R16/R17/R19/R22, report §2.5).
//!
//! Singleton and startup (R1/R16/R19/R22):
//!
//! - simultaneous-start            -> `simultaneous_start_n_processes_has_one_winner_and_clean_losers`
//! - loser diagnostics             -> `already_running_error_carries_incumbent_diagnostics`
//! - stale-PID-reuse               -> `stale_pid_reuse_is_diagnostic_only_and_does_not_block_start`
//! - cold-start socket-missing     -> `cold_start_socket_missing_serves_handshake_ping_and_session_list`
//! - failed-listener startup       -> `failed_listener_startup_publishes_failed_and_releases_profile_lock`
//! - abrupt-death (kill -9)        -> `abrupt_death_kill_9_leaves_recoverable_socket_and_next_start_serves`
//! - reconcile-before-ready        -> `reconcile_before_ready_marks_unknown_exactly_once_and_never_retries_effect`
//!
//! Endpoint ownership (R2/R3/R22):
//!
//! - successor-socket-deletion     -> `successor_socket_deletion_guard_preserves_replacement_identity`
//! - replacement before cleanup    -> `endpoint_replacement_before_cleanup_is_never_deleted`
//! - replacement RACING cleanup    -> `endpoint_replacement_racing_cleanup_is_never_deleted`
//! - node that goes live in the
//!   probe → removal window        -> `stale_cleanup_never_removes_a_node_that_went_live`
//! - staging leftovers swept,
//!   live staging left alone       -> `stranded_staging_nodes_are_swept_but_live_ones_are_left`
//! - live foreign endpoint         -> `live_foreign_endpoint_is_refused_and_left_intact`
//!
//! Connection layer and its bounds (R12, report §2.5):
//!
//! - version-mismatch rejection    -> `handshake_version_mismatch_returns_fatal_rejection`
//! - oversize frame                -> `oversize_frame_is_rejected_at_connection_layer_before_body_allocation`
//! - client frame-limit honored    -> `client_max_receive_frame_is_enforced_on_welcome`
//! - duplicate Hello               -> `duplicate_hello_after_handshake_is_a_fatal_unexpected_frame`
//! - capability downscoping        -> `view_only_connection_is_denied_the_control_frame`
//! - binding capability rule       -> `binding_baseline_requires_view_or_control_capability`
//! - ordered unbound baseline      -> `view_connection_receives_exactly_one_unbound_baseline_after_welcome`
//! - pre-Hello slot exhaustion     -> `silent_peer_is_closed_at_the_handshake_deadline_and_frees_its_slot`
//! - connection admission cap      -> `connection_admission_cap_rejects_over_limit_peers_and_readmits_a_freed_slot`
//! - queued-byte budget            -> `outbound_byte_budget_refuses_a_frame_the_connection_cannot_hold`
//!
//! Drain barrier (R17):
//!
//! - drain-notifies-connections    -> `drain_notifies_every_open_connection_before_close`
//! - reserved drain notice         -> `reserved_drain_notice_survives_an_exhausted_outbound_byte_budget`
//! - over-limit drain reason       -> `drain_reason_is_truncated_to_fit_a_small_client_frame_limit`
//! - blocked writer, deadline path -> `never_reading_client_is_cut_at_the_drain_deadline_and_releases_everything`
//! - peer that reads NOTHING       -> `client_that_never_reads_a_byte_cannot_hold_the_barrier_open`
//! - peer that reads ONE byte      -> `one_byte_reader_cannot_hold_the_barrier_open`
//! - blocked writer, forced path   -> `forced_shutdown_aborts_a_blocked_writer_instead_of_detaching_it`
//! - accepts racing the request     -> `connections_racing_the_shutdown_request_are_torn_down_completely`
//! - deadline covers finalization  -> `drain_deadline_covers_the_finalization_tail`
//! - second-signal termination     -> `second_signal_request_selects_immediate_forced_termination_path`
//! - second OS signal, end to end  -> `second_os_signal_terminates_the_daemon_through_the_forced_exit_path`
//! - shutdown before startup       -> `first_signal_before_startup_drains_without_advertising_ready`
//! - forced shutdown before startup -> `second_signal_before_startup_prevents_ready_and_forces_termination`
//!
//! Covered by crate-internal unit tests instead (they need types this
//! integration crate cannot see): the phase publisher's refusal of illegal
//! edges (`haider-daemon/src/lifecycle_tests.rs`) and the barrier's arbitration
//! helpers, including second-signal-during-finalization
//! (`haider-daemon/src/runtime_tests.rs`).
//!
//! Two windows have no test, by construction rather than by omission: the
//! bind → identity window (the socket is created under an unpredictable name and
//! renamed into place, so no replacement can be adopted), and abort-vs-join of
//! a child writer (indistinguishable once the daemon has reported). Both are
//! recorded in docs/OPTIMIZATIONS.md.
//!
//! Tests that were mutation-checked carry a `MUTATION CHECK:` comment naming
//! the change to revert and the failure to expect, so the evidence is
//! re-executable rather than a claim in a commit message.
//!
//! All cases use a real UDS in a tempdir runtime dir and poll readiness
//! states — no sleeps as synchronization. Where only the OS can answer (a
//! child's exit status, a socket appearing), the loop still polls a real
//! condition against a deadline; [`POLL_BACKOFF`] just keeps that poll from
//! spinning a core.

#![allow(clippy::expect_used)] // integration failures should name the exact lifecycle boundary

mod support;

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
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Barrier};
use std::time::Duration;
use support::{DEADLINE, UdsClient as TestClient, test_root};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Interval between polls of an OS-only condition (child exit, endpoint
/// appearing, a freed connection slot). The deadline is the synchronization;
/// this only stops the poll loop from burning a core on `yield_now`.
const POLL_BACKOFF: Duration = Duration::from_millis(5);

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
    let mut config = DaemonConfig::new(
        profile,
        root.path().join("store"),
        root.path().join("runtime"),
    );
    // Hermetic: direct spawns here bypass support::ready's guard; never
    // probe the developer machine's real credential stores (A2 auto-adopt).
    config.discovery_disabled = true;
    config
}

fn assert_runtime_directory_residual(
    outcome: Result<ShutdownOutcome, DaemonError>,
    runtime_dir: &Path,
    mut expected: Vec<PathBuf>,
) {
    let DaemonError::RuntimeDirectoryNotEmpty {
        path,
        mut remaining_entries,
    } = outcome.expect_err("foreign runtime entries must prevent a clean removal")
    else {
        panic!("shutdown returned the wrong typed residual");
    };
    expected.sort();
    remaining_entries.sort();
    assert_eq!(path, runtime_dir);
    assert_eq!(remaining_entries, expected);
}

fn assert_racing_endpoint_shutdown(
    outcome: Result<ShutdownOutcome, DaemonError>,
    config: &DaemonConfig,
) {
    match outcome {
        Ok(ShutdownOutcome::Graceful) => {}
        Err(DaemonError::RuntimeDirectoryNotEmpty {
            path,
            remaining_entries,
        }) => {
            assert_eq!(path, config.runtime_dir);
            assert!(!remaining_entries.is_empty());
            assert!(remaining_entries.iter().all(|entry| {
                entry == &config.endpoint_path()
                    || entry
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(".haiderd-"))
            }));
        }
        other => panic!("unexpected racing shutdown outcome: {other:?}"),
    }
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
        // Hermetic: the REAL child binary parses CLI args, so the in-process
        // `test_config` flag never reaches it — without this env it runs
        // startup auto-adoption (A2) against the HOST's credential stores.
        // Under gate load that wedged shutdown (gate116/117: the two
        // child-process lifecycle tests were the only flakes).
        .env("HAIDER_DISCOVERY_DISABLED", "1")
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
        encodings: Vec::new(),
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
                    ..
                }) if found == effect
            )
        })
        .count()
}

#[tokio::test]
async fn cold_start_socket_missing_serves_handshake_ping_and_session_list() {
    let root = test_root("w3b1-");
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
    assert_eq!(client.receive_reply().await, WireFrame::Pong { nonce: 77 });
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("list"),
                body: RequestBody::SessionList {
                    cursor: None,
                    limit: 10,
                },
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        client.receive_reply().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::SessionList {
                ref sessions,
                next_cursor: None,
            },
            ..
        } if request_id.as_str() == "list" && sessions.is_empty()
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
    let root = test_root("w3b1-");
    let config = test_config(&root, "stale-pid");
    fs::create_dir_all(&config.store_dir).expect("store root");
    fs::write(
        config.store_dir.join("lock.owner"),
        format!("pid={}\ncreated_at_ms=1\n", std::process::id()),
    )
    .expect("stale diagnostics");

    let store_dir = config.store_dir.clone();
    let task = spawn(config);
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;
    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
    assert_eq!(
        fs::read(store_dir.join("lock")).expect("read released pure lock file"),
        b""
    );
    assert!(!store_dir.join("lock.owner").exists());
}

#[tokio::test]
async fn already_running_error_carries_incumbent_diagnostics() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "diagnostics");
    let incumbent = spawn(config.clone());
    wait_for_state(incumbent.readiness(), |state| *state == DaemonState::Ready).await;
    assert!(
        fs::read_to_string(config.store_dir.join("lock.owner"))
            .expect("read owner diagnostics")
            .contains("pid=")
    );

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
    assert_eq!(
        fs::read(root.path().join("store/lock")).expect("read released pure lock file"),
        b""
    );
    assert!(!root.path().join("store/lock.owner").exists());
}

#[tokio::test]
async fn simultaneous_start_n_processes_has_one_winner_and_clean_losers() {
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
    let config = test_config(&root, "successor");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let socket_path = config.endpoint_path();
    fs::remove_file(&socket_path).expect("unlink old rendezvous node");
    let successor = StdUnixListener::bind(&socket_path).expect("bind successor node");
    let successor_metadata = fs::symlink_metadata(&socket_path).expect("successor metadata");
    task.shutdown_handle().request("controlled handover");
    assert_runtime_directory_residual(
        task.join().await,
        &config.runtime_dir,
        vec![socket_path.clone()],
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
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
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
    assert_eq!(client.receive_reply().await, WireFrame::Pong { nonce: 9 });
    recovered.signal(Signal::TERM);
    assert!(recovered.wait().await.success());
}

#[tokio::test]
async fn handshake_version_mismatch_returns_fatal_rejection() {
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
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

/// Both capabilities that can act on binding state receive the baseline;
/// a capability-empty connection receives no binding frames at all.
///
/// MUTATION CHECK (Control omitted): remove the `Capability::Control` arm of
/// `may_view_binding` in `SessionHub::open_connection`. The raw post-Welcome
/// read gets Pong instead of the required control baseline.
///
/// MUTATION CHECK (gate widened): replace `may_view_binding` with `true`.
/// The capability-empty connection gets a binding baseline instead of Pong.
#[tokio::test]
async fn binding_baseline_requires_view_or_control_capability() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "binding-capability-rule");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut controller = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect control-only client");
    let mut request = hello(
        WIRE_PROTOCOL_VERSION,
        WIRE_PROTOCOL_VERSION,
        u32::try_from(config.frame_limit).expect("frame limit fits"),
    );
    if let WireFrame::Hello(hello) = &mut request {
        hello.capabilities_requested = CapabilitySet::from([Capability::Control]);
    }
    controller.send(&request, config.frame_limit).await;
    match controller.next().await {
        WireFrame::Welcome(welcome) => assert_eq!(
            welcome.capabilities_granted,
            CapabilitySet::from([Capability::Control])
        ),
        frame => panic!("expected Welcome first, got {frame:?}"),
    }

    controller
        .send(&WireFrame::Ping { nonce: 41 }, config.frame_limit)
        .await;
    let control_baseline = controller.next().await;
    assert!(
        matches!(
            &control_baseline,
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation,
                ..
            } if *worker_generation > 0
        ),
        "a control-only connection must receive the explicit unbound baseline, got {control_baseline:?}"
    );
    assert_eq!(controller.next().await, WireFrame::Pong { nonce: 41 });

    let mut unprivileged = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect capability-empty client");
    let mut request = hello(
        WIRE_PROTOCOL_VERSION,
        WIRE_PROTOCOL_VERSION,
        u32::try_from(config.frame_limit).expect("frame limit fits"),
    );
    if let WireFrame::Hello(hello) = &mut request {
        hello.capabilities_requested = CapabilitySet::new();
    }
    unprivileged.send(&request, config.frame_limit).await;
    match unprivileged.next().await {
        WireFrame::Welcome(welcome) => {
            assert_eq!(welcome.capabilities_granted, CapabilitySet::new());
        }
        frame => panic!("expected Welcome first, got {frame:?}"),
    }
    unprivileged
        .send(&WireFrame::Ping { nonce: 43 }, config.frame_limit)
        .await;
    assert_eq!(
        unprivileged.next().await,
        WireFrame::Pong { nonce: 43 },
        "a connection with neither View nor Control must receive no binding baseline"
    );

    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

/// The open-time binding snapshot is an explicit frame, not silence: Welcome
/// is first, then exactly one unbound baseline precedes a later Pong.
///
/// MUTATION CHECK (Welcome first): change the System-lane admission in
/// `OutboundLane::try_push_with_floor` from `push_back` to `push_front`. The
/// first raw read gets the binding baseline instead of Welcome.
///
/// MUTATION CHECK (exactly one): send the open-time baseline twice in
/// `SessionHub::open_connection`. The second raw post-baseline read gets the
/// duplicate binding frame instead of Pong.
///
/// MUTATION CHECK (present unbound state): synthesize a non-`None` session id
/// in the no-binding fallback. The baseline's `session_id.is_none()` assertion
/// fails on the frame that was actually received.
#[tokio::test]
async fn view_connection_receives_exactly_one_unbound_baseline_after_welcome() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "ordered-binding-baseline");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let mut client = TestClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect view client");
    let mut request = hello(
        WIRE_PROTOCOL_VERSION,
        WIRE_PROTOCOL_VERSION,
        u32::try_from(config.frame_limit).expect("frame limit fits"),
    );
    if let WireFrame::Hello(hello) = &mut request {
        hello.capabilities_requested = CapabilitySet::from([Capability::View]);
    }
    client.send(&request, config.frame_limit).await;
    match client.next().await {
        WireFrame::Welcome(welcome) => assert_eq!(
            welcome.capabilities_granted,
            CapabilitySet::from([Capability::View])
        ),
        frame => panic!("expected Welcome first, got {frame:?}"),
    }

    match client.next().await {
        WireFrame::ResidentSessionBinding {
            session_id,
            worker_generation,
            ..
        } => {
            assert!(
                session_id.is_none(),
                "the present open-time baseline must explicitly say nothing is bound, got {session_id:?}"
            );
            assert!(
                worker_generation > 0,
                "the unbound baseline must carry the daemon's live generation"
            );
        }
        frame => panic!("expected exactly one binding baseline after Welcome, got {frame:?}"),
    }
    client
        .send(&WireFrame::Ping { nonce: 42 }, config.frame_limit)
        .await;
    assert_eq!(
        client.next().await,
        WireFrame::Pong { nonce: 42 },
        "a duplicate open-time baseline must not precede the next reply"
    );

    task.shutdown_handle().request("test complete");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Graceful
    );
}

#[tokio::test]
async fn drain_notifies_every_open_connection_before_close() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "drain-notify");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;
    let mut first = handshake(&config.endpoint_path(), config.frame_limit).await;
    let mut second = handshake(&config.endpoint_path(), config.frame_limit).await;

    task.shutdown_handle().request("maintenance");
    for client in [&mut first, &mut second] {
        assert!(matches!(
            client.receive_reply().await,
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
    let root = test_root("w3b1-");
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

/// MUTATION CHECK: drop the `charge` call in `connection.rs::enqueue` (enqueue
/// straight into the frame queue). Expected failure: the pongs keep queueing,
/// the connection never dies, and the `try_receive` below returns a frame
/// instead of EOF.
#[tokio::test]
async fn outbound_byte_budget_refuses_a_frame_the_connection_cannot_hold() {
    let root = test_root("w3b1-");
    // A half-megabyte Welcome parks the writer inside `write_all` against a
    // peer that never reads, so nothing is ever credited back: replies then
    // accumulate against the byte budget until one is refused. The budget is
    // exactly one maximum encoded frame, `frame_limit + 4` (the smallest
    // coherent setting), and the pongs that follow have nowhere left to go.
    let profile = format!("byte-budget-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    // Headroom for the Welcome frame beyond the giant profile id. The
    // features list lives inside Welcome, so every new feature bit grows
    // it — 4 KiB keeps these byte-budget laws stable across additions.
    config.frame_limit = config.profile_id.len() + 4_096;
    config.outbound_queued_bytes = config.frame_limit + 4;
    // rev933c finding 7: the connection HALVES this capacity into per-lane
    // slots, and every pong rides the System lane — the per-lane count
    // bound must stay strictly above the flood so only the BYTES fire.
    config.outbound_queue_capacity = 1024;
    // rev933b finding 8: the refusal must not depend on how much of the
    // 4 KiB Welcome headroom the CURRENT feature list happens to eat. 400
    // framed pongs (~14 KiB) exceed the entire possible slack, so a refusal
    // fires under any Welcome size that still fits the frame limit.
    let pings = 400;
    assert!(
        config.outbound_queue_capacity / 2 > pings + 1,
        "the per-lane frame-count bound must not be what fires here — only the bytes"
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
    for nonce in 0..pings {
        // The refused charge may close the connection while the flood is
        // still in flight — a mid-flood write failure IS the law firing,
        // never a test defect.
        if !client
            .try_send(
                &WireFrame::Ping {
                    nonce: nonce as u64,
                },
                config.frame_limit,
            )
            .await
        {
            break;
        }
    }
    // The client never reads, so the only way this returns is the connection
    // being closed by the refused charge.
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

/// MUTATION CHECK: in `connection.rs`, replace the reserve `try_send` with an
/// ordinary `enqueue(&lane, &frame, outbound_limit)`. Expected failure: the
/// notice is refused by the exhausted budget, the client sees EOF after the
/// Welcome, and `receive()` panics with "connection closed before a frame
/// arrived". Verified 2026-07-27.
///
/// MUTATION CHECK (notice absent): remove every `ServerDraining` from the
/// completed delivery set before validation. The notice cardinality assertion
/// fails with 0 versus 1. Verified 2026-08-23.
///
/// MUTATION CHECK (duplicate baseline): append a clone of the delivered
/// `ResidentSessionBinding` before validation. The baseline cardinality
/// assertion fails with 2 versus 1. Verified 2026-08-23.
#[tokio::test]
async fn reserved_drain_notice_survives_an_exhausted_outbound_byte_budget() {
    let root = test_root("w3b1-");
    // A half-megabyte profile id makes the Welcome larger than any socket
    // buffer, so it is still mid-write — and still charged — when the drain
    // fires. A one-maximum-encoded-frame budget leaves less headroom than the
    // (deliberately long-reasoned) ServerDraining frame needs, so only a
    // reserve outside the ordinary budget can deliver it. W3b2's real
    // responses are the traffic this budget exists for; the Welcome is the
    // one large daemon-authored frame a W3b1 test can size.
    let profile = format!("drain-reserve-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    // Headroom for the Welcome frame beyond the giant profile id. The
    // features list lives inside Welcome, so every new feature bit grows
    // it — 4 KiB keeps these byte-budget laws stable across additions.
    config.frame_limit = config.profile_id.len() + 4_096;
    config.outbound_queued_bytes = config.frame_limit + 4;
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
    let mut delivered = Vec::new();
    while let Some(frame) = client.try_receive().await {
        delivered.push(frame);
    }

    assert!(
        matches!(delivered.first(), Some(WireFrame::Welcome(_))),
        "Welcome must be delivered first, got {:?}",
        delivered.first()
    );

    // After Welcome, the drain notice and open-time binding baseline may arrive
    // in either order: the writer contract permits already-queued traffic to
    // follow ServerDraining while the client is disconnecting.
    let notices: Vec<_> = delivered
        .iter()
        .filter(|frame| matches!(frame, WireFrame::ServerDraining { .. }))
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "the reserved drain notice must be delivered exactly once among {} frames",
        delivered.len()
    );
    let notice = notices[0];
    assert!(
        matches!(
            notice,
            WireFrame::ServerDraining {
                reason: notice_reason,
                deadline_unix_ms,
                ..
            } if notice_reason == &reason && *deadline_unix_ms > 0
        ),
        "the reserved drain notice must carry the requested reason and a deadline, got {notice:?}"
    );

    let baselines: Vec<_> = delivered
        .iter()
        .filter(|frame| matches!(frame, WireFrame::ResidentSessionBinding { .. }))
        .collect();
    assert_eq!(
        baselines.len(),
        1,
        "exactly one resident-binding baseline must be delivered among {} frames",
        delivered.len()
    );
    assert!(
        matches!(
            baselines[0],
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation,
                ..
            } if *worker_generation > 0
        ),
        "the resident-binding baseline must report live unbound state, got {:?}",
        baselines[0]
    );
    assert_eq!(
        delivered.len(),
        3,
        "expected only Welcome, one drain notice, and one binding baseline"
    );

    // Pin the counterfactual: had the notice shared the ordinary budget with
    // the charged-but-unwritten Welcome, it could not have been queued at all.
    let welcome_bytes = uds_codec::encode(&delivered[0], config.frame_limit)
        .expect("re-encode welcome")
        .len();
    let notice_bytes = uds_codec::encode(notice, config.frame_limit)
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
    let root = test_root("w3b1-");
    let profile = format!("never-reads-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    // Headroom for the Welcome frame beyond the giant profile id. The
    // features list lives inside Welcome, so every new feature bit grows
    // it — 4 KiB keeps these byte-budget laws stable across additions.
    config.frame_limit = config.profile_id.len() + 4_096;
    config.outbound_queued_bytes = config.frame_limit + 4;
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

/// The strict form of the same law with a peer that reads NOTHING — not one
/// byte, ever. The daemon's reply is parked in `write_all` from the first
/// moment, so the barrier is the only thing that can end this connection, and
/// the writer must be gone (aborted AND joined) before the daemon reports.
#[tokio::test]
async fn client_that_never_reads_a_byte_cannot_hold_the_barrier_open() {
    let root = test_root("w3b1-");
    let profile = format!("never-reads-strict-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    // Headroom for the Welcome frame beyond the giant profile id. The
    // features list lives inside Welcome, so every new feature bit grows
    // it — 4 KiB keeps these byte-budget laws stable across additions.
    config.frame_limit = config.profile_id.len() + 4_096;
    config.outbound_queued_bytes = config.frame_limit + 4;
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
    // Synchronise WITHOUT consuming: readable() proves the daemon's reply has
    // started arriving, and the client still has not read a single byte.
    tokio::time::timeout(DEADLINE, client.stream.readable())
        .await
        .expect("reply deadline")
        .expect("readable");

    task.shutdown_handle().request("strict deadline");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Forced
    );
    assert!(!config.endpoint_path().exists());
    poll_store_release(&config).await;

    // Nothing is feeding this socket any more: the reply is a truncated prefix
    // followed by EOF, reached promptly because the writer was joined, not left
    // to finish its half-megabyte frame.
    let delivered = tokio::time::timeout(Duration::from_secs(2), client.frames_until_eof())
        .await
        .expect("EOF must follow the barrier promptly");
    assert_eq!(
        delivered, 0,
        "a writer that outlived teardown completed its frame"
    );
}

/// A peer that reads exactly one byte and then stops is the same hazard with a
/// different shape: the write is in flight, the socket buffer never drains, and
/// the barrier still has to end everything on time.
#[tokio::test]
async fn one_byte_reader_cannot_hold_the_barrier_open() {
    let root = test_root("w3b1-");
    let profile = format!("one-byte-reader-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    // Headroom for the Welcome frame beyond the giant profile id. The
    // features list lives inside Welcome, so every new feature bit grows
    // it — 4 KiB keeps these byte-budget laws stable across additions.
    config.frame_limit = config.profile_id.len() + 4_096;
    config.outbound_queued_bytes = config.frame_limit + 4;
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
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(DEADLINE, client.stream.read(&mut byte))
        .await
        .expect("first byte deadline")
        .expect("first byte");
    assert_eq!(read, 1);

    task.shutdown_handle().request("one byte and no more");
    assert_eq!(
        task.join().await.expect("daemon joins"),
        ShutdownOutcome::Forced
    );
    assert!(!config.endpoint_path().exists());
    poll_store_release(&config).await;
    let delivered = tokio::time::timeout(Duration::from_secs(2), client.frames_until_eof())
        .await
        .expect("EOF must follow the barrier promptly");
    assert_eq!(delivered, 0);
}

/// The forced path aborts connection tasks outright. The writer must die with
/// its connection: joining it may not hand its handle away, or the abort finds
/// nothing to cancel and the writer (plus its socket and payload) survives
/// endpoint cleanup and the profile-lock release.
/// A connection accepted in the same instant as the shutdown request: its
/// first poll can run while the barrier is already draining, so its writer may
/// be registered AFTER the barrier's first collection. The re-drain that
/// follows the final connection join is what keeps that writer from being
/// aborted-but-never-joined.
///
/// MUTATION CHECK: delete the second `self.collect_writers()` (and the abort
/// loop that follows it) in `ConnectionRuntime::drain`. The window is a
/// scheduling coincidence, so this test cannot force it — see the honest note
/// in docs/OPTIMIZATIONS.md; what it does pin is that connections racing the
/// barrier are still torn down completely and the daemon stays honest.
#[tokio::test]
async fn connections_racing_the_shutdown_request_are_torn_down_completely() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "late-registration");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    // Fire a burst of connects and the shutdown request together, so some
    // connections are accepted (and register their writers) while the barrier
    // is already running.
    let endpoint = config.endpoint_path();
    let limit = config.frame_limit;
    let racers = (0..8)
        .map(|_| {
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                let Ok(mut client) = TestClient::connect(&endpoint, limit).await else {
                    return;
                };
                let announced = u32::try_from(limit).expect("frame limit fits");
                let _ = client
                    .try_send(
                        &hello(WIRE_PROTOCOL_VERSION, WIRE_PROTOCOL_VERSION, announced),
                        limit,
                    )
                    .await;
                let _ = client.try_receive().await;
            })
        })
        .collect::<Vec<_>>();
    task.shutdown_handle().request("racing accepts");

    let outcome = task.join().await.expect("daemon joins");
    assert!(matches!(
        outcome,
        ShutdownOutcome::Graceful | ShutdownOutcome::Forced
    ));
    for racer in racers {
        let _ = racer.await;
    }
    // Whatever the ordering did, teardown completed: no socket, no lock.
    assert!(!config.endpoint_path().exists());
    poll_store_release(&config).await;
}

/// MUTATION CHECK: remove BOTH writer cancellations — the `abort` in
/// `WriterGuard::drop` and the runtime's `writer.abort()` loop in
/// `ConnectionRuntime::drain`. Expected failure: the detached writer finishes
/// its half-megabyte frame after the abort, so `frames_until_eof` returns 2
/// instead of 0. Verified 2026-07-27.
#[tokio::test]
async fn forced_shutdown_aborts_a_blocked_writer_instead_of_detaching_it() {
    let root = test_root("w3b1-");
    let profile = format!("forced-writer-{}", "p".repeat(512 * 1024));
    let mut config = test_config(&root, &profile);
    // Headroom for the Welcome frame beyond the giant profile id. The
    // features list lives inside Welcome, so every new feature bit grows
    // it — 4 KiB keeps these byte-budget laws stable across additions.
    config.frame_limit = config.profile_id.len() + 4_096;
    config.outbound_queued_bytes = config.frame_limit + 4;
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
/// MUTATION CHECK: in `finalize`, call `store.flush()`/`store.close()`
/// directly instead of through `barrier_step`. Expected failure: the overrun
/// goes unnoticed and the daemon reports `Graceful` where the test demands
/// `Forced`. Verified 2026-07-27.
#[tokio::test]
async fn drain_deadline_covers_the_finalization_tail() {
    let root = test_root("w3b1-");
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

/// R3/R22, quiet ordering: a replacement that is already in place when cleanup
/// runs is left completely alone.
#[tokio::test]
async fn endpoint_replacement_before_cleanup_is_never_deleted() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "cleanup-replacement");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let socket_path = config.endpoint_path();
    let _ = fs::remove_file(&socket_path);
    let replacement = StdUnixListener::bind(&socket_path).expect("bind replacement node");
    let replacement_metadata = fs::symlink_metadata(&socket_path).expect("replacement metadata");
    task.shutdown_handle().request("handover");
    assert_runtime_directory_residual(
        task.join().await,
        &config.runtime_dir,
        vec![socket_path.clone()],
    );
    let after = fs::symlink_metadata(&socket_path).expect("replacement node survives cleanup");
    assert_eq!(
        (after.dev(), after.ino()),
        (replacement_metadata.dev(), replacement_metadata.ino())
    );
    drop(replacement);
    fs::remove_file(socket_path).expect("remove replacement");
}

/// R3/R22, real race: another same-UID process replaces the endpoint node in a
/// tight loop across the whole drain, so its swaps genuinely interleave with
/// the daemon's cleanup on another thread. The invariant is absolute — the
/// daemon may unlink only a node whose identity it recorded — so a node the
/// racer created must NEVER disappear under it.
#[tokio::test]
async fn endpoint_replacement_racing_cleanup_is_never_deleted() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "cleanup-race");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    let socket_path = config.endpoint_path();
    let stop = Arc::new(AtomicBool::new(false));
    let stolen = Arc::new(AtomicUsize::new(0));
    let racer = std::thread::spawn({
        let path = socket_path.clone();
        let runtime_dir = config.runtime_dir.clone();
        let stop = Arc::clone(&stop);
        let stolen = Arc::clone(&stolen);
        move || {
            while !stop.load(AtomicOrdering::Relaxed) {
                let _ = fs::remove_file(&path);
                let Ok(listener) = StdUnixListener::bind(&path) else {
                    continue;
                };
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let mine = (metadata.dev(), metadata.ino());
                // Watch my own node for a while: anything that removes or
                // replaces it in this window is not me. Cleanup is allowed to
                // claim the node under a staging name while it verifies the
                // inode, so disappearance from the public name is a violation
                // only when the inode disappeared from the directory too.
                for _ in 0..256 {
                    match fs::symlink_metadata(&path) {
                        Ok(found) if (found.dev(), found.ino()) == mine => {}
                        _ if !inode_exists_in(&runtime_dir, mine) => {
                            stolen.fetch_add(1, AtomicOrdering::Relaxed);
                            break;
                        }
                        _ => {}
                    }
                    std::thread::yield_now();
                }
                drop(listener);
            }
        }
    });

    task.shutdown_handle().request("racing handover");
    assert_racing_endpoint_shutdown(task.join().await, &config);
    stop.store(true, AtomicOrdering::Relaxed);
    racer.join().expect("racer thread");
    assert_eq!(
        stolen.load(AtomicOrdering::Relaxed),
        0,
        "the daemon removed an endpoint node it never created"
    );
    let _ = fs::remove_file(&socket_path);
}

/// Does any entry in `directory` still refer to this exact inode? Used to tell
/// "the node was moved" (legal, and the documented claim residual) from "the
/// node was unlinked" (never legal for a node this daemon did not create).
fn inode_exists_in(directory: &Path, identity: (u64, u64)) -> bool {
    // A rename is atomic, but a directory scan is not, so a node caught
    // mid-claim can be missed once; retry before believing it is gone.
    for _ in 0..8 {
        let Ok(entries) = fs::read_dir(directory) else {
            return true;
        };
        for entry in entries.flatten() {
            if let Ok(metadata) = fs::symlink_metadata(entry.path())
                && (metadata.dev(), metadata.ino()) == identity
            {
                return true;
            }
        }
        std::thread::yield_now();
    }
    false
}

/// R3's widest window: the stale-cleanup decision is made by a connect probe,
/// and a node can go LIVE between that probe and the removal. Cleanup must
/// therefore re-probe the node it actually holds — under its claimed name —
/// so a listener that came up in the gap is restored, never unlinked.
///
/// The racer flips the endpoint between live and stale continuously while the
/// daemon repeatedly tries to start, so the gap is genuinely exercised; the
/// daemon may legitimately refuse (live at probe) or start (stale at probe),
/// and either way a LIVE node of the racer's must never be removed.
/// MUTATION CHECK: replace `remove_verified_stale` with the pre-fix shape —
/// `statat` the PUBLIC name, then `unlinkat` it, with no claim and no liveness
/// re-probe. Expected failure: a node that went live inside the probe → unlink
/// window is deleted, so `stolen` is non-zero (observed in 5 of 6 runs; the
/// window is real but not certain, so re-run a few times). Verified
/// 2026-07-27.
#[tokio::test]
async fn stale_cleanup_never_removes_a_node_that_went_live() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "stale-flip");
    fs::create_dir_all(&config.runtime_dir).expect("runtime directory");
    let socket_path = config.endpoint_path();
    let stop = Arc::new(AtomicBool::new(false));
    let stolen = Arc::new(AtomicUsize::new(0));
    let racer = std::thread::spawn({
        let path = socket_path.clone();
        let runtime_dir = config.runtime_dir.clone();
        let stop = Arc::clone(&stop);
        let stolen = Arc::clone(&stolen);
        move || {
            while !stop.load(AtomicOrdering::Relaxed) {
                let _ = fs::remove_file(&path);
                let Ok(listener) = StdUnixListener::bind(&path) else {
                    continue;
                };
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let mine = (metadata.dev(), metadata.ino());
                // While this listener is LIVE, its inode may never be UNLINKED.
                // A claim that moves it under a staging name and restores it
                // (or strands it there) is the documented residual — the law
                // being pinned is that the node still exists somewhere.
                for _ in 0..64 {
                    match fs::symlink_metadata(&path) {
                        Ok(found) if (found.dev(), found.ino()) == mine => {}
                        _ if !inode_exists_in(&runtime_dir, mine) => {
                            stolen.fetch_add(1, AtomicOrdering::Relaxed);
                            break;
                        }
                        _ => {}
                    }
                    std::thread::yield_now();
                }
                // Now the node is stale: exactly what the daemon's preflight
                // probe is allowed to remove — but only if it is still stale
                // when the removal happens.
                drop(listener);
                for _ in 0..64 {
                    std::thread::yield_now();
                }
            }
        }
    });

    for _ in 0..32 {
        let task = spawn(config.clone());
        // A start may legitimately succeed (the node was stale all the way
        // through) or be refused (it was live at the probe); either way the
        // daemon must stop on request rather than be waited on forever.
        wait_for_state(task.readiness(), |state| {
            matches!(state, DaemonState::Ready | DaemonState::Failed { .. })
        })
        .await;
        task.shutdown_handle().request("flip round");
        match task.join().await {
            Ok(_) | Err(DaemonError::Endpoint { .. }) => {}
            outcome @ Err(DaemonError::RuntimeDirectoryNotEmpty { .. }) => {
                assert_racing_endpoint_shutdown(outcome, &config);
            }
            other => panic!("unexpected startup outcome: {other:?}"),
        }
        poll_store_release(&config).await;
    }
    stop.store(true, AtomicOrdering::Relaxed);
    racer.join().expect("racer thread");
    assert_eq!(
        stolen.load(AtomicOrdering::Relaxed),
        0,
        "startup cleanup unlinked a node that was live when it was removed"
    );
    let _ = fs::remove_file(&socket_path);
}

/// A daemon that died between claim and restore leaves a staging node behind.
/// The next start sweeps its own leftovers — and only those: a staging name
/// that is still LIVE belongs to someone else's in-flight bind and is left.
/// MUTATION CHECK: delete the `sweep_staging(..)` call in `endpoint::bind`.
/// Expected failure: the stranded node is still there after the daemon reaches
/// Ready ("a stale staging leftover must be swept at startup"). Verified
/// 2026-07-27.
#[tokio::test]
async fn stranded_staging_nodes_are_swept_but_live_ones_are_left() {
    let root = test_root("w3b1-");
    let config = test_config(&root, "staging-sweep");
    fs::create_dir_all(&config.runtime_dir).expect("runtime directory");
    let stranded = config
        .runtime_dir
        .join(".haiderd-00112233445566778899aabbccddeeff");
    let live = config
        .runtime_dir
        .join(".haiderd-ffeeddccbbaa99887766554433221100");
    let unrelated = config.runtime_dir.join("keep-me.sock");
    drop(StdUnixListener::bind(&stranded).expect("stranded staging node"));
    let live_listener = StdUnixListener::bind(&live).expect("live staging node");
    drop(StdUnixListener::bind(&unrelated).expect("unrelated node"));

    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;
    assert!(
        !stranded.exists(),
        "a stale staging leftover must be swept at startup"
    );
    assert!(
        live.exists(),
        "a live staging node is somebody's in-flight bind, not garbage"
    );
    assert!(
        unrelated.exists(),
        "the sweep must only ever consider its own staging prefix"
    );

    drop(live_listener);
    task.shutdown_handle().request("test complete");
    assert_runtime_directory_residual(
        task.join().await,
        &config.runtime_dir,
        vec![live.clone(), unrelated.clone()],
    );
}

/// The conservative half of R3: a LIVE endpoint owned by someone else is
/// refused, never unlinked — even though the profile lock was free.
#[tokio::test]
async fn live_foreign_endpoint_is_refused_and_left_intact() {
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
    let config = test_config(&root, "small-limit");
    let task = spawn(config.clone());
    wait_for_state(task.readiness(), |state| *state == DaemonState::Ready).await;

    // Big enough for the REAL Welcome (the additive workflow, descendant,
    // and monitor feature tokens pushed it past 2048), but still smaller
    // than the 4KiB-plus-label drain reason below, which MUST truncate.
    let announced = 4096;
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
    let notice = client.receive_reply().await;
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
    let root = test_root("w3b1-");
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
            viewer.receive_reply().await,
            WireFrame::Response {
                ref request_id,
                body: ResponseBody::Error { ref code, .. },
            } if request_id.as_str() == "menu-request" && code == "capability_denied"
        ),
        "a view-only connection must be denied the control frame, correlated to its request"
    );

    // Capability alone does not authorize a controller without a viewport:
    // policy requires a control attachment to the target session.
    let mut controller = handshake(&config.endpoint_path(), config.frame_limit).await;
    controller
        .send(&menu_answer(None), config.frame_limit)
        .await;
    assert!(matches!(
        controller.receive_reply().await,
        WireFrame::ProtocolError(ProtocolError { ref code, fatal: false, .. })
            if code == "capability_denied"
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
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
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
        client.receive_reply().await,
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
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
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
    let root = test_root("w3b1-");
    let config = test_config(&root, "forced-signals");
    let mut child = ManagedChild::new(child_command(&config).spawn().expect("daemon process"));
    let _client = poll_process_ready(&config).await;
    child.signal(Signal::TERM);
    child.signal(Signal::INT);
    assert_eq!(child.wait().await.code(), Some(130));
}

#[tokio::test]
async fn reconcile_before_ready_marks_unknown_exactly_once_and_never_retries_effect() {
    let root = test_root("w3b1-");
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
                    freshness: None,
                    workspace_mutation: None,
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

    // Directed hardening (W3c2 review P3-7, twice-observed flake): the joined
    // daemon's store-lock release can lag the join under full-workspace
    // parallel load, so the immediate inspect open raced it and failed with
    // the RETRYABLE StoreLocked. Honor the retryable contract with a bounded
    // retry; the law under test (exactly-once Unknown reconciliation) is
    // unchanged.
    let mut store = Store::open(&config.store_dir);
    for _ in 0..200u32 {
        match &store {
            Err(error) if error.retryable => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                store = Store::open(&config.store_dir);
            }
            _ => break,
        }
    }
    let store = store.expect("inspect recovered store");
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
