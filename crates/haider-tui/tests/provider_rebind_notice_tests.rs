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
        selection_epoch: Some(1),
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

#[test]
fn rebound_before_model_metadata_retains_budget_epoch_and_provider() {
    use haider_tui::context_meter::MeterEpoch;
    for user in [false, true] {
        let mut meter = MeterEpoch::default();
        assert!(meter.admit_rebound("haider-code", Some(2), Some(8192), Some(user)));
        assert_eq!(meter.pair, None, "a rebound does not invent a model");
        assert_eq!(meter.provider(), Some("haider-code"));
        assert_eq!(meter.selection_epoch, Some(2));
        assert_eq!(meter.output_budget, Some(8192));
        assert_eq!(meter.user_budget, user);
        let held = meter.clone();
        for (provider, epoch, budget) in [
            ("haider-code", Some(1), 20_000),
            ("haider-code", None, 20_000),
            ("other-provider", Some(2), 8192),
            ("haider-code", Some(2), 20_000),
        ] {
            assert!(!meter.admit(
                (provider.into(), "small".into()),
                epoch,
                Some(budget),
                Some(user)
            ));
            assert_eq!(meter, held, "delayed metadata cannot undo the rebound");
        }
        assert!(!meter.admit_rebound("other-provider", Some(2), Some(8192), Some(user)));
        assert!(!meter.admit_rebound("haider-code", Some(1), Some(20_000), Some(user)));
        assert_eq!(meter, held);
        assert!(meter.admit(
            ("haider-code".into(), "small".into()),
            Some(2),
            Some(8192),
            Some(user)
        ));
        assert_eq!(meter.pair, Some(("haider-code".into(), "small".into())));
        assert!(meter.admit_rebound("haider-code", Some(3), Some(20_000), Some(user)));
        assert_eq!(
            meter.output_budget,
            Some(20_000),
            "a later restore is admitted"
        );
    }
}

#[test]
fn unknown_model_rebound_is_ordered_for_active_and_parked_sessions() {
    for parked in [false, true] {
        for explicit_epoch in [false, true] {
            for source in [
                SessionOutputBudgetSourceV1::Derived,
                SessionOutputBudgetSourceV1::UserSet { requested: 20_000 },
            ] {
                let session = SessionId::new("unknown-rebound");
                let other = SessionId::new("unrelated-session");
                let mut model = launcher_model();
                model.mode = RuntimeMode::Live;
                model.sessions.clear();
                model.upsert_live_session(&session);
                model.open_session(&session);
                let mut driver = LiveDriver::new("unknown-rebound-test");
                let attachment = AttachmentId::new("unknown-rebound-attachment");
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
                if parked {
                    model.upsert_live_session(&other);
                    model.open_session(&other);
                    model.note_session_metadata_at(
                        &other,
                        "other-provider",
                        "other-model",
                        6000,
                        Some(4),
                        Some(SessionOutputBudgetSourceV1::Derived),
                    );
                }
                let other_meter = model.meter_epoch.clone();
                for (seq, epoch, cap) in [(1, 1, 8192), (2, 0, 1000), (3, 3, 20_000)] {
                    let fact = SessionProviderRebound {
                        selection_epoch: if explicit_epoch || epoch == 0 {
                            Some(epoch)
                        } else {
                            None
                        },
                        rebind_id: format!("unknown-rebind-{seq}"),
                        provider: "haider-code".into(),
                        base_url: None,
                        account: None,
                        output_budget: Some(SessionOutputBudgetV1 {
                            max_tokens: cap,
                            source,
                            clamped: (cap < 20_000
                                && matches!(source, SessionOutputBudgetSourceV1::UserSet { .. }))
                            .then_some(OutputBudgetClampV1 {
                                requested: 20_000,
                                max_output_tokens: cap,
                            }),
                        }),
                        active_output_budget: None,
                    };
                    let envelope: RawEnvelope = EventEnvelope {
                        schema_version: 1,
                        event_id: EventId::new(format!("unknown-fact-{seq}")),
                        seq,
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
                            attachment: attachment.clone(),
                            session: session.clone(),
                            envelope: Box::new(envelope.clone()),
                        },
                    );
                    let flash = model.flash.clone();
                    driver.apply(
                        &mut model,
                        LiveReply::Event {
                            attachment: attachment.clone(),
                            session: session.clone(),
                            envelope: Box::new(envelope),
                        },
                    );
                    assert_eq!(
                        model.flash, flash,
                        "duplicate delivery cannot duplicate the notice"
                    );
                    if parked {
                        assert_eq!(
                            model.meter_epoch, other_meter,
                            "parked rebound leaves the viewed session alone"
                        );
                        assert_eq!(
                            model.flash, None,
                            "parked rebound has no active-session notice"
                        );
                    } else if seq == 1
                        && matches!(source, SessionOutputBudgetSourceV1::UserSet { .. })
                    {
                        assert!(
                            model
                                .flash
                                .as_deref()
                                .expect("early clamp notice")
                                .contains("using 8192 per response")
                        );
                    }
                    let meter = if parked {
                        &model
                            .sessions
                            .iter()
                            .find(|row| row.id == session)
                            .expect("session")
                            .meter_epoch
                    } else {
                        &model.meter_epoch
                    };
                    assert_eq!(meter.pair, None);
                    assert_eq!(meter.selection_epoch, Some(if seq == 2 { 1 } else { seq }));
                    assert_eq!(meter.output_budget, Some(if seq == 2 { 8192 } else { cap }));
                    assert_eq!(
                        meter.user_budget,
                        matches!(source, SessionOutputBudgetSourceV1::UserSet { .. })
                    );
                    assert!(
                        !model
                            .flash
                            .as_deref()
                            .unwrap_or("")
                            .contains("using 1000 per response")
                    );
                }
                model.note_session_metadata_at(
                    &session,
                    "haider-code",
                    "late-model",
                    20_000,
                    Some(3),
                    Some(source),
                );
                if parked {
                    model.open_session(&session);
                }
                assert_eq!(
                    model.meter_epoch.pair,
                    Some(("haider-code".into(), "late-model".into()))
                );
                assert_eq!(model.meter_epoch.output_budget, Some(20_000));
                assert_eq!(model.meter_epoch.selection_epoch, Some(3));
            }
        }
    }
}
