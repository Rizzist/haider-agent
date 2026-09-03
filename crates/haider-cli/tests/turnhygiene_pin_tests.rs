#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Behaviour-preservation pins for the warm turn path (lane `turnhygiene`).
//!
//! Every test here observes the product from the outside — `haider run`
//! stdout bytes, the OpenAI-compatible request body an in-process proxy
//! records, files a hook writes, and the durable replay document — so the
//! turn-hygiene implementation can reshape internals freely while these
//! outcomes stay byte-for-byte fixed. Nothing here reaches into crate
//! internals; the only sibling dependency is the prebuilt `haiderd` binary.
//!
//! Goldens live under `tests/fixtures/turnhygiene/`. Re-bless deliberately
//! with `UPDATE_FIXTURES=1` after reviewing the diff the failing test prints.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const PREBUILT_SIBLINGS_ENV: &str = "HAIDER_TEST_SIBLINGS_PREBUILT";
const FIXTURE_DIR: &str = "tests/fixtures/turnhygiene";
/// Long enough that every run of one test attaches to the daemon the first
/// run spawned; `TestProfile::drop` stops it explicitly.
const RESIDENT_IDLE_TTL_MS: &str = "120000";
const PROCESS_DEADLINE: Duration = Duration::from_secs(60);
const PROXY_PROVIDER: &str = "pinproxy";
const PROXY_MODEL: &str = "pin-model";
fn ensure_siblings_prebuilt() {
    assert_eq!(
        std::env::var(PREBUILT_SIBLINGS_ENV).as_deref(),
        Ok("1"),
        "CLI subprocess pins require a fresh sibling; run \
         `cargo build -p haider-daemond --bin haiderd` first, then set \
         {PREBUILT_SIBLINGS_ENV}=1 for the test command"
    );
    let sibling = PathBuf::from(env!("CARGO_BIN_EXE_haider"))
        .parent()
        .expect("haider binary parent")
        .join(format!("haiderd{}", std::env::consts::EXE_SUFFIX));
    assert!(
        sibling.is_file(),
        "haiderd sibling missing at {}",
        sibling.display()
    );
}
/// One hermetic profile, machine home, and canonical workspace. Every command
/// built from it shares the same daemon rendezvous, so sequential runs
/// exercise one resident daemon; `Drop` stops that daemon.
struct TestProfile {
    _root: tempfile::TempDir,
    profile: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
}

impl TestProfile {
    fn new() -> Self {
        ensure_siblings_prebuilt();
        let root = tempfile::tempdir().expect("temporary profile root");
        let profile = root.path().join("profile");
        let home = root.path().join("machine-home");
        let workspace = root.path().join("workspace");
        for directory in [&profile, &home, &workspace] {
            std::fs::create_dir_all(directory).expect("profile directories");
        }
        let workspace = workspace.canonicalize().expect("canonical workspace");
        Self {
            _root: root,
            profile,
            home,
            workspace,
        }
    }

    fn root(&self) -> &Path {
        self._root.path()
    }

    /// A sibling workspace under the same root (a different canonical cwd).
    fn sibling_workspace(&self, name: &str) -> PathBuf {
        let path = self.root().join(name);
        std::fs::create_dir_all(&path).expect("sibling workspace");
        path.canonicalize().expect("canonical sibling workspace")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haider"));
        command
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env_remove("HAIDER_RUNTIME_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("HAIDER_MODEL")
            .env_remove("HAIDER_TEST_FAKE_PROVIDER")
            .env("HAIDER_PROFILE_DIR", &self.profile)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env("HAIDER_NO_UPDATE_CHECK", "1")
            .env("HAIDER_TEST_DEVICE_NAME", "test-mac")
            .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", RESIDENT_IDLE_TTL_MS)
            .stdin(Stdio::null());
        command
    }

    /// A command against the custom no-auth proxy provider. Automatic
    /// hermetic policy would otherwise bind a lockdown tool pack and drop
    /// project instructions; `turnperf_support.py` measures with it off too.
    fn proxy_command(&self) -> Command {
        let mut command = self.command();
        command.env("HAIDER_AUTO_HERMETIC", "0");
        command
    }

    fn write_custom_provider(&self, origin: &str) {
        std::fs::write(
            self.profile.join("providers.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "providers": [{
                    "provider_id": PROXY_PROVIDER,
                    "display_name": "Turn hygiene proxy",
                    "api_family": "openai_chat_completions",
                    "base_url": origin,
                    "enabled": true,
                    "auth_requirement": "none",
                    "configured_models": [PROXY_MODEL],
                    "default_model": PROXY_MODEL,
                    "provenance": "custom"
                }],
                "fallback_chain": []
            }))
            .expect("providers JSON"),
        )
        .expect("write providers registry");
    }

    fn stop_daemon(&self) {
        let mut stop = self.command();
        stop.args(["daemon", "stop", "--json"]);
        let _ = bounded_output(&mut stop, Duration::from_secs(30));
        if let Some(pid) = daemon_pid(&self.profile)
            && process_is_alive(pid)
        {
            kill_process(pid);
        }
    }
}

impl Drop for TestProfile {
    fn drop(&mut self) {
        self.stop_daemon();
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

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    haider_platform::process_exists(pid)
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    let _ = Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    let _ = haider_platform::kill_process_tree(pid, true);
}

/// Runs one child under a wall-clock bound, draining both pipes on threads so
/// a chatty child never deadlocks on a full pipe.
fn bounded_output(command: &mut Command, deadline: Duration) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("bounded child starts");
    let mut child_stdout = child.stdout.take().expect("piped stdout");
    let mut child_stderr = child.stderr.take().expect("piped stderr");
    let stdout = thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stdout.read_to_end(&mut bytes).expect("read stdout");
        bytes
    });
    let stderr = thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stderr.read_to_end(&mut bytes).expect("read stderr");
        bytes
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        assert!(
            started.elapsed() < deadline,
            "child exceeded its {deadline:?} deadline"
        );
        thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: stdout.join().expect("stdout reader"),
        stderr: stderr.join().expect("stderr reader"),
    }
}

/// One bounded retry for the transient cold-daemon startup miss (exit 69)
/// only; every other failure surfaces on the caller's assertion.
fn output_with_boot_retry(command: &mut Command) -> Output {
    let output = bounded_output(command, PROCESS_DEADLINE);
    if output.status.code() == Some(69) {
        bounded_output(command, PROCESS_DEADLINE)
    } else {
        output
    }
}

fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what}: exit {:?}; stderr: {}; stdout: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

fn jsonl_records(stdout: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8(stdout.to_vec()).expect("JSONL stdout is UTF-8");
    assert!(text.ends_with('\n'), "JSONL must end with LF");
    text.lines()
        .map(|line| serde_json::from_str(line).expect("every JSONL line is one JSON object"))
        .collect()
}

/// Splits the acceptance record from the envelopes and re-checks the cursor
/// law before any pin consumes the stream.
fn accepted_and_envelopes(stdout: &[u8]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut records = jsonl_records(stdout);
    assert!(!records.is_empty(), "JSONL stream has no records");
    let accepted = records.remove(0);
    assert_eq!(accepted["event"], "accepted");
    assert_eq!(accepted["head_seq"], records[0]["seq"]);
    assert!(
        records
            .windows(2)
            .all(|pair| pair[1]["seq"].as_u64() == pair[0]["seq"].as_u64().map(|seq| seq + 1)),
        "JSONL sequence is not contiguous"
    );
    let terminals = records
        .iter()
        .filter(|record| record["payload"].get("terminal_kind").is_some())
        .count();
    assert_eq!(terminals, 1, "exactly one typed terminal");
    (accepted, records)
}

// ---------------------------------------------------------------------------
// Normalisation and goldens
// ---------------------------------------------------------------------------

/// Replaces every maximal lowercase-hex run of exactly 32 or 64 characters
/// (session/run/event/item identities, keyed and content digests) and every
/// 13-digit decimal run (epoch milliseconds) with typed placeholders. Every
/// other byte, including field order, survives.
fn normalize_identities(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase() {
            let mut end = index;
            while end < bytes.len()
                && bytes[end].is_ascii_hexdigit()
                && !bytes[end].is_ascii_uppercase()
            {
                end += 1;
            }
            let run = &text[index..end];
            let all_digits = run.bytes().all(|byte| byte.is_ascii_digit());
            if run.len() == 64 {
                out.push_str("<H64>");
            } else if run.len() == 32 {
                out.push_str("<H32>");
            } else if all_digits && run.len() == 13 {
                out.push_str("<TS>");
            } else {
                out.push_str(run);
            }
            index = end;
        } else {
            let character = text[index..].chars().next().expect("char boundary");
            out.push(character);
            index += character.len_utf8();
        }
    }
    out
}

/// Byte-estimate token counters depend on the workspace path length, which
/// differs per machine; the golden keeps their presence and position only.
fn normalize_numeric_field(text: &str, key: &str) -> String {
    let marker = format!("\"{key}\":");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find(&marker) {
        let after = position + marker.len();
        out.push_str(&rest[..after]);
        let digits = rest[after..].bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            out.push_str("<N>");
        }
        rest = &rest[after + digits..];
    }
    out.push_str(rest);
    out
}

const ESTIMATED_TOKEN_FIELDS: &[&str] = &["input_tokens", "used_tokens", "stable_prefix_tokens"];

fn normalize_jsonl(stdout: &[u8], workspace: &Path, replacements: &[(&str, &str)]) -> String {
    let workspace_text = workspace.to_str().expect("UTF-8 workspace path");
    let escaped_workspace = serde_json::to_string(workspace_text).expect("JSON string");
    let escaped_workspace = escaped_workspace.trim_matches('"');
    let text = String::from_utf8(stdout.to_vec()).expect("UTF-8 JSONL");
    let mut normalized = String::new();
    for line in text.lines() {
        let mut line = line.replace(escaped_workspace, "<CWD>");
        for (from, to) in replacements {
            let escaped = serde_json::to_string(from).expect("JSON string");
            line = line.replace(escaped.trim_matches('"'), to);
        }
        let mut line = normalize_identities(&line);
        for key in ESTIMATED_TOKEN_FIELDS {
            line = normalize_numeric_field(&line, key);
        }
        normalized.push_str(&line);
        normalized.push('\n');
    }
    normalized
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(name)
}

fn assert_golden(name: &str, actual: &str) {
    let path = fixture_path(name);
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().expect("fixture dir")).expect("fixture dir");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("golden {} is unreadable: {error}", path.display()));
    if expected != actual {
        let mut report = String::new();
        for (number, (left, right)) in expected.lines().zip(actual.lines()).enumerate() {
            if left != right {
                report.push_str(&format!(
                    "line {}:\n  golden: {left}\n  actual: {right}\n",
                    number + 1
                ));
            }
        }
        panic!(
            "golden {} differs (expected {} lines, actual {}); re-bless with UPDATE_FIXTURES=1 after review\n{report}",
            path.display(),
            expected.lines().count(),
            actual.lines().count()
        );
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible loopback proxy with a request ledger
// ---------------------------------------------------------------------------

fn read_http_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
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

const CHAT_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-pin\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"pin-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-pin\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"pin-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pinned reply\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-pin\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"pin-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"chatcmpl-pin\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"pin-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n",
    "data: [DONE]\n\n"
);

/// Serves every `POST /chat/completions` with one fixed text reply and keeps
/// the raw request bodies in arrival order — the same shape as the
/// `turnperf_support.py` ledger, in-process.
struct CompatProxy {
    origin: String,
    address: SocketAddr,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CompatProxy {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind compatible proxy");
        let address = listener.local_addr().expect("proxy address");
        let origin = format!("http://{address}/v1");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let ledger = Arc::clone(&bodies);
        let stopping = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            for incoming in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = incoming else { break };
                let (request_line, body) = read_http_request(&mut stream);
                if request_line.starts_with("POST ") && request_line.contains("/chat/completions") {
                    ledger.lock().expect("ledger").push(body);
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        CHAT_SSE.as_bytes(),
                    );
                } else if request_line.contains("/models") {
                    let catalog = serde_json::json!({
                        "object": "list",
                        "data": [{"id": PROXY_MODEL, "object": "model"}]
                    })
                    .to_string();
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        catalog.as_bytes(),
                    );
                } else {
                    write_http_response(&mut stream, "200 OK", "application/json", b"{}");
                }
            }
        });
        Self {
            origin,
            address,
            bodies,
            stop,
            thread: Some(thread),
        }
    }

    fn bodies(&self) -> Vec<Vec<u8>> {
        self.bodies.lock().expect("ledger").clone()
    }

    /// The `index`-th recorded chat body (bodies are appended before the
    /// response is written, so a completed run has already recorded its own).
    fn body(&self, index: usize) -> Vec<u8> {
        let bodies = self.bodies();
        assert!(
            bodies.len() > index,
            "expected at least {} chat requests, recorded {}",
            index + 1,
            bodies.len()
        );
        bodies[index].clone()
    }
}

impl Drop for CompatProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn session_context_of(body: &[u8]) -> String {
    let body: serde_json::Value = serde_json::from_slice(body).expect("chat body JSON");
    let messages = body["messages"].as_array().expect("messages");
    messages
        .iter()
        .find_map(|message| {
            let blocks = message["content"].as_array()?;
            let text = blocks.first()?["text"].as_str()?;
            text.starts_with("[DAEMON-BOUND SESSION CONTEXT]")
                .then(|| text.to_owned())
        })
        .expect("daemon-bound session context message")
}

fn normalize_body(body: &[u8], workspace: &Path) -> String {
    let text = String::from_utf8(body.to_vec()).expect("UTF-8 chat body");
    let workspace_text = workspace.to_str().expect("UTF-8 workspace path");
    let escaped = serde_json::to_string(workspace_text).expect("JSON string");
    normalize_identities(&text.replace(escaped.trim_matches('"'), "<CWD>"))
}

fn proxy_run(profile: &TestProfile, cwd: &Path, extra: &[&str], prompt: &str) -> Output {
    let mut command = profile.proxy_command();
    command.current_dir(cwd).args([
        "run",
        "--provider",
        PROXY_PROVIDER,
        "--model",
        PROXY_MODEL,
        "--jsonl",
    ]);
    command.args(extra).args(["-p", prompt]);
    let output = output_with_boot_retry(&mut command);
    assert_success(&output, "proxy run");
    output
}

// ---------------------------------------------------------------------------
// Pins: provider request body
// ---------------------------------------------------------------------------

/// MUTATION CHECK: let budget configuration, a warm second request, or the
/// projection short-cut alter a single byte of the outgoing chat request, or
/// let the journal differ beyond the declared budget. Expected RUNTIME
/// failure: the recorded bodies stop being byte-identical, or the no-budget
/// body drifts from its golden.
#[test]
fn provider_request_body_is_budget_independent_and_matches_the_golden_ledger() {
    let proxy = CompatProxy::spawn();
    let profile = TestProfile::new();
    profile.write_custom_provider(&proxy.origin);
    let prompt = "byte-identical request";

    let no_budget = proxy_run(&profile, &profile.workspace, &[], prompt);
    let token_budget = proxy_run(
        &profile,
        &profile.workspace,
        &["--max-tokens", "1000000"],
        prompt,
    );
    let time_budget = proxy_run(&profile, &profile.workspace, &["--max-time", "10m"], prompt);
    let warm_repeat = proxy_run(&profile, &profile.workspace, &[], prompt);
    assert_eq!(proxy.bodies().len(), 4, "one chat request per run");

    let bodies = proxy.bodies();
    let normalized = bodies
        .iter()
        .map(|body| normalize_body(body, &profile.workspace))
        .collect::<Vec<_>>();
    assert_eq!(
        normalized[0], normalized[1],
        "token budget must not change the request"
    );
    assert_eq!(
        normalized[0], normalized[2],
        "time budget must not change the request"
    );
    assert_eq!(
        normalized[0], normalized[3],
        "the warm second request equals the cold one"
    );
    let body: serde_json::Value = serde_json::from_slice(&bodies[0]).expect("chat body JSON");
    assert_eq!(body["model"], PROXY_MODEL);
    assert_eq!(body["stream"], true);
    assert!(
        body["tools"]
            .as_array()
            .is_some_and(|tools| tools.len() > 10)
    );
    assert_golden(
        "provider_request_no_budget.json",
        &format!("{}\n", normalized[0]),
    );

    // The journals differ only by the declared budget; the footprint, cache
    // attempt, and usage projections must be identical with or without one.
    let journal = |output: &Output| {
        let (_, envelopes) = accepted_and_envelopes(&output.stdout);
        let mut text = String::new();
        for mut envelope in envelopes {
            if envelope["payload"]["type"] == "headless_run_configured" {
                envelope["payload"]
                    .as_object_mut()
                    .expect("configured payload")
                    .remove("budget");
            }
            text.push_str(&serde_json::to_string(&envelope).expect("envelope JSON"));
            text.push('\n');
        }
        normalize_jsonl(text.as_bytes(), &profile.workspace, &[])
    };
    let reference = journal(&no_budget);
    assert_eq!(
        journal(&token_budget),
        reference,
        "token budget journal parity"
    );
    assert_eq!(
        journal(&time_budget),
        reference,
        "time budget journal parity"
    );
    assert_eq!(
        journal(&warm_repeat),
        reference,
        "warm repeat journal parity"
    );
    let configured = |output: &Output| {
        let (_, envelopes) = accepted_and_envelopes(&output.stdout);
        envelopes
            .into_iter()
            .find(|envelope| envelope["payload"]["type"] == "headless_run_configured")
            .expect("configured fact")["payload"]
            .clone()
    };
    assert!(configured(&no_budget).get("budget").is_none_or(|budget| {
        budget
            .as_object()
            .is_none_or(|budget| budget.values().all(serde_json::Value::is_null))
    }));
    assert_eq!(configured(&token_budget)["budget"]["max_tokens"], 1_000_000);
    assert_eq!(configured(&time_budget)["budget"]["max_time_ms"], 600_000);
}

// ---------------------------------------------------------------------------
// Pins: project instructions and hooks across runs on one resident daemon
// ---------------------------------------------------------------------------

/// MUTATION CHECK: cache the instruction snapshot across runs without
/// invalidation, key it on the wrong directory, stop walking ancestors, or
/// keep a removed file. Expected RUNTIME failure: a later request carries
/// stale, missing, or foreign instruction bytes.
#[test]
fn resident_daemon_rediscovers_project_instructions_across_runs_and_cwds() {
    let proxy = CompatProxy::spawn();
    let profile = TestProfile::new();
    profile.write_custom_provider(&proxy.origin);
    let deep = profile.workspace.join("a").join("b").join("c");
    std::fs::create_dir_all(&deep).expect("deep cwd");
    let deep = deep.canonicalize().expect("canonical deep cwd");
    let root_agents = profile.workspace.join("AGENTS.md");
    let nearest_haider = profile.workspace.join("a").join("b").join("HAIDER.md");
    let other = profile.sibling_workspace("other-workspace");
    std::fs::write(other.join("AGENTS.md"), "other-delta\n").expect("other instructions");

    std::fs::write(&root_agents, "policy-alpha\n").expect("root instructions");
    proxy_run(&profile, &deep, &[], "instructions one");
    let first = session_context_of(&proxy.body(0));
    assert!(first.contains(&format!("Canonical workspace: {}", deep.display())));
    assert!(first.contains(&format!(
        "Project instructions ({}):",
        root_agents.display()
    )));
    assert!(first.contains("policy-alpha"));

    // Same byte length, different bytes: a size-only stamp must not hide it.
    std::fs::write(&root_agents, "policy-bravo\n").expect("edit root instructions");
    proxy_run(&profile, &deep, &[], "instructions two");
    let second = session_context_of(&proxy.body(1));
    assert!(second.contains("policy-bravo"));
    assert!(!second.contains("policy-alpha"));

    std::fs::write(&nearest_haider, "nearest-charlie\n").expect("nearest instructions");
    proxy_run(&profile, &deep, &[], "instructions three");
    let third = session_context_of(&proxy.body(2));
    assert!(third.contains("policy-bravo"));
    assert!(third.contains("nearest-charlie"));
    assert!(
        third.find("policy-bravo") < third.find("nearest-charlie"),
        "the nearest file composes last"
    );

    proxy_run(&profile, &other, &[], "instructions four");
    let fourth = session_context_of(&proxy.body(3));
    assert!(fourth.contains("other-delta"));
    assert!(!fourth.contains("policy-bravo"));
    assert!(!fourth.contains("nearest-charlie"));

    std::fs::remove_file(&root_agents).expect("remove root instructions");
    std::fs::remove_file(&nearest_haider).expect("remove nearest instructions");
    proxy_run(&profile, &deep, &[], "instructions five");
    let fifth = session_context_of(&proxy.body(4));
    assert!(!fifth.contains("Project instructions ("));

    proxy_run(&profile, &other, &[], "instructions six");
    let sixth = session_context_of(&proxy.body(5));
    assert!(
        sixth.contains("other-delta"),
        "the other workspace is unaffected"
    );
}
