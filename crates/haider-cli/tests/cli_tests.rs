#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only
//! Black-box tests for the `haider` binary surface.

use haider_protocol::EventPayload;
use haider_protocol::effect::{AuthorizationVerdict, EffectPhase};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope};
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::state::RunState;
#[cfg(unix)]
use std::fs::{OpenOptions, TryLockError};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{self, BufRead, BufReader};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod cli_main;

use cli_main::account::{AccountCommand, parse_account_command};
use cli_main::hooks::{HooksCommand, parse_hooks_command};
use cli_main::run::{
    EX_BLOCKED, EX_CANCELLED, EX_IOERR, EX_PROTOCOL, EX_PROVIDER, EX_SOFTWARE, EX_TIMEOUT,
    EX_UNAVAILABLE, EX_USAGE, ProviderSelection, RunAction, RunOptions, RunOutput,
    exit_code_for_error, exit_code_for_result, parse_run_options, read_stdin_prompt_from,
    write_final,
};
use cli_main::{ImportDispatch, ImportSource, parse_import_dispatch};
use haider_client::{
    DisconnectReason, EnsureError, HeadlessBlockingReason, HeadlessFailureCode, HeadlessOutcome,
    HeadlessPermissionDenial, HeadlessRunError, HeadlessRunEvents, HeadlessRunFailure,
    HeadlessRunResult, load_image_attachment,
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
    configure_test_home(&mut command, &profile);
    command
        .current_dir(&workspace)
        .env("HAIDER_PROFILE_DIR", &profile)
        // Lockdown and other machine-user-global subsystems must never escape
        // this nominally hermetic profile into the developer's real home.
        // `configure_test_home` is also the one authority used by attached
        // helper commands and parent-side endpoint resolution.
        .env_remove("HAIDER_MODEL")
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

fn configure_test_home(command: &mut Command, profile: &Path) {
    let home = test_home(profile);
    std::fs::create_dir_all(&home).expect("create isolated machine-user home");
    command
        .env("HOME", &home)
        .env("USERPROFILE", home)
        .env_remove("HAIDER_RUNTIME_DIR")
        .env_remove("XDG_RUNTIME_DIR");
}

fn test_home(profile: &Path) -> PathBuf {
    profile.parent().unwrap_or(profile).join("machine-home")
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

fn assert_terminal_jsonl_error(output: &std::process::Output) -> Vec<serde_json::Value> {
    assert!(
        !output.status.success(),
        "expected nonzero exit; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.ends_with(b"\n"), "JSONL must end with LF");
    let values = String::from_utf8(output.stdout.clone())
        .expect("JSONL stdout is UTF-8")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("every stdout line is one JSON object"))
        .collect::<Vec<serde_json::Value>>();
    assert!(
        values.iter().all(serde_json::Value::is_object),
        "every nonempty JSONL line must be an object"
    );
    let terminal = values.last().expect("terminal JSONL record");
    assert_eq!(terminal["event"], "error");
    assert_eq!(terminal["stage"], "bootstrap");
    assert!(terminal["outcome"].as_str().is_some());
    assert!(terminal["error"]["code"].as_str().is_some());
    assert!(terminal["error"]["message"].as_str().is_some());
    assert!(terminal["error"]["retryable"].as_bool().is_some());
    values
}

#[cfg(unix)]
fn wait_for_daemon_pid(profile: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(pid) = daemon_pid(profile) {
            return pid;
        }
        assert!(Instant::now() < deadline, "daemon PID publication deadline");
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn staged_process_thread_count(pid: u32) -> io::Result<Option<usize>> {
    std::fs::read_dir(format!("/proc/{pid}/task"))?
        .try_fold(0_usize, |count, entry| {
            entry.map(|_| count.saturating_add(1))
        })
        .map(Some)
}

#[cfg(target_os = "macos")]
fn staged_process_thread_count(pid: u32) -> io::Result<Option<usize>> {
    let output = match Command::new("/bin/ps")
        .args(["-M", "-p", &pid.to_string(), "-o", "pid="])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(None),
        Err(error) => return Err(error),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr
            .to_ascii_lowercase()
            .contains("operation not permitted")
        {
            return Ok(None);
        }
        return Err(io::Error::other(format!(
            "ps thread probe failed with {}: {stderr}",
            output.status
        )));
    }
    // macOS `ps -M` emits one process summary row before its per-thread rows.
    // Exclude that summary so this matches Linux `/proc/<pid>/task` semantics.
    Ok(Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .skip(1)
            .count(),
    ))
}

#[cfg(unix)]
fn wait_for_profile_lock(profile: &Path, should_be_free: bool) {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(profile.join("lock"))
        .expect("profile lock file");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match lock.try_lock() {
            Ok(()) if should_be_free => {
                lock.unlock().expect("release profile lock probe");
                return;
            }
            Ok(()) => {
                lock.unlock()
                    .expect("release unexpected profile lock probe");
                panic!("incumbent profile lock was unexpectedly free");
            }
            Err(TryLockError::WouldBlock) if !should_be_free => return,
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => panic!("profile lock proof failed: {error}"),
        }
        assert!(Instant::now() < deadline, "profile lock release deadline");
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn configure_isolated_runtime(command: &mut HaiderCommand) -> PathBuf {
    let mut environment = haider_client::ProfileEnv::capture();
    environment.profile_dir = Some(command.profile.clone());
    let home = test_home(&command.profile);
    environment.home = Some(home.clone());
    environment.user_profile = Some(home);
    environment.runtime_dir = None;
    environment.xdg_runtime_dir = None;
    haider_client::resolve_profile(&environment)
        .expect("resolve profile identity")
        .endpoint_path
}

fn read_http_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request read timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut expected = None;
    loop {
        let read = stream.read(&mut chunk).expect("read proxy request");
        assert!(read > 0, "proxy request closed before headers/body");
        bytes.extend_from_slice(&chunk[..read]);
        if expected.is_none()
            && let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            expected = Some((header_end, header_end + content_length));
        }
        if expected.is_some_and(|(_, total)| bytes.len() >= total) {
            break;
        }
    }
    let (header_end, total) = expected.expect("complete request headers");
    let request_line = String::from_utf8_lossy(&bytes[..header_end])
        .lines()
        .next()
        .expect("request line")
        .to_owned();
    (request_line, bytes[header_end..total].to_vec())
}

fn write_http_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .expect("write response headers");
    stream.write_all(body).expect("write response body");
    stream.flush().expect("flush response");
}

fn spawn_compatible_proxy(
    catalog: Option<&'static [u8]>,
) -> (
    String,
    std::sync::mpsc::Receiver<serde_json::Value>,
    Arc<AtomicUsize>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind compatible proxy");
    let origin = format!(
        "http://{}/v1",
        listener.local_addr().expect("proxy address")
    );
    let (captured, receiver) = std::sync::mpsc::channel();
    let model_list_requests = Arc::new(AtomicUsize::new(0));
    let observed_model_list_requests = Arc::clone(&model_list_requests);
    let task = thread::spawn(move || {
        for incoming in listener.incoming() {
            let mut stream = incoming.expect("accept proxy request");
            let (request_line, body) = read_http_request(&mut stream);
            if request_line.starts_with("POST ") && request_line.contains("/chat/completions") {
                let value = serde_json::from_slice(&body).expect("chat request JSON");
                captured.send(value).expect("capture chat request");
                let sse = concat!(
                    "data: {\"id\":\"chatcmpl-bench\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-bench\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"id\":\"chatcmpl-bench\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: [DONE]\n\n"
                );
                write_http_response(&mut stream, "200 OK", "text/event-stream", sse.as_bytes());
                return;
            }
            if request_line.contains("/models") {
                observed_model_list_requests.fetch_add(1, Ordering::SeqCst);
                match catalog {
                    Some(body) => {
                        write_http_response(&mut stream, "200 OK", "application/json", body)
                    }
                    None => write_http_response(
                        &mut stream,
                        "404 Not Found",
                        "application/json",
                        br#"{"error":"no catalog"}"#,
                    ),
                }
            } else {
                write_http_response(&mut stream, "200 OK", "application/json", b"{}");
            }
        }
    });
    (origin, receiver, model_list_requests, task)
}

fn run_custom_model_wire_case(
    profile_default: &str,
    explicit_model: Option<&str>,
    cached_models: &[&str],
    catalog: Option<&'static [u8]>,
) -> (serde_json::Value, usize) {
    let mut command = haider();
    // This case exercises the REAL OpenAI-compatible HTTP path against a fake
    // proxy to capture the wire model. The default fake-provider script the
    // `haider()` helper installs would short-circuit the turn and never emit a
    // `/chat/completions` request, so it must be removed for this one case.
    command.env_remove("HAIDER_TEST_FAKE_PROVIDER");
    #[cfg(unix)]
    let _endpoint = configure_isolated_runtime(&mut command);
    let (origin, captured, model_list_requests, proxy) = spawn_compatible_proxy(catalog);
    std::fs::create_dir_all(&command.profile).expect("profile directory");
    std::fs::write(
        command.profile.join("config.json"),
        serde_json::to_vec(&serde_json::json!({"default_model": profile_default}))
            .expect("profile config JSON"),
    )
    .expect("profile config");
    std::fs::write(
        command.profile.join("providers.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "providers": [{
                "provider_id": "bench-proxy",
                "display_name": "Bench Proxy",
                "api_family": "openai_chat_completions",
                "base_url": origin,
                "enabled": true,
                "auth_requirement": "none",
                "configured_models": ["deepseek-v4-flash"],
                "default_model": null,
                "promotion_model": null,
                "provenance": "custom"
            }],
            "fallback_chain": []
        }))
        .expect("providers JSON"),
    )
    .expect("providers registry");
    let discovered = cached_models
        .iter()
        .map(|slug| haider_provider::DiscoveredModel {
            slug: (*slug).to_owned(),
            display_name: (*slug).to_owned(),
            context_window: None,
            description: None,
            default_effort: None,
            supported_efforts: Vec::new(),
            visible: true,
            priority: None,
            extensions: None,
        })
        .collect::<Vec<_>>();
    {
        let store = haider_store::Store::open(&command.profile).expect("seed provider cache");
        store
            .put_provider_models(
                "bench-proxy",
                &serde_json::to_string(&discovered).expect("catalog cache JSON"),
                None,
                1,
            )
            .expect("put provider cache");
    }
    let mut args = vec!["run", "--provider", "bench-proxy", "--jsonl", "wire model"];
    if let Some(model) = explicit_model {
        args.splice(3..3, ["--model", model]);
    }
    let output = command.args(args).output().expect("custom model run exits");
    assert!(
        output.status.success(),
        "stderr: {}; stdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let request = captured
        .recv_timeout(Duration::from_secs(10))
        .expect("captured chat request");
    proxy.join().expect("proxy joins");
    (request, model_list_requests.load(Ordering::SeqCst))
}

#[cfg(unix)]
const EXEC_WRITE_COMMAND: &str = "printf ok > exec-created.txt";

// T4 made absolute System32 PowerShell the Windows interpreter for BOTH
// user `!` commands and the exec tool (the shell_command pin in
// haider-tools), so fixtures speak PowerShell, not cmd.
#[cfg(windows)]
const EXEC_WRITE_COMMAND: &str = "[IO.File]::WriteAllText('exec-created.txt','ok')";

#[cfg(unix)]
const REPLAY_BACKGROUND_COMMAND: &str = "while [ ! -f replay-release ]; do sleep 0.02; done";

#[cfg(windows)]
const REPLAY_BACKGROUND_COMMAND: &str =
    "while (-not (Test-Path 'replay-release')) { Start-Sleep -Milliseconds 20 }";

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
        account_identity: None,
        created_at_ms: None,
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

#[cfg(unix)]
fn daemon_log_count(profile: &Path) -> usize {
    std::fs::read_dir(profile.join("daemon-logs"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_durable_run_acceptance(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| {
        value["run_id"].as_str().is_some()
            && value["payload"]["type"] == "run_state"
            && value["payload"]["state"] == "queued"
    })
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
    let terminals = envelopes
        .iter()
        .filter(|envelope| envelope.payload.get("terminal_kind").is_some())
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].payload["terminal_kind"], "success");
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

#[test]
fn run_json_is_one_stable_typed_stream_summary() {
    let out = haider_with_boot_retry(&["run", "--provider", "fake", "--json", "-p", "hello"], &[]);
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.ends_with(b"\n"));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("run JSON");
    assert_eq!(value["schema"], "haider.run.v1");
    assert_eq!(value["response"], "fake response: hello");
    let events = value["events"].as_array().expect("typed events");
    assert!(events.iter().any(|event| {
        event["payload"]["type"] == "item"
            && event["payload"]["event"] == "completed"
            && event["payload"]["item"]["item"] == "agent_message"
    }));
    assert_eq!(
        events.last().map(|event| &event["payload"]),
        Some(&serde_json::json!({"type":"run_state","state":"done"}))
    );
}

#[test]
fn run_prompt_flag_and_piped_stdin_are_process_parity() {
    let inline = haider_with_boot_retry(&["run", "--provider", "fake", "-p", "hello"], &[]);
    let piped = haider_with_stdin_boot_retry(&["run", "--provider", "fake", "-"], b"hello\r\n");
    assert_eq!(inline.status.code(), Some(0));
    assert_eq!(piped.status.code(), inline.status.code());
    assert_eq!(piped.stdout, inline.stdout);
    for stderr in [&inline.stderr, &piped.stderr] {
        let stderr = String::from_utf8_lossy(stderr);
        assert!(stderr.starts_with("session "), "stderr: {stderr}");
    }
}

enum DetachedLifecycleFailure {
    AlreadyTerminal,
    Other(String),
}

fn detached_run_lifecycle_attempt() -> Result<(), DetachedLifecycleFailure> {
    let mut starter = haider();
    starter
        .args([
            "run",
            "--provider",
            "fake",
            "--start",
            "--json",
            "-p",
            "wait",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", r#"[{"step":"hang"}]"#);
    let started = output_with_boot_retry(&mut starter);
    if !started.status.success() {
        return Err(DetachedLifecycleFailure::Other(format!(
            "start stderr: {}",
            String::from_utf8_lossy(&started.stderr)
        )));
    }
    let started: serde_json::Value = serde_json::from_slice(&started.stdout)
        .map_err(|error| DetachedLifecycleFailure::Other(format!("start JSON: {error}")))?;
    if started["outcome"] != "started" {
        return Err(DetachedLifecycleFailure::Other(format!(
            "unexpected start outcome: {}",
            started["outcome"]
        )));
    }
    let run_id = started["run_id"]
        .as_str()
        .ok_or_else(|| DetachedLifecycleFailure::Other("start omitted run id".into()))?
        .to_owned();

    let invoke = |arguments: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haider"));
        configure_test_home(&mut command, &starter.profile);
        command
            .args(arguments)
            .env("HAIDER_PROFILE_DIR", &starter.profile)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env("HAIDER_TEST_FAKE_PROVIDER", r#"[{"step":"hang"}]"#);
        bounded_output(&mut command, None)
    };
    let status = invoke(&["run", "--status", &run_id, "--json"]);
    if !status.status.success() {
        return Err(DetachedLifecycleFailure::Other(format!(
            "status stderr: {}",
            String::from_utf8_lossy(&status.stderr)
        )));
    }
    let status: serde_json::Value = serde_json::from_slice(&status.stdout)
        .map_err(|error| DetachedLifecycleFailure::Other(format!("status JSON: {error}")))?;
    if status["schema"] != "haider.run.status.v1" || status["result"]["run_id"] != run_id {
        return Err(DetachedLifecycleFailure::Other(format!(
            "unexpected lifecycle status: {status}"
        )));
    }

    let stopped = invoke(&["run", "--stop", &run_id, "--json"]);
    if !stopped.status.success() {
        return Err(DetachedLifecycleFailure::Other(format!(
            "stop stderr: {}",
            String::from_utf8_lossy(&stopped.stderr)
        )));
    }
    let stopped: serde_json::Value = serde_json::from_slice(&stopped.stdout)
        .map_err(|error| DetachedLifecycleFailure::Other(format!("stop JSON: {error}")))?;
    if stopped["schema"] != "haider.run.stop.v1" {
        return Err(DetachedLifecycleFailure::Other(format!(
            "unexpected stop response: {stopped}"
        )));
    }
    match stopped["result"]["status"].as_str() {
        Some("accepted") => Ok(()),
        Some("already_terminal") => Err(DetachedLifecycleFailure::AlreadyTerminal),
        status => Err(DetachedLifecycleFailure::Other(format!(
            "unexpected stop status: {status:?}"
        ))),
    }
}

#[test]
fn detached_run_lifecycle_is_addressable_by_run_id() {
    let result = detached_run_lifecycle_attempt();
    #[cfg(windows)]
    let result = match result {
        // One bounded retry starts a wholly fresh profile, daemon, run, and
        // command identity. AlreadyTerminal never counts as cancellation.
        Err(DetachedLifecycleFailure::AlreadyTerminal) => detached_run_lifecycle_attempt(),
        result => result,
    };
    match result {
        Ok(()) => {}
        Err(DetachedLifecycleFailure::AlreadyTerminal) => {
            panic!("fresh hanging run became terminal before cancellation")
        }
        Err(DetachedLifecycleFailure::Other(message)) => panic!("{message}"),
    }
}

#[test]
fn unknown_lifecycle_run_is_a_machine_readable_error() {
    let out = haider_with_boot_retry(&["run", "--status", "missing-run", "--json"], &[]);
    assert!(!out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("status error JSON");
    assert_eq!(value["schema"], "haider.run.status.v1");
    assert!(value["result"].is_null());
    assert_eq!(value["error"]["code"], "not_found");
}

#[test]
fn replay_is_a_read_only_exact_durable_projection() {
    let mut source = haider();
    source
        .args([
            "run",
            "--provider",
            "fake",
            "--allow-writes",
            "--seed",
            "0",
            "--json",
            "-p",
            "repeat",
        ])
        .env(
            "HAIDER_TEST_FAKE_PROVIDER",
            r#"[{"step":"emit_text","text":"ORIGINAL"},{"step":"emit_usage","usage":{"input":2,"output":1,"reasoning":0,"cached":0,"source":"locally_exact"}},{"step":"finish","reason":"end_turn"}]"#,
        )
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0");
    let original = output_with_boot_retry(&mut source);
    assert!(
        original.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&original.stderr)
    );
    let original: serde_json::Value =
        serde_json::from_slice(&original.stdout).expect("source JSON");
    let run_id = original["run_id"].as_str().expect("source run id");
    let session_id = original["session_id"].as_str().expect("source session id");
    assert!(
        original["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|event| {
                event["payload"]["type"] == "headless_run_configured"
                    && event["payload"]["permission_overrides"]["allow_writes"] == true
                    && event["payload"]["seed"] == 0
                    && event["payload"]["provider"] == "fake"
                    && event["payload"]["model"] == "fake-model"
                    && event["payload"]["max_output_tokens"]
                        .as_u64()
                        .is_some_and(|tokens| tokens > 0)
                    && event["payload"]["cwd"]
                        .as_str()
                        .is_some_and(|cwd| !cwd.is_empty())
            }))
    );

    let mut replay_command = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut replay_command, &source.profile);
    replay_command
        .args(["run", "--replay", run_id])
        .env("HAIDER_PROFILE_DIR", &source.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .env(
            "HAIDER_TEST_FAKE_PROVIDER",
            r#"[{"step":"emit_text","text":"REEXECUTED"},{"step":"emit_usage","usage":{"input":7,"output":3,"reasoning":0,"cached":0,"source":"estimated"}},{"step":"finish","reason":"end_turn"}]"#,
        )
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0");
    let replay = output_with_boot_retry(&mut replay_command);
    assert!(
        replay.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay_bytes = replay.stdout.clone();
    let replay: serde_json::Value = serde_json::from_slice(&replay.stdout).expect("replay JSON");
    assert_eq!(replay["schema"], "haider.run.replay.v1");
    assert_eq!(replay["mode"], "durable_journal");
    assert_eq!(replay["source_run_id"], run_id);
    assert_eq!(replay["session_id"], session_id);
    assert_eq!(replay["provider_requests"], 0);
    assert_eq!(replay["response"], "ORIGINAL");
    assert_eq!(replay["events"], original["events"]);
    assert_eq!(replay["integrity"]["sequences_strictly_increasing"], true);
    assert_eq!(replay["integrity"]["run_id_stable"], true);
    assert_eq!(replay["integrity"]["exactly_one_typed_terminal"], true);
    assert_eq!(replay["integrity"]["terminal_seq_matches_status"], true);
    assert_eq!(
        replay["equivalence"]["definition"],
        "durable_run_projection_v1"
    );
    assert_eq!(replay["equivalence"]["final_text_matches"], true);
    assert_eq!(replay["equivalence"]["tool_trace_matches"], true);
    assert_eq!(replay["equivalence"]["usage_matches"], true);
    assert_eq!(replay["equivalence"]["terminal_matches"], true);
    assert_eq!(replay["equivalence"]["equivalent"], true);

    // The daemon above exits after every command. A second replay therefore
    // observes a different live daemon/worker generation, but the immutable
    // source-run projection must remain byte-for-byte identical.
    let mut repeated_replay = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut repeated_replay, &source.profile);
    repeated_replay
        .args(["run", "--replay", run_id])
        .env("HAIDER_PROFILE_DIR", &source.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .env_remove("HAIDER_TEST_FAKE_PROVIDER")
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0");
    let repeated_replay = output_with_boot_retry(&mut repeated_replay);
    assert!(
        repeated_replay.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&repeated_replay.stderr)
    );
    assert_eq!(repeated_replay.stdout, replay_bytes);

    // A re-execution would consume the poison script above and append a
    // second session. The read-only command leaves the durable cardinality at
    // exactly the source session and succeeds without credentials/provider IO.
    let mut status_command = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut status_command, &source.profile);
    status_command
        .args(["status", "--json"])
        .env("HAIDER_PROFILE_DIR", &source.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .env_remove("HAIDER_TEST_FAKE_PROVIDER");
    let status = output_with_boot_retry(&mut status_command);
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["session_count"], 1);
}

#[test]
fn replay_is_sealed_at_terminal_before_late_same_run_task_facts() {
    let script = serde_json::json!([
        {
            "step": "emit_tool_call",
            "call_id": "background-replay-1",
            "name": "exec",
            "args": {
                "command": REPLAY_BACKGROUND_COMMAND,
                "background": true,
                "name": "replay-late-task"
            }
        },
        {"step": "finish", "reason": "tool_use"},
        {"step": "expect_tool_result", "call_id": "background-replay-1"},
        {"step": "emit_text", "text": "sealed before task completion"},
        {"step": "finish", "reason": "end_turn"}
    ])
    .to_string();
    let mut source = haider();
    source
        .args([
            "run",
            "--provider",
            "fake",
            "--allow-exec",
            "--json",
            "-p",
            "start a background task",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", script);
    let source_output = output_with_boot_retry(&mut source);
    assert!(
        source_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&source_output.stderr)
    );
    let source_json: serde_json::Value =
        serde_json::from_slice(&source_output.stdout).expect("source JSON");
    let run_id = source_json["run_id"].as_str().expect("source run id");
    let session_id = source_json["session_id"]
        .as_str()
        .expect("source session id");
    assert_eq!(
        source_json["background_tasks_running"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    let replay = |profile: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haider"));
        configure_test_home(&mut command, profile);
        command
            .args(["run", "--replay", run_id])
            .env("HAIDER_PROFILE_DIR", profile)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env_remove("HAIDER_TEST_FAKE_PROVIDER");
        command.output().expect("replay executes")
    };
    let before_completion = replay(&source.profile);
    assert!(
        before_completion.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&before_completion.stderr)
    );

    let workspace = source
        .profile
        .parent()
        .expect("profile parent")
        .join("workspace");
    std::fs::write(workspace.join("replay-release"), b"release").expect("release background task");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut session = Command::new(env!("CARGO_BIN_EXE_haider"));
        configure_test_home(&mut session, &source.profile);
        session
            .args(["session", session_id, "--json"])
            .env("HAIDER_PROFILE_DIR", &source.profile)
            .env("HAIDER_DISCOVERY_DISABLED", "1");
        let observed = session.output().expect("session observation executes");
        assert!(observed.status.success());
        if String::from_utf8_lossy(&observed.stdout).contains("task_completed") {
            break;
        }
        assert!(Instant::now() < deadline, "late task never completed");
        thread::sleep(Duration::from_millis(20));
    }

    let after_completion = replay(&source.profile);
    assert!(
        after_completion.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&after_completion.stderr)
    );
    assert_eq!(after_completion.stdout, before_completion.stdout);
    let replay_json: serde_json::Value =
        serde_json::from_slice(&after_completion.stdout).expect("replay JSON");
    assert_eq!(
        replay_json["events"]
            .as_array()
            .and_then(|events| events.last())
            .map(|event| &event["payload"]),
        Some(&serde_json::json!({"type": "run_state", "state": "done"}))
    );
}

#[test]
fn session_readiness_and_resume_are_finite_event_driven_json_barriers() {
    let mut source = haider();
    source
        .args(["run", "--provider", "fake", "--json", "-p", "ready"])
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0");
    let source_output = output_with_boot_retry(&mut source);
    assert!(
        source_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&source_output.stderr)
    );
    let source_json: serde_json::Value =
        serde_json::from_slice(&source_output.stdout).expect("source JSON");
    let session_id = source_json["session_id"]
        .as_str()
        .expect("source session id");

    let mut ready = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut ready, &source.profile);
    ready
        .args([
            "sessions",
            "wait-ready",
            "--count",
            "1",
            "--session",
            session_id,
            "--timeout",
            "2s",
            "--json",
        ])
        .env("HAIDER_PROFILE_DIR", &source.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1");
    let ready = output_with_boot_retry(&mut ready);
    assert!(
        ready.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ready.stderr)
    );
    let ready: serde_json::Value = serde_json::from_slice(&ready.stdout).expect("readiness JSON");
    assert_eq!(ready["schema"], "haider.sessions.ready.v1");
    assert_eq!(ready["ready"], true);
    assert_eq!(ready["daemon_ready"], true);
    assert_eq!(ready["expected_count"], 1);
    assert_eq!(ready["ready_count"], 1);
    assert_eq!(ready["ready_session_ids"], serde_json::json!([session_id]));
    assert_eq!(ready["state_counts"]["idle"], 1);

    let mut resume = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut resume, &source.profile);
    resume
        .args(["resume", session_id, "--json", "--timeout", "2s"])
        .env("HAIDER_PROFILE_DIR", &source.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1");
    let resume = output_with_boot_retry(&mut resume);
    assert!(
        resume.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&resume.stderr)
    );
    let resume: serde_json::Value = serde_json::from_slice(&resume.stdout).expect("resume JSON");
    assert_eq!(resume["schema"], "haider.session.resume.v1");
    assert_eq!(resume["session_id"], session_id);
    assert_eq!(resume["completed"], true);
    assert_eq!(resume["outcome"], "idle");
    assert_eq!(resume["run_state"], "idle");

    let mut unmet = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut unmet, &source.profile);
    unmet
        .args([
            "sessions",
            "wait-ready",
            "--count",
            "2",
            "--timeout",
            "50ms",
            "--json",
        ])
        .env("HAIDER_PROFILE_DIR", &source.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1");
    let unmet = unmet.output().expect("unmet readiness command runs");
    assert_eq!(unmet.status.code(), Some(i32::from(EX_TIMEOUT)));
    let unmet: serde_json::Value = serde_json::from_slice(&unmet.stdout).expect("unmet JSON");
    assert_eq!(unmet["daemon_ready"], true);
    assert!(
        unmet["daemon_generation"]
            .as_u64()
            .is_some_and(|value| value > 0)
    );
    assert_eq!(unmet["expected_count"], 2);
    assert_eq!(unmet["ready_count"], 1);
    assert_eq!(unmet["total_session_count"], 1);
    assert_eq!(unmet["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(unmet["error"]["code"], "timeout");

    let mut timeout = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut timeout, &source.profile);
    timeout
        .args([
            "sessions",
            "wait-ready",
            "--count",
            "1",
            "--session",
            "session-does-not-exist",
            "--timeout",
            "10ms",
            "--json",
        ])
        .env("HAIDER_PROFILE_DIR", &source.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1");
    let timeout = timeout.output().expect("readiness timeout command runs");
    assert_eq!(timeout.status.code(), Some(i32::from(EX_TIMEOUT)));
    let timeout: serde_json::Value = serde_json::from_slice(&timeout.stdout).expect("timeout JSON");
    assert_eq!(timeout["ready"], false);
    assert_eq!(timeout["timed_out"], true);
    assert_eq!(timeout["error"]["code"], "timeout");

    let mut hanging = haider();
    hanging
        .args([
            "run",
            "--provider",
            "fake",
            "--start",
            "--json",
            "-p",
            "stay running",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", r#"[{"step":"hang"}]"#);
    let hanging_started = output_with_boot_retry(&mut hanging);
    assert!(
        hanging_started.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&hanging_started.stderr)
    );
    let hanging_started: serde_json::Value =
        serde_json::from_slice(&hanging_started.stdout).expect("hanging start JSON");
    let hanging_session = hanging_started["session_id"]
        .as_str()
        .expect("hanging session id");
    let hanging_run = hanging_started["run_id"].as_str().expect("hanging run id");
    thread::sleep(Duration::from_millis(100));

    let mut running_resume = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut running_resume, &hanging.profile);
    running_resume
        .args(["resume", hanging_session, "--json", "--timeout", "50ms"])
        .env("HAIDER_PROFILE_DIR", &hanging.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1");
    let running_resume = running_resume.output().expect("running resume executes");
    assert_eq!(running_resume.status.code(), Some(i32::from(EX_TIMEOUT)));
    let running_resume: serde_json::Value =
        serde_json::from_slice(&running_resume.stdout).expect("running resume JSON");
    assert_eq!(running_resume["daemon_ready"], true);
    assert_eq!(running_resume["session_id"], hanging_session);
    assert_eq!(running_resume["run_id"], hanging_run);
    assert_eq!(running_resume["run_state"], "running");
    assert_eq!(running_resume["outcome"], "timeout");
    assert_eq!(running_resume["error"]["code"], "timeout");

    let mut stop = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut stop, &hanging.profile);
    stop.args(["run", "--stop", hanging_run, "--json"])
        .env("HAIDER_PROFILE_DIR", &hanging.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1");
    let stopped = bounded_output(&mut stop, None);
    assert!(
        stopped.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
}

/// MUTATION CHECK: remove the raw JSONL pre-scan, restrict the typed emitter
/// to single JSON, or omit the final post-acceptance write. Expected runtime
/// failure: one case has empty/non-JSON stdout or does not end in `error`.
#[test]
fn run_jsonl_bootstrap_failures_always_end_in_a_typed_error_record() {
    let malformed = haider()
        .args(["run", "--bogus", "--output", "jsonl"])
        .output()
        .expect("malformed run executes");
    let values = assert_terminal_jsonl_error(&malformed);
    assert_eq!(
        values.last().expect("error")["error"]["code"],
        "invalid_argument"
    );

    let mut invalid_profile = haider();
    std::fs::create_dir_all(&invalid_profile.profile).expect("profile directory");
    std::fs::write(invalid_profile.profile.join("config.json"), b"{")
        .expect("malformed profile config");
    let invalid_profile = invalid_profile
        .args(["run", "--jsonl", "hello"])
        .output()
        .expect("invalid profile run executes");
    let values = assert_terminal_jsonl_error(&invalid_profile);
    assert_eq!(
        values.last().expect("error")["error"]["code"],
        "protocol_mismatch"
    );

    let missing_attachment = haider()
        .args([
            "run",
            "--provider",
            "fake",
            "--jsonl",
            "--attach",
            "missing-fixture.txt",
            "hello",
        ])
        .output()
        .expect("missing attachment run executes");
    let values = assert_terminal_jsonl_error(&missing_attachment);
    assert_eq!(
        values.last().expect("error")["error"]["code"],
        "attachment_io"
    );

    let mut no_account_command = haider();
    let no_account = no_account_command
        .args(["run", "--jsonl", "hello"])
        .output()
        .expect("no-account run executes");
    let no_account_logs = daemon_logs(&no_account_command.profile);
    if no_account.status.code() == Some(69)
        && no_account_logs.contains("bind Unix socket")
        && no_account_logs.contains("Operation not permitted")
    {
        eprintln!("local IPC is sandbox-denied; daemon JSONL cases skipped");
        return;
    }
    let values = assert_terminal_jsonl_error(&no_account);
    assert_eq!(
        values.last().expect("error")["error"]["code"],
        haider_client::ERROR_CODE_NO_ACTIVE_ACCOUNT
    );

    let create_refusal = haider()
        .args([
            "run",
            "--provider",
            "unknown",
            "--model",
            "fixture-model",
            "--jsonl",
            "hello",
        ])
        .output()
        .expect("create-refusal run executes");
    let values = assert_terminal_jsonl_error(&create_refusal);
    assert_eq!(
        values.last().expect("error")["error"]["code"],
        "invalid_argument"
    );

    let atomic_create_refusal = haider()
        .args([
            "run",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--effort",
            "definitely-unsupported",
            "--jsonl",
            "hello",
        ])
        .output()
        .expect("atomic create refusal executes");
    let values = assert_terminal_jsonl_error(&atomic_create_refusal);
    assert_eq!(values[0]["event"], "error");
    assert_eq!(
        values.last().expect("error")["error"]["code"],
        "effort_unsupported"
    );
}

/// MUTATION CHECK: change the one-shot lifetime back to Persistent or drop
/// the authenticated ownership token. Expected runtime failure: either PID
/// remains live, the socket/owner file survives, or the profile lock stays
/// held after the command exits.
#[cfg(unix)]
#[test]
fn one_shot_reaps_only_the_daemon_it_spawned_on_success_and_bootstrap_failure() {
    let delayed_fake = concat!(
        r#"[{"step":"delay","ms":300},{"step":"emit_text","text":"done"},{"step":"finish","reason":"end_turn"},"#,
        r#"{"step":"emit_text","text":"done"},{"step":"finish","reason":"end_turn"}]"#,
    );
    let mut success = haider();
    let success_endpoint = configure_isolated_runtime(&mut success);
    success
        .env("HAIDER_TEST_FAKE_PROVIDER", delayed_fake)
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["run", "--provider", "fake", "--jsonl", "hello"]);
    let success_child = success.spawn().expect("spawn successful one-shot");
    let success_pid = wait_for_daemon_pid(&success.profile);
    let success_output = success_child
        .wait_with_output()
        .expect("successful one-shot exits");
    let success_logs = daemon_logs(&success.profile);
    if !success_output.status.success()
        && success_logs.contains("bind Unix socket")
        && success_logs.contains("Operation not permitted")
    {
        eprintln!("local IPC is sandbox-denied; owned-daemon host pin skipped");
        return;
    }
    assert!(
        success_output.status.success(),
        "stdout: {}; stderr: {}; daemon logs: {}",
        String::from_utf8_lossy(&success_output.stdout),
        String::from_utf8_lossy(&success_output.stderr),
        success_logs
    );
    assert!(
        !process_is_alive(success_pid),
        "owned daemon must be reaped"
    );
    assert!(!success_endpoint.exists(), "owned socket must be removed");
    assert!(!success.profile.join("lock.owner").exists());
    wait_for_profile_lock(&success.profile, true);

    let mut bootstrap_failure = haider();
    let failure_endpoint = configure_isolated_runtime(&mut bootstrap_failure);
    bootstrap_failure
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["run", "--jsonl", "hello"]);
    let failure_child = bootstrap_failure
        .spawn()
        .expect("spawn bootstrap-failing one-shot");
    let failure_pid = wait_for_daemon_pid(&bootstrap_failure.profile);
    let failure_output = failure_child
        .wait_with_output()
        .expect("bootstrap-failing one-shot exits");
    assert_eq!(failure_output.status.code(), Some(65));
    assert_terminal_jsonl_error(&failure_output);
    assert!(
        !process_is_alive(failure_pid),
        "owned daemon must be reaped"
    );
    assert!(!failure_endpoint.exists(), "owned socket must be removed");
    assert!(!bootstrap_failure.profile.join("lock.owner").exists());
    wait_for_profile_lock(&bootstrap_failure.profile, true);
}

/// Regression guard for the measured +1.04 ms cost of accidentally restoring
/// a multi-threaded Tokio runtime to `haider run`. The daemon is already
/// resident, so this observes only the staged CLI: main + its blocking JSONL
/// adapter worker. The durable queued-run envelope fences `turn.submit`, not
/// merely the earlier session-resolution announcement, from the steady-state
/// window; a cold owned-child waiter must never enter this path.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn staged_run_with_resident_daemon_has_two_steady_state_threads() {
    let haider_binary = PathBuf::from(env!("CARGO_BIN_EXE_haider"));
    let warmed = Command::new(&haider_binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("warm staged CLI binary");
    assert!(warmed.success(), "staged CLI warm-up failed: {warmed}");

    let delayed_fake = concat!(
        r#"[{"step":"delay","ms":750},{"step":"emit_text","text":"done"},"#,
        r#"{"step":"finish","reason":"end_turn"}]"#,
    );
    let mut resident = haider();
    let deep_home = resident
        ._profile_root
        .path()
        .join("integration-style-home")
        .join("h".repeat(100));
    std::fs::create_dir_all(&deep_home).expect("create deep machine-user home");
    let mut profile_env = haider_client::ProfileEnv::capture();
    profile_env.profile_dir = Some(resident.profile.clone());
    profile_env.home = Some(deep_home.clone());
    profile_env.user_profile = Some(deep_home.clone());
    profile_env.runtime_dir = None;
    profile_env.xdg_runtime_dir = None;
    let staged_profile = haider_client::resolve_profile(&profile_env)
        .expect("resolve staged deep-HOME profile coordinate");
    assert!(
        !staged_profile.endpoint_path.starts_with(&deep_home),
        "deep HOME must exercise the short endpoint fallback"
    );
    resident
        .env("HOME", &deep_home)
        .env("USERPROFILE", &deep_home)
        .env("HAIDER_TEST_FAKE_PROVIDER", delayed_fake)
        .args(["status", "--json"]);
    let status = resident.output().expect("start resident daemon");
    assert!(
        status.status.success(),
        "resident status failed: {}; daemon logs: {}",
        String::from_utf8_lossy(&status.stderr),
        daemon_logs(&resident.profile)
    );
    let status_document: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("resident status JSON");
    let resident_pid = status_document["daemon"]["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("resident daemon PID");
    assert_eq!(
        status_document["daemon"]["socket_path"].as_str(),
        Some(staged_profile.endpoint_path.to_string_lossy().as_ref()),
        "resident daemon and staged client must share one endpoint"
    );
    let resident_daemon_logs = daemon_log_count(&resident.profile);
    assert_eq!(resident_daemon_logs, 1, "one resident daemon candidate");

    let workspace = resident._profile_root.path().join("workspace");
    let mut command = Command::new(&haider_binary);
    configure_test_home(&mut command, &resident.profile);
    command
        .current_dir(workspace)
        .env("HAIDER_PROFILE_DIR", &resident.profile)
        .env("HOME", &deep_home)
        .env("USERPROFILE", &deep_home)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .env("HAIDER_TEST_FAKE_PROVIDER", delayed_fake)
        .args(["run", "--provider", "fake", "--jsonl", "thread guard"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("start staged run thread probe");
    let pid = child.id();
    let staged_stdout = child.stdout.take().expect("capture staged run stdout");
    let (fence_tx, fence_rx) = std::sync::mpsc::sync_channel(1);
    let fence_reader = thread::Builder::new()
        .name("staged-stdout-read".to_owned())
        .spawn(move || {
            let mut stdout = BufReader::new(staged_stdout);
            let mut accepted_line = String::new();
            let fence = (|| {
                match stdout.read_line(&mut accepted_line) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "staged run closed stdout before its session announcement",
                        ));
                    }
                    Ok(_) => {}
                    Err(error) => return Err(error),
                }
                loop {
                    let mut line = String::new();
                    match stdout.read_line(&mut line) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "staged run closed stdout before durable run acceptance",
                            ));
                        }
                        Ok(_) if is_durable_run_acceptance(&line) => {
                            return Ok((accepted_line, line, stdout));
                        }
                        Ok(_) => {}
                        Err(error) => return Err(error),
                    }
                }
            })();
            let _ = fence_tx.send(fence);
        })
        .expect("start bounded acceptance reader");
    let (accepted_line, queued_line, mut stdout) =
        match fence_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(fence)) => fence,
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                fence_reader
                    .join()
                    .expect("join failed staged acceptance reader");
                panic!("staged run did not cross its durable acceptance fence: {error}");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                fence_reader
                    .join()
                    .expect("join timed-out staged acceptance reader");
                panic!("staged run durable acceptance deadline elapsed: {error}");
            }
        };
    fence_reader.join().expect("join staged acceptance reader");
    let accepted: serde_json::Value =
        serde_json::from_str(&accepted_line).expect("staged run acceptance JSON");
    assert_eq!(accepted["event"], "accepted");
    assert!(
        is_durable_run_acceptance(&queued_line),
        "steady-state fence must be the durable queued-run envelope"
    );
    assert_eq!(
        daemon_pid(&resident.profile),
        Some(resident_pid),
        "staged run must retain the exact resident daemon generation"
    );
    assert_eq!(
        daemon_log_count(&resident.profile),
        resident_daemon_logs,
        "staged run must not launch a second daemon candidate"
    );
    assert!(
        child
            .try_wait()
            .expect("poll accepted staged run")
            .is_none(),
        "staged run exited before its steady-state thread sample"
    );

    // A multi-threaded runtime keeps the count elevated, while a short-lived
    // reaper or adapter helper can make one instantaneous sample read high.
    // Define steady state as ten consecutive exact-two samples so transients
    // before settling do not weaken the runtime-shape regression guard. On a
    // slow macOS runner each sample forks `ps`; if the deadline expires before
    // ten samples, a nonempty sequence containing only exact-two observations
    // still disproves a persistently elevated multi-threaded runtime.
    const SETTLED_SAMPLE_COUNT: usize = 10;
    let settling_deadline = Instant::now() + Duration::from_millis(500);
    let mut consecutive_exact_two = 0_usize;
    let mut observed_thread_counts = Vec::new();
    loop {
        match staged_process_thread_count(pid).expect("sample staged run steady-state threads") {
            Some(count) => {
                observed_thread_counts.push(count);
                if count == 2 {
                    consecutive_exact_two = consecutive_exact_two.saturating_add(1);
                } else {
                    consecutive_exact_two = 0;
                }
                if consecutive_exact_two == SETTLED_SAMPLE_COUNT {
                    break;
                }
            }
            None if observed_thread_counts.is_empty() => {
                eprintln!(
                    "macOS sandbox denied ps thread inspection; staged thread guard skipped locally"
                );
                break;
            }
            None => panic!(
                "staged run thread inspection became unavailable after observations: \
                 {observed_thread_counts:?}"
            ),
        }
        if Instant::now() >= settling_deadline {
            assert!(
                !observed_thread_counts.is_empty()
                    && observed_thread_counts.iter().all(|count| *count == 2),
                "staged run never settled at exactly two threads for \
                 {SETTLED_SAMPLE_COUNT} consecutive samples, and the bounded sample contained \
                 an excursion; observed {observed_thread_counts:?}"
            );
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let mut remaining_stdout = Vec::new();
    stdout
        .read_to_end(&mut remaining_stdout)
        .expect("drain staged run stdout");
    let output = child.wait_with_output().expect("staged run exits");
    assert!(
        output.status.success(),
        "staged run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// An attached one-shot has no ownership token and must leave the exact
/// incumbent generation serving after completion.
#[cfg(unix)]
#[test]
fn one_shot_never_shuts_down_a_prestarted_incumbent() {
    let mut incumbent = haider();
    let endpoint = configure_isolated_runtime(&mut incumbent);
    let ready = incumbent.output().expect("prestart incumbent");
    let incumbent_logs = daemon_logs(&incumbent.profile);
    if !ready.status.success()
        && incumbent_logs.contains("bind Unix socket")
        && incumbent_logs.contains("Operation not permitted")
    {
        eprintln!("local IPC is sandbox-denied; incumbent host pin skipped");
        return;
    }
    assert!(
        ready.status.success(),
        "stderr: {}; daemon logs: {}",
        String::from_utf8_lossy(&ready.stderr),
        incumbent_logs
    );
    let pid = daemon_pid(&incumbent.profile).expect("incumbent PID");

    let mut attached = Command::new(env!("CARGO_BIN_EXE_haider"));
    configure_test_home(&mut attached, &incumbent.profile);
    let output = attached
        .args(["run", "--provider", "fake", "--jsonl", "hello"])
        .env("HAIDER_PROFILE_DIR", &incumbent.profile)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .env("HAIDER_TEST_FAKE_PROVIDER", DEFAULT_FAKE_SCRIPT)
        .output()
        .expect("attached one-shot exits");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(daemon_pid(&incumbent.profile), Some(pid));
    assert!(process_is_alive(pid));
    assert!(endpoint.exists());
    wait_for_profile_lock(&incumbent.profile, false);
    terminate_daemon_checked(&incumbent.profile).expect("stop incumbent fixture");
}

/// MUTATION CHECK: drop the profile-default fallback, restore catalog-first
/// substitution, restore the custom summary membership filter, bypass the
/// selector path, treat the custom catalog as authoritative, skip its one
/// refresh-on-miss probe, or restore the per-turn model-list membership probe.
/// Expected runtime failure: the captured OpenAI-compatible chat body contains
/// `canonical-other`, a provider prefix, no request, or a discovery count is
/// not exactly one. The 404 case pins that even refresh failure stays advisory.
#[test]
fn configured_custom_model_reaches_chat_wire_verbatim_despite_catalog() {
    const CATALOG: &[u8] =
        br#"{"object":"list","data":[{"id":"canonical-other","object":"model"}]}"#;
    match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => drop(listener),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("local loopback listeners are sandbox-denied; host proxy pin skipped");
            return;
        }
        Err(error) => panic!("probe compatible proxy bind: {error}"),
    }

    let (unqualified, unqualified_discoveries) = run_custom_model_wire_case(
        "deepseek-v4-flash",
        None,
        &["canonical-other"],
        Some(CATALOG),
    );
    assert_eq!(unqualified["model"], "deepseek-v4-flash");
    assert_eq!(unqualified_discoveries, 1);

    let (qualified, qualified_discoveries) = run_custom_model_wire_case(
        "bench-proxy/deepseek-v4-flash",
        None,
        &["canonical-other"],
        Some(CATALOG),
    );
    assert_eq!(qualified["model"], "deepseek-v4-flash");
    assert_eq!(qualified_discoveries, 1);

    let (explicit, explicit_discoveries) = run_custom_model_wire_case(
        "bench-proxy/deepseek-v4-flash",
        Some("explicit-wire-model"),
        &["canonical-other"],
        Some(CATALOG),
    );
    assert_eq!(explicit["model"], "explicit-wire-model");
    assert_eq!(explicit_discoveries, 1);

    let (no_catalog, no_catalog_discoveries) = run_custom_model_wire_case(
        "bench-proxy/deepseek-v4-flash",
        None,
        &["canonical-other"],
        None,
    );
    assert_eq!(no_catalog["model"], "deepseek-v4-flash");
    assert_eq!(no_catalog_discoveries, 1);
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
    assert_eq!(value["model"], haider_client::PACKAGED_DEFAULT_MODEL);
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
    configure_test_home(&mut command, &profile);
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
    // Provider resolution belongs to the daemon after durable acceptance, so
    // JSONL exposes the credential-specific Errored audit trail instead of
    // collapsing a missing credential into a provider transport failure.
    let envelopes = parse_jsonl(&out.stdout);
    assert_eq!(
        envelopes.last().map(typed),
        Some(Some(EventPayload::RunState(RunState::Errored)))
    );
    let terminals = envelopes
        .iter()
        .filter(|envelope| envelope.payload.get("terminal_kind").is_some())
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].payload["terminal_kind"], "failure");
    assert_eq!(terminals[0].payload["error_code"], "credential_missing");
    assert!(envelopes.iter().any(|envelope| {
        envelope.payload["type"] == "run_failed" && envelope.payload["code"] == "credential_missing"
    }));
}

#[test]
fn sequential_ephemeral_cli_runs_advance_profile_owned_worker_generations() {
    ensure_haiderd_present();
    let profile_parent = tempfile::tempdir().expect("temporary CLI profile parent");
    let profile = profile_parent.path().join("profile");
    let run = |prompt: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haider"));
        configure_test_home(&mut command, &profile);
        command
            .args(["run", "--provider", "fake", "--jsonl", prompt])
            .env("HAIDER_PROFILE_DIR", &profile)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env("HAIDER_TEST_FAKE_PROVIDER", DEFAULT_FAKE_SCRIPT)
            .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0")
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
    assert!(daemon_pid(&profile).is_none());
    #[cfg(unix)]
    wait_for_profile_lock(&profile, true);
    let second_output = run("restarted process");
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
    assert!(daemon_pid(&profile).is_none());
}

/// Runs `haider` with ONE bounded retry when the autospawned daemon misses
/// its startup deadline under full-gate load (exit 69) — the transient
/// class only; real failures surface with stderr on the caller's assert.
fn haider_with_boot_retry(args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = haider();
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    output_with_boot_retry(&mut command)
}

fn output_with_boot_retry(command: &mut Command) -> std::process::Output {
    let out = bounded_output(command, None);
    if out.status.code() == Some(69) {
        bounded_output(command, None)
    } else {
        out
    }
}

fn haider_with_stdin_boot_retry(args: &[&str], input: &[u8]) -> std::process::Output {
    let run = || {
        let mut command = haider();
        command.args(args);
        bounded_output(&mut command, Some(input))
    };
    let out = run();
    if out.status.code() == Some(69) {
        run()
    } else {
        out
    }
}

const CLI_PROCESS_DEADLINE: Duration = Duration::from_secs(60);

fn bounded_output(command: &mut Command, input: Option<&[u8]>) -> std::process::Output {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("bounded binary starts");
    let mut child_stdout = child.stdout.take().expect("piped child stdout");
    let mut child_stderr = child.stderr.take().expect("piped child stderr");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stdout
            .read_to_end(&mut bytes)
            .map(|_| bytes)
            .expect("read bounded child stdout")
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stderr
            .read_to_end(&mut bytes)
            .map(|_| bytes)
            .expect("read bounded child stderr")
    });
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input)
            .expect("write prompt stdin");
    }
    let deadline = Instant::now() + CLI_PROCESS_DEADLINE;
    loop {
        match child.try_wait().expect("inspect bounded binary") {
            Some(status) => {
                return std::process::Output {
                    status,
                    stdout: stdout_reader.join().expect("join stdout reader"),
                    stderr: stderr_reader.join().expect("join stderr reader"),
                };
            }
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let status = child.wait().expect("reap timed-out binary");
                let output = std::process::Output {
                    status,
                    stdout: stdout_reader.join().expect("join timed-out stdout reader"),
                    stderr: stderr_reader.join().expect("join timed-out stderr reader"),
                };
                panic!(
                    "binary exceeded {CLI_PROCESS_DEADLINE:?}; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
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
    let terminals = envelopes
        .iter()
        .filter(|envelope| envelope.payload.get("terminal_kind").is_some())
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].payload["terminal_kind"], "cancellation");
}

#[test]
fn run_jsonl_timeout_has_one_distinct_timeout_terminal() {
    let out = haider()
        .args([
            "run",
            "--provider",
            "fake",
            "hello",
            "--output",
            "jsonl",
            "--timeout",
            "2s",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", r#"[{"step":"hang"}]"#)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(124));
    let envelopes = parse_jsonl(&out.stdout);
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1)
    );
    let terminals = envelopes
        .iter()
        .filter(|envelope| envelope.payload.get("terminal_kind").is_some())
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].payload["terminal_kind"], "timeout");
    assert_eq!(terminals[0].payload["error_code"], "timeout");
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

#[test]
fn daemon_time_budget_exhaustion_is_typed_and_exits_77() {
    let out = haider_with_boot_retry(
        &[
            "run",
            "--provider",
            "fake",
            "--json",
            "--max-time",
            "20ms",
            "-p",
            "budget",
        ],
        &[("HAIDER_TEST_FAKE_PROVIDER", r#"[{"step":"hang"}]"#)],
    );
    assert_eq!(
        out.status.code(),
        Some(77),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("budget JSON");
    assert_eq!(value["outcome"], "errored");
    assert_eq!(value["error"]["code"], "budget_exhausted");
    assert_eq!(value["budget_exhausted"]["dimension"], "time");
    assert!(value["events"].as_array().is_some_and(|events| {
        events
            .iter()
            .any(|event| event["payload"]["type"] == "run_budget_exhausted")
    }));
}

#[test]
fn fast_final_usage_is_budget_checked_before_done() {
    let out = haider_with_boot_retry(
        &[
            "run",
            "--provider",
            "fake",
            "--json",
            "--max-tokens",
            "2",
            "-p",
            "budget",
        ],
        &[(
            "HAIDER_TEST_FAKE_PROVIDER",
            r#"[{"step":"emit_usage","usage":{"input":2,"output":0,"reasoning":0,"cached":0,"source":"locally_exact"}},{"step":"finish","reason":"end_turn"}]"#,
        )],
    );
    assert_eq!(
        out.status.code(),
        Some(77),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("budget JSON");
    assert_eq!(value["error"]["code"], "budget_exhausted");
    assert_eq!(value["budget_exhausted"]["dimension"], "tokens");
    let states = value["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|event| {
            if event["payload"]["type"] == "run_state" {
                event["payload"]["state"].as_str()
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(states.last(), Some(&"errored"));
    assert!(!states.contains(&"done"));
}

/// §C MUTATION CHECK: invent an answer for a no-default request, park the
/// autonomous run, or stop after the rejected tool result. Expected RUNTIME
/// failure: an answer/menu wait appears, the typed rejection disappears, or
/// the provider does not receive its continuation turn.
#[test]
fn run_nonpermission_input_rejects_without_guessing_and_continues() {
    let script = r#"[
        {"step":"emit_request_input","call_id":"ask","kind":"question","title":"Need input"},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"ask"},
        {"step":"emit_text","text":"continued without invented input"},
        {"step":"finish","reason":"end_turn"}
    ]"#;
    let out = haider_with_boot_retry(
        &["run", "--provider", "fake", "hello", "--jsonl"],
        &[("HAIDER_TEST_FAKE_PROVIDER", script)],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let envelopes = parse_jsonl(&out.stdout);
    let rejection = envelopes
        .iter()
        .find_map(|envelope| match typed(envelope) {
            Some(EventPayload::ToolResult { call_id, result }) if call_id == "ask" => Some(result),
            _ => None,
        })
        .expect("request_input rejection");
    assert_eq!(
        rejection.status,
        haider_protocol::tool::ToolResultStatus::Rejected
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&rejection.preview)
            .expect("typed rejection JSON")["code"],
        "no_human_available"
    );
    assert!(!envelopes.iter().any(|envelope| matches!(
        typed(envelope),
        Some(EventPayload::RunState(RunState::InputRequired { .. }))
            | Some(EventPayload::MenuAnswered(_))
    )));
    assert_eq!(
        envelopes.last().map(typed),
        Some(Some(EventPayload::RunState(RunState::Done)))
    );
}

/// MUTATION CHECK: allocate a second id while accumulating argument chunks,
/// omit the provider call id from completion/result, or skip one cursor. The
/// exact call/result join and the contiguous stream assertions then fail.
#[test]
fn run_jsonl_fragmented_tool_call_keeps_one_call_identity_and_cursor() {
    let workspace = tempfile::tempdir().expect("fragmented tool workspace");
    let script = r#"[
        {"step":"emit_tool_call_start","call_id":"fragmented-call","name":"fs_write"},
        {"step":"emit_tool_args_delta","call_id":"fragmented-call","fragment":"{\"path\":\"fragmented.txt\","},
        {"step":"emit_tool_args_delta","call_id":"fragmented-call","fragment":"\"content\":\"ok\"}"},
        {"step":"emit_tool_call_end","call_id":"fragmented-call"},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"fragmented-call"},
        {"step":"emit_text","text":"fragmented call completed"},
        {"step":"finish","reason":"end_turn"}
    ]"#;
    let out = haider()
        .current_dir(workspace.path())
        .args([
            "run",
            "--provider",
            "fake",
            "--allow-writes",
            "--jsonl",
            "fragmented tool",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", script)
        .output()
        .expect("fragmented tool run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let envelopes = parse_jsonl(&out.stdout);
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1)
    );

    let started = envelopes
        .iter()
        .find(|envelope| {
            envelope.payload["type"] == "item"
                && envelope.payload["event"] == "started"
                && envelope.payload["item"]["item"] == "tool_call"
        })
        .expect("tool-call start");
    let item_id = started.payload["item_id"]
        .as_str()
        .expect("started item id");
    assert_eq!(started.payload["item"]["call_id"], "fragmented-call");

    let deltas = envelopes
        .iter()
        .filter(|envelope| {
            envelope.payload["type"] == "item"
                && envelope.payload["event"] == "delta"
                && envelope.payload["delta"]["delta"] == "tool_args"
        })
        .collect::<Vec<_>>();
    assert!(
        !deltas.is_empty(),
        "fragmented provider arguments must retain an item-correlated delta"
    );
    assert!(
        deltas
            .iter()
            .all(|delta| delta.payload["item_id"] == item_id)
    );

    let completed = envelopes
        .iter()
        .find(|envelope| {
            envelope.payload["type"] == "item"
                && envelope.payload["event"] == "completed"
                && envelope.payload["item"]["item"] == "tool_call"
        })
        .expect("tool-call completion");
    assert_eq!(completed.payload["item_id"], item_id);
    assert_eq!(completed.payload["item"]["call_id"], "fragmented-call");
    assert_eq!(
        completed.payload["item"]["args"],
        serde_json::json!({"path": "fragmented.txt", "content": "ok"})
    );

    let result = envelopes
        .iter()
        .find(|envelope| envelope.payload["type"] == "tool_result")
        .expect("tool result");
    assert_eq!(result.payload["call_id"], "fragmented-call");
    assert_eq!(
        envelopes
            .iter()
            .filter(|envelope| envelope.payload.get("terminal_kind").is_some())
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("fragmented.txt"))
            .expect("tool wrote fragmented payload"),
        "ok"
    );
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
    assert!(denied_json["events"].as_array().is_some_and(|events| {
        events
            .iter()
            .any(|event| event["payload"]["type"] == "tool_result")
    }));

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
        events: HeadlessRunEvents::empty(RunId::new("run-json")),
        budget_exhausted: None,
        replay: None,
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
                    code: HeadlessFailureCode::Run(ErrorCode::WorkflowUnfinished),
                    message: "workflow remains unfinished".into(),
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
            prompt_stdin: false,
            action: RunAction::Execute,
            output: RunOutput::Print,
            timeout: None,
            allow_writes: false,
            allow_exec: false,
            auto_allow: false,
            trust_hooks: false,
            provider: None,
            model: None,
            attachments: Vec::new(),
            budget: haider_protocol::headless::RunBudgetV1::default(),
            seed: None,
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

#[test]
fn run_prompt_flag_and_stdin_have_identical_text() {
    let inline = parse_run_options(&["-p".into(), "same prompt".into()]).expect("inline prompt");
    let stdin = parse_run_options(&["-".into()]).expect("stdin prompt");
    assert_eq!(inline.prompt, "same prompt");
    assert!(!inline.prompt_stdin);
    assert!(stdin.prompt_stdin);
    assert_eq!(
        read_stdin_prompt_from(&b"same prompt\r\n"[..]).expect("stdin text"),
        inline.prompt
    );
}

#[test]
fn empty_prompt_flag_and_stdin_share_invalid_usage_exit() {
    let inline = haider_with_boot_retry(&["run", "--json", "-p", ""], &[]);
    let piped = haider_with_stdin_boot_retry(&["run", "--json", "-"], b"");
    assert_eq!(inline.status.code(), Some(i32::from(EX_USAGE)));
    assert_eq!(piped.status.code(), inline.status.code());
    for output in [inline, piped] {
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("invalid prompt JSON");
        assert_eq!(value["error"]["code"], "invalid_argument");
    }
}

#[test]
fn run_parser_pins_budgets_seed_and_lifecycle() {
    let parsed = parse_run_options(&[
        "--json".into(),
        "--start".into(),
        "--max-tokens".into(),
        "123".into(),
        "--max-cost".into(),
        "0.125001".into(),
        "--max-time".into(),
        "2m".into(),
        "--seed".into(),
        "0".into(),
        "-p".into(),
        "ship".into(),
    ])
    .expect("headless controls");
    assert_eq!(parsed.action, RunAction::Start);
    assert_eq!(parsed.output, RunOutput::Json);
    assert_eq!(parsed.budget.max_tokens, Some(123));
    assert_eq!(parsed.budget.max_cost_microusd, Some(125_001));
    assert_eq!(parsed.budget.max_time_ms, Some(120_000));
    assert_eq!(parsed.seed, Some(0));
    assert_eq!(
        parse_run_options(&["--status".into(), "run-1".into()])
            .expect("status")
            .action,
        RunAction::Status(RunId::new("run-1"))
    );
    assert!(parse_run_options(&["--status".into(), "--json".into()]).is_err());
    assert!(
        parse_run_options(&[
            "--status".into(),
            "run-1".into(),
            "--timeout".into(),
            "1s".into(),
        ])
        .is_err()
    );
    assert!(
        parse_run_options(&[
            "--replay".into(),
            "run-1".into(),
            "--timeout".into(),
            "1s".into(),
        ])
        .is_ok()
    );
    assert_eq!(
        parse_run_options(&["--replay".into(), "run-1".into()])
            .expect("default replay output")
            .output,
        RunOutput::Json
    );
    assert!(
        parse_run_options(&[
            "--replay".into(),
            "run-1".into(),
            "--output".into(),
            "print".into(),
        ])
        .is_err()
    );
    assert!(parse_run_options(&["--replay".into(), "run-1".into(), "--jsonl".into()]).is_err());
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
/// the byte golden or the fixed-key/null assertions change.
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
        "{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{\"count\":0,\"refs\":[]},\"outcome\":\"done\",\"response\":\"final answer\",\"events\":[],\"usage\":null,\"budget_exhausted\":null,\"replay\":null,\"permission_denials\":[],\"background_tasks_running\":[],\"error\":null}\n"
    );
    let value: serde_json::Value = serde_json::from_slice(&json).expect("v1 JSON");
    assert_eq!(value.as_object().expect("object").len(), 15);
    assert_eq!(value["provider"], "fake");
    assert_eq!(value["model"], "fake-model");
    assert!(value["usage"].is_null());
    assert!(value["error"].is_null());

    let envelope = |seq, event_id: &str, payload: serde_json::Value| {
        serde_json::from_value::<RawEnvelope>(serde_json::json!({
            "schema_version": 1,
            "event_id": event_id,
            "seq": seq,
            "session_id": "session-json",
            "run_id": "run-json",
            "device_id": "device-json",
            "authority_epoch": 1,
            "worker_generation": 2,
            "committed_at_ms": 3,
            "render": {"ui": true, "durable": true, "prompt": "omit"},
            "payload": payload,
        }))
        .expect("fixed typed envelope")
    };
    let mut typed_stream = result(HeadlessOutcome::Done, None);
    typed_stream.events = HeadlessRunEvents::from_envelopes(
        RunId::new("run-json"),
        [
            envelope(
                7,
                "event-tool",
                serde_json::json!({
                    "type": "tool_result",
                    "call_id": "call-1",
                    "result": {"preview": "ok", "truncated": false, "status": "completed"},
                }),
            ),
            envelope(
                8,
                "event-usage",
                serde_json::json!({
                    "type": "usage",
                    "input": 10,
                    "output": 2,
                    "source": "provider_reported",
                    "normalized": {
                        "logical_input": 10,
                        "uncached_input": 6,
                        "cache_read_input": 4,
                        "cache_write_input": 3,
                        "billed_output": 2
                    }
                }),
            ),
        ],
    )
    .expect("fixture event ledger");
    let mut first = Vec::new();
    let mut second = Vec::new();
    write_final(&mut first, RunOutput::Json, &typed_stream).expect("first stable stream");
    write_final(&mut second, RunOutput::Json, &typed_stream).expect("second stable stream");
    assert_eq!(first, second, "the same journal summary is byte-stable");
    let stable: serde_json::Value = serde_json::from_slice(&first).expect("stable typed stream");
    assert_eq!(stable["events"][0]["payload"]["type"], "tool_result");
    assert_eq!(
        stable["events"][1]["payload"]["normalized"]["cache_read_input"],
        4
    );
    assert_eq!(
        stable["events"][1]["payload"]["normalized"]["cache_write_input"],
        3
    );

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
            HeadlessOutcome::Started | HeadlessOutcome::Done => {
                unreachable!("successful outcomes are covered separately")
            }
        };
        assert_eq!(
            String::from_utf8(bytes.clone()).expect("failure utf8"),
            format!(
                "{{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{{\"count\":0,\"refs\":[]}},\"outcome\":\"{outcome_name}\",\"response\":null,\"events\":[],\"usage\":null,\"budget_exhausted\":null,\"replay\":null,\"permission_denials\":[],\"background_tasks_running\":[],\"error\":{error}}}\n"
            )
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("failure object");
        assert_eq!(value.as_object().expect("object").len(), 15);
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
        "{\"schema\":\"haider.run.v1\",\"session_id\":\"session-json\",\"run_id\":\"run-json\",\"provider\":\"fake\",\"model\":\"fake-model\",\"attachments\":{\"count\":2,\"refs\":[\"blake3:first\",\"blake3:second\"]},\"outcome\":\"done\",\"response\":null,\"events\":[],\"usage\":null,\"budget_exhausted\":null,\"replay\":null,\"permission_denials\":[],\"background_tasks_running\":[],\"error\":null}\n"
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
