#![allow(clippy::expect_used)]
//! CU-1 daemon round trip: tool -> bounded CAS ref -> durable result ->
//! provider-neutral request with ephemeral resolved bytes. No network.

use crate::session_hub::{HubStoreHandle, SessionHub, SessionHubConfig};
use crate::worker::{
    ProviderFactory, ResolvedTurnProvider, TurnToolFactory, WorkerDependencies, WorkerManager,
    WorkerToolContext,
};
use async_trait::async_trait;
use base64::Engine as _;
use haider_core::{
    ArtifactReader, CancelToken, ContextCompactor, EventIdGenerator, SessionCreateCommand,
    SqliteStoreHandle, StoreHandle, ToolDispatchResult, ToolDispatcher, TurnAcceptCommand,
    TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CompactionIntent, CompactionResume,
};
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, ItemId, NodeId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{Block, CapabilityDoc, FeatureResolve, FinishReason, UsageScope};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::RunState;
use haider_protocol::tool::{AttachmentBlock, BoundedResult, ImageBlockRef, ToolResultStatus};
use haider_provider::{
    FakeProvider, FakeStep, Message, MessageRole, Provider, ProviderError, ProviderErrorKind,
    ProviderStream, ToolDefinition, TurnRequest,
};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, timeout};

fn png_fixture() -> Vec<u8> {
    let pixels = image::RgbaImage::from_pixel(1, 1, image::Rgba([17, 42, 91, 255]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(pixels)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("encode fixture PNG");
    encoded.into_inner()
}

async fn seed_compaction_run(
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    intent: &CompactionIntent,
    label: &str,
) {
    let item_id = ItemId::new(format!("{label}-intent-item"));
    let item = TurnItem::Extension {
        kind: COMPACTION_INTENT_EXTENSION_KIND.into(),
        data: serde_json::to_value(intent).expect("serialize compaction intent"),
    };
    let mut envelopes = vec![
        super::supervisor_envelope(
            lease,
            device_id,
            None,
            Some(run_id.clone()),
            EventId::new(format!("{label}-intent-start")),
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: item.clone(),
            }),
        )
        .expect("build compaction intent start"),
        super::supervisor_envelope(
            lease,
            device_id,
            None,
            Some(run_id.clone()),
            EventId::new(format!("{label}-intent-complete")),
            EventPayload::Item(ItemEvent::Completed { item_id, item }),
        )
        .expect("build compaction intent completion"),
        super::supervisor_envelope(
            lease,
            device_id,
            None,
            Some(run_id.clone()),
            EventId::new(format!("{label}-compacting")),
            EventPayload::RunState(RunState::Compacting),
        )
        .expect("build compacting state"),
    ];
    StoreHandle::append(lease, &mut envelopes)
        .await
        .expect("seed durable compaction run");
}

struct FixtureArtifact {
    artifact: ArtifactRef,
    bytes: Vec<u8>,
}

#[async_trait]
impl ArtifactReader for FixtureArtifact {
    async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        if artifact == &self.artifact {
            Ok(self.bytes.clone())
        } else {
            Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "fixture artifact is missing",
                false,
            ))
        }
    }
}

struct FixedProviderFactory {
    provider: Arc<FakeProvider>,
}

#[derive(Debug)]
struct RejectFirstReplayProvider {
    calls: AtomicUsize,
    requests: Mutex<Vec<TurnRequest>>,
    fallback: FakeProvider,
}

impl RejectFirstReplayProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            fallback: FakeProvider::new(vec![
                FakeStep::EmitText {
                    text: "summary".into(),
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ]),
        }
    }

    fn requests(&self) -> Vec<TurnRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Provider for RejectFirstReplayProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.fallback.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "fixture rejects multimodal replay",
            ));
        }
        self.fallback.stream_turn(request).await
    }
}

#[async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider) as Arc<dyn Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

struct ImageFixtureToolFactory;

#[async_trait]
impl TurnToolFactory for ImageFixtureToolFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "fixture_image".into(),
            description: "Return a deterministic bounded image".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }]
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        Ok(Some(Arc::new(ImageFixtureDispatcher {
            store: context.store,
        })))
    }
}

struct ImageFixtureDispatcher {
    store: HubStoreHandle,
}

#[async_trait]
impl ToolDispatcher for ImageFixtureDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        if name != "fixture_image" {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("unexpected fixture tool `{name}`"),
                false,
            ));
        }
        let bytes = png_fixture();
        let image = self
            .store
            .put_image_artifact(bytes, "image/png".into())
            .await?;
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "captured fixture image".into(),
            truncated: false,
            data: None,
            artifact: None,
            images: vec![image],
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

#[tokio::test]
async fn replay_rejects_conflicting_metadata_and_unsupported_vision_gets_a_placeholder() {
    let bytes = png_fixture();
    let artifact = ArtifactRef::new(format!("blake3:{}", blake3::hash(&bytes).to_hex()));
    let image = ImageBlockRef {
        artifact: artifact.clone(),
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        byte_len: bytes.len() as u64,
    };
    let store = FixtureArtifact {
        artifact: artifact.clone(),
        bytes,
    };
    let mut conflicting = image.clone();
    conflicting.width = 2;
    let mut messages = vec![
        Message::tool_result_with_images("call-1", "one", false, vec![image.clone()]),
        Message::tool_result_with_images("call-2", "two", false, vec![conflicting]),
    ];
    for index in 0..5 {
        messages.push(Message::tool_result_with_images(
            format!("call-new-{index}"),
            "newer",
            false,
            vec![image.clone()],
        ));
    }
    let error = super::resolve_prompt_attachments(
        &store,
        &mut messages,
        FeatureResolve::Native,
        FeatureResolve::Unsupported,
    )
    .await
    .expect_err("conflicting replay metadata must fail closed");
    assert_eq!(error.code, ErrorCode::StoreCorrupt);

    let mut unsupported = vec![Message::tool_result_with_images(
        "call-3",
        "captured",
        false,
        vec![image.clone()],
    )];
    let resolved = super::resolve_prompt_attachments(
        &store,
        &mut unsupported,
        FeatureResolve::Unsupported,
        FeatureResolve::Unsupported,
    )
    .await
    .expect("unsupported vision degrades after validating CAS truth");
    assert!(resolved.is_empty());
    let Block::ToolResult {
        preview, images, ..
    } = &unsupported[0].blocks[0]
    else {
        panic!("tool result remains paired");
    };
    assert!(images.is_empty());
    assert!(preview.contains(artifact.as_str()));
    assert!(preview.contains("unavailable"));

    let mut repeated = vec![
        Message::tool_result_with_images("call-repeat-1", "one", false, vec![image.clone()]),
        Message::tool_result_with_images("call-repeat-2", "two", false, vec![image]),
    ];
    let resolved = super::resolve_prompt_attachments(
        &store,
        &mut repeated,
        FeatureResolve::Native,
        FeatureResolve::Unsupported,
    )
    .await
    .expect("the same honest artifact can appear in multiple results");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].artifact, artifact);
}

#[tokio::test]
async fn text_only_compaction_validates_before_budget_then_degrades_the_retained_suffix() {
    let bytes = png_fixture();
    let artifact = ArtifactRef::new(format!("blake3:{}", blake3::hash(&bytes).to_hex()));
    let image = ImageBlockRef {
        artifact: artifact.clone(),
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        byte_len: bytes.len() as u64,
    };
    let store = FixtureArtifact {
        artifact: artifact.clone(),
        bytes,
    };
    let mut messages = (0..6)
        .map(|index| {
            Message::tool_result_with_images(
                format!("call-{index}"),
                "captured",
                false,
                vec![image.clone()],
            )
        })
        .collect::<Vec<_>>();

    super::prepare_tool_images_for_text_only_request(&store, &mut messages)
        .await
        .expect("compaction projection");

    let previews = messages
        .iter()
        .map(|message| match &message.blocks[0] {
            Block::ToolResult {
                preview, images, ..
            } => {
                assert!(images.is_empty());
                preview.as_str()
            }
            block => panic!("expected tool result, got {block:?}"),
        })
        .collect::<Vec<_>>();
    assert!(previews[0].contains("oldest first"));
    assert_eq!(
        previews
            .iter()
            .filter(|preview| preview.contains("unavailable to this provider"))
            .count(),
        5
    );

    let missing = ImageBlockRef {
        artifact: ArtifactRef::new(format!("blake3:{}", "0".repeat(64))),
        ..image
    };
    let mut corrupt = vec![Message::tool_result_with_images(
        "call-missing",
        "missing",
        false,
        vec![missing],
    )];
    corrupt.extend((0..5).map(|index| {
        Message::tool_result_with_images(
            format!("call-valid-{index}"),
            "valid",
            false,
            vec![ImageBlockRef {
                artifact: artifact.clone(),
                media_type: "image/png".into(),
                width: 1,
                height: 1,
                byte_len: store.bytes.len() as u64,
            }],
        )
    }));
    let error = super::prepare_tool_images_for_text_only_request(&store, &mut corrupt)
        .await
        .expect_err("an oldest budget-dropped missing ref must still fail closed");
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
}

#[tokio::test]
async fn daemon_compactor_replays_exact_lane_prefix_with_cache_boundary() {
    let root = tempfile::tempdir().expect("profile");
    let sqlite = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(sqlite.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new("cu1-compactor-session");
    let device_id = DeviceId::new("cu1-compactor-device");
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-cu1-compactor".into(),
        request_digest: "create-cu1-compactor-digest".into(),
        request_json: r#"{"session":"cu1-compactor"}"#.into(),
        session_id: session_id.clone(),
        cwd,
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("cu1-compactor-created"),
        device_id: device_id.clone(),
    })
    .await
    .expect("create compactor session");
    let lease = hub
        .acquire_worker_lease(session_id)
        .await
        .expect("compactor lease");
    let image = lease
        .put_image_artifact(png_fixture(), "image/png".into())
        .await
        .expect("store compactor image");
    let mut covered_messages = (0..6)
        .map(|index| {
            Message::tool_result_with_images(
                format!("call-{index}"),
                "captured",
                false,
                vec![image.clone()],
            )
        })
        .collect::<Vec<_>>();
    covered_messages.insert(
        0,
        Message {
            role: MessageRole::User,
            blocks: vec![
                Block::Text {
                    text: "inspect this image".into(),
                },
                Block::Attachment(AttachmentBlock::Image {
                    artifact: image.artifact.clone(),
                    mime: image.media_type.clone(),
                    width: Some(image.width),
                    height: Some(image.height),
                }),
            ],
        },
    );
    let lane_system_prompt = Some("byte-identical lane system prompt".to_owned());
    let lane_tools = vec![ToolDefinition {
        name: "lane_tool".into(),
        description: "byte-identical lane tool".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "summary".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let compactor = super::DaemonContextCompactor {
        store: lease,
        provider: Arc::clone(&provider) as Arc<dyn Provider>,
        model: "fake-model".into(),
        max_tokens: 4096,
        context_window: None,
        reserved_output_tokens: 4096,
        post_compaction_system_prompt: lane_system_prompt.clone(),
        post_compaction_volatile_tail: None,
        post_compaction_grant_scope: "cu1-image-grant".into(),
        post_compaction_tools: lane_tools.clone().into(),
        post_compaction_tool_digest: super::canonical_tool_definitions_digest(&lane_tools),
        reasoning_settings: "lane-reasoning".into(),
        cache_expected_later_reads: 2,
        cache_reuse_gap_ms: Some(17),
        cache_cohort: None,
        device_id,
        event_ids: Arc::new(EventIdGenerator::new("cu1-compactor-event")),
        agent_id: None,
        branch_id: None,
        usage_scope: UsageScope {
            provider: "fake".into(),
            auth_scope: "fixture-auth".into(),
            ..UsageScope::default()
        },
        usage_account: None,
    };
    let intent = CompactionIntent {
        operation_id: "cu1-compaction".into(),
        covers_from: NodeId::new("cu1-from"),
        covers_to: NodeId::new("cu1-to"),
        resume_cause: CompactionResume::ManualIdle,
    };
    let run_id = RunId::new("cu1-compactor-run");
    seed_compaction_run(
        &compactor.store,
        &compactor.device_id,
        &run_id,
        &intent,
        "cu1-compactor",
    )
    .await;

    let _result = compactor
        .compact(&run_id, &intent, covered_messages.clone(), Vec::new(), None)
        .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let mut expected_messages = covered_messages.clone();
    expected_messages.push(Message::user_text(super::COMPACTION_SUMMARY_INSTRUCTION));
    // Mutation pin: eager image rewriting, dropping the direct attachment,
    // or changing the single appended instruction breaks exact replay.
    assert_eq!(request.messages, expected_messages);
    assert_eq!(request.system_prompt, lane_system_prompt);
    assert_eq!(request.tools, lane_tools);
    let metadata = request
        .cache_metadata
        .as_ref()
        .expect("cache-riding replay metadata");
    // Mutation pin: the appended instruction is volatile; only the exact
    // covered conversation belongs behind the stable cache breakpoint.
    assert_eq!(metadata.stable_history_end, covered_messages.len());
    assert_eq!(metadata.current_user_start, covered_messages.len());
}

#[tokio::test]
async fn daemon_compactor_falls_back_once_to_text_only_after_replay_rejection() {
    let root = tempfile::tempdir().expect("profile");
    let sqlite = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(sqlite, SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new("cu1-compactor-fallback-session");
    let device_id = DeviceId::new("cu1-compactor-fallback-device");
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-cu1-compactor-fallback".into(),
        request_digest: "create-cu1-compactor-fallback-digest".into(),
        request_json: r#"{"session":"cu1-compactor-fallback"}"#.into(),
        session_id: session_id.clone(),
        cwd,
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("cu1-compactor-fallback-created"),
        device_id: device_id.clone(),
    })
    .await
    .expect("create fallback compactor session");
    let lease = hub
        .acquire_worker_lease(session_id)
        .await
        .expect("fallback compactor lease");
    let image = lease
        .put_image_artifact(png_fixture(), "image/png".into())
        .await
        .expect("store fallback image");
    let covered_messages = vec![
        Message {
            role: MessageRole::User,
            blocks: vec![
                Block::Text {
                    text: "inspect this image".into(),
                },
                Block::Attachment(AttachmentBlock::Image {
                    artifact: image.artifact.clone(),
                    mime: image.media_type.clone(),
                    width: Some(image.width),
                    height: Some(image.height),
                }),
            ],
        },
        Message::tool_result_with_images("call-fallback", "captured", false, vec![image]),
    ];
    let lane_system_prompt = Some("fallback lane system".to_owned());
    let lane_tools = vec![ToolDefinition {
        name: "fallback_lane_tool".into(),
        description: "fallback lane tool".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    let provider = Arc::new(RejectFirstReplayProvider::new());
    let compactor = super::DaemonContextCompactor {
        store: lease,
        provider: Arc::clone(&provider) as Arc<dyn Provider>,
        model: "fake-model".into(),
        max_tokens: 4096,
        context_window: None,
        reserved_output_tokens: 4096,
        post_compaction_system_prompt: lane_system_prompt.clone(),
        post_compaction_volatile_tail: None,
        post_compaction_grant_scope: "cu1-fallback-grant".into(),
        post_compaction_tools: lane_tools.clone().into(),
        post_compaction_tool_digest: super::canonical_tool_definitions_digest(&lane_tools),
        reasoning_settings: "lane-reasoning".into(),
        cache_expected_later_reads: 2,
        cache_reuse_gap_ms: None,
        cache_cohort: None,
        device_id,
        event_ids: Arc::new(EventIdGenerator::new("cu1-compactor-fallback-event")),
        agent_id: None,
        branch_id: None,
        usage_scope: UsageScope {
            provider: "fake".into(),
            auth_scope: "fixture-auth".into(),
            ..UsageScope::default()
        },
        usage_account: None,
    };
    let intent = CompactionIntent {
        operation_id: "cu1-compaction-fallback".into(),
        covers_from: NodeId::new("cu1-fallback-from"),
        covers_to: NodeId::new("cu1-fallback-to"),
        resume_cause: CompactionResume::ManualIdle,
    };
    let run_id = RunId::new("cu1-compactor-fallback-run");
    seed_compaction_run(
        &compactor.store,
        &compactor.device_id,
        &run_id,
        &intent,
        "cu1-compactor-fallback",
    )
    .await;

    let post_summary_result = compactor
        .compact(&run_id, &intent, covered_messages.clone(), Vec::new(), None)
        .await;
    assert!(
        post_summary_result.is_ok(),
        "durably accepted compaction should commit: {post_summary_result:?}"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let mut exact_replay = covered_messages.clone();
    exact_replay.push(Message::user_text(super::COMPACTION_SUMMARY_INSTRUCTION));
    // Mutation pin: the cheap first attempt must remain cache-compatible;
    // degrading it eagerly would hide the defect this fallback repairs.
    assert_eq!(requests[0].messages, exact_replay);
    assert_eq!(requests[0].system_prompt, lane_system_prompt);
    assert_eq!(requests[0].tools, lane_tools);
    assert!(requests[0].cache_metadata.is_some());

    // Mutation pin: deleting this one-shot fallback turns a provider's
    // multimodal replay rejection into a failed compaction.
    assert_eq!(
        requests[1].system_prompt,
        Some(super::SystemPromptBuilder::shared_immutable_base(
            &[],
            "cu1-fallback-grant"
        ))
    );
    assert!(requests[1].tools.is_empty());
    assert!(requests[1].cache_metadata.is_none());
    assert_eq!(requests[1].messages.len(), covered_messages.len() + 1);
    assert!(
        requests[1].messages[0]
            .blocks
            .iter()
            .all(|block| !matches!(block, Block::Attachment(AttachmentBlock::Image { .. })))
    );
    assert!(requests[1].messages[1].blocks.iter().any(|block| {
        matches!(block, Block::ToolResult { preview, images, .. }
            if images.is_empty() && preview.contains("unavailable to this provider"))
    }));
    assert_eq!(requests[1].messages.last(), exact_replay.last());
}

#[tokio::test]
async fn image_tool_ref_reaches_provider_request_without_journal_bytes() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "call-fixture-image".into(),
                name: "fixture_image".into(),
                args: serde_json::json!({}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "call-fixture-image".into(),
            },
            FakeStep::EmitText {
                text: "image received".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory {
                provider: Arc::clone(&provider),
            }),
            tool_factory: Arc::new(ImageFixtureToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install manager");
    let session_id = SessionId::new("cu1-image-session");
    let device_id = DeviceId::new("cu1-image-device");
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-cu1-image".into(),
        request_digest: "create-cu1-image-digest".into(),
        request_json: r#"{"session":"cu1-image"}"#.into(),
        session_id: session_id.clone(),
        cwd,
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("cu1-image-created"),
        device_id: device_id.clone(),
    })
    .await
    .expect("create session");
    let run_id = RunId::new("cu1-image-run");
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "submit-cu1-image".into(),
            request_digest: "submit-cu1-image-digest".into(),
            request_json: r#"{"turn":"cu1-image"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "capture the fixture".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
            queued_event_id: EventId::new("cu1-image-queued"),
            user_event_id: EventId::new("cu1-image-user"),
            active_event_id: EventId::new("cu1-image-active"),
            device_id,
        })
        .await
        .expect("accept turn");
    assert_eq!(accepted.disposition, TurnAdmissionDisposition::Started);
    manager
        .handle()
        .submit(accepted)
        .await
        .expect("submit turn");

    timeout(Duration::from_secs(10), async {
        loop {
            let events = store.read(&session_id, 0, 2048).await.expect("journal");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(&run_id)
                    && serde_json::from_value::<EventPayload>(event.payload.clone())
                        .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Done))
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("run reaches Done");

    let events = store.read(&session_id, 0, 2048).await.expect("journal");
    let (journal_json, image) = events
        .iter()
        .find_map(|event| {
            let payload = serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?;
            match payload {
                EventPayload::ToolResult { result, .. } if !result.images.is_empty() => {
                    Some((event.payload.to_string(), result.images[0].clone()))
                }
                _ => None,
            }
        })
        .expect("durable image tool result");
    assert!(!journal_json.contains("data_base64"));
    let cas_bytes = store.get(&image.artifact).await.expect("image CAS bytes");
    assert!(!journal_json.contains(&base64::engine::general_purpose::STANDARD.encode(&cas_bytes)));
    assert_eq!(cas_bytes.len() as u64, image.byte_len);

    let requests: Vec<TurnRequest> = provider.requests();
    assert_eq!(requests.len(), 2);
    let continuation = &requests[1];
    let result_position = continuation
        .messages
        .iter()
        .position(|message| {
            matches!(
                message.blocks.as_slice(),
                [Block::ToolResult { call_id, images, .. }]
                    if call_id == "call-fixture-image" && images == std::slice::from_ref(&image)
            )
        })
        .expect("image-bearing provider tool result");
    assert!(result_position > 0);
    assert!(matches!(
        continuation.messages[result_position - 1].blocks.as_slice(),
        [Block::ToolCall { call_id, .. }] if call_id == "call-fixture-image"
    ));
    let resolved = continuation
        .attachments
        .iter()
        .find(|attachment| attachment.artifact == image.artifact)
        .expect("ephemeral resolved image bytes");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&resolved.data_base64)
            .expect("resolved base64"),
        cas_bytes
    );
}
