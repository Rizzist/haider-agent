#![cfg(unix)]
//! Bare-`haider` auto-spawn acceptance over REAL subprocess binaries
//! (report §6.2 tests): the concurrent-launch race, stale-socket recovery by
//! the winner only, and parent-exit-leaves-daemon.
//!
//! Each test isolates its profile in a fresh store directory (`profile_id`
//! is store-path-derived, so endpoints never collide) and kills its daemon
//! through a drop guard, so no test leaks a process past its assertions.
#![allow(clippy::expect_used)]

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use haider_client::{ProfileEnv, ResolvedProfile, resolve_profile};

fn haider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_haider"))
}

/// The sibling `haiderd` next to the `haider` under test — fingerprint-checked
/// on Linux and built on demand elsewhere when this test crate runs alone.
fn ensure_haiderd_built() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let sibling = haider_binary()
        .parent()
        .expect("haider binary has a parent directory")
        .join("haiderd");
    // A persistent Linux target directory can contain a sibling built from
    // older sources even though this package's test artifacts are current.
    // Enter Cargo's fingerprint/build lock once on Linux instead of trusting
    // existence alone; this also prevents another integration-test process
    // from replacing `haiderd` while this suite starts it. Non-Linux retains
    // the historical existence-only behavior.
    if cfg!(target_os = "linux") || !sibling.exists() {
        BUILD.call_once(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut command = Command::new(cargo);
            command.arg("build");
            #[cfg(target_os = "linux")]
            command.arg("--locked");
            let status = command
                .args(["-p", "haider-daemond", "--bin", "haiderd"])
                .status()
                .expect("build haiderd for auto-spawn tests");
            assert!(status.success(), "haiderd build failed");
        });
    }
    assert!(
        sibling.exists(),
        "haiderd sibling missing at {}",
        sibling.display()
    );
    warm_autospawn_binaries(&haider_binary(), &sibling);
    sibling
}

fn warm_autospawn_binaries(haider: &Path, haiderd: &Path) {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        // macOS validates each newly written Mach-O inode before entering
        // `main`: measured cold launches were 4.975 s and 4.79 s, while the
        // same inodes then launched in 0.23 s, 0.20 s, and 0.24 s. Charge
        // that one-time validation to this process fixture, not the 950 ms
        // own-child authentication assertion. Both binaries handle
        // `--version` before profile/store/runtime-directory/socket setup, so warming
        // cannot create a daemon or mutate a test profile.
        for (name, binary) in [("haider", haider), ("haiderd", haiderd)] {
            let status = Command::new(binary)
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap_or_else(|error| panic!("warm {name} binary: {error}"));
            assert!(
                status.success(),
                "{name} --version warm-up failed: {status}"
            );
        }
    });
}

fn resolved_for(store: &Path) -> ResolvedProfile {
    resolve_profile(&ProfileEnv {
        profile_dir: Some(store.to_path_buf()),
        home: None,
        user_profile: None,
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve test profile")
}

fn haider_command(store: &Path) -> Command {
    let mut command = Command::new(haider_binary());
    configure_test_home(&mut command, store);
    command
        .env("HAIDER_PROFILE_DIR", store)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        // The harness resolves its expected endpoint with
        // `xdg_runtime_dir: None`; the CHILD must agree. On CI Linux the
        // runner exports XDG_RUNTIME_DIR=/run/user/NNN, so an inheriting
        // daemon binds there while the test watches the fallback path — the
        // round-4 daemon.log finally named this split.
        .env_remove("XDG_RUNTIME_DIR")
        // `resolved_for` deliberately supplies no runtime override, so an
        // ambient harness override must not make the child resolve a
        // different root from the parent-side expected profile.
        .env_remove("HAIDER_RUNTIME_DIR")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

fn configure_test_home(command: &mut Command, store: &Path) {
    let home = store.join("machine-home");
    std::fs::create_dir_all(&home).expect("create isolated machine-user home");
    command.env("HOME", &home).env("USERPROFILE", home);
}

const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Waits for a captured child without allowing a broken candidate or launcher
/// to consume the test runner forever. Every caller supplies the operation it
/// expects to finish so timeout failures identify the stuck boundary.
fn wait_for_output(child: Child, waiting_for: &str) -> Output {
    wait_for_output_with_timeout(child, waiting_for, CHILD_EXIT_TIMEOUT)
}

fn wait_for_output_with_timeout(mut child: Child, waiting_for: &str, timeout: Duration) -> Output {
    #[derive(Clone, Copy)]
    enum CapturedStream {
        Stdout,
        Stderr,
    }

    let (sender, receiver) = mpsc::channel();
    let mut stream_count = 0;
    if let Some(mut stdout) = child.stdout.take() {
        stream_count += 1;
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send((CapturedStream::Stdout, result));
        });
    }
    if let Some(mut stderr) = child.stderr.take() {
        stream_count += 1;
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stderr.read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send((CapturedStream::Stderr, result));
        });
    }
    drop(sender);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let kill_result = child.kill();
                let reap_deadline = Instant::now() + Duration::from_millis(250);
                let reap_result = loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break format!("reaped with {status}"),
                        Ok(None) if Instant::now() < reap_deadline => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Ok(None) => break "still running after kill".to_owned(),
                        Err(error) => break format!("reap failed: {error}"),
                    }
                };
                panic!(
                    "timed out after {timeout:?} waiting for {waiting_for}; \
                     kill result: {kill_result:?}; {reap_result}"
                );
            }
            Err(error) => panic!("poll child while waiting for {waiting_for}: {error}"),
        }
    };

    let drain_deadline = Instant::now() + Duration::from_secs(1);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for _ in 0..stream_count {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        let (stream, bytes) = receiver.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!("drain captured output after waiting for {waiting_for}: {error}")
        });
        let bytes = bytes.unwrap_or_else(|error| {
            panic!("read captured output after waiting for {waiting_for}: {error}")
        });
        match stream {
            CapturedStream::Stdout => stdout = bytes,
            CapturedStream::Stderr => stderr = bytes,
        }
    }
    Output {
        status,
        stdout,
        stderr,
    }
}

fn output_with_timeout(command: &mut Command, waiting_for: &str) -> Output {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn child while waiting for {waiting_for}: {error}"));
    wait_for_output(child, waiting_for)
}

#[test]
fn bounded_child_wait_allows_a_prompt_exit() {
    let child = Command::new("sh")
        .args(["-c", "sleep 0.05; printf done"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn prompt child");
    let output = wait_for_output(child, "prompt fixture child to exit");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"done");
}

#[test]
fn bounded_child_wait_failure_names_the_stuck_operation() {
    let child = Command::new("sh")
        .args(["-c", "sleep 60"])
        .spawn()
        .expect("spawn stalled child");
    let panic = std::panic::catch_unwind(|| {
        wait_for_output_with_timeout(
            child,
            "deliberately stalled fixture child to exit",
            Duration::from_millis(50),
        )
    })
    .expect_err("stalled child must time out");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("<non-string panic>");
    assert!(
        message.contains("waiting for deliberately stalled fixture child to exit"),
        "timeout must name the stuck operation: {message}"
    );
}

/// Kills the profile's daemon (via the advisory pid in the readable owner
/// diagnostics) when the test ends, so failed assertions cannot leak daemons.
struct DaemonGuard {
    store: PathBuf,
}

impl DaemonGuard {
    fn pid(&self) -> Option<u32> {
        let contents = std::fs::read_to_string(self.store.join("lock.owner")).ok()?;
        contents
            .lines()
            .find_map(|line| line.strip_prefix("pid="))
            .and_then(|pid| pid.trim().parse().ok())
    }

    fn terminate_and_wait(&self, endpoint: &Path) {
        if let Some(pid) = self.pid() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while endpoint.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

fn assert_daemon_serves(profile: &ResolvedProfile) {
    let endpoint = profile.endpoint_path.clone();
    let expected_profile = profile.profile_id.clone();
    let daemon_log = profile
        .store_dir
        .join(haider_client::spawn::DAEMON_LOG_FILE);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async move {
        let connected =
            match haider_client::connect(&endpoint, haider_client::ClientConfig::default()).await {
                Ok(connected) => connected,
                Err(error) => {
                    // CI-as-debugger: the spawned daemon dies before serving on
                    // some runners — surface ITS OWN log so the failure names
                    // the startup error instead of a bare connect refusal.
                    let log = std::fs::read_to_string(&daemon_log)
                        .unwrap_or_else(|_| "<no daemon.log written>".to_owned());
                    panic!(
                        "daemon endpoint must be serving: {error:?}\n--- {} ---\n{log}",
                        daemon_log.display()
                    );
                }
            };
        assert_eq!(connected.welcome.profile_id, expected_profile);
        assert!(
            connected
                .welcome
                .features
                .is_superset(&haider_client::required_live_features()),
            "daemon must advertise the live feature families"
        );
        let _ = connected.client.close();
    });
}

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Three real `haider run` processes on one profile must authenticate the
/// same daemon PID. The bounded idle timer then proves that reuse does not
/// trade one cold start per invocation for an indefinitely resident child.
#[test]
fn repeated_run_invocations_pay_one_cold_daemon_start_then_idle_exit() {
    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };
    let fake_script = concat!(
        r#"[{"step":"emit_text","text":"ok"},{"step":"finish","reason":"end_turn"},"#,
        r#"{"step":"emit_text","text":"ok"},{"step":"finish","reason":"end_turn"},"#,
        r#"{"step":"emit_text","text":"ok"},{"step":"finish","reason":"end_turn"}]"#,
    );
    let mut daemon_pids = Vec::new();
    for invocation in 1..=3 {
        let output = output_with_timeout(
            haider_command(store.path())
                .args(["run", "--provider", "fake", "--json", "-p", "hello"])
                .env("HAIDER_TEST_FAKE_PROVIDER", fake_script)
                .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "5000"),
            &format!("same-profile run invocation {invocation}"),
        );
        assert!(
            output.status.success(),
            "run {invocation} failed: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        daemon_pids.push(guard.pid().expect("lingering daemon PID"));
    }
    assert!(
        daemon_pids.windows(2).all(|pair| pair[0] == pair[1]),
        "same-profile invocations must reuse one daemon: {daemon_pids:?}"
    );

    let daemon_pid = daemon_pids[0];
    let deadline = Instant::now() + Duration::from_secs(15);
    while (process_exists(daemon_pid) || profile.endpoint_path.exists())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !process_exists(daemon_pid),
        "daemon {daemon_pid} survived its bounded idle TTL"
    );
    assert!(
        !profile.endpoint_path.exists(),
        "idle exit must remove the profile endpoint"
    );
}

// MUTATION CHECK: R8 concurrent-launch arbitration — the store lock elects
// exactly one daemon, a losing candidate exits 75, and BOTH parents complete
// a Ready handshake. Mutating ensure_daemon to treat exit 75 as fatal (or to
// stop polling once its own candidate dies) makes one launcher exit nonzero
// and fails this test.
#[test]
fn two_simultaneous_launchers_elect_one_daemon_and_both_reach_ready() {
    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };

    let first = haider_command(store.path())
        .spawn()
        .expect("spawn first haider");
    let second = haider_command(store.path())
        .spawn()
        .expect("spawn second haider");
    let first = wait_for_output(first, "first simultaneous haider launcher to exit");
    let second = wait_for_output(second, "second simultaneous haider launcher to exit");
    for (name, output) in [("first", &first), ("second", &second)] {
        assert!(
            output.status.success(),
            "{name} launcher failed: status {:?}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("daemon ready"),
            "{name} launcher must report a ready daemon"
        );
    }

    // Exactly one daemon serves the shared endpoint afterward, and a third
    // launcher attaches without spawning.
    assert_daemon_serves(&profile);
    let third = output_with_timeout(
        &mut haider_command(store.path()),
        "third haider launcher to attach and exit",
    );
    assert!(third.status.success());
    assert!(
        String::from_utf8_lossy(&third.stdout).contains("already running"),
        "third launcher must attach to the incumbent: {}",
        String::from_utf8_lossy(&third.stdout)
    );

    guard.terminate_and_wait(&profile.endpoint_path);
}

// MUTATION CHECK: stale-endpoint recovery is exclusively the lock-winning
// daemon's job. The launcher observes ConnectionRefused on the dead socket
// node, spawns a candidate, and the daemon's claim/probe/unlink law recovers
// the node. Mutating the client to unlink the socket itself has no test to
// hide behind — the client crate contains no unlink call — and mutating
// ensure_daemon to treat Refused as fatal fails this test.
#[test]
fn stale_owner_socket_is_recovered_by_the_winning_daemon() {
    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };

    // Plant a dead same-owner socket node at the endpoint: bind, then drop
    // the listener. Connecting to the node now yields ConnectionRefused.
    // Use the production creator for the whole shared-root/profile/tmp tree.
    // A plain create_dir_all here creates the shared `<TMPDIR>/haider` parent
    // through the test process's umask (normally 0755); chmodding only the
    // profile child then poisons every concurrently starting daemon because
    // production correctly refuses a non-private shared root.
    haider_platform::prepare_runtime_directory(&profile.runtime_dir)
        .expect("create owner-private runtime tree");
    let _ = std::fs::remove_file(&profile.endpoint_path);
    let stale =
        std::os::unix::net::UnixListener::bind(&profile.endpoint_path).expect("bind stale socket");
    drop(stale);
    assert!(
        profile.endpoint_path.exists(),
        "stale socket node must exist"
    );

    let output = output_with_timeout(
        &mut haider_command(store.path()),
        "haider launcher to recover the stale socket and exit",
    );
    assert!(
        output.status.success(),
        "launcher must recover through the daemon: stdout {} stderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("daemon ready"));
    assert_daemon_serves(&profile);

    guard.terminate_and_wait(&profile.endpoint_path);
}

// MUTATION CHECK: R8 shutdown policy — parent exit leaves the daemon
// running; a client exit never implies daemon shutdown. Mutating the front
// door to kill or signal its spawned child on exit fails the post-exit
// serving assertion.
#[test]
fn parent_exit_leaves_the_daemon_running() {
    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };

    let started = Instant::now();
    let output = output_with_timeout(
        &mut haider_command(store.path()),
        "haider launcher to spawn the persistent daemon and exit",
    );
    let launch_elapsed = started.elapsed();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("daemon ready"));
    assert!(
        stdout.contains("spawned"),
        "first launch must spawn: {stdout}"
    );
    let launch_deadline = if std::env::var("CI").is_ok() {
        Duration::from_secs(10)
    } else {
        Duration::from_millis(950)
    };
    assert!(
        launch_elapsed < launch_deadline,
        "an authenticated own child must skip the 40 x 25 ms loser grace; launch took \
         {launch_elapsed:?} (deadline {launch_deadline:?})"
    );

    // The launcher has fully exited; the daemon must still serve, and the
    // daemon log must exist owner-only in the profile store.
    assert_daemon_serves(&profile);
    let log = store.path().join(haider_client::DAEMON_LOG_FILE);
    assert!(log.exists(), "owner-only daemon log must exist");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log)
            .expect("log metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "daemon log must be owner-only");
    }

    guard.terminate_and_wait(&profile.endpoint_path);
}

// MUTATION CHECK (W3b1/R8 numeric race-loser contract): map
// `DaemonError::AlreadyRunning` to any exit code other than 75 in
// `DaemonError::exit_code`. Expected failure: the deterministic
// second-candidate assertion below (the CLI race test exercises the same
// contract probabilistically; this pins the NUMBER the poll loop trusts).
#[test]
fn a_second_daemon_candidate_for_one_profile_exits_seventy_five() {
    let haiderd = ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };

    let mut winner = Command::new(&haiderd);
    configure_test_home(&mut winner, store.path());
    let winner = winner
        .arg("--profile")
        .arg(&profile.profile_id)
        .arg("--store-dir")
        .arg(&profile.store_dir)
        .arg("--runtime-dir")
        .arg(&profile.runtime_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn winning daemon");
    // Wait until the winner serves its endpoint.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !profile.endpoint_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        profile.endpoint_path.exists(),
        "winner must bind its endpoint"
    );

    let mut loser = Command::new(&haiderd);
    configure_test_home(&mut loser, store.path());
    let loser = output_with_timeout(
        loser
            .arg("--profile")
            .arg(&profile.profile_id)
            .arg("--store-dir")
            .arg(&profile.store_dir)
            .arg("--runtime-dir")
            .arg(&profile.runtime_dir),
        "second daemon candidate to concede the profile lock and exit",
    );
    assert_eq!(
        loser.status.code(),
        Some(75),
        "the race loser exits EX_TEMPFAIL(75): {}",
        String::from_utf8_lossy(&loser.stderr)
    );

    guard.terminate_and_wait(&profile.endpoint_path);
    let _ = wait_for_output(winner, "winning daemon to exit after test cleanup");
}
