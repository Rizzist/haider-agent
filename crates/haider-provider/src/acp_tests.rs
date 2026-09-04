//! ACP protocol-core tests.
//!
//! Every exchange runs against a fixture-driven fake agent over
//! `tokio::io::duplex`: no network, no real Google binary, no real
//! credentials. The one exception is
//! [`spawned_child_round_trips_initialize_and_is_reaped_without_an_orphan`],
//! which spawns a shell script written into a scratch directory by the test.

#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use haider_protocol::provider::{FeatureResolve, FinishReason, StreamEvent};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream};
use tokio::sync::oneshot;

use crate::acp::antigravity::select_oauth_personal_method;
use crate::acp::client::{
    ACP_MAX_PENDING_REQUESTS, ACP_OAUTH_URL_LINE_PREFIX, ACP_OAUTH_URL_REDACTION,
    ACP_STDERR_TAIL_BYTES, ACP_STRIPPED_ENVIRONMENT_NAMES, ACP_STRIPPED_ENVIRONMENT_PREFIXES,
    AcpChildReap, AcpClientHandler, AcpConnection, AcpError, AcpLaunchSpec,
    RefusingAcpClientHandler, StderrRing, acp_child_environment,
};
use crate::acp::codec::{FrameError, LineFramer, encode_frame};
use crate::acp::wire::{
    ACP_PROTOCOL_VERSION, AuthMethod, AuthMethodType, ClientInfo, FsReadTextFileRequest,
    FsReadTextFileResponse, FsWriteTextFileRequest, JsonRpcError, RequestPermissionRequest,
    RequestPermissionResponse, SessionUpdate, StopReason,
};
use crate::acp::{
    ACP_OAUTH_PERSONAL_METHOD_ID, AntigravityAcpProvider, AntigravitySessionConfig,
    GOOGLE_ANTIGRAVITY_PROVIDER_NAME,
};
use crate::{Message, Provider, ProviderErrorKind, ProviderStream, TurnRequest};

/// Duplex capacity for the fixture transports: large enough that a fake agent
/// script never blocks on a write while the client is mid-handshake.
const DUPLEX_CAPACITY: usize = 1024 * 1024;

const FIXTURE_SESSION_ID: &str = "sess-fixture-1";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct FakeAgent {
    reader: BufReader<DuplexStream>,
    writer: DuplexStream,
}

impl FakeAgent {
    async fn next_request(&mut self) -> Value {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .await
            .expect("reads one client frame");
        assert!(read > 0, "the client closed its side of the transport");
        assert!(
            line.ends_with('\n'),
            "every client frame is newline terminated"
        );
        assert_eq!(
            line.matches('\n').count(),
            1,
            "a client frame must not contain an embedded newline"
        );
        serde_json::from_str(&line).expect("client frames are JSON")
    }

    async fn send(&mut self, value: &Value) {
        let mut bytes = serde_json::to_vec(value).expect("encodes the fixture frame");
        bytes.push(b'\n');
        self.send_raw(&bytes).await;
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.writer
            .write_all(bytes)
            .await
            .expect("writes the fixture frame");
        self.writer
            .flush()
            .await
            .expect("flushes the fixture frame");
    }

    async fn send_update(&mut self, update: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": FIXTURE_SESSION_ID, "update": update },
        }))
        .await;
    }
}

fn connect_pair(handler: Arc<dyn AcpClientHandler>) -> (Arc<AcpConnection>, FakeAgent) {
    let (client_reader, agent_writer) = tokio::io::duplex(DUPLEX_CAPACITY);
    let (client_writer, agent_reader) = tokio::io::duplex(DUPLEX_CAPACITY);
    let connection = AcpConnection::connect(client_reader, client_writer, handler);
    (
        connection,
        FakeAgent {
            reader: BufReader::new(agent_reader),
            writer: agent_writer,
        },
    )
}

fn refusing() -> Arc<dyn AcpClientHandler> {
    Arc::new(RefusingAcpClientHandler)
}

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "haider".to_owned(),
        version: "0.0.0-test".to_owned(),
    }
}

fn session_config() -> AntigravitySessionConfig {
    session_config_requesting("gemini-3.7-flash-high")
}

/// The same config with an explicit REQUESTED model, so a test can tell what
/// the session was asked for apart from what it ended up running on.
fn session_config_requesting(model: &str) -> AntigravitySessionConfig {
    AntigravitySessionConfig {
        cwd: "/workspace".to_owned(),
        additional_directories: Vec::new(),
        model: model.to_owned(),
    }
}

fn turn_request(text: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![Message::user_text(text)],
        model: "gemini-3.7-flash-high".to_owned(),
        max_tokens: 1024,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    }
}

/// The four methods the live 1.1.1 binary advertises. None of them carries a
/// `type` field, so all four are `agent`-typed by the documented default.
fn live_auth_methods() -> Value {
    json!([
        { "id": "oauth-personal", "name": "Log in with Google", "description": "Log in with your Google account" },
        { "id": "oauth-business", "name": "Log in with Gemini Enterprise" },
        { "id": "gemini-api-key", "name": "Gemini API key" },
        { "id": "agent-platform", "name": "Gemini Enterprise Agent Platform" },
    ])
}

/// Serves `initialize` -> `session/new` (refused with the documented
/// `-32000`) -> `authenticate` -> `session/new`, and returns the method id the
/// client chose.
async fn serve_handshake(
    agent: &mut FakeAgent,
    auth_methods: Value,
    protocol_version: u16,
) -> String {
    serve_handshake_returning(
        agent,
        auth_methods,
        protocol_version,
        json!({ "sessionId": FIXTURE_SESSION_ID }),
    )
    .await
}

/// The same exchange, with the caller supplying the `session/new` RESULT — the
/// frame that carries `modes` and `configOptions`, and therefore the model
/// catalog.
async fn serve_handshake_returning(
    agent: &mut FakeAgent,
    auth_methods: Value,
    protocol_version: u16,
    session_result: Value,
) -> String {
    let initialize = agent.next_request().await;
    assert_eq!(initialize["method"], "initialize");
    assert_eq!(initialize["jsonrpc"], "2.0");
    assert_eq!(initialize["params"]["protocolVersion"], 1);
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "id": initialize["id"],
            "result": {
                "protocolVersion": protocol_version,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": { "image": true, "audio": true, "embeddedContext": true },
                },
                "authMethods": auth_methods,
                "agentInfo": { "name": "antigravity-acp", "title": "Google Antigravity", "version": "agy_acp_server_1.1.1" },
            },
        }))
        .await;

    let unauthenticated = agent.next_request().await;
    assert_eq!(unauthenticated["method"], "session/new");
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "id": unauthenticated["id"],
            "error": { "code": -32000, "message": "Authentication required" },
        }))
        .await;

    let authenticate = agent.next_request().await;
    assert_eq!(authenticate["method"], "authenticate");
    let method_id = authenticate["params"]["methodId"]
        .as_str()
        .expect("authenticate carries a methodId")
        .to_owned();
    agent
        .send(&json!({ "jsonrpc": "2.0", "id": authenticate["id"], "result": {} }))
        .await;

    let new_session = agent.next_request().await;
    assert_eq!(new_session["method"], "session/new");
    assert_eq!(new_session["params"]["cwd"], "/workspace");
    assert!(new_session["params"]["mcpServers"].is_array());
    agent
        .send(&json!({
            "jsonrpc": "2.0",
            "id": new_session["id"],
            "result": session_result,
        }))
        .await;
    method_id
}

/// Renders one stream item as a compact, order-sensitive string so a whole
/// event sequence can be pinned in one assertion.
fn describe(item: &crate::ProviderStreamItem) -> String {
    match item {
        Ok(StreamEvent::TextDelta { text }) => format!("text:{}", text.to_owned_string()),
        Ok(StreamEvent::ReasoningDelta { text }) => {
            format!("reasoning:{}", text.to_owned_string())
        }
        Ok(StreamEvent::ServerToolUse {
            call_id,
            name,
            args,
        }) => {
            format!("server_tool_use:{call_id}:{name}:{args}")
        }
        Ok(StreamEvent::ServerToolResult {
            call_id,
            preview,
            is_error,
        }) => format!("server_tool_result:{call_id}:{preview}:{is_error}"),
        Ok(StreamEvent::UsageUpdate(usage)) => format!("usage:{}:{}", usage.input, usage.output),
        Ok(StreamEvent::Finish { reason }) => format!("finish:{reason:?}"),
        Ok(other) => format!("other:{other:?}"),
        Err(error) => format!("error:{:?}", error.kind),
    }
}

async fn drain(stream: &mut ProviderStream) -> Vec<crate::ProviderStreamItem> {
    let mut items = Vec::new();
    while let Some(item) = stream.recv().await {
        items.push(item);
    }
    items
}

fn rendered(items: &[crate::ProviderStreamItem]) -> Vec<String> {
    items.iter().map(describe).collect()
}

/// Exactly one terminal: one `Finish` or one `ProviderError`, and it is last.
fn assert_exactly_one_terminal(items: &[crate::ProviderStreamItem]) {
    let terminals = items
        .iter()
        .filter(|item| matches!(item, Ok(StreamEvent::Finish { .. }) | Err(_)))
        .count();
    assert_eq!(
        terminals,
        1,
        "expected one terminal in {:?}",
        rendered(items)
    );
    assert!(
        matches!(
            items.last(),
            Some(Ok(StreamEvent::Finish { .. })) | Some(Err(_))
        ),
        "the terminal must be last in {:?}",
        rendered(items)
    );
}

// ---------------------------------------------------------------------------
// 1. Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_turn_streams_text_and_reasoning_then_one_finish() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        let method_id = serve_handshake(&mut agent, live_auth_methods(), 1).await;
        let prompt = agent.next_request().await;
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["sessionId"], FIXTURE_SESSION_ID);
        assert_eq!(prompt["params"]["prompt"][0]["type"], "text");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "explain the diff");
        agent
            .send_update(json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "weighing options" },
            }))
            .await;
        agent
            .send_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "the diff " },
            }))
            .await;
        agent
            .send_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "renames one field" },
            }))
            .await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "result": { "stopReason": "end_turn" },
            }))
            .await;
        method_id
    });

    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("handshakes with the fixture agent");
    assert_eq!(provider.session_id(), FIXTURE_SESSION_ID);
    assert_eq!(provider.model(), "gemini-3.7-flash-high");

    let mut stream = provider
        .stream_turn(turn_request("explain the diff"))
        .await
        .expect("opens the turn");
    let items = drain(&mut stream).await;

    assert_eq!(
        rendered(&items),
        vec![
            "reasoning:weighing options",
            "text:the diff ",
            "text:renames one field",
            "finish:EndTurn",
        ]
    );
    assert_exactly_one_terminal(&items);
    assert_eq!(
        script.await.expect("the fixture script finishes"),
        ACP_OAUTH_PERSONAL_METHOD_ID
    );
}

// ---------------------------------------------------------------------------
// 2. Agent-executed tools are display-only
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_executed_tool_calls_surface_as_server_tool_events_never_local_tool_calls() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake(&mut agent, live_auth_methods(), 1).await;
        let prompt = agent.next_request().await;
        agent
            .send_update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "Read src/main.rs",
                "kind": "read",
                "status": "pending",
                "rawInput": { "path": "src/main.rs" },
                "locations": [{ "path": "src/main.rs" }],
            }))
            .await;
        agent
            .send_update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "in_progress",
            }))
            .await;
        agent
            .send_update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "fn main() {}" } }],
                "rawOutput": { "bytes": 12 },
            }))
            .await;
        // A repeat terminal update must not emit a second result row.
        agent
            .send_update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
            }))
            .await;
        agent
            .send_update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-2",
                "title": "Run tests",
                "kind": "execute",
                "status": "failed",
                "rawOutput": { "exit": 1 },
            }))
            .await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "result": { "stopReason": "end_turn" },
            }))
            .await;
    });

    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("handshakes");
    let mut stream = provider
        .stream_turn(turn_request("read and test"))
        .await
        .expect("opens the turn");
    let items = drain(&mut stream).await;

    assert_eq!(
        rendered(&items),
        vec![
            "server_tool_use:call-1:Read src/main.rs:{\"path\":\"src/main.rs\"}",
            "server_tool_result:call-1:fn main() {}:false",
            "server_tool_use:call-2:Run tests:null",
            "server_tool_result:call-2:{\"exit\":1}:true",
            "finish:EndTurn",
        ]
    );
    assert!(
        !items.iter().any(|item| matches!(
            item,
            Ok(StreamEvent::ToolCallStart { .. })
                | Ok(StreamEvent::ToolCallArgsDelta { .. })
                | Ok(StreamEvent::ToolCallEnd { .. })
        )),
        "an agent-executed tool call must never enter Haider's local dispatch loop"
    );
    assert_exactly_one_terminal(&items);
    script.await.expect("the fixture script finishes");
}

// ---------------------------------------------------------------------------
// 3. Framing
// ---------------------------------------------------------------------------

#[test]
fn framer_reassembles_one_message_split_across_reads() {
    let mut framer = LineFramer::new();
    let message = br#"{"jsonrpc":"2.0","id":7,"result":{"protocolVersion":1}}"#;
    for byte in message {
        framer.feed(&[*byte]);
        assert!(framer.next_frame().is_none());
    }
    framer.feed(b"\n");
    let frame = framer
        .next_frame()
        .expect("the reassembled line yields a frame")
        .expect("the reassembled line is valid JSON");
    assert_eq!(frame.id.expect("carries an id").as_outbound(), Some(7));
    assert!(framer.next_frame().is_none());
    assert_eq!(framer.buffered_bytes(), 0);
}

#[test]
fn framer_yields_every_message_coalesced_in_one_read() {
    let mut framer = LineFramer::new();
    framer.feed(
        br#"{"jsonrpc":"2.0","id":1,"result":{}}
{"jsonrpc":"2.0","id":2,"result":{}}
{"jsonrpc":"2.0","id":3,"result":{}}
"#,
    );
    let ids: Vec<Option<u64>> = std::iter::from_fn(|| framer.next_frame())
        .map(|frame| {
            frame
                .expect("each coalesced line is valid JSON")
                .id
                .expect("each response carries an id")
                .as_outbound()
        })
        .collect();
    assert_eq!(ids, vec![Some(1), Some(2), Some(3)]);
}

#[test]
fn framer_skips_empty_and_whitespace_only_lines() {
    let mut framer = LineFramer::new();
    framer.feed(b"\n   \n\t\n{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\n\n");
    let frame = framer
        .next_frame()
        .expect("the one real line yields a frame")
        .expect("the one real line is valid JSON");
    assert_eq!(frame.id.expect("carries an id").as_outbound(), Some(5));
    assert!(framer.next_frame().is_none());
}

#[test]
fn framer_rejects_an_over_long_line_without_unbounded_buffering() {
    let limit = 64;
    let mut framer = LineFramer::with_limit(limit);
    // Two full limits of payload, fed with no terminator in sight.
    framer.feed(&vec![b'x'; limit * 2]);
    let overrun = framer
        .next_frame()
        .expect("the overrun yields an outcome")
        .expect_err("an over-long line is not a frame");
    assert_eq!(overrun, FrameError::LineTooLong { limit });
    assert_eq!(
        framer.buffered_bytes(),
        0,
        "the over-long line is dropped rather than retained"
    );
    // More of the same line keeps being discarded, still without buffering.
    framer.feed(&vec![b'x'; limit * 4]);
    assert!(framer.next_frame().is_none());
    assert_eq!(framer.buffered_bytes(), 0);
    // The decoder resynchronizes at the next newline.
    framer.feed(b"tail\n{\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n");
    let frame = framer
        .next_frame()
        .expect("the decoder resynchronizes")
        .expect("the following line is valid JSON");
    assert_eq!(frame.id.expect("carries an id").as_outbound(), Some(9));
}

#[test]
fn framer_reports_malformed_json_as_a_recoverable_error_without_frame_content() {
    let mut framer = LineFramer::new();
    let oauth_line = format!(
        "{ACP_OAUTH_URL_LINE_PREFIX}https://accounts.google.com/o/oauth2/v2/auth?client_id=fixture\n"
    );
    framer.feed(oauth_line.as_bytes());
    framer.feed(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{}}\n");
    let error = framer
        .next_frame()
        .expect("the non-JSON line yields an outcome")
        .expect_err("a non-JSON line is not a frame");
    assert_eq!(error, FrameError::MalformedJson);
    let rendered = error.to_string();
    assert!(
        !rendered.contains("accounts.google.com") && !rendered.contains("client_id"),
        "a framing error must carry no frame content: {rendered}"
    );
    // The connection survives: the next line still decodes.
    let frame = framer
        .next_frame()
        .expect("the following line yields a frame")
        .expect("the following line is valid JSON");
    assert_eq!(frame.id.expect("carries an id").as_outbound(), Some(4));
}

#[test]
fn outbound_frames_are_one_line_and_never_contain_an_embedded_newline() {
    let encoded = encode_frame(&json!({ "text": "first\nsecond\r\nthird" }))
        .expect("encodes a value carrying newlines");
    assert_eq!(
        encoded.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "only the frame terminator may be a raw newline"
    );
    assert_eq!(encoded.last(), Some(&b'\n'));
}

#[tokio::test]
async fn unknown_session_update_variant_and_unknown_fields_are_ignored() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake(&mut agent, live_auth_methods(), 1).await;
        let prompt = agent.next_request().await;
        agent
            .send_update(json!({
                "sessionUpdate": "quantum_entanglement_update",
                "payload": { "anything": [1, 2, 3] },
            }))
            .await;
        agent
            .send_update(json!({
                "sessionUpdate": "plan",
                "entries": [{ "content": "step one", "status": "pending" }],
            }))
            .await;
        agent
            .send_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "still fine" },
                "messageId": "msg-1",
                "unknownField": { "future": true },
                "_meta": { "vendor": "google" },
            }))
            .await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "result": { "stopReason": "end_turn", "_meta": { "future": 1 } },
            }))
            .await;
    });

    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("handshakes");
    let mut stream = provider
        .stream_turn(turn_request("hello"))
        .await
        .expect("opens the turn");
    let items = drain(&mut stream).await;
    assert_eq!(rendered(&items), vec!["text:still fine", "finish:EndTurn"]);
    assert_exactly_one_terminal(&items);
    script.await.expect("the fixture script finishes");
}

// ---------------------------------------------------------------------------
// 4. Correlation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_and_duplicate_response_ids_do_not_corrupt_the_pending_map() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        // A response to an id Haider never minted.
        agent
            .send(&json!({ "jsonrpc": "2.0", "id": 999_999, "result": { "protocolVersion": 1 } }))
            .await;
        let initialize = agent.next_request().await;
        let response = json!({
            "jsonrpc": "2.0",
            "id": initialize["id"],
            "result": { "protocolVersion": 1, "authMethods": [] },
        });
        agent.send(&response).await;
        // The very same response again, after the correlator was settled.
        agent.send(&response).await;
        // A response carrying a string id Haider never minted.
        agent
            .send(&json!({ "jsonrpc": "2.0", "id": "not-a-haider-id", "result": {} }))
            .await;
        let new_session = agent.next_request().await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": new_session["id"],
                "result": { "sessionId": FIXTURE_SESSION_ID },
            }))
            .await;
        agent
    });

    let initialized = connection
        .initialize(client_info())
        .await
        .expect("the first response settles the correlator");
    assert_eq!(initialized.protocol_version, ACP_PROTOCOL_VERSION);
    let session = connection
        .new_session("/workspace", Vec::new())
        .await
        .expect("the pending map still correlates after stray responses");
    assert_eq!(session.session_id, FIXTURE_SESSION_ID);
    assert_eq!(connection.pending_len(), 0);
    drop(script.await.expect("the fixture script finishes"));
}

#[tokio::test]
async fn pending_request_bound_is_refused_with_a_typed_error() {
    // The fixture agent is retained but never answers, so every call issued
    // below stays outstanding.
    let (connection, _agent) = connect_pair(refusing());
    let mut outstanding = Vec::with_capacity(ACP_MAX_PENDING_REQUESTS);
    for _ in 0..ACP_MAX_PENDING_REQUESTS {
        let connection = Arc::clone(&connection);
        outstanding.push(tokio::spawn(async move {
            connection.initialize(client_info()).await
        }));
    }
    for _ in 0..100_000 {
        if connection.pending_len() == ACP_MAX_PENDING_REQUESTS {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(connection.pending_len(), ACP_MAX_PENDING_REQUESTS);

    let refused = connection.new_session("/workspace", Vec::new()).await;
    assert_eq!(
        refused.err(),
        Some(AcpError::PendingLimit {
            limit: ACP_MAX_PENDING_REQUESTS
        })
    );
    for task in outstanding {
        task.abort();
    }
}

// ---------------------------------------------------------------------------
// 5. Authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn authenticate_refuses_when_oauth_personal_is_not_advertised_and_names_advertised_ids() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        let initialize = agent.next_request().await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": {
                    "protocolVersion": 1,
                    "authMethods": [
                        { "id": "oauth-business" },
                        { "id": "gemini-api-key" },
                        { "id": "agent-platform" },
                    ],
                },
            }))
            .await;
        let new_session = agent.next_request().await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": new_session["id"],
                "error": { "code": -32000, "message": "Authentication required" },
            }))
            .await;
        // The client must NOT send `authenticate` for any other method id.
        agent
    });

    let error = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect_err("refuses to authenticate without oauth-personal");
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(
        error.message.contains("oauth-business"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("gemini-api-key"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("agent-platform"),
        "{}",
        error.message
    );
    drop(script.await.expect("the fixture script finishes"));
}

#[test]
fn auth_method_without_a_type_field_defaults_to_agent_and_is_selectable() {
    let methods: Vec<AuthMethod> =
        serde_json::from_value(live_auth_methods()).expect("the live auth method list decodes");
    assert!(
        methods
            .iter()
            .all(|method| method.method_type == AuthMethodType::Agent),
        "an absent `type` means `agent`"
    );
    let selected = select_oauth_personal_method(&methods).expect("selects oauth-personal");
    assert_eq!(selected.id, ACP_OAUTH_PERSONAL_METHOD_ID);
}

#[test]
fn a_terminal_typed_oauth_personal_method_is_never_passed_to_authenticate() {
    let methods: Vec<AuthMethod> = serde_json::from_value(json!([
        { "id": "oauth-personal", "type": "terminal" },
        { "id": "gemini-api-key" },
    ]))
    .expect("decodes");
    let error =
        select_oauth_personal_method(&methods).expect_err("a terminal method is not selectable");
    assert_eq!(
        error,
        AcpError::AuthMethodUnavailable {
            advertised: vec!["oauth-personal".to_owned(), "gemini-api-key".to_owned()],
        }
    );
}

// ---------------------------------------------------------------------------
// 6. Protocol version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn protocol_version_mismatch_is_refused_with_a_typed_error() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        let initialize = agent.next_request().await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": { "protocolVersion": 2, "authMethods": live_auth_methods() },
            }))
            .await;
        agent
    });

    let error = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect_err("refuses a protocol version Haider cannot speak");
    assert_eq!(error.kind, ProviderErrorKind::ConnectionConfiguration);
    assert!(error.message.contains('2'), "{}", error.message);
    assert!(error.message.contains('1'), "{}", error.message);
    drop(script.await.expect("the fixture script finishes"));
}

// ---------------------------------------------------------------------------
// 7. Cancellation
// ---------------------------------------------------------------------------

/// A permission handler that never answers, so `session/cancel` is the only
/// thing that can settle the request.
struct WedgedPermissionHandler {
    reached: Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

#[async_trait]
impl AcpClientHandler for WedgedPermissionHandler {
    async fn request_permission(
        &self,
        _request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, JsonRpcError> {
        if let Some(sender) = self
            .reached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = sender.send(());
        }
        std::future::pending().await
    }

    async fn read_text_file(
        &self,
        _request: FsReadTextFileRequest,
    ) -> Result<FsReadTextFileResponse, JsonRpcError> {
        std::future::pending().await
    }

    async fn write_text_file(&self, _request: FsWriteTextFileRequest) -> Result<(), JsonRpcError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_answers_pending_permission_requests_and_ends_with_one_terminal() {
    let (reached_sender, reached) = oneshot::channel();
    let handler = Arc::new(WedgedPermissionHandler {
        reached: Arc::new(std::sync::Mutex::new(Some(reached_sender))),
    });
    let (connection, mut agent) = connect_pair(handler);
    let script = tokio::spawn(async move {
        serve_handshake(&mut agent, live_auth_methods(), 1).await;
        let prompt = agent.next_request().await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": 4242,
                "method": "session/request_permission",
                "params": {
                    "sessionId": FIXTURE_SESSION_ID,
                    "toolCall": { "toolCallId": "call-1", "title": "Delete build/" },
                    "options": [
                        { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
                    ],
                },
            }))
            .await;
        // The schema REQUIRES the cancelled outcome for every still-pending
        // permission request once the client cancels.
        let permission_reply = agent.next_request().await;
        assert_eq!(permission_reply["id"], 4242);
        assert_eq!(
            permission_reply["result"]["outcome"]["outcome"],
            "cancelled"
        );
        let cancel = agent.next_request().await;
        assert_eq!(cancel["method"], "session/cancel");
        assert_eq!(cancel["params"]["sessionId"], FIXTURE_SESSION_ID);
        assert!(cancel["id"].is_null(), "session/cancel is a notification");
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "result": { "stopReason": "cancelled" },
            }))
            .await;
        agent
    });

    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("handshakes");
    let mut stream = provider
        .stream_turn(turn_request("delete the build directory"))
        .await
        .expect("opens the turn");
    reached
        .await
        .expect("the permission request reaches Haider");
    provider
        .cancel_active_turn()
        .await
        .expect("sends session/cancel");

    let items = drain(&mut stream).await;
    assert_eq!(rendered(&items), vec!["finish:Cancelled"]);
    assert_exactly_one_terminal(&items);
    assert!(
        matches!(
            items.last(),
            Some(Ok(StreamEvent::Finish {
                reason: FinishReason::Cancelled
            }))
        ),
        "cancellation is an outcome, never an error"
    );
    drop(script.await.expect("the fixture script finishes"));
}

#[tokio::test]
async fn a_request_cancelled_rpc_error_is_a_cancellation_outcome_not_a_failure() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake(&mut agent, live_auth_methods(), 1).await;
        let prompt = agent.next_request().await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "error": { "code": -32800, "message": "Request cancelled" },
            }))
            .await;
        agent
    });

    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("handshakes");
    let mut stream = provider
        .stream_turn(turn_request("anything"))
        .await
        .expect("opens the turn");
    let items = drain(&mut stream).await;
    assert_eq!(rendered(&items), vec!["finish:Cancelled"]);
    assert_exactly_one_terminal(&items);
    drop(script.await.expect("the fixture script finishes"));
}

// ---------------------------------------------------------------------------
// 8. Early EOF
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_stdout_closing_mid_turn_produces_exactly_one_terminal_error() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake(&mut agent, live_auth_methods(), 1).await;
        let _prompt = agent.next_request().await;
        agent
            .send_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "partial" },
            }))
            .await;
        // The child dies: its stdout closes with the turn still open.
        drop(agent);
    });

    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("handshakes");
    let mut stream = provider
        .stream_turn(turn_request("start something"))
        .await
        .expect("opens the turn");
    let items = drain(&mut stream).await;
    assert_eq!(
        rendered(&items),
        vec!["text:partial", "error:StreamInterrupted"]
    );
    assert_exactly_one_terminal(&items);
    script.await.expect("the fixture script finishes");
}

// ---------------------------------------------------------------------------
// 9. Bounded stderr
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stderr_flood_does_not_block_stdout_progress_and_the_retained_tail_is_bounded() {
    let (client_reader, agent_writer) = tokio::io::duplex(DUPLEX_CAPACITY);
    let (client_writer, agent_reader) = tokio::io::duplex(DUPLEX_CAPACITY);
    let (client_stderr, mut agent_stderr) = tokio::io::duplex(DUPLEX_CAPACITY);
    let connection =
        AcpConnection::connect_with_stderr(client_reader, client_writer, client_stderr, refusing());
    let mut agent = FakeAgent {
        reader: BufReader::new(agent_reader),
        writer: agent_writer,
    };

    // 8192 glog-shaped lines, roughly 90 bytes each: ~90 times the ring.
    let noise = tokio::spawn(async move {
        let line = b"I0904 12:03:12.535072 1 main.py:80] AGY ACP Server chatter chatter\n";
        for _ in 0..8192 {
            if agent_stderr.write_all(line).await.is_err() {
                return;
            }
        }
        agent_stderr.flush().await.ok();
    });

    let script = tokio::spawn(async move {
        serve_handshake(&mut agent, live_auth_methods(), 1).await;
        let prompt = agent.next_request().await;
        agent
            .send_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "progress despite the flood" },
            }))
            .await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "result": { "stopReason": "end_turn" },
            }))
            .await;
        agent
    });

    let ring = Arc::clone(connection.stderr_ring());
    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("the handshake completes while stderr floods");
    let mut stream = provider
        .stream_turn(turn_request("keep going"))
        .await
        .expect("opens the turn");
    let items = drain(&mut stream).await;
    assert_eq!(
        rendered(&items),
        vec!["text:progress despite the flood", "finish:EndTurn"]
    );

    noise.await.expect("the flood finishes");
    assert!(
        ring.retained_len() <= ACP_STDERR_TAIL_BYTES,
        "retained {} bytes, bound is {ACP_STDERR_TAIL_BYTES}",
        ring.retained_len()
    );
    assert!(ring.partial_len() <= ACP_STDERR_TAIL_BYTES);
    drop(script.await.expect("the fixture script finishes"));
}

#[test]
fn the_stderr_tail_never_retains_an_oauth_url() {
    let ring = StderrRing::new(ACP_STDERR_TAIL_BYTES);
    ring.push(b"I0904 12:03:12.535072 1 main.py:80] Starting AGY ACP Server...\n");
    ring.push(ACP_OAUTH_URL_LINE_PREFIX.as_bytes());
    ring.push(b"https://accounts.google.com/o/oauth2/v2/auth?client_id=fixture&redirect_uri=http://127.0.0.1:5411/\n");
    let tail = ring.tail();
    assert!(tail.contains("Starting AGY ACP Server"));
    assert!(tail.contains(ACP_OAUTH_URL_REDACTION));
    assert!(!tail.contains("accounts.google.com"), "{tail}");
    assert!(!tail.contains("client_id"), "{tail}");
    assert!(!tail.contains("127.0.0.1"), "{tail}");
}

#[test]
fn the_stderr_ring_bounds_a_child_that_never_emits_a_newline() {
    let ring = StderrRing::new(ACP_STDERR_TAIL_BYTES);
    for _ in 0..64 {
        ring.push(&vec![b'z'; ACP_STDERR_TAIL_BYTES]);
    }
    assert_eq!(ring.retained_len(), 0);
    assert_eq!(ring.partial_len(), ACP_STDERR_TAIL_BYTES);
}

// ---------------------------------------------------------------------------
// 10. Child environment
// ---------------------------------------------------------------------------

#[test]
fn child_environment_strips_ambient_google_credentials_and_contains_exactly_the_expected_keys() {
    let ambient: Vec<(OsString, OsString)> = [
        ("PATH", "/usr/bin:/bin"),
        ("TMPDIR", "/tmp/fixture"),
        ("LANG", "en_US.UTF-8"),
        ("LC_ALL", "en_US.UTF-8"),
        ("LC_CTYPE", "en_US.UTF-8"),
        ("HOME", "/Users/operator"),
        ("GEMINI_HOME", "/Users/operator/.gemini"),
        ("GEMINI_API_KEY", "AIza-fixture-not-a-real-key"),
        ("GOOGLE_API_KEY", "AIza-fixture-not-a-real-key"),
        ("GOOGLE_APPLICATION_CREDENTIALS", "/Users/operator/adc.json"),
        ("GOOGLE_CLOUD_PROJECT", "operator-project"),
        ("CLOUDSDK_CORE_PROJECT", "operator-project"),
        ("CLOUDSDK_AUTH_ACCESS_TOKEN", "fixture-token"),
        ("AGY_ACP_CCPA_PROJECT", "operator-project"),
        ("AGY_ACP_ENABLE_OAUTH", "1"),
        ("ANTIGRAVITY_HARNESS_PATH", "/opt/harness"),
        ("BROWSER", "/usr/bin/open"),
        ("SSH_AUTH_SOCK", "/private/tmp/ssh"),
    ]
    .into_iter()
    .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    .collect();

    let environment = acp_child_environment(
        std::path::Path::new("/profiles/account-a"),
        std::path::Path::new("/profiles/account-a/home"),
        ambient,
    );

    let names: Vec<String> = environment
        .keys()
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "AGY_ACP_FORCE_FILE_STORAGE",
            "GEMINI_HOME",
            "HOME",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "PATH",
            "PYTHONUNBUFFERED",
            "TMPDIR",
        ]
    );
    assert_eq!(
        environment[&OsString::from("GEMINI_HOME")],
        OsString::from("/profiles/account-a")
    );
    assert_eq!(
        environment[&OsString::from("HOME")],
        OsString::from("/profiles/account-a/home")
    );
    assert_eq!(
        environment[&OsString::from("AGY_ACP_FORCE_FILE_STORAGE")],
        OsString::from("1")
    );
    assert_eq!(
        environment[&OsString::from("PYTHONUNBUFFERED")],
        OsString::from("1")
    );
    for stripped in ACP_STRIPPED_ENVIRONMENT_NAMES {
        assert!(
            stripped == "GEMINI_HOME" || !environment.contains_key(&OsString::from(stripped)),
            "{stripped} reached the child"
        );
    }
    for prefix in ACP_STRIPPED_ENVIRONMENT_PREFIXES {
        assert!(
            !names.iter().any(|name| name.starts_with(prefix)),
            "a {prefix}* variable reached the child"
        );
    }
    // The ambient GEMINI_HOME is REPLACED by the per-account profile, never
    // inherited.
    assert_ne!(
        environment[&OsString::from("GEMINI_HOME")],
        OsString::from("/Users/operator/.gemini")
    );
}

// ---------------------------------------------------------------------------
// 11. Usage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_update_does_not_produce_a_billing_usage_event() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake(&mut agent, live_auth_methods(), 1).await;
        let prompt = agent.next_request().await;
        agent
            .send_update(json!({
                "sessionUpdate": "usage_update",
                "used": 128_000,
                "size": 1_048_576,
                "cost": 0.42,
            }))
            .await;
        agent
            .send_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "done" },
            }))
            .await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "result": { "stopReason": "end_turn" },
            }))
            .await;
    });

    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("handshakes");
    let mut stream = provider
        .stream_turn(turn_request("count my context"))
        .await
        .expect("opens the turn");
    let items = drain(&mut stream).await;
    assert_eq!(rendered(&items), vec!["text:done", "finish:EndTurn"]);
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, Ok(StreamEvent::UsageUpdate(_)))),
        "context-window occupancy is not billing and must never become a Usage event"
    );
    script.await.expect("the fixture script finishes");
}

// ---------------------------------------------------------------------------
// 12. Real subprocess
// ---------------------------------------------------------------------------

fn scratch_dir(label: &str) -> std::path::PathBuf {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "haider-acp-{label}-{}-{}",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&directory).expect("creates the scratch directory");
    directory
}

/// Gated to Unix because the fixture agent is a `/bin/sh` script; Windows is
/// covered by inspection — `AcpConnection::spawn` and `terminate_child` are
/// the same code on both platforms, and `haider_platform::signal_process`
/// terminates the process directly on Windows instead of delivering SIGTERM.
#[cfg(unix)]
#[tokio::test]
async fn spawned_child_round_trips_initialize_and_is_reaped_without_an_orphan() {
    let directory = scratch_dir("spawn");
    let script = directory.join("fake-acp-agent.sh");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "read -r line\n",
            "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,",
            "\"authMethods\":[{\"id\":\"oauth-personal\",\"name\":\"Log in with Google\"}]}}'\n",
            "printf '%s\\n' 'I0904 12:03:12.535072 1 main.py:80] Starting AGY ACP Server...' >&2\n",
        ),
    )
    .expect("writes the fixture agent script");

    let spec = AcpLaunchSpec {
        program: std::path::PathBuf::from("/bin/sh"),
        args: vec![OsString::from(&script)],
        profile_dir: directory.join("profile"),
        home_dir: directory.join("home"),
        working_dir: directory.clone(),
    };
    let connection = AcpConnection::spawn(&spec, refusing()).expect("spawns the fixture agent");
    assert!(connection.supervised_child_present().await);

    let initialized = connection
        .initialize(client_info())
        .await
        .expect("round trips initialize through a real child");
    assert_eq!(initialized.protocol_version, ACP_PROTOCOL_VERSION);
    assert_eq!(
        initialized
            .auth_methods
            .first()
            .expect("the fixture advertises one method")
            .id,
        ACP_OAUTH_PERSONAL_METHOD_ID
    );

    // The stderr drain is a separate task; give it a bounded window to observe
    // the child's one diagnostics line.
    let mut tail = connection.stderr_tail();
    for _ in 0..200 {
        if !tail.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        tail = connection.stderr_tail();
    }
    assert!(tail.contains("Starting AGY ACP Server"), "{tail}");

    // `Exited` is the reap proof: it is only returned once `Child::wait`
    // yielded an exit status, which is exactly what reaps the process.
    assert_eq!(connection.shutdown(None).await, AcpChildReap::Exited);
    assert!(!connection.supervised_child_present().await);

    std::fs::remove_dir_all(&directory).ok();
}

// ---------------------------------------------------------------------------
// Capability and inbound-request contracts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_default_handler_refuses_every_filesystem_and_terminal_request() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        let initialize = agent.next_request().await;
        // Haider must not advertise a capability it cannot enforce.
        assert_eq!(
            initialize["params"]["clientCapabilities"]["terminal"],
            false
        );
        assert_eq!(
            initialize["params"]["clientCapabilities"]["fs"]["readTextFile"],
            false
        );
        assert_eq!(
            initialize["params"]["clientCapabilities"]["fs"]["writeTextFile"],
            false
        );
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": { "protocolVersion": 1, "authMethods": live_auth_methods() },
            }))
            .await;

        let mut refusals = Vec::new();
        for (index, request) in [
            json!({ "sessionId": FIXTURE_SESSION_ID, "path": "/etc/passwd" }),
            json!({ "sessionId": FIXTURE_SESSION_ID, "path": "/etc/hosts", "content": "boom" }),
            json!({ "sessionId": FIXTURE_SESSION_ID, "command": "rm -rf /" }),
        ]
        .into_iter()
        .enumerate()
        {
            let method = ["fs/read_text_file", "fs/write_text_file", "terminal/create"][index];
            agent
                .send(&json!({
                    "jsonrpc": "2.0",
                    "id": 500 + index,
                    "method": method,
                    "params": request,
                }))
                .await;
            let reply = agent.next_request().await;
            refusals.push((
                reply["id"].as_i64().expect("the id is echoed"),
                reply["error"]["code"]
                    .as_i64()
                    .expect("a refusal carries a code"),
            ));
        }
        refusals
    });

    connection
        .initialize(client_info())
        .await
        .expect("initializes");
    let refusals = script.await.expect("the fixture script finishes");
    assert_eq!(refusals, vec![(500, -32601), (501, -32601), (502, -32601)]);
}

#[tokio::test]
async fn capabilities_declare_agent_owned_tools_and_visible_thinking() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake(&mut agent, live_auth_methods(), 1).await;
        agent
    });
    let provider = AntigravityAcpProvider::handshake(connection, &session_config())
        .await
        .expect("handshakes");
    let capabilities = provider.capabilities().await;
    assert_eq!(capabilities.provider, GOOGLE_ANTIGRAVITY_PROVIDER_NAME);
    // The agent runs its own tools; Haider never dispatches one for it.
    assert_eq!(capabilities.parallel_tools, FeatureResolve::Unsupported);
    assert_eq!(
        capabilities.streaming_tool_args,
        FeatureResolve::Unsupported
    );
    assert_eq!(capabilities.thinking_visible, FeatureResolve::Native);
    assert_eq!(capabilities.context_limit, 1_048_576);
    assert_eq!(
        provider.credential_surface(),
        crate::ProviderCredentialSurface::Opaque
    );
    drop(script.await.expect("the fixture script finishes"));
}

#[test]
fn the_provider_name_is_a_builtin_provider() {
    assert_eq!(GOOGLE_ANTIGRAVITY_PROVIDER_NAME, "google-antigravity");
    assert!(
        crate::BUILTIN_PROVIDER_NAMES.contains(&GOOGLE_ANTIGRAVITY_PROVIDER_NAME),
        "the installer and account plumbing landed in v0.0.970"
    );
    assert!(
        crate::BUILTIN_PROVIDER_NAMES.contains(&crate::GEMINI_PROVIDER_NAME),
        "the API-key gemini class is a separate roster entry and stays"
    );
}

#[test]
fn every_stop_reason_maps_to_one_finish_reason() {
    let cases = [
        ("end_turn", StopReason::EndTurn, FinishReason::EndTurn),
        ("max_tokens", StopReason::MaxTokens, FinishReason::MaxTokens),
        (
            "max_turn_requests",
            StopReason::MaxTurnRequests,
            FinishReason::MaxTokens,
        ),
        ("refusal", StopReason::Refusal, FinishReason::Refusal),
        ("cancelled", StopReason::Cancelled, FinishReason::Cancelled),
    ];
    for (wire, expected, finish) in cases {
        let decoded: StopReason =
            serde_json::from_value(Value::String(wire.to_owned())).expect("decodes");
        assert_eq!(decoded, expected, "{wire}");
        assert_eq!(
            crate::acp::antigravity::finish_reason(decoded),
            finish,
            "{wire}"
        );
    }
    // The enum is exhaustive by design: an unknown terminal outcome cannot be
    // mapped honestly, so it fails to decode rather than becoming `EndTurn`.
    assert!(serde_json::from_value::<StopReason>(json!("teleported")).is_err());
}

#[test]
fn an_unknown_session_update_variant_decodes_to_the_catch_all() {
    let update: SessionUpdate =
        serde_json::from_value(json!({ "sessionUpdate": "not_in_v1", "extra": 1 }))
            .expect("an unknown variant decodes rather than failing the stream");
    assert!(matches!(update, SessionUpdate::Other));
}

// ---------------------------------------------------------------------------
// Model catalog (a session CONFIGURATION OPTION, not an ACP field)
// ---------------------------------------------------------------------------

/// The model selector as a FLAT `select` option carrying the reserved `model`
/// category — the shape the schema documents first.
fn flat_model_option(current: &str) -> Value {
    json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": current,
        "options": [
            { "value": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (high)" },
            { "value": "gemini-3.7-flash-high", "name": "Gemini 3.7 Flash (high)" },
            { "value": "gemini-pro-agent", "name": "Gemini Pro Agent" },
        ],
    })
}

/// The same selector in the GROUPED shape the `anyOf` also permits.
fn grouped_model_option(current: &str) -> Value {
    json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": current,
        "options": [
            {
                "group": "flash",
                "name": "Flash",
                "options": [
                    { "value": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (high)" },
                    { "value": "gemini-3.8-flash-low", "name": "Gemini 3.8 Flash (low)" },
                ],
            },
            {
                "group": "pro",
                "name": "Pro",
                "options": [{ "value": "gemini-pro-agent", "name": "Gemini Pro Agent" }],
            },
        ],
    })
}

fn session_result(config_options: Value) -> Value {
    json!({ "sessionId": FIXTURE_SESSION_ID, "configOptions": config_options })
}

/// Handshakes against a fixture that answers `session/new` with `result`, and
/// hands the still-open fixture back so the test can keep serving.
async fn handshake_with_session_result(
    requested_model: &str,
    result: Value,
) -> (AntigravityAcpProvider, tokio::task::JoinHandle<FakeAgent>) {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake_returning(&mut agent, live_auth_methods(), 1, result).await;
        agent
    });
    let provider =
        AntigravityAcpProvider::handshake(connection, &session_config_requesting(requested_model))
            .await
            .expect("handshakes with the fixture agent");
    (provider, script)
}

/// A flat model select publishes the catalog the daemon's policy consumes:
/// ids, display names, the `configId` a selection must name, and the value the
/// session is ALREADY on.
///
/// MUTATION CHECK: read the catalog off a `models`/`availableModels` field on
/// `NewSessionResponse`. Expected runtime failure: no published ACP schema has
/// one, the fixture sends none, and `model_catalog()` is `None`.
#[tokio::test]
async fn a_flat_model_select_option_publishes_the_session_catalog() {
    let (provider, script) = handshake_with_session_result(
        "gemini-3.8-flash-high",
        session_result(json!([flat_model_option("gemini-3.7-flash-high")])),
    )
    .await;
    let _agent = script.await.expect("the fixture script completes");

    let catalog = provider
        .model_catalog()
        .expect("the agent published a model selector");
    assert_eq!(catalog.config_id, "model");
    assert_eq!(
        catalog.current_value.as_deref(),
        Some("gemini-3.7-flash-high")
    );
    assert_eq!(
        catalog.model_ids(),
        vec![
            "gemini-3.8-flash-high".to_owned(),
            "gemini-3.7-flash-high".to_owned(),
            "gemini-pro-agent".to_owned(),
        ]
    );
    assert_eq!(catalog.models[0].name, "Gemini 3.8 Flash (high)");
    assert_eq!(
        provider.model(),
        "gemini-3.7-flash-high",
        "the session reports the model it is RUNNING on, not the one requested"
    );
}

/// The GROUPED `anyOf` shape flattens to the same catalog, in wire order.
///
/// MUTATION CHECK: decode only the flat variant. Expected runtime failure: the
/// grouped array does not match `SessionConfigSelectOption`, the option
/// publishes nothing, and `model_catalog()` is `None`.
#[tokio::test]
async fn a_grouped_model_select_option_is_flattened_in_wire_order() {
    let (provider, script) = handshake_with_session_result(
        "",
        session_result(json!([grouped_model_option("gemini-3.8-flash-low")])),
    )
    .await;
    let _agent = script.await.expect("the fixture script completes");

    let catalog = provider.model_catalog().expect("a grouped selector");
    assert_eq!(
        catalog.model_ids(),
        vec![
            "gemini-3.8-flash-high".to_owned(),
            "gemini-3.8-flash-low".to_owned(),
            "gemini-pro-agent".to_owned(),
        ],
        "groups are flattened, and group order is wire order"
    );
    assert_eq!(
        catalog.current_value.as_deref(),
        Some("gemini-3.8-flash-low")
    );
}

/// A session that publishes no model selector publishes no catalog. Two
/// near-misses are present and neither is one: `modes` — whose
/// `availableModes`/`currentModeId` are the fields most easily mistaken for a
/// model list, so the fixture fills them with model-shaped ids — and a
/// `boolean` option that is NAMED `model` and even carries an `options` array.
///
/// MUTATION CHECK 1: resolve the catalog from `modes.availableModes`. Expected
/// runtime failure: `model_catalog()` becomes `Some` and the `is_none`
/// assertion fails.
///
/// MUTATION CHECK 2: let `SessionConfigOptionType::Boolean` publish options.
/// Expected runtime failure: `not-a-model` resolves as a catalog and the same
/// assertion fails.
#[tokio::test]
async fn no_model_selector_means_no_catalog_and_modes_are_never_models() {
    let (provider, script) = handshake_with_session_result(
        "gemini-3.8-flash-high",
        json!({
            "sessionId": FIXTURE_SESSION_ID,
            "modes": {
                "currentModeId": "gemini-3.7-flash-high",
                "availableModes": [
                    { "id": "gemini-3.7-flash-high", "name": "Fast" },
                    { "id": "gemini-pro-agent", "name": "Deep" },
                ],
            },
            "configOptions": [
                { "id": "yolo", "name": "Auto approve", "type": "boolean", "currentValue": true },
                {
                    "id": "model",
                    "name": "Model preview",
                    "type": "boolean",
                    "currentValue": false,
                    "options": [{ "value": "not-a-model", "name": "Not a model" }],
                },
            ],
        }),
    )
    .await;
    let _agent = script.await.expect("the fixture script completes");

    assert!(
        provider.model_catalog().is_none(),
        "a mode list and a boolean option named `model` are not a model catalog"
    );
    assert_eq!(
        provider.model(),
        "gemini-3.8-flash-high",
        "with no selector to report, the requested model is what is recorded"
    );
}

/// A `session/new` response carrying unknown extra fields, an unknown
/// `category` and an unknown config-option `type` decodes without failing, and
/// the real selector is still found. ACP is explicitly extensible; a decode
/// failure here would drop a catalog the agent did publish.
///
/// MUTATION CHECK: `#[serde(deny_unknown_fields)]` on `SessionConfigOption`.
/// Expected runtime failure: the `session/new` result no longer decodes, the
/// handshake returns a malformed-frame error and the test panics on `expect`.
#[tokio::test]
async fn unknown_fields_categories_and_option_types_never_fail_the_catalog() {
    let (provider, script) = handshake_with_session_result(
        "",
        json!({
            "sessionId": FIXTURE_SESSION_ID,
            "_meta": { "vendor": "google" },
            "somethingHaiderHasNeverHeardOf": [1, 2, 3],
            "configOptions": [
                {
                    "id": "telemetry",
                    "name": "Telemetry",
                    "category": "diagnostics",
                    "type": "slider",
                    "currentValue": 3,
                    "_meta": { "unit": "level" },
                },
                {
                    "id": "reasoning",
                    "name": "Show reasoning",
                    "category": "presentation",
                    "type": "boolean",
                    "currentValue": false,
                },
                {
                    "id": "engine",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "gemini-3.8-flash-high",
                    "unknownOptionField": true,
                    "options": [
                        {
                            "value": "gemini-3.8-flash-high",
                            "name": "Gemini 3.8 Flash (high)",
                            "_meta": { "badge": "new" },
                        },
                        { "value": "gemini-pro-agent" },
                    ],
                },
            ],
        }),
    )
    .await;
    let _agent = script.await.expect("the fixture script completes");

    let catalog = provider
        .model_catalog()
        .expect("the unknown neighbours did not take the catalog down");
    assert_eq!(catalog.config_id, "engine");
    assert_eq!(
        catalog.model_ids(),
        vec![
            "gemini-3.8-flash-high".to_owned(),
            "gemini-pro-agent".to_owned(),
        ],
        "the categorized selector is the catalog, unknown neighbours and all"
    );
    assert_eq!(
        catalog.models[1].name, "gemini-pro-agent",
        "an option that published no name displays as its own id"
    );
}

/// The reserved `model` CATEGORY wins over an option merely named `model`.
///
/// MUTATION CHECK: try the id convention before the category. Expected runtime
/// failure: the catalog resolves to `model`/`legacy-only` and both assertions
/// below fail.
#[tokio::test]
async fn the_model_category_wins_over_an_option_merely_named_model() {
    let (provider, script) = handshake_with_session_result(
        "",
        session_result(json!([
            {
                "id": "model",
                "name": "Legacy",
                "type": "select",
                "currentValue": "legacy-only",
                "options": [{ "value": "legacy-only", "name": "Legacy only" }],
            },
            {
                "id": "engine",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "gemini-3.8-flash-high",
                "options": [
                    { "value": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (high)" },
                    { "value": "gemini-pro-agent", "name": "Gemini Pro Agent" },
                ],
            },
        ])),
    )
    .await;
    let _agent = script.await.expect("the fixture script completes");

    let catalog = provider.model_catalog().expect("the categorized selector");
    assert_eq!(catalog.config_id, "engine");
    assert!(
        !catalog.model_ids().contains(&"legacy-only".to_owned()),
        "the option named `model` was not the selector: {:?}",
        catalog.model_ids()
    );
}

/// With no category published anywhere, the `model` id is the fallback. The
/// schema forbids requiring a category for correctness, so a catalog must
/// still be reachable without one.
///
/// MUTATION CHECK: drop the id fallback and resolve by category only. Expected
/// runtime failure: `model_catalog()` is `None` and the `expect` panics.
#[tokio::test]
async fn the_model_id_is_the_fallback_when_no_category_is_published() {
    let (provider, script) = handshake_with_session_result(
        "",
        session_result(json!([
            {
                "id": "verbosity",
                "name": "Verbosity",
                "type": "select",
                "currentValue": "normal",
                "options": [
                    { "value": "normal", "name": "Normal" },
                    { "value": "terse", "name": "Terse" },
                ],
            },
            {
                "id": "model",
                "name": "Model",
                "type": "select",
                "currentValue": "gemini-3.7-flash-high",
                "options": [
                    { "value": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (high)" },
                    { "value": "gemini-3.7-flash-high", "name": "Gemini 3.7 Flash (high)" },
                ],
            },
        ])),
    )
    .await;
    let _agent = script.await.expect("the fixture script completes");

    let catalog = provider.model_catalog().expect("the id fallback");
    assert_eq!(catalog.config_id, "model");
    assert_eq!(
        catalog.current_value.as_deref(),
        Some("gemini-3.7-flash-high")
    );
    assert!(
        !catalog.model_ids().contains(&"terse".to_owned()),
        "an unrelated select is not a model catalog: {:?}",
        catalog.model_ids()
    );
}

/// Selecting a model that is not the one in force writes
/// `session/set_config_option` naming the selector's own `configId`, carrying
/// the STRING value variant (no `type` on the wire), and refreshes the cache
/// from the full option set the agent answers with.
///
/// MUTATION CHECK: send `{"type":"boolean","value":...}` instead of the string
/// variant. Expected runtime failure: the fixture's `type`-absent assertion
/// fires.
#[tokio::test]
async fn selecting_a_non_current_model_writes_set_config_option_with_the_string_variant() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake_returning(
            &mut agent,
            live_auth_methods(),
            1,
            session_result(json!([flat_model_option("gemini-3.7-flash-high")])),
        )
        .await;
        let set = agent.next_request().await;
        assert_eq!(set["method"], "session/set_config_option");
        assert_eq!(set["params"]["sessionId"], FIXTURE_SESSION_ID);
        assert_eq!(set["params"]["configId"], "model");
        assert_eq!(set["params"]["value"], "gemini-3.8-flash-high");
        assert!(
            set["params"].get("type").is_none(),
            "the string value variant carries no `type`: {}",
            set["params"]
        );
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": set["id"],
                "result": { "configOptions": [flat_model_option("gemini-3.8-flash-high")] },
            }))
            .await;
        agent
    });

    let provider = AntigravityAcpProvider::handshake(
        connection,
        &session_config_requesting("gemini-3.8-flash-high"),
    )
    .await
    .expect("handshakes with the fixture agent");
    provider
        .select_model("gemini-3.8-flash-high")
        .await
        .expect("selects an offered model");
    let _agent = script.await.expect("the fixture script completes");

    assert_eq!(provider.model(), "gemini-3.8-flash-high");
    assert_eq!(
        provider
            .model_catalog()
            .expect("catalog")
            .current_value
            .as_deref(),
        Some("gemini-3.8-flash-high"),
        "the cache is refreshed from the agent's answer, not from the request"
    );
}

/// Selecting the model already in force writes NO frame: `currentValue` is
/// what the session is running on, and a round trip to set it to itself would
/// spend a Google round trip per turn.
///
/// MUTATION CHECK: always write `session/set_config_option`. Expected runtime
/// failure: the fixture's next request is `session/set_config_option` instead
/// of `session/prompt` and its assertion fires.
#[tokio::test]
async fn selecting_the_model_already_in_force_writes_no_frame() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake_returning(
            &mut agent,
            live_auth_methods(),
            1,
            session_result(json!([flat_model_option("gemini-3.7-flash-high")])),
        )
        .await;
        let next = agent.next_request().await;
        assert_eq!(
            next["method"], "session/prompt",
            "nothing was written between the handshake and the turn: {next}"
        );
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": next["id"],
                "result": { "stopReason": "end_turn" },
            }))
            .await;
        agent
    });

    let provider = AntigravityAcpProvider::handshake(
        connection,
        &session_config_requesting("gemini-3.7-flash-high"),
    )
    .await
    .expect("handshakes with the fixture agent");
    provider
        .select_model("gemini-3.7-flash-high")
        .await
        .expect("the model already in force");
    let mut stream = provider
        .stream_turn(turn_request("explain the diff"))
        .await
        .expect("opens a turn");
    let items = drain(&mut stream).await;
    let _agent = script.await.expect("the fixture script completes");

    assert_eq!(rendered(&items), vec!["finish:EndTurn".to_owned()]);
    assert_eq!(provider.model(), "gemini-3.7-flash-high");
}

/// A mid-turn `config_option_update` carries the FULL option set with current
/// values, so it refreshes the cached catalog — and the recorded model with it.
///
/// MUTATION CHECK: keep ignoring `config_option_update`. Expected runtime
/// failure: the catalog still reports `gemini-3.7-flash-high` and both
/// post-turn assertions fail.
#[tokio::test]
async fn a_config_option_update_refreshes_the_cached_catalog() {
    let (connection, mut agent) = connect_pair(refusing());
    let script = tokio::spawn(async move {
        serve_handshake_returning(
            &mut agent,
            live_auth_methods(),
            1,
            session_result(json!([flat_model_option("gemini-3.7-flash-high")])),
        )
        .await;
        let prompt = agent.next_request().await;
        assert_eq!(prompt["method"], "session/prompt");
        agent
            .send_update(json!({
                "sessionUpdate": "config_option_update",
                "configOptions": [{
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "gemini-pro-agent",
                    "options": [
                        { "value": "gemini-pro-agent", "name": "Gemini Pro Agent" },
                        { "value": "gemini-3.9-flash-high", "name": "Gemini 3.9 Flash (high)" },
                    ],
                }],
            }))
            .await;
        agent
            .send(&json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "result": { "stopReason": "end_turn" },
            }))
            .await;
        agent
    });

    let provider = AntigravityAcpProvider::handshake(
        connection,
        &session_config_requesting("gemini-3.7-flash-high"),
    )
    .await
    .expect("handshakes with the fixture agent");
    let mut stream = provider
        .stream_turn(turn_request("explain the diff"))
        .await
        .expect("opens a turn");
    let items = drain(&mut stream).await;
    let _agent = script.await.expect("the fixture script completes");

    assert_eq!(
        rendered(&items),
        vec!["finish:EndTurn".to_owned()],
        "a config-option update renders nothing"
    );
    let catalog = provider.model_catalog().expect("the refreshed catalog");
    assert_eq!(catalog.current_value.as_deref(), Some("gemini-pro-agent"));
    assert_eq!(
        catalog.model_ids(),
        vec![
            "gemini-pro-agent".to_owned(),
            "gemini-3.9-flash-high".to_owned(),
        ],
        "the update REPLACES the catalog; it is not a delta"
    );
    assert_eq!(provider.model(), "gemini-pro-agent");
}

/// A session with no selector cannot be put on a model at all, and says so
/// instead of pretending the write happened.
///
/// MUTATION CHECK: return `Ok(())` from `select_model` when no catalog exists.
/// Expected runtime failure: `expect_err` panics.
#[tokio::test]
async fn selecting_a_model_without_a_selector_is_refused_rather_than_faked() {
    let (provider, script) =
        handshake_with_session_result("gemini-3.8-flash-high", session_result(json!([]))).await;
    let _agent = script.await.expect("the fixture script completes");

    let error = provider
        .select_model("gemini-3.8-flash-high")
        .await
        .expect_err("there is nothing to set");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(
        error.message.contains("no model selector"),
        "the refusal names what is missing: {}",
        error.message
    );
}
