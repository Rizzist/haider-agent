#![allow(clippy::expect_used)]
//! Real CLI/RPC retraction pins against an OpenAI-compatible proxy whose
//! first semantic response byte is released by the test, never a sleep.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_rpc::{AttachMode, CommandId, RequestBody, ResponseBody};
use haider_store::{Cas as _, FileCas};
use serde_json::{Value, json};

const PROVIDER: &str = "retract-proxy";
const MODEL: &str = "retract-model";
const PROMPT: &str = "escretract original prompt to edit";
const FOLLOW_UP: &str = "escretract replacement prompt";
const PARTIAL: &str = "escretract visible partial reply";
const NOTE: &[u8] = b"escretract attachment retained for editing\nsecond line\n";
const RUN_BUDGET: Duration = Duration::from_secs(30);
// The longest control command permits two connections (one lost-receipt
// redial), each with startup 30s + handshake 10s + four request budgets of
// 60s (attach, observe, retract, fallback cancel): 2 * (30 + 10 + 4*60) = 560s.
// This also covers --timeout 30s plus the ordinary 2s terminal grace.
const PROCESS_DEADLINE: Duration = Duration::from_secs(560);
// First-request admission is bounded by cold-start 30s + run budget 30s.
const ADMISSION_DEADLINE: Duration = Duration::from_secs(60);

struct Profile {
    _root: tempfile::TempDir,
    profile: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
}

impl Profile {
    fn new(proxy: &Proxy) -> Self {
        assert_eq!(
            std::env::var("HAIDER_TEST_SIBLINGS_PREBUILT").as_deref(),
            Ok("1"),
            "prebuild haiderd and set HAIDER_TEST_SIBLINGS_PREBUILT=1"
        );
        let daemon = PathBuf::from(env!("CARGO_BIN_EXE_haider"))
            .with_file_name(format!("haiderd{}", std::env::consts::EXE_SUFFIX));
        assert!(
            std::fs::metadata(daemon).expect("prebuilt haiderd").len() > 10 * 1024 * 1024,
            "a real daemon binary must exceed 10 MiB"
        );
        let root = tempfile::tempdir().expect("retraction profile");
        let profile = root.path().join("profile");
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        for path in [&profile, &home, &workspace] {
            std::fs::create_dir_all(path).expect("profile directories");
        }
        std::fs::write(
            profile.join("providers.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": [{
                    "provider_id": PROVIDER,
                    "display_name": "Retraction test proxy",
                    "api_family": "openai_chat_completions",
                    "base_url": proxy.origin,
                    "enabled": true,
                    "auth_requirement": "none",
                    "configured_models": [MODEL],
                    "default_model": MODEL,
                    "provenance": "custom"
                }],
                "fallback_chain": []
            }))
            .expect("provider registry JSON"),
        )
        .expect("provider registry");
        Self {
            _root: root,
            profile,
            home,
            workspace: workspace.canonicalize().expect("canonical workspace"),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haider"));
        command
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("HAIDER_PROFILE_DIR", &self.profile)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env("HAIDER_TEST_DEVICE_NAME", "test-mac")
            .env("HAIDER_NO_UPDATE_CHECK", "1")
            .env("HAIDER_AUTO_HERMETIC", "0")
            .env("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "120000")
            .env_remove("HAIDER_MODEL")
            .env_remove("HAIDER_RUNTIME_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("HAIDER_TEST_FAKE_PROVIDER")
            .stdin(Stdio::null());
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        Running::spawn(self.command().args(args)).finish()
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
        .expect("resolve fixture profile")
    }

    fn start_run(&self, attachment: bool) -> Running {
        // Boot before acceptance so a transient spawn failure cannot strand a
        // proxy gate. Retry only the existing transient unavailable exit.
        let boot = self.output(&["status", "--json"]);
        if boot.status.code() == Some(69) {
            assert_success(&self.output(&["status", "--json"]));
        } else {
            assert_success(&boot);
        }
        let mut command = self.command();
        command.args([
            "run",
            "--provider",
            PROVIDER,
            "--model",
            MODEL,
            "--jsonl",
            "--timeout",
            "30s",
        ]);
        if attachment {
            std::fs::write(self.workspace.join("note.txt"), NOTE).expect("attachment fixture");
            command.args(["--attach", "note.txt"]);
        }
        command.args(["-p", PROMPT]);
        Running::spawn(&mut command)
    }

    fn retract(&self, session_id: &str) -> Value {
        let output = self.output(&["session", "retract", "--session", session_id, "--json"]);
        assert_success(&output);
        serde_json::from_slice(&output.stdout).expect("retraction JSON")
    }

    fn replay(&self, run_id: &str) -> Value {
        let output = self.output(&["run", "--replay", run_id]);
        assert_success(&output);
        serde_json::from_slice(&output.stdout).expect("replay document")
    }

    fn transcript(&self, session_id: &str) -> String {
        let output = self.output(&["export", session_id, "--format", "markdown"]);
        assert_success(&output);
        String::from_utf8(output.stdout).expect("transcript UTF-8")
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = self.output(&["daemon", "stop", "--json", "--timeout", "5s"]);
    }
}

/// Both pipes drain while commands run; a panic kills and reaps this exact
/// child. The real run client's keepalive remains active while proxy-gated.
struct Running {
    child: Option<Child>,
    stdout: Option<thread::JoinHandle<Vec<u8>>>,
    stderr: Option<thread::JoinHandle<Vec<u8>>>,
    lines: mpsc::Receiver<Value>,
}

impl Running {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("CLI starts");
        let stdout = child.stdout.take().expect("stdout pipe");
        let mut stderr = child.stderr.take().expect("stderr pipe");
        let (sender, lines) = mpsc::channel();
        Self {
            child: Some(child),
            stdout: Some(thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut output = Vec::new();
                loop {
                    let mut line = Vec::new();
                    if reader.read_until(b'\n', &mut line).expect("read stdout") == 0 {
                        return output;
                    }
                    if let Ok(value) = serde_json::from_slice(&line) {
                        let _ = sender.send(value);
                    }
                    output.extend(line);
                }
            })),
            stderr: Some(thread::spawn(move || {
                let mut bytes = Vec::new();
                stderr.read_to_end(&mut bytes).expect("read stderr");
                bytes
            })),
            lines,
        }
    }

    fn until(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + ADMISSION_DEADLINE;
        loop {
            let value = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("live JSONL reaches the controlled boundary");
            if predicate(&value) {
                return value;
            }
        }
    }

    fn finish(mut self) -> Output {
        let started = Instant::now();
        let status = loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("child")
                .try_wait()
                .expect("poll CLI")
            {
                break status;
            }
            assert!(
                started.elapsed() < PROCESS_DEADLINE,
                "CLI deadline exceeded"
            );
            thread::sleep(Duration::from_millis(10));
        };
        self.child.take();
        Output {
            status,
            stdout: self
                .stdout
                .take()
                .expect("stdout thread")
                .join()
                .expect("stdout joined"),
            stderr: self
                .stderr
                .take()
                .expect("stderr thread")
                .join()
                .expect("stderr joined"),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "exit {:?}; stdout {}; stderr {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

enum Release {
    Content,
    Finish,
}

struct Proxy {
    origin: String,
    address: SocketAddr,
    bodies: Arc<Mutex<Vec<Value>>>,
    ready: mpsc::Receiver<()>,
    release: mpsc::Sender<Release>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Proxy {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("proxy binds");
        let address = listener.local_addr().expect("proxy address");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let ledger = Arc::clone(&bodies);
        let stopping = Arc::clone(&stop);
        let (ready_tx, ready) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut first_gate = Some(release_rx);
            let mut handlers = Vec::new();
            for incoming in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = incoming.expect("proxy accepts");
                let (request, body) = read_request(&mut stream);
                if request.starts_with("POST ") && request.contains("/chat/completions") {
                    ledger
                        .lock()
                        .expect("proxy ledger")
                        .push(serde_json::from_slice(&body).expect("provider request JSON"));
                    let gate = first_gate.take();
                    let ready = ready_tx.clone();
                    handlers.push(thread::spawn(move || {
                        stream.set_write_timeout(Some(RUN_BUDGET)).expect("proxy write bound");
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
                        // A role-only chunk opens the transport without making
                        // the first response delta observable to the user.
                        write_chunk(&mut stream, json!({"role":"assistant"}), Value::Null);
                        if let Some(gate) = gate {
                            ready.send(()).expect("announce first request");
                            while let Ok(release) = gate.recv_timeout(RUN_BUDGET) {
                                match release {
                                    Release::Content => write_chunk(&mut stream, json!({"content": PARTIAL}), Value::Null),
                                    Release::Finish => break,
                                }
                            }
                        } else {
                            write_chunk(&mut stream, json!({"content":"replacement reply"}), Value::Null);
                        }
                        write_chunk(&mut stream, json!({}), json!("stop"));
                        let _ = stream.write_all(b"data: [DONE]\n\n");
                        let _ = stream.flush();
                    }));
                } else {
                    let catalog =
                        json!({"object":"list","data":[{"id":MODEL,"object":"model"}]}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{catalog}", catalog.len()).expect("proxy catalog");
                }
            }
            for handler in handlers {
                handler.join().expect("proxy handler joined");
            }
        });
        Self {
            origin: format!("http://{address}/v1"),
            address,
            bodies,
            ready,
            release,
            stop,
            thread: Some(thread),
        }
    }

    fn first_request(&self) {
        self.ready
            .recv_timeout(ADMISSION_DEADLINE)
            .expect("provider request opened before first delta");
    }

    fn release_content(&self) {
        self.release
            .send(Release::Content)
            .expect("release first semantic byte");
    }

    fn finish(&self) {
        let _ = self.release.send(Release::Finish);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.finish();
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("proxy joined");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    stream
        .set_read_timeout(Some(RUN_BUDGET))
        .expect("bounded HTTP request read");
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    reader.read_line(&mut request).expect("HTTP request line");
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).expect("HTTP header") > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().expect("HTTP content length");
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).expect("HTTP request body");
    (request, body)
}

fn write_chunk(stream: &mut TcpStream, delta: Value, finish_reason: Value) {
    let chunk =
        json!({"id":"chatcmpl-retract","object":"chat.completion.chunk","created":1,"model":MODEL,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}]})
        .to_string();
    // The request can be cancelled before the gated byte is released. The
    // server's broken pipe is the expected transport cancellation outcome.
    let _ = write!(stream, "data: {chunk}\n\n");
    let _ = stream.flush();
}

fn live_envelopes(output: &Output) -> Vec<Value> {
    let text = std::str::from_utf8(&output.stdout).expect("JSONL UTF-8");
    assert!(text.ends_with('\n'));
    let mut records = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("JSONL object"));
    let accepted = records.next().expect("acceptance");
    assert_eq!(accepted["event"], "accepted");
    let envelopes = records.collect::<Vec<_>>();
    assert_eq!(accepted["head_seq"], envelopes[0]["seq"]);
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[1]["seq"].as_u64() == pair[0]["seq"].as_u64().map(|seq| seq + 1))
    );
    assert_eq!(
        envelopes
            .iter()
            .filter(|event| event["payload"].get("terminal_kind").is_some())
            .count(),
        1
    );
    envelopes
}

fn assert_replay_parity(profile: &Profile, run_id: &str, live: &[Value]) -> Value {
    let replay = profile.replay(run_id);
    assert_eq!(replay["provider_requests"], 0);
    assert_eq!(replay["integrity"]["exactly_one_typed_terminal"], true);
    let expected = live
        .iter()
        .filter(|event| event["run_id"] == run_id)
        .cloned()
        .collect::<Vec<_>>();
    // No durable field is normalized away for live/replay comparison.
    assert_eq!(replay["events"], json!(expected));
    replay
}

fn with_control(
    profile: &Profile,
    session_id: &str,
    body: impl FnOnce(u64) -> RequestBody,
) -> ResponseBody {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("RPC runtime");
    runtime.block_on(async {
        let connected = haider_client::connect(
            &profile.resolved().endpoint_path,
            haider_client::ClientConfig::default(),
        )
        .await
        .expect("RPC connects");
        let response = connected
            .client
            .request(RequestBody::SessionAttach {
                session_id: SessionId::new(session_id),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            })
            .await
            .expect("control attaches");
        let ResponseBody::SessionAttach { attach_state, .. } = response else {
            panic!("unexpected attach response {response:?}")
        };
        // This long-lived client services keepalive while request completion
        // waits for the daemon's durable admission/cancellation transaction.
        connected
            .client
            .request(body(attach_state.worker_generation))
            .await
            .expect("RPC response")
    })
}

#[test]
fn retract_before_first_delta_restores_attachments_hides_history_and_replays_exactly() {
    let proxy = Proxy::spawn();
    let profile = Profile::new(&proxy);
    let run = profile.start_run(true);
    let accepted = run.until(|event| event["event"] == "accepted");
    let session_id = accepted["session_id"].as_str().expect("session id");
    proxy.first_request();
    let receipt = profile.retract(session_id);
    assert_eq!(receipt["schema"], "haider.session_retract.v1");
    assert_eq!(receipt["status"], "retracted");
    assert_eq!(receipt["text"], PROMPT);
    // Retract wins this ordering: the proxy cannot release content until
    // after the durable retraction receipt has been returned to the client.
    proxy.release_content();
    proxy.finish();
    let live = live_envelopes(&run.finish());
    let prompt = live
        .iter()
        .find(|event| event["payload"]["type"] == "user_message")
        .expect("accepted prompt remains in append-only journal");
    let retracted = live
        .iter()
        .find(|event| event["payload"]["type"] == "prompt_retracted")
        .expect("durable retraction fact");
    let terminal = live
        .iter()
        .find(|event| event["payload"].get("terminal_kind").is_some())
        .expect("terminal");
    assert_eq!(retracted["payload"]["prompt_seq"], prompt["seq"]);
    assert_eq!(retracted["payload"]["text"], PROMPT);
    assert_eq!(
        retracted["payload"]["attachments"],
        prompt["payload"]["attachments"]
    );
    assert_eq!(receipt["prompt_seq"], prompt["seq"]);
    assert_eq!(receipt["retracted_seq"], retracted["seq"]);
    assert_eq!(receipt["attachments"], retracted["payload"]["attachments"]);
    assert_eq!(terminal["payload"]["state"], "cancelled");
    assert_eq!(terminal["payload"]["terminal_kind"], "cancellation");
    assert_eq!(terminal["payload"]["reason"], "retracted");
    assert!(
        !live.iter().any(|event| event["payload"]["type"] == "item"
            && event["payload"]["delta"]["delta"] == "text")
    );

    let run_id = receipt["run_id"].as_str().expect("retracted run id");
    let replay = assert_replay_parity(&profile, run_id, &live);
    assert_eq!(replay["response"], Value::Null);
    assert!(!profile.transcript(session_id).contains(PROMPT));
    for format in ["json", "pipe"] {
        let exported = profile.output(&["export", session_id, "--format", format]);
        assert_success(&exported);
        assert!(
            !String::from_utf8_lossy(&exported.stdout).contains(PROMPT),
            "{format} transcript must hide the retracted prompt"
        );
    }
    let attachment = &retracted["payload"]["attachments"][0];
    assert_eq!(attachment["name"], "note.txt");
    let artifact = ArtifactRef::new(
        attachment["artifact"]
            .as_str()
            .expect("attachment CAS address"),
    );
    let cas = FileCas::open(&profile.profile).expect("profile CAS opens");
    std::fs::remove_file(profile.workspace.join("note.txt")).expect("remove original input file");
    assert_eq!(
        cas.get(&artifact)
            .expect("composer can restore retained attachment"),
        NOTE
    );
    assert_retraction_golden(&retracted["payload"], &terminal["payload"]);

    let submit = with_control(&profile, session_id, |worker_generation| {
        RequestBody::TurnSubmit {
            command_id: CommandId::new("retract-followup"),
            session_id: SessionId::new(session_id),
            worker_generation,
            text: FOLLOW_UP.into(),
            attachments: Vec::new(),
            mode: Default::default(),
        }
    });
    assert!(
        matches!(submit, ResponseBody::TurnSubmit { .. }),
        "follow-up accepted: {submit:?}"
    );
    let deadline = Instant::now() + RUN_BUDGET;
    let next_body = loop {
        if let Some(body) = proxy.bodies.lock().expect("proxy ledger").get(1).cloned() {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "replacement request never reached provider"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let messages = next_body["messages"].to_string();
    assert!(messages.contains(FOLLOW_UP));
    assert!(
        !messages.contains(PROMPT),
        "retracted prompt leaked into next provider history"
    );
    assert!(
        !messages.contains("escretract attachment retained"),
        "retracted attachment leaked into next provider history"
    );
    assert_eq!(
        proxy.bodies.lock().expect("proxy ledger").len(),
        2,
        "one admitted physical request per turn; cancellation never retries"
    );
}

#[test]
fn first_delta_before_retract_is_typed_too_late_and_cli_keeps_prompt_and_partial_reply() {
    let proxy = Proxy::spawn();
    let profile = Profile::new(&proxy);
    let run = profile.start_run(false);
    let accepted = run.until(|event| event["event"] == "accepted");
    let session_id = accepted["session_id"].as_str().expect("session id");
    proxy.first_request();
    proxy.release_content();
    let first = run.until(|event| {
        event["payload"]["type"] == "item"
            && event["payload"]["event"] == "delta"
            && event.to_string().contains(PARTIAL)
    });
    let run_id = first["run_id"].as_str().expect("response run id");
    let response = with_control(&profile, session_id, |worker_generation| {
        RequestBody::TurnRetract {
            command_id: CommandId::new("too-late-retract"),
            session_id: SessionId::new(session_id),
            worker_generation,
            run_id: RunId::new(run_id),
        }
    });
    assert!(
        matches!(response, ResponseBody::Error { ref code, .. } if code == "too_late"),
        "typed refusal: {response:?}"
    );
    let receipt = profile.retract(session_id);
    assert_eq!(receipt["status"], "cancelled");
    assert_eq!(receipt["reason"], "too_late");
    assert_eq!(receipt["run_id"], run_id);
    assert!(receipt.get("text").is_none());
    assert!(receipt.get("attachments").is_none());
    proxy.finish();
    let live = live_envelopes(&run.finish());
    assert!(
        !live
            .iter()
            .any(|event| event["payload"]["type"] == "prompt_retracted")
    );
    assert_eq!(
        live.iter()
            .filter(|event| event["payload"]["state"] == "cancelling")
            .count(),
        1,
        "fallback uses one durable turn.cancel"
    );
    let terminal = live
        .iter()
        .find(|event| event["payload"].get("terminal_kind").is_some())
        .expect("terminal");
    assert_eq!(terminal["payload"]["state"], "cancelled");
    assert_ne!(terminal["payload"]["reason"], "retracted");
    assert_replay_parity(&profile, run_id, &live);
    let transcript = profile.transcript(session_id);
    assert!(transcript.contains(PROMPT));
    assert!(transcript.contains(PARTIAL));
    assert_eq!(proxy.bodies.lock().expect("proxy ledger").len(), 1);
}

fn assert_retraction_golden(fact: &Value, terminal: &Value) {
    // Pin the complete new fact and terminal payloads. Replace only variable
    // coordinates; unrelated per-turn telemetry is checked by replay parity.
    let mut fact = fact.clone();
    fact["prompt_seq"] = json!("<PROMPT_SEQ>");
    fact["prompt_node_id"] = json!("<PROMPT_NODE_ID>");
    for attachment in fact["attachments"].as_array_mut().expect("attachments") {
        attachment["artifact"] = json!("<ATTACHMENT_ARTIFACT>");
    }
    let actual = format!(
        "{}\n{}\n",
        serde_json::to_string(&fact).expect("fact JSON"),
        serde_json::to_string(terminal).expect("terminal JSON")
    );
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/escretract_payloads.jsonl");
    if std::env::var("UPDATE_FIXTURES").as_deref() == Ok("1") {
        std::fs::write(path, actual).expect("update retraction fixture");
    } else {
        assert_eq!(
            std::fs::read_to_string(path).expect("retraction fixture"),
            actual,
            "UPDATE_FIXTURES=1 regenerates this golden from the real CLI stream"
        );
    }
}
