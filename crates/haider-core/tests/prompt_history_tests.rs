#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    ArtifactReader, MemoryStore, PromptHistoryCompiler, SessionCreateCommand, SqliteStoreHandle,
    StoreHandle,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CompactionIntent, CompactionResume, NodeKind, TreeNode,
};
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, ItemId, NodeId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::provider::{Block, PROVIDER_OPAQUE_EXTENSION_KIND};
use haider_protocol::state::RunState;
use haider_protocol::tool::BoundedResult;
use haider_protocol::verify::VerifyVerdict;
use std::collections::HashMap;

fn envelope(
    session_id: &SessionId,
    run_id: &RunId,
    event_id: &str,
    payload: EventPayload,
    prompt: PromptRender,
) -> haider_protocol::envelope::RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("opaque-history-test"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt,
        },
        payload: serde_json::to_value(payload).expect("payload"),
    }
}

fn node(
    session_id: &SessionId,
    run_id: &RunId,
    id: &str,
    parent: Option<&str>,
    kind: NodeKind,
) -> haider_protocol::envelope::RawEnvelope {
    envelope(
        session_id,
        run_id,
        &format!("commit-{id}"),
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new(id),
            parent: parent.map(NodeId::new),
            kind,
        }),
        PromptRender::Omit,
    )
}

struct TestArtifacts(HashMap<ArtifactRef, Vec<u8>>);

#[async_trait]
impl ArtifactReader for TestArtifacts {
    async fn read_artifact(
        &self,
        artifact: &ArtifactRef,
    ) -> Result<Vec<u8>, haider_protocol::error::HaiderError> {
        self.0.get(artifact).cloned().ok_or_else(|| {
            haider_protocol::error::HaiderError::new(
                haider_protocol::error::ErrorCode::StoreCorrupt,
                format!("missing test artifact {artifact}"),
                false,
            )
        })
    }
}

#[tokio::test]
async fn provider_opaque_extension_rehydrates_for_a_terminal_prior_run() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("opaque-history-session");
    let prior = RunId::new("opaque-prior");
    let current = RunId::new("opaque-current");
    let opaque = serde_json::json!({
        "id": "rs_sanitized",
        "type": "reasoning",
        "encrypted_content": "encrypted-synthetic-continuation",
        "summary": []
    });
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "opaque-prior-user",
            EventPayload::UserMessage {
                text: "first".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "opaque-prior-item",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("opaque-item"),
                item: TurnItem::Extension {
                    kind: PROVIDER_OPAQUE_EXTENSION_KIND.into(),
                    data: serde_json::json!({
                        "provider": "openai",
                        "data": opaque.clone(),
                    }),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "opaque-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "opaque-current-user",
            EventPayload::UserMessage {
                text: "continue".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append opaque history");

    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current)
        .await
        .expect("compile opaque history");

    assert!(messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ProviderOpaque { provider, data }
                    if provider == "openai" && data == &opaque
            )
        })
    }));
}

/// The activation law: tree selection changes which committed fragments are
/// eligible, never how an eligible fragment is encoded for a provider.
///
/// MUTATION CHECK: drop opaque extensions, re-serialize tool exchanges from
/// `NodeKind`, or reorder tool result/call rendering in the tree path.
/// Expected runtime failure: the serialized provider-message bytes differ.
#[tokio::test]
async fn tree_compilation_is_byte_identical_to_journal_rendering() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("tree-equivalence-session");
    let prior = RunId::new("tree-equivalence-prior");
    let current = RunId::new("tree-equivalence-current");
    let opaque = serde_json::json!({
        "id": "rs_exact",
        "type": "reasoning",
        "encrypted_content": "signed-provider-state",
        "summary": [{"type":"summary_text","text":"kept byte-for-byte"}]
    });
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "equivalence-user",
            EventPayload::UserMessage {
                text: "inspect the file".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "n-user",
            None,
            NodeKind::UserTurn {
                text: "inspect the file".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &prior,
            "equivalence-opaque",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("opaque-item"),
                item: TurnItem::Extension {
                    kind: PROVIDER_OPAQUE_EXTENSION_KIND.into(),
                    data: serde_json::json!({
                        "provider": "openai",
                        "data": opaque,
                    }),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "n-opaque",
            Some("n-user"),
            NodeKind::AssistantCommit {
                text: String::new(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &prior,
            "equivalence-result",
            EventPayload::ToolResult {
                call_id: "call-exact".into(),
                result: BoundedResult {
                    preview: "exact\nresult".into(),
                    truncated: true,
                    artifact: None,
                    cursor: Some("cursor-7".into()),
                },
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "equivalence-call",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("tool-item"),
                item: TurnItem::ToolCall {
                    call_id: "call-exact".into(),
                    name: "fs_read".into(),
                    args: serde_json::json!({"path":"a.txt","line":7}),
                    status: ToolStatus::Completed,
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "n-tool",
            Some("n-opaque"),
            NodeKind::ToolExchange {
                tool: "fs_read".into(),
                summary: "not used for provider rendering".into(),
                artifact: None,
            },
        ),
        envelope(
            &session_id,
            &prior,
            "equivalence-assistant",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("assistant-item"),
                item: TurnItem::AgentMessage {
                    text: "The file is valid.".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "n-assistant",
            Some("n-tool"),
            NodeKind::AssistantCommit {
                text: "The file is valid.".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &prior,
            "equivalence-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "equivalence-current-user",
            EventPayload::UserMessage {
                text: "continue".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "n-current",
            Some("n-assistant"),
            NodeKind::UserTurn {
                text: "continue".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append equivalence history");

    let journal = PromptHistoryCompiler::compile_journal(&store, &session_id, None, None, &current)
        .await
        .expect("compile journal oracle");
    let tree = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current)
        .await
        .expect("compile tree projection");

    assert_eq!(
        serde_json::to_vec(&tree).expect("serialize tree prompt"),
        serde_json::to_vec(&journal).expect("serialize journal prompt")
    );
}

/// MUTATION CHECK: retain covered journal fragments after inserting the
/// summary. Expected runtime failure: `old prefix` remains in the prompt or
/// the two restart-equivalent compiles produce different bytes.
#[tokio::test]
async fn compaction_substitutes_summary_and_keeps_only_the_suffix() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("open store");
    let session_id = SessionId::new("compacted-session");
    let first = RunId::new("compacted-first");
    let second = RunId::new("compacted-second");
    let current = RunId::new("compacted-current");
    store
        .create_session(SessionCreateCommand {
            command_id: "create-compacted-session".into(),
            request_digest: "create-compacted-session-digest".into(),
            request_json: r#"{"session":"compacted-session"}"#.into(),
            session_id: session_id.clone(),
            cwd: std::env::current_dir()
                .expect("cwd")
                .to_string_lossy()
                .into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            system_prompt_version: "test-v1".into(),
            event_id: EventId::new("created-compacted-session"),
            device_id: DeviceId::new("compaction-restart-test"),
        })
        .await
        .expect("create compacted session");
    let artifact = store
        .put(b"durable summary".to_vec())
        .await
        .expect("put durable summary");
    let mut events = vec![
        envelope(
            &session_id,
            &first,
            "compacted-old-user",
            EventPayload::UserMessage {
                text: "old prefix".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &first,
            "compact-n1",
            None,
            NodeKind::UserTurn {
                text: "old prefix".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &first,
            "compacted-old-assistant",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("old-answer"),
                item: TurnItem::AgentMessage {
                    text: "old answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &first,
            "compact-n2",
            Some("compact-n1"),
            NodeKind::AssistantCommit {
                text: "old answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &first,
            "compacted-first-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        node(
            &session_id,
            &first,
            "compact-overlay",
            Some("compact-n2"),
            NodeKind::Compaction {
                covers_from: NodeId::new("compact-n1"),
                covers_to: NodeId::new("compact-n2"),
                summary_artifact: artifact.clone(),
                tokens_before: 100,
                tokens_after: 12,
                resume_cause: CompactionResume::ManualIdle,
            },
        ),
        envelope(
            &session_id,
            &second,
            "compacted-suffix-user",
            EventPayload::UserMessage {
                text: "suffix user".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &second,
            "compact-n3",
            Some("compact-overlay"),
            NodeKind::UserTurn {
                text: "suffix user".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &second,
            "compacted-suffix-assistant",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("suffix-answer"),
                item: TurnItem::AgentMessage {
                    text: "suffix answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &second,
            "compact-n4",
            Some("compact-n3"),
            NodeKind::AssistantCommit {
                text: "suffix answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &second,
            "compacted-second-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "compacted-current-user",
            EventPayload::UserMessage {
                text: "current".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "compact-n5",
            Some("compact-n4"),
            NodeKind::UserTurn {
                text: "current".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append compacted history");
    let first_compile = PromptHistoryCompiler::compile_with_artifacts(
        &store,
        &store,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("compile compacted projection");
    store.close().await.expect("close before restart");
    let restarted = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store");
    let restarted_compile = PromptHistoryCompiler::compile_with_artifacts(
        &restarted,
        &restarted,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("compile after restart-equivalent replay");
    let text = first_compile
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        text,
        ["durable summary", "suffix user", "suffix answer", "current"]
    );
    assert_eq!(
        serde_json::to_vec(&first_compile).expect("serialize first compile"),
        serde_json::to_vec(&restarted_compile).expect("serialize restarted compile")
    );
    restarted.close().await.expect("close restarted store");
}

/// MUTATION CHECK: make intent itself switch the projection. Expected runtime
/// failure: the original prefix disappears before a compaction node commits.
/// Removing the durable intent also fails the explicit marker assertion.
#[tokio::test]
async fn crash_after_intent_never_half_substitutes_the_prompt() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("intent-crash-session");
    let prior = RunId::new("intent-prior");
    let compaction = RunId::new("intent-compaction");
    let current = RunId::new("intent-current");
    let intent = CompactionIntent {
        operation_id: "crashed-compaction".into(),
        covers_from: NodeId::new("intent-n1"),
        covers_to: NodeId::new("intent-n2"),
        resume_cause: CompactionResume::ManualIdle,
    };
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "intent-old-user",
            EventPayload::UserMessage {
                text: "original prefix".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "intent-n1",
            None,
            NodeKind::UserTurn {
                text: "original prefix".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &prior,
            "intent-old-answer",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("intent-answer"),
                item: TurnItem::AgentMessage {
                    text: "original answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "intent-n2",
            Some("intent-n1"),
            NodeKind::AssistantCommit {
                text: "original answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &prior,
            "intent-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &compaction,
            "intent-marker",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("intent-marker-item"),
                item: TurnItem::Extension {
                    kind: COMPACTION_INTENT_EXTENSION_KIND.into(),
                    data: serde_json::to_value(intent).expect("intent"),
                },
            }),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &compaction,
            "intent-compacting",
            EventPayload::RunState(RunState::Compacting),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "intent-current-user",
            EventPayload::UserMessage {
                text: "after restart".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "intent-current-node",
            Some("intent-n2"),
            NodeKind::UserTurn {
                text: "after restart".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append intent crash history");

    let compiled = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current)
        .await
        .expect("compile original projection");
    let text = compiled
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        text,
        ["original prefix", "original answer", "after restart"]
    );
    assert_eq!(
        store
            .events(&session_id)
            .await
            .iter()
            .filter(|event| matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Extension { ref kind, .. },
                    ..
                })) if kind == COMPACTION_INTENT_EXTENSION_KIND
            ))
            .count(),
        1
    );
}

/// A committed projection switch may never silently resurrect its covered
/// prefix when CAS is missing or corrupt.
#[tokio::test]
async fn missing_compaction_artifact_is_store_corruption() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("missing-summary-session");
    let prior = RunId::new("missing-summary-prior");
    let current = RunId::new("missing-summary-current");
    let missing = ArtifactRef::new("blake3:missing-summary");
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "missing-user",
            EventPayload::UserMessage {
                text: "covered".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "missing-n1",
            None,
            NodeKind::UserTurn {
                text: "covered".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &prior,
            "missing-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        node(
            &session_id,
            &prior,
            "missing-overlay",
            Some("missing-n1"),
            NodeKind::Compaction {
                covers_from: NodeId::new("missing-n1"),
                covers_to: NodeId::new("missing-n1"),
                summary_artifact: missing,
                tokens_before: 10,
                tokens_after: 2,
                resume_cause: CompactionResume::ManualIdle,
            },
        ),
        envelope(
            &session_id,
            &current,
            "missing-current-user",
            EventPayload::UserMessage {
                text: "current".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "missing-current-node",
            Some("missing-overlay"),
            NodeKind::UserTurn {
                text: "current".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append missing artifact history");
    let artifacts = TestArtifacts(HashMap::new());

    let error = PromptHistoryCompiler::compile_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect_err("missing summary must fail closed");
    assert_eq!(error.code, haider_protocol::error::ErrorCode::StoreCorrupt);
}

// ── The two laws the W7a rewrite DELETED, restored from a8418a0^ ──
// (the daemond seam-sweep manifest caught the first by name; the second
// had no manifest pin and vanished silently — both re-pinned here, and
// the manifest coordinate now points at this file.)

/// MUTATION CHECK (restored): reorder the compiled tool result before its
/// completed call. Expected runtime failure: the position assertion — a
/// provider rejects a tool result preceding its call.
#[tokio::test]
async fn tool_result_is_presented_after_its_completed_tool_call() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("history-session");
    let prior = RunId::new("prior-run");
    let current = RunId::new("current-run");
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "prior-user",
            EventPayload::UserMessage {
                text: "read".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "prior-result",
            EventPayload::ToolResult {
                call_id: "call-1".into(),
                result: BoundedResult {
                    preview: "contents".into(),
                    truncated: false,
                    artifact: None,
                    cursor: None,
                },
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "prior-call",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("item-1"),
                item: TurnItem::ToolCall {
                    call_id: "call-1".into(),
                    name: "fs_read".into(),
                    args: serde_json::json!({"path":"note.txt"}),
                    status: ToolStatus::Completed,
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "current-user",
            EventPayload::UserMessage {
                text: "continue".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append history");
    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current)
        .await
        .expect("compile");
    let call = messages
        .iter()
        .position(|message| {
            message.blocks.iter().any(
                |block| matches!(block, Block::ToolCall { call_id, .. } if call_id == "call-1"),
            )
        })
        .expect("tool call");
    let result = messages
        .iter()
        .position(|message| message.tool_result_for("call-1").is_some())
        .expect("tool result");
    assert_eq!(result, call + 1);
}

/// Fable D2-5 pin (restored): only Done history in the requested
/// branch/agent scope may reach the provider; prompt-marked partial
/// output is still excluded.
/// MUTATION CHECK: drop the scope filter (or the terminal-run gate) in
/// the compiler. Expected runtime failure: the rendered text below gains
/// "wrong branch"/"wrong agent"/"partial output".
#[tokio::test]
async fn branch_agent_and_nonterminal_history_are_excluded_structurally() {
    use haider_protocol::ids::{AgentId, BranchId};
    let store = MemoryStore::new();
    let session_id = SessionId::new("scoped-history");
    let branch = BranchId::new("branch-a");
    let other_branch = BranchId::new("branch-b");
    let agent = AgentId::new("agent-a");
    let other_agent = AgentId::new("agent-b");
    let matching = RunId::new("matching");
    let wrong_branch = RunId::new("wrong-branch");
    let wrong_agent = RunId::new("wrong-agent");
    let interrupted = RunId::new("interrupted");
    let current = RunId::new("current");
    let scoped =
        |mut raw: haider_protocol::envelope::RawEnvelope, branch: &BranchId, agent: &AgentId| {
            raw.branch_id = Some(branch.clone());
            raw.agent_id = Some(agent.clone());
            raw
        };
    let user = |run: &RunId, id: &str, text: &str| {
        envelope(
            &session_id,
            run,
            id,
            EventPayload::UserMessage {
                text: text.into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        )
    };
    let done = |run: &RunId, id: &str| {
        envelope(
            &session_id,
            run,
            id,
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        )
    };
    let mut events = vec![
        scoped(
            user(&matching, "matching-user", "matching"),
            &branch,
            &agent,
        ),
        scoped(done(&matching, "matching-done"), &branch, &agent),
        scoped(
            user(&wrong_branch, "wrong-branch-user", "wrong branch"),
            &other_branch,
            &agent,
        ),
        scoped(
            done(&wrong_branch, "wrong-branch-done"),
            &other_branch,
            &agent,
        ),
        scoped(
            user(&wrong_agent, "wrong-agent-user", "wrong agent"),
            &branch,
            &other_agent,
        ),
        scoped(
            done(&wrong_agent, "wrong-agent-done"),
            &branch,
            &other_agent,
        ),
        scoped(
            user(&interrupted, "interrupted-user", "partial"),
            &branch,
            &agent,
        ),
        scoped(
            envelope(
                &session_id,
                &interrupted,
                "interrupted-stream",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("partial-item"),
                    item: TurnItem::AgentMessage {
                        text: "partial output".into(),
                    },
                }),
                PromptRender::Verbatim,
            ),
            &branch,
            &agent,
        ),
        scoped(user(&current, "current-user", "current"), &branch, &agent),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append scoped history");

    let messages =
        PromptHistoryCompiler::compile(&store, &session_id, Some(&branch), Some(&agent), &current)
            .await
            .expect("compile");
    let rendered = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rendered, vec!["matching", "current"]);

    // B2a named-ref phase: preserve the exact W7a assertion above, then
    // prove that a declared branch sees its source prefix only through the
    // fork coordinate and renders each ancestor fragment under its owner.
    use haider_protocol::branch::{BranchCreated, BranchDescriptor};
    let tree_store = MemoryStore::new();
    let tree_session = SessionId::new("named-lineage-history");
    let source_run = RunId::new("source-prefix");
    let source_suffix_run = RunId::new("source-suffix");
    let matching = RunId::new("named-matching");
    let interrupted = RunId::new("named-interrupted");
    let wrong_agent = RunId::new("named-wrong-agent");
    let sibling = RunId::new("named-sibling");
    let current = RunId::new("named-current");
    let source_node = NodeId::new("source-prefix-node");
    let matching_node = NodeId::new("named-matching-node");
    let interrupted_node = NodeId::new("named-interrupted-node");
    let current_node = NodeId::new("named-current-node");
    let tree_scoped = |mut raw: haider_protocol::envelope::RawEnvelope,
                       branch: Option<&BranchId>,
                       owner: &AgentId| {
        raw.branch_id = branch.cloned();
        raw.agent_id = Some(owner.clone());
        raw
    };
    let tree_user = |run: &RunId, id: &str, text: &str| {
        envelope(
            &tree_session,
            run,
            id,
            EventPayload::UserMessage {
                text: text.into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        )
    };
    let tree_done = |run: &RunId, id: &str| {
        envelope(
            &tree_session,
            run,
            id,
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        )
    };
    let tree_node = |run: &RunId, id: &str, parent: Option<&NodeId>| {
        node(
            &tree_session,
            run,
            id,
            parent.map(NodeId::as_str),
            NodeKind::UserTurn {
                text: id.into(),
                attachments: Vec::new(),
            },
        )
    };
    let mut tree_events = vec![
        tree_scoped(
            tree_user(&source_run, "source-user", "source prefix"),
            None,
            &agent,
        ),
        tree_scoped(
            tree_node(&source_run, source_node.as_str(), None),
            None,
            &agent,
        ),
        tree_scoped(tree_done(&source_run, "source-done"), None, &agent),
        tree_scoped(
            tree_user(&source_suffix_run, "source-suffix-user", "source suffix"),
            None,
            &agent,
        ),
        tree_scoped(
            tree_node(&source_suffix_run, "source-suffix-node", Some(&source_node)),
            None,
            &agent,
        ),
        tree_scoped(
            tree_done(&source_suffix_run, "source-suffix-done"),
            None,
            &agent,
        ),
        tree_scoped(
            tree_user(&matching, "named-matching-user", "branch matching"),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            tree_node(&matching, matching_node.as_str(), Some(&source_node)),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            tree_done(&matching, "named-matching-done"),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            tree_user(&interrupted, "named-partial-user", "partial input"),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            tree_node(
                &interrupted,
                interrupted_node.as_str(),
                Some(&matching_node),
            ),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            envelope(
                &tree_session,
                &interrupted,
                "named-partial-output",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("named-partial-item"),
                    item: TurnItem::AgentMessage {
                        text: "partial output".into(),
                    },
                }),
                PromptRender::Verbatim,
            ),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            tree_user(&wrong_agent, "named-wrong-agent-user", "wrong agent"),
            Some(&branch),
            &other_agent,
        ),
        tree_scoped(
            tree_node(&wrong_agent, "named-wrong-agent-node", Some(&matching_node)),
            Some(&branch),
            &other_agent,
        ),
        tree_scoped(
            tree_done(&wrong_agent, "named-wrong-agent-done"),
            Some(&branch),
            &other_agent,
        ),
        tree_scoped(
            tree_user(&sibling, "named-sibling-user", "sibling"),
            Some(&other_branch),
            &agent,
        ),
        tree_scoped(
            tree_node(&sibling, "named-sibling-node", Some(&source_node)),
            Some(&other_branch),
            &agent,
        ),
        tree_scoped(
            tree_done(&sibling, "named-sibling-done"),
            Some(&other_branch),
            &agent,
        ),
        tree_scoped(
            tree_user(&current, "named-current-user", "branch current"),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            tree_node(&current, current_node.as_str(), Some(&interrupted_node)),
            Some(&branch),
            &agent,
        ),
    ];
    let created_seq = u64::try_from(tree_events.len() + 1).expect("created seq");
    tree_events.push(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("named-branch-created"),
        seq: 0,
        session_id: tree_session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("opaque-history-test"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: BranchCreated {
            branch: BranchDescriptor {
                branch_id: branch.clone(),
                name: "Plan A".into(),
                source_branch_id: None,
                fork_node_id: source_node,
                fork_seq: 2,
                created_seq,
                created_at_ms: 1,
                head_node_id: current_node,
                head_seq: 20,
            },
        }
        .to_payload_value()
        .expect("branch payload"),
    });
    StoreHandle::append(&tree_store, &mut tree_events)
        .await
        .expect("append named lineage");
    let named_messages = PromptHistoryCompiler::compile(
        &tree_store,
        &tree_session,
        Some(&branch),
        Some(&agent),
        &current,
    )
    .await
    .expect("compile named lineage");
    let named_rendered = named_messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        named_rendered,
        vec!["source prefix", "branch matching", "branch current"]
    );
}

/// MUTATION CHECK: select compaction overlays by session order instead of the
/// requested branch's immutable ancestry. Expected RUNTIME failure: the fork
/// before the compaction renders `source summary`, or the fork after it
/// resurrects `original prefix`/`original answer`.
#[tokio::test]
async fn named_forks_before_and_after_compaction_diverge() {
    use haider_protocol::branch::{BranchCreated, BranchDescriptor};
    use haider_protocol::ids::BranchId;

    let store = MemoryStore::new();
    let session_id = SessionId::new("branch-compaction-divergence");
    let source_run = RunId::new("divergence-source");
    let compaction_run = RunId::new("divergence-compaction");
    let before_run = RunId::new("divergence-before");
    let after_run = RunId::new("divergence-after");
    let before_branch = BranchId::new("fork-before-compaction");
    let after_branch = BranchId::new("fork-after-compaction");
    let summary_artifact = ArtifactRef::new("summary-artifact");

    let scoped = |mut raw: haider_protocol::envelope::RawEnvelope, branch_id: Option<&BranchId>| {
        raw.branch_id = branch_id.cloned();
        raw
    };
    let mut events = vec![
        envelope(
            &session_id,
            &source_run,
            "divergence-source-user",
            EventPayload::UserMessage {
                text: "original prefix".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &source_run,
            "divergence-n1",
            None,
            NodeKind::UserTurn {
                text: "original prefix".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &source_run,
            "divergence-source-answer",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("divergence-source-answer-item"),
                item: TurnItem::AgentMessage {
                    text: "original answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &source_run,
            "divergence-n2",
            Some("divergence-n1"),
            NodeKind::AssistantCommit {
                text: "original answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &source_run,
            "divergence-source-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        node(
            &session_id,
            &compaction_run,
            "divergence-summary-node",
            Some("divergence-n2"),
            NodeKind::Compaction {
                covers_from: NodeId::new("divergence-n1"),
                covers_to: NodeId::new("divergence-n2"),
                summary_artifact: summary_artifact.clone(),
                tokens_before: 100,
                tokens_after: 8,
                resume_cause: CompactionResume::ManualIdle,
            },
        ),
        scoped(
            envelope(
                &session_id,
                &before_run,
                "divergence-before-user",
                EventPayload::UserMessage {
                    text: "before fork turn".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
            Some(&before_branch),
        ),
        scoped(
            node(
                &session_id,
                &before_run,
                "divergence-before-node",
                Some("divergence-n2"),
                NodeKind::UserTurn {
                    text: "before fork turn".into(),
                    attachments: Vec::new(),
                },
            ),
            Some(&before_branch),
        ),
        scoped(
            envelope(
                &session_id,
                &after_run,
                "divergence-after-user",
                EventPayload::UserMessage {
                    text: "after fork turn".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
            Some(&after_branch),
        ),
        scoped(
            node(
                &session_id,
                &after_run,
                "divergence-after-node",
                Some("divergence-summary-node"),
                NodeKind::UserTurn {
                    text: "after fork turn".into(),
                    attachments: Vec::new(),
                },
            ),
            Some(&after_branch),
        ),
    ];
    for (event_id, descriptor) in [
        (
            "divergence-before-created",
            BranchDescriptor {
                branch_id: before_branch.clone(),
                name: "Before compaction".into(),
                source_branch_id: None,
                fork_node_id: NodeId::new("divergence-n2"),
                fork_seq: 4,
                created_seq: 11,
                created_at_ms: 1,
                head_node_id: NodeId::new("divergence-before-node"),
                head_seq: 8,
            },
        ),
        (
            "divergence-after-created",
            BranchDescriptor {
                branch_id: after_branch.clone(),
                name: "After compaction".into(),
                source_branch_id: None,
                fork_node_id: NodeId::new("divergence-summary-node"),
                fork_seq: 6,
                created_seq: 12,
                created_at_ms: 2,
                head_node_id: NodeId::new("divergence-after-node"),
                head_seq: 10,
            },
        ),
    ] {
        events.push(EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(event_id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("divergence-device"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: BranchCreated { branch: descriptor }
                .to_payload_value()
                .expect("branch payload"),
        });
    }
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append divergence history");
    let artifacts = TestArtifacts(HashMap::from([(
        summary_artifact,
        b"source summary".to_vec(),
    )]));

    let before = PromptHistoryCompiler::compile_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        Some(&before_branch),
        None,
        &before_run,
    )
    .await
    .expect("compile pre-compaction fork");
    let after = PromptHistoryCompiler::compile_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        Some(&after_branch),
        None,
        &after_run,
    )
    .await
    .expect("compile post-compaction fork");
    let text = |messages: &[haider_provider::Message]| {
        messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(
        text(&before),
        ["original prefix", "original answer", "before fork turn"]
    );
    assert_eq!(text(&after), ["source summary", "after fork turn"]);
}

/// MUTATION CHECK: flatten nested named refs to the leaf branch, reuse only
/// the root fork ceiling, or derive a virgin branch head from later source
/// traffic. Expected RUNTIME failure: B leaks either source suffix, or the
/// virgin branch moves past its exact fork node.
#[tokio::test]
async fn nested_lineage_uses_every_owner_ceiling_and_virgin_head() {
    use haider_protocol::branch::{BranchCreated, BranchDescriptor};
    use haider_protocol::ids::BranchId;

    let store = MemoryStore::new();
    let session_id = SessionId::new("nested-lineage");
    let main = RunId::new("nested-main");
    let main_suffix = RunId::new("nested-main-suffix");
    let branch_a_run = RunId::new("nested-a");
    let branch_a_suffix = RunId::new("nested-a-suffix");
    let branch_b_run = RunId::new("nested-b");
    let branch_a = BranchId::new("nested-branch-a");
    let branch_b = BranchId::new("nested-branch-b");
    let virgin = BranchId::new("nested-virgin");
    let scoped = |mut raw: haider_protocol::envelope::RawEnvelope, branch_id: Option<&BranchId>| {
        raw.branch_id = branch_id.cloned();
        raw
    };
    let user = |run: &RunId, id: &str, text: &str| {
        envelope(
            &session_id,
            run,
            id,
            EventPayload::UserMessage {
                text: text.into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        )
    };
    let done = |run: &RunId, id: &str| {
        envelope(
            &session_id,
            run,
            id,
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        )
    };
    let user_node = |run: &RunId, id: &str, parent: Option<&str>| {
        node(
            &session_id,
            run,
            id,
            parent,
            NodeKind::UserTurn {
                text: id.into(),
                attachments: Vec::new(),
            },
        )
    };
    let mut events = vec![
        user(&main, "nested-main-user", "main prefix"),
        user_node(&main, "nested-main-node", None),
        done(&main, "nested-main-done"),
        user(&main_suffix, "nested-main-suffix-user", "main suffix"),
        user_node(
            &main_suffix,
            "nested-main-suffix-node",
            Some("nested-main-node"),
        ),
        done(&main_suffix, "nested-main-suffix-done"),
        scoped(
            user(&branch_a_run, "nested-a-user", "A prefix"),
            Some(&branch_a),
        ),
        scoped(
            user_node(&branch_a_run, "nested-a-node", Some("nested-main-node")),
            Some(&branch_a),
        ),
        scoped(done(&branch_a_run, "nested-a-done"), Some(&branch_a)),
        scoped(
            user(&branch_a_suffix, "nested-a-suffix-user", "A suffix"),
            Some(&branch_a),
        ),
        scoped(
            user_node(
                &branch_a_suffix,
                "nested-a-suffix-node",
                Some("nested-a-node"),
            ),
            Some(&branch_a),
        ),
        scoped(
            done(&branch_a_suffix, "nested-a-suffix-done"),
            Some(&branch_a),
        ),
        scoped(
            user(&branch_b_run, "nested-b-user", "B current"),
            Some(&branch_b),
        ),
        scoped(
            user_node(&branch_b_run, "nested-b-node", Some("nested-a-node")),
            Some(&branch_b),
        ),
    ];
    for (event_id, descriptor) in [
        (
            "nested-a-created",
            BranchDescriptor {
                branch_id: branch_a.clone(),
                name: "A".into(),
                source_branch_id: None,
                fork_node_id: NodeId::new("nested-main-node"),
                fork_seq: 2,
                created_seq: 15,
                created_at_ms: 1,
                head_node_id: NodeId::new("nested-a-suffix-node"),
                head_seq: 11,
            },
        ),
        (
            "nested-b-created",
            BranchDescriptor {
                branch_id: branch_b.clone(),
                name: "B".into(),
                source_branch_id: Some(branch_a.clone()),
                fork_node_id: NodeId::new("nested-a-node"),
                fork_seq: 8,
                created_seq: 16,
                created_at_ms: 2,
                head_node_id: NodeId::new("nested-b-node"),
                head_seq: 14,
            },
        ),
        (
            "nested-virgin-created",
            BranchDescriptor {
                branch_id: virgin.clone(),
                name: "Virgin".into(),
                source_branch_id: None,
                fork_node_id: NodeId::new("nested-main-node"),
                fork_seq: 2,
                created_seq: 17,
                created_at_ms: 3,
                head_node_id: NodeId::new("nested-main-node"),
                head_seq: 2,
            },
        ),
    ] {
        events.push(EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(event_id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("nested-device"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: BranchCreated { branch: descriptor }
                .to_payload_value()
                .expect("branch payload"),
        });
    }
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append nested lineage");

    let nested =
        PromptHistoryCompiler::compile(&store, &session_id, Some(&branch_b), None, &branch_b_run)
            .await
            .expect("compile nested branch");
    let text = |messages: &[haider_provider::Message]| {
        messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(text(&nested), ["main prefix", "A prefix", "B current"]);
    assert_eq!(
        PromptHistoryCompiler::latest_head(&store, &session_id, Some(&virgin), None)
            .await
            .expect("virgin latest head"),
        Some(NodeId::new("nested-main-node"))
    );
    let virgin_prompt = PromptHistoryCompiler::compile_idle_with_artifacts(
        &store,
        &TestArtifacts(HashMap::new()),
        &session_id,
        Some(&virgin),
        None,
    )
    .await
    .expect("compile virgin fork");
    assert_eq!(text(&virgin_prompt), ["main prefix"]);
}
