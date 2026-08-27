#![allow(clippy::expect_used)]
//! Black-box contracts for ephemeral launcher liveness and per-profile runtime
//! isolation. The helper is a real client process; `Child::kill` is SIGKILL on
//! Unix and TerminateProcess on Windows.

use haider_client::{
    ClientConfig, DAEMON_LOG_FILE, DaemonLifetime, EnsureOptions, ProfileEnv, ResolvedProfile,
    connect, ensure_daemon, resolve_profile,
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const HELPER_ENV: &str = "HAIDER_EPHEMERAL_LIVENESS_HELPER";
const HELPER_MARKER_ENV: &str = "HAIDER_EPHEMERAL_LIVENESS_MARKER";
const HELPER_DAEMON_ENV: &str = "HAIDER_EPHEMERAL_LIVENESS_DAEMON";
const HELPER_KILL_BEFORE_READY_ENV: &str = "HAIDER_EPHEMERAL_KILL_BEFORE_READY";
const DEADLINE: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(20);

struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn kill_and_wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.kill()?;
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn process_test_guard() -> MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
}

fn profile(store: &Path, runtime_root: &Path) -> ResolvedProfile {
    resolve_profile(&ProfileEnv {
        profile_dir: Some(store.to_path_buf()),
        home: None,
        model: None,
        runtime_dir: Some(runtime_root.to_path_buf()),
        xdg_runtime_dir: None,
    })
    .expect("resolve isolated profile")
}

fn pid_path(profile: &ResolvedProfile) -> PathBuf {
    profile.runtime_dir.join(haider_daemon::DAEMON_PID_FILE)
}

fn wait_until(mut predicate: impl FnMut() -> bool, boundary: &str) {
    let deadline = Instant::now() + DEADLINE;
    while !predicate() {
        assert!(Instant::now() < deadline, "deadline waiting for {boundary}");
        std::thread::sleep(POLL);
    }
}

fn wait_for_helper(marker: &Path, child: &mut ChildGuard, boundary: &str) {
    wait_until(
        || {
            assert!(
                child.child_mut().try_wait().expect("poll helper").is_none(),
                "helper exited before {boundary}"
            );
            marker.exists()
        },
        boundary,
    );
}

fn spawn_helper(
    profile: &ResolvedProfile,
    marker: &Path,
    kill_before_readiness: bool,
) -> ChildGuard {
    let runtime_root = profile
        .runtime_dir
        .parent()
        .expect("runtime has profile-scoped parent");
    let mut command =
        Command::new(std::env::current_exe().expect("locate integration test executable"));
    command
        .args(["--exact", "ephemeral_client_process_helper", "--nocapture"])
        .env(HELPER_ENV, "1")
        .env(HELPER_MARKER_ENV, marker)
        .env(HELPER_DAEMON_ENV, env!("CARGO_BIN_EXE_haiderd"))
        .env("HAIDER_PROFILE_DIR", &profile.store_dir)
        .env("HAIDER_RUNTIME_DIR", runtime_root)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if kill_before_readiness {
        command.env(HELPER_KILL_BEFORE_READY_ENV, "1");
    }
    let child = command.spawn().expect("spawn ephemeral client helper");
    ChildGuard::new(child)
}

fn kill_helper(child: &mut ChildGuard) {
    let status = child
        .kill_and_wait()
        .expect("SIGKILL/TerminateProcess and reap helper");
    assert!(
        !status.success(),
        "killed helper must not exit successfully"
    );
}

fn wait_for_cleanup(profile: &ResolvedProfile) {
    wait_until(
        || !profile.runtime_dir.exists() && !pid_path(profile).exists(),
        "ephemeral daemon runtime cleanup",
    );
    let runtime = runtime();
    assert!(
        runtime
            .block_on(connect(&profile.endpoint_path, ClientConfig::default()))
            .is_err(),
        "cleaned ephemeral endpoint must no longer serve"
    );
}

#[cfg(unix)]
fn process_has_exited(pid: u32) -> bool {
    let raw = i32::try_from(pid).expect("daemon pid fits Unix pid_t");
    let pid = rustix::process::Pid::from_raw(raw).expect("daemon pid is non-zero");
    match rustix::process::test_kill_process(pid) {
        Ok(()) => false,
        Err(error) if error == rustix::io::Errno::SRCH => true,
        Err(_) => false,
    }
}

#[cfg(windows)]
fn process_has_exited(pid: u32) -> bool {
    let Some(pid) = haider_platform::process_id(Some(pid)) else {
        return true;
    };
    match haider_platform::process_leader_exited(pid) {
        Ok(exited) => exited,
        Err(error) if haider_platform::process_error_is_missing(&error) => true,
        Err(_) => false,
    }
}

fn spawn_persistent_daemon(profile: &ResolvedProfile) -> ChildGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_haiderd"))
        .arg("--profile")
        .arg(&profile.profile_id)
        .arg("--store-dir")
        .arg(&profile.store_dir)
        .arg("--runtime-dir")
        .arg(&profile.runtime_dir)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn persistent daemon");
    ChildGuard::new(child)
}

async fn connect_until_ready(profile: &ResolvedProfile) -> haider_client::Connected {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(connected) = connect(&profile.endpoint_path, ClientConfig::default()).await {
                return connected;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("daemon readiness deadline")
}

/// Runs only in the subprocess launched by `spawn_helper`. The retained
/// ownership token holds the client's liveness end for the entire process.
#[test]
fn ephemeral_client_process_helper() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    let marker =
        PathBuf::from(std::env::var_os(HELPER_MARKER_ENV).expect("helper marker environment"));
    let daemon =
        PathBuf::from(std::env::var_os(HELPER_DAEMON_ENV).expect("helper daemon environment"));
    let profile = resolve_profile(&ProfileEnv::capture()).expect("helper profile");
    runtime().block_on(async move {
        if std::env::var_os(HELPER_KILL_BEFORE_READY_ENV).is_some() {
            let log_path = haider_platform::allocate_daemon_log_path(&profile.store_dir)
                .expect("allocate early-kill daemon log");
            let (spawned, liveness) = haider_platform::spawn_daemon_with_readiness_and_liveness(
                haider_platform::DaemonSpawn {
                    binary: &daemon,
                    profile_id: &profile.profile_id,
                    store_dir: &profile.store_dir,
                    runtime_dir: &profile.runtime_dir,
                    log_path: &log_path,
                },
            )
            .expect("spawn daemon without consuming readiness");
            std::fs::write(
                &marker,
                format!("daemon_pid={}\n", spawned.child.id()).as_bytes(),
            )
            .expect("publish unready helper marker");
            let retained = (spawned, liveness);
            std::future::pending::<()>().await;
            drop(retained);
            return;
        }
        let ensured = ensure_daemon(
            &profile,
            EnsureOptions {
                daemon_binary: Some(daemon),
                daemon_lifetime: DaemonLifetime::EphemeralIfSpawned,
                startup_deadline: DEADLINE,
                ..EnsureOptions::default()
            },
        )
        .await
        .expect("helper connect-or-spawn");
        std::fs::write(&marker, format!("spawned={}\n", ensured.spawned).as_bytes())
            .expect("publish helper readiness");
        std::future::pending::<()>().await;
        drop(ensured);
    });
}

/// S3(a): even a kill before the daemon has consumed any user traffic leaves
/// only a buffered EOF proof; the daemon still exits and removes its files.
#[test]
fn killed_spawning_client_reaps_ephemeral_daemon_and_runtime_files() {
    let _guard = process_test_guard();
    let root = tempfile::tempdir().expect("test root");
    let profile = profile(&root.path().join("store"), &root.path().join("runtime"));
    let marker = root.path().join("client-ready");
    let mut helper = spawn_helper(&profile, &marker, true);
    wait_for_helper(&marker, &mut helper, "daemon spawn before readiness read");
    let marker = std::fs::read_to_string(&marker).expect("read helper marker");
    let daemon_pid = marker
        .strip_prefix("daemon_pid=")
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .expect("helper marker carries daemon pid");

    kill_helper(&mut helper);
    wait_until(
        || process_has_exited(daemon_pid),
        "ephemeral daemon exit after pre-readiness client kill",
    );
    wait_for_cleanup(&profile);
}

/// S3(a): once readiness has proved the endpoint and PID file existed, killing
/// the sole spawning client still makes the daemon exit and remove both.
#[test]
fn killed_ready_spawning_client_reaps_ephemeral_daemon_and_runtime_files() {
    let _guard = process_test_guard();
    let root = tempfile::tempdir().expect("test root");
    let profile = profile(&root.path().join("store"), &root.path().join("runtime"));
    let marker = root.path().join("client-ready");
    let mut helper = spawn_helper(&profile, &marker, false);
    wait_for_helper(&marker, &mut helper, "ready ephemeral helper");
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read helper marker"),
        "spawned=true\n"
    );
    assert!(profile.runtime_dir.is_dir(), "runtime existed at Ready");
    let daemon_pid = std::fs::read_to_string(pid_path(&profile))
        .expect("PID file existed at Ready")
        .trim()
        .parse::<u32>()
        .expect("PID file carries daemon process id");
    let runtime = runtime();
    let probe = runtime.block_on(connect_until_ready(&profile));
    probe.client.close();
    drop(probe);

    kill_helper(&mut helper);
    wait_until(
        || process_has_exited(daemon_pid),
        "ready ephemeral daemon exit after client kill",
    );
    wait_for_cleanup(&profile);
}

/// S3(b): an attaching client receives no liveness authority over a daemon it
/// did not spawn, so killing that client cannot stop the incumbent.
#[test]
fn killed_attached_client_does_not_stop_persistent_daemon() {
    let _guard = process_test_guard();
    let root = tempfile::tempdir().expect("test root");
    let profile = profile(&root.path().join("store"), &root.path().join("runtime"));
    let mut daemon = spawn_persistent_daemon(&profile);
    let runtime = runtime();
    let warmup = runtime.block_on(connect_until_ready(&profile));
    warmup.client.close();
    drop(warmup);

    let marker = root.path().join("attached-ready");
    let mut helper = spawn_helper(&profile, &marker, false);
    wait_for_helper(&marker, &mut helper, "attached helper readiness");
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read helper marker"),
        "spawned=false\n"
    );
    kill_helper(&mut helper);

    let still_serving = runtime.block_on(connect_until_ready(&profile));
    assert!(
        daemon
            .child_mut()
            .try_wait()
            .expect("poll persistent daemon")
            .is_none(),
        "attached client death must not stop the incumbent"
    );
    still_serving.client.close();
    drop(still_serving);
    let _ = daemon
        .kill_and_wait()
        .expect("terminate and reap persistent daemon after assertion");
}

/// S3(c): launcher death arms idle shutdown, but a second live connection
/// keeps the daemon serving until that connection closes.
#[test]
fn second_client_holds_ephemeral_daemon_until_its_disconnect() {
    let _guard = process_test_guard();
    let root = tempfile::tempdir().expect("test root");
    let profile = profile(&root.path().join("store"), &root.path().join("runtime"));
    let marker = root.path().join("client-ready");
    let mut helper = spawn_helper(&profile, &marker, false);
    wait_for_helper(&marker, &mut helper, "ephemeral helper readiness");
    let runtime = runtime();
    let second = runtime.block_on(connect_until_ready(&profile));

    kill_helper(&mut helper);
    let daemon_log = profile.store_dir.join(DAEMON_LOG_FILE);
    wait_until(
        || {
            std::fs::read_to_string(&daemon_log).is_ok_and(|contents| {
                contents.contains(
                    "spawning client vanished; ephemeral daemon is waiting for live clients",
                )
            })
        },
        "launcher liveness observation with a second live client",
    );
    assert!(
        pid_path(&profile).exists(),
        "second client keeps daemon alive"
    );
    let probe = runtime.block_on(connect_until_ready(&profile));
    probe.client.close();
    drop(probe);

    second.client.close();
    drop(second);
    wait_for_cleanup(&profile);
}

/// S3(d): one runtime root may contain many profiles, but every writable
/// daemon artifact remains under the matching profile child.
#[test]
fn two_profiles_have_disjoint_runtime_trees_and_writes() {
    let _guard = process_test_guard();
    let root = tempfile::tempdir().expect("test root");
    let runtime_root = root.path().join("runtime");
    let first = profile(&root.path().join("store-a"), &runtime_root);
    let second = profile(&root.path().join("store-b"), &runtime_root);
    assert_ne!(first.runtime_dir, second.runtime_dir);

    let first_marker = root.path().join("first-ready");
    let second_marker = root.path().join("second-ready");
    let mut first_helper = spawn_helper(&first, &first_marker, false);
    let mut second_helper = spawn_helper(&second, &second_marker, false);
    wait_for_helper(&first_marker, &mut first_helper, "first profile readiness");
    wait_for_helper(
        &second_marker,
        &mut second_helper,
        "second profile readiness",
    );

    for profile in [&first, &second] {
        #[cfg(unix)]
        assert!(profile.endpoint_path.starts_with(&profile.runtime_dir));
        #[cfg(windows)]
        assert!(
            profile
                .endpoint_path
                .to_string_lossy()
                .starts_with(r"\\.\pipe\haider-")
        );
        assert!(pid_path(profile).starts_with(&profile.runtime_dir));
        assert!(profile.runtime_dir.join("tmp").is_dir());
        assert!(pid_path(profile).is_file());
    }
    let actual = std::fs::read_dir(&runtime_root)
        .expect("list runtime root")
        .map(|entry| entry.expect("read runtime entry").file_name())
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [&first, &second]
        .into_iter()
        .map(|profile| {
            profile
                .runtime_dir
                .file_name()
                .expect("profile runtime basename")
                .to_os_string()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actual, expected, "runtime root has no shared writes");

    kill_helper(&mut first_helper);
    kill_helper(&mut second_helper);
    wait_for_cleanup(&first);
    wait_for_cleanup(&second);
}
