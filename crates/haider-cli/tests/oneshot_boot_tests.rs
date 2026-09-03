#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Behaviour-preservation pins for the one-shot daemon boot path
//! (`HAIDER_RUN_DAEMON_IDLE_TTL_MS=0` against a fresh profile) and the
//! profile state a LATER persistent daemon depends on.
//!
//! Every pin observes a contract-visible outcome (JSONL bytes, profile
//! files, JSON documents, typed exit codes), never an internal. The
//! inventory of what each pin protects lives in
//! `docs/testing/v0.0.969/oneshotboot-tests.md`.
//!
//! Golden fixtures under `tests/fixtures/` are regenerated ONLY by an
//! explicit `HAIDER_ONESHOT_GOLDEN_UPDATE=1`; a plain run compares.

use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{ArtifactRef, SessionId};
use haider_store::{Cas as _, FileCas};

const PREBUILT_DAEMON_ENV: &str = "HAIDER_TEST_SIBLINGS_PREBUILT";
const GOLDEN_UPDATE_ENV: &str = "HAIDER_ONESHOT_GOLDEN_UPDATE";
const IDLE_TTL_ENV: &str = "HAIDER_RUN_DAEMON_IDLE_TTL_MS";
const FAKE_PROVIDER_ENV: &str = "HAIDER_TEST_FAKE_PROVIDER";
/// One fixed fake-provider turn: text, one locally exact usage sample, end.
const FAKE_TURN: &str = concat!(
    r#"{"step":"emit_text","text":"fake response: hello"},"#,
    r#"{"step":"emit_usage","usage":{"input":2,"output":1,"reasoning":0,"cached":0,"source":"locally_exact"}},"#,
    r#"{"step":"finish","reason":"end_turn"}"#,
);
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(60);
const DAEMON_EXIT_DEADLINE: Duration = Duration::from_secs(20);
const EX_PROTOCOL: i32 = 76;
const EX_UNAVAILABLE: i32 = 69;

/// The fake script every daemon in this file boots with: four identical
/// turns so a lingering daemon can serve follow-up runs.
fn fake_script() -> String {
    format!("[{FAKE_TURN},{FAKE_TURN},{FAKE_TURN},{FAKE_TURN}]")
}

fn haider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_haider"))
}

fn ensure_haiderd_present() {
    assert_eq!(
        std::env::var(PREBUILT_DAEMON_ENV).as_deref(),
        Ok("1"),
        "CLI subprocess fixtures require a fresh sibling; run \
         `cargo build -p haider-daemond --bin haiderd` first, then set \
         {PREBUILT_DAEMON_ENV}=1 for the test command"
    );
    let sibling = haider_binary()
        .parent()
        .expect("haider binary parent")
        .join(format!("haiderd{}", std::env::consts::EXE_SUFFIX));
    assert!(
        sibling.is_file(),
        "haiderd sibling missing at {}; prebuild with `cargo build -p haider-daemond --bin haiderd`",
        sibling.display()
    );
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// One isolated profile + machine-user home + hermetic workspace. Dropping
/// it stops any daemon still owning the profile, so no test leaks a process.
struct Profile {
    _root: tempfile::TempDir,
    profile: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
}

impl Profile {
    fn new() -> Self {
        ensure_haiderd_present();
        let root = tempfile::tempdir().expect("temporary profile parent");
        let profile = root.path().join("profile");
        let home = root.path().join("machine-home");
        let workspace = root.path().join("workspace");
        for directory in [&profile, &home, &workspace] {
            std::fs::create_dir_all(directory).expect("create fixture directory");
        }
        Self {
            _root: root,
            profile,
            home,
            workspace,
        }
    }

    /// A hermetic `haider` invocation: isolated HOME, disabled discovery and
    /// update checks, no inherited fake provider, runtime root, or TTL.
    fn command(&self) -> Command {
        let mut command = Command::new(haider_binary());
        command
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("HAIDER_PROFILE_DIR", &self.profile)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env("HAIDER_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1")
            .env_remove("HAIDER_MODEL")
            .env_remove("HAIDER_RUNTIME_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove(FAKE_PROVIDER_ENV)
            .env_remove(IDLE_TTL_ENV)
            .stdin(Stdio::null());
        command
    }

    /// A one-shot invocation: the spawned daemon has a zero idle TTL.
    fn one_shot(&self) -> Command {
        let mut command = self.command();
        command.env(IDLE_TTL_ENV, "0");
        command
    }

    /// A lingering invocation: a spawned daemon stays for 30 s of idleness
    /// (well past every bounded wait in this file) and is stopped explicitly.
    fn lingering(&self) -> Command {
        let mut command = self.command();
        command.env(IDLE_TTL_ENV, "30000");
        command
    }

    fn lockdown_root(&self) -> PathBuf {
        self.home.join(".haider").join("lockdown")
    }

    fn resolved(&self) -> haider_client::ResolvedProfile {
        haider_client::resolve_profile(&haider_client::ProfileEnv {
            profile_dir: Some(self.profile.clone()),
            home: Some(self.home.clone()),
            user_profile: Some(self.home.clone()),
            model: None,
            runtime_dir: None,
            xdg_runtime_dir: None,
        })
        .expect("resolve the fixture profile")
    }

    /// Stops the daemon owning this profile through the operator door and
    /// pins the typed clean-stop receipt.
    fn stop_daemon_cleanly(&self) -> serde_json::Value {
        let output = bounded_output(
            self.command().args(["daemon", "stop", "--json"]),
            "daemon stop",
        );
        assert!(
            output.status.success(),
            "daemon stop failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("daemon stop JSON");
        assert_eq!(report["schema"], "haider.daemon-stop.v1");
        assert_eq!(report["outcome"], "stopped_cleanly", "report: {report}");
        assert_eq!(report["daemon"]["completion"], "graceful");
        assert_eq!(report["daemon"]["process_exited"], true);
        wait_for_daemon_gone(&self.profile);
        report
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        if daemon_pid(&self.profile).is_none() {
            return;
        }
        let _ = self
            .command()
            .args(["daemon", "stop", "--json", "--timeout", "5s"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(pid) = daemon_pid(&self.profile)
            && process_alive(pid)
        {
            #[cfg(unix)]
            let _ = Command::new("kill")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            #[cfg(windows)]
            let _ = haider_platform::kill_process_tree(pid, true);
        }
    }
}

fn daemon_pid(profile: &Path) -> Option<u32> {
    std::fs::read_to_string(profile.join("lock.owner"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("pid="))?
        .trim()
        .parse()
        .ok()
}

/// Cross-platform liveness: a process identity can be retained only while
/// the kernel still knows the PID.
fn process_alive(pid: u32) -> bool {
    haider_platform::process_id(Some(pid))
        .and_then(|id| haider_platform::ProcessExitMonitor::capture(id).ok())
        .is_some()
}

#[cfg(unix)]
fn profile_lock_is_free(profile: &Path) -> bool {
    let Ok(lock) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(profile.join("lock"))
    else {
        return true;
    };
    match lock.try_lock() {
        Ok(()) => {
            lock.unlock().expect("release profile lock probe");
            true
        }
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn profile_lock_is_free(_profile: &Path) -> bool {
    true
}

/// Bounded wait for the profile to have no daemon: owner file removed,
/// process gone, profile lock released.
fn wait_for_daemon_gone(profile: &Path) {
    let deadline = Instant::now() + DAEMON_EXIT_DEADLINE;
    loop {
        let pid = daemon_pid(profile);
        let gone = pid.is_none_or(|pid| !process_alive(pid));
        if gone && !profile.join("lock.owner").exists() && profile_lock_is_free(profile) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon {pid:?} still owns {} after {DAEMON_EXIT_DEADLINE:?}",
            profile.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Immediate (no-wait) proof that a finished one-shot left nothing behind:
/// `haider run` must not return before its owned daemon's checked exit.
fn assert_daemon_gone_now(profile: &Path, spawned_pid: Option<u32>) {
    assert!(
        !profile.join("lock.owner").exists(),
        "one-shot daemon must remove lock.owner before the CLI exits"
    );
    if let Some(pid) = spawned_pid {
        assert!(
            !process_alive(pid),
            "one-shot daemon {pid} must be reaped before the CLI exits"
        );
    }
    assert!(
        profile_lock_is_free(profile),
        "one-shot daemon must release the profile lock before the CLI exits"
    );
}

/// Waits for a captured child with a hard deadline so a hung boundary fails
/// with its name instead of consuming the test runner.
fn bounded_output(command: &mut Command, waiting_for: &str) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {waiting_for}: {error}"));
    let (sender, receiver) = mpsc::channel();
    let mut streams = 0;
    if let Some(mut stdout) = child.stdout.take() {
        streams += 1;
        let sender = sender.clone();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send((true, result));
        });
    }
    if let Some(mut stderr) = child.stderr.take() {
        streams += 1;
        let sender = sender.clone();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stderr.read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send((false, result));
        });
    }
    drop(sender);
    let deadline = Instant::now() + CHILD_EXIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("timed out after {CHILD_EXIT_TIMEOUT:?} waiting for {waiting_for}");
            }
            Err(error) => panic!("poll child while waiting for {waiting_for}: {error}"),
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for _ in 0..streams {
        let (is_stdout, bytes) = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("drain {waiting_for} output: {error}"));
        let bytes = bytes.unwrap_or_else(|error| panic!("read {waiting_for} output: {error}"));
        if is_stdout {
            stdout = bytes;
        } else {
            stderr = bytes;
        }
    }
    Output {
        status,
        stdout,
        stderr,
    }
}

fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn json_document(output: &Output, what: &str) -> serde_json::Value {
    assert_success(output, what);
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{what} stdout is not one JSON document: {error}"))
}

/// Splits `haider run --jsonl` stdout into the acceptance object and the
/// durable envelopes, checking the LF framing and contiguous cursor law.
fn parse_jsonl(output: &Output) -> (serde_json::Value, Vec<RawEnvelope>) {
    assert!(output.stdout.ends_with(b"\n"), "JSONL must end with LF");
    assert!(!output.stdout.contains(&b'\r'), "JSONL must not contain CR");
    let text = String::from_utf8(output.stdout.clone()).expect("JSONL stdout is UTF-8");
    let mut lines = text.lines();
    let accepted: serde_json::Value =
        serde_json::from_str(lines.next().expect("accepted line")).expect("accepted JSON");
    assert_eq!(accepted["event"], "accepted");
    let envelopes = lines
        .map(|line| serde_json::from_str::<RawEnvelope>(line).expect("RawEnvelope JSONL line"))
        .collect::<Vec<_>>();
    assert!(!envelopes.is_empty(), "a run emits envelopes");
    assert_eq!(accepted["head_seq"], envelopes[0].seq);
    assert_eq!(accepted["session_id"], envelopes[0].session_id.as_str());
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1),
        "JSONL cursor must be contiguous"
    );
    (accepted, envelopes)
}

fn run_one_shot_jsonl(
    profile: &Profile,
    prompt: &str,
) -> (Output, serde_json::Value, Vec<RawEnvelope>) {
    let output = bounded_output(
        profile
            .one_shot()
            .env(FAKE_PROVIDER_ENV, fake_script())
            .args(["run", "--provider", "fake", "--jsonl", prompt]),
        "one-shot JSONL run",
    );
    assert_success(&output, "one-shot JSONL run");
    let (accepted, envelopes) = parse_jsonl(&output);
    (output, accepted, envelopes)
}

/// Replays one session's durable journal from the daemon currently serving
/// the profile (never spawning one), as a client would after restart.
fn replay_session(profile: &Profile, session_id: &str) -> Vec<RawEnvelope> {
    let resolved = profile.resolved();
    let session_id = SessionId::new(session_id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async move {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(30),
            haider_client::observe_stream_session_after(
                &resolved, false, session_id, false, sender, 0,
            ),
        )
        .await
        .expect("session replay finishes")
        .expect("session replay succeeds against the serving daemon");
        let mut envelopes = Vec::new();
        while let Ok(envelope) = receiver.try_recv() {
            envelopes.push(envelope);
        }
        envelopes
    })
}

fn envelope_value(envelope: &RawEnvelope) -> serde_json::Value {
    serde_json::to_value(envelope).expect("serialize envelope")
}

/// Normalizes volatile identity/time/digest fields so one fixed fake script
/// yields one stable golden. Everything else (sequence, payload kinds,
/// ordering, states, texts, usage numbers, render flags) is compared exactly.
fn normalize(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "session_id" | "run_id" | "event_id" | "device_id" | "item_id" | "node"
                    | "parent" | "call_id" | "instance_id" => *child = serde_json::json!("<id>"),
                    "committed_at_ms" | "cwd" | "updated_at" | "fetched_at" | "inventory_age"
                    | "last_activity_ms" => *child = serde_json::json!("<volatile>"),
                    "input_tokens" | "used_tokens" | "stable_prefix_tokens" => {
                        *child = serde_json::json!("<estimate>");
                    }
                    _ => normalize(child),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(normalize),
        serde_json::Value::String(text) => {
            let scrubbed = scrub_hex_runs(text);
            if scrubbed != *text {
                *text = scrubbed;
            }
        }
        _ => {}
    }
}

/// Replaces every run of 32+ lowercase hex characters (digests, keyed
/// hashes, cache epochs) with `<hex>`.
fn scrub_hex_runs(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, output: &mut String| {
        if run.len() >= 32 {
            output.push_str("<hex>");
        } else {
            output.push_str(run);
        }
        run.clear();
    };
    for character in text.chars() {
        if character.is_ascii_hexdigit() && !character.is_ascii_uppercase() {
            run.push(character);
        } else {
            flush(&mut run, &mut output);
            output.push(character);
        }
    }
    flush(&mut run, &mut output);
    output
}

fn normalized_line(value: &serde_json::Value) -> String {
    let mut value = value.clone();
    normalize(&mut value);
    serde_json::to_string(&value).expect("normalized JSON line")
}

fn compare_or_update_golden(name: &str, actual: &str) {
    let path = fixture_path(name);
    if std::env::var_os(GOLDEN_UPDATE_ENV).is_some() {
        std::fs::write(&path, actual).expect("write golden fixture");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read golden {}: {error}", path.display()));
    if expected == actual {
        return;
    }
    let mismatch = expected
        .lines()
        .zip(actual.lines())
        .enumerate()
        .find(|(_, (expected, actual))| expected != actual);
    match mismatch {
        Some((index, (expected, actual))) => panic!(
            "golden {name} differs at line {}:\n expected: {expected}\n actual:   {actual}\n\
             (regenerate deliberately with {GOLDEN_UPDATE_ENV}=1 after reviewing the contract change)",
            index + 1
        ),
        None => panic!(
            "golden {name} differs in length: expected {} lines, actual {} lines\n\
             (regenerate deliberately with {GOLDEN_UPDATE_ENV}=1 after reviewing the contract change)",
            expected.lines().count(),
            actual.lines().count()
        ),
    }
}

// ---------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------

/// A fresh profile served once by a one-shot daemon must hand a LATER
/// persistent daemon a complete session: it is listed, its journal replays
/// envelope-for-envelope, and the lingering daemon serves a follow-up run
/// without a second spawn.
///
/// MUTATION CHECK: dropping a journal flush, the lock release, or the
/// checked child exit from the one-shot shutdown leaves either the replay
/// short, `lock.owner` present at CLI exit, or a second daemon per run.
#[test]
fn one_shot_run_state_is_visible_to_a_later_persistent_daemon() {
    let profile = Profile::new();
    let (_, accepted, streamed) = run_one_shot_jsonl(&profile, "hello");
    let session_id = accepted["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert_daemon_gone_now(&profile.profile, None);
    let first_generation = streamed[0].worker_generation;
    let run_id = streamed
        .iter()
        .find_map(|envelope| envelope.run_id.clone())
        .expect("run id");

    // A lingering daemon on the SAME profile: the carried-over session is
    // listed by identity and shows the terminal-idle state.
    let sessions = json_document(
        &bounded_output(
            profile
                .lingering()
                .env(FAKE_PROVIDER_ENV, fake_script())
                .args(["sessions", "--json"]),
            "sessions after one-shot",
        ),
        "sessions after one-shot",
    );
    let row = sessions["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|row| row["session_id"] == session_id)
        .unwrap_or_else(|| panic!("one-shot session missing from {sessions}"))
        .clone();
    assert_eq!(row["run_state"], "idle");
    assert_eq!(row["provider"], "fake");
    assert_eq!(row["model"], "fake-model");
    assert_eq!(row["run_id"], run_id.as_str());
    assert!(
        row["worker_generation"]
            .as_u64()
            .is_some_and(|generation| generation > first_generation),
        "a new daemon advances the profile generation: {row}"
    );
    let lingering_pid = daemon_pid(&profile.profile).expect("lingering daemon PID");
    assert!(process_alive(lingering_pid));

    // The durable journal replays exactly what the one-shot streamed. The
    // stream ends at the run terminal; the journal may continue with the
    // session's own post-terminal state facts, never with more run events.
    let replayed = replay_session(&profile, &session_id);
    assert!(
        replayed.len() >= streamed.len(),
        "replay must contain every streamed envelope: replayed {} < streamed {}",
        replayed.len(),
        streamed.len()
    );
    for (streamed, replayed) in streamed.iter().zip(&replayed) {
        assert_eq!(
            envelope_value(streamed),
            envelope_value(replayed),
            "seq {} must replay identically",
            streamed.seq
        );
    }
    for trailing in &replayed[streamed.len()..] {
        assert_eq!(
            trailing.payload["type"], "session_state",
            "only session-state facts may follow the streamed terminal: {}",
            trailing.payload
        );
    }
    assert!(
        replayed
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1),
        "the durable journal is contiguous"
    );

    // The lingering daemon serves the next run itself (no second spawn).
    let second = bounded_output(
        profile
            .lingering()
            .env(FAKE_PROVIDER_ENV, fake_script())
            .args(["run", "--provider", "fake", "--jsonl", "again"]),
        "attached follow-up run",
    );
    assert_success(&second, "attached follow-up run");
    let (_, second_envelopes) = parse_jsonl(&second);
    assert_eq!(
        second_envelopes[0].worker_generation, row["worker_generation"],
        "the follow-up run is served by the lingering daemon's generation"
    );
    assert_eq!(daemon_pid(&profile.profile), Some(lingering_pid));
    assert!(process_alive(lingering_pid));

    profile.stop_daemon_cleanly();
}

/// The one-shot JSONL surface for one fixed fake script is a golden: the
/// acceptance object, every envelope in cursor order, and the single typed
/// terminal. Identity, time, digest, and estimate fields are normalized;
/// nothing else is.
///
/// MUTATION CHECK: dropping, reordering, or duplicating any envelope
/// (including the trailing `run_state: done` terminal) or changing the
/// acceptance object changes a golden line.
#[test]
fn one_shot_jsonl_stream_matches_the_normalized_golden() {
    let profile = Profile::new();
    let (output, accepted, envelopes) = run_one_shot_jsonl(&profile, "hello");
    assert!(
        output.stderr.is_empty(),
        "JSONL runs keep stderr silent: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_daemon_gone_now(&profile.profile, None);
    assert_eq!(accepted["head_seq"], 1, "a fresh profile starts at seq 1");
    let terminals = envelopes
        .iter()
        .filter(|envelope| envelope.payload.get("terminal_kind").is_some())
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].payload["terminal_kind"], "success");
    assert!(std::ptr::eq(
        terminals[0],
        envelopes.last().expect("terminal")
    ));

    let mut lines = vec![normalized_line(&accepted)];
    lines.extend(
        envelopes
            .iter()
            .map(|envelope| normalized_line(&envelope_value(envelope))),
    );
    let actual = format!("{}\n", lines.join("\n"));
    compare_or_update_golden("oneshot_run_golden.jsonl", &actual);
}

/// A daemon boot writes lines into a per-process log, publishes it at the
/// stable `daemon.log` name, and keeps the per-process history bounded to
/// the newest `DAEMON_LOG_RETENTION` files.
///
/// MUTATION CHECK: skipping the publication leaves `daemon.log` absent;
/// skipping the prune leaves 41 logs; pruning newest-first removes the
/// live log's neighbours instead of the oldest seeds.
#[test]
fn one_shot_boot_publishes_a_nonempty_daemon_log_and_bounds_log_history() {
    let profile = Profile::new();
    let logs = profile.profile.join(haider_platform::DAEMON_LOG_DIRECTORY);
    std::fs::create_dir_all(&logs).expect("seed log directory");
    let seeded = haider_platform::DAEMON_LOG_RETENTION + 8;
    let now = SystemTime::now();
    let seeded_names = (0..seeded)
        .map(|index| {
            let name = format!("haiderd-0-{index}-seed.log");
            let path = logs.join(&name);
            std::fs::write(&path, b"seeded\n").expect("seed log");
            let modified = now - Duration::from_secs((seeded - index) as u64 * 60 + 3_600);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open seed log")
                .set_modified(modified)
                .expect("age seed log");
            name
        })
        .collect::<Vec<_>>();

    let (_, _, _) = run_one_shot_jsonl(&profile, "hello");
    assert_daemon_gone_now(&profile.profile, None);

    let deadline = Instant::now() + Duration::from_secs(5);
    let live_logs = loop {
        let mut names = std::fs::read_dir(&logs)
            .expect("read log directory")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("haiderd-") && name.ends_with(".log"))
            .collect::<Vec<_>>();
        names.sort();
        if names.len() <= haider_platform::DAEMON_LOG_RETENTION || Instant::now() >= deadline {
            break names;
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(
        live_logs.len() <= haider_platform::DAEMON_LOG_RETENTION,
        "per-process log history must stay bounded: {live_logs:?}"
    );
    let fresh = live_logs
        .iter()
        .filter(|name| !seeded_names.contains(name))
        .collect::<Vec<_>>();
    assert_eq!(
        fresh.len(),
        1,
        "exactly one new per-process log: {live_logs:?}"
    );
    let pruned = seeded - (haider_platform::DAEMON_LOG_RETENTION - 1);
    for (index, name) in seeded_names.iter().enumerate() {
        assert_eq!(
            live_logs.contains(name),
            index >= pruned,
            "prune keeps the newest seeds and removes the oldest: {name} in {live_logs:?}"
        );
    }
    let process_log = std::fs::read_to_string(logs.join(fresh[0])).expect("read live log");
    assert!(
        process_log.lines().count() >= 1 && process_log.ends_with('\n'),
        "the spawned daemon writes complete lines: {process_log:?}"
    );
    let published = std::fs::read_to_string(profile.profile.join(haider_platform::DAEMON_LOG_FILE))
        .expect("stable daemon.log is published");
    assert_eq!(
        published, process_log,
        "daemon.log names the live per-process log"
    );
}

/// The lockdown door on a machine-user home with NO policy reports the
/// default ceiling, refuses nothing, and accepts a quota change.
#[test]
fn fresh_daemon_lockdown_status_reports_defaults_without_a_policy() {
    let profile = Profile::new();
    let status = json_document(
        &bounded_output(
            profile.one_shot().args(["lockdown", "status", "--json"]),
            "lockdown status",
        ),
        "lockdown status",
    );
    assert_eq!(status["schema"], "haider.lockdown.v1");
    assert_eq!(status["status"]["quota_used"], 0);
    assert_eq!(status["status"]["quota_limit"], 1_073_741_824_u64);
    assert_eq!(
        status["status"]["tools_allowed"],
        serde_json::json!([
            "fs_read",
            "fs_glob",
            "fs_search",
            "fs_write",
            "request_input",
            "todo_write",
            "plan",
            "web_search",
            "web_fetch",
            "peer_list",
            "ssh_list",
            "spawn_subagent"
        ])
    );
    wait_for_daemon_gone(&profile.profile);

    let changed = json_document(
        &bounded_output(
            profile
                .one_shot()
                .args(["lockdown", "quota", "--set", "4096", "--json"]),
            "lockdown quota set",
        ),
        "lockdown quota set",
    );
    assert_eq!(changed["status"]["quota_limit"], 4096);
    assert_eq!(changed["status"]["quota_used"], 0);
    let ledger: serde_json::Value = serde_json::from_slice(
        &std::fs::read(profile.lockdown_root().join("quota.json")).expect("durable quota ledger"),
    )
    .expect("quota ledger JSON");
    assert_eq!(ledger["limit"], 4096);
}

/// With a lockdown ledger and provider data already on disk, the very first
/// command of a fresh daemon sees the reconciled usage (real bytes, not the
/// stale `used` field), refuses a quota below that usage with the typed
/// exit, and leaves the durable ledger untouched by the refusal.
///
/// MUTATION CHECK: skipping the startup reconcile reports `used: 0` and
/// accepts the lowering; reconciling lazily after the first response makes
/// the first `--set 10` succeed.
#[test]
fn fresh_daemon_reconciles_seeded_lockdown_quota_before_its_first_command() {
    let profile = Profile::new();
    let root = profile.lockdown_root();
    std::fs::create_dir_all(root.join("seeded-provider")).expect("seed lockdown data");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for directory in [profile.home.join(".haider"), root.clone()] {
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("owner-private lockdown root");
        }
    }
    std::fs::write(root.join("seeded-provider").join("blob"), [b'x'; 40]).expect("seed blob");
    std::fs::write(
        root.join("quota.json"),
        br#"{"version":1,"limit":64,"used":0}"#,
    )
    .expect("seed quota ledger");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            root.join("quota.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("owner-only ledger");
    }

    let status = json_document(
        &bounded_output(
            profile.one_shot().args(["lockdown", "status", "--json"]),
            "seeded lockdown status",
        ),
        "seeded lockdown status",
    );
    assert_eq!(
        status["status"]["quota_used"], 40,
        "reconciled from real bytes"
    );
    assert_eq!(
        status["status"]["quota_limit"], 64,
        "seeded ceiling honoured"
    );
    wait_for_daemon_gone(&profile.profile);

    let refused = bounded_output(
        profile
            .one_shot()
            .args(["lockdown", "quota", "--set", "10", "--json"]),
        "lowering the quota below reconciled use",
    );
    assert_eq!(
        refused.status.code(),
        Some(EX_PROTOCOL),
        "typed refusal exit: stdout={} stderr={}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        refused.stdout.is_empty(),
        "a refusal prints no status document"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("quota.json")).expect("ledger"))
            .expect("ledger JSON");
    assert_eq!(
        ledger["limit"], 64,
        "a refused change never reaches the ledger"
    );
    wait_for_daemon_gone(&profile.profile);

    let raised = json_document(
        &bounded_output(
            profile
                .one_shot()
                .args(["lockdown", "quota", "--set", "100", "--json"]),
            "raising the quota",
        ),
        "raising the quota",
    );
    assert_eq!(raised["status"]["quota_limit"], 100);
    assert_eq!(raised["status"]["quota_used"], 40);
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("quota.json")).expect("ledger"))
            .expect("ledger JSON");
    assert_eq!(ledger["limit"], 100);
    assert_eq!(ledger["used"], 40);
}

/// The built-in provider catalog a fresh profile exposes through `models
/// --json` is a golden (ids, families, endpoints, auth methods, seeded
/// inventories, default models). `provider list --json` names the same ids.
///
/// MUTATION CHECK: dropping a built-in, changing its family/endpoint, or
/// losing a seeded model row changes the golden.
#[test]
fn fresh_profile_models_catalog_matches_the_golden() {
    let profile = Profile::new();
    let mut models = json_document(
        &bounded_output(profile.one_shot().args(["models", "--json"]), "models"),
        "models",
    );
    assert_eq!(models["schema"], "haider.models.v1");
    let ids = models["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|provider| {
            provider["provider"]
                .as_str()
                .expect("provider id")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "catalog is sorted by provider id: {ids:?}"
    );
    normalize(&mut models);
    let actual = format!(
        "{}\n",
        serde_json::to_string_pretty(&models).expect("pretty models")
    );
    compare_or_update_golden("models_fresh_profile.json", &actual);
    wait_for_daemon_gone(&profile.profile);

    let list = json_document(
        &bounded_output(
            profile.one_shot().args(["provider", "list", "--json"]),
            "provider list",
        ),
        "provider list",
    );
    let mut listed = list["providers"]
        .as_array()
        .expect("provider summaries")
        .iter()
        .map(|provider| {
            provider["provider"]
                .as_str()
                .expect("provider id")
                .to_owned()
        })
        .collect::<Vec<_>>();
    listed.sort();
    assert_eq!(listed, ids, "both doors expose the same built-in catalog");
}

/// A user-added custom provider (the registry delta) survives a daemon
/// restart and is listed beside the complete built-in catalog with its
/// discovered inventory intact.
///
/// MUTATION CHECK: rebuilding the registry from the built-in catalog alone,
/// or losing the persisted delta on the one-shot shutdown, drops
/// `delta-proxy` from the second daemon's answer.
#[test]
fn custom_provider_delta_survives_daemon_restart_beside_the_builtin_catalog() {
    let profile = Profile::new();
    let catalog =
        CatalogServer::start(r#"{"object":"list","data":[{"id":"delta-model","object":"model"}]}"#);
    let origin = format!("http://{}/v1", catalog.address);

    let added = json_document(
        &bounded_output(
            profile.one_shot().args([
                "provider",
                "add",
                "delta-proxy",
                "--base-url",
                &origin,
                "--no-auth",
                "--json",
            ]),
            "provider add",
        ),
        "provider add",
    );
    assert_eq!(added["schema"], "haider.account.custom.v1");
    assert_eq!(added["alias"], "delta-proxy");
    assert_eq!(added["reachable"], true);
    assert_eq!(
        added["models"],
        serde_json::json!(["delta-proxy/delta-model"])
    );
    wait_for_daemon_gone(&profile.profile);

    let status = json_document(
        &bounded_output(profile.one_shot().args(["status", "--json"]), "status"),
        "status",
    );
    assert!(
        status["daemon"]["generation"]
            .as_u64()
            .is_some_and(|generation| generation >= 2),
        "the second command is served by a restarted daemon: {status}"
    );
    wait_for_daemon_gone(&profile.profile);

    let list = json_document(
        &bounded_output(
            profile.one_shot().args(["provider", "list", "--json"]),
            "provider list after restart",
        ),
        "provider list after restart",
    );
    let providers = list["providers"].as_array().expect("provider summaries");
    let delta = providers
        .iter()
        .find(|provider| provider["provider"] == "delta-proxy")
        .unwrap_or_else(|| panic!("delta provider lost across restart: {list}"));
    assert_eq!(delta["api_family"], "openai_chat_completions");
    assert_eq!(delta["endpoint"], origin);
    assert_eq!(delta["enabled"], true);
    assert_eq!(delta["trust"], "full");
    assert_eq!(delta["models"], serde_json::json!(["delta-model"]));
    assert_eq!(delta["default_model"], "delta-model");
    let golden: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixture_path("models_fresh_profile.json")).expect("models golden"),
    )
    .expect("models golden JSON");
    for builtin in golden["providers"].as_array().expect("golden providers") {
        let id = builtin["provider"].as_str().expect("golden id");
        assert!(
            providers.iter().any(|provider| provider["provider"] == id),
            "built-in {id} missing beside the delta: {list}"
        );
    }
    drop(catalog);
}

/// A persistent daemon's cache-diagnostic key is durable: the keyed
/// breakpoint hashes it journals for one prompt are identical after a clean
/// stop and restart, and the key file itself is unchanged.
///
/// MUTATION CHECK: regenerating the key on every boot (or never persisting
/// it) changes `breakpoint_hashes.system` between the two runs.
#[test]
fn persistent_daemon_cache_diagnostic_key_is_durable_across_restart() {
    let profile = Profile::new();
    let key_path = profile.profile.join("cache-diagnostic.key");
    let system_hash = |envelopes: &[RawEnvelope]| {
        envelopes
            .iter()
            .find(|envelope| envelope.payload["type"] == "usage")
            .map(|usage| usage.payload["request"]["cache"]["breakpoint_hashes"]["system"].clone())
            .expect("usage envelope with a cache diagnostic")
    };

    let first = bounded_output(
        profile
            .lingering()
            .env(FAKE_PROVIDER_ENV, fake_script())
            .args(["run", "--provider", "fake", "--jsonl", "same prompt"]),
        "first persistent run",
    );
    assert_success(&first, "first persistent run");
    let (_, first_envelopes) = parse_jsonl(&first);
    let first_hash = system_hash(&first_envelopes);
    assert!(
        first_hash
            .as_str()
            .is_some_and(|hash| hash.starts_with("blake3-keyed:")),
        "keyed diagnostic hash: {first_hash}"
    );
    let key = std::fs::read(&key_path).expect("persistent daemon publishes the durable key");
    assert_eq!(key.len(), 32, "the durable key is exactly 32 bytes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&key_path)
                .expect("key metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the durable key is owner-only"
        );
    }
    profile.stop_daemon_cleanly();
    assert_eq!(std::fs::read(&key_path).expect("key after stop"), key);

    let second = bounded_output(
        profile
            .lingering()
            .env(FAKE_PROVIDER_ENV, fake_script())
            .args(["run", "--provider", "fake", "--jsonl", "same prompt"]),
        "second persistent run",
    );
    assert_success(&second, "second persistent run");
    let (_, second_envelopes) = parse_jsonl(&second);
    assert!(
        second_envelopes[0].worker_generation > first_envelopes[0].worker_generation,
        "the second run is served by a restarted daemon"
    );
    assert_eq!(
        system_hash(&second_envelopes),
        first_hash,
        "the restarted daemon keys its diagnostics with the same durable key"
    );
    assert_eq!(std::fs::read(&key_path).expect("key after restart"), key);
    profile.stop_daemon_cleanly();
}

/// A text attachment accepted by a one-shot run is durable in the profile
/// CAS after the daemon is gone: the journal names its address and the
/// bytes are readable at that address by anyone opening the store.
///
/// MUTATION CHECK: skipping the object or directory publication on the
/// one-shot path leaves the address unreadable after exit.
#[test]
fn one_shot_attachment_is_durable_in_the_profile_cas() {
    const NOTE: &[u8] = b"oneshot cas marker line one\nline two\nline three\n";
    let profile = Profile::new();
    let note = profile.workspace.join("note.txt");
    std::fs::write(&note, NOTE).expect("write attachment");
    let output = bounded_output(
        profile
            .one_shot()
            .env(FAKE_PROVIDER_ENV, fake_script())
            .args([
                "run",
                "--provider",
                "fake",
                "--attach",
                "note.txt",
                "--jsonl",
                "read it",
            ]),
        "one-shot run with an attachment",
    );
    assert_success(&output, "one-shot run with an attachment");
    let (_, envelopes) = parse_jsonl(&output);
    assert_daemon_gone_now(&profile.profile, None);

    let attachment = envelopes
        .iter()
        .find(|envelope| envelope.payload["type"] == "user_message")
        .map(|message| message.payload["attachments"][0].clone())
        .expect("the accepted user message carries the attachment");
    assert_eq!(attachment["kind"], "file");
    assert_eq!(attachment["name"], "note.txt");
    assert_eq!(attachment["lines"], 3);
    let artifact = attachment["artifact"].as_str().expect("artifact address");
    let hex = artifact.strip_prefix("blake3:").expect("blake3 address");
    assert_eq!(hex.len(), 64);
    assert!(
        profile
            .profile
            .join("cas")
            .join(&hex[..2])
            .join(hex)
            .is_file(),
        "the CAS object is published under the profile namespace"
    );
    let cas = FileCas::open(&profile.profile).expect("open the profile CAS");
    assert_eq!(
        cas.get(&ArtifactRef::new(artifact))
            .expect("read the attachment back"),
        NOTE
    );
}

/// Minimal loopback catalog server: answers every request with one fixed
/// OpenAI-style model list so `provider add --no-auth` can discover.
struct CatalogServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl CatalogServer {
    fn start(body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind catalog server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking catalog listener");
        let address = listener.local_addr().expect("catalog address");
        let stop = Arc::new(AtomicBool::new(false));
        let observed_stop = Arc::clone(&stop);
        let task = thread::spawn(move || {
            while !observed_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("blocking catalog stream");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("catalog read timeout");
                        let mut buffer = [0_u8; 8192];
                        let mut request = Vec::new();
                        while let Ok(read) = stream.read(&mut buffer) {
                            if read == 0 {
                                break;
                            }
                            request.extend_from_slice(&buffer[..read]);
                            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        use std::io::Write as _;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stop,
            task: Some(task),
        }
    }
}

impl Drop for CatalogServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

/// A profile that has never seen a daemon answers `status --json --no-spawn`
/// with the typed unavailable exit and creates no daemon; the same profile
/// then boots one whose reported version is this build.
#[test]
fn fresh_profile_status_is_typed_without_a_daemon_and_reports_the_build_version_with_one() {
    let profile = Profile::new();
    let absent = bounded_output(
        profile.command().args(["status", "--json", "--no-spawn"]),
        "status without a daemon",
    );
    assert_eq!(absent.status.code(), Some(EX_UNAVAILABLE));
    assert!(!profile.profile.join("lock.owner").exists());

    let status = json_document(
        &bounded_output(
            profile.lingering().args(["status", "--json"]),
            "status with spawn",
        ),
        "status with spawn",
    );
    assert_eq!(status["schema"], "haider.observe.v1");
    assert_eq!(status["daemon"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["daemon"]["ready"], true);
    assert!(status["daemon"]["ready_since"].as_u64().is_some());
    assert_eq!(status["daemon"]["providers_loaded"], true);
    assert_eq!(status["daemon"]["generation"], 1);
    assert_eq!(status["session_count"], 0);
    assert_eq!(
        status["daemon"]["pid"].as_u64().map(|pid| pid as u32),
        daemon_pid(&profile.profile)
    );
    profile.stop_daemon_cleanly();
}
