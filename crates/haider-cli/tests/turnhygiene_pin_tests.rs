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

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PREBUILT_SIBLINGS_ENV: &str = "HAIDER_TEST_SIBLINGS_PREBUILT";
const FIXTURE_DIR: &str = "tests/fixtures/turnhygiene";
/// Long enough that every run of one test attaches to the daemon the first
/// run spawned; `TestProfile::drop` stops it explicitly.
const RESIDENT_IDLE_TTL_MS: &str = "120000";
const PROCESS_DEADLINE: Duration = Duration::from_secs(60);
const FILE_WAIT_DEADLINE: Duration = Duration::from_secs(20);
const PROXY_PROVIDER: &str = "pinproxy";
const PROXY_MODEL: &str = "pin-model";

const TEXT_TURN_SCRIPT: &str = r#"[{"step":"emit_text","text":"golden text"},{"step":"emit_usage","usage":{"input":12,"output":3,"reasoning":0,"cached":0,"source":"locally_exact"}},{"step":"finish","reason":"end_turn"}]"#;

#[cfg(unix)]
const GOLDEN_EXEC_COMMAND: &str = "printf golden; exit 3";
#[cfg(windows)]
const GOLDEN_EXEC_COMMAND: &str = "[Console]::Out.Write('golden'); exit 3";

fn tool_turn_script() -> String {
    serde_json::json!([
        {
            "step": "emit_tool_call",
            "call_id": "golden-exec",
            "name": "process_exec",
            "args": {"command": GOLDEN_EXEC_COMMAND}
        },
        {"step": "finish", "reason": "tool_use"},
        {"step": "expect_tool_result", "call_id": "golden-exec"},
        {"step": "emit_text", "text": "golden tool"},
        {
            "step": "emit_usage",
            "usage": {"input": 40, "output": 5, "reasoning": 0, "cached": 0, "source": "locally_exact"}
        },
        {"step": "finish", "reason": "end_turn"}
    ])
    .to_string()
}

fn text_segments(count: usize) -> String {
    let steps = (0..count)
        .flat_map(|index| {
            [
                serde_json::json!({"step": "emit_text", "text": format!("segment-{index}")}),
                serde_json::json!({"step": "finish", "reason": "end_turn"}),
            ]
        })
        .collect::<Vec<_>>();
    serde_json::Value::Array(steps).to_string()
}

// ---------------------------------------------------------------------------
// Process harness
// ---------------------------------------------------------------------------

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

fn run_id_of(envelopes: &[serde_json::Value]) -> String {
    envelopes
        .iter()
        .find_map(|envelope| envelope["run_id"].as_str().map(str::to_owned))
        .expect("run-scoped envelope")
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

fn wait_for_file_lines(path: &Path, count: usize) -> Vec<String> {
    let started = Instant::now();
    loop {
        let lines = std::fs::read_to_string(path)
            .map(|text| {
                text.lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if lines.len() >= count {
            return lines;
        }
        assert!(
            started.elapsed() < FILE_WAIT_DEADLINE,
            "{} never reached {count} lines (has {})",
            path.display(),
            lines.len()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Pins: `haider run --jsonl` goldens
// ---------------------------------------------------------------------------

/// MUTATION CHECK: drop, reorder, rename, or retype any envelope or payload
/// field on the one-request warm path (context footprint, cache attempt,
/// usage scope, terminal augmentation). Expected RUNTIME failure: the
/// normalized stream differs from the golden line by line.
#[test]
fn run_jsonl_text_turn_matches_the_normalized_golden() {
    let profile = TestProfile::new();
    let mut command = profile.command();
    command
        .args([
            "run",
            "--provider",
            "fake",
            "--jsonl",
            "-p",
            "golden prompt",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", TEXT_TURN_SCRIPT)
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0");
    let output = output_with_boot_retry(&mut command);
    assert_success(&output, "text turn");
    assert!(output.stderr.is_empty(), "JSONL keeps stderr silent");
    let (_, envelopes) = accepted_and_envelopes(&output.stdout);
    assert_eq!(
        envelopes.last().expect("terminal")["payload"]["terminal_kind"],
        "success"
    );
    assert!(envelopes.iter().any(|envelope| {
        envelope["payload"]["type"] == "item"
            && envelope["payload"]["event"] == "completed"
            && envelope["payload"]["item"]["item"] == "agent_message"
            && envelope["payload"]["item"]["text"] == "golden text"
    }));
    // The estimated counters are normalized in the golden; pin their
    // invariants directly so the placeholder never hides a zero or a mismatch.
    let footprints = envelopes
        .iter()
        .filter(|envelope| envelope["payload"]["item"]["kind"] == "context_footprint_v1")
        .map(|envelope| envelope["payload"]["item"]["data"]["input_tokens"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(
        footprints.len(),
        4,
        "one footprint pair before the request and one after the response"
    );
    assert!(
        footprints
            .iter()
            .all(|tokens| tokens.is_some_and(|tokens| tokens > 0))
    );
    assert_eq!(footprints[0], footprints[1]);
    assert_eq!(footprints[2], footprints[3]);

    assert_golden(
        "run_jsonl_text_turn.jsonl",
        &normalize_jsonl(&output.stdout, &profile.workspace, &[]),
    );
}

/// MUTATION CHECK: change how a tool round is journaled — effect phases,
/// command-output deltas, the process signal, the bounded tool result, the
/// node commit, or the second-request cache/footprint items. Expected RUNTIME
/// failure: the normalized stream differs from the golden line by line.
#[test]
fn run_jsonl_tool_turn_matches_the_normalized_golden() {
    let profile = TestProfile::new();
    let mut command = profile.command();
    command
        .args([
            "run",
            "--provider",
            "fake",
            "--allow-exec",
            "--jsonl",
            "-p",
            "golden tool prompt",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", tool_turn_script())
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0");
    let output = output_with_boot_retry(&mut command);
    assert_success(&output, "tool turn");
    let (_, envelopes) = accepted_and_envelopes(&output.stdout);
    let result = envelopes
        .iter()
        .find(|envelope| envelope["payload"]["type"] == "tool_result")
        .expect("tool result");
    assert_eq!(result["payload"]["call_id"], "golden-exec");
    let preview: serde_json::Value = serde_json::from_str(
        result["payload"]["result"]["preview"]
            .as_str()
            .expect("preview"),
    )
    .expect("preview JSON");
    assert_eq!(preview["exit_code"], 3);
    assert_eq!(preview["output"], "golden");
    assert_eq!(preview["output_bytes"], 6);
    assert_eq!(preview["status"], "failed");
    assert_eq!(
        envelopes
            .iter()
            .filter(|envelope| envelope["payload"]["type"] == "process_signal_recorded")
            .count(),
        1,
        "one process signal per foreground tool call"
    );
    assert!(envelopes.iter().any(|envelope| {
        envelope["payload"]["type"] == "effect"
            && envelope["payload"]["phase"] == "outcome"
            && envelope["payload"]["workspace_mutation"]["mutation_digest"].is_string()
    }));

    assert_golden(
        "run_jsonl_tool_turn.jsonl",
        &normalize_jsonl(
            &output.stdout,
            &profile.workspace,
            &[(GOLDEN_EXEC_COMMAND, "<CMD>")],
        ),
    );
}

/// MUTATION CHECK: let the live stream and the durable journal diverge across
/// a tool round (a skipped append, a reordered batch, a synthesized envelope,
/// or a second terminal). Expected RUNTIME failure: the replay document's
/// events are not exactly the run-scoped live envelopes.
#[test]
fn replay_of_a_tool_call_turn_equals_the_live_run_scoped_jsonl() {
    let profile = TestProfile::new();
    let mut live = profile.command();
    live.args([
        "run",
        "--provider",
        "fake",
        "--allow-exec",
        "--jsonl",
        "-p",
        "replay parity",
    ])
    .env("HAIDER_TEST_FAKE_PROVIDER", tool_turn_script());
    let live = output_with_boot_retry(&mut live);
    assert_success(&live, "live tool turn");
    let (accepted, envelopes) = accepted_and_envelopes(&live.stdout);
    let run_id = run_id_of(&envelopes);

    let mut replay = profile.command();
    replay.args(["run", "--replay", &run_id]);
    let replay = output_with_boot_retry(&mut replay);
    assert_success(&replay, "replay");
    let replay: serde_json::Value = serde_json::from_slice(&replay.stdout).expect("replay JSON");
    assert_eq!(replay["schema"], "haider.run.replay.v1");
    assert_eq!(replay["provider_requests"], 0);
    assert_eq!(replay["session_id"], accepted["session_id"]);
    assert_eq!(replay["integrity"]["exactly_one_typed_terminal"], true);
    assert_eq!(replay["equivalence"]["tool_trace_matches"], true);
    assert_eq!(replay["response"], "golden tool");

    let expected = envelopes
        .iter()
        .filter(|envelope| envelope["run_id"].as_str() == Some(run_id.as_str()))
        .map(|envelope| {
            let mut envelope = envelope.clone();
            if let Some(payload) = envelope["payload"].as_object_mut() {
                payload.remove("terminal_kind");
                payload.remove("error_code");
            }
            envelope
        })
        .collect::<Vec<_>>();
    assert!(
        expected.len() > 30,
        "a tool round journals a rich run scope"
    );
    assert_eq!(
        replay["events"].as_array().expect("replay events"),
        &expected,
        "durable replay must reproduce the run-scoped live stream exactly"
    );
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

#[cfg(unix)]
fn capture_hook_command(capture: &Path) -> String {
    format!("(cat; printf '\\n') >> '{}'", capture.display())
}

#[cfg(windows)]
fn capture_hook_command(capture: &Path) -> String {
    let capture = capture.display().to_string().replace('\'', "''");
    format!(
        "$i=[Console]::OpenStandardInput();$f=[IO.File]::Open('{capture}',[IO.FileMode]::Append,[IO.FileAccess]::Write,[IO.FileShare]::ReadWrite);$i.CopyTo($f);$f.WriteByte(10);$f.Dispose()"
    )
}

#[cfg(unix)]
const CAPTURE_HOOK_TIMEOUT_MS: u64 = 2_000;
#[cfg(windows)]
const CAPTURE_HOOK_TIMEOUT_MS: u64 = 5_000;

fn write_workspace_hook(workspace: &Path, name: &str, capture: &Path) {
    std::fs::write(
        workspace.join("hooks.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "haider.hooks.v1",
            "hooks": {
                name: {
                    "matcher": {"event": "user_message"},
                    "kind": "exec",
                    "command": capture_hook_command(capture),
                    "timeout_ms": CAPTURE_HOOK_TIMEOUT_MS,
                }
            }
        }))
        .expect("workspace hooks JSON"),
    )
    .expect("write workspace hooks");
}

fn fake_run(profile: &TestProfile, cwd: &Path, script: &str, prompt: &str) -> (String, String) {
    let mut command = profile.command();
    command
        .current_dir(cwd)
        .args(["run", "--provider", "fake", "--jsonl", "-p", prompt])
        .env("HAIDER_TEST_FAKE_PROVIDER", script);
    let output = output_with_boot_retry(&mut command);
    assert_success(&output, "fake run");
    let (accepted, envelopes) = accepted_and_envelopes(&output.stdout);
    (
        accepted["session_id"]
            .as_str()
            .expect("session id")
            .to_owned(),
        run_id_of(&envelopes),
    )
}

/// MUTATION CHECK: snapshot hook discovery once per daemon instead of per
/// run, key the snapshot on the wrong cwd, or drop the hook's JSON payload.
/// Expected RUNTIME failure: the hook installed between runs never fires, a
/// foreign workspace's hook fires, or the captured payload loses its
/// session/run/text fields.
#[test]
fn resident_daemon_discovers_a_hook_installed_between_runs_and_scopes_it_by_cwd() {
    let profile = TestProfile::new();
    std::fs::write(
        profile.profile.join("hooks.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "haider.hooks.v1",
            "policy": "trust_workspace",
            "hooks": {}
        }))
        .expect("profile hooks JSON"),
    )
    .expect("write profile hooks");
    let script = text_segments(4);
    let workspace_a = profile.workspace.clone();
    let workspace_b = profile.sibling_workspace("workspace-b");
    let capture_a = profile.root().join("capture-a.jsonl");
    let capture_b = profile.root().join("capture-b.jsonl");

    let (session_one, run_one) = fake_run(&profile, &workspace_a, &script, "no hook yet");
    assert!(!capture_a.exists(), "no hook exists yet, so nothing fires");

    // Installing a hook replays the retained (undecoded) facts of the hookless
    // run first, then fires for the new run: two captured messages.
    write_workspace_hook(&workspace_a, "capture_a", &capture_a);
    let (session_two, run_two) = fake_run(&profile, &workspace_a, &script, "hook installed");
    let lines = wait_for_file_lines(&capture_a, 2);
    let payloads = lines
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("hook payload JSON"))
        .collect::<Vec<_>>();
    let payload_for = |payloads: &[serde_json::Value], session: &str| {
        payloads
            .iter()
            .find(|payload| payload["session"] == session)
            .cloned()
            .unwrap_or_else(|| panic!("hook payload for {session} among {payloads:?}"))
    };
    let replayed = payload_for(&payloads, &session_one);
    assert_eq!(replayed["event"], "user_message");
    assert_eq!(replayed["run"], run_one);
    assert!(lines.iter().any(|line| line.contains("no hook yet")));
    let fired = payload_for(&payloads, &session_two);
    assert_eq!(fired["event"], "user_message");
    assert_eq!(fired["run"], run_two);
    assert_eq!(fired["mode"], "queue");
    assert!(lines.iter().any(|line| line.contains("hook installed")));

    write_workspace_hook(&workspace_b, "capture_b", &capture_b);
    let (session_three, run_three) = fake_run(&profile, &workspace_b, &script, "other workspace");
    let lines_b = wait_for_file_lines(&capture_b, 1);
    let payload_b: serde_json::Value =
        serde_json::from_str(&lines_b[0]).expect("hook payload JSON");
    assert_eq!(payload_b["session"], session_three);
    assert_eq!(payload_b["run"], run_three);

    let (session_four, run_four) = fake_run(&profile, &workspace_a, &script, "back in a");
    let lines = wait_for_file_lines(&capture_a, 3);
    assert_eq!(
        lines.len(),
        3,
        "workspace A fired exactly for its own three runs"
    );
    let payloads = lines
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("hook payload JSON"))
        .collect::<Vec<_>>();
    let fired = payload_for(&payloads, &session_four);
    assert_eq!(fired["run"], run_four);
    assert!(
        !lines.iter().any(|line| line.contains(&session_three)),
        "workspace B's run never reached workspace A's hook"
    );
    let lines_b = wait_for_file_lines(&capture_b, 1);
    assert_eq!(
        lines_b.len(),
        1,
        "workspace A's runs never reached workspace B's hook"
    );
    assert!(!lines_b.iter().any(|line| line.contains(&session_four)));
}

// ---------------------------------------------------------------------------
// Pins: profile / provider / model resolution
// ---------------------------------------------------------------------------

/// MUTATION CHECK: require a profile default before honouring explicit
/// flags, stop splitting a `provider/model` selector, or stop applying the
/// profile default when flags are absent. Expected RUNTIME failure: a run
/// refuses, or the wire model/provider differs from the selection.
#[test]
fn custom_provider_binds_from_explicit_flags_a_model_selector_and_the_profile_default() {
    let proxy = CompatProxy::spawn();
    let profile = TestProfile::new();
    profile.write_custom_provider(&proxy.origin);
    assert!(!profile.profile.join("config.json").exists());

    let configured_of = |output: &Output| {
        let (_, envelopes) = accepted_and_envelopes(&output.stdout);
        envelopes
            .into_iter()
            .find(|envelope| envelope["payload"]["type"] == "headless_run_configured")
            .expect("configured fact")["payload"]
            .clone()
    };
    let wire_model = |index: usize| {
        let body: serde_json::Value =
            serde_json::from_slice(&proxy.body(index)).expect("chat body JSON");
        body["model"].as_str().expect("wire model").to_owned()
    };

    let explicit = proxy_run(&profile, &profile.workspace, &[], "explicit flags");
    let configured = configured_of(&explicit);
    assert_eq!(configured["provider"], PROXY_PROVIDER);
    assert_eq!(configured["model"], PROXY_MODEL);
    assert_eq!(wire_model(0), PROXY_MODEL);

    let mut selector = profile.proxy_command();
    selector.args([
        "run",
        "--model",
        &format!("{PROXY_PROVIDER}/{PROXY_MODEL}"),
        "--jsonl",
        "-p",
        "selector only",
    ]);
    let selector = output_with_boot_retry(&mut selector);
    assert_success(&selector, "selector run");
    let configured = configured_of(&selector);
    assert_eq!(configured["provider"], PROXY_PROVIDER);
    assert_eq!(configured["model"], PROXY_MODEL);
    assert_eq!(wire_model(1), PROXY_MODEL);

    std::fs::write(
        profile.profile.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "default_model": format!("{PROXY_PROVIDER}/{PROXY_MODEL}")
        }))
        .expect("profile config JSON"),
    )
    .expect("write profile config");
    let mut flagless = profile.proxy_command();
    flagless.args(["run", "--jsonl", "-p", "profile default"]);
    let flagless = output_with_boot_retry(&mut flagless);
    assert_success(&flagless, "flagless run");
    let configured = configured_of(&flagless);
    assert_eq!(configured["provider"], PROXY_PROVIDER);
    assert_eq!(configured["model"], PROXY_MODEL);
    assert_eq!(wire_model(2), PROXY_MODEL);
    assert_eq!(proxy.bodies().len(), 3);
}

// ---------------------------------------------------------------------------
// Pins: stdout delivery latency and unattached journal parity
// ---------------------------------------------------------------------------

struct TimedLine {
    at: Instant,
    unix_ms: u128,
    line: String,
}

/// MUTATION CHECK: hold committed envelopes in the client until the terminal
/// (or a large batch) instead of flushing them as they arrive. Expected
/// RUNTIME failure: the first text delta reaches stdout only together with
/// the second one, so the observed gap collapses below the scripted delay.
#[test]
fn jsonl_envelopes_reach_stdout_before_a_later_provider_delay_elapses() {
    const DELAY_MS: u64 = 1_500;
    let script = serde_json::json!([
        {"step": "emit_text", "text": "first"},
        {"step": "delay", "ms": DELAY_MS},
        {"step": "emit_text", "text": "second"},
        {"step": "finish", "reason": "end_turn"}
    ])
    .to_string();
    let profile = TestProfile::new();
    let mut command = profile.command();
    command
        .args(["run", "--provider", "fake", "--jsonl", "-p", "latency"])
        .env("HAIDER_TEST_FAKE_PROVIDER", script)
        .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("latency child starts");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let reader = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("read JSONL line");
            lines.push(TimedLine {
                at: Instant::now(),
                unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_millis(),
                line,
            });
        }
        lines
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).expect("read stderr");
        bytes
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        assert!(
            started.elapsed() < PROCESS_DEADLINE,
            "latency child deadline"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let lines = reader.join().expect("reader");
    let stderr = stderr_reader.join().expect("stderr reader");
    assert!(
        status.success(),
        "exit {:?}; stderr: {}",
        status.code(),
        String::from_utf8_lossy(&stderr)
    );

    let delta_line = |text: &str| {
        lines
            .iter()
            .find(|timed| {
                serde_json::from_str::<serde_json::Value>(&timed.line).is_ok_and(|value| {
                    value["payload"]["type"] == "item"
                        && value["payload"]["event"] == "delta"
                        && value["payload"]["delta"]["delta"] == "text"
                        && value["payload"]["delta"]["text"] == text
                })
            })
            .unwrap_or_else(|| panic!("text delta {text:?} reached stdout"))
    };
    let first = delta_line("first");
    let second = delta_line("second");
    let gap = second.at.saturating_duration_since(first.at);
    assert!(
        gap >= Duration::from_millis(DELAY_MS / 2),
        "the first delta must reach stdout before the provider delay elapses (gap {gap:?})"
    );
    for timed in &lines {
        let value: serde_json::Value = serde_json::from_str(&timed.line).expect("JSON line");
        let Some(committed) = value["committed_at_ms"].as_u64() else {
            continue;
        };
        let committed = u128::from(committed);
        assert!(
            timed.unix_ms + 1_000 >= committed,
            "an envelope cannot reach stdout before it was committed"
        );
        assert!(
            timed.unix_ms.saturating_sub(committed) < 10_000,
            "envelope committed at {committed} reached stdout at {} (bounded delivery)",
            timed.unix_ms
        );
    }
}

fn wait_for_terminal_status(profile: &TestProfile, run_id: &str) {
    let started = Instant::now();
    loop {
        let mut status = profile.command();
        status.args(["run", "--status", run_id, "--json"]);
        let output = bounded_output(&mut status, PROCESS_DEADLINE);
        assert_success(&output, "run status");
        let status: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("status JSON");
        assert_eq!(status["schema"], "haider.run.status.v1");
        if status["result"]["terminal_seq"].is_u64() {
            let state = &status["result"]["state"];
            assert!(
                state == "done" || state["state"] == "done",
                "detached run ended in {state}"
            );
            return;
        }
        assert!(
            started.elapsed() < PROCESS_DEADLINE,
            "detached run {run_id} never became terminal"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn normalized_replay(profile: &TestProfile, run_id: &str) -> (String, String) {
    let mut replay = profile.command();
    replay.args(["run", "--replay", run_id]);
    let output = output_with_boot_retry(&mut replay);
    assert_success(&output, "replay");
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).expect("replay JSON");
    assert_eq!(document["integrity"]["exactly_one_typed_terminal"], true);
    let mut text = String::new();
    for event in document["events"].as_array().expect("replay events") {
        let mut event = event.clone();
        if event["payload"]["type"] == "headless_run_configured" {
            // Detachment is the one declared difference between the runs.
            event["payload"]
                .as_object_mut()
                .expect("configured payload")
                .remove("detached");
        }
        text.push_str(&serde_json::to_string(&event).expect("event JSON"));
        text.push('\n');
    }
    (
        normalize_jsonl(text.as_bytes(), &profile.workspace, &[]),
        document["response"].as_str().expect("response").to_owned(),
    )
}

/// MUTATION CHECK: journal extra, fewer, or differently shaped envelopes when
/// no client is attached during the turn (a projection or fan-out that only
/// exists for listeners leaking into the durable record). Expected RUNTIME
/// failure: the normalized durable replays of an attached and a detached run
/// of the same script differ.
#[test]
fn detached_run_journals_the_same_envelopes_as_an_attached_run() {
    let profile = TestProfile::new();
    let script = text_segments(2);

    let mut attached = profile.command();
    attached
        .args([
            "run",
            "--provider",
            "fake",
            "--jsonl",
            "-p",
            "journal parity",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", &script);
    let attached = output_with_boot_retry(&mut attached);
    assert_success(&attached, "attached run");
    let (_, envelopes) = accepted_and_envelopes(&attached.stdout);
    let attached_run = run_id_of(&envelopes);

    let mut detached = profile.command();
    detached
        .args([
            "run",
            "--provider",
            "fake",
            "--start",
            "--json",
            "-p",
            "journal parity",
        ])
        .env("HAIDER_TEST_FAKE_PROVIDER", &script);
    let detached = output_with_boot_retry(&mut detached);
    assert_success(&detached, "detached start");
    let started: serde_json::Value = serde_json::from_slice(&detached.stdout).expect("start JSON");
    assert_eq!(started["outcome"], "started");
    let detached_run = started["run_id"]
        .as_str()
        .expect("detached run id")
        .to_owned();
    wait_for_terminal_status(&profile, &detached_run);

    let (attached_replay, attached_response) = normalized_replay(&profile, &attached_run);
    let (detached_replay, detached_response) = normalized_replay(&profile, &detached_run);
    assert_eq!(attached_response, "segment-0");
    assert_eq!(detached_response, "segment-1");
    let strip_response = |text: &str| {
        text.replace("segment-0", "<TEXT>")
            .replace("segment-1", "<TEXT>")
    };
    assert_eq!(
        strip_response(&detached_replay),
        strip_response(&attached_replay),
        "an unattached turn must journal exactly what an attached one does"
    );
}
