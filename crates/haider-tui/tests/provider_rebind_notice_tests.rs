#![allow(clippy::expect_used)]

use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::output_budget::{
    OutputBudgetClampV1, SessionOutputBudgetSourceV1, SessionOutputBudgetV1,
};
use haider_protocol::session::{ActiveRunOutputBudgetV1, SessionProviderRebound};
use haider_rpc::AttachmentId;
use haider_tui::app::RuntimeMode;
use haider_tui::live::{LiveDriver, LiveReply};

mod common;
use common::launcher_model;

#[test]
fn rebound_fact_shows_queued_and_active_clamp_to_an_observing_tui() {
    let session = SessionId::new("rebind-notice");
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&session);
    model.open_session(&session);
    let mut driver = LiveDriver::new("rebind-notice-test");
    let attachment = AttachmentId::new("rebind-notice-attachment");
    driver.apply(
        &mut model,
        LiveReply::Attached {
            launch_origin: None,
            session: session.clone(),
            attachment: attachment.clone(),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    let user = SessionOutputBudgetSourceV1::UserSet { requested: 20_000 };
    let fact = SessionProviderRebound {
        rebind_id: "rebind-command".into(),
        provider: "haider-code".into(),
        base_url: None,
        account: None,
        output_budget: Some(SessionOutputBudgetV1 {
            max_tokens: 12_000,
            source: user,
            clamped: Some(OutputBudgetClampV1 {
                requested: 20_000,
                max_output_tokens: 12_000,
            }),
        }),
        active_output_budget: Some(ActiveRunOutputBudgetV1 {
            run_id: RunId::new("active-run"),
            model: "small".into(),
            budget: SessionOutputBudgetV1 {
                max_tokens: 8192,
                source: user,
                clamped: Some(OutputBudgetClampV1 {
                    requested: 20_000,
                    max_output_tokens: 8192,
                }),
            },
        }),
    };
    let envelope: RawEnvelope = EventEnvelope {
        schema_version: 1,
        event_id: EventId::new("rebound-fact"),
        seq: 1,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("test-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: fact.to_payload_value().expect("fact").into(),
    };
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment,
            session,
            envelope: Box::new(envelope),
        },
    );
    let notice = model.flash.as_deref().expect("observer notice");
    assert!(notice.contains("using 12000 per response"), "{notice}");
    assert!(notice.contains("using 8192 per response"), "{notice}");
}
