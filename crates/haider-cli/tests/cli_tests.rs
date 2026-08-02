//! Black-box tests for the `haider` binary surface.
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use haider_protocol::EventPayload;
use haider_protocol::effect::{AuthorizationVerdict, EffectPhase};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::state::RunState;
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod cli_main;

use cli_main::run::{
    EX_BLOCKED, EX_CANCELLED, EX_IOERR, EX_PROTOCOL, EX_PROVIDER, EX_SOFTWARE, EX_TIMEOUT,
    EX_UNAVAILABLE, EX_USAGE, ProviderSelection, RunOptions, RunOutput, exit_code_for_error,
    exit_code_for_result, parse_run_options, write_final,
};
use cli_main::{ImportDispatch, ImportSource, parse_import_dispatch};
use haider_client::{
    DisconnectReason, EnsureError, HeadlessBlockingReason, HeadlessFailureCode, HeadlessOutcome,
    HeadlessPermissionDenial, HeadlessRunError, HeadlessRunFailure, HeadlessRunResult,
    load_image_attachment,
};

const DEFAULT_FAKE_SCRIPT: &str = concat!(
    r#"[{"step":"emit_text","text":"fake response: hello"},{"step":"finish","reason":"end_turn"},"#,
    r#"{"step":"emit_text","text":"fake response: hello"},{"step":"finish","reason":"end_turn"},"#,
    r#"{"step":"emit_text","text":"fake response: hello"},{"step":"finish","reason":"end_turn"},"#,
    r#"{"step":"emit_text","text":"fake response: hello"},{"step":"finish","reason":"end_turn"}]"#,
);

struct HaiderCommand {
    command: Command,
    _profile_root: tempfile::TempDir,
    profile: PathBuf,
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
    ensure_haiderd_built();
    let profile_root = tempfile::tempdir().expect("temporary CLI profile parent");
    let profile = profile_root.path().join("profile");
    let mut command = Command::new(env!("CARGO_BIN_EXE_haider"));
    // Hermetic workspace: the daemon's project-instruction walk climbs to
    // the filesystem root, so an inherited repo cwd would let the OWNER'S
    // real ~/AGENTS.md into every test daemon's prompt and journal.
    let workspace = profile_root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    command
        .current_dir(&workspace)
        .env("HAIDER_PROFILE_DIR", &profile)
        .env("HAIDER_TEST_FAKE_PROVIDER", DEFAULT_FAKE_SCRIPT);
    HaiderCommand {
        command,
        _profile_root: profile_root,
        profile,
    }
}

impl Drop for HaiderCommand {
    fn drop(&mut self) {
        terminate_daemon(&self.profile);
    }
}

fn ensure_haiderd_built() {
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-p", "haider-daemond", "--bin", "haiderd"])
            .status()
            .expect("build haiderd for run tests");
        assert!(status.success(), "haiderd build failed");
    });
}

fn daemon_pid(profile: &Path) -> Option<u32> {
    std::fs::read_to_string(profile.join("lock"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("pid="))?
        .trim()
        .parse()
        .ok()
}

fn terminate_daemon(profile: &Path) {
    if let Some(pid) = daemon_pid(profile) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
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

/// MUTATION CHECK: route `codex` to `ClaudeCode`. Expected runtime failure:
/// the parsed source differs from the source the daemon must read.
#[test]
fn import_codex_dispatches_to_codex_source() {
    assert_eq!(
        parse_import_dispatch(&["codex".to_owned()]),
        Ok(ImportDispatch::Source(ImportSource::Codex))
    );
}

/// MUTATION CHECK: reject the `claude-code` match arm. Expected runtime
/// failure: this supported source parses as an error.
#[test]
fn import_claude_code_dispatches_to_claude_source() {
    assert_eq!(
        parse_import_dispatch(&["claude-code".to_owned()]),
        Ok(ImportDispatch::Source(ImportSource::ClaudeCode))
    );
}

/// MUTATION CHECK: make bare import default to Codex. Expected runtime
/// failure: the parser performs an import instead of selecting the safe
/// existence-only listing.
#[test]
fn bare_import_dispatches_to_source_listing() {
    assert_eq!(parse_import_dispatch(&[]), Ok(ImportDispatch::List));
}

/// MUTATION CHECK: accept an arbitrary import source. Expected runtime
/// failure: `other-cli` no longer returns the usage error asserted here.
#[test]
fn unknown_import_source_is_rejected() {
    let error = parse_import_dispatch(&["other-cli".to_owned()]).expect_err("unknown source");
    assert!(error.contains("unknown source `other-cli`"));
}

#[test]
fn run_jsonl_is_lf_framed_and_every_line_is_a_raw_envelope() {
    let out = haider()
        .args(["run", "--provider", "fake", "--jsonl", "hello"])
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
        Some(Some(EventPayload::RunState(RunState::Done)))
    );
    let response = envelopes
        .iter()
        .find_map(|envelope| match typed(envelope)? {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => Some(text),
            _ => None,
        });
    assert_eq!(response.as_deref(), Some("fake response: hello"));
}

/// MUTATION CHECK: make print depend on a TTY/TERM, leak progress to stdout,
/// omit the one trailing LF, or put the final response on stderr. Expected
/// RUNTIME failure: redirected subprocess bytes differ from this exact split.
#[test]
fn run_default_print_is_exact_under_redirected_no_term_io() {
    let mut out = haider()
        .args(["run", "--provider", "fake", "hello"])
        .env_remove("TERM")
        .stdin(Stdio::null())
        .output()
        .expect("binary runs");
    // Bounded retry for the transient class ONLY: under full-gate load the
    // autospawned daemon can miss its startup deadline (exit 69). A real
    // print-contract regression exits differently and never retries.
    if out.status.code() == Some(69) {
        out = haider()
            .args(["run", "--provider", "fake", "hello"])
            .env_remove("TERM")
            .stdin(Stdio::null())
            .output()
            .expect("binary runs");
    }
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"fake response: hello\n");
    assert!(out.stderr.is_empty());
}

#[test]
fn run_jsonl_accepts_explicit_fake_provider_and_model() {
    let mut out = haider()
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
    // Bounded retry for the transient class ONLY (exit 69 = the autospawned
    // daemon missed its startup deadline under full-gate load).
    if out.status.code() == Some(69) {
        out = haider()
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
    }
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let envelopes = parse_jsonl(&out.stdout);
    assert_eq!(
        envelopes.last().map(typed),
        Some(Some(EventPayload::RunState(RunState::Done)))
    );
}

/// MUTATION CHECK: restore the parser's closed provider allowlist or reject
/// before session.create. Expected RUNTIME failure: the process returns usage
/// 2 instead of the daemon's typed create refusal and protocol exit.
#[test]
fn unknown_run_provider_surfaces_daemon_create_refusal() {
    let out = haider()
        .args([
            "run",
            "--output",
            "json",
            "--provider",
            "unknown",
            "--model",
            "fixture-model",
            "hello",
        ])
        .output()
        .expect("binary runs");

    assert_eq!(out.status.code(), Some(76));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("refusal JSON");
    assert_eq!(value["error"]["code"], "invalid_argument");
    assert_eq!(value["provider"], "unknown");
    assert_eq!(value["model"], "fixture-model");
    assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported session provider"));
}

/// MUTATION CHECK: fall back to profile defaults when no provider flag is
/// present. Expected RUNTIME failure: the fresh profile creates a fake session
/// instead of returning typed no_active_account with an actionable remedy.
#[test]
fn flagless_run_without_an_active_account_exits_65_with_remedy() {
    let out = haider()
        .args(["run", "hello", "--output", "json"])
        .output()
        .expect("binary runs");

    assert_eq!(out.status.code(), Some(65));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("error JSON");
    assert_eq!(value["error"]["code"], "no_active_account");
    assert!(value["provider"].is_null());
    assert!(value["model"].is_null());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no_active_account"));
    assert!(stderr.contains("remedy:"));
    assert!(stderr.contains("TUI"));
}

#[test]
fn anthropic_missing_credential_exits_65_without_network_access() {
    // Hermetic profile: without this the test inherits the developer's
    // real ~/.haider (real credentials would defeat the missing-key law).
    let profile_parent = tempfile::tempdir().expect("temporary CLI profile parent");
    // One bounded retry: under full-gate load the cold daemon spawn can
    // miss the startup deadline (exit 69 = unavailable) before the
    // credential law is even reachable. The law under test is the 65
    // classification, not spawn latency.
    let mut out = None;
    for _ in 0..2 {
        let attempt = haider()
            .env("HAIDER_PROFILE_DIR", profile_parent.path().join("profile"))
            .args([
                "run",
                "--jsonl",
                "--provider",
                "anthropic",
                "--model",
                "claude-sonnet-5",
                "hello",
            ])
            .env_remove("HAIDER_TEST_FAKE_PROVIDER")
            .env_remove("HAIDER_ANTHROPIC_API_KEY")
            .output()
            .expect("binary runs");
        let unavailable = attempt.status.code() == Some(69);
        out = Some(attempt);
        if !unavailable {
            break;
        }
    }
    let out = out.expect("at least one attempt ran");

    assert_eq!(out.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&out.stderr).contains("HAIDER_ANTHROPIC_API_KEY"));
    // W9b migration: provider resolution belongs to the daemon after durable
    // acceptance, so JSONL exposes the resulting Errored audit trail instead
    // of performing a second client-side credential preflight.
    let envelopes = parse_jsonl(&out.stdout);
    assert_eq!(
        envelopes.last().map(typed),
        Some(Some(EventPayload::RunState(RunState::Errored)))
    );
}

#[test]
fn sequential_cli_runs_use_profile_owned_worker_generations() {
    ensure_haiderd_built();
    let profile_parent = tempfile::tempdir().expect("temporary CLI profile parent");
    let profile = profile_parent.path().join("profile");
    let run = |prompt: &str| {
        Command::new(env!("CARGO_BIN_EXE_haider"))
            .args(["run", "--provider", "fake", "--jsonl", prompt])
            .env("HAIDER_PROFILE_DIR", &profile)
            .env("HAIDER_TEST_FAKE_PROVIDER", DEFAULT_FAKE_SCRIPT)
            .output()
            .expect("binary runs")
    };

    let first_output = run("first process");
    assert!(
        first_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let first = parse_jsonl(&first_output.stdout);
    let first_generation = first[0].worker_generation;
    assert!(
        first
            .iter()
            .all(|envelope| envelope.worker_generation == first_generation)
    );
    terminate_daemon(&profile);
    let resolved = haider_client::resolve_profile(&haider_client::ProfileEnv {
        profile_dir: Some(profile.clone()),
        home: None,
        model: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve profile");
    let deadline = Instant::now() + Duration::from_secs(5);
    while resolved.endpoint_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let second_output = run("restarted process");
    terminate_daemon(&profile);
    assert!(
        second_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    let second = parse_jsonl(&second_output.stdout);
    let second_generation = second[0].worker_generation;
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
        .args(["run", "--provider", "fake", "--jsonl", "hello"])
        .env(
            "HAIDER_TEST_FAKE_PROVIDER",
            r#"[{"step":"malformed_frame"}]"#,
        )
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
        Some(Some(EventPayload::RunState(RunState::Errored)))
    );
}

#[test]
fn run_jsonl_cancelled_has_130_exit_and_terminal_envelope() {
    let out = haider()
        .args(["run", "--provider", "fake", "--jsonl", "hello"])
        .env(
            "HAIDER_TEST_FAKE_PROVIDER",
            r#"[{"step":"finish","reason":"cancelled"}]"#,
        )
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(130));
    assert!(out.stderr.is_empty());
    let envelopes = parse_jsonl(&out.stdout);
    assert_eq!(
        envelopes.last().map(typed),
        Some(Some(EventPayload::RunState(RunState::Cancelled)))
    );
}

/// MUTATION CHECK: let the later Cancelled terminal overwrite a wall-clock
/// timeout or emit a success object. Expected RUNTIME failure: exit is not
/// 124 or the v1 outcome/error stop reporting timeout.
#[test]
fn run_timeout_emits_timeout_json_and_exits_124() {
    let out = haider()
        .args([
            "run",
            "--provider",
            "fake",
            "hello",
            "--output",
            "json",
            "--timeout",
            "20ms",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", r#"[{"step":"hang"}]"#)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(124));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("timeout JSON");
    assert_eq!(value["schema"], "haider.run.v1");
    assert_eq!(value["outcome"], "timeout");
    assert!(value["response"].is_null());
    assert_eq!(value["error"]["code"], "timeout");
}

/// MUTATION CHECK: invent an answer for a non-permission input menu or leave
/// it parked forever. Expected RUNTIME failure: the bounded command does not
/// return exit 77 with the typed input_required v1 object.
#[test]
fn run_nonpermission_input_cancels_and_exits_77() {
    let script = r#"[
        {"step":"emit_request_input","call_id":"ask","kind":"question","title":"Need input"},
        {"step":"finish","reason":"tool_use"}
    ]"#;
    let out = haider()
        .args(["run", "--provider", "fake", "hello", "--output", "json"])
        .env("HAIDER_TEST_FAKE_PROVIDER", script)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(77));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("blocked JSON");
    assert_eq!(value["outcome"], "input_required");
    assert_eq!(value["error"]["code"], "input_required");
}

/// MUTATION CHECK: silently approve the default Ask, fail to persist/apply
/// `--allow-writes`, or forge `PreAuthorized(UserTyped)`. Expected RUNTIME
/// failure: the default run writes, the flagged run still asks, or its durable
/// authorization verdict is not ordinary Allow.
#[test]
fn run_write_and_exec_permission_flags_journal_ordinary_allow() {
    let script = r#"[
        {"step":"emit_tool_call","call_id":"write-1","name":"fs_write","args":{"path":"created.txt","content":"ok"}},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"write-1"},
        {"step":"emit_text","text":"continued"},
        {"step":"finish","reason":"end_turn"}
    ]"#;

    let denied_workspace = tempfile::tempdir().expect("denied workspace");
    let denied = haider()
        .current_dir(denied_workspace.path())
        .args(["run", "--provider", "fake", "write", "--output", "json"])
        .env("HAIDER_TEST_FAKE_PROVIDER", script)
        .output()
        .expect("denied run");
    assert!(denied.status.success());
    assert!(!denied_workspace.path().join("created.txt").exists());
    let denied_json: serde_json::Value =
        serde_json::from_slice(&denied.stdout).expect("denied JSON");
    assert_eq!(denied_json["outcome"], "done");
    assert_eq!(
        denied_json["permission_denials"].as_array().map(Vec::len),
        Some(1)
    );

    let allowed_workspace = tempfile::tempdir().expect("allowed workspace");
    let allowed = haider()
        .current_dir(allowed_workspace.path())
        .args([
            "run",
            "--provider",
            "fake",
            "write",
            "--jsonl",
            "--allow-writes",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", script)
        .output()
        .expect("allowed run");
    assert!(allowed.status.success());
    assert_eq!(
        std::fs::read_to_string(allowed_workspace.path().join("created.txt"))
            .expect("allowed file"),
        "ok"
    );
    let envelopes = parse_jsonl(&allowed.stdout);
    assert!(envelopes.iter().any(|envelope| matches!(
        typed(envelope),
        Some(EventPayload::Effect(EffectPhase::Authorized {
            verdict: AuthorizationVerdict::Allow,
            ..
        }))
    )));
    assert!(!envelopes.iter().any(|envelope| matches!(
        typed(envelope),
        Some(EventPayload::Effect(EffectPhase::Authorized {
            verdict: AuthorizationVerdict::PreAuthorized { .. },
            ..
        }))
    )));

    let exec_workspace = tempfile::tempdir().expect("exec workspace");
    let exec_script = serde_json::json!([
        {
            "step": "emit_tool_call",
            "call_id": "exec-1",
            "name": "exec",
            "args": {
                "command": "printf ok > exec-created.txt",
                "cwd": exec_workspace.path().to_str().expect("UTF-8 exec workspace")
            }
        },
        {"step": "finish", "reason": "tool_use"},
        {"step": "expect_tool_result", "call_id": "exec-1"},
        {"step": "emit_text", "text": "continued"},
        {"step": "finish", "reason": "end_turn"}
    ])
    .to_string();
    let exec = haider()
        .current_dir(exec_workspace.path())
        .args([
            "run",
            "--provider",
            "fake",
            "execute",
            "--jsonl",
            "--allow-exec",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", &exec_script)
        .output()
        .expect("allowed exec run");
    assert!(exec.status.success());
    assert_eq!(
        std::fs::read_to_string(exec_workspace.path().join("exec-created.txt"))
            .expect("allowed exec file"),
        "ok"
    );
    let exec_envelopes = parse_jsonl(&exec.stdout);
    assert!(exec_envelopes.iter().any(|envelope| matches!(
        typed(envelope),
        Some(EventPayload::Effect(EffectPhase::Authorized {
            verdict: AuthorizationVerdict::Allow,
            ..
        }))
    )));
    assert!(!exec_envelopes.iter().any(|envelope| matches!(
        typed(envelope),
        Some(EventPayload::Effect(EffectPhase::Authorized {
            verdict: AuthorizationVerdict::PreAuthorized { .. },
            ..
        }))
    )));
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
        .args(["run", "--provider", "fake", "--jsonl", "backpressure"])
        .env("HAIDER_TEST_FAKE_PROVIDER", script)
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

    // Cold daemon spawn + 500 provider rounds + drain on a loaded box
    // regularly exceeds 5s — the LAW is termination + complete replay,
    // not latency; the bound only guards a true wedge.
    let deadline = Instant::now() + Duration::from_secs(30);
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
        .filter(|envelope| {
            matches!(
                typed(envelope),
                Some(EventPayload::Item(ItemEvent::Delta { .. }))
            )
        })
        .count();
    assert_eq!(delta_count, 500);
    assert_eq!(
        envelopes.last().map(typed),
        Some(Some(EventPayload::RunState(RunState::Done)))
    );
}

/// MIGRATION ORACLE: the former in-process store injection pinned an
/// Errored/StoreCorrupt JSONL terminal as nonzero 70 without a wait-forever
/// path. The one-shot fault now lives at the daemon worker-store boundary, so
/// this remains a real sibling-daemon CLI test without a second run authority.
///
/// MUTATION CHECK: remove the daemon fault/fallback, wait after the adjacent
/// terminal, or map StoreCorrupt to success/provider failure. Expected RUNTIME
/// failure: the bound fires, exit 70 changes, or the final two raw
/// envelopes are no longer RunFailed(StoreCorrupt) then Errored.
#[test]
fn jsonl_store_failure_emits_errored_and_returns_nonzero_without_hanging() {
    let mut command = haider();
    let mut child = command
        .args(["run", "--provider", "fake", "store failure", "--jsonl"])
        .env("HAIDER_TEST_FAIL_NEXT_DONE_APPEND", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary starts");
    // Cold daemon spawn + turn + fault handling regularly exceeds 5s on a
    // loaded box — the LAW is termination + the typed trail, not latency.
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out child");
            let _ = child.wait();
            panic!("store-failure run did not terminate");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_end(&mut stdout)
        .expect("read stdout");
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_end(&mut stderr)
        .expect("read stderr");

    assert_eq!(status.code(), Some(i32::from(EX_SOFTWARE)));
    let envelopes = parse_jsonl(&stdout);
    let terminal = envelopes
        .iter()
        .rev()
        .take(2)
        .map(typed)
        .collect::<Vec<_>>();
    assert_eq!(
        terminal,
        vec![
            Some(EventPayload::RunState(RunState::Errored)),
            Some(EventPayload::RunFailed {
                code: ErrorCode::StoreCorrupt,
                message: "injected terminal append failure".into(),
                retryable: false,
            }),
        ]
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains("injected terminal append failure"),
        "stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
}

fn result(outcome: HeadlessOutcome, failure: Option<HeadlessRunFailure>) -> HeadlessRunResult {
    HeadlessRunResult {
        session_id: SessionId::new("session-json"),
        run_id: RunId::new("run-json"),
        provider: "fake".into(),
        model: "fake-model".into(),
        attachments: Vec::new(),
        outcome,
        response: None,
        usage: None,
        permission_denials: Vec::new(),
        failure,
        terminal_seq: Some(9),
    }
}

/// MUTATION CHECK: alter any stable exit mapping. Expected RUNTIME failure:
/// the corresponding table row differs, including denied-then-Done,
/// RunFailed provider codes, blocked input, timeout, cancel, transport, and
/// pre-acceptance RPC/daemon failures.
#[test]
fn run_exit_codes_are_table_driven() {
    let mut denied_done = result(HeadlessOutcome::Done, None);
    denied_done
        .permission_denials
        .push(HeadlessPermissionDenial {
            menu_id: "menu-1".into(),
            effect_summary: "write file".into(),
            notice: "permission_denied_by_headless_default".into(),
        });
    let terminal_cases = [
        (denied_done, 0),
        (
            result(
                HeadlessOutcome::Errored,
                Some(HeadlessRunFailure {
                    code: HeadlessFailureCode::Run(ErrorCode::ProviderError),
                    message: "provider".into(),
                    retryable: false,
                }),
            ),
            EX_PROVIDER,
        ),
        (
            result(
                HeadlessOutcome::Errored,
                Some(HeadlessRunFailure {
                    code: HeadlessFailureCode::Run(ErrorCode::ProviderTimeout),
                    message: "provider timeout".into(),
                    retryable: true,
                }),
            ),
            EX_PROVIDER,
        ),
        (result(HeadlessOutcome::Cancelled, None), EX_CANCELLED),
        (result(HeadlessOutcome::Timeout, None), EX_TIMEOUT),
        (
            result(
                HeadlessOutcome::InputRequired,
                Some(HeadlessRunFailure {
                    code: HeadlessFailureCode::Blocked(HeadlessBlockingReason::InputRequired),
                    message: "input".into(),
                    retryable: false,
                }),
            ),
            EX_BLOCKED,
        ),
        (
            result(
                HeadlessOutcome::Errored,
                Some(HeadlessRunFailure {
                    code: HeadlessFailureCode::Run(ErrorCode::ProtocolMismatch),
                    message: "protocol".into(),
                    retryable: false,
                }),
            ),
            EX_PROTOCOL,
        ),
        (
            result(
                HeadlessOutcome::Errored,
                Some(HeadlessRunFailure {
                    code: HeadlessFailureCode::Run(ErrorCode::PermissionDenied),
                    message: "permission".into(),
                    retryable: false,
                }),
            ),
            EX_BLOCKED,
        ),
        (
            result(
                HeadlessOutcome::Errored,
                Some(HeadlessRunFailure {
                    code: HeadlessFailureCode::Run(ErrorCode::EffectUnknownOutcome),
                    message: "unknown effect".into(),
                    retryable: false,
                }),
            ),
            EX_BLOCKED,
        ),
        (
            result(
                HeadlessOutcome::Errored,
                Some(HeadlessRunFailure {
                    code: HeadlessFailureCode::Internal,
                    message: "internal".into(),
                    retryable: false,
                }),
            ),
            EX_SOFTWARE,
        ),
    ];
    for (result, expected) in terminal_cases {
        assert_eq!(exit_code_for_result(&result), expected, "{result:?}");
    }

    let pre_accept_cases = [
        (
            HeadlessRunError::Rpc {
                stage: "session.create",
                code: "credential_missing".into(),
                message: "missing".into(),
                retryable: false,
            },
            EX_PROVIDER,
        ),
        (
            HeadlessRunError::Protocol {
                stage: "session.create",
                message: "wrong coordinates".into(),
            },
            EX_PROTOCOL,
        ),
        (
            HeadlessRunError::Ensure(EnsureError::Spawn {
                binary: PathBuf::from("haiderd"),
                message: "missing".into(),
            }),
            EX_UNAVAILABLE,
        ),
        (
            HeadlessRunError::Ensure(EnsureError::MissingFeatures {
                missing: std::collections::BTreeSet::from([
                    haider_rpc::FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned(),
                ]),
                daemon_version: "old".into(),
            }),
            EX_PROTOCOL,
        ),
        (
            HeadlessRunError::Transport {
                stage: "stream",
                reason: DisconnectReason::PeerClosed,
            },
            EX_IOERR,
        ),
        (
            HeadlessRunError::Rpc {
                stage: "turn.submit",
                code: "busy".into(),
                message: "busy".into(),
                retryable: true,
            },
            EX_SOFTWARE,
        ),
        (
            HeadlessRunError::Rpc {
                stage: "turn.submit",
                code: "timeout_before_acceptance".into(),
                message: "timeout".into(),
                retryable: true,
            },
            EX_TIMEOUT,
        ),
        (
            HeadlessRunError::Bootstrap {
                stage: "account.list",
                code: haider_client::ERROR_CODE_NO_ACTIVE_ACCOUNT,
                message: "no active daemon account is configured".into(),
                retryable: false,
            },
            EX_PROVIDER,
        ),
    ];
    for (error, expected) in pre_accept_cases {
        assert_eq!(exit_code_for_error(&error), expected, "{error}");
    }
    assert_eq!(EX_USAGE, 2);
}

/// MUTATION CHECK: change the manual parser's default, timeout bounds, or
/// flag propagation. Expected RUNTIME failure: one of these exact options or
/// usage errors changes.
#[test]
fn run_parser_pins_outputs_timeouts_and_permission_flags() {
    assert_eq!(
        parse_run_options(&["hello".into()]),
        Ok(RunOptions {
            prompt: "hello".into(),
            output: RunOutput::Print,
            timeout: None,
            allow_writes: false,
            allow_exec: false,
            provider: None,
            model: None,
            attachments: Vec::new(),
        })
    );
    let parsed = parse_run_options(&[
        "hello".into(),
        "--output".into(),
        "json".into(),
        "--timeout".into(),
        "1500ms".into(),
        "--allow-writes".into(),
        "--allow-exec".into(),
        "--provider".into(),
        "fake".into(),
        "--model".into(),
        "fixture".into(),
        "--attach".into(),
        "/tmp/one.png".into(),
        "--attach".into(),
        "/tmp/two.gif".into(),
    ])
    .expect("full options");
    assert_eq!(parsed.output, RunOutput::Json);
    assert_eq!(parsed.timeout, Some(Duration::from_millis(1500)));
    assert!(parsed.allow_writes && parsed.allow_exec);
    assert_eq!(
        parsed.provider.as_ref().map(ProviderSelection::as_str),
        Some("fake")
    );
    assert_eq!(parsed.model.as_deref(), Some("fixture"));
    assert_eq!(
        parsed.attachments,
        vec![PathBuf::from("/tmp/one.png"), PathBuf::from("/tmp/two.gif")]
    );
    let open_provider =
        parse_run_options(&["hello".into(), "--provider".into(), "openai-oauth".into()])
            .expect("provider names are daemon-owned");
    assert_eq!(
        open_provider
            .provider
            .as_ref()
            .map(ProviderSelection::as_str),
        Some("openai-oauth")
    );
    assert!(open_provider.model.is_none());
    assert!(
        parse_run_options(&["hello".into(), "--provider".into(), "anthropic".into(),]).is_ok(),
        "a provider's published default may supply the model"
    );
    assert_eq!(
        parse_run_options(&["--jsonl".into(), "hello".into()])
            .expect("legacy alias")
            .output,
        RunOutput::Jsonl
    );
    for invalid in ["0s", "86400001ms", "1.5s", "forever"] {
        assert!(
            parse_run_options(&["hello".into(), "--timeout".into(), invalid.into()]).is_err(),
            "{invalid} must be refused"
        );
    }
}

/// MUTATION CHECK: reorder/remove a v1 field, omit nulls, add ANSI, or stop
/// writing exactly one LF after assistant text/JSON. Expected RUNTIME failure:
/// the byte golden or the eleven-key/null assertions change.
#[test]
fn print_and_json_outputs_pin_bytes_schema_and_nulls() {
    let mut done = result(HeadlessOutcome::Done, None);
    done.response = Some("final answer".into());
    let mut print = Vec::new();
    write_final(&mut print, RunOutput::Print, &done).expect("print");
    assert_eq!(print, b"final answer\n");

    let mut json = Vec::new();
    write_final(&mut json, RunOutput::Json, &done).expect("json");
    assert_eq!(
        String::from_utf8(json.clone()).expect("utf8"),
        "{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{\"count\":0,\"refs\":[]},\"outcome\":\"done\",\"response\":\"final answer\",\"usage\":null,\"permission_denials\":[],\"error\":null}\n"
    );
    let value: serde_json::Value = serde_json::from_slice(&json).expect("v1 JSON");
    assert_eq!(value.as_object().expect("object").len(), 11);
    assert_eq!(value["provider"], "fake");
    assert_eq!(value["model"], "fake-model");
    assert!(value["usage"].is_null());
    assert!(value["error"].is_null());

    done.permission_denials.push(HeadlessPermissionDenial {
        menu_id: "menu-json".into(),
        effect_summary: "run command".into(),
        notice: "permission_denied_by_headless_default".into(),
    });
    let mut denied_json = Vec::new();
    write_final(&mut denied_json, RunOutput::Json, &done).expect("denied JSON");
    let denied: serde_json::Value = serde_json::from_slice(&denied_json).expect("denied object");
    assert_eq!(denied["permission_denials"][0]["menu_id"], "menu-json");

    for outcome in [
        HeadlessOutcome::Errored,
        HeadlessOutcome::Cancelled,
        HeadlessOutcome::Timeout,
        HeadlessOutcome::InputRequired,
    ] {
        let failure = (outcome != HeadlessOutcome::Cancelled).then(|| HeadlessRunFailure {
            code: if outcome == HeadlessOutcome::InputRequired {
                HeadlessFailureCode::Blocked(HeadlessBlockingReason::InputRequired)
            } else if outcome == HeadlessOutcome::Timeout {
                HeadlessFailureCode::Timeout
            } else {
                HeadlessFailureCode::Run(ErrorCode::Internal)
            },
            message: "failure".into(),
            retryable: false,
        });
        let failed = result(outcome, failure);
        let mut bytes = Vec::new();
        write_final(&mut bytes, RunOutput::Json, &failed).expect("failure JSON");
        let (outcome_name, error) = match outcome {
            HeadlessOutcome::Errored => (
                "errored",
                r#"{"code":"internal","message":"failure","retryable":false}"#,
            ),
            HeadlessOutcome::Cancelled => ("cancelled", "null"),
            HeadlessOutcome::Timeout => (
                "timeout",
                r#"{"code":"timeout","message":"failure","retryable":false}"#,
            ),
            HeadlessOutcome::InputRequired => (
                "input_required",
                r#"{"code":"input_required","message":"failure","retryable":false}"#,
            ),
            HeadlessOutcome::Done => unreachable!("Done is the success golden above"),
        };
        assert_eq!(
            String::from_utf8(bytes.clone()).expect("failure utf8"),
            format!(
                "{{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{{\"count\":0,\"refs\":[]}},\"outcome\":\"{outcome_name}\",\"response\":null,\"usage\":null,\"permission_denials\":[],\"error\":{error}}}\n"
            )
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("failure object");
        assert_eq!(value.as_object().expect("object").len(), 11);
        assert!(value["response"].is_null());
        assert_eq!(
            value["error"].is_null(),
            outcome == HeadlessOutcome::Cancelled
        );
    }
}

/// MUTATION CHECK: trust a filename extension instead of file magic or omit
/// one supported image signature. Expected RUNTIME failure: a disguised PNG
/// is refused, or the invalid `.png` payload is accepted.
#[test]
fn attach_loader_sniffs_image_magic_not_extensions() {
    let directory = tempfile::tempdir().expect("attachment tempdir");
    for (name, bytes, expected_mime) in [
        ("jpeg.txt", vec![0xff, 0xd8, 0xff], "image/jpeg"),
        (
            "png.txt",
            vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            "image/png",
        ),
        ("gif.txt", b"GIF89a".to_vec(), "image/gif"),
        (
            "webp.txt",
            [b"RIFF".as_slice(), &[0, 0, 0, 0], b"WEBP".as_slice()].concat(),
            "image/webp",
        ),
    ] {
        let disguised = directory.path().join(name);
        std::fs::write(&disguised, bytes).expect("write disguised image");
        let loaded = load_image_attachment(&disguised).expect("magic identifies image");
        assert_eq!(loaded.mime, expected_mime);
    }

    let false_extension = directory.path().join("not-an-image.png");
    std::fs::write(&false_extension, b"plain text").expect("write invalid image");
    let error = load_image_attachment(&false_extension).expect_err("extension is not trusted");
    assert!(matches!(
        error,
        HeadlessRunError::Attachment { ref code, .. }
            if code == "unsupported_attachment_type"
    ));
}

/// MUTATION CHECK: omit landed artifact refs/count from the additive JSON
/// result or serialize raw bytes. Expected RUNTIME failure: the exact
/// attachment object no longer contains only the stable CAS identities.
#[test]
fn run_json_reports_attachments_additively() {
    let mut attached = result(HeadlessOutcome::Done, None);
    attached.attachments = vec![
        ArtifactRef::new("blake3:first"),
        ArtifactRef::new("blake3:second"),
    ];
    let mut bytes = Vec::new();
    write_final(&mut bytes, RunOutput::Json, &attached).expect("attachment JSON");
    assert_eq!(
        String::from_utf8(bytes.clone()).expect("utf8"),
        "{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{\"count\":2,\"refs\":[\"blake3:first\",\"blake3:second\"]},\"outcome\":\"done\",\"response\":null,\"usage\":null,\"permission_denials\":[],\"error\":null}\n"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("attachment object");
    assert_eq!(value["attachments"]["count"], 2);
    assert_eq!(
        value["attachments"]["refs"],
        serde_json::json!(["blake3:first", "blake3:second"])
    );
}

struct BrokenWriter;

impl std::io::Write for BrokenWriter {
    fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed consumer",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// MUTATION CHECK: swallow BrokenPipe or panic through print macros. Expected
/// RUNTIME failure: the injected output fault is no longer classified as the
/// deliberate exit-74 path.
#[test]
fn output_broken_pipe_is_a_typed_io_failure() {
    let mut done = result(HeadlessOutcome::Done, None);
    done.response = Some("answer".into());
    let error = write_final(BrokenWriter, RunOutput::Print, &done).expect_err("broken pipe");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(EX_IOERR, 74);
}

fn parse_jsonl(output: &[u8]) -> Vec<RawEnvelope> {
    String::from_utf8(output.to_vec())
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("RawEnvelope JSONL line"))
        .collect()
}

/// Decodes a core payload, tolerating additive supplemental kinds (the
/// journal's forward-compat law: unknown `kind`s are DATA, never errors —
/// e.g. `project_instructions_loaded`). A payload without a string `kind`
/// is still a hard frame violation.
fn typed(envelope: &RawEnvelope) -> Option<EventPayload> {
    assert!(
        envelope
            .payload
            .get("type")
            .is_some_and(|kind| kind.is_string()),
        "payload frame lacks a string type tag: {}",
        envelope.payload
    );
    serde_json::from_value(envelope.payload.clone()).ok()
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
