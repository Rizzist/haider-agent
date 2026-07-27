#![allow(dead_code)]

use std::collections::BTreeSet;

use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, MenuId, SessionId};
use haider_rpc::{
    AttachMode, AttachState, AttachmentId, Capability, ClientKind, CommandId,
    ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CURSOR_AHEAD, ErrorData,
    Hello, LifecyclePhase, MenuInput, ProtocolError, RequestBody, RequestId, ResponseBody,
    SeqRange, SessionReadResult, SessionSummary, Welcome, WireFrame,
};

pub const TEST_FRAME_LIMIT: usize = 1024 * 1024;

pub fn capabilities(values: impl IntoIterator<Item = Capability>) -> BTreeSet<Capability> {
    values.into_iter().collect()
}

pub fn raw_envelope(seq: u64) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("ev-{seq}")),
        seq,
        session_id: SessionId::new("session-1"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("device-1"),
        authority_epoch: 3,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1_753_500_000_000 + seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::json!({
            "type": "future_event",
            "detail": "kept raw"
        }),
    }
}

pub fn transcript() -> Vec<WireFrame> {
    let session_id = SessionId::new("session-1");
    let attachment_id = AttachmentId::new("attachment-1");
    let range = SeqRange {
        start_seq: 5,
        end_seq: 9,
    };

    vec![
        WireFrame::Hello(Hello {
            protocol_min: 1,
            protocol_max: 2,
            client_name: "haider-gui".into(),
            client_version: "0.0.8".into(),
            client_instance_id: "client-instance-1".into(),
            client_kind: ClientKind::Gui,
            capabilities_requested: capabilities([Capability::View, Capability::Control]),
            max_receive_frame: TEST_FRAME_LIMIT as u32,
        }),
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-1".into(),
            daemon_generation: 4,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.8".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View]),
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-list"),
            body: RequestBody::SessionList {
                cursor: Some("cursor-after-session-0".into()),
                limit: 50,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-read"),
            body: RequestBody::SessionRead {
                session_id: session_id.clone(),
                range,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-attach"),
            body: RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 4,
                mode: AttachMode::View,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-detach"),
            body: RequestBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-list"),
            body: ResponseBody::SessionList {
                sessions: vec![SessionSummary {
                    session_id: session_id.clone(),
                    head_seq: 9,
                    worker_generation: 7,
                }],
                next_cursor: Some("cursor-after-session-1".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-read"),
            body: ResponseBody::SessionRead {
                result: SessionReadResult {
                    session_id: session_id.clone(),
                    range,
                    head_seq: 9,
                    envelopes: vec![raw_envelope(9)],
                },
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-attach"),
            body: ResponseBody::SessionAttach {
                attachment_id: attachment_id.clone(),
                attach_state: AttachState {
                    session_id: session_id.clone(),
                    requested_after_seq: 4,
                    replay_through_seq: 9,
                    worker_generation: 7,
                    authority_epoch: 3,
                },
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-detach"),
            body: ResponseBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-control"),
            body: ResponseBody::Error {
                code: ERROR_CODE_CAPABILITY_DENIED.into(),
                message: "control capability required".into(),
                retryable: false,
                data: None,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-attach-ahead"),
            body: ResponseBody::Error {
                code: ERROR_CODE_CURSOR_AHEAD.into(),
                message: "requested cursor is beyond the committed head".into(),
                retryable: true,
                data: Some(ErrorData::CursorAhead {
                    requested: 40,
                    head: 10,
                }),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-menu-lost"),
            body: ResponseBody::Error {
                code: ERROR_CODE_ALREADY_RESOLVED.into(),
                message: "an earlier answer won".into(),
                retryable: false,
                data: Some(ErrorData::AlreadyResolved { resolution_seq: 9 }),
            },
        },
        WireFrame::Event {
            attachment_id: attachment_id.clone(),
            session_id: session_id.clone(),
            envelope: raw_envelope(10),
        },
        WireFrame::AttachCaughtUp {
            attachment_id: attachment_id.clone(),
            high_water_seq: 10,
        },
        // Correlated form: carries a request_id so the daemon can answer a CAS
        // loser with a Response { already_resolved }.
        WireFrame::MenuAnswer {
            request_id: Some(RequestId::new("request-menu-1")),
            command_id: CommandId::new("command-1"),
            session_id,
            menu_id: MenuId::new("menu-1"),
            request_seq: 8,
            worker_generation: 7,
            option_key: "other".into(),
            option_index: 2,
            input: Some(MenuInput::Text {
                text: "custom answer".into(),
            }),
        },
        // Uncorrelated form: request_id omitted entirely (older/simpler
        // clients), so the field must stay off the wire when absent.
        WireFrame::MenuAnswer {
            request_id: None,
            command_id: CommandId::new("command-2"),
            session_id: SessionId::new("session-1"),
            menu_id: MenuId::new("menu-2"),
            request_seq: 9,
            worker_generation: 7,
            option_key: "submit_secret".into(),
            option_index: 0,
            input: Some(MenuInput::SecretVaultReference {
                vault_reference: "vault-ref-1".into(),
            }),
        },
        WireFrame::Lagged {
            attachment_id: attachment_id.clone(),
            last_queued_seq: 10,
        },
        WireFrame::ServerDraining {
            reason: "upgrade".into(),
            instance_id: "instance-1".into(),
            daemon_generation: 4,
            deadline_unix_ms: 1_753_500_030_000,
        },
        WireFrame::Ping { nonce: 99 },
        WireFrame::Pong { nonce: 99 },
        WireFrame::ProtocolError(ProtocolError {
            code: "invalid_frame".into(),
            message: "connection framing failed".into(),
            fatal: true,
        }),
        WireFrame::Unknown,
    ]
}
