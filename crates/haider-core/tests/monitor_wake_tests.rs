#![allow(clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use haider_core::{
    CancelToken, HarnessActor, HarnessConfig, MemoryStore, SubmitTurn, ToolDispatchResult,
    ToolDispatcher,
};
use haider_protocol::error::HaiderError;
use haider_protocol::ids::{DeviceId, ItemId, RunId, SessionId};
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason, StreamEvent};
use haider_protocol::state::RunState;
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_provider::{FakeProvider, Provider, ProviderError, ProviderStream, TurnRequest};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

type Request = (
    TurnRequest,
    mpsc::Sender<Result<StreamEvent, ProviderError>>,
);

struct ControlledProvider(mpsc::UnboundedSender<Request>);

#[async_trait]
impl Provider for ControlledProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let (tx, rx) = mpsc::channel(16);
        self.0.send((request, tx)).expect("request observer");
        Ok(rx.into())
    }
}

struct RegistrationDispatcher(mpsc::UnboundedSender<String>);

#[async_trait]
impl ToolDispatcher for RegistrationDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        self.0.send(call_id.to_owned()).expect("dispatch observer");
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "Monitor started".into(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

async fn receive<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("actor progress")
        .expect("observer remains open")
}

async fn tool(tx: &mpsc::Sender<Result<StreamEvent, ProviderError>>, id: &str, name: &str) {
    for event in [
        StreamEvent::ToolCallStart {
            call_id: id.into(),
            name: name.into(),
        },
        StreamEvent::ToolCallArgsDelta {
            call_id: id.into(),
            args_fragment: "{}".into(),
        },
        StreamEvent::ToolCallEnd { call_id: id.into() },
    ] {
        tx.send(Ok(event)).await.expect("stream open");
    }
}

fn assert_pairs(request: &TurnRequest) {
    let mut pending = std::collections::BTreeSet::new();
    for message in &request.messages {
        for block in &message.blocks {
            match block {
                Block::ToolCall { call_id, .. } => {
                    assert!(pending.insert(call_id));
                }
                Block::ToolResult { call_id, .. } => {
                    assert!(
                        pending.remove(call_id),
                        "duplicate/unmatched result {call_id}"
                    );
                }
                Block::Text { text } => {
                    assert!(!text.is_blank(), "empty text");
                    if message.role == haider_provider::MessageRole::User {
                        assert!(
                            pending.is_empty(),
                            "user input before tool results: {pending:?}"
                        );
                    }
                }
                _ => {}
            }
        }
    }
    assert!(pending.is_empty(), "unmatched calls: {pending:?}");
}

#[tokio::test]
async fn monitor_wake_keeps_completed_results_when_holding_a_later_call() {
    let (requests_tx, mut requests) = mpsc::unbounded_channel();
    let (dispatch_tx, mut dispatched) = mpsc::unbounded_channel();
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        HarnessConfig::for_session(SessionId::new("monitor-test"), DeviceId::new("test"), 1, 1),
        Arc::new(ControlledProvider(requests_tx)),
        Arc::new(MemoryStore::new()),
        Some(Arc::new(RegistrationDispatcher(dispatch_tx))),
    );
    let task = tokio::spawn(actor.run());
    let turn = handle
        .submit_turn(SubmitTurn::new("watch the file"))
        .await
        .expect("turn");
    let (_, tx) = receive(&mut requests).await;
    tool(&tx, "registration", "monitor").await;
    assert_eq!(receive(&mut dispatched).await, "registration");
    handle
        .subturn("<monitor-event>\n```json\n{\"type\":\"monitor_event\"}\n```\n</monitor-event>")
        .expect("wake accepted");
    tool(&tx, "held", "inspect").await;
    let (wake_request, next) = receive(&mut requests).await;
    println!(
        "ACTOR_WAKE_REQUEST={}",
        serde_json::to_string_pretty(&wake_request).expect("request JSON")
    );
    println!(
        "WIRE_WAKE_REQUEST={}",
        serde_json::to_string_pretty(&wire_requests(&wake_request)).expect("wire JSON")
    );
    assert_pairs(&wake_request);
    assert!(
        wake_request
            .messages
            .iter()
            .flat_map(|m| &m.blocks)
            .any(|block| {
                matches!(block, Block::ToolResult { call_id, preview, .. }
            if call_id == "registration" && preview == "Monitor started")
            })
    );
    next.send(Ok(StreamEvent::Finish {
        reason: FinishReason::EndTurn,
    }))
    .await
    .expect("finish");
    assert_eq!(turn.wait().await.expect("outcome").state, RunState::Done);
    assert!(dispatched.try_recv().is_err(), "held call must not execute");
    handle.stop().await.expect("stop");
    task.await.expect("join");
}

fn wire_requests(request: &TurnRequest) -> serde_json::Value {
    use haider_accounts::{CredentialAlias, MemoryVault, Vault};
    use haider_provider::{
        AnthropicProvider, GeminiProvider, OpenAiCompatibleProvider, OpenAiProvider,
    };
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("monitor-wire-fixture");
    vault
        .put(&alias, b"synthetic-never-sent")
        .expect("fixture key");
    let mut request = request.clone();
    // Freeze nondeterministic cache diagnostics while retaining the actor's exact messages.
    request.cache_metadata = None;
    request.model = "claude-fable-5-1".into();
    let anthropic =
        AnthropicProvider::new(vault.resolve(&alias).expect("fixture key"), &request.model)
            .expect("Anthropic adapter")
            .request_payload(&request)
            .expect("Anthropic request");
    let anthropic_oauth = AnthropicProvider::new_subscription(
        vault.resolve(&alias).expect("fixture key"),
        &request.model,
        haider_provider::ANTHROPIC_OAUTH_BASE_URL,
    )
    .expect("Anthropic OAuth adapter")
    .request_payload(&request)
    .expect("Anthropic OAuth request");
    assert_eq!(
        anthropic["messages"], anthropic_oauth["messages"],
        "OAuth keeps the same event/tool pairing"
    );
    request.model = "gpt-5".into();
    let openai = OpenAiProvider::new(vault.resolve(&alias).expect("fixture key"), &request.model)
        .expect("OpenAI adapter")
        .request_payload(&request)
        .expect("OpenAI request");
    let chat = OpenAiCompatibleProvider::new(
        vault.resolve(&alias).expect("fixture key"),
        &request.model,
        "https://api.openai.com/v1",
    )
    .expect("Chat adapter")
    .request_payload(&request)
    .expect("Chat request");
    request.model = "gemini-2.5-flash".into();
    let gemini = GeminiProvider::new(vault.resolve(&alias).expect("fixture key"), &request.model)
        .expect("Gemini adapter")
        .request_payload(&request)
        .expect("Gemini request");
    serde_json::json!({"anthropic": anthropic, "anthropic_oauth": anthropic_oauth, "openai": openai, "chat": chat, "gemini": gemini})
}

fn monitor_event(payload: &str) -> String {
    format!(
        "<monitor-event monitor=\"monitor-fixture\" source=\"file\" occurrence=\"once\">\n```json\n{payload}\n```\n</monitor-event>"
    )
}

fn completed_registration() -> Vec<haider_provider::Message> {
    use haider_provider::Message;
    vec![
        Message::user_text("watch the file"),
        Message::assistant(vec![Block::ToolCall {
            call_id: "registration".into(),
            name: "monitor".into(),
            args: serde_json::json!({}),
        }]),
        Message::tool_result("registration", "Monitor started", false),
        Message::assistant(vec![Block::Text {
            text: "Monitoring.".into(),
        }]),
    ]
}

async fn committed_wake(name: &str, messages: Vec<haider_provider::Message>) {
    let (tx, mut requests) = mpsc::unbounded_channel();
    let handle = HarnessActor::spawn(
        HarnessConfig::for_session(
            SessionId::new("monitor-committed"),
            DeviceId::new("test"),
            1,
            1,
        ),
        Arc::new(ControlledProvider(tx)),
        Arc::new(MemoryStore::new()),
    );
    let turn = handle
        .submit_committed_turn(haider_core::SubmitCommittedTurn {
            run_id: RunId::new("committed-wake"),
            messages,
        })
        .await
        .expect("committed wake");
    let (request, stream) = receive(&mut requests).await;
    assert_pairs(&request);
    let wire = wire_requests(&request);
    assert_wire_shapes(&wire);
    record_golden(name, &wire);
    stream
        .send(Ok(StreamEvent::Finish {
            reason: FinishReason::EndTurn,
        }))
        .await
        .expect("finish");
    assert_eq!(turn.wait().await.expect("outcome").state, RunState::Done);
    handle.stop().await.expect("stop");
}

fn record_golden(name: &str, wire: &serde_json::Value) {
    let rendered = serde_json::to_string_pretty(wire).expect("wire JSON") + "\n";
    if let Some(directory) = std::env::var_os("HAIDER_MONITOR_TEST_EVIDENCE") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("evidence directory");
        std::fs::write(directory.join(format!("{name}.json")), &rendered).expect("wire evidence");
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/monitor-wake")
        .join(format!("{name}.json"));
    if !path.exists() {
        println!("WIRE_GOLDEN_{name}={wire}");
    }
    let expected = std::fs::read_to_string(path).expect("reviewed wire golden");
    assert_eq!(rendered, expected, "{name} outgoing wire golden");
}

fn assert_wire_shapes(wire: &serde_json::Value) {
    let mut pending = std::collections::BTreeSet::new();
    let mut previous = "";
    // Messages combines adjacent equal roles before validating turn alternation.
    let anthropic_turns = effective_turns(&wire["anthropic"]["messages"], "content");
    for message in &anthropic_turns {
        let role = message["role"].as_str().expect("role");
        assert_ne!(role, previous, "Anthropic role alternation");
        previous = role;
        let blocks = message["content"].as_array().expect("content blocks");
        assert!(!blocks.is_empty());
        for block in blocks {
            match block["type"].as_str().expect("block type") {
                "tool_use" => {
                    assert_eq!(role, "assistant");
                    assert!(pending.insert(block["id"].as_str().expect("call id")));
                }
                "tool_result" => {
                    assert_eq!(role, "user");
                    assert!(pending.remove(block["tool_use_id"].as_str().expect("result id")));
                }
                "text" => {
                    assert!(!block["text"].as_str().expect("text").trim().is_empty());
                    if role == "user" {
                        assert!(pending.is_empty(), "wake before results");
                    }
                }
                other => panic!("unexpected monitor block {other}"),
            }
        }
        if role == "user" {
            assert!(pending.is_empty(), "unpaired call");
        }
    }
    assert!(pending.is_empty());
    for family in ["openai", "chat"] {
        let mut pending = std::collections::BTreeSet::new();
        let items = if family == "openai" {
            &wire[family]["input"]
        } else {
            &wire[family]["messages"]
        };
        for item in items.as_array().expect("OpenAI messages") {
            if item["type"] == "function_call" {
                assert!(pending.insert(item["call_id"].as_str().expect("call id")));
            }
            if item["type"] == "function_call_output" {
                assert!(pending.remove(item["call_id"].as_str().expect("result id")));
            }
            if let Some(calls) = item["tool_calls"].as_array() {
                for call in calls {
                    assert!(pending.insert(call["id"].as_str().expect("call id")));
                }
            }
            if item["role"] == "tool" {
                assert!(pending.remove(item["tool_call_id"].as_str().expect("result id")));
            }
            if item["role"] == "user" {
                assert!(pending.is_empty(), "OpenAI user before results");
            }
        }
        assert!(pending.is_empty());
    }
    let mut pending = Vec::new();
    let mut previous = "";
    for message in wire["gemini"]["contents"]
        .as_array()
        .expect("Gemini contents")
    {
        let role = message["role"].as_str().expect("role");
        assert_ne!(previous, role, "Gemini role alternation");
        previous = role;
        for part in message["parts"].as_array().expect("parts") {
            if let Some(call) = part.get("functionCall") {
                assert_eq!(role, "model");
                pending.push(call["name"].as_str().expect("name"));
            }
            if let Some(result) = part.get("functionResponse") {
                assert_eq!(role, "user");
                let name = result["name"].as_str().expect("name");
                let index = pending
                    .iter()
                    .position(|call| *call == name)
                    .expect("paired function response");
                pending.remove(index);
            }
            if let Some(text) = part.get("text") {
                assert!(!text.as_str().expect("text").trim().is_empty());
                if role == "user" {
                    assert!(pending.is_empty());
                }
            }
        }
        if role == "user" {
            assert!(pending.is_empty());
        }
    }
    assert!(pending.is_empty());
}

#[tokio::test]
async fn monitor_wake_after_completed_registration_wire_golden() {
    let mut messages = completed_registration();
    messages.push(haider_provider::Message::user_text(monitor_event(
        r#"{"type":"monitor_event","payload":"file changed"}"#,
    )));
    committed_wake("completed-registration", messages).await;
}

#[tokio::test]
async fn monitor_wake_with_pending_call_wire_golden() {
    let mut messages = completed_registration();
    // The history compiler can retain a tool call whose interrupted run never recorded a result.
    messages.pop();
    messages.push(haider_provider::Message::assistant(vec![Block::ToolCall {
        call_id: "pending".into(),
        name: "inspect".into(),
        args: serde_json::json!({}),
    }]));
    messages.push(haider_provider::Message::user_text(monitor_event(
        r#"{"type":"monitor_event","payload":"file changed"}"#,
    )));
    committed_wake("pending-call", messages).await;
}

#[tokio::test]
async fn monitor_wake_with_fully_redacted_payload_wire_golden() {
    let mut messages = completed_registration();
    messages.push(haider_provider::Message::user_text(""));
    messages.push(haider_provider::Message::user_text(monitor_event(
        r#"{"payload":"[REDACTED:high_entropy]"}"#,
    )));
    committed_wake("fully-redacted", messages).await;
}

#[tokio::test]
async fn two_monitor_wakes_back_to_back_wire_golden() {
    let mut messages = completed_registration();
    for payload in [r#"{"event":1}"#, r#"{"event":2}"#] {
        messages.push(haider_provider::Message::user_text(monitor_event(payload)));
    }
    committed_wake("two-wakes", messages).await;
}

fn effective_turns(messages: &serde_json::Value, content_key: &str) -> Vec<serde_json::Value> {
    let mut turns: Vec<serde_json::Value> = Vec::new();
    for message in messages.as_array().expect("wire message array") {
        if let Some(last) = turns.last_mut()
            && last["role"] == message["role"]
        {
            last[content_key].as_array_mut().expect("content").extend(
                message[content_key]
                    .as_array()
                    .expect("content")
                    .iter()
                    .cloned(),
            );
        } else {
            turns.push(message.clone());
        }
    }
    turns
}
