#![cfg(unix)]
//! Real-daemon W9a drain/restart/health/rollback acceptance.
#![allow(clippy::expect_used)]
#![allow(dead_code)]

#[path = "../src/main.rs"]
mod cli_main;

use cli_main::update::UpdateError;
use cli_main::update::discovery::{CurlTransport, ReleaseSelection, SemVersion};
use cli_main::update::restart::{
    RestartHooks, detect_incumbent, restart_committed, restart_committed_for_test,
};
use cli_main::update::stage_then_acquire;
use cli_main::update::staging::{SystemStageVerifier, sha256_file, verified_pair_for_test};
use cli_main::update::transaction::{
    InstallLayout, InstalledPairVerifier, NoFaults, PreparedTransaction, commit_pair, marker_path,
};
use haider_client::{ClientConfig, ResolvedProfile, connect, signal_authenticated_peer};
use haider_rpc::{
    Capability, CapabilitySet, LifecyclePhase, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
};
use haider_store::Store;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{Instant, sleep};

const DEADLINE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
const STDERR_TAIL_LINES: usize = 80;
const STDERR_LINE_BYTES: usize = 4096;
const STDERR_FINISH_GRACE: Duration = Duration::from_secs(1);

fn fixture_target() -> &'static str {
    cli_main::update::discovery::compiled_target().unwrap_or("aarch64-apple-darwin")
}

struct DigestVerifier;

impl InstalledPairVerifier for DigestVerifier {
    fn verify(
        &self,
        layout: &InstallLayout,
        pair: &cli_main::update::staging::VerifiedStagedPair,
    ) -> Result<(), UpdateError> {
        if sha256_file(&layout.haider)? == pair.haider_digest()
            && sha256_file(&layout.haiderd)? == pair.haiderd_digest()
        {
            Ok(())
        } else {
            Err(UpdateError::Internal("fixture pair digest mismatch".into()))
        }
    }
}

#[derive(Default)]
struct SpyRestartHooks {
    events: Mutex<Vec<String>>,
}

impl RestartHooks for SpyRestartHooks {
    fn observe_committed_pair(
        &self,
        committed: &cli_main::update::transaction::CommittedUpdate,
    ) -> Result<(), UpdateError> {
        let daemon = fs::read(&committed.layout().haiderd)
            .map_err(|error| UpdateError::io("restart spy read daemon", error))?;
        let cli = fs::read(&committed.layout().haider)
            .map_err(|error| UpdateError::io("restart spy read CLI", error))?;
        if daemon != b"new-daemon" || cli != b"new-cli" {
            return Err(UpdateError::Internal(
                "restart spy observed anything other than the new pair".into(),
            ));
        }
        committed.verify_target_pair()?;
        self.events
            .lock()
            .expect("restart spy mutex")
            .push("observe:new-daemon+new-cli".into());
        Ok(())
    }

    fn signal(&self, pid: u32) -> Result<(), UpdateError> {
        self.events
            .lock()
            .expect("restart spy mutex")
            .push(format!("signal:{pid}"));
        Ok(())
    }
}

/// MUTATION CHECK: move any restart action before observation or make a
/// partially committed state capable of calling restart. Expected RUNTIME
/// failure: the real restart function's first spy event is not an exact
/// daemon-new/CLI-new read.
#[tokio::test]
async fn restart_spy_observes_exact_new_pair_as_the_first_restart_action() {
    let install = install_fixture_bytes();
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let pair = verified_pair_bytes(install.path());
    let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare restart spy");
    let mut committed = commit_pair(prepared, pair, &NoFaults, &DigestVerifier, "1.0.0")
        .expect("commit restart spy pair");
    let profile = resolved_test_profile(install.path().join("profile"));
    let hooks = SpyRestartHooks::default();
    restart_committed_for_test(
        &mut committed,
        None,
        &profile,
        &hooks,
        Duration::from_millis(25),
    )
    .await
    .expect("finalize no-incumbent restart");
    assert_eq!(
        *hooks.events.lock().expect("restart spy mutex"),
        ["observe:new-daemon+new-cli"]
    );
    assert!(!marker_path(&layout).exists());
}

/// MUTATION CHECK: retry SIGTERM after a drain timeout, finalize the marker,
/// or delete backups on timeout. Expected RUNTIME failure: signal count is
/// not exactly one or recovery assets do not remain durable.
#[tokio::test]
async fn drain_timeout_signals_once_and_retains_recovery_assets() {
    let install = install_fixture_bytes();
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let pair = verified_pair_bytes(install.path());
    let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare timeout pair");
    let mut committed = commit_pair(prepared, pair, &NoFaults, &DigestVerifier, "1.0.0")
        .expect("commit timeout pair");
    let profile = resolved_test_profile(install.path().join("profile"));
    let Some((server, incumbent)) = fake_non_draining_incumbent(&profile).await else {
        return;
    };
    let hooks = SpyRestartHooks::default();
    let error = restart_committed_for_test(
        &mut committed,
        Some(incumbent),
        &profile,
        &hooks,
        Duration::from_millis(50),
    )
    .await
    .expect_err("non-draining incumbent must time out");
    assert!(matches!(error, UpdateError::RestartTimeout(_)));
    let events = hooks.events.lock().expect("restart spy mutex").clone();
    assert_eq!(
        events.first().map(String::as_str),
        Some("observe:new-daemon+new-cli")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("signal:"))
            .count(),
        1
    );
    assert!(committed.recovery_assets_exist());
    server.abort();
    let _ = server.await;
}

/// MUTATION CHECK: let an HTTP-truncated archive reach transaction acquire,
/// daemon detection, or signaling. Expected RUNTIME failure: canonical state
/// changes, a marker appears, or the live daemon identity/generation changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_archive_http_leaves_pair_and_live_daemon_unchanged() {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("bind truncated archive fixture: {error}"),
    };
    let address = listener.local_addr().expect("truncated archive address");

    let fixture = RestartFixture::new();
    let before = pair_snapshot(&fixture.layout);
    let mut old_child = fixture.spawn_installed();
    let Some(old) = wait_initial_ready(&fixture.profile, &mut old_child).await else {
        return;
    };
    let old_instance = old.welcome.instance_id.clone();
    let old_generation = old.welcome.daemon_generation;
    let _ = old.client.close();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept archive request");
        let mut request = [0_u8; 4096];
        let _ = std::io::Read::read(&mut stream, &mut request);
        std::io::Write::write_all(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\npartial",
        )
        .expect("write truncated archive");
    });

    // The transaction remains worth exercising on Unix hosts where the
    // packaged self-update command is intentionally unavailable.
    let target = fixture_target();
    let selection = ReleaseSelection {
        version: SemVersion::parse("9.0.0").expect("target version"),
        archive_name: format!("haider-v9.0.0-{target}.tar.xz"),
        archive_url: format!("http://{address}/archive"),
        checksum_name: format!("haider-v9.0.0-{target}.tar.xz.sha256"),
        checksum_url: format!("http://{address}/checksum"),
    };
    let mut transport = CurlTransport::without_token();
    assert!(
        stage_then_acquire(
            &mut transport,
            &SystemStageVerifier,
            fixture.layout.clone(),
            &selection,
        )
        .is_err()
    );
    server.join().expect("truncated archive server");
    assert_eq!(pair_snapshot(&fixture.layout), before);
    assert!(!marker_path(&fixture.layout).exists());
    assert!(
        old_child
            .try_wait()
            .expect("poll unchanged daemon")
            .is_none()
    );
    let still_running = wait_ready(&fixture.profile).await;
    assert_eq!(still_running.welcome.instance_id, old_instance);
    assert_eq!(still_running.welcome.daemon_generation, old_generation);
    stop_connected(still_running, &fixture.profile).await;
    let old_status = wait_child(&mut old_child).await;
    assert!(
        old_status.success(),
        "unchanged daemon did not stop cleanly ({old_status}); {}",
        old_child.evidence()
    );
}

/// MUTATION CHECK: signal twice, skip matching drain/lock release, discard
/// the spawned child before health, accept a stale generation, or weaken the
/// exact Welcome version check. Expected RUNTIME failure: the old child is
/// forced/nonzero, new identity does not advance, or restart returns error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_daemon_drains_once_releases_lock_and_restarts_exact_version() {
    let fixture = RestartFixture::new();
    let mut old_child = fixture.spawn_installed();
    let Some(old) = wait_initial_ready(&fixture.profile, &mut old_child).await else {
        return;
    };
    let old_instance = old.welcome.instance_id.clone();
    let old_generation = old.welcome.daemon_generation;
    let _ = old.client.close();

    let prepared = PreparedTransaction::acquire(fixture.layout.clone()).expect("prepare update");
    let pair = verified_pair_for_test(
        &fixture.layout.dir,
        &fixture.source_haider,
        &fixture.source_haiderd,
        env!("CARGO_PKG_VERSION"),
    )
    .expect("verified current-version fixture");
    let incumbent = detect_incumbent(&fixture.profile)
        .await
        .expect("authenticate incumbent")
        .expect("incumbent running");
    let mut committed = commit_pair(
        prepared,
        pair,
        &NoFaults,
        &DigestVerifier,
        env!("CARGO_PKG_VERSION"),
    )
    .expect("commit pair");
    restart_committed(&mut committed, Some(incumbent), &fixture.profile)
        .await
        .expect("restart exact target");

    let old_status = wait_child(&mut old_child).await;
    assert!(
        old_status.success(),
        "one SIGTERM must produce graceful exit, not forced 130: {old_status}; {}",
        old_child.evidence()
    );
    let new = wait_ready(&fixture.profile).await;
    assert_eq!(new.welcome.lifecycle_phase, LifecyclePhase::Ready);
    assert_eq!(new.welcome.profile_id, fixture.profile.profile_id);
    assert_eq!(new.welcome.daemon_version, env!("CARGO_PKG_VERSION"));
    assert_ne!(new.welcome.instance_id, old_instance);
    assert!(new.welcome.daemon_generation > old_generation);
    assert!(
        new.welcome
            .features
            .is_superset(&haider_client::required_live_features())
    );
    stop_connected(new, &fixture.profile).await;
}

/// MUTATION CHECK: make a missing daemon enter auto-spawn during update.
/// Expected RUNTIME failure: an endpoint appears after a successful pair
/// commit even though no incumbent existed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_daemon_case_commits_and_remains_stopped() {
    let fixture = RestartFixture::new();
    assert!(!fixture.profile.endpoint_path.exists());
    let prepared = PreparedTransaction::acquire(fixture.layout.clone()).expect("prepare update");
    let pair = verified_pair_for_test(
        &fixture.layout.dir,
        &fixture.source_haider,
        &fixture.source_haiderd,
        env!("CARGO_PKG_VERSION"),
    )
    .expect("verified fixture");
    let mut committed = commit_pair(
        prepared,
        pair,
        &NoFaults,
        &DigestVerifier,
        env!("CARGO_PKG_VERSION"),
    )
    .expect("commit pair");
    restart_committed(&mut committed, None, &fixture.profile)
        .await
        .expect("no-daemon completion");
    assert!(!fixture.profile.endpoint_path.exists());
}

/// MUTATION CHECK: accept a non-exact Welcome version, fail to stop the bad
/// retained child, restore only one binary, or fail to restart the old
/// sibling. Expected RUNTIME failure: result is success, marker survives,
/// canonical inode pair differs, or no old-version daemon serves afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_version_mismatch_stops_child_rolls_back_pair_and_restarts_old() {
    let fixture = RestartFixture::new();
    let old_pair = pair_snapshot(&fixture.layout);
    let mut old_child = fixture.spawn_installed();
    let Some(old) = wait_initial_ready(&fixture.profile, &mut old_child).await else {
        return;
    };
    let _ = old.client.close();

    let prepared = PreparedTransaction::acquire(fixture.layout.clone()).expect("prepare update");
    let pair = verified_pair_for_test(
        &fixture.layout.dir,
        &fixture.source_haider,
        &fixture.source_haiderd,
        "9.0.0",
    )
    .expect("verified wrong-version fixture");
    let incumbent = detect_incumbent(&fixture.profile)
        .await
        .expect("authenticate incumbent")
        .expect("incumbent running");
    let mut committed = commit_pair(
        prepared,
        pair,
        &NoFaults,
        &DigestVerifier,
        env!("CARGO_PKG_VERSION"),
    )
    .expect("commit wrong target marker");
    let error = restart_committed(&mut committed, Some(incumbent), &fixture.profile)
        .await
        .expect_err("exact Welcome mismatch must roll back");
    assert!(matches!(error, UpdateError::Health(_)));
    let old_status = wait_child(&mut old_child).await;
    assert!(
        old_status.success(),
        "old daemon did not drain cleanly ({old_status}); {}",
        old_child.evidence()
    );
    assert_eq!(pair_snapshot(&fixture.layout), old_pair);
    assert!(!marker_path(&fixture.layout).exists());
    let restarted_old = wait_ready(&fixture.profile).await;
    assert_eq!(
        restarted_old.welcome.daemon_version,
        env!("CARGO_PKG_VERSION")
    );
    stop_connected(restarted_old, &fixture.profile).await;
}

struct RestartFixture {
    _root: tempfile::TempDir,
    layout: InstallLayout,
    profile: ResolvedProfile,
    source_haider: PathBuf,
    source_haiderd: PathBuf,
}

impl RestartFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("restart fixture root");
        let install = root.path().join("install");
        fs::create_dir(&install).expect("install directory");
        fs::set_permissions(&install, fs::Permissions::from_mode(0o700)).expect("chmod install");
        let source_haider = PathBuf::from(env!("CARGO_BIN_EXE_haider"));
        let source_haiderd = ensure_haiderd_built();
        let layout = InstallLayout::for_test(install);
        copy_executable(&source_haider, &layout.haider);
        copy_executable(&source_haiderd, &layout.haiderd);
        let profile = haider_client::resolve_profile(&haider_client::ProfileEnv {
            profile_dir: Some(root.path().join("profile")),
            home: None,
            model: None,
            runtime_dir: None,
            xdg_runtime_dir: None,
        })
        .expect("resolve update profile");
        Self {
            _root: root,
            layout,
            profile,
            source_haider,
            source_haiderd,
        }
    }

    fn spawn_installed(&self) -> CapturedDaemon {
        CapturedDaemon::spawn(&self.profile, &self.layout.haiderd)
    }
}

struct CapturedDaemon {
    child: Child,
    log_path: PathBuf,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stderr_completion: Option<mpsc::Receiver<()>>,
    stderr_reader_note: Option<&'static str>,
}

impl CapturedDaemon {
    fn spawn(profile: &ResolvedProfile, binary: &Path) -> Self {
        let log_path = haider_platform::allocate_daemon_log_path(&profile.store_dir)
            .expect("allocate installed daemon log");
        let mut child =
            haider_platform::spawn_daemon_with_piped_stderr(haider_platform::DaemonSpawn {
                binary,
                profile_id: &profile.profile_id,
                store_dir: &profile.store_dir,
                runtime_dir: &profile.runtime_dir,
                log_path: &log_path,
            })
            .expect("spawn installed daemon");
        let stderr = child.stderr.take().expect("installed daemon stderr pipe");
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let reader_tail = Arc::clone(&stderr_tail);
        let reader_log_path = log_path.clone();
        let (completion_sender, stderr_completion) = mpsc::sync_channel(1);
        let stderr_reader = std::thread::Builder::new()
            .name("haiderd-stderr-capture".into())
            .spawn(move || {
                capture_stderr(stderr, &reader_log_path, &reader_tail);
                let _ = completion_sender.send(());
            })
            .expect("spawn installed daemon stderr reader");
        // Completion is observed through the bounded channel below. Detaching
        // the handle keeps a descendant that inherited fd 2 from defeating the
        // test's deadline by holding this reader open forever.
        drop(stderr_reader);
        Self {
            child,
            log_path,
            stderr_tail,
            stderr_completion: Some(stderr_completion),
            stderr_reader_note: None,
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    fn finish_stderr(&mut self) {
        let Some(completion) = self.stderr_completion.take() else {
            return;
        };
        self.stderr_reader_note = match completion.recv_timeout(STDERR_FINISH_GRACE) {
            Ok(()) => None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Some("<stderr reader still open after daemon exit; tail is a bounded snapshot>")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Some("<stderr reader stopped before reporting completion>")
            }
        };
    }

    fn evidence(&self) -> String {
        daemon_failure_evidence(&self.log_path, &self.stderr_tail, self.stderr_reader_note)
    }
}

fn capture_stderr(mut stderr: ChildStderr, log_path: &Path, tail: &Mutex<VecDeque<String>>) {
    let mut log = OpenOptions::new().append(true).open(log_path).ok();
    let mut buffer = [0_u8; 8192];
    let mut line = Vec::new();
    let mut line_truncated = false;
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if let Some(log) = log.as_mut() {
                    let _ = log.write_all(&buffer[..read]);
                }
                for &byte in &buffer[..read] {
                    if byte == b'\n' {
                        retain_stderr_line(tail, &mut line, line_truncated);
                        line_truncated = false;
                    } else if line.len() < STDERR_LINE_BYTES {
                        line.push(byte);
                    } else {
                        line_truncated = true;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    if !line.is_empty() || line_truncated {
        retain_stderr_line(tail, &mut line, line_truncated);
    }
    if let Some(log) = log.as_mut() {
        let _ = log.flush();
    }
}

fn retain_stderr_line(tail: &Mutex<VecDeque<String>>, line: &mut Vec<u8>, line_truncated: bool) {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let mut rendered = String::from_utf8_lossy(line).into_owned();
    if line_truncated {
        rendered.push_str("<line truncated>");
    }
    line.clear();
    let mut tail = tail.lock().expect("stderr tail mutex");
    if tail.len() == STDERR_TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(rendered);
}

fn daemon_failure_evidence(
    log_path: &Path,
    stderr_tail: &Mutex<VecDeque<String>>,
    stderr_reader_note: Option<&str>,
) -> String {
    let log = read_daemon_log(log_path);
    let stderr = {
        let tail = stderr_tail.lock().expect("stderr tail mutex");
        if tail.is_empty() {
            "<no stderr captured>".to_owned()
        } else {
            tail.iter().cloned().collect::<Vec<_>>().join("\n")
        }
    };
    let reader_note = stderr_reader_note
        .map(|note| format!("\n{note}"))
        .unwrap_or_default();
    format!(
        "daemon process log:\n{log}\ndaemon stderr tail (last {STDERR_TAIL_LINES} lines):\n{stderr}{reader_note}"
    )
}

fn read_daemon_log(log_path: &Path) -> String {
    match fs::read_to_string(log_path) {
        Ok(log) => log,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => format!(
            "<daemon log is absent: {error}; containing directory entries (names only): {}>",
            directory_entry_names(log_path.parent())
        ),
        Err(error) => format!("<cannot read daemon log: {error}>"),
    }
}

fn directory_entry_names(directory: Option<&Path>) -> String {
    let Some(directory) = directory else {
        return "<no parent directory>".into();
    };
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => return format!("<cannot list directory: {error}>"),
    };
    let mut names = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    if names.is_empty() {
        "<empty>".into()
    } else {
        names.join(", ")
    }
}

fn ensure_haiderd_built() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let sibling = PathBuf::from(env!("CARGO_BIN_EXE_haider"))
            .parent()
            .expect("binary parent")
            .join("haiderd");
        if !sibling.exists() {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = Command::new(cargo)
                .args([
                    "build",
                    "--locked",
                    "-p",
                    "haider-daemond",
                    "--bin",
                    "haiderd",
                ])
                .env("CARGO_INCREMENTAL", "0")
                .status()
                .expect("build haiderd");
            assert!(status.success(), "haiderd build failed");
        }
        sibling
    })
    .clone()
}

fn copy_executable(source: &Path, destination: &Path) {
    fs::copy(source, destination).expect("copy executable fixture");
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
        .expect("chmod executable fixture");
}

async fn wait_ready(profile: &ResolvedProfile) -> haider_client::Connected {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match connect(&profile.endpoint_path, ClientConfig::default()).await {
            Ok(connected) if connected.welcome.lifecycle_phase == LifecyclePhase::Ready => {
                return connected;
            }
            Ok(connected) => {
                let _ = connected.client.close();
            }
            Err(error) if error.is_spawnable() => {}
            Err(error) => panic!("daemon health connect failed: {error}"),
        }
        if Instant::now() + POLL > deadline {
            let log = read_daemon_log(&profile.store_dir.join("daemon.log"));
            panic!("daemon readiness timeout; daemon.log:\n{log}");
        }
        sleep(POLL).await;
    }
}

async fn wait_initial_ready(
    profile: &ResolvedProfile,
    child: &mut CapturedDaemon,
) -> Option<haider_client::Connected> {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match connect(&profile.endpoint_path, ClientConfig::default()).await {
            Ok(connected) if connected.welcome.lifecycle_phase == LifecyclePhase::Ready => {
                return Some(connected);
            }
            Ok(connected) => {
                let _ = connected.client.close();
            }
            Err(error) if error.is_spawnable() => {}
            Err(error) => panic!("daemon health connect failed: {error}"),
        }
        if let Some(status) = child.try_wait().expect("poll initial daemon") {
            child.finish_stderr();
            let evidence = child.evidence();
            if evidence.contains("Operation not permitted") {
                // Hermetic product sandboxes can deny a spawned subprocess
                // the Unix-listener syscall. Ordinary macOS CI executes the
                // full acceptance; keep this as a sandbox skip, not a fake.
                return None;
            }
            panic!("initial daemon exited {status}; {evidence}");
        }
        assert!(
            Instant::now() + POLL <= deadline,
            "initial daemon readiness timeout"
        );
        sleep(POLL).await;
    }
}

async fn wait_child(child: &mut CapturedDaemon) -> std::process::ExitStatus {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            child.finish_stderr();
            return status;
        }
        assert!(Instant::now() + POLL <= deadline, "child exit timeout");
        sleep(POLL).await;
    }
}

#[test]
fn absent_daemon_log_evidence_lists_names_and_captured_bounded_stderr_tail() {
    let directory = tempfile::tempdir().expect("missing-log evidence directory");
    let script = directory.path().join("stderr-fixture");
    write_executable_bytes(
        &script,
        b"#!/bin/sh\nindex=0\nwhile [ \"$index\" -lt 81 ]; do\n  printf 'stderr-marker-%03d\\n' \"$index\" >&2\n  index=$((index + 1))\ndone\nprintf 'stderr-marker-081' >&2\nexit 70\n",
    );
    let profile = resolved_test_profile(directory.path().join("profile"));
    let mut daemon = CapturedDaemon::spawn(&profile, &script);
    let status = daemon.child.wait().expect("wait stderr fixture");
    daemon.finish_stderr();
    assert_eq!(status.code(), Some(70));

    fs::remove_file(&daemon.log_path).expect("force missing daemon log evidence");
    let log_directory = daemon.log_path.parent().expect("daemon log parent");
    fs::write(log_directory.join("present.log"), "DO_NOT_RENDER").expect("write evidence sibling");
    fs::create_dir(log_directory.join("runtime")).expect("create evidence sibling directory");

    let evidence = daemon.evidence();

    assert!(evidence.contains("daemon log is absent"));
    assert!(evidence.contains("containing directory entries (names only): present.log, runtime"));
    assert!(evidence.contains("stderr tail (last 80 lines)"));
    assert!(!evidence.contains("stderr-marker-000"));
    assert!(!evidence.contains("stderr-marker-001"));
    assert!(evidence.contains("stderr-marker-002"));
    assert!(evidence.contains("stderr-marker-081"));
    assert!(!evidence.contains("stderr reader still open"));
    assert!(!evidence.contains("DO_NOT_RENDER"));
}

async fn stop_connected(connected: haider_client::Connected, profile: &ResolvedProfile) {
    let pid = connected
        .peer_credentials
        .pid
        .expect("authenticated daemon PID");
    signal_authenticated_peer(pid).expect("stop fixture daemon once");
    let _ = tokio::time::timeout(DEADLINE, connected.client.disconnected())
        .await
        .expect("fixture daemon disconnect");
    let deadline = Instant::now() + DEADLINE;
    loop {
        match Store::acquire_profile(&profile.store_dir) {
            Ok(lease) => {
                drop(lease);
                break;
            }
            Err(error) if error.code == haider_protocol::error::ErrorCode::StoreLocked => {}
            Err(error) => panic!("fixture lock proof failed: {}", error.message),
        }
        assert!(Instant::now() + POLL <= deadline, "cleanup timeout");
        sleep(POLL).await;
    }
}

fn install_fixture_bytes() -> tempfile::TempDir {
    let install = tempfile::tempdir().expect("byte install fixture");
    write_executable_bytes(&install.path().join("haider"), b"old-cli");
    write_executable_bytes(&install.path().join("haiderd"), b"old-daemon");
    install
}

fn verified_pair_bytes(install: &Path) -> cli_main::update::staging::VerifiedStagedPair {
    let source_cli = install.join("source-haider");
    let source_daemon = install.join("source-haiderd");
    write_executable_bytes(&source_cli, b"new-cli");
    write_executable_bytes(&source_daemon, b"new-daemon");
    verified_pair_for_test(install, &source_cli, &source_daemon, "9.0.0")
        .expect("verified byte pair")
}

fn write_executable_bytes(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write executable bytes");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("chmod executable bytes");
}

fn resolved_test_profile(path: PathBuf) -> ResolvedProfile {
    haider_client::resolve_profile(&haider_client::ProfileEnv {
        profile_dir: Some(path),
        home: None,
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve test profile")
}

async fn fake_non_draining_incumbent(
    profile: &ResolvedProfile,
) -> Option<(
    tokio::task::JoinHandle<()>,
    cli_main::update::restart::Incumbent,
)> {
    fs::create_dir_all(&profile.runtime_dir).expect("create fake runtime directory");
    fs::set_permissions(&profile.runtime_dir, fs::Permissions::from_mode(0o700))
        .expect("chmod fake runtime directory");
    let listener = match UnixListener::bind(&profile.endpoint_path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("bind fake daemon endpoint: {error}"),
    };
    let welcome = Welcome {
        protocol: WIRE_PROTOCOL_VERSION,
        instance_id: "timeout-incumbent".into(),
        daemon_generation: 7,
        frame_limit: 1024 * 1024,
        profile_id: profile.profile_id.clone(),
        daemon_version: "1.0.0".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features: haider_client::required_live_features(),
        user_command_withheld: false,
        encoding: None,
    };
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept fake incumbent");
        let mut decoder = uds_codec::Decoder::new(1024 * 1024);
        let frames = read_fake_frames(&mut stream, &mut decoder).await;
        assert!(matches!(frames.first(), Some(WireFrame::Hello(_))));
        let encoded = uds_codec::encode(&WireFrame::Welcome(welcome), 1024 * 1024)
            .expect("encode fake Welcome");
        stream
            .write_all(&encoded)
            .await
            .expect("write fake Welcome");
        std::future::pending::<()>().await;
    });
    let incumbent = detect_incumbent(profile)
        .await
        .expect("authenticate fake incumbent")
        .expect("fake incumbent exists");
    Some((server, incumbent))
}

async fn read_fake_frames(
    stream: &mut UnixStream,
    decoder: &mut uds_codec::Decoder,
) -> Vec<WireFrame> {
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.expect("read fake Hello");
        assert_ne!(read, 0, "fake client closed before Hello");
        let batch = decoder.push(&buffer[..read]);
        assert!(
            batch.error.is_none(),
            "decode fake Hello: {:?}",
            batch.error
        );
        if !batch.frames.is_empty() {
            return batch.frames;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PairSnapshot {
    cli: (Vec<u8>, u32, u64),
    daemon: (Vec<u8>, u32, u64),
}

fn pair_snapshot(layout: &InstallLayout) -> PairSnapshot {
    PairSnapshot {
        cli: file_snapshot(&layout.haider),
        daemon: file_snapshot(&layout.haiderd),
    }
}

fn file_snapshot(path: &Path) -> (Vec<u8>, u32, u64) {
    let metadata = fs::metadata(path).expect("pair metadata");
    (
        fs::read(path).expect("pair bytes"),
        metadata.mode(),
        metadata.ino(),
    )
}
