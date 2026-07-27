//! Black-box tests for the `haider` binary surface.
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use async_trait::async_trait;
use haider_core::{CommittedRange, HarnessConfig, MemoryStore, StoreHandle};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep, Provider};
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tokio::time::timeout;

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod cli_main;

use cli_main::{exit_code_for_outcome, stream_jsonl_turn};

struct HaiderCommand {
    command: Command,
    _profile_root: tempfile::TempDir,
}

impl Deref for HaiderCommand {
    type Target = Command;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

impl DerefMut for HaiderCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

fn haider() -> HaiderCommand {
    let profile_root = tempfile::tempdir().expect("temporary CLI profile parent");
    let mut command = Command::new(env!("CARGO_BIN_EXE_haider"));
    command.env("HAIDER_PROFILE_DIR", profile_root.path().join("profile"));
    HaiderCommand {
        command,
        _profile_root: profile_root,
    }
}

#[test]
fn version_prints_workspace_version() {
    let out = haider().arg("--version").output().expect("binary runs");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(text.trim(), format!("haider {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn self_test_reports_ok_json() {
    let out = haider().arg("self-test").output().expect("binary runs");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert!(text.contains(r#""schema":"haider.selftest.v0""#));
    assert!(text.contains(r#""ok":true"#));
    assert!(text.contains("link:haider-protocol"));
    assert!(text.contains("link:haider-tui"));
    assert!(text.contains("fake-provider-turn"));
}

#[test]
fn unknown_command_exits_2() {
    let out = haider().arg("frobnicate").output().expect("binary runs");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn run_jsonl_is_lf_framed_and_every_line_is_a_raw_envelope() {
    let out = haider()
        .args(["run", "--jsonl", "hello"])
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    assert!(out.stdout.ends_with(b"\n"));
    assert!(!out.stdout.contains(&b'\r'));

    let text = String::from_utf8(out.stdout).expect("utf8");
    let envelopes: Vec<RawEnvelope> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("RawEnvelope JSONL line"))
        .collect();
    assert!(!envelopes.is_empty());
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1)
    );
    assert_eq!(
        envelopes.last().map(typed),
        Some(EventPayload::RunState(RunState::Done))
    );
    let response = envelopes.iter().find_map(|envelope| match typed(envelope) {
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::AgentMessage { text },
            ..
        }) => Some(text),
        _ => None,
    });
    assert_eq!(response.as_deref(), Some("fake response: hello"));
}

#[test]
fn run_jsonl_accepts_explicit_fake_provider_and_model() {
    let out = haider()
        .args([
            "run",
            "--jsonl",
            "--provider",
            "fake",
            "--model",
            "fixture-model",
            "hello",
        ])
        .output()
        .expect("binary runs");

    assert!(out.status.success());
    let envelopes = parse_jsonl(&out.stdout);
    assert_eq!(
        envelopes.last().map(typed),
        Some(EventPayload::RunState(RunState::Done))
    );
}

#[test]
fn anthropic_provider_requires_an_explicit_model() {
    let out = haider()
        .args(["run", "--jsonl", "--provider", "anthropic", "hello"])
        .output()
        .expect("binary runs");

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("requires --model"));
}

#[test]
fn unknown_run_provider_is_usage_error() {
    let out = haider()
        .args(["run", "--jsonl", "--provider", "unknown", "hello"])
        .output()
        .expect("binary runs");

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown provider"));
}

#[test]
fn anthropic_missing_credential_exits_65_without_network_access() {
    let out = haider()
        .args([
            "run",
            "--jsonl",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-5",
            "hello",
        ])
        .env_remove("HAIDER_ANTHROPIC_API_KEY")
        .output()
        .expect("binary runs");

    assert_eq!(out.status.code(), Some(65));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("HAIDER_ANTHROPIC_API_KEY"));
}

#[test]
fn sequential_cli_runs_use_profile_owned_worker_generations() {
    let profile_parent = tempfile::tempdir().expect("temporary CLI profile parent");
    let profile = profile_parent.path().join("profile");
    let run = |prompt: &str| {
        Command::new(env!("CARGO_BIN_EXE_haider"))
            .args(["run", "--jsonl", prompt])
            .env("HAIDER_PROFILE_DIR", &profile)
            .output()
            .expect("binary runs")
    };

    let first_output = run("first process");
    assert!(
        first_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let second_output = run("restarted process");
    assert!(
        second_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second_output.stderr)
    );

    let first = parse_jsonl(&first_output.stdout);
    let second = parse_jsonl(&second_output.stdout);
    let first_generation = first[0].worker_generation;
    let second_generation = second[0].worker_generation;
    assert!(
        first
            .iter()
            .all(|envelope| envelope.worker_generation == first_generation)
    );
    assert!(
        second
            .iter()
            .all(|envelope| envelope.worker_generation == second_generation)
    );
    assert!(
        second_generation > first_generation,
        "CLI reused profile generation {first_generation}"
    );
}

#[test]
fn run_jsonl_exits_65_when_fake_provider_errors() {
    let out = haider()
        .args(["run", "--jsonl", "hello"])
        .env("HAIDER_FAKE_SCRIPT_JSON", r#"[{"step":"malformed_frame"}]"#)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(65));
    let text = String::from_utf8(out.stdout).expect("utf8");
    let envelopes: Vec<RawEnvelope> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("RawEnvelope JSONL line"))
        .collect();
    assert_eq!(
        envelopes.last().map(typed),
        Some(EventPayload::RunState(RunState::Errored))
    );
}

#[test]
fn run_jsonl_cancelled_has_130_exit_and_terminal_envelope() {
    let out = haider()
        .args(["run", "--jsonl", "hello"])
        .env(
            "HAIDER_FAKE_SCRIPT_JSON",
            r#"[{"step":"finish","reason":"cancelled"}]"#,
        )
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(130));
    assert!(out.stderr.is_empty());
    let envelopes = parse_jsonl(&out.stdout);
    assert_eq!(
        envelopes.last().map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    );
}

#[test]
fn run_jsonl_replays_every_envelope_to_a_slow_pipe_consumer() {
    let mut steps: Vec<_> = (0..500)
        .map(|index| serde_json::json!({"step":"emit_text","text":index.to_string()}))
        .collect();
    steps.push(serde_json::json!({"step":"finish","reason":"end_turn"}));
    let script = serde_json::to_string(&steps).expect("fixture serializes");
    let mut command = haider();
    let mut child = command
        .args(["run", "--jsonl", "backpressure"])
        .env("HAIDER_FAKE_SCRIPT_JSON", script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary starts");
    let mut stdout = child.stdout.take().expect("stdout pipe");
    let mut stderr = child.stderr.take().expect("stderr pipe");

    // Let the OS pipe fill before beginning consumption. This used to make
    // the bounded broadcast receiver lag and truncate the JSONL stream.
    thread::sleep(Duration::from_millis(250));
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).expect("read stdout");
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).expect("read stderr");
        bytes
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out child");
            let _ = child.wait();
            panic!("slow-consumer run did not terminate");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader.join().expect("stdout reader");
    let stderr = stderr_reader.join().expect("stderr reader");

    assert!(
        status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(stderr.is_empty());
    let envelopes = parse_jsonl(&stdout);
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1)
    );
    let delta_count = envelopes
        .iter()
        .filter(|envelope| matches!(typed(envelope), EventPayload::Item(ItemEvent::Delta { .. })))
        .count();
    assert_eq!(delta_count, 500);
    assert_eq!(
        envelopes.last().map(typed),
        Some(EventPayload::RunState(RunState::Done))
    );
}

#[tokio::test]
async fn jsonl_store_failure_emits_errored_and_returns_nonzero_without_hanging() {
    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    // The first four appends reach Streaming. Failing the attempted Done
    // append reproduces the former wait-forever path; the next append must
    // commit Errored and the outcome must wake the JSONL runner.
    let store: Arc<dyn StoreHandle> = Arc::new(FailOnceStore::new(5));
    let config = HarnessConfig::for_session(
        SessionId::new("cli-failing-store"),
        DeviceId::new("cli-device"),
        1,
        9,
    );
    let mut output = Vec::new();

    let outcome = timeout(
        Duration::from_secs(1),
        stream_jsonl_turn("store failure", config, provider, store, &mut output),
    )
    .await
    .expect("JSONL runner must not hang")
    .expect("runner reports the turn outcome");

    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.as_ref().map(|error| error.code),
        Some(ErrorCode::StoreCorrupt)
    );
    assert_eq!(exit_code_for_outcome(&outcome), 70);
    let envelopes = parse_jsonl(&output);
    assert_eq!(
        envelopes.last().map(typed),
        Some(EventPayload::RunState(RunState::Errored))
    );
}

fn parse_jsonl(output: &[u8]) -> Vec<RawEnvelope> {
    String::from_utf8(output.to_vec())
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("RawEnvelope JSONL line"))
        .collect()
}

fn typed(envelope: &RawEnvelope) -> EventPayload {
    serde_json::from_value(envelope.payload.clone()).expect("known payload")
}

struct FailOnceStore {
    inner: MemoryStore,
    fail_on_append: usize,
    append_count: AtomicUsize,
}

impl FailOnceStore {
    fn new(fail_on_append: usize) -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_on_append,
            append_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl StoreHandle for FailOnceStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let append = self.append_count.fetch_add(1, Ordering::SeqCst) + 1;
        if append == self.fail_on_append {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "injected append failure",
                false,
            ));
        }
        self.inner.append(envelopes).await
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.read(session_id, since_seq, limit).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.inner.latest_seq(session_id).await
    }
}

#[test]
fn tui_demo_with_piped_stdout_renders_plain() {
    let out = haider()
        .args(["tui", "--demo"])
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert!(text.contains("❯ fix the failing boundary test in haider-store"));
    assert!(text.contains("✓ plan — 3/3 done"));
    assert!(
        text.lines()
            .last()
            .expect("status line")
            .starts_with("IDLE")
    );
}

#[test]
fn tui_demo_plain_flag_matches_piped_output() {
    let piped = haider()
        .args(["tui", "--demo"])
        .output()
        .expect("binary runs");
    let flagged = haider()
        .args(["tui", "--demo", "--plain", "--theme", "dark"])
        .output()
        .expect("binary runs");
    assert!(flagged.status.success());
    assert_eq!(piped.stdout, flagged.stdout, "plain output is theme-free");
}

#[test]
fn tui_without_demo_rejects_the_demo_only_plain_oracle() {
    // DIRECTED CHANGE (W3c3, report §6.3: "bare `haider` and `haider tui`
    // enter live mode"). This test used to pin "only `haider tui --demo` is
    // available until the daemon lands" — a law the keystone DELETES, so
    // pinning it would pin the pre-W3c3 world. The usage law that survives
    // is the one that still has meaning: `--plain` is the DEMO's
    // deterministic oracle and has no live counterpart, so asking for it
    // without `--demo` is a usage error (2), never a silent no-op that
    // leaves the user waiting for text that will never come.
    //
    // The live entry itself needs a daemon and is covered by
    // scripts/tui-probes/pty-probe-live.py, not by an exit-code assertion.
    let out = haider()
        .args(["tui", "--plain"])
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(2));
    let out = haider()
        .args(["tui", "--nonsense"])
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(2), "an unknown tui flag is usage");
}

#[test]
fn tui_rejects_bad_theme() {
    let out = haider()
        .args(["tui", "--demo", "--theme", "sepia"])
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(2));
}
