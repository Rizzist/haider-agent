//! Prompt-fork client seam: feature absence, journal projection, and the
//! lossless draft the TUI hands straight back to the composer.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, DeviceId, EventId, SessionId};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::session_fork::{
    SessionForkDraft, SessionForkInvalidCutReason, SessionForkProvenance,
};
use haider_protocol::tool::{AttachmentBlock, PdfDeliveryMode};
use haider_rpc::{
    CapabilitySet, ErrorData, FEATURE_SESSION_FORK_V1, FEATURE_SESSION_PROMPT_FORK_V1,
    LifecyclePhase, ResponseBody, Welcome,
};

use super::session_fork::{
    ForkablePrompt, SessionForkClientError, forkable_prompts_in, prompt_fork_available,
    prompt_fork_response,
};

fn welcome() -> Welcome {
    Welcome {
        protocol: 1,
        instance_id: "instance".into(),
        daemon_generation: 1,
        frame_limit: 1_024,
        profile_id: "profile".into(),
        daemon_version: "test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::default(),
        features: Default::default(),
        user_command_withheld: false,
        encoding: None,
    }
}

fn metadata() -> SessionMetadataV1 {
    serde_json::from_value(serde_json::json!({
        "cwd": "/workspace",
        "provider": "fake",
        "model": "fake-model",
        "max_tokens": 1_024_u64,
        "created_at_ms": 7_u64,
    }))
    .expect("metadata fixture decodes")
}

fn envelope(seq: u64, agent: Option<&str>, payload: EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("event-{seq}")),
        seq,
        session_id: SessionId::new("source"),
        branch_id: None,
        run_id: None,
        agent_id: agent.map(AgentId::new),
        device_id: DeviceId::new("device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

fn user(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

/// The absence law: BOTH the method token and the additive prompt-selector
/// shape token are required. A daemon serving only `session_fork_v1` would
/// read the prompt request as a fork with no coordinates at all.
///
/// MUTATION CHECK: drop the `FEATURE_SESSION_PROMPT_FORK_V1` conjunct.
/// Expected failure: the prompt-only daemon claims availability.
#[test]
fn prompt_fork_needs_both_feature_tokens() {
    let mut welcome = welcome();
    assert!(!prompt_fork_available(&welcome));
    welcome.features.insert(FEATURE_SESSION_FORK_V1.into());
    assert!(
        !prompt_fork_available(&welcome),
        "the legacy exact-node fork alone cannot honor a prompt cut"
    );
    welcome
        .features
        .insert(FEATURE_SESSION_PROMPT_FORK_V1.into());
    assert!(prompt_fork_available(&welcome));
}

/// A fork cut names one of the SESSION's own prompts and carries the durable
/// sequence verbatim. Subagent-authored messages and non-prompt payloads are
/// not offered — the roster never invents a coordinate.
#[test]
fn journal_projection_reports_only_session_prompts_with_durable_sequences() {
    let envelopes = vec![
        envelope(1, None, user("oldest")),
        envelope(2, None, EventPayload::IdleDecayed),
        envelope(3, Some("agent-7"), user("subagent prompt")),
        envelope(4, None, user("newest")),
    ];
    assert_eq!(
        forkable_prompts_in(&envelopes),
        [
            ForkablePrompt {
                seq: 1,
                text: "oldest".to_owned(),
                branch_id: None,
            },
            ForkablePrompt {
                seq: 4,
                text: "newest".to_owned(),
                branch_id: None,
            },
        ]
    );
}

/// The draft crosses the seam byte-identical: image dimensions and the PDF
/// delivery mode survive, so a resubmitted fork draft reaches the provider
/// exactly as the source prompt did.
#[test]
fn prompt_fork_receipt_preserves_typed_attachment_blocks() {
    let attachments = vec![
        AttachmentBlock::Image {
            artifact: haider_protocol::ids::ArtifactRef::new("blake3:image"),
            mime: "image/png".to_owned(),
            width: Some(1_920),
            height: Some(1_080),
        },
        AttachmentBlock::Pdf {
            artifact: haider_protocol::ids::ArtifactRef::new("blake3:pdf"),
            name: "brief.pdf".to_owned(),
            pages: 12,
            delivery: PdfDeliveryMode::NativeDocument,
        },
    ];
    let fork = prompt_fork_response(ResponseBody::SessionFork {
        session_id: SessionId::new("child"),
        source_session_id: SessionId::new("source"),
        source_branch_id: None,
        fork_node_id: haider_protocol::ids::NodeId::new("node"),
        fork_seq: 3,
        created_seq: 1,
        worker_generation: 4,
        metadata: metadata(),
        forked_from: Some(SessionForkProvenance {
            session_id: SessionId::new("source"),
            seq: 4,
        }),
        draft: Some(SessionForkDraft {
            text: "editable second prompt".to_owned(),
            attachments: attachments.clone(),
        }),
    })
    .expect("prompt fork receipt");
    assert_eq!(fork.session_id, SessionId::new("child"));
    assert_eq!(fork.source_session_id, SessionId::new("source"));
    assert_eq!(fork.forked_from.seq, 4);
    assert_eq!(fork.draft.text, "editable second prompt");
    assert_eq!(fork.draft.attachments, attachments);
}

/// A `session.fork` answered WITHOUT provenance and a draft is the legacy
/// exact-node shape. It is an unexpected body, never a prompt fork silently
/// downgraded to an empty draft.
#[test]
fn legacy_fork_response_is_refused_rather_than_fabricated_into_a_draft() {
    let error = prompt_fork_response(ResponseBody::SessionFork {
        session_id: SessionId::new("child"),
        source_session_id: SessionId::new("source"),
        source_branch_id: None,
        fork_node_id: haider_protocol::ids::NodeId::new("node"),
        fork_seq: 3,
        created_seq: 1,
        worker_generation: 4,
        metadata: metadata(),
        forked_from: None,
        draft: None,
    })
    .expect_err("a legacy fork body cannot become a prompt fork");
    assert!(matches!(
        error,
        SessionForkClientError::UnexpectedResponse(_)
    ));
}

/// An unforkable cut keeps its typed coordinates so a caller can point at the
/// exact row it offered instead of flattening the refusal into a string.
#[test]
fn invalid_cut_refusal_stays_typed() {
    let error = prompt_fork_response(ResponseBody::Error {
        code: "invalid_argument".to_owned(),
        message: "not a user prompt".to_owned(),
        retryable: false,
        data: Some(ErrorData::SessionForkInvalidCut {
            session_id: SessionId::new("source"),
            seq: 9,
            reason: SessionForkInvalidCutReason::NotUserPrompt,
        }),
    })
    .expect_err("an invalid cut is a refusal");
    match error {
        SessionForkClientError::InvalidCut {
            session_id,
            seq,
            reason,
        } => {
            assert_eq!(session_id, SessionId::new("source"));
            assert_eq!(seq, 9);
            assert_eq!(reason, SessionForkInvalidCutReason::NotUserPrompt);
        }
        other => panic!("expected a typed invalid cut, got {other:?}"),
    }
}
