//! Black-box tests for the `haider` binary surface.
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use haider_protocol::EventPayload;
use haider_protocol::effect::{AuthorizationVerdict, EffectPhase};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope};
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::state::RunState;
use std::fs::{OpenOptions, TryLockError};
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod cli_main;

use cli_main::account::{AccountCommand, parse_account_command};
use cli_main::hooks::{HooksCommand, parse_hooks_command};
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
const PREBUILT_DAEMON_ENV: &str = "HAIDER_TEST_SIBLINGS_PREBUILT";

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
    ensure_haiderd_present();
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
        // Hermetic accounts: startup auto-adoption (A2) would otherwise read
        // the HOST machine's real codex/Claude credentials into this
        // throwaway profile — "no active account" tests stop being true.
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .env("HAIDER_TEST_FAKE_PROVIDER", DEFAULT_FAKE_SCRIPT);
    HaiderCommand {
        command,
        _profile_root: profile_root,
        profile,
    }
}

impl Drop for HaiderCommand {
    fn drop(&mut self) {
        let _ = terminate_daemon_checked(&self.profile);
    }
}

fn ensure_haiderd_present() {
    // Integration tests must not recursively enter Cargo while their parent
    // `cargo test` is holding build/artifact state. Besides being unbounded
    // fixture work, that can deadlock the first Windows test at this shared
    // helper and park every concurrent caller behind a process-local Once.
    // The gate prebuilds the workspace and then exports an explicit proof;
    // focused invocations must do the same. Existence alone is insufficient
    // because a persistent target directory may contain a stale sibling.
    assert_eq!(
        std::env::var(PREBUILT_DAEMON_ENV).as_deref(),
        Ok("1"),
        "CLI subprocess fixtures require a fresh sibling; run \
         `cargo build -p haider-daemond --bin haiderd` first, then set \
         {PREBUILT_DAEMON_ENV}=1 for the test command"
    );
    let sibling = PathBuf::from(env!("CARGO_BIN_EXE_haider"))
        .parent()
        .expect("haider binary parent")
        .join(format!("haiderd{}", std::env::consts::EXE_SUFFIX));
    assert!(
        sibling.is_file(),
        "haiderd sibling missing at {}; prebuild with `cargo build -p haider-daemond --bin haiderd`",
        sibling.display()
    );
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

fn terminate_daemon_checked(profile: &Path) -> std::io::Result<()> {
    let pid = daemon_pid(profile).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "daemon PID is missing from {}",
                profile.join("lock.owner").display()
            ),
        )
    })?;
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "kill reported {status} for daemon {pid}"
            )));
        }
    }
    #[cfg(windows)]
    haider_platform::kill_process_tree(pid, true)?;
    Ok(())
}

#[cfg(unix)]
const EXEC_WRITE_COMMAND: &str = "printf ok > exec-created.txt";

// T4 made absolute System32 PowerShell the Windows interpreter for BOTH
// user `!` commands and the exec tool (the shell_command pin in
// haider-tools), so fixtures speak PowerShell, not cmd.
#[cfg(windows)]
const EXEC_WRITE_COMMAND: &str = "[IO.File]::WriteAllText('exec-created.txt','ok')";

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
fn account_parser_pins_list_and_remove_grammar() {
    assert_eq!(
        parse_account_command(&["list".into()]),
        Ok(AccountCommand::List { json: false })
    );
    assert_eq!(
        parse_account_command(&["list".into(), "--json".into()]),
        Ok(AccountCommand::List { json: true })
    );
    assert_eq!(
        parse_account_command(&["remove".into(), "probe".into()]),
        Ok(AccountCommand::Remove {
            alias: "probe".into(),
            confirm: false,
        })
    );
    assert_eq!(
        parse_account_command(&["remove".into(), "probe".into(), "--confirm".into()]),
        Ok(AccountCommand::Remove {
            alias: "probe".into(),
            confirm: true,
        })
    );
    assert!(parse_account_command(&["list".into(), "--yaml".into()]).is_err());
    assert!(parse_account_command(&["remove".into(), "--confirm".into()]).is_err());
}

fn seed_cli_account(command: &mut HaiderCommand, alias: &str) -> Vec<u8> {
    let runtime_dir = command._profile_root.path().to_path_buf();
    command
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("TMPDIR", runtime_dir);
    let descriptors = vec![haider_protocol::credential::CredentialDescriptor {
        alias: haider_protocol::ids::CredentialAlias::new(alias),
        provider: "anthropic".into(),
        base_url: Some("https://SECRET-ENDPOINT.invalid/TOKEN-SENTINEL".into()),
        auth_method: haider_protocol::credential::AuthMethod::ApiKey,
        identity: "SECRET-IDENTITY-SENTINEL".into(),
        status: haider_protocol::credential::CredentialStatus::NeedsAttention {
            reason: haider_protocol::credential::CredentialAttentionReason::KeychainMissing,
        },
        active: true,
        label: Some("SECRET-LABEL-SENTINEL".into()),
    }];
    let mut bytes = serde_json::to_vec_pretty(&descriptors).expect("account fixture JSON");
    bytes.push(b'\n');
    std::fs::create_dir_all(&command.profile).expect("profile directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&command.profile, std::fs::Permissions::from_mode(0o700))
            .expect("secure profile permissions");
    }
    let accounts = command.profile.join("accounts.json");
    std::fs::write(&accounts, &bytes).expect("seed accounts");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&accounts, std::fs::Permissions::from_mode(0o600))
            .expect("secure accounts permissions");
    }
    bytes
}

fn daemon_logs(profile: &Path) -> String {
    let directory = profile.join("daemon-logs");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return "<no daemon log>".into();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The real sibling daemon answers `account.list`; the CLI projection must
/// never widen to descriptor Debug/serde fields when human or JSON evolves.
#[test]
fn account_list_json_uses_daemon_rpc_and_exposes_only_the_safe_projection() {
    let mut command = haider();
    seed_cli_account(&mut command, "probe-json");
    let output = command
        .args(["account", "list", "--json"])
        .output()
        .expect("account list runs");
    assert!(
        output.status.success(),
        "stderr: {}\ndaemon log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        daemon_logs(&command.profile)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("list JSON");
    assert_eq!(value["schema"], "haider.accounts.v1");
    let account = value["accounts"][0].as_object().expect("account object");
    assert_eq!(account.len(), 4, "only the four sanctioned fields");
    assert_eq!(account["alias"], "probe-json");
    assert_eq!(account["provider"], "anthropic");
    assert_eq!(account["auth_kind"], "api_key");
    assert!(account["created"].is_null());
    let output = String::from_utf8(output.stdout).expect("UTF-8 output");
    for secret in [
        "SECRET-ENDPOINT",
        "TOKEN-SENTINEL",
        "SECRET-IDENTITY-SENTINEL",
        "SECRET-LABEL-SENTINEL",
        "keychain_missing",
    ] {
        assert!(!output.contains(secret), "list leaked {secret}: {output}");
    }
}

/// MUTATION CHECK: delete/invert the `--confirm` gate. Expected RUNTIME
/// failure: this command succeeds, starts a daemon, or changes accounts.json.
#[test]
fn account_remove_without_confirm_cannot_reach_the_daemon_or_mutate() {
    let mut command = haider();
    let before = seed_cli_account(&mut command, "probe-unconfirmed");
    let output = command
        .args(["account", "remove", "probe-unconfirmed"])
        .output()
        .expect("unconfirmed account remove runs");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("would remove account `probe-unconfirmed`"));
    assert!(stderr.contains("--confirm"));
    assert!(
        daemon_pid(&command.profile).is_none(),
        "daemon must not start"
    );
    assert_eq!(
        std::fs::read(command.profile.join("accounts.json")).expect("accounts remain"),
        before
    );
}

#[test]
fn account_remove_confirmed_uses_the_daemon_and_commits_removal() {
    let mut command = haider();
    seed_cli_account(&mut command, "probe-confirmed");
    let output = command
        .args(["account", "remove", "probe-confirmed", "--confirm"])
        .output()
        .expect("confirmed account remove runs");
    assert!(
        output.status.success(),
        "stderr: {}\ndaemon log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        daemon_logs(&command.profile)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "removed account `probe-confirmed`\n"
    );
    let descriptors: Vec<haider_protocol::credential::CredentialDescriptor> =
        serde_json::from_slice(
            &std::fs::read(command.profile.join("accounts.json")).expect("accounts projection"),
        )
        .expect("accounts JSON");
    assert!(descriptors.is_empty(), "confirmed removal must commit");
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

/// MUTATION CHECK: move the accepted record after the first envelope, omit
/// its cursor, or send it to stderr. Expected runtime failure: the first-line
/// schema/order assertions fail before any model output is inspected.
#[test]
fn run_jsonl_announces_acceptance_before_lf_framed_envelopes() {
    let out = haider()
        .args(["run", "--provider", "fake", "--jsonl", "hello"])
        .output()
        .expect("binary runs");
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    assert!(out.stdout.ends_with(b"\n"));
    assert!(!out.stdout.contains(&b'\r'));

    let text = String::from_utf8(out.stdout).expect("utf8");
    let mut lines = text.lines();
    let accepted: serde_json::Value =
        serde_json::from_str(lines.next().expect("accepted line")).expect("accepted JSON");
    assert_eq!(accepted["event"], "accepted");
    assert!(
        accepted["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(accepted["head_seq"].as_u64().is_some_and(|seq| seq > 0));
    assert_eq!(accepted.as_object().expect("accepted object").len(), 3);
    let envelopes: Vec<RawEnvelope> = lines
        .map(|line| serde_json::from_str(line).expect("RawEnvelope JSONL line"))
        .collect();
    assert!(!envelopes.is_empty());
    assert_eq!(accepted["session_id"], envelopes[0].session_id.as_str());
    assert_eq!(accepted["head_seq"], envelopes[0].seq);
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
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert!(stderr.starts_with("session "));
    assert_eq!(stderr.lines().count(), 1);
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
    let profile = profile_parent.path().join("profile");
    let mut command = haider();
    // `HaiderCommand` owns daemon cleanup. Keep its cleanup identity aligned
    // with this test's explicit profile instead of leaking the real daemon
    // while trying to terminate the helper's unused throwaway profile.
    command.profile = profile.clone();
    command
        .env("HAIDER_PROFILE_DIR", &profile)
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
        .env_remove("HAIDER_ANTHROPIC_API_KEY");
    let mut out = None;
    for _ in 0..2 {
        let attempt = command.output().expect("binary runs");
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
    ensure_haiderd_present();
    let profile_parent = tempfile::tempdir().expect("temporary CLI profile parent");
    let profile = profile_parent.path().join("profile");
    let run = |prompt: &str| {
        Command::new(env!("CARGO_BIN_EXE_haider"))
            .args(["run", "--provider", "fake", "--jsonl", prompt])
            .env("HAIDER_PROFILE_DIR", &profile)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
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
    terminate_daemon_checked(&profile).expect("terminate first daemon generation");
    // Endpoint removal precedes store close during an orderly drain. Waiting
    // for the endpoint therefore races the successor into the intentional
    // endpoint-gone/profile-lock-held interval, where it must exit 75. Prove
    // release of the actual singleton authority instead. Acquiring and
    // releasing this non-mutating probe does not open the store, rewrite the
    // sibling owner diagnostics, or consume a worker generation.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(profile.join("lock"))
        .expect("profile lock file");
    // The product's five-second drain barrier bounds the reported outcome,
    // but an already-started blocking close may release the OS lock just
    // after it. Use the existing startup observation bound so this fixture
    // waits beyond that honest forced-close edge without changing the law.
    let deadline = Instant::now() + haider_client::STARTUP_DEADLINE;
    loop {
        match lock.try_lock() {
            Ok(()) => {
                lock.unlock().expect("release profile lock probe");
                break;
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => panic!("profile lock proof failed: {error}"),
        }
        assert!(Instant::now() < deadline, "profile lock release deadline");
        thread::sleep(Duration::from_millis(20));
    }
    let second_output = run("restarted process");
    terminate_daemon_checked(&profile).expect("terminate second daemon generation");
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

/// Runs `haider` with ONE bounded retry when the autospawned daemon misses
/// its startup deadline under full-gate load (exit 69) — the transient
/// class only; real failures surface with stderr on the caller's assert.
fn haider_with_boot_retry(args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let run = || {
        let mut command = haider();
        command.args(args);
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("binary runs")
    };
    let out = run();
    if out.status.code() == Some(69) {
        run()
    } else {
        out
    }
}

#[test]
fn run_jsonl_exits_65_when_fake_provider_errors() {
    let out = haider_with_boot_retry(
        &["run", "--provider", "fake", "--jsonl", "hello"],
        &[(
            "HAIDER_TEST_FAKE_PROVIDER",
            r#"[{"step":"malformed_frame"}]"#,
        )],
    );
    assert_eq!(
        out.status.code(),
        Some(65),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let envelopes = parse_jsonl(&out.stdout);
    assert_eq!(
        envelopes.last().map(typed),
        Some(Some(EventPayload::RunState(RunState::Errored)))
    );
}

#[test]
fn run_jsonl_cancelled_has_130_exit_and_terminal_envelope() {
    let out = haider_with_boot_retry(
        &["run", "--provider", "fake", "--jsonl", "hello"],
        &[(
            "HAIDER_TEST_FAKE_PROVIDER",
            r#"[{"step":"finish","reason":"cancelled"}]"#,
        )],
    );
    assert_eq!(
        out.status.code(),
        Some(130),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    let out = haider_with_boot_retry(
        &["run", "--provider", "fake", "hello", "--output", "json"],
        &[("HAIDER_TEST_FAKE_PROVIDER", script)],
    );
    assert_eq!(
        out.status.code(),
        Some(77),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
                "command": EXEC_WRITE_COMMAND,
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
    // On the run surface an exec tool call completes as its `tool_call`
    // item plus a bounded ToolResult whose preview carries the exit code —
    // the CommandExecution item belongs to the direct-shell and live-turn
    // RPC surfaces, not this stream.
    assert!(exec_envelopes.iter().any(|envelope| matches!(
        typed(envelope),
        Some(EventPayload::ToolResult { call_id, result })
            if call_id == "exec-1" && result.preview.contains("\"exit_code\":0")
    )));
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
    // Delta coalescing (v0.0.936 #25) makes the delta-envelope COUNT
    // provider-timing dependent, so the count is no longer a law. The law
    // this test owns is LOSSLESS replay under consumer lag: every journaled
    // envelope arrives (seq contiguity above) and no streamed byte is
    // dropped — the concatenated delta text and the completed item must
    // both carry all 500 fragments.
    let expected_text: String = (0..500).map(|index| index.to_string()).collect();
    let delta_text: String = envelopes
        .iter()
        .filter_map(|envelope| match typed(envelope) {
            Some(EventPayload::Item(ItemEvent::Delta {
                delta: ItemDelta::Text { text },
                ..
            })) => Some(text),
            _ => None,
        })
        .collect();
    assert!(!delta_text.is_empty(), "streamed deltas must journal");
    assert_eq!(delta_text, expected_text);
    assert!(envelopes.iter().any(|envelope| matches!(
        typed(envelope),
        Some(EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::AgentMessage { ref text },
            ..
        })) if *text == expected_text
    )));
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
    // Exit-69 boot-retry family (5th sibling, gate119): a cold daemon under
    // full-gate load can miss the startup deadline (exit 69 = unavailable)
    // before the injected fault is ever reachable. The law under test is the
    // 70/StoreCorrupt trail, not spawn latency — retry the unavailable case
    // once, exactly like the flagless-run credential law above.
    let mut outcome = None;
    for _ in 0..2 {
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
        let unavailable = status.code() == Some(i32::from(EX_UNAVAILABLE));
        outcome = Some((status, stdout, stderr));
        if !unavailable {
            break;
        }
    }
    let (status, stdout, stderr) = outcome.expect("at least one attempt ran");

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
                presentation: Some(ErrorPresentation::new(
                    "store-corrupt",
                    "Haider could not complete the turn",
                    "injected terminal append failure",
                    ErrorScope::Turn,
                    [ErrorAction::None],
                )),
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
        background_tasks_running: Vec::new(),
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
                    presentation: None,
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
                    presentation: None,
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
                    presentation: None,
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
                    presentation: None,
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
                    presentation: None,
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
                    presentation: None,
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
                    presentation: None,
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
            auto_allow: false,
            trust_hooks: false,
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
        "--auto-allow".into(),
        "--trust-hooks".into(),
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
    assert!(parsed.allow_writes && parsed.allow_exec && parsed.auto_allow && parsed.trust_hooks);
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

/// MUTATION CHECK: loosen the hooks grammar or drop the machine-readable list
/// flag. Expected RUNTIME failure: an exact dispatch below changes.
#[test]
fn hooks_parser_pins_list_trust_and_revoke_grammar() {
    assert_eq!(
        parse_hooks_command(&["list".into()]),
        Ok(HooksCommand::List { json: false })
    );
    assert_eq!(
        parse_hooks_command(&["list".into(), "--json".into()]),
        Ok(HooksCommand::List { json: true })
    );
    assert_eq!(
        parse_hooks_command(&["trust".into(), "a".repeat(64)]),
        Ok(HooksCommand::Trust {
            digest: "a".repeat(64)
        })
    );
    assert_eq!(
        parse_hooks_command(&["revoke".into(), "b".repeat(64)]),
        Ok(HooksCommand::Revoke {
            digest: "b".repeat(64)
        })
    );
    assert!(parse_hooks_command(&["list".into(), "--yaml".into()]).is_err());
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
        "{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{\"count\":0,\"refs\":[]},\"outcome\":\"done\",\"response\":\"final answer\",\"usage\":null,\"permission_denials\":[],\"background_tasks_running\":[],\"error\":null}\n"
    );
    let value: serde_json::Value = serde_json::from_slice(&json).expect("v1 JSON");
    assert_eq!(value.as_object().expect("object").len(), 12);
    assert_eq!(value["provider"], "fake");
    assert_eq!(value["model"], "fake-model");
    assert!(value["usage"].is_null());
    assert!(value["error"].is_null());

    // W-A decision 8 (additive): still-running background tasks are NAMED
    // in the v1 object — the daemon keeps ownership past the run.
    let mut with_tasks = result(HeadlessOutcome::Done, None);
    with_tasks
        .background_tasks_running
        .push(haider_client::HeadlessBackgroundTask {
            task_id: "task-cafe".into(),
            name: "watcher".into(),
        });
    let mut task_json = Vec::new();
    write_final(&mut task_json, RunOutput::Json, &with_tasks).expect("task JSON");
    let tasks: serde_json::Value = serde_json::from_slice(&task_json).expect("task object");
    assert_eq!(tasks["background_tasks_running"][0]["task_id"], "task-cafe");
    assert_eq!(tasks["background_tasks_running"][0]["name"], "watcher");

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
            presentation: None,
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
                "{{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{{\"count\":0,\"refs\":[]}},\"outcome\":\"{outcome_name}\",\"response\":null,\"usage\":null,\"permission_denials\":[],\"background_tasks_running\":[],\"error\":{error}}}\n"
            )
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("failure object");
        assert_eq!(value.as_object().expect("object").len(), 12);
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

/// LAW (LA2 client half + LA3, G2): the text-file loader accepts UTF-8 with
/// an honest line count and a sanitized BASENAME, refuses non-UTF-8 with the
/// DISTINCT `unsupported_attachment_encoding` code, and refuses the 5 MiB
/// overrun with the same cap the image lane enforces.
///
/// MUTATION CHECK: drop the UTF-8 validation, reuse
/// `unsupported_attachment_type` for binary payloads, or carry the full path
/// as the name. Expected RUNTIME failure: the matching assertion below.
#[test]
fn attach_text_loader_validates_utf8_and_sanitizes_the_name() {
    let directory = tempfile::tempdir().expect("attachment tempdir");

    let text = directory.path().join("notes.md");
    std::fs::write(&text, "line one\nline two\nline three").expect("write text");
    let loaded = haider_client::load_text_attachment(&text).expect("UTF-8 text loads");
    assert_eq!(loaded.name, "notes.md", "basename only, never the path");
    assert_eq!(loaded.lines, 3);
    assert_eq!(loaded.bytes, b"line one\nline two\nline three");

    // Non-UTF-8 is the DISTINCT encoding refusal — never the image code.
    let binary = directory.path().join("blob.pdf");
    std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x80, 0x81]).expect("write binary");
    let error = haider_client::load_text_attachment(&binary).expect_err("binary refused");
    assert!(matches!(
        error,
        HeadlessRunError::Attachment { ref code, ref message, .. }
            if code == "unsupported_attachment_encoding" && message.contains("not UTF-8")
    ));

    // Over the 5 MiB per-attachment cap: same bound as the image lane.
    let big = directory.path().join("big.txt");
    std::fs::write(&big, "a".repeat(5 * 1024 * 1024 + 1)).expect("write oversized");
    let error = haider_client::load_text_attachment(&big).expect_err("oversize refused");
    assert!(matches!(
        error,
        HeadlessRunError::Attachment { ref code, .. } if code == "attachment_too_large"
    ));

    // Control characters are stripped from the display name and the length
    // is capped at 120 characters.
    #[cfg(unix)]
    let weird = directory.path().join("a\u{7}b.txt");
    // NTFS refuses C0 control characters in a file name. U+0085 is still a
    // Rust control character, but it is a legal Windows filename, so this
    // exercises the exact same sanitizer law through the real loader.
    #[cfg(windows)]
    let weird = directory.path().join("a\u{85}b.txt");
    std::fs::write(&weird, "x").expect("write control-name file");
    let loaded = haider_client::load_text_attachment(&weird).expect("loads");
    assert_eq!(loaded.name, "ab.txt", "control characters stripped");
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
        "{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{\"count\":2,\"refs\":[\"blake3:first\",\"blake3:second\"]},\"outcome\":\"done\",\"response\":null,\"usage\":null,\"permission_denials\":[],\"background_tasks_running\":[],\"error\":null}\n"
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
    let text = String::from_utf8(output.to_vec()).expect("utf8");
    let mut lines = text.lines();
    let accepted: serde_json::Value =
        serde_json::from_str(lines.next().expect("accepted line")).expect("accepted JSONL line");
    assert_eq!(accepted["event"], "accepted");
    assert!(accepted["session_id"].is_string());
    assert!(accepted["head_seq"].is_u64());
    lines
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
