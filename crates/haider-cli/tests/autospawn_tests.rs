#![cfg(unix)]
//! Bare-`haider` auto-spawn acceptance over REAL subprocess binaries
//! (report §6.2 tests): the concurrent-launch race, stale-socket recovery by
//! the winner only, and parent-exit-leaves-daemon.
//!
//! Each test isolates its profile in a fresh store directory (`profile_id`
//! is store-path-derived, so endpoints never collide) and kills its daemon
//! through a drop guard, so no test leaks a process past its assertions.
#![allow(clippy::expect_used)]

use std::io::{BufRead as _, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use haider_client::{ProfileEnv, ResolvedProfile, resolve_profile};

fn haider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_haider"))
}

/// Validate the externally prebuilt daemon and payload beside the thin CLI.
/// Bare invocations starting a daemon execute all three before readiness.
fn ensure_haiderd_built() -> PathBuf {
    // Match the other CLI subprocess fixtures: the caller owns fresh sibling
    // builds. Recursively entering Cargo here can contend with its parent or
    // replace an image after this process's Once warmup has completed.
    assert_eq!(
        std::env::var("HAIDER_TEST_SIBLINGS_PREBUILT").as_deref(),
        Ok("1"),
        "auto-spawn fixtures require freshly prebuilt runtime siblings; run \
         `cargo build -p haider-cli -p haider-daemond -p haider-tui-exe --bins` \
         first, then set HAIDER_TEST_SIBLINGS_PREBUILT=1 for the test command"
    );
    let haider = haider_binary();
    let directory = haider
        .parent()
        .expect("haider binary has a parent directory");
    let sibling = directory.join("haiderd");
    let payload = directory.join("haider-tui");
    for (name, binary) in [("haiderd", &sibling), ("haider-tui", &payload)] {
        assert!(
            binary.is_file(),
            "prebuilt {name} sibling missing at {}",
            binary.display()
        );
    }
    warm_autospawn_binaries(&haider, &payload, &sibling);
    sibling
}

fn warm_autospawn_binaries(haider: &Path, payload: &Path, haiderd: &Path) {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        // macOS validates each newly written Mach-O inode before entering
        // `main`: measured cold launches were 4.975 s and 4.79 s, while the
        // same inodes then launched in 0.23 s, 0.20 s, and 0.24 s. Charge
        // that one-time validation to this process fixture, not the 950 ms
        // own-child authentication assertion. The thin CLI's local --version
        // does not execute its payload, so warm that separate inode too.
        // All three version paths precede profile/store/runtime-directory/
        // socket setup; warming cannot start a daemon or mutate a profile.
        for (name, binary) in [
            ("haider", haider),
            ("haider-tui", payload),
            ("haiderd", haiderd),
        ] {
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
    let home = test_home(store);
    std::fs::create_dir_all(&home).expect("create isolated machine-user home");
    resolve_profile(&ProfileEnv {
        profile_dir: Some(store.to_path_buf()),
        home: Some(home.clone()),
        user_profile: Some(home),
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
    let home = test_home(store);
    std::fs::create_dir_all(&home).expect("create isolated machine-user home");
    command.env("HOME", &home).env("USERPROFILE", home);
}

fn test_home(store: &Path) -> PathBuf {
    store.join("machine-home")
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
        let pid = self.pid().expect("running daemon PID before cleanup");
        let _ = Command::new("kill").arg(pid.to_string()).status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while (process_exists(pid) || endpoint.exists()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let alive_after = process_exists(pid);
        assert!(!alive_after, "daemon {pid} survived test cleanup");
        assert!(!endpoint.exists(), "daemon endpoint survived test cleanup");
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

fn wait_for_daemon_ready(profile: &ResolvedProfile, guard: &DaemonGuard) -> u32 {
    let deadline = Instant::now() + CHILD_EXIT_TIMEOUT;
    loop {
        if let Some(pid) = guard.pid()
            && profile.endpoint_path.exists()
        {
            assert_daemon_serves(profile);
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not publish its PID and endpoint before the startup deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_daemon_exit(profile: &ResolvedProfile, pid: u32, boundary: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while process_exists(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_exists(pid),
        "daemon {pid} survived {boundary}\n{}",
        std::fs::read_to_string(profile.store_dir.join(haider_client::DAEMON_LOG_FILE))
            .unwrap_or_else(|error| format!("<daemon log unavailable: {error}>"))
    );
    assert_eq!(
        haider_client::profile_lock::profile_lock_owner_pid(&profile.store_dir)
            .expect("query profile lock after daemon exit"),
        None,
        "daemon exit must release the kernel profile lock"
    );
}

#[derive(Debug)]
struct ObservedRun {
    run_id: String,
}

fn observe_active_run(launcher: &mut Child) -> (ObservedRun, std::thread::JoinHandle<()>) {
    let stdout = launcher.stdout.take().expect("launcher stdout");
    let (observed_tx, observed_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut stdout = std::io::BufReader::new(stdout);
        let mut observation_pending = Some(observed_tx);
        loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) => {
                    if let Some(sender) = observation_pending.take() {
                        let _ = sender.send(Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "launcher exited before publishing the provider-active thinking state",
                        )));
                    }
                    break;
                }
                Ok(_) => {
                    let Ok(document) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    let payload = document.get("payload");
                    let active = payload.and_then(|value| value.get("type"))
                        == Some(&serde_json::Value::String("run_state".into()))
                        && payload
                            .and_then(|value| value.get("state"))
                            .and_then(serde_json::Value::as_str)
                            == Some("thinking");
                    if active && let Some(sender) = observation_pending.take() {
                        let observation = (|| {
                            Ok(ObservedRun {
                                run_id: document["run_id"]
                                    .as_str()
                                    .ok_or_else(|| {
                                        std::io::Error::new(
                                            std::io::ErrorKind::InvalidData,
                                            "thinking envelope has no run id",
                                        )
                                    })?
                                    .to_owned(),
                            })
                        })();
                        let _ = sender.send(observation);
                    }
                }
                Err(error) => {
                    if let Some(sender) = observation_pending.take() {
                        let _ = sender.send(Err(error));
                    }
                    break;
                }
            }
        }
    });
    let observed = observed_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("provider-active thinking state within daemon startup budget")
        .expect("read provider-active thinking state");
    (observed, reader)
}

fn durable_run_head(store: &Path, observed: &ObservedRun) -> (String, bool) {
    let connection = rusqlite::Connection::open_with_flags(
        store.join("store.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open live store read-only");
    connection
        .busy_timeout(Duration::from_secs(2))
        .expect("set live-store read timeout");
    let (state_json, terminal): (String, bool) = connection
        .query_row(
            "SELECT state_json, terminal FROM run_heads \
             WHERE run_id = ?1",
            [observed.run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load durable run head");
    let state = serde_json::from_str::<serde_json::Value>(&state_json)
        .expect("durable run state JSON")["state"]
        .as_str()
        .expect("durable run state tag")
        .to_owned();
    (state, terminal)
}

/// Registry #158 signal matrix: a second client interrupt takes the immediate
/// exit path after its durable `Cancelling` receipt. Test-only fake-provider
/// settlement is held past the 250 ms linger so the run cannot terminalize
/// first. Both the intact coordinate and a deliberately degraded coordinate
/// must reach the same absolute launcher deadline, then durably cancel during
/// graceful drain.
#[test]
fn sigint_twice_with_nonterminal_run_honors_absolute_linger_for_both_endpoints() {
    const IDLE_TTL_MS: u64 = 250;

    ensure_haiderd_built();
    for (case, remove_endpoint) in [("endpoint_intact", false), ("endpoint_absent", true)] {
        let store = tempfile::tempdir().expect("store dir");
        let profile = resolved_for(store.path());
        let guard = DaemonGuard {
            store: store.path().to_path_buf(),
        };
        let mut launcher = haider_command(store.path());
        launcher
            .args([
                "run",
                "--provider",
                "fake",
                "--output",
                "jsonl",
                "--timeout",
                "30s",
                "-p",
                "hold for double interrupt",
            ])
            .env("HAIDER_TEST_FAKE_PROVIDER", r#"[{"step":"hang"}]"#)
            .env("HAIDER_TEST_CANCEL_SETTLE_DELAY_MS", "500")
            .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", IDLE_TTL_MS.to_string());
        if remove_endpoint {
            launcher.env("HAIDER_TEST_DISABLE_ENDPOINT_LOSS_RECOVERY", "1");
        }
        let mut launcher = launcher.spawn().expect("spawn stalled run launcher");
        let daemon_pid = wait_for_daemon_ready(&profile, &guard);
        let (observed, stdout_reader) = observe_active_run(&mut launcher);

        haider_platform::signal_process(launcher.id(), haider_platform::ProcessSignal::Interrupt)
            .expect("first SIGINT to run client");
        let cancellation_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let (state, terminal) = durable_run_head(store.path(), &observed);
            if state == "cancelling" && !terminal {
                break;
            }
            assert!(
                Instant::now() < cancellation_deadline,
                "{case} first SIGINT did not retain a durable nonterminal cancellation: \
                 state={state} terminal={terminal}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        haider_platform::signal_process(launcher.id(), haider_platform::ProcessSignal::Interrupt)
            .expect("second SIGINT to run client");
        if remove_endpoint {
            std::fs::remove_file(&profile.endpoint_path)
                .expect("unlink daemon endpoint during double-interrupt teardown");
        }
        let client = wait_for_output(launcher, &format!("{case} double-interrupt client"));
        stdout_reader.join().expect("join launcher stdout reader");
        assert!(
            !client.status.success(),
            "double-interrupted client unexpectedly succeeded: stderr={}",
            String::from_utf8_lossy(&client.stderr)
        );

        wait_for_daemon_exit(&profile, daemon_pid, case);
        let log = std::fs::read_to_string(profile.store_dir.join(haider_client::DAEMON_LOG_FILE))
            .expect("read daemon recovery log");
        assert!(
            log.contains("durable_quiescent=false decision=shutdown"),
            "{case} deadline did not prove the stalled run was still nonterminal: {log}"
        );
        if remove_endpoint {
            assert!(
                log.contains("endpoint-health event=coordinate_lost"),
                "{case} did not observe the lost endpoint: {log}"
            );
            assert!(
                log.contains("reason=degraded_endpoint_idle_deadline"),
                "degraded linger did not own the shutdown decision: {log}"
            );
        } else {
            assert!(
                log.contains("reason=launcher_linger_deadline")
                    && log.contains("endpoint_degraded=false"),
                "intact endpoint did not reach the absolute launcher deadline: {log}"
            );
        }
        let (final_state, terminal) = durable_run_head(store.path(), &observed);
        assert!(terminal, "{case} graceful drain left the run nonterminal");
        assert_eq!(final_state, "cancelled");
    }
}

/// When both autonomous recovery paths are test-disabled, `daemon stop`
/// obtains the exact PID from `F_GETLK`, authenticates its executable/profile
/// argv, and SIGTERMs only that process. PID files are corroboration only.
#[test]
fn daemon_stop_recovers_unreachable_lock_owner_with_matching_stale_or_absent_pid_file() {
    let haiderd = ensure_haiderd_built();
    for pid_file_case in ["matching", "stale", "absent"] {
        let store = tempfile::tempdir().expect("store dir");
        let profile = resolved_for(store.path());
        let guard = DaemonGuard {
            store: store.path().to_path_buf(),
        };
        let mut daemon = Command::new(&haiderd)
            .args(["--profile", &profile.profile_id])
            .arg("--store-dir")
            .arg(&profile.store_dir)
            .arg("--runtime-dir")
            .arg(&profile.runtime_dir)
            .env(
                "HAIDER_TEST_FAKE_PROVIDER",
                r#"[{"step":"finish","reason":"end_turn"}]"#,
            )
            .env("HAIDER_TEST_DISABLE_ENDPOINT_LOSS_RECOVERY", "1")
            .env("HAIDER_TEST_DISABLE_DEGRADED_IDLE_REAP", "1")
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env("HOME", test_home(store.path()))
            .env("USERPROFILE", test_home(store.path()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn deliberately degraded daemon");
        let daemon_pid = wait_for_daemon_ready(&profile, &guard);
        assert_eq!(daemon.id(), daemon_pid);
        std::fs::remove_file(&profile.endpoint_path).expect("unlink daemon endpoint");
        let pid_path = profile.runtime_dir.join("haiderd.pid");
        match pid_file_case {
            "matching" => {}
            "stale" => std::fs::write(&pid_path, format!("{}\n", daemon_pid + 1))
                .expect("replace PID contents with stale value"),
            "absent" => std::fs::remove_file(&pid_path).expect("remove daemon PID file"),
            _ => unreachable!(),
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            process_exists(daemon_pid),
            "test hooks failed to retain the degraded daemon"
        );

        let mut stop = haider_command(store.path());
        stop.args(["daemon", "stop", "--json", "--timeout", "7s"]);
        let alternate_runtime = (pid_file_case == "absent").then(|| {
            tempfile::Builder::new()
                .prefix("hds-")
                .tempdir_in("/tmp")
                .expect("short alternate runtime root")
        });
        if let Some(alternate_runtime) = alternate_runtime.as_ref() {
            // Match the peer recovery recipe: HOME/store still identify the
            // owned profile, but the caller no longer has the daemon's
            // isolated runtime-root environment. The profile lock remains
            // the authoritative coordinate; a PID file is unavailable here.
            stop.env("HAIDER_RUNTIME_DIR", alternate_runtime.path());
        }
        let stopped = output_with_timeout(
            &mut stop,
            &format!("daemon stop recovery with {pid_file_case} PID file"),
        );
        assert!(
            stopped.status.success(),
            "recovery failed: stdout={} stderr={}",
            String::from_utf8_lossy(&stopped.stdout),
            String::from_utf8_lossy(&stopped.stderr)
        );
        let report: serde_json::Value =
            serde_json::from_slice(&stopped.stdout).expect("daemon stop recovery JSON");
        assert_eq!(report["outcome"], "recovered_by_signal");
        assert_eq!(
            report["recovery"]["owner_source"],
            "kernel_profile_lock_f_getlk"
        );
        assert_eq!(report["recovery"]["pid"], daemon_pid);
        assert_eq!(report["recovery"]["binary_and_profile_verified"], true);
        assert_eq!(report["recovery"]["pid_file"]["outcome"], pid_file_case);
        assert_eq!(report["recovery"]["signal"], "sigterm");
        assert_eq!(report["recovery"]["process_exited"], true);
        assert_eq!(report["recovery"]["profile_lock_released"], true);
        let status = daemon.wait().expect("reap recovered daemon");
        assert!(
            status.success(),
            "SIGTERM recovery was not graceful: {status}"
        );
    }
}

fn wait_for_idle_daemon_exit(profile: &ResolvedProfile, daemon_pid: u32, spawn_path: &str) {
    const IDLE_TTL_MS: u64 = 250;
    const DAEMON_DRAIN_BUDGET_MS: u64 = 5_000;
    const PROCESS_EXIT_GRACE_MS: u64 = 2_000;
    // Registry #94: 250 ms requested idle TTL + the daemon's 5,000 ms
    // graceful-drain budget + 2,000 ms process-observation grace = 7,250 ms.
    const EXIT_DEADLINE: Duration =
        Duration::from_millis(IDLE_TTL_MS + DAEMON_DRAIN_BUDGET_MS + PROCESS_EXIT_GRACE_MS);

    let deadline = Instant::now() + EXIT_DEADLINE;
    while (process_exists(daemon_pid) || profile.endpoint_path.exists())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !process_exists(daemon_pid),
        "{spawn_path} daemon {daemon_pid} survived the 7,250 ms idle-exit budget"
    );
    assert!(
        !profile.endpoint_path.exists(),
        "{spawn_path} idle exit must remove the profile endpoint"
    );
}

/// Every short-lived autospawn surface uses the same bounded daemon policy.
/// `real_run_short_idle_ttl_terminalizes_spawned_daemon` pins the run path;
/// this table pins status, the non-interactive TUI front door, and the
/// recovery probe with a real sibling daemon and an isolated profile each.
///
/// MUTATION CHECK: making the default lifetime persistent, or reading the
/// idle-TTL override only in `haider run`, leaves the affected daemon alive
/// through `wait_for_idle_daemon_exit`.
#[test]
fn real_status_tui_and_probe_autospawns_share_short_idle_ttl() {
    const IDLE_TTL_MS: &str = "250";

    ensure_haiderd_built();
    for (spawn_path, arguments) in [
        ("status", vec!["status", "--json"]),
        ("tui", Vec::new()),
        (
            "recovery-probe",
            vec![
                "session",
                "missing-idle-ttl-session",
                "recover",
                "--probe",
                "--json",
            ],
        ),
    ] {
        let store = tempfile::tempdir().expect("store dir");
        let profile = resolved_for(store.path());
        let guard = DaemonGuard {
            store: store.path().to_path_buf(),
        };
        let output = output_with_timeout(
            haider_command(store.path())
                .args(arguments)
                .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", IDLE_TTL_MS),
            &format!("{spawn_path} short-idle-TTL invocation"),
        );
        if spawn_path != "recovery-probe" {
            assert!(
                output.status.success(),
                "{spawn_path} failed: status={} stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        let daemon_pid = guard.pid().unwrap_or_else(|| {
            panic!(
                "{spawn_path} did not spawn a daemon: status={} stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
        });
        wait_for_idle_daemon_exit(&profile, daemon_pid, spawn_path);
    }
}

/// A real `haider run` must pass the configured idle TTL into the daemon, and
/// the daemon's own deadline wake must terminalize it after the launcher exits.
///
/// MUTATION CHECK: removing the idle-deadline branch from the daemon accept
/// loop leaves the retained process identity alive until this test's derived
/// deadline expires.
#[test]
fn real_run_short_idle_ttl_terminalizes_spawned_daemon() {
    const IDLE_TTL_MS: u64 = 250;
    const DAEMON_DRAIN_BUDGET_MS: u64 = 5_000;
    // Registry #94: 250 ms idle TTL + the daemon's 5,000 ms graceful-drain
    // budget = a 5,250 ms process-exit deadline. The retained kernel identity
    // reports exit directly, so this boundary needs no polling allowance.
    const EXIT_DEADLINE: Duration = Duration::from_millis(IDLE_TTL_MS + DAEMON_DRAIN_BUDGET_MS);

    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };
    let output = output_with_timeout(
        haider_command(store.path())
            .args(["run", "--provider", "fake", "--json", "-p", "hello"])
            .env(
                "HAIDER_TEST_FAKE_PROVIDER",
                r#"[{"step":"emit_text","text":"ok"},{"step":"finish","reason":"end_turn"}]"#,
            )
            .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", IDLE_TTL_MS.to_string()),
        "short-idle-TTL run invocation",
    );
    assert!(
        output.status.success(),
        "run failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let daemon_pid = guard.pid().expect("spawned daemon PID");
    let process_id = haider_platform::process_id(Some(daemon_pid)).expect("valid daemon PID");
    match haider_platform::ProcessExitMonitor::capture(process_id) {
        Ok(exit) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async {
                tokio::time::timeout(EXIT_DEADLINE, exit.wait())
                    .await
                    .expect("spawned daemon exceeded idle TTL plus drain budget")
                    .expect("wait for spawned daemon exit");
            });
        }
        Err(error) if !process_exists(daemon_pid) => {
            // A sufficiently fast daemon may exit between reading lock.owner
            // and retaining its process identity; that is the required state.
            let _ = error;
        }
        Err(error) => panic!("retain spawned daemon process identity: {error}"),
    }
    assert!(
        !profile.endpoint_path.exists(),
        "idle exit must remove the profile endpoint"
    );
}

/// Abrupt launcher death leaves no cancellation receipt, so a hanging fake
/// provider pins a genuinely nonterminal run through the launcher linger. The
/// deadline must still start graceful drain, whose worker-aware cancellation
/// becomes durable before the store and profile lock close.
///
/// MUTATION CHECK: restoring the durable-quiescence guard around the expired
/// linger timer leaves the daemon and profile lock alive past the exit budget.
#[test]
fn absolute_idle_linger_durably_settles_a_nonterminal_run() {
    const IDLE_TTL_MS: u64 = 250;
    const DAEMON_DRAIN_BUDGET_MS: u64 = 5_000;
    const PROCESS_EXIT_GRACE_MS: u64 = 2_000;
    const EXIT_DEADLINE: Duration =
        Duration::from_millis(IDLE_TTL_MS + DAEMON_DRAIN_BUDGET_MS + PROCESS_EXIT_GRACE_MS);

    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };
    let mut launcher = haider_command(store.path())
        .args([
            "run",
            "--provider",
            "fake",
            "--output",
            "jsonl",
            "--timeout",
            "5s",
            "-p",
            "hello",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", r#"[{"step":"hang"}]"#)
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", IDLE_TTL_MS.to_string())
        .spawn()
        .expect("spawn hanging run launcher");
    let (observed, stdout_reader) = observe_active_run(&mut launcher);
    let daemon_pid = guard.pid().expect("spawned daemon PID");

    launcher.kill().expect("SIGKILL run launcher");
    let output = wait_for_output(launcher, "SIGKILLed nonterminal run launcher");
    stdout_reader.join().expect("join launcher stdout reader");
    assert!(
        !output.status.success(),
        "killed launcher must not exit successfully"
    );

    let deadline = Instant::now() + EXIT_DEADLINE;
    while (process_exists(daemon_pid) || profile.endpoint_path.exists())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !process_exists(daemon_pid),
        "daemon {daemon_pid} did not retire after the absolute launcher linger\n{}",
        std::fs::read_to_string(store.path().join(haider_client::DAEMON_LOG_FILE))
            .unwrap_or_else(|error| format!("<daemon log unavailable: {error}>"))
    );
    assert!(
        !profile.endpoint_path.exists(),
        "absolute linger exit must remove the profile endpoint"
    );
    assert_eq!(
        haider_client::profile_lock::profile_lock_owner_pid(&profile.store_dir)
            .expect("query profile lock after absolute linger"),
        None,
        "absolute linger exit must release the kernel profile lock"
    );
    let (state, terminal) = durable_run_head(store.path(), &observed);
    assert!(terminal, "graceful linger drain left the run nonterminal");
    assert_eq!(
        state, "cancelled",
        "graceful drain preserves a replayable terminal"
    );
    let daemon_log = std::fs::read_to_string(store.path().join(haider_client::DAEMON_LOG_FILE))
        .expect("read absolute linger daemon log");
    assert!(
        daemon_log.contains("reason=launcher_linger_deadline")
            && daemon_log.contains("durable_quiescent=false decision=shutdown"),
        "absolute linger did not observe and drain the nonterminal run: {daemon_log}"
    );
}

/// Three default-policy `haider run` processes on one profile authenticate
/// the same warm daemon. Status projects that launch policy, and the operator
/// stop path proves the daemon does not outlive the test.
#[test]
fn repeated_run_invocations_default_to_one_warm_daemon_until_operator_stop() {
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
                .env("HAIDER_TEST_FAKE_PROVIDER", fake_script),
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
    let status_output = output_with_timeout(
        haider_command(store.path()).args(["status", "--json", "--no-spawn"]),
        "default warm daemon status",
    );
    assert!(
        status_output.status.success(),
        "status failed: status={} stdout={} stderr={}",
        status_output.status,
        String::from_utf8_lossy(&status_output.stdout),
        String::from_utf8_lossy(&status_output.stderr),
    );
    let status: serde_json::Value =
        serde_json::from_slice(&status_output.stdout).expect("status stdout is JSON");
    assert_eq!(status["daemon"]["pid"], daemon_pid);
    assert_eq!(status["daemon"]["idle_ttl_ms"], 30_000);
    assert_eq!(status["daemon"]["caching"]["idle_ttl_ms"], 30_000);
    assert_eq!(status["daemon"]["caching"]["session_reuse"], "resident");
    assert_eq!(status["daemon"]["caching"]["prompt_cache"], true);
    assert_eq!(status["daemon"]["caching"]["provider_view_cas"], true);
    assert_eq!(status["daemon"]["warm"], true);
    assert_eq!(status["daemon"]["ready"], true);
    assert!(status["daemon"]["ready_since"].as_u64().is_some());
    assert_eq!(status["daemon"]["providers_loaded"], true);
    assert!(
        process_exists(daemon_pid),
        "warm daemon must still be alive"
    );

    let stop_output = output_with_timeout(
        haider_command(store.path()).args(["daemon", "stop", "--json", "--timeout", "10s"]),
        "operator daemon stop",
    );
    assert!(
        stop_output.status.success(),
        "daemon stop failed: status={} stdout={} stderr={}",
        stop_output.status,
        String::from_utf8_lossy(&stop_output.stdout),
        String::from_utf8_lossy(&stop_output.stderr),
    );
    let stopped: serde_json::Value =
        serde_json::from_slice(&stop_output.stdout).expect("daemon stop stdout is JSON");
    assert_eq!(stopped["outcome"], "stopped_cleanly");
    assert_eq!(stopped["daemon"]["pid"], daemon_pid);
    assert_eq!(stopped["daemon"]["process_exited"], true);

    let alive_after = process_exists(daemon_pid);
    assert!(!alive_after, "daemon {daemon_pid} survived operator stop");
    assert!(
        !profile.endpoint_path.exists(),
        "operator stop must remove the profile endpoint"
    );
}

// MUTATION CHECK: notify the launcher on Recovering, make `--ready` trust
// process/PID existence, or let immediate clients give up after losing their
// daemon-candidate race. Expected failure: the starter exits before the
// delayed readiness edge or at least one of the N client turns fails.
#[test]
fn clients_immediately_after_daemon_pid_publication_all_wait_and_succeed() {
    const CLIENTS: usize = 4;
    const READY_DELAY_MS: u64 = 750;

    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };
    let fake_steps = (0..CLIENTS)
        .flat_map(|index| {
            [
                serde_json::json!({"step": "emit_text", "text": format!("ok-{index}")}),
                serde_json::json!({"step": "finish", "reason": "end_turn"}),
            ]
        })
        .collect::<Vec<_>>();
    let fake_script = serde_json::to_string(&fake_steps).expect("fake script JSON");

    let mut starter = haider_command(store.path())
        .arg("--ready")
        .env("HAIDER_TEST_FAKE_PROVIDER", &fake_script)
        .env("HAIDER_TEST_READY_DELAY_MS", READY_DELAY_MS.to_string())
        .spawn()
        .expect("spawn readiness launcher");

    let pid_deadline = Instant::now() + CHILD_EXIT_TIMEOUT;
    let daemon_pid = loop {
        if let Some(pid) = guard.pid() {
            break pid;
        }
        assert!(
            Instant::now() < pid_deadline,
            "daemon did not publish its early profile-lock PID"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        starter.try_wait().expect("poll --ready launcher").is_none(),
        "--ready must not exit on PID publication before positive readiness"
    );

    let clients = (0..CLIENTS)
        .map(|index| {
            haider_command(store.path())
                .args([
                    "run",
                    "--provider",
                    "fake",
                    "--json",
                    "-p",
                    &format!("immediate client {index}"),
                ])
                .env("HAIDER_TEST_FAKE_PROVIDER", &fake_script)
                .spawn()
                .unwrap_or_else(|error| panic!("spawn immediate client {index}: {error}"))
        })
        .collect::<Vec<_>>();
    for (index, client) in clients.into_iter().enumerate() {
        let output = wait_for_output(client, &format!("immediate client {index}"));
        assert!(
            output.status.success(),
            "immediate client {index} failed: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let _: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("client {index} stdout is JSON: {error}"));
    }

    let starter = wait_for_output(starter, "positive-readiness launcher");
    assert!(
        starter.status.success(),
        "--ready failed: status={} stdout={} stderr={}",
        starter.status,
        String::from_utf8_lossy(&starter.stdout),
        String::from_utf8_lossy(&starter.stderr),
    );
    assert!(String::from_utf8_lossy(&starter.stdout).contains("daemon ready"));
    assert_eq!(
        guard.pid(),
        Some(daemon_pid),
        "one daemon must serve every client"
    );

    let status_output = output_with_timeout(
        haider_command(store.path()).args(["status", "--json", "--no-spawn"]),
        "post-race readiness status",
    );
    assert!(status_output.status.success());
    let status: serde_json::Value =
        serde_json::from_slice(&status_output.stdout).expect("status stdout is JSON");
    assert_eq!(status["daemon"]["pid"], daemon_pid);
    assert_eq!(status["daemon"]["ready"], true);
    assert!(status["daemon"]["ready_since"].as_u64().is_some());
    assert_eq!(status["daemon"]["providers_loaded"], true);

    guard.terminate_and_wait(&profile.endpoint_path);
}

// MUTATION CHECK: notify the inherited startup pipe from Recovering instead
// of the shared positive predicate. Expected failure: the first timeout sees
// the early byte after the daemon-owned PID file appears.
#[test]
fn launcher_readiness_pipe_stays_silent_until_positive_predicate() {
    const READY_DELAY_MS: u64 = 3_000;
    const SILENCE_WINDOW: Duration = Duration::from_millis(200);

    let haiderd = ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };
    let log_path =
        haider_platform::allocate_daemon_log_path(&profile.store_dir).expect("allocate daemon log");
    let ready_delay = READY_DELAY_MS.to_string();
    let machine_home = test_home(store.path());
    let machine_home = machine_home.to_str().expect("UTF-8 machine-user home");
    let spawned = haider_platform::spawn_daemon_with_readiness_and_environment(
        haider_platform::DaemonSpawn {
            binary: &haiderd,
            profile_id: &profile.profile_id,
            store_dir: &profile.store_dir,
            runtime_dir: &profile.runtime_dir,
            log_path: &log_path,
        },
        &[
            (
                "HAIDER_TEST_FAKE_PROVIDER",
                r#"[{"step":"finish","reason":"end_turn"}]"#,
            ),
            ("HAIDER_TEST_READY_DELAY_MS", &ready_delay),
            ("HAIDER_DISCOVERY_DISABLED", "1"),
            ("HOME", machine_home),
            ("USERPROFILE", machine_home),
        ],
    )
    .expect("spawn daemon with a directly observable readiness pipe");
    let haider_platform::SpawnedDaemon {
        mut child,
        readiness,
    } = spawned;

    let pid_file = profile.runtime_dir.join("haiderd.pid");
    let pid_deadline = Instant::now() + CHILD_EXIT_TIMEOUT;
    while !pid_file.exists() {
        if let Some(status) = child.try_wait().expect("poll direct readiness daemon") {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("daemon exited before PID-file publication: {status}; log={log}");
        }
        assert!(
            Instant::now() < pid_deadline,
            "daemon did not publish its PID file before the injected pause"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("readiness test runtime");
    runtime.block_on(async {
        let mut readiness = Box::pin(readiness.wait());
        // The 200 ms observation is bounded by a 3,000 ms injected pause,
        // leaving 2,800 ms of scheduler headroom after PID-file observation.
        assert!(
            tokio::time::timeout(SILENCE_WINDOW, &mut readiness)
                .await
                .is_err(),
            "launcher readiness pipe fired before the positive predicate"
        );
        tokio::time::timeout(CHILD_EXIT_TIMEOUT, &mut readiness)
            .await
            .expect("positive readiness pipe deadline")
            .expect("positive readiness pipe result");
    });

    let pid = guard.pid().expect("running daemon PID before cleanup");
    let signal = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .expect("signal daemon after readiness assertion");
    assert!(signal.success(), "signal daemon: {signal}");
    let output = wait_for_output(child, "direct readiness daemon shutdown");
    assert!(
        output.status.success(),
        "daemon shutdown: {}",
        output.status
    );
    assert!(!profile.endpoint_path.exists());
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
    let _guard = DaemonGuard {
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

    let winner_pid = winner.id();
    let _ = Command::new("kill").arg(winner_pid.to_string()).status();
    let _ = wait_for_output(winner, "winning daemon to exit after test cleanup");
    let alive_after = process_exists(winner_pid);
    assert!(!alive_after, "winning daemon survived test cleanup");
    assert!(
        !profile.endpoint_path.exists(),
        "winning daemon endpoint survived test cleanup"
    );
}
