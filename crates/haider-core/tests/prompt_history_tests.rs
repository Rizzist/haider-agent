#![allow(clippy::expect_used)]

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_core::{
    ArtifactReader, CommittedRange, MemoryStore, PromptCompactionPlanRequest,
    PromptHistoryCompiler, SessionCreateCommand, SessionProjectionCheckpoint, SqliteStoreHandle,
    StoreHandle, USER_COMMAND_OUTPUT_PREVIEW_BYTES,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CompactionIntent, CompactionResume, NodeKind, TreeNode,
};
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, DeviceId, EventId, ItemId, NodeId, RunId, SessionId,
};
use haider_protocol::item::{
    CommandExecutionOrigin, ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem,
    USER_COMMAND_ORIGIN_EXTENSION_KIND, UserCommandOriginV1,
};
use haider_protocol::provider::{Block, PROVIDER_OPAQUE_EXTENSION_KIND};
use haider_protocol::state::RunState;
use haider_protocol::tool::{AttachmentBlock, BoundedResult};
use haider_protocol::verify::VerifyVerdict;
use haider_provider::{Message, MessageRole};
use std::collections::HashMap;
use std::sync::Mutex;

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

struct CountingArtifacts {
    values: HashMap<ArtifactRef, Vec<u8>>,
    reads: Mutex<Vec<ArtifactRef>>,
}

impl CountingArtifacts {
    fn read_count(&self, artifact: &ArtifactRef) -> usize {
        self.reads
            .lock()
            .expect("artifact read ledger")
            .iter()
            .filter(|read| *read == artifact)
            .count()
    }
}

#[async_trait]
impl ArtifactReader for CountingArtifacts {
    async fn read_artifact(
        &self,
        artifact: &ArtifactRef,
    ) -> Result<Vec<u8>, haider_protocol::error::HaiderError> {
        self.reads
            .lock()
            .expect("artifact read ledger")
            .push(artifact.clone());
        self.values.get(artifact).cloned().ok_or_else(|| {
            haider_protocol::error::HaiderError::new(
                haider_protocol::error::ErrorCode::StoreCorrupt,
                format!("missing counted artifact {artifact}"),
                false,
            )
        })
    }
}

struct RecordingStore<'a, S: ?Sized> {
    inner: &'a S,
    reads: Mutex<Vec<u64>>,
    lineage_reads: Mutex<Vec<Option<BranchId>>>,
    loaded_checkpoints: Mutex<Vec<SessionProjectionCheckpoint>>,
    checkpoint_override: Option<SessionProjectionCheckpoint>,
}

impl<'a, S: ?Sized> RecordingStore<'a, S> {
    fn new(inner: &'a S) -> Self {
        Self {
            inner,
            reads: Mutex::new(Vec::new()),
            lineage_reads: Mutex::new(Vec::new()),
            loaded_checkpoints: Mutex::new(Vec::new()),
            checkpoint_override: None,
        }
    }

    fn with_checkpoint(inner: &'a S, checkpoint: SessionProjectionCheckpoint) -> Self {
        Self {
            inner,
            reads: Mutex::new(Vec::new()),
            lineage_reads: Mutex::new(Vec::new()),
            loaded_checkpoints: Mutex::new(Vec::new()),
            checkpoint_override: Some(checkpoint),
        }
    }

    fn read_cursors(&self) -> Vec<u64> {
        self.reads.lock().expect("read ledger").clone()
    }

    fn loaded_checkpoints(&self) -> Vec<SessionProjectionCheckpoint> {
        self.loaded_checkpoints
            .lock()
            .expect("checkpoint ledger")
            .clone()
    }

    fn lineage_read_count(&self) -> usize {
        self.lineage_reads.lock().expect("lineage ledger").len()
    }
}

#[async_trait]
impl<S: StoreHandle + ?Sized> StoreHandle for RecordingStore<'_, S> {
    async fn append(
        &self,
        envelopes: &mut [haider_protocol::envelope::RawEnvelope],
    ) -> Result<CommittedRange, haider_protocol::error::HaiderError> {
        StoreHandle::append(self.inner, envelopes).await
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<haider_protocol::envelope::RawEnvelope>, haider_protocol::error::HaiderError>
    {
        self.reads.lock().expect("read ledger").push(since_seq);
        StoreHandle::read(self.inner, session_id, since_seq, limit).await
    }

    async fn latest_seq(
        &self,
        session_id: &SessionId,
    ) -> Result<u64, haider_protocol::error::HaiderError> {
        StoreHandle::latest_seq(self.inner, session_id).await
    }

    async fn projection_checkpoint(
        &self,
        session_id: &SessionId,
        projection: &str,
        timeline_key: &str,
    ) -> Result<Option<SessionProjectionCheckpoint>, haider_protocol::error::HaiderError> {
        let loaded = if let Some(checkpoint) = &self.checkpoint_override {
            Some(checkpoint.clone())
        } else {
            StoreHandle::projection_checkpoint(self.inner, session_id, projection, timeline_key)
                .await?
        };
        if let Some(checkpoint) = &loaded {
            self.loaded_checkpoints
                .lock()
                .expect("checkpoint ledger")
                .push(checkpoint.clone());
        }
        Ok(loaded)
    }

    async fn put_projection_checkpoint(
        &self,
        checkpoint: SessionProjectionCheckpoint,
    ) -> Result<(), haider_protocol::error::HaiderError> {
        StoreHandle::put_projection_checkpoint(self.inner, checkpoint).await
    }

    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, haider_protocol::error::HaiderError> {
        self.lineage_reads
            .lock()
            .expect("lineage ledger")
            .push(branch_id.cloned());
        StoreHandle::branch_lineage(self.inner, session_id, branch_id).await
    }
}

#[tokio::test]
async fn same_event_log_compiles_to_byte_identical_message_bytes() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("deterministic-prompt-history-session");
    let run_id = RunId::new("deterministic-prompt-history-run");
    let mut events = vec![
        envelope(
            &session_id,
            &run_id,
            "deterministic-initial-user",
            EventPayload::UserMessage {
                text: "Inspect the cache projection.".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &run_id,
            "deterministic-steer-user",
            EventPayload::UserMessage {
                text: "Also verify byte-for-byte determinism.".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append deterministic event log");

    let first = PromptHistoryCompiler::compile(&store, &session_id, None, None, &run_id)
        .await
        .expect("first independent compile");
    let second = PromptHistoryCompiler::compile(&store, &session_id, None, None, &run_id)
        .await
        .expect("second independent compile");

    assert_eq!(
        serde_json::to_vec(&first).expect("serialize first projection"),
        serde_json::to_vec(&second).expect("serialize second projection")
    );
}

/// MUTATION CHECK: restore the current-run `!current_user_seen` filter.
/// Expected RUNTIME failure: the durable mid-round STEER disappears from the
/// restarted provider prompt even though its acceptance committed.
#[tokio::test]
async fn current_run_recovery_keeps_every_durable_steer_message() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("current-steer-history-session");
    let run_id = RunId::new("current-steer-history-run");
    let mut events = vec![
        envelope(
            &session_id,
            &run_id,
            "current-steer-initial",
            EventPayload::UserMessage {
                text: "inspect the parser".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &run_id,
            "current-steer-mid-round",
            EventPayload::UserMessage {
                text: "also reproduce the Unicode boundary failure".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append current-run steer history");

    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &run_id)
        .await
        .expect("compile restarted current run");
    let text = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        text,
        [
            "inspect the parser",
            "also reproduce the Unicode boundary failure"
        ]
    );
}

/// Direct user commands are prior user actions, not assistant tool calls. The
/// committed marker, mixed byte output, and terminal CommandExecution must
/// become one labeled user-role record before the next accepted user turn.
/// MUTATION CHECK: omit the origin marker, command-output collector, or
/// user-role shaping. Expected RUNTIME failure: the first prompt record is
/// absent, loses stderr, or is no longer labeled `origin: user_command`.
#[tokio::test]
async fn user_command_and_output_reach_the_next_turn_as_one_labeled_record() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("user-command-history-session");
    let shell_run = RunId::new("user-command-shell-run");
    let current_run = RunId::new("user-command-current-run");
    let command_item = ItemId::new("user-command-item");
    let origin = UserCommandOriginV1 {
        origin: CommandExecutionOrigin::UserCommand,
        command_item_id: command_item.clone(),
        call_id: "shell-command".into(),
    };
    let mut events = vec![
        envelope(
            &session_id,
            &shell_run,
            "user-command-origin",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("user-command-origin-item"),
                item: origin.extension_item().expect("origin item"),
            }),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &shell_run,
            "user-command-stdout",
            EventPayload::Item(ItemEvent::Delta {
                item_id: command_item.clone(),
                delta: ItemDelta::CommandOutput {
                    stream: OutputStream::Stdout,
                    chunk_b64: BASE64.encode("héllo\n"),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "user-command-stderr",
            EventPayload::Item(ItemEvent::Delta {
                item_id: command_item.clone(),
                delta: ItemDelta::CommandOutput {
                    stream: OutputStream::Stderr,
                    chunk_b64: BASE64.encode("warning\n"),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "user-command-completed",
            EventPayload::Item(ItemEvent::Completed {
                item_id: command_item,
                item: TurnItem::CommandExecution {
                    call_id: "shell-command".into(),
                    command: "printf héllo; printf warning >&2".into(),
                    status: ToolStatus::Completed,
                    exit_code: Some(0),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "user-command-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current_run,
            "user-command-next-turn",
            EventPayload::UserMessage {
                text: "explain that result".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append user-command history");

    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current_run)
        .await
        .expect("compile next-turn history");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, haider_provider::MessageRole::User);
    let Block::Text { text } = &messages[0].blocks[0] else {
        panic!("user command must be provider-portable text")
    };
    assert!(text.contains("origin: user_command"));
    assert!(text.contains("printf héllo; printf warning >&2"));
    assert!(text.contains("[stdout]\\nhéllo\\n\\n[stderr]\\nwarning\\n"));
    assert!(text.contains("status: completed"));
    assert!(text.contains("exit_code: 0"));
    assert_eq!(
        messages[1],
        haider_provider::Message::user_text("explain that result")
    );
}

#[tokio::test]
async fn user_command_output_keeps_raw_delta_but_models_head_marker_and_failure_tail() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("user-command-elision-session");
    let shell_run = RunId::new("user-command-elision-shell");
    let current_run = RunId::new("user-command-elision-current");
    let command_item = ItemId::new("user-command-elision-item");
    let raw = format!(
        "HEAD: cargo test --locked\n{}TAIL: linker failed with exit 1\n",
        (0..2_000)
            .map(|index| format!("progress line {index}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let origin = UserCommandOriginV1 {
        origin: CommandExecutionOrigin::UserCommand,
        command_item_id: command_item.clone(),
        call_id: "shell-elision".into(),
    };
    let mut events = vec![
        envelope(
            &session_id,
            &shell_run,
            "user-command-elision-origin",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("user-command-elision-origin-item"),
                item: origin.extension_item().expect("origin item"),
            }),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &shell_run,
            "user-command-elision-output",
            EventPayload::Item(ItemEvent::Delta {
                item_id: command_item.clone(),
                delta: ItemDelta::CommandOutput {
                    stream: OutputStream::Stdout,
                    chunk_b64: BASE64.encode(&raw),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "user-command-elision-completed",
            EventPayload::Item(ItemEvent::Completed {
                item_id: command_item,
                item: TurnItem::CommandExecution {
                    call_id: "shell-elision".into(),
                    command: "cargo test --locked".into(),
                    status: ToolStatus::Failed,
                    exit_code: Some(1),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "user-command-elision-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current_run,
            "user-command-elision-next",
            EventPayload::UserMessage {
                text: "diagnose it".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append user command history");

    let stored = StoreHandle::read(&store, &session_id, 0, 64)
        .await
        .expect("read raw command journal");
    let stored_chunk = stored
        .into_iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
        .find_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Delta {
                delta: ItemDelta::CommandOutput { chunk_b64, .. },
                ..
            }) => Some(chunk_b64),
            _ => None,
        })
        .expect("durable command delta");
    assert_eq!(
        BASE64.decode(stored_chunk).expect("raw base64"),
        raw.as_bytes()
    );

    let first = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current_run)
        .await
        .expect("compile compact command view");
    let second = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current_run)
        .await
        .expect("replay compact command view");
    assert_eq!(first, second, "replay must reproduce identical elision");
    let Block::Text { text } = &first[0].blocks[0] else {
        panic!("user command compiles to portable text");
    };
    assert!(text.contains("HEAD: cargo test --locked"));
    assert!(text.contains("TAIL: linker failed with exit 1"));
    let output_json = text
        .lines()
        .find_map(|line| line.strip_prefix("output_json (stdout/stderr in capture order): "))
        .expect("portable output JSON string");
    let output_preview: String =
        serde_json::from_str(output_json).expect("decode portable output JSON string");
    assert!(output_preview.contains("\"haider_elision_v1\""));
    assert!(output_preview.contains("\"scope\":\"user_command_output_model_boundary\""));
    assert!(text.len() < raw.len());
}

/// Known provenance is authority metadata, so malformed or duplicate markers
/// must fail closed instead of silently reclassifying a direct command as a
/// model tool execution.
#[tokio::test]
async fn malformed_and_duplicate_user_command_origins_are_store_corruption() {
    for duplicate in [false, true] {
        let store = MemoryStore::new();
        let session_id = SessionId::new(if duplicate {
            "duplicate-command-origin-session"
        } else {
            "malformed-command-origin-session"
        });
        let shell_run = RunId::new("origin-shell-run");
        let current_run = RunId::new("origin-current-run");
        let origin = UserCommandOriginV1 {
            origin: CommandExecutionOrigin::UserCommand,
            command_item_id: ItemId::new("origin-command-item"),
            call_id: "origin-call".into(),
        };
        let origin_item = if duplicate {
            origin.extension_item().expect("origin item")
        } else {
            TurnItem::Extension {
                kind: USER_COMMAND_ORIGIN_EXTENSION_KIND.into(),
                data: serde_json::json!({"origin": "user_command"}),
            }
        };
        let mut events = vec![
            envelope(
                &session_id,
                &shell_run,
                "origin-marker-one",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("origin-marker-one-item"),
                    item: origin_item,
                }),
                PromptRender::Omit,
            ),
            envelope(
                &session_id,
                &shell_run,
                "origin-shell-done",
                EventPayload::RunState(RunState::Done),
                PromptRender::Omit,
            ),
            envelope(
                &session_id,
                &current_run,
                "origin-current-user",
                EventPayload::UserMessage {
                    text: "continue".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
        ];
        if duplicate {
            events.insert(
                1,
                envelope(
                    &session_id,
                    &shell_run,
                    "origin-marker-two",
                    EventPayload::Item(ItemEvent::Completed {
                        item_id: ItemId::new("origin-marker-two-item"),
                        item: origin.extension_item().expect("duplicate origin item"),
                    }),
                    PromptRender::Omit,
                ),
            );
        }
        StoreHandle::append(&store, &mut events)
            .await
            .expect("append corrupt origin fixture");
        let error = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current_run)
            .await
            .expect_err("known corrupt origin must fail closed");
        assert_eq!(error.code, haider_protocol::error::ErrorCode::StoreCorrupt);
    }
}

/// The provider-context cap is independent of the process hard cap. Every
/// omitted byte is disclosed, while a model-origin CommandExecution with no
/// user-command marker remains absent instead of being mislabeled.
#[tokio::test]
async fn user_command_context_is_bounded_and_model_exec_is_not_reclassified() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("bounded-user-command-session");
    let shell_run = RunId::new("bounded-user-command-shell");
    let current_run = RunId::new("bounded-user-command-current");
    let command_item = ItemId::new("bounded-user-command-item");
    let output = vec![b'x'; USER_COMMAND_OUTPUT_PREVIEW_BYTES + 1_024];
    let origin = UserCommandOriginV1 {
        origin: CommandExecutionOrigin::UserCommand,
        command_item_id: command_item.clone(),
        call_id: "bounded-shell".into(),
    };
    let mut events = vec![
        envelope(
            &session_id,
            &shell_run,
            "bounded-origin",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("bounded-origin-item"),
                item: origin.extension_item().expect("origin item"),
            }),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &shell_run,
            "bounded-output",
            EventPayload::Item(ItemEvent::Delta {
                item_id: command_item.clone(),
                delta: ItemDelta::CommandOutput {
                    stream: OutputStream::Stdout,
                    chunk_b64: BASE64.encode(&output),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "bounded-completed",
            EventPayload::Item(ItemEvent::Completed {
                item_id: command_item,
                item: TurnItem::CommandExecution {
                    call_id: "bounded-shell".into(),
                    command: "produce output".into(),
                    status: ToolStatus::Completed,
                    exit_code: Some(0),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "model-exec-completed",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("model-exec-item"),
                item: TurnItem::CommandExecution {
                    call_id: "model-exec".into(),
                    command: "must stay hidden".into(),
                    status: ToolStatus::Completed,
                    exit_code: Some(0),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "bounded-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current_run,
            "bounded-current",
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
        .expect("append bounded command history");

    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current_run)
        .await
        .expect("compile bounded command history");
    let Block::Text { text } = &messages[0].blocks[0] else {
        panic!("bounded command must be text")
    };
    assert!(text.contains("output_bytes: 9216"));
    assert!(text.contains("model-context output preview truncated"));
    assert!(!text.contains("must stay hidden"));
    assert!(text.len() < USER_COMMAND_OUTPUT_PREVIEW_BYTES + 1_024);
}

#[tokio::test]
async fn cancelled_and_failed_user_commands_reach_later_context_in_their_exact_scope() {
    for (suffix, status, terminal) in [
        ("cancelled", ToolStatus::Cancelled, RunState::Cancelled),
        ("failed", ToolStatus::Failed, RunState::Errored),
    ] {
        let store = MemoryStore::new();
        let session_id = SessionId::new(format!("scoped-{suffix}-command-session"));
        let shell_run = RunId::new(format!("scoped-{suffix}-command-shell"));
        let current_run = RunId::new(format!("scoped-{suffix}-command-current"));
        let branch = BranchId::new("command-branch");
        let agent = haider_protocol::ids::AgentId::new("command-agent");
        let command_item = ItemId::new(format!("scoped-{suffix}-command-item"));
        let origin = UserCommandOriginV1 {
            origin: CommandExecutionOrigin::UserCommand,
            command_item_id: command_item.clone(),
            call_id: format!("scoped-{suffix}-command"),
        };
        let scoped = |mut raw: haider_protocol::envelope::RawEnvelope| {
            raw.branch_id = Some(branch.clone());
            raw.agent_id = Some(agent.clone());
            raw
        };
        let mut events = vec![
            scoped(envelope(
                &session_id,
                &shell_run,
                &format!("scoped-{suffix}-origin"),
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new(format!("scoped-{suffix}-origin-item")),
                    item: origin.extension_item().expect("origin item"),
                }),
                PromptRender::Omit,
            )),
            scoped(envelope(
                &session_id,
                &shell_run,
                &format!("scoped-{suffix}-output"),
                EventPayload::Item(ItemEvent::Delta {
                    item_id: command_item.clone(),
                    delta: ItemDelta::CommandOutput {
                        stream: OutputStream::Stderr,
                        chunk_b64: BASE64.encode(format!("{suffix} output")),
                    },
                }),
                PromptRender::Verbatim,
            )),
            scoped(envelope(
                &session_id,
                &shell_run,
                &format!("scoped-{suffix}-completed"),
                EventPayload::Item(ItemEvent::Completed {
                    item_id: command_item,
                    item: TurnItem::CommandExecution {
                        call_id: format!("scoped-{suffix}-command"),
                        command: format!("produce {suffix} result"),
                        status,
                        exit_code: None,
                    },
                }),
                PromptRender::Verbatim,
            )),
            scoped(envelope(
                &session_id,
                &shell_run,
                &format!("scoped-{suffix}-terminal"),
                EventPayload::RunState(terminal),
                PromptRender::Omit,
            )),
            scoped(envelope(
                &session_id,
                &current_run,
                &format!("scoped-{suffix}-current"),
                EventPayload::UserMessage {
                    text: "continue in scope".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            )),
        ];
        StoreHandle::append(&store, &mut events)
            .await
            .expect("append scoped terminal command");

        let messages = PromptHistoryCompiler::compile(
            &store,
            &session_id,
            Some(&branch),
            Some(&agent),
            &current_run,
        )
        .await
        .expect("compile scoped terminal command");
        let Block::Text { text } = &messages[0].blocks[0] else {
            panic!("terminal command is user text")
        };
        assert!(text.contains(&format!("status: {suffix}")));
        assert!(text.contains(&format!("{suffix} output")));
        assert_eq!(
            messages[1],
            haider_provider::Message::user_text("continue in scope")
        );
    }
}

#[tokio::test]
async fn idle_compaction_input_includes_completed_user_command_after_the_tree_head() {
    let store = MemoryStore::new();
    let artifacts = TestArtifacts(HashMap::new());
    let session_id = SessionId::new("idle-command-tail-session");
    let prior_run = RunId::new("idle-command-tail-prior");
    let shell_run = RunId::new("idle-command-tail-shell");
    let command_item = ItemId::new("idle-command-tail-item");
    let origin = UserCommandOriginV1 {
        origin: CommandExecutionOrigin::UserCommand,
        command_item_id: command_item.clone(),
        call_id: "idle-command-tail-call".into(),
    };
    let mut events = vec![
        envelope(
            &session_id,
            &prior_run,
            "idle-command-prior-user",
            EventPayload::UserMessage {
                text: "prior question".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior_run,
            "idle-command-prior-node",
            None,
            NodeKind::UserTurn {
                text: "prior question".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &prior_run,
            "idle-command-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &shell_run,
            "idle-command-tail-origin",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("idle-command-tail-origin-item"),
                item: origin.extension_item().expect("origin item"),
            }),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &shell_run,
            "idle-command-tail-output",
            EventPayload::Item(ItemEvent::Delta {
                item_id: command_item.clone(),
                delta: ItemDelta::CommandOutput {
                    stream: OutputStream::Stdout,
                    chunk_b64: BASE64.encode("tail output"),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "idle-command-tail-completed",
            EventPayload::Item(ItemEvent::Completed {
                item_id: command_item,
                item: TurnItem::CommandExecution {
                    call_id: "idle-command-tail-call".into(),
                    command: "printf tail output".into(),
                    status: ToolStatus::Completed,
                    exit_code: Some(0),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &shell_run,
            "idle-command-tail-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append idle command tail");

    let messages = PromptHistoryCompiler::compile_idle_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
    )
    .await
    .expect("compile idle command tail");
    assert_eq!(messages.len(), 2);
    let Block::Text { text } = &messages[1].blocks[0] else {
        panic!("idle command tail is text")
    };
    assert!(text.contains("printf tail output"));
    assert!(text.contains("tail output"));
}

/// MUTATION CHECK: delete the exact-prefix extensions, or extend a completed
/// tree request with every raw suffix envelope. Expected runtime failure: a
/// cached compile rereads lineage, or the next run resurrects the replaced
/// sibling. Selected suffixes must remain byte-identical to the full oracle.
#[tokio::test]
async fn prompt_cache_matches_fresh_compile_after_append() {
    let store = MemoryStore::new();
    let artifacts = TestArtifacts(HashMap::new());
    let cache = PromptHistoryCompiler::cache();
    let session_id = SessionId::new("prompt-cache-append-session");
    let run_id = RunId::new("prompt-cache-append-run");
    let next_run = RunId::new("prompt-cache-next-run");
    let mut initial = vec![
        envelope(
            &session_id,
            &run_id,
            "prompt-cache-initial",
            EventPayload::UserMessage {
                text: "inspect the cache".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &run_id,
            "prompt-cache-initial-node",
            None,
            NodeKind::UserTurn {
                text: "inspect the cache".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut initial)
        .await
        .expect("append initial prompt");
    let recording = RecordingStore::new(&store);
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &run_id,
    )
    .await
    .expect("prime prompt cache");
    let lineage_reads_before_append = recording.lineage_read_count();
    let head_before_append = StoreHandle::latest_seq(&store, &session_id)
        .await
        .expect("sample pre-append head");

    let mut suffix = vec![
        envelope(
            &session_id,
            &run_id,
            "prompt-cache-steer",
            EventPayload::UserMessage {
                text: "include the invalidation law".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &run_id,
            "prompt-cache-steer-node",
            Some("prompt-cache-initial-node"),
            NodeKind::UserTurn {
                text: "include the invalidation law".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut suffix)
        .await
        .expect("append cache-invalidating suffix");

    let cached = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &run_id,
    )
    .await
    .expect("compile cached projection after append");
    let fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &run_id,
    )
    .await
    .expect("compile fresh projection after append");
    assert_eq!(cached, fresh);
    assert_eq!(recording.lineage_read_count(), lineage_reads_before_append);
    assert_eq!(recording.read_cursors().last(), Some(&head_before_append));

    // A sibling replaces, rather than extends, the selected ancestry. The
    // exact cache must fall back to the indexed full fold and drop the old
    // steer fragment byte-for-byte like the oracle.
    let mut sibling = vec![
        envelope(
            &session_id,
            &run_id,
            "prompt-cache-sibling-user",
            EventPayload::UserMessage {
                text: "replace the earlier steer".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &run_id,
            "prompt-cache-sibling-node",
            Some("prompt-cache-initial-node"),
            NodeKind::UserTurn {
                text: "replace the earlier steer".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut sibling)
        .await
        .expect("append same-run sibling ancestry");
    let cached_sibling = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &run_id,
    )
    .await
    .expect("compile cached sibling ancestry");
    let fresh_sibling = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &run_id,
    )
    .await
    .expect("compile fresh sibling ancestry");
    assert_eq!(cached_sibling, fresh_sibling);
    assert!(cached_sibling.messages.iter().all(|message| {
        message.blocks.iter().all(
            |block| !matches!(block, Block::Text { text } if text == "include the invalidation law"),
        )
    }));

    // The next run extends the selected sibling ancestry, not every raw event
    // appended after the original request boundary. The discarded steer must
    // stay discarded across the run transition without rereading lineage.
    let lineage_reads_before_next_run = recording.lineage_read_count();
    let mut next = vec![
        envelope(
            &session_id,
            &run_id,
            "prompt-cache-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &next_run,
            "prompt-cache-next-user",
            EventPayload::UserMessage {
                text: "continue from the replacement".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &next_run,
            "prompt-cache-next-node",
            Some("prompt-cache-sibling-node"),
            NodeKind::UserTurn {
                text: "continue from the replacement".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut next)
        .await
        .expect("append next run after sibling replacement");
    let cached_next = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &next_run,
    )
    .await
    .expect("extend cached sibling into next run");
    let fresh_next = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &next_run,
    )
    .await
    .expect("fully compile next run after sibling replacement");
    assert_eq!(cached_next, fresh_next);
    assert_eq!(
        recording.lineage_read_count(),
        lineage_reads_before_next_run,
        "an indexed cross-run extension must reuse its lineage"
    );
    assert!(cached_next.messages.iter().all(|message| {
        message.blocks.iter().all(
            |block| !matches!(block, Block::Text { text } if text == "include the invalidation law"),
        )
    }));

    // Structural corruption discovered in an appended suffix must have the
    // same typed error as rebuilding the complete tree.
    let mut duplicate = vec![envelope(
        &session_id,
        &run_id,
        "prompt-cache-duplicate-node-event",
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("prompt-cache-sibling-node"),
            parent: Some(NodeId::new("prompt-cache-initial-node")),
            kind: NodeKind::UserTurn {
                text: "duplicate node".into(),
                attachments: Vec::new(),
            },
        }),
        PromptRender::Omit,
    )];
    StoreHandle::append(&store, &mut duplicate)
        .await
        .expect("append duplicate cached node");
    let cached_error = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &run_id,
    )
    .await
    .expect_err("cached duplicate node is corruption");
    let fresh_error = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &run_id,
    )
    .await
    .expect_err("fresh duplicate node is corruption");
    assert_eq!(cached_error.code, fresh_error.code);
    assert_eq!(cached_error.message, fresh_error.message);
}

/// MUTATION CHECK: unconditionally seed the cross-run prefix from the first
/// cached compile. Expected runtime failure: because that compile occurs after
/// the live run's assistant item, the next-run cache omits `late answer` while
/// the fresh projection includes it.
#[tokio::test]
async fn prompt_cache_does_not_seed_a_late_lossy_live_run_prefix() {
    let store = MemoryStore::new();
    let artifacts = TestArtifacts(HashMap::new());
    let cache = PromptHistoryCompiler::cache();
    let session_id = SessionId::new("prompt-cache-late-prefix-session");
    let live = RunId::new("prompt-cache-late-prefix-live");
    let next = RunId::new("prompt-cache-late-prefix-next");
    let mut live_events = vec![
        envelope(
            &session_id,
            &live,
            "prompt-cache-late-prefix-user",
            EventPayload::UserMessage {
                text: "start late-prefix test".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &live,
            "prompt-cache-late-prefix-answer",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("prompt-cache-late-prefix-answer-item"),
                item: TurnItem::AgentMessage {
                    text: "late answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &live,
            "prompt-cache-late-prefix-user-node",
            None,
            NodeKind::UserTurn {
                text: "start late-prefix test".into(),
                attachments: Vec::new(),
            },
        ),
        node(
            &session_id,
            &live,
            "prompt-cache-late-prefix-answer-node",
            Some("prompt-cache-late-prefix-user-node"),
            NodeKind::AssistantCommit {
                text: "late answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
    ];
    StoreHandle::append(&store, &mut live_events)
        .await
        .expect("append completed live-run output before first cached compile");
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &live,
    )
    .await
    .expect("compile live run after its assistant output");

    let mut transition = vec![
        envelope(
            &session_id,
            &live,
            "prompt-cache-late-prefix-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &next,
            "prompt-cache-late-prefix-next-user",
            EventPayload::UserMessage {
                text: "continue after late answer".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &next,
            "prompt-cache-late-prefix-next-node",
            Some("prompt-cache-late-prefix-answer-node"),
            NodeKind::UserTurn {
                text: "continue after late answer".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut transition)
        .await
        .expect("append next run after late first cache compile");
    let cached = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &next,
    )
    .await
    .expect("compile next run without a lossy live prefix");
    let fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &next,
    )
    .await
    .expect("fully compile next run after late first cache compile");

    assert_eq!(cached, fresh);
    assert!(cached.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text == "late answer"))
    }));
}

/// MUTATION CHECK: promote a live prefix to prior history without checking its
/// suffix-final run state. Expected runtime failure: cached output retains the
/// old user while a fresh fold omits that nonterminal run.
#[tokio::test]
async fn prompt_cache_requires_a_terminal_prefix_before_cross_run_extension() {
    let store = MemoryStore::new();
    let artifacts = TestArtifacts(HashMap::new());
    let cache = PromptHistoryCompiler::cache();
    let session_id = SessionId::new("prompt-cache-nonterminal-prefix-session");
    let prior = RunId::new("prompt-cache-nonterminal-prefix-prior");
    let current = RunId::new("prompt-cache-nonterminal-prefix-current");
    let mut initial = vec![
        envelope(
            &session_id,
            &prior,
            "prompt-cache-nonterminal-prefix-user",
            EventPayload::UserMessage {
                text: "must disappear while nonterminal".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "prompt-cache-nonterminal-prefix-node",
            None,
            NodeKind::UserTurn {
                text: "must disappear while nonterminal".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut initial)
        .await
        .expect("append live prefix");
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &prior,
    )
    .await
    .expect("prime live prefix");

    let mut transition = vec![
        envelope(
            &session_id,
            &prior,
            "prompt-cache-nonterminal-prefix-thinking",
            EventPayload::RunState(RunState::Thinking),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "prompt-cache-nonterminal-current-user",
            EventPayload::UserMessage {
                text: "current after invalid transition".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "prompt-cache-nonterminal-current-node",
            Some("prompt-cache-nonterminal-prefix-node"),
            NodeKind::UserTurn {
                text: "current after invalid transition".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut transition)
        .await
        .expect("append nonterminal cross-run transition");
    let cached = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("rebuild nonterminal prior run");
    let fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("fully compile nonterminal prior run");

    assert_eq!(cached, fresh);
    assert!(cached.messages.iter().all(|message| {
        message.blocks.iter().all(
            |block| !matches!(block, Block::Text { text } if text == "must disappear while nonterminal"),
        )
    }));
}

/// MUTATION CHECK: clear every exact projection when either agent advances,
/// or omit `agent_id` from the cache identity. Expected runtime failure: a
/// lineage reread or cached/fresh byte mismatch on one of the two timelines.
#[tokio::test]
async fn prompt_cache_extends_interleaved_agents_from_exact_heads() {
    let store = MemoryStore::new();
    let recording = RecordingStore::new(&store);
    let artifacts = TestArtifacts(HashMap::new());
    let cache = PromptHistoryCompiler::cache();
    let session_id = SessionId::new("prompt-cache-agent-session");
    let first_agent = AgentId::new("prompt-cache-agent-a");
    let second_agent = AgentId::new("prompt-cache-agent-b");
    let first_run = RunId::new("prompt-cache-agent-a-run");
    let second_run = RunId::new("prompt-cache-agent-b-run");
    let scoped = |mut event: haider_protocol::envelope::RawEnvelope, agent: &AgentId| {
        event.agent_id = Some(agent.clone());
        event
    };
    let mut initial = vec![
        scoped(
            envelope(
                &session_id,
                &first_run,
                "prompt-cache-agent-a-user",
                EventPayload::UserMessage {
                    text: "agent a".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
            &first_agent,
        ),
        scoped(
            node(
                &session_id,
                &first_run,
                "prompt-cache-agent-a-node",
                None,
                NodeKind::UserTurn {
                    text: "agent a".into(),
                    attachments: Vec::new(),
                },
            ),
            &first_agent,
        ),
        scoped(
            envelope(
                &session_id,
                &second_run,
                "prompt-cache-agent-b-user",
                EventPayload::UserMessage {
                    text: "agent b".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
            &second_agent,
        ),
        scoped(
            node(
                &session_id,
                &second_run,
                "prompt-cache-agent-b-node",
                None,
                NodeKind::UserTurn {
                    text: "agent b".into(),
                    attachments: Vec::new(),
                },
            ),
            &second_agent,
        ),
    ];
    StoreHandle::append(&store, &mut initial)
        .await
        .expect("append agent cache prefixes");
    for (agent, run) in [(&first_agent, &first_run), (&second_agent, &second_run)] {
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &cache,
            &recording,
            &artifacts,
            &session_id,
            None,
            Some(agent),
            run,
        )
        .await
        .expect("prime agent prompt cache");
    }
    let lineage_reads_before_suffix = recording.lineage_read_count();
    let mut suffix = vec![
        scoped(
            envelope(
                &session_id,
                &first_run,
                "prompt-cache-agent-a-steer",
                EventPayload::UserMessage {
                    text: "agent a steer".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Steer,
                },
                PromptRender::Verbatim,
            ),
            &first_agent,
        ),
        scoped(
            node(
                &session_id,
                &first_run,
                "prompt-cache-agent-a-steer-node",
                Some("prompt-cache-agent-a-node"),
                NodeKind::UserTurn {
                    text: "agent a steer".into(),
                    attachments: Vec::new(),
                },
            ),
            &first_agent,
        ),
        scoped(
            envelope(
                &session_id,
                &second_run,
                "prompt-cache-agent-b-steer",
                EventPayload::UserMessage {
                    text: "agent b steer".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Steer,
                },
                PromptRender::Verbatim,
            ),
            &second_agent,
        ),
        scoped(
            node(
                &session_id,
                &second_run,
                "prompt-cache-agent-b-steer-node",
                Some("prompt-cache-agent-b-node"),
                NodeKind::UserTurn {
                    text: "agent b steer".into(),
                    attachments: Vec::new(),
                },
            ),
            &second_agent,
        ),
    ];
    StoreHandle::append(&store, &mut suffix)
        .await
        .expect("append interleaved agent suffixes");

    for (agent, run) in [(&first_agent, &first_run), (&second_agent, &second_run)] {
        let cached = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &cache,
            &recording,
            &artifacts,
            &session_id,
            None,
            Some(agent),
            run,
        )
        .await
        .expect("extend exact agent projection");
        let fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
            &store,
            &artifacts,
            &session_id,
            None,
            Some(agent),
            run,
        )
        .await
        .expect("fully compile agent projection");
        assert_eq!(cached, fresh);
    }
    assert_eq!(
        recording.lineage_read_count(),
        lineage_reads_before_suffix,
        "interleaved agent suffixes must reuse both exact ancestry indexes"
    );
}

/// MUTATION CHECK: apply suffix facts only to suffix envelopes, or ignore every
/// fact whose envelope belongs to the current run. Expected runtime failure:
/// the cached projection omits the newly terminal prior run or its reclassified
/// user command while the full projection retroactively includes it.
#[tokio::test]
async fn prompt_cache_rebuilds_when_suffix_facts_revise_the_prefix() {
    let store = MemoryStore::new();
    let artifacts = TestArtifacts(HashMap::new());
    let cache = PromptHistoryCompiler::cache();
    let session_id = SessionId::new("prompt-cache-retroactive-session");
    let prior = RunId::new("prompt-cache-retroactive-prior");
    let current = RunId::new("prompt-cache-retroactive-current");
    let command_item = ItemId::new("prompt-cache-retroactive-command");
    let command_origin = UserCommandOriginV1 {
        origin: CommandExecutionOrigin::UserCommand,
        command_item_id: command_item.clone(),
        call_id: "prompt-cache-retroactive-call".into(),
    };
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "prompt-cache-retroactive-prior-user",
            EventPayload::UserMessage {
                text: "previously unfinished".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "prompt-cache-retroactive-output",
            EventPayload::Item(ItemEvent::Delta {
                item_id: command_item.clone(),
                delta: ItemDelta::CommandOutput {
                    stream: OutputStream::Stdout,
                    chunk_b64: BASE64.encode("retroactive output"),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "prompt-cache-retroactive-command-completed",
            EventPayload::Item(ItemEvent::Completed {
                item_id: command_item,
                item: TurnItem::CommandExecution {
                    call_id: "prompt-cache-retroactive-call".into(),
                    command: "printf retroactive output".into(),
                    status: ToolStatus::Completed,
                    exit_code: Some(0),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "prompt-cache-retroactive-prior-node",
            None,
            NodeKind::UserTurn {
                text: "previously unfinished".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &current,
            "prompt-cache-retroactive-current-user",
            EventPayload::UserMessage {
                text: "current request".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "prompt-cache-retroactive-current-node",
            Some("prompt-cache-retroactive-prior-node"),
            NodeKind::UserTurn {
                text: "current request".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append retroactive cache history");
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("prime retroactive prompt cache");

    let mut terminal = vec![envelope(
        &session_id,
        &prior,
        "prompt-cache-retroactive-prior-done",
        EventPayload::RunState(RunState::Done),
        PromptRender::Omit,
    )];
    StoreHandle::append(&store, &mut terminal)
        .await
        .expect("terminalize prior run after cached head");
    let cached = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("rebuild cached projection after retroactive fact");
    let fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("fully compile retroactive projection");
    assert_eq!(cached, fresh);
    assert!(cached.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text == "previously unfinished"))
    }));

    // The fact envelope belongs to the current run but classifies command
    // events selected from the prior run. Treating every current-run fact as
    // suffix-local would leave the cached prefix stale.
    let mut origin = vec![envelope(
        &session_id,
        &current,
        "prompt-cache-retroactive-command-origin",
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("prompt-cache-retroactive-origin-item"),
            item: command_origin.extension_item().expect("origin item"),
        }),
        PromptRender::Omit,
    )];
    StoreHandle::append(&store, &mut origin)
        .await
        .expect("append current-run retroactive origin");
    let cached = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("rebuild cached projection after current-run fact");
    let fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("fully compile current-run retroactive fact");
    assert_eq!(cached, fresh);
    assert!(cached.messages.iter().any(|message| {
        message.blocks.iter().any(
            |block| matches!(block, Block::Text { text } if text.contains("origin: user_command")),
        )
    }));
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
                    data: None,
                    artifact: None,
                    images: Vec::new(),
                    cursor: Some("cursor-7".into()),
                    status: haider_protocol::tool::ToolResultStatus::Completed,
                    reason: None,
                    presentation: None,
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

/// MUTATION CHECK: initialize the resumed fold at sequence zero instead of
/// the stored terminal boundary. Expected runtime failure: the read-cursor
/// assertion observes zero. Changing the checkpoint's `boundary_event_id` to
/// the compaction-node event also fails the terminal-boundary identity pin.
/// Replacing `prefix.projection.messages.clone()` with `Vec::new()` in the
/// resumed fold fails `restarted_projection == fresh_projection` at runtime.
/// Reusing the pre-compaction exact key after the epoch changes fails
/// `first_projection == fresh_projection` byte-for-byte.
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
    let after_restart = RunId::new("compacted-after-restart");
    let rewound = RunId::new("compacted-rewound-before-anchor");
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
            effort: None,
            fast: false,
            cache_policy: Default::default(),
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
    let surviving_image = AttachmentBlock::Image {
        artifact: ArtifactRef::new(format!("blake3:{}", "1".repeat(64))),
        mime: "image/png".into(),
        width: None,
        height: None,
    };
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
            &first,
            "compacted-first-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &second,
            "compacted-suffix-user",
            EventPayload::UserMessage {
                text: "suffix user".into(),
                attachments: vec![surviving_image.clone()],
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
                attachments: vec![surviving_image.clone()],
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
    let mut compacted_suffix = events.split_off(4);
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append pre-compaction history");
    let cache = PromptHistoryCompiler::cache();
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &store,
        &session_id,
        None,
        None,
        &first,
    )
    .await
    .expect("prime cache before compaction");
    StoreHandle::append(&store, &mut compacted_suffix)
        .await
        .expect("append compacted suffix");
    let first_projection =
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &cache,
            &store,
            &store,
            &session_id,
            None,
            None,
            &current,
        )
        .await
        .expect("compile cached compacted projection");
    let fresh_projection = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &store,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("compile fresh compacted projection");
    assert_eq!(first_projection, fresh_projection);
    store.close().await.expect("close before restart");
    let restarted = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store");
    let recording = RecordingStore::new(&restarted);
    let restarted_cache = PromptHistoryCompiler::cache();
    let restarted_projection =
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &restarted_cache,
            &recording,
            &restarted,
            &session_id,
            None,
            None,
            &current,
        )
        .await
        .expect("compile after restart from durable boundary");
    let loaded_checkpoints = recording.loaded_checkpoints();
    assert_eq!(loaded_checkpoints.len(), 1);
    assert_eq!(
        loaded_checkpoints[0].boundary_event_id,
        EventId::new("compacted-first-done"),
        "the checkpoint must use TranscriptProjector's terminal boundary, not the compaction node"
    );
    assert!(
        !recording.read_cursors().contains(&0),
        "a readable boundary checkpoint must not replay from zero: {:?}",
        recording.read_cursors()
    );
    let text = first_projection
        .messages
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
    assert!(first_projection.messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(block, Block::Attachment(attachment) if attachment == &surviving_image)
        })
    }));
    assert_eq!(first_projection.latest_compaction_summary_end, Some(1));
    assert_eq!(first_projection.stable_history_end, 3);
    assert_eq!(first_projection.current_user_start, 3);
    assert_eq!(first_projection, restarted_projection);
    assert_eq!(restarted_projection, fresh_projection);
    assert_eq!(
        serde_json::to_vec(&first_projection.messages).expect("serialize first compile"),
        serde_json::to_vec(&restarted_projection.messages).expect("serialize restarted compile")
    );

    // The checkpoint anchor remains a valid exact-prefix seam when the same
    // current run appends a descendant. This must extend the resident suffix,
    // not fall through to a truncated-tree compile or replay from sequence 0.
    let mut same_current = vec![
        envelope(
            &session_id,
            &current,
            "compacted-current-steer",
            EventPayload::UserMessage {
                text: "same current after restart".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "compact-current-steer-node",
            Some("compact-n5"),
            NodeKind::UserTurn {
                text: "same current after restart".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&restarted, &mut same_current)
        .await
        .expect("append same-current descendant after checkpoint restart");
    let cached_same_current =
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &restarted_cache,
            &recording,
            &restarted,
            &session_id,
            None,
            None,
            &current,
        )
        .await
        .expect("extend exact checkpoint prefix for the same current run");
    let fresh_same_current = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &restarted,
        &restarted,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("fully compile same-current checkpoint descendant");
    assert_eq!(cached_same_current, fresh_same_current);
    assert!(
        !recording.read_cursors().contains(&0),
        "a same-current descendant must extend the checkpoint suffix: {:?}",
        recording.read_cursors()
    );

    // Force sibling replacements before the next run. Raw suffix replay would
    // resurrect both discarded steers; the checkpoint anchor must follow only
    // the selected ancestry without rereading the compacted prefix.
    let mut post_checkpoint = vec![
        envelope(
            &session_id,
            &current,
            "compacted-discarded-steer",
            EventPayload::UserMessage {
                text: "discarded post-checkpoint steer".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "compact-discarded-node",
            Some("compact-n5"),
            NodeKind::UserTurn {
                text: "discarded post-checkpoint steer".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &current,
            "compacted-selected-steer",
            EventPayload::UserMessage {
                text: "selected post-checkpoint steer".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "compact-selected-node",
            Some("compact-n5"),
            NodeKind::UserTurn {
                text: "selected post-checkpoint steer".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &current,
            "compacted-current-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &after_restart,
            "compacted-after-restart-user",
            EventPayload::UserMessage {
                text: "continue after restart".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &after_restart,
            "compact-after-restart-node",
            Some("compact-selected-node"),
            NodeKind::UserTurn {
                text: "continue after restart".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&restarted, &mut post_checkpoint)
        .await
        .expect("append post-checkpoint sibling transition");
    let cached_after_restart =
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &restarted_cache,
            &recording,
            &restarted,
            &session_id,
            None,
            None,
            &after_restart,
        )
        .await
        .expect("compile post-checkpoint sibling transition from cache");
    let fresh_after_restart = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &restarted,
        &restarted,
        &session_id,
        None,
        None,
        &after_restart,
    )
    .await
    .expect("fully compile post-checkpoint sibling transition");
    assert_eq!(cached_after_restart, fresh_after_restart);
    assert!(
        !recording.read_cursors().contains(&0),
        "a selected checkpoint descendant must not replay from zero: {:?}",
        recording.read_cursors()
    );
    assert!(cached_after_restart.messages.iter().all(|message| {
        message.blocks.iter().all(
            |block| !matches!(block, Block::Text { text } if text == "discarded post-checkpoint steer" || text == "same current after restart"),
        )
    }));
    restarted.close().await.expect("close restarted store");

    // Reopen again so the sibling-rich suffix is present before the first
    // cache request. This pins first-resume ancestry selection, not merely a
    // warm transition that learned the siblings incrementally.
    let second_restart = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store with sibling-rich checkpoint suffix");
    let second_recording = RecordingStore::new(&second_restart);
    let second_cache = PromptHistoryCompiler::cache();
    let second_resumed = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &second_cache,
        &second_recording,
        &second_restart,
        &session_id,
        None,
        None,
        &after_restart,
    )
    .await
    .expect("first cache compile selects checkpoint suffix ancestry");
    let second_fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &second_restart,
        &second_restart,
        &session_id,
        None,
        None,
        &after_restart,
    )
    .await
    .expect("fully compile sibling-rich checkpoint suffix");
    assert_eq!(second_resumed, second_fresh);
    assert!(
        !second_recording.read_cursors().contains(&0),
        "first resume must select siblings from the compaction anchor: {:?}",
        second_recording.read_cursors()
    );
    assert!(second_resumed.messages.iter().all(|message| {
        message.blocks.iter().all(
            |block| !matches!(block, Block::Text { text } if text == "discarded post-checkpoint steer" || text == "same current after restart"),
        )
    }));

    // A selected suffix may validly rewind to ancestry older than the
    // checkpoint anchor. The truncated index cannot prove that parent, so it
    // must replay the oracle instead of reporting missing-parent corruption.
    let mut rewind = vec![
        envelope(
            &session_id,
            &after_restart,
            "compacted-after-restart-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &rewound,
            "compacted-rewound-user",
            EventPayload::UserMessage {
                text: "rewind before the checkpoint anchor".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &rewound,
            "compacted-rewound-node",
            Some("compact-n2"),
            NodeKind::UserTurn {
                text: "rewind before the checkpoint anchor".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&second_restart, &mut rewind)
        .await
        .expect("append valid rewind before checkpoint anchor");
    let cached_rewind = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &second_cache,
        &second_recording,
        &second_restart,
        &session_id,
        None,
        None,
        &rewound,
    )
    .await
    .expect("checkpoint rewind replays complete ancestry");
    let fresh_rewind = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &second_restart,
        &second_restart,
        &session_id,
        None,
        None,
        &rewound,
    )
    .await
    .expect("fully compile rewind before checkpoint anchor");
    assert_eq!(cached_rewind, fresh_rewind);
    assert!(
        second_recording.read_cursors().contains(&0),
        "an unprovable pre-anchor rewind must replay from zero: {:?}",
        second_recording.read_cursors()
    );
    second_restart
        .close()
        .await
        .expect("close second restarted store");
}

/// MUTATION CHECK: replace the `Ok(stored) => stored?` cache-miss arm with
/// `Some(stored.expect("checkpoint"))`. Expected runtime failure: this
/// never-compacted session panics instead of folding normally from sequence 0.
#[tokio::test]
async fn session_without_compaction_folds_from_zero_without_a_checkpoint() {
    let store = MemoryStore::new();
    let artifacts = TestArtifacts(HashMap::new());
    let session_id = SessionId::new("no-compaction-checkpoint-session");
    let current = RunId::new("no-compaction-current");
    let mut events = vec![envelope(
        &session_id,
        &current,
        "no-compaction-user",
        EventPayload::UserMessage {
            text: "ordinary short session".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
        PromptRender::Verbatim,
    )];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append uncompacted history");

    let full = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("fold uncompacted journal");
    let recording = RecordingStore::new(&store);
    let resumed = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &PromptHistoryCompiler::cache(),
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("cache miss falls back to journal");

    assert_eq!(resumed, full);
    assert!(
        recording.read_cursors().contains(&0),
        "a session without a checkpoint must begin at zero"
    );
}

/// MUTATION CHECK: omit `prefix_node_ids` from the durable checkpoint or skip
/// its suffix collision check. Expected runtime failure: cached compilation
/// accepts a node ID reused from omitted ancestry while the fresh tree reports
/// typed duplicate-node corruption.
#[tokio::test]
async fn checkpoint_membership_detects_a_suffix_duplicate_node() {
    let store = MemoryStore::new();
    let artifacts = TestArtifacts(HashMap::new());
    let session_id = SessionId::new("checkpoint-duplicate-node-session");
    let prior = RunId::new("checkpoint-duplicate-node-prior");
    let current = RunId::new("checkpoint-duplicate-node-current");
    let anchor = "checkpoint-duplicate-anchor";
    let mut events = vec![
        node(
            &session_id,
            &prior,
            "checkpoint-duplicate-covered-from",
            None,
            NodeKind::UserTurn {
                text: "covered user".into(),
                attachments: Vec::new(),
            },
        ),
        node(
            &session_id,
            &prior,
            "checkpoint-duplicate-covered-to",
            Some("checkpoint-duplicate-covered-from"),
            NodeKind::AssistantCommit {
                text: "covered answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        node(
            &session_id,
            &prior,
            anchor,
            Some("checkpoint-duplicate-covered-to"),
            NodeKind::Compaction {
                covers_from: NodeId::new("checkpoint-duplicate-covered-from"),
                covers_to: NodeId::new("checkpoint-duplicate-covered-to"),
                summary_artifact: ArtifactRef::new(format!("blake3:{}", "9".repeat(64))),
                tokens_before: 10,
                tokens_after: 1,
                resume_cause: CompactionResume::ManualIdle,
            },
        ),
        envelope(
            &session_id,
            &prior,
            "checkpoint-duplicate-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "checkpoint-duplicate-current-user",
            EventPayload::UserMessage {
                text: "reuse the omitted node id".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            anchor,
            Some(anchor),
            NodeKind::UserTurn {
                text: "reuse the omitted node id".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append suffix duplicate of checkpoint node");
    let checkpoint_message = Message::user_text("checkpoint summary");
    let checkpoint = SessionProjectionCheckpoint {
        session_id: session_id.clone(),
        projection: "prompt_history".into(),
        timeline_key: "checkpoint-duplicate-fixture".into(),
        through_seq: 4,
        boundary_event_id: events[3].event_id.clone(),
        payload: serde_json::to_vec(&serde_json::json!({
            "shape_version": 1,
            "reducer_version": "prompt-history-v1",
            "through_seq": 4,
            "boundary_event_id": events[3].event_id,
            "boundary_run_id": prior,
            "compaction_epoch": 3,
            "prefix_node_ids": [
                "checkpoint-duplicate-covered-from",
                "checkpoint-duplicate-covered-to",
                anchor
            ],
            "prefix_run_ids": [prior],
            "messages": [serde_json::to_value(checkpoint_message).expect("message value")],
            "stable_history_end": 1,
            "current_user_start": 1,
            "latest_compaction_summary_end": 1
        }))
        .expect("encode duplicate-membership checkpoint"),
    };
    let recording = RecordingStore::with_checkpoint(&store, checkpoint);
    let cached_error = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &PromptHistoryCompiler::cache(),
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect_err("checkpoint suffix duplicate is corruption");
    let fresh_error = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect_err("fresh suffix duplicate is corruption");

    assert_eq!(cached_error.code, fresh_error.code);
    assert_eq!(cached_error.message, fresh_error.message);
    assert!(recording.read_cursors().contains(&0));
}

/// MUTATION CHECK: remove the checkpoint-prefix run membership guard in
/// `suffix_revises_prior_facts`. Expected runtime failure: the resumed fold
/// omits the late assistant item because its terminal fact is before the
/// checkpoint boundary, while the fresh fold renders it.
#[tokio::test]
async fn checkpoint_replays_a_late_envelope_for_a_covered_run() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("checkpoint-late-covered-run-session");
    let prior = RunId::new("checkpoint-late-covered-prior");
    let current = RunId::new("checkpoint-late-covered-current");
    let summary_artifact = ArtifactRef::new(format!("blake3:{}", "8".repeat(64)));
    let artifacts = TestArtifacts(HashMap::from([(
        summary_artifact.clone(),
        b"checkpoint summary".to_vec(),
    )]));
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "checkpoint-late-covered-user",
            EventPayload::UserMessage {
                text: "covered user".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "checkpoint-late-covered-user-node",
            None,
            NodeKind::UserTurn {
                text: "covered user".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &prior,
            "checkpoint-late-covered-first-answer",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("checkpoint-late-covered-first-answer-item"),
                item: TurnItem::AgentMessage {
                    text: "covered answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "checkpoint-late-covered-answer-node",
            Some("checkpoint-late-covered-user-node"),
            NodeKind::AssistantCommit {
                text: "covered answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        node(
            &session_id,
            &prior,
            "checkpoint-late-covered-anchor",
            Some("checkpoint-late-covered-answer-node"),
            NodeKind::Compaction {
                covers_from: NodeId::new("checkpoint-late-covered-user-node"),
                covers_to: NodeId::new("checkpoint-late-covered-answer-node"),
                summary_artifact: summary_artifact.clone(),
                tokens_before: 10,
                tokens_after: 1,
                resume_cause: CompactionResume::ManualIdle,
            },
        ),
        envelope(
            &session_id,
            &prior,
            "checkpoint-late-covered-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &prior,
            "checkpoint-late-covered-second-answer",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("checkpoint-late-covered-second-answer-item"),
                item: TurnItem::AgentMessage {
                    text: "late terminal answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &current,
            "checkpoint-late-covered-current-user",
            EventPayload::UserMessage {
                text: "current user".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "checkpoint-late-covered-current-node",
            Some("checkpoint-late-covered-anchor"),
            NodeKind::UserTurn {
                text: "current user".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append late covered-run envelope");
    let checkpoint = SessionProjectionCheckpoint {
        session_id: session_id.clone(),
        projection: "prompt_history".into(),
        timeline_key: "checkpoint-late-covered-run-fixture".into(),
        through_seq: 6,
        boundary_event_id: events[5].event_id.clone(),
        payload: serde_json::to_vec(&serde_json::json!({
            "shape_version": 1,
            "reducer_version": "prompt-history-v1",
            "through_seq": 6,
            "boundary_event_id": events[5].event_id,
            "boundary_run_id": prior,
            "compaction_epoch": 5,
            "prefix_node_ids": [
                "checkpoint-late-covered-user-node",
                "checkpoint-late-covered-answer-node",
                "checkpoint-late-covered-anchor"
            ],
            "prefix_run_ids": [prior],
            "messages": [serde_json::to_value(Message::user_text("checkpoint summary"))
                .expect("message value")],
            "stable_history_end": 1,
            "current_user_start": 1,
            "latest_compaction_summary_end": 1
        }))
        .expect("encode late covered-run checkpoint"),
    };
    let recording = RecordingStore::with_checkpoint(&store, checkpoint);
    let resumed = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &PromptHistoryCompiler::cache(),
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("resume after a late covered-run envelope");
    let fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("fully compile a late covered-run envelope");

    assert_eq!(resumed, fresh);
    assert!(resumed.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text == "late terminal answer"))
    }));
    assert!(
        recording.read_cursors().contains(&0),
        "continuing a covered run must replay its omitted facts: {:?}",
        recording.read_cursors()
    );
}

/// MUTATION CHECK: remove `decoded.branch_id != timeline.branch_id` from the
/// checkpoint validator. Expected runtime failure: the branch-A summary is
/// prepended to branch B and the full/resumed equality assertion fails.
#[tokio::test]
async fn checkpoint_from_another_branch_is_rejected() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("cross-branch-checkpoint-session");
    let branch_a = BranchId::new("checkpoint-branch-a");
    let branch_b = BranchId::new("requested-branch-b");
    let compacting = RunId::new("branch-a-compaction");
    let current = RunId::new("branch-b-current");
    let summary_artifact = ArtifactRef::new(format!("blake3:{}", "a".repeat(64)));
    let artifacts = TestArtifacts(HashMap::from([(
        summary_artifact.clone(),
        b"real branch A summary".to_vec(),
    )]));
    let scoped = |mut event: haider_protocol::envelope::RawEnvelope, branch: &BranchId| {
        event.branch_id = Some(branch.clone());
        event
    };
    let registry = |event_id: &str, branch: BranchDescriptor| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("checkpoint-branch-test"),
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
        payload: BranchCreated { branch }
            .to_payload_value()
            .expect("branch payload"),
    };
    let mut events = vec![
        scoped(
            envelope(
                &session_id,
                &compacting,
                "branch-a-covered-user",
                EventPayload::UserMessage {
                    text: "covered branch A user".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
            &branch_a,
        ),
        scoped(
            node(
                &session_id,
                &compacting,
                "branch-a-covered-from",
                None,
                NodeKind::UserTurn {
                    text: "covered branch A user".into(),
                    attachments: Vec::new(),
                },
            ),
            &branch_a,
        ),
        scoped(
            envelope(
                &session_id,
                &compacting,
                "branch-a-covered-answer",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("branch-a-covered-answer-item"),
                    item: TurnItem::AgentMessage {
                        text: "covered branch A answer".into(),
                    },
                }),
                PromptRender::Verbatim,
            ),
            &branch_a,
        ),
        scoped(
            node(
                &session_id,
                &compacting,
                "branch-a-covered-to",
                Some("branch-a-covered-from"),
                NodeKind::AssistantCommit {
                    text: "covered branch A answer".into(),
                    verdict: VerifyVerdict::NotApplicable,
                },
            ),
            &branch_a,
        ),
        scoped(
            node(
                &session_id,
                &compacting,
                "branch-a-compaction-node",
                Some("branch-a-covered-to"),
                NodeKind::Compaction {
                    covers_from: NodeId::new("branch-a-covered-from"),
                    covers_to: NodeId::new("branch-a-covered-to"),
                    summary_artifact,
                    tokens_before: 40,
                    tokens_after: 4,
                    resume_cause: CompactionResume::ManualIdle,
                },
            ),
            &branch_a,
        ),
        scoped(
            envelope(
                &session_id,
                &compacting,
                "branch-a-compaction-done",
                EventPayload::RunState(RunState::Done),
                PromptRender::Omit,
            ),
            &branch_a,
        ),
        scoped(
            envelope(
                &session_id,
                &current,
                "branch-b-current-user",
                EventPayload::UserMessage {
                    text: "branch B only".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
            &branch_b,
        ),
        scoped(
            node(
                &session_id,
                &current,
                "branch-b-current-node",
                Some("branch-a-compaction-node"),
                NodeKind::UserTurn {
                    text: "branch B only".into(),
                    attachments: Vec::new(),
                },
            ),
            &branch_b,
        ),
        registry(
            "checkpoint-branch-a-created",
            BranchDescriptor {
                branch_id: branch_a.clone(),
                name: "Checkpoint A".into(),
                source_branch_id: None,
                fork_node_id: NodeId::new("branch-a-covered-from"),
                fork_seq: 2,
                created_seq: 9,
                created_at_ms: 1,
                head_node_id: NodeId::new("branch-a-compaction-node"),
                head_seq: 5,
            },
        ),
        registry(
            "checkpoint-branch-b-created",
            BranchDescriptor {
                branch_id: branch_b.clone(),
                name: "Checkpoint B".into(),
                source_branch_id: Some(branch_a.clone()),
                fork_node_id: NodeId::new("branch-a-compaction-node"),
                fork_seq: 5,
                created_seq: 10,
                created_at_ms: 2,
                head_node_id: NodeId::new("branch-b-current-node"),
                head_seq: 8,
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append divergent timelines");

    let wrong_message = Message {
        role: MessageRole::User,
        blocks: vec![Block::Text {
            text: "branch A summary must never leak".into(),
        }],
    };
    let wrong_checkpoint = SessionProjectionCheckpoint {
        session_id: session_id.clone(),
        projection: "prompt_history".into(),
        timeline_key: "branch-a-key-returned-adversarially".into(),
        through_seq: 6,
        boundary_event_id: events[5].event_id.clone(),
        payload: serde_json::to_vec(&serde_json::json!({
            "shape_version": 1,
            "reducer_version": "prompt-history-v1",
            "through_seq": 6,
            "boundary_event_id": events[5].event_id,
            "boundary_run_id": compacting,
            "branch_id": branch_a,
            "compaction_epoch": 5,
            "prefix_node_ids": [
                "branch-a-covered-from",
                "branch-a-covered-to",
                "branch-a-compaction-node"
            ],
            "prefix_run_ids": [compacting],
            "messages": [serde_json::to_value(wrong_message).expect("message value")],
            "stable_history_end": 1,
            "current_user_start": 1,
            "latest_compaction_summary_end": 1
        }))
        .expect("encode shape-valid wrong-branch checkpoint"),
    };
    let recording = RecordingStore::with_checkpoint(&store, wrong_checkpoint);
    let resumed = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &PromptHistoryCompiler::cache(),
        &recording,
        &artifacts,
        &session_id,
        Some(&branch_b),
        None,
        &current,
    )
    .await
    .expect("reject wrong-branch checkpoint");
    let full = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        Some(&branch_b),
        None,
        &current,
    )
    .await
    .expect("fold requested branch from zero");

    assert_eq!(resumed, full);
    assert!(
        recording.read_cursors().contains(&0),
        "an unprovable timeline checkpoint must fall back to sequence zero"
    );
}

/// MUTATION CHECK: replace `affects_checkpoint_timeline` with `true` for
/// suffix compaction nodes. Expected runtime failure: branch A's compaction
/// makes the valid main-timeline resume read from sequence zero.
#[tokio::test]
async fn compaction_on_another_branch_does_not_invalidate_the_checkpoint() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("unrelated-branch-compaction-session");
    let prior = RunId::new("main-prior-compaction");
    let other = RunId::new("other-branch-compaction");
    let current = RunId::new("main-current-after-other-compaction");
    let other_branch = BranchId::new("unrelated-branch");
    let summary_artifact = ArtifactRef::new(format!("blake3:{}", "b".repeat(64)));
    let artifacts = TestArtifacts(HashMap::from([(
        summary_artifact.clone(),
        b"main checkpoint summary".to_vec(),
    )]));
    let mut other_compaction = node(
        &session_id,
        &other,
        "unrelated-compaction-node",
        None,
        NodeKind::Compaction {
            covers_from: NodeId::new("unrelated-covered-from"),
            covers_to: NodeId::new("unrelated-covered-to"),
            summary_artifact: ArtifactRef::new(format!("blake3:{}", "c".repeat(64))),
            tokens_before: 50,
            tokens_after: 5,
            resume_cause: CompactionResume::ManualIdle,
        },
    );
    other_compaction.branch_id = Some(other_branch.clone());
    let mut other_done = envelope(
        &session_id,
        &other,
        "unrelated-compaction-done",
        EventPayload::RunState(RunState::Done),
        PromptRender::Omit,
    );
    other_done.branch_id = Some(other_branch);
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "main-prior-user",
            EventPayload::UserMessage {
                text: "main old user".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "main-prior-user-node",
            None,
            NodeKind::UserTurn {
                text: "main old user".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &prior,
            "main-prior-assistant",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("main-prior-answer"),
                item: TurnItem::AgentMessage {
                    text: "main old answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior,
            "main-prior-assistant-node",
            Some("main-prior-user-node"),
            NodeKind::AssistantCommit {
                text: "main old answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        node(
            &session_id,
            &prior,
            "main-prior-compaction-node",
            Some("main-prior-assistant-node"),
            NodeKind::Compaction {
                covers_from: NodeId::new("main-prior-user-node"),
                covers_to: NodeId::new("main-prior-assistant-node"),
                summary_artifact,
                tokens_before: 100,
                tokens_after: 10,
                resume_cause: CompactionResume::ManualIdle,
            },
        ),
        envelope(
            &session_id,
            &prior,
            "main-prior-compaction-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        other_compaction,
        other_done,
        envelope(
            &session_id,
            &current,
            "main-current-user-after-other",
            EventPayload::UserMessage {
                text: "main current".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current,
            "main-current-node-after-other",
            Some("main-prior-compaction-node"),
            NodeKind::UserTurn {
                text: "main current".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append interleaved branch compactions");
    let checkpoint_message = Message {
        role: MessageRole::User,
        blocks: vec![Block::Text {
            text: "main checkpoint summary".into(),
        }],
    };
    let checkpoint = SessionProjectionCheckpoint {
        session_id: session_id.clone(),
        projection: "prompt_history".into(),
        timeline_key: "main-checkpoint-returned-by-fixture".into(),
        through_seq: 6,
        boundary_event_id: events[5].event_id.clone(),
        payload: serde_json::to_vec(&serde_json::json!({
            "shape_version": 1,
            "reducer_version": "prompt-history-v1",
            "through_seq": 6,
            "boundary_event_id": events[5].event_id,
            "boundary_run_id": prior,
            "compaction_epoch": 5,
            "prefix_node_ids": [
                "main-prior-user-node",
                "main-prior-assistant-node",
                "main-prior-compaction-node"
            ],
            "prefix_run_ids": [prior],
            "messages": [serde_json::to_value(checkpoint_message).expect("message value")],
            "stable_history_end": 1,
            "current_user_start": 1,
            "latest_compaction_summary_end": 1
        }))
        .expect("encode main checkpoint"),
    };
    let recording = RecordingStore::with_checkpoint(&store, checkpoint);
    let resumed = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &PromptHistoryCompiler::cache(),
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("unrelated branch leaves checkpoint usable");
    let full = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("full main-timeline fold");

    assert_eq!(resumed, full);
    assert!(
        !recording.read_cursors().contains(&0),
        "another timeline's compaction must not force a full replay"
    );
}

/// MUTATION CHECK: replace the timeline-scoped `append_prefixes.retain` with
/// `append_prefixes.clear()` whenever any compaction epoch changes. Expected
/// runtime failure: the final main compile rereads `main_summary` after only
/// the other agent timeline compacted.
#[tokio::test]
async fn warm_compaction_epoch_invalidation_is_timeline_scoped() {
    let store = MemoryStore::new();
    let cache = PromptHistoryCompiler::cache();
    let session_id = SessionId::new("warm-scoped-compaction-session");
    let main_prior = RunId::new("warm-main-prior");
    let main_warm = RunId::new("warm-main-current");
    let main_next = RunId::new("warm-main-next");
    let other_agent = AgentId::new("warm-other-agent");
    let other_prior = RunId::new("warm-other-prior");
    let other_warm = RunId::new("warm-other-current");
    let other_next = RunId::new("warm-other-next");
    let main_summary = ArtifactRef::new(format!("blake3:{}", "d".repeat(64)));
    let other_summary = ArtifactRef::new(format!("blake3:{}", "e".repeat(64)));
    let other_next_summary = ArtifactRef::new(format!("blake3:{}", "f".repeat(64)));
    let artifacts = CountingArtifacts {
        values: HashMap::from([
            (main_summary.clone(), b"main compacted summary".to_vec()),
            (other_summary.clone(), b"other compacted summary".to_vec()),
            (
                other_next_summary.clone(),
                b"other compacted summary again".to_vec(),
            ),
        ]),
        reads: Mutex::new(Vec::new()),
    };
    let scoped = |mut envelope: haider_protocol::envelope::RawEnvelope| {
        envelope.agent_id = Some(other_agent.clone());
        envelope
    };
    let mut events = vec![
        envelope(
            &session_id,
            &main_prior,
            "warm-main-user-message",
            EventPayload::UserMessage {
                text: "main history".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &main_prior,
            "warm-main-user-node",
            None,
            NodeKind::UserTurn {
                text: "main history".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &main_prior,
            "warm-main-answer-item",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("warm-main-answer-item"),
                item: TurnItem::AgentMessage {
                    text: "main answer".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &main_prior,
            "warm-main-answer-node",
            Some("warm-main-user-node"),
            NodeKind::AssistantCommit {
                text: "main answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        node(
            &session_id,
            &main_prior,
            "warm-main-compaction",
            Some("warm-main-answer-node"),
            NodeKind::Compaction {
                covers_from: NodeId::new("warm-main-user-node"),
                covers_to: NodeId::new("warm-main-answer-node"),
                summary_artifact: main_summary.clone(),
                tokens_before: 100,
                tokens_after: 10,
                resume_cause: CompactionResume::ManualIdle,
            },
        ),
        envelope(
            &session_id,
            &main_prior,
            "warm-main-compaction-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &main_warm,
            "warm-main-current-message",
            EventPayload::UserMessage {
                text: "main warm request".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &main_warm,
            "warm-main-current-node",
            Some("warm-main-compaction"),
            NodeKind::UserTurn {
                text: "main warm request".into(),
                attachments: Vec::new(),
            },
        ),
        scoped(envelope(
            &session_id,
            &other_prior,
            "warm-other-user-message",
            EventPayload::UserMessage {
                text: "other history".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        )),
        scoped(node(
            &session_id,
            &other_prior,
            "warm-other-user-node",
            None,
            NodeKind::UserTurn {
                text: "other history".into(),
                attachments: Vec::new(),
            },
        )),
        scoped(envelope(
            &session_id,
            &other_prior,
            "warm-other-answer-item",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("warm-other-answer-item"),
                item: TurnItem::AgentMessage {
                    text: "other answer".into(),
                },
            }),
            PromptRender::Verbatim,
        )),
        scoped(node(
            &session_id,
            &other_prior,
            "warm-other-answer-node",
            Some("warm-other-user-node"),
            NodeKind::AssistantCommit {
                text: "other answer".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        )),
        scoped(node(
            &session_id,
            &other_prior,
            "warm-other-compaction",
            Some("warm-other-answer-node"),
            NodeKind::Compaction {
                covers_from: NodeId::new("warm-other-user-node"),
                covers_to: NodeId::new("warm-other-answer-node"),
                summary_artifact: other_summary.clone(),
                tokens_before: 80,
                tokens_after: 8,
                resume_cause: CompactionResume::ManualIdle,
            },
        )),
        scoped(envelope(
            &session_id,
            &other_prior,
            "warm-other-compaction-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        )),
        scoped(envelope(
            &session_id,
            &other_warm,
            "warm-other-current-message",
            EventPayload::UserMessage {
                text: "other warm request".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        )),
        scoped(node(
            &session_id,
            &other_warm,
            "warm-other-current-node",
            Some("warm-other-compaction"),
            NodeKind::UserTurn {
                text: "other warm request".into(),
                attachments: Vec::new(),
            },
        )),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append both warm timelines");

    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &main_warm,
    )
    .await
    .expect("prime real main warm cache");
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        Some(&other_agent),
        &other_warm,
    )
    .await
    .expect("prime real other-agent warm cache");
    let main_reads_before = artifacts.read_count(&main_summary);

    let mut unrelated = vec![
        scoped(node(
            &session_id,
            &other_next,
            "warm-other-next-compaction",
            Some("warm-other-current-node"),
            NodeKind::Compaction {
                covers_from: NodeId::new("warm-other-current-node"),
                covers_to: NodeId::new("warm-other-current-node"),
                summary_artifact: other_next_summary,
                tokens_before: 60,
                tokens_after: 6,
                resume_cause: CompactionResume::ManualIdle,
            },
        )),
        scoped(envelope(
            &session_id,
            &other_next,
            "warm-other-next-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        )),
        scoped(envelope(
            &session_id,
            &other_next,
            "warm-other-next-message",
            EventPayload::UserMessage {
                text: "other request after compaction".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        )),
        scoped(node(
            &session_id,
            &other_next,
            "warm-other-next-node",
            Some("warm-other-next-compaction"),
            NodeKind::UserTurn {
                text: "other request after compaction".into(),
                attachments: Vec::new(),
            },
        )),
    ];
    StoreHandle::append(&store, &mut unrelated)
        .await
        .expect("append unrelated compaction");
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        Some(&other_agent),
        &other_next,
    )
    .await
    .expect("compile unrelated compacted timeline");

    let mut main_suffix = vec![
        envelope(
            &session_id,
            &main_warm,
            "warm-main-current-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &main_next,
            "warm-main-next-message",
            EventPayload::UserMessage {
                text: "main request after unrelated compaction".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &main_next,
            "warm-main-next-node",
            Some("warm-main-current-node"),
            NodeKind::UserTurn {
                text: "main request after unrelated compaction".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut main_suffix)
        .await
        .expect("append main suffix");
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &main_next,
    )
    .await
    .expect("extend main warm prefix");
    assert_eq!(
        artifacts.read_count(&main_summary),
        main_reads_before,
        "another timeline's compaction must not reread the main summary artifact"
    );
}

/// MUTATION CHECK: replace `.ok()?` on the checkpoint JSON decode with
/// `.expect("checkpoint JSON")`. Expected runtime failure: corrupt cache bytes
/// panic instead of falling back to the authoritative journal. Deleting the
/// `decoded.shape_version != PROMPT_CHECKPOINT_SHAPE_VERSION` guard makes the
/// older-shape fixture pollute the prompt and fail resumed/full equality.
#[tokio::test]
async fn unreadable_checkpoint_falls_back_to_full_replay() {
    let store = MemoryStore::new();
    let artifacts = TestArtifacts(HashMap::new());
    let session_id = SessionId::new("unreadable-checkpoint-session");
    let prior = RunId::new("unreadable-checkpoint-prior");
    let current = RunId::new("unreadable-checkpoint-current");
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "unreadable-checkpoint-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "unreadable-checkpoint-user",
            EventPayload::UserMessage {
                text: "journal remains authoritative".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append authoritative journal");
    let corrupt_checkpoint = SessionProjectionCheckpoint {
        session_id: session_id.clone(),
        projection: "prompt_history".into(),
        timeline_key: "corrupt-main-key".into(),
        through_seq: 1,
        boundary_event_id: events[0].event_id.clone(),
        payload: b"not valid checkpoint JSON".to_vec(),
    };
    let recording = RecordingStore::with_checkpoint(&store, corrupt_checkpoint);

    let resumed = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &PromptHistoryCompiler::cache(),
        &recording,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("corrupt checkpoint falls back");
    let full = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("fold authoritative journal");

    assert_eq!(resumed, full);
    assert!(
        recording.read_cursors().contains(&0),
        "corrupt checkpoint bytes must trigger replay from sequence zero"
    );

    let obsolete_message = Message {
        role: MessageRole::User,
        blocks: vec![Block::Text {
            text: "obsolete checkpoint state".into(),
        }],
    };
    let obsolete_checkpoint = SessionProjectionCheckpoint {
        session_id: session_id.clone(),
        projection: "prompt_history".into(),
        timeline_key: "obsolete-main-key".into(),
        through_seq: 1,
        boundary_event_id: events[0].event_id.clone(),
        payload: serde_json::to_vec(&serde_json::json!({
            "shape_version": 0,
            "reducer_version": "prompt-history-v1",
            "through_seq": 1,
            "boundary_event_id": events[0].event_id,
            "boundary_run_id": prior,
            "compaction_epoch": 1,
            "messages": [serde_json::to_value(obsolete_message).expect("message value")],
            "stable_history_end": 1,
            "current_user_start": 1,
            "latest_compaction_summary_end": 1
        }))
        .expect("encode obsolete checkpoint"),
    };
    let obsolete_recording = RecordingStore::with_checkpoint(&store, obsolete_checkpoint);
    let obsolete_fallback =
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &PromptHistoryCompiler::cache(),
            &obsolete_recording,
            &artifacts,
            &session_id,
            None,
            None,
            &current,
        )
        .await
        .expect("older checkpoint shape falls back");
    assert_eq!(obsolete_fallback, full);
    assert!(
        obsolete_recording.read_cursors().contains(&0),
        "an older checkpoint shape must trigger replay from sequence zero"
    );
}

/// Manual timing probe for the cold-cache path. Kept ignored because elapsed
/// time is diagnostic, while the ordinary equivalence test above owns the
/// correctness gate.
#[tokio::test]
#[ignore = "manual multi-compaction cold-cache timing probe"]
async fn measure_cold_fold_after_several_compactions() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("open timing store");
    let session_id = SessionId::new("multi-compaction-timing-session");
    store
        .create_session(SessionCreateCommand {
            command_id: "create-timing-session".into(),
            request_digest: "create-timing-session-digest".into(),
            request_json: r#"{"session":"multi-compaction-timing-session"}"#.into(),
            session_id: session_id.clone(),
            cwd: std::env::current_dir()
                .expect("cwd")
                .to_string_lossy()
                .into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test-v1".into(),
            event_id: EventId::new("created-timing-session"),
            device_id: DeviceId::new("checkpoint-timing-test"),
        })
        .await
        .expect("create timing session");

    let mut artifacts = HashMap::new();
    let mut events = Vec::new();
    let mut previous_compaction = None::<NodeId>;
    for cycle in 0..5 {
        let run = RunId::new(format!("timing-run-{cycle}"));
        let user_node = NodeId::new(format!("timing-user-node-{cycle}"));
        let assistant_node = NodeId::new(format!("timing-assistant-node-{cycle}"));
        let compaction_node = NodeId::new(format!("timing-compaction-node-{cycle}"));
        let summary_ordinal = u64::try_from(cycle + 1).expect("small timing cycle");
        let summary_artifact = ArtifactRef::new(format!("blake3:{summary_ordinal:064x}"));
        artifacts.insert(
            summary_artifact.clone(),
            format!("summary after compaction {cycle}").into_bytes(),
        );
        events.push(envelope(
            &session_id,
            &run,
            &format!("timing-user-{cycle}"),
            EventPayload::UserMessage {
                text: format!("user turn {cycle}"),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ));
        events.push(node(
            &session_id,
            &run,
            user_node.as_str(),
            previous_compaction.as_ref().map(NodeId::as_str),
            NodeKind::UserTurn {
                text: format!("user turn {cycle}"),
                attachments: Vec::new(),
            },
        ));
        events.push(envelope(
            &session_id,
            &run,
            &format!("timing-assistant-{cycle}"),
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(format!("timing-answer-{cycle}")),
                item: TurnItem::AgentMessage {
                    text: format!("assistant turn {cycle}"),
                },
            }),
            PromptRender::Verbatim,
        ));
        events.push(node(
            &session_id,
            &run,
            assistant_node.as_str(),
            Some(user_node.as_str()),
            NodeKind::AssistantCommit {
                text: format!("assistant turn {cycle}"),
                verdict: VerifyVerdict::NotApplicable,
            },
        ));
        for filler in 0..600 {
            let mut ignored = envelope(
                &session_id,
                &run,
                &format!("timing-filler-{cycle}-{filler}"),
                EventPayload::RunState(RunState::Thinking),
                PromptRender::Omit,
            );
            ignored.payload = serde_json::json!({
                "type": "timing_filler",
                "padding": "0123456789abcdef0123456789abcdef"
            });
            events.push(ignored);
        }
        events.push(node(
            &session_id,
            &run,
            compaction_node.as_str(),
            Some(assistant_node.as_str()),
            NodeKind::Compaction {
                covers_from: previous_compaction
                    .clone()
                    .unwrap_or_else(|| user_node.clone()),
                covers_to: assistant_node.clone(),
                summary_artifact,
                tokens_before: 10_000,
                tokens_after: 100,
                resume_cause: CompactionResume::ManualIdle,
            },
        ));
        events.push(envelope(
            &session_id,
            &run,
            &format!("timing-done-{cycle}"),
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ));
        previous_compaction = Some(compaction_node);
    }
    let current = RunId::new("timing-current");
    events.push(envelope(
        &session_id,
        &current,
        "timing-current-user",
        EventPayload::UserMessage {
            text: "measure the suffix".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
        PromptRender::Verbatim,
    ));
    events.push(node(
        &session_id,
        &current,
        "timing-current-node",
        previous_compaction.as_ref().map(NodeId::as_str),
        NodeKind::UserTurn {
            text: "measure the suffix".into(),
            attachments: Vec::new(),
        },
    ));
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append timing journal");
    let journal_json_bytes = events
        .iter()
        .map(|event| {
            serde_json::to_vec(event)
                .expect("encode timing event")
                .len()
        })
        .sum::<usize>();
    let artifacts = TestArtifacts(artifacts);

    let oracle = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("compile timing oracle");
    PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &PromptHistoryCompiler::cache(),
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current,
    )
    .await
    .expect("install latest boundary checkpoint");

    let mut full_micros = Vec::new();
    let mut resumed_micros = Vec::new();
    for _ in 0..9 {
        let started = std::time::Instant::now();
        let full = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
            &store,
            &artifacts,
            &session_id,
            None,
            None,
            &current,
        )
        .await
        .expect("timed full fold");
        full_micros.push(started.elapsed().as_micros());
        assert_eq!(full, oracle);
    }
    for _ in 0..9 {
        let recording = RecordingStore::new(&store);
        let started = std::time::Instant::now();
        let resumed = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &PromptHistoryCompiler::cache(),
            &recording,
            &artifacts,
            &session_id,
            None,
            None,
            &current,
        )
        .await
        .expect("timed resumed fold");
        resumed_micros.push(started.elapsed().as_micros());
        assert_eq!(resumed, oracle);
        assert!(!recording.read_cursors().contains(&0));
    }
    full_micros.sort_unstable();
    resumed_micros.sort_unstable();
    eprintln!(
        "multi-compaction cold fold: envelopes={} journal_json_bytes={} full_median_us={} resumed_median_us={}",
        events.len(),
        journal_json_bytes,
        full_micros[full_micros.len() / 2],
        resumed_micros[resumed_micros.len() / 2]
    );
    store.close().await.expect("close timing store");
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
                    data: None,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status: haider_protocol::tool::ToolResultStatus::Completed,
                    reason: None,
                    presentation: None,
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

#[tokio::test]
async fn journal_keeps_full_tool_output_while_replay_builds_the_same_compact_model_view() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("raw-tool-journal-session");
    let prior = RunId::new("raw-tool-journal-prior");
    let current = RunId::new("raw-tool-journal-current");
    let raw = format!(
        "HEAD identifies the command\n{}TAIL diagnostic: assertion failed\n",
        "boilerplate\n".repeat(2_000)
    );
    let durable_result = BoundedResult {
        preview: raw.clone(),
        truncated: false,
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: haider_protocol::tool::ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    };
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "raw-tool-result",
            EventPayload::ToolResult {
                call_id: "raw-call".into(),
                result: durable_result,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "raw-tool-call",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("raw-tool-item"),
                item: TurnItem::ToolCall {
                    call_id: "raw-call".into(),
                    name: "peer_list".into(),
                    args: serde_json::json!({}),
                    status: ToolStatus::Completed,
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "raw-tool-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "raw-tool-next-user",
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
        .expect("append raw tool history");

    let stored = StoreHandle::read(&store, &session_id, 0, 64)
        .await
        .expect("read raw tool journal");
    let stored_preview = stored
        .into_iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
        .find_map(|payload| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "raw-call" => {
                Some(result.preview)
            }
            _ => None,
        })
        .expect("durable raw tool result");
    assert_eq!(stored_preview, raw);
    assert!(!stored_preview.contains("haider_elision_v1"));

    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current)
        .await
        .expect("compile model projection");
    let block = messages
        .iter()
        .find_map(|message| message.tool_result_for("raw-call"))
        .expect("model tool result");
    let Block::ToolResult {
        preview, truncated, ..
    } = block
    else {
        panic!("tool-result lookup returns a tool-result block");
    };
    assert!(*truncated);
    assert!(preview.len() <= 8 * 1024);
    assert!(preview.starts_with("HEAD identifies the command"));
    assert!(preview.contains("\"haider_elision_v1\""));
    assert!(preview.ends_with("TAIL diagnostic: assertion failed\n"));

    let replay = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current)
        .await
        .expect("replay same model projection");
    assert_eq!(messages, replay, "model-boundary elision is deterministic");
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
    let inherited_command = ItemId::new("named-inherited-command");
    let inherited_origin = UserCommandOriginV1 {
        origin: CommandExecutionOrigin::UserCommand,
        command_item_id: inherited_command.clone(),
        call_id: "named-inherited-call".into(),
    };
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
            envelope(
                &tree_session,
                &source_run,
                "source-command",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: inherited_command,
                    item: TurnItem::CommandExecution {
                        call_id: "named-inherited-call".into(),
                        command: "printf inherited".into(),
                        status: ToolStatus::Completed,
                        exit_code: Some(0),
                    },
                }),
                PromptRender::Verbatim,
            ),
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
                fork_seq: 3,
                created_seq,
                created_at_ms: 1,
                head_node_id: current_node.clone(),
                head_seq: 21,
            },
        }
        .to_payload_value()
        .expect("branch payload"),
    });
    StoreHandle::append(&tree_store, &mut tree_events)
        .await
        .expect("append named lineage");
    let cache = PromptHistoryCompiler::cache();
    let artifacts = TestArtifacts(HashMap::new());
    let recording = RecordingStore::new(&tree_store);
    let cached_main = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &recording,
        &artifacts,
        &tree_session,
        None,
        Some(&agent),
        &source_suffix_run,
    )
    .await
    .expect("compile cached main-branch projection");
    let fresh_main = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &tree_store,
        &artifacts,
        &tree_session,
        None,
        Some(&agent),
        &source_suffix_run,
    )
    .await
    .expect("compile fresh main-branch projection");
    assert_eq!(cached_main, fresh_main);
    let named_messages = PromptHistoryCompiler::compile(
        &tree_store,
        &tree_session,
        Some(&branch),
        Some(&agent),
        &current,
    )
    .await
    .expect("compile named lineage");
    let cached_named = PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
        &cache,
        &recording,
        &artifacts,
        &tree_session,
        Some(&branch),
        Some(&agent),
        &current,
    )
    .await
    .expect("compile cached named-branch projection");
    let fresh_named = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &tree_store,
        &artifacts,
        &tree_session,
        Some(&branch),
        Some(&agent),
        &current,
    )
    .await
    .expect("compile fresh named-branch projection");
    assert_eq!(cached_named, fresh_named);
    assert_eq!(cached_named.messages, named_messages);

    // MUTATION CHECK: drop branch/agent/current-run identity from the exact
    // prefix match, or rebuild the tree after this append. Expected runtime
    // failure: cached/fresh bytes diverge or the lineage-read count advances.
    let lineage_reads_before_increment = recording.lineage_read_count();
    let mut named_increment = vec![
        tree_scoped(
            tree_user(&current, "named-current-steer", "branch current steer"),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            tree_node(&current, "named-current-steer-node", Some(&current_node)),
            Some(&branch),
            &agent,
        ),
    ];
    StoreHandle::append(&tree_store, &mut named_increment)
        .await
        .expect("append named incremental suffix");
    let incrementally_cached =
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &cache,
            &recording,
            &artifacts,
            &tree_session,
            Some(&branch),
            Some(&agent),
            &current,
        )
        .await
        .expect("extend exact named-branch projection");
    let incrementally_fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &tree_store,
        &artifacts,
        &tree_session,
        Some(&branch),
        Some(&agent),
        &current,
    )
    .await
    .expect("fully rebuild named-branch projection");
    assert_eq!(incrementally_cached, incrementally_fresh);
    assert_eq!(
        recording.lineage_read_count(),
        lineage_reads_before_increment,
        "an exact branch+agent+run suffix must reuse its indexed ancestry"
    );

    // Facts are timeline-wide within each ancestry owner, not fork-limited.
    // A main-branch origin appended after the fork can therefore reclassify a
    // command in the inherited prefix; the named-branch cache must rebuild.
    let mut inherited_fact = vec![
        tree_scoped(
            envelope(
                &tree_session,
                &source_run,
                "source-command-origin-after-fork",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("named-inherited-origin-item"),
                    item: inherited_origin.extension_item().expect("origin item"),
                }),
                PromptRender::Omit,
            ),
            None,
            &agent,
        ),
        tree_scoped(
            tree_user(
                &current,
                "named-current-steer-after-origin",
                "branch current after inherited origin",
            ),
            Some(&branch),
            &agent,
        ),
        tree_scoped(
            tree_node(
                &current,
                "named-current-steer-after-origin-node",
                Some(&NodeId::new("named-current-steer-node")),
            ),
            Some(&branch),
            &agent,
        ),
    ];
    StoreHandle::append(&tree_store, &mut inherited_fact)
        .await
        .expect("append inherited-owner retroactive fact");
    let inherited_cached =
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &cache,
            &recording,
            &artifacts,
            &tree_session,
            Some(&branch),
            Some(&agent),
            &current,
        )
        .await
        .expect("rebuild named branch after inherited-owner fact");
    let inherited_fresh = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &tree_store,
        &artifacts,
        &tree_session,
        Some(&branch),
        Some(&agent),
        &current,
    )
    .await
    .expect("fully compile inherited-owner fact");
    assert_eq!(inherited_cached, inherited_fresh);
    assert!(inherited_cached.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text.contains("printf inherited")))
    }));
    let cached_other_agent_error =
        PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &cache,
            &recording,
            &artifacts,
            &tree_session,
            Some(&branch),
            Some(&other_agent),
            &wrong_agent,
        )
        .await
        .expect_err("invalid switched-agent ancestry fails from cache");
    let fresh_other_agent_error =
        PromptHistoryCompiler::compile_provider_projection_with_artifacts(
            &tree_store,
            &artifacts,
            &tree_session,
            Some(&branch),
            Some(&other_agent),
            &wrong_agent,
        )
        .await
        .expect_err("invalid switched-agent ancestry fails fresh");
    assert_eq!(cached_other_agent_error.code, fresh_other_agent_error.code);
    assert_eq!(
        cached_other_agent_error.message,
        fresh_other_agent_error.message
    );
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

/// MUTATION CHECK (LT3): gate task facts behind the run-terminal filter,
/// drop the `render.prompt == Omit` off switch, or stop rendering the
/// bounded notice. Expected RUNTIME failure: the completion of a task whose
/// spawning run was CANCELLED disappears from the next turn's prompt, the
/// steer-delivered (Omit) completion leaks a second prompt copy, or the
/// notice literals below change.
#[tokio::test]
async fn task_facts_reach_the_next_turn_prompt_and_omit_is_the_off_switch() {
    use haider_protocol::task::{
        TaskCompleted, TaskCompletionDelivery, TaskEventPayload, TaskStarted, TaskTerminalState,
    };
    let store = MemoryStore::new();
    let session_id = SessionId::new("task-prompt-session");
    let spawning_run = RunId::new("task-prompt-run-1");
    let current_run = RunId::new("task-prompt-run-2");
    let task_fact = |event_id: &str, payload: &TaskEventPayload, prompt: PromptRender| {
        let mut fact = envelope(
            &session_id,
            &spawning_run,
            event_id,
            EventPayload::IdleDecayed,
            prompt,
        );
        fact.payload = payload.to_payload_value().expect("task payload");
        fact
    };
    let started = TaskEventPayload::TaskStarted(TaskStarted {
        task: haider_protocol::ids::TaskId::new("task-11"),
        name: "watcher".into(),
        command: "cargo watch -x test".into(),
        pid: 999,
        started_at_ms: 1,
    });
    let completed = TaskEventPayload::TaskCompleted(TaskCompleted {
        task: haider_protocol::ids::TaskId::new("task-11"),
        name: "watcher".into(),
        state: TaskTerminalState::Completed { exit_code: Some(0) },
        elapsed_ms: 42_000,
        output_bytes: 900_000,
        tail: "test result: ok\n".into(),
        artifact: Some(ArtifactRef::new("blake3:task-output")),
        truncated: true,
        full_output_unavailable: false,
        delivery: TaskCompletionDelivery::DeliveredQueued,
        workspace_mutation: None,
    });
    let steered = TaskEventPayload::TaskCompleted(TaskCompleted {
        task: haider_protocol::ids::TaskId::new("task-12"),
        name: "steered".into(),
        state: TaskTerminalState::Killed,
        elapsed_ms: 1_000,
        output_bytes: 0,
        tail: String::new(),
        artifact: None,
        truncated: false,
        full_output_unavailable: false,
        delivery: TaskCompletionDelivery::DeliveredSteer,
        workspace_mutation: None,
    });
    let mut events = vec![
        envelope(
            &session_id,
            &spawning_run,
            "task-prompt-user-1",
            EventPayload::UserMessage {
                text: "start the watcher".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        task_fact("task-prompt-started", &started, PromptRender::Verbatim),
        // The spawning run was CANCELLED (esc) — the task outlives it by
        // design, so its facts must still reach the next prompt.
        envelope(
            &session_id,
            &spawning_run,
            "task-prompt-run-1-state",
            EventPayload::RunState(RunState::Cancelled),
            PromptRender::Omit,
        ),
        task_fact("task-prompt-completed", &completed, PromptRender::Verbatim),
        // Steer-delivered completion: the durable steer user message owns
        // the prompt copy, so the fact itself journals with Omit.
        task_fact("task-prompt-steered", &steered, PromptRender::Omit),
        envelope(
            &session_id,
            &current_run,
            "task-prompt-user-2",
            EventPayload::UserMessage {
                text: "how did it go".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append task prompt history");

    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current_run)
        .await
        .expect("compile next turn");
    let text = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    // The cancelled run's committed content (its user message) is visible —
    // any terminal run contributes its committed history. The task-fact
    // bypass stays load-bearing for facts whose spawning run is non-terminal
    // and non-current (e.g. parked awaiting input).
    assert_eq!(text[0], "start the watcher");
    assert_eq!(
        text[1],
        "[background task started] watcher (task-11) — cargo watch -x test"
    );
    assert!(text[2].starts_with(
        "[background task finished] watcher (task-11) exited with code 0 after 42s — \
         900000 output bytes (truncated; full retained output in the task artifact)\n\
         output tail:\n"
    ));
    assert!(text[2].contains("\"haider_elision_v1\""));
    assert!(text[2].contains("\"scope\":\"background_task_notice\""));
    assert!(text[2].ends_with("result: ok\n"));
    assert_eq!(text[3], "how did it go");
    assert_eq!(
        text.len(),
        4,
        "the Omit (steer-delivered) fact renders nothing: {text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("steered")),
        "no second prompt copy for a steer-delivered completion: {text:?}"
    );
}

/// MUTATION CHECK (dogfood bug 1 — context loss): restore the
/// `RunState::Done`-only visibility gate in `render_journal`
/// (`prior_state == Done` instead of `RunState::is_terminal`). Expected
/// runtime failure: the cancelled and errored runs' committed content —
/// including the USER'S OWN MESSAGES — vanishes from the next prompt, and
/// the prefix-stability assertion below breaks (the dogfood cache-miss
/// bug was this exact divergence).
#[tokio::test]
async fn cancelled_and_errored_runs_keep_their_committed_history() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("terminal-history-session");
    let run_1 = RunId::new("terminal-history-run-1");
    let run_2 = RunId::new("terminal-history-run-2");
    let run_3 = RunId::new("terminal-history-run-3");
    let interruption = haider_protocol::error::ErrorPresentation::new(
        "stream-interrupted",
        "Stream interrupted",
        "the turn was cancelled mid-stream",
        haider_protocol::error::ErrorScope::Turn,
        [haider_protocol::error::ErrorAction::Retry],
    );
    let mut events = vec![
        // run 1: CANCELLED after committing real work.
        envelope(
            &session_id,
            &run_1,
            "th-user-1",
            EventPayload::UserMessage {
                text: "fix the parser".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &run_1,
            "th-agent-1",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("th-item-agent-1"),
                item: TurnItem::AgentMessage {
                    text: "half analysis done".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &run_1,
            "th-tool-result-1",
            EventPayload::ToolResult {
                call_id: "th-call-1".into(),
                result: BoundedResult {
                    preview: "grep output".into(),
                    truncated: false,
                    data: None,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status: haider_protocol::tool::ToolResultStatus::Completed,
                    reason: None,
                    presentation: None,
                },
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &run_1,
            "th-tool-1",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("th-item-tool-1"),
                item: TurnItem::ToolCall {
                    call_id: "th-call-1".into(),
                    name: "fs_read".into(),
                    args: serde_json::json!({"path": "parser.rs"}),
                    status: ToolStatus::Completed,
                },
            }),
            PromptRender::Verbatim,
        ),
        // A tool call the cancel closed with NO result: never rendered —
        // an assistant tool_use without its result would corrupt the wire.
        envelope(
            &session_id,
            &run_1,
            "th-tool-2",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("th-item-tool-2"),
                item: TurnItem::ToolCall {
                    call_id: "th-call-2".into(),
                    name: "process_exec".into(),
                    args: serde_json::json!({"cmd": "cargo test"}),
                    status: ToolStatus::Cancelled,
                },
            }),
            PromptRender::Verbatim,
        ),
        // A torn partial stream with NO continue-partial answer: excluded.
        envelope(
            &session_id,
            &run_1,
            "th-torn-1",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("th-item-torn-1"),
                item: TurnItem::IncompleteAgentMessage {
                    text: "torn half-sentence".into(),
                    interruption,
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &run_1,
            "th-run-1-state",
            EventPayload::RunState(RunState::Cancelled),
            PromptRender::Omit,
        ),
        // run 2: ERRORED right after the user message.
        envelope(
            &session_id,
            &run_2,
            "th-user-2",
            EventPayload::UserMessage {
                text: "try again".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &run_2,
            "th-run-2-state",
            EventPayload::RunState(RunState::Errored),
            PromptRender::Omit,
        ),
        // run 3: the current turn.
        envelope(
            &session_id,
            &run_3,
            "th-user-3",
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
        .expect("append terminal history");

    let as_if_run_2 = PromptHistoryCompiler::compile(&store, &session_id, None, None, &run_2)
        .await
        .expect("compile as run 2");
    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &run_3)
        .await
        .expect("compile next turn");
    let text = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        text,
        [
            "fix the parser",
            "half analysis done",
            "try again",
            "continue"
        ],
        "every terminal run's committed content survives; torn content does not"
    );
    assert!(
        messages.iter().flat_map(|message| &message.blocks).any(
            |block| matches!(block, Block::ToolCall { call_id, .. } if call_id == "th-call-1")
        ),
        "the completed tool call is carried forward"
    );
    assert!(
        messages.iter().flat_map(|message| &message.blocks).any(
            |block| matches!(block, Block::ToolResult { call_id, .. } if call_id == "th-call-1")
        ),
        "…with its paired result"
    );
    assert!(
        !messages.iter().flat_map(|message| &message.blocks).any(
            |block| matches!(block, Block::ToolCall { call_id, .. } if call_id == "th-call-2")
        ),
        "a cancelled call with no result never emits an orphaned tool_use"
    );
    // Dogfood bug 2 (cache misses) was the divergence this pins: the next
    // turn's projection must EXTEND the prior turn's — byte-stable prefix.
    assert!(
        messages.len() > as_if_run_2.len() && messages[..as_if_run_2.len()] == as_if_run_2[..],
        "the cancel boundary preserves the carried-forward prompt prefix"
    );
}

#[derive(Clone, Copy)]
enum CompactionProtectedTurn {
    Skill(usize),
    Image(usize),
}

async fn seed_clean_compaction_history(
    name: &str,
    protected: Option<CompactionProtectedTurn>,
) -> (MemoryStore, SessionId, RunId, Vec<String>) {
    let store = MemoryStore::new();
    let session_id = SessionId::new(format!("{name}-session"));
    let mut parent = None::<String>;
    let mut retained_text = Vec::new();
    let mut events = Vec::new();
    for ordinal in 0..5 {
        let run_id = RunId::new(format!("{name}-run-{ordinal}"));
        let user_text = format!("{name}-user-{ordinal}");
        let assistant_text = if ordinal >= 3 {
            format!("{name}-assistant-{ordinal}-{}", "x".repeat(50_000))
        } else {
            format!("{name}-assistant-{ordinal}")
        };
        if ordinal >= 3 {
            retained_text.push(user_text.clone());
            retained_text.push(assistant_text.clone());
        }
        let attachments = match protected {
            Some(CompactionProtectedTurn::Skill(index)) if index == ordinal => {
                vec![AttachmentBlock::Skill {
                    name: "load-bearing-skill".into(),
                    version_hash: "sha256:skill-version".into(),
                }]
            }
            Some(CompactionProtectedTurn::Image(index)) if index == ordinal => {
                vec![AttachmentBlock::Image {
                    artifact: ArtifactRef::new("load-bearing-image"),
                    mime: "image/png".into(),
                    width: Some(64),
                    height: Some(32),
                }]
            }
            _ => Vec::new(),
        };
        let user_node = format!("{name}-user-node-{ordinal}");
        let assistant_node = format!("{name}-assistant-node-{ordinal}");
        events.extend([
            envelope(
                &session_id,
                &run_id,
                &format!("{name}-user-event-{ordinal}"),
                EventPayload::UserMessage {
                    text: user_text.clone(),
                    attachments: attachments.clone(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
            node(
                &session_id,
                &run_id,
                &user_node,
                parent.as_deref(),
                NodeKind::UserTurn {
                    text: user_text,
                    attachments,
                },
            ),
            envelope(
                &session_id,
                &run_id,
                &format!("{name}-assistant-event-{ordinal}"),
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new(format!("{name}-assistant-item-{ordinal}")),
                    item: TurnItem::AgentMessage {
                        text: assistant_text.clone(),
                    },
                }),
                PromptRender::Verbatim,
            ),
            node(
                &session_id,
                &run_id,
                &assistant_node,
                Some(&user_node),
                NodeKind::AssistantCommit {
                    text: assistant_text,
                    verdict: VerifyVerdict::NotApplicable,
                },
            ),
            envelope(
                &session_id,
                &run_id,
                &format!("{name}-done-{ordinal}"),
                EventPayload::RunState(RunState::Done),
                PromptRender::Omit,
            ),
        ]);
        parent = Some(assistant_node);
    }
    let current_run = RunId::new(format!("{name}-current-run"));
    let current_text = format!("{name}-current-user");
    retained_text.push(current_text.clone());
    events.extend([
        envelope(
            &session_id,
            &current_run,
            &format!("{name}-current-user-event"),
            EventPayload::UserMessage {
                text: current_text.clone(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current_run,
            &format!("{name}-current-user-node"),
            parent.as_deref(),
            NodeKind::UserTurn {
                text: current_text,
                attachments: Vec::new(),
            },
        ),
    ]);
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append clean compaction history");
    (store, session_id, current_run, retained_text)
}

#[tokio::test]
async fn compaction_split_is_always_a_clean_turn_boundary() {
    let (store, session_id, current_run, _) =
        seed_clean_compaction_history("clean-boundary", None).await;
    let artifacts = TestArtifacts(HashMap::new());

    let planned = PromptHistoryCompiler::plan_compaction(
        &store,
        &artifacts,
        PromptCompactionPlanRequest {
            session_id: &session_id,
            branch_id: None,
            agent_id: None,
            current_run: &current_run,
            operation_id: "clean-boundary-operation".into(),
            resume_cause: CompactionResume::AutoMidTurn,
        },
    )
    .await
    .expect("plan clean-boundary compaction");

    assert_eq!(
        planned.intent.covers_from,
        NodeId::new("clean-boundary-user-node-0")
    );
    assert_eq!(
        planned.intent.covers_to,
        NodeId::new("clean-boundary-assistant-node-2")
    );
    assert_eq!(planned.covered_message_count, 6);
}

#[tokio::test]
async fn recent_context_window_stays_verbatim_after_the_summary_boundary() {
    let (store, session_id, current_run, retained_text) =
        seed_clean_compaction_history("recent-verbatim", None).await;
    let artifacts = TestArtifacts(HashMap::new());
    let planned = PromptHistoryCompiler::plan_compaction(
        &store,
        &artifacts,
        PromptCompactionPlanRequest {
            session_id: &session_id,
            branch_id: None,
            agent_id: None,
            current_run: &current_run,
            operation_id: "recent-verbatim-operation".into(),
            resume_cause: CompactionResume::AutoMidTurn,
        },
    )
    .await
    .expect("plan recent-window compaction");
    let projection = PromptHistoryCompiler::compile_with_artifacts(
        &store,
        &artifacts,
        &session_id,
        None,
        None,
        &current_run,
    )
    .await
    .expect("compile recent-window projection");
    let actual = projection[planned.covered_message_count..]
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(actual, retained_text);
}

#[tokio::test]
async fn skill_turn_is_never_folded_into_a_summary() {
    let (store, session_id, current_run, _) =
        seed_clean_compaction_history("skill-preserved", Some(CompactionProtectedTurn::Skill(1)))
            .await;
    let artifacts = TestArtifacts(HashMap::new());

    let planned = PromptHistoryCompiler::plan_compaction(
        &store,
        &artifacts,
        PromptCompactionPlanRequest {
            session_id: &session_id,
            branch_id: None,
            agent_id: None,
            current_run: &current_run,
            operation_id: "skill-preserved-operation".into(),
            resume_cause: CompactionResume::AutoMidTurn,
        },
    )
    .await
    .expect("plan around protected skill turn");

    assert_eq!(
        planned.intent.covers_to,
        NodeId::new("skill-preserved-assistant-node-0")
    );
    assert_eq!(planned.covered_message_count, 2);
}

#[tokio::test]
async fn image_turn_moves_the_summary_boundary_back_and_stays_whole() {
    let (store, session_id, current_run, _) =
        seed_clean_compaction_history("image-preserved", Some(CompactionProtectedTurn::Image(1)))
            .await;
    let artifacts = TestArtifacts(HashMap::new());

    let planned = PromptHistoryCompiler::plan_compaction(
        &store,
        &artifacts,
        PromptCompactionPlanRequest {
            session_id: &session_id,
            branch_id: None,
            agent_id: None,
            current_run: &current_run,
            operation_id: "image-preserved-operation".into(),
            resume_cause: CompactionResume::AutoMidTurn,
        },
    )
    .await
    .expect("plan around protected image turn");

    assert_eq!(
        planned.intent.covers_to,
        NodeId::new("image-preserved-assistant-node-0")
    );
    assert_eq!(planned.covered_message_count, 2);
}

#[tokio::test]
async fn structural_selection_replays_from_the_append_only_journal_after_restart() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("structural-replay-session");
    let prior_run = RunId::new("structural-replay-prior");
    let current_run = RunId::new("structural-replay-current");
    let reused_id_run = RunId::new("structural-replay-reused-id");
    let replay_run = RunId::new("structural-replay-final");
    let (_, savings) = haider_protocol::context::ContextEconomy::default()
        .record_with_removed_tool_calls(
            haider_protocol::context::ContextCompactionTier::StructuralTrim24,
            60_000,
            48_000,
            vec!["stale-call".into()],
        );
    let savings_item = savings.extension_item().expect("savings carrier");
    let mut events = vec![
        envelope(
            &session_id,
            &prior_run,
            "structural-replay-user",
            EventPayload::UserMessage {
                text: "inspect old tool output".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior_run,
            "structural-replay-user-node",
            None,
            NodeKind::UserTurn {
                text: "inspect old tool output".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &prior_run,
            "structural-replay-result",
            EventPayload::ToolResult {
                call_id: "stale-call".into(),
                result: BoundedResult {
                    preview: "complete stale file contents".into(),
                    truncated: false,
                    data: None,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status: haider_protocol::tool::ToolResultStatus::Completed,
                    reason: None,
                    presentation: None,
                },
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior_run,
            "structural-replay-call",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("structural-replay-call-item"),
                item: TurnItem::ToolCall {
                    call_id: "stale-call".into(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "/large.log"}),
                    status: ToolStatus::Completed,
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &prior_run,
            "structural-replay-assistant-node",
            Some("structural-replay-user-node"),
            NodeKind::AssistantCommit {
                text: String::new(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &prior_run,
            "structural-replay-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current_run,
            "structural-replay-current-user",
            EventPayload::UserMessage {
                text: "continue from prose".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current_run,
            "structural-replay-current-node",
            Some("structural-replay-assistant-node"),
            NodeKind::UserTurn {
                text: "continue from prose".into(),
                attachments: Vec::new(),
            },
        ),
        // Crash-window pin: the trim is durable after the current user node,
        // before any assistant node can anchor it into the tree ancestry.
        envelope(
            &session_id,
            &current_run,
            "structural-replay-saving",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("structural-replay-saving-item"),
                item: savings_item,
            }),
            PromptRender::Omit,
        ),
        node(
            &session_id,
            &current_run,
            "structural-replay-current-assistant-node",
            Some("structural-replay-current-node"),
            NodeKind::AssistantCommit {
                text: String::new(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &current_run,
            "structural-replay-current-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &reused_id_run,
            "structural-replay-reused-user",
            EventPayload::UserMessage {
                text: "reuse a provider-scoped call id".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &reused_id_run,
            "structural-replay-reused-user-node",
            Some("structural-replay-current-assistant-node"),
            NodeKind::UserTurn {
                text: "reuse a provider-scoped call id".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &reused_id_run,
            "structural-replay-reused-result",
            EventPayload::ToolResult {
                call_id: "stale-call".into(),
                result: BoundedResult {
                    preview: "current tool result must survive".into(),
                    truncated: false,
                    data: None,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status: haider_protocol::tool::ToolResultStatus::Completed,
                    reason: None,
                    presentation: None,
                },
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &reused_id_run,
            "structural-replay-reused-call",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("structural-replay-reused-call-item"),
                item: TurnItem::ToolCall {
                    call_id: "stale-call".into(),
                    name: "current_read_file".into(),
                    args: serde_json::json!({"path": "/current.log"}),
                    status: ToolStatus::Completed,
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &reused_id_run,
            "structural-replay-reused-assistant-node",
            Some("structural-replay-reused-user-node"),
            NodeKind::AssistantCommit {
                text: String::new(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &reused_id_run,
            "structural-replay-reused-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &replay_run,
            "structural-replay-final-user",
            EventPayload::UserMessage {
                text: "compile after the reused id".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &replay_run,
            "structural-replay-final-user-node",
            Some("structural-replay-reused-assistant-node"),
            NodeKind::UserTurn {
                text: "compile after the reused id".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append structural replay fixture");
    let raw = StoreHandle::read(&store, &session_id, 0, 100)
        .await
        .expect("read raw structural journal");
    assert!(
        raw.iter()
            .any(|envelope| envelope.event_id.as_str() == "structural-replay-call")
    );
    assert!(
        raw.iter()
            .any(|envelope| envelope.event_id.as_str() == "structural-replay-result")
    );

    let first = PromptHistoryCompiler::compile(&store, &session_id, None, None, &replay_run)
        .await
        .expect("first projection after trim");
    let restarted = PromptHistoryCompiler::compile(&store, &session_id, None, None, &replay_run)
        .await
        .expect("independent restart projection after trim");
    assert_eq!(first, restarted);
    let surviving_reused_id_blocks = first
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| {
            matches!(
                block,
                Block::ToolCall { call_id, .. } | Block::ToolResult { call_id, .. }
                    if call_id == "stale-call"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        surviving_reused_id_blocks.len(),
        2,
        "restart removes one recorded old occurrence, not every future reuse: {surviving_reused_id_blocks:#?}"
    );
    assert!(
        first
            .iter()
            .flat_map(|message| &message.blocks)
            .any(|block| {
                matches!(
                    block,
                    Block::ToolCall { call_id, name, .. }
                        if call_id == "stale-call" && name == "current_read_file"
                )
            })
    );
    assert_eq!(
        PromptHistoryCompiler::latest_context_economy(&store, &session_id)
            .await
            .expect("reduce durable savings")
            .expect("savings coordinate")
            .cumulative_estimated_tokens_saved,
        12_000
    );
}

#[tokio::test]
async fn malformed_context_savings_event_fails_closed_during_restart_replay() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("malformed-context-savings-session");
    let run_id = RunId::new("malformed-context-savings-run");
    let mut events = vec![
        envelope(
            &session_id,
            &run_id,
            "malformed-context-savings-user",
            EventPayload::UserMessage {
                text: "current turn".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &run_id,
            "malformed-context-savings-node",
            None,
            NodeKind::UserTurn {
                text: "current turn".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &run_id,
            "malformed-context-savings-event",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("malformed-context-savings-item"),
                item: TurnItem::Extension {
                    kind: haider_protocol::context::CONTEXT_SAVINGS_EXTENSION_KIND.into(),
                    data: serde_json::json!({"operation_count": "not-a-number"}),
                },
            }),
            PromptRender::Omit,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append malformed savings fixture");

    let error = PromptHistoryCompiler::compile(&store, &session_id, None, None, &run_id)
        .await
        .expect_err("known authoritative savings kind must not disappear on decode failure");
    assert_eq!(error.code, haider_protocol::error::ErrorCode::StoreCorrupt);
    assert!(error.message.contains("context-savings event is malformed"));
}

#[tokio::test]
async fn replacement_summary_source_never_contains_the_prior_summary() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("summary-replacement-source-session");
    let first_run = RunId::new("summary-replacement-first");
    let second_run = RunId::new("summary-replacement-second");
    let current_run = RunId::new("summary-replacement-current");
    let mut events = vec![
        envelope(
            &session_id,
            &first_run,
            "summary-replacement-first-user",
            EventPayload::UserMessage {
                text: "ORIGINAL FIRST TURN".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &first_run,
            "summary-replacement-first-user-node",
            None,
            NodeKind::UserTurn {
                text: "ORIGINAL FIRST TURN".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &first_run,
            "summary-replacement-first-answer",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("summary-replacement-first-answer-item"),
                item: TurnItem::AgentMessage {
                    text: "ORIGINAL FIRST ANSWER".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &first_run,
            "summary-replacement-first-answer-node",
            Some("summary-replacement-first-user-node"),
            NodeKind::AssistantCommit {
                text: "ORIGINAL FIRST ANSWER".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &first_run,
            "summary-replacement-first-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        node(
            &session_id,
            &first_run,
            "summary-replacement-old-compaction",
            Some("summary-replacement-first-answer-node"),
            NodeKind::Compaction {
                covers_from: NodeId::new("summary-replacement-first-user-node"),
                covers_to: NodeId::new("summary-replacement-first-answer-node"),
                summary_artifact: ArtifactRef::new("OLD-SUMMARY-MUST-NOT-BE-READ"),
                tokens_before: 20_000,
                tokens_after: 1_000,
                resume_cause: CompactionResume::ManualIdle,
            },
        ),
        envelope(
            &session_id,
            &second_run,
            "summary-replacement-second-user",
            EventPayload::UserMessage {
                text: "ORIGINAL SECOND TURN".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &second_run,
            "summary-replacement-second-user-node",
            Some("summary-replacement-old-compaction"),
            NodeKind::UserTurn {
                text: "ORIGINAL SECOND TURN".into(),
                attachments: Vec::new(),
            },
        ),
        envelope(
            &session_id,
            &second_run,
            "summary-replacement-second-answer",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("summary-replacement-second-answer-item"),
                item: TurnItem::AgentMessage {
                    text: "ORIGINAL SECOND ANSWER".into(),
                },
            }),
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &second_run,
            "summary-replacement-second-answer-node",
            Some("summary-replacement-second-user-node"),
            NodeKind::AssistantCommit {
                text: "ORIGINAL SECOND ANSWER".into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        ),
        envelope(
            &session_id,
            &second_run,
            "summary-replacement-second-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current_run,
            "summary-replacement-current-user",
            EventPayload::UserMessage {
                text: "CURRENT TURN".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        node(
            &session_id,
            &current_run,
            "summary-replacement-current-node",
            Some("summary-replacement-second-answer-node"),
            NodeKind::UserTurn {
                text: "CURRENT TURN".into(),
                attachments: Vec::new(),
            },
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append replacement summary fixture");
    let source = PromptHistoryCompiler::compile_compaction_source(
        &store,
        &session_id,
        None,
        None,
        &current_run,
        &CompactionIntent {
            operation_id: "replacement".into(),
            covers_from: NodeId::new("summary-replacement-first-user-node"),
            covers_to: NodeId::new("summary-replacement-second-answer-node"),
            resume_cause: CompactionResume::AutoMidTurn,
        },
    )
    .await
    .expect("reconstruct original replacement-summary source");
    let text = source
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        text,
        [
            "ORIGINAL FIRST TURN",
            "ORIGINAL FIRST ANSWER",
            "ORIGINAL SECOND TURN",
            "ORIGINAL SECOND ANSWER"
        ]
    );
    assert!(!text.iter().any(|value| value.contains("OLD-SUMMARY")));
}
