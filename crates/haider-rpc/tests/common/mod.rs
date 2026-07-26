#![allow(dead_code)]

use std::collections::BTreeSet;

use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, MenuId, SessionId};
use haider_rpc::{
    AttachMode, AttachState, AttachmentId, Capability, ClientKind, CommandId, Hello,
    LifecyclePhase, ProtocolError, RequestBody, RequestId, ResponseBody, SeqRange,
    SessionReadResult, SessionSummary, Welcome, WireFrame,
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
            client_kind: ClientKind::Gui,
            capabilities_requested: capabilities([Capability::View, Capability::Control]),
        }),
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-1".into(),
            daemon_generation: 4,
            frame_limit: TEST_FRAME_LIMIT as u32,
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View]),
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-list"),
            body: RequestBody::SessionList {
                page: 2,
                page_size: 50,
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
        WireFrame::Request {
            request_id: RequestId::new("request-ping"),
            body: RequestBody::Ping { nonce: 99 },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-list"),
            body: ResponseBody::SessionList {
                page: 2,
                page_size: 50,
                sessions: vec![SessionSummary {
                    session_id: session_id.clone(),
                    head_seq: 9,
                    worker_generation: 7,
                }],
                next_page: Some(3),
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
            request_id: RequestId::new("request-ping"),
            body: ResponseBody::Pong { nonce: 99 },
        },
        WireFrame::Event {
            session_id: session_id.clone(),
            envelope: raw_envelope(10),
        },
        WireFrame::AttachCaughtUp {
            attachment_id: attachment_id.clone(),
            high_water_seq: 10,
        },
        WireFrame::MenuAnswer {
            command_id: CommandId::new("command-1"),
            session_id,
            menu_id: MenuId::new("menu-1"),
            request_seq: 8,
            worker_generation: 7,
            option: "allow".into(),
        },
        WireFrame::Lagged {
            attachment_id,
            resume_after_seq: 10,
        },
        WireFrame::ServerDraining {
            deadline_ms: 1_753_500_030_000,
        },
        WireFrame::ProtocolError(ProtocolError {
            code: "capability_denied".into(),
            message: "control capability required".into(),
            fatal: false,
        }),
        WireFrame::Unknown,
    ]
}
