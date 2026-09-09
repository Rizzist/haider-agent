#![allow(clippy::expect_used)]

use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::SessionId;
use haider_rpc::{AttachmentId, SessionSummary};
use haider_tui::app::{AppModel, AppRequest, RuntimeMode};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use serde_json::{Value, json};

mod common;

fn sid() -> SessionId {
    SessionId::new("compat-session")
}
fn aid(n: u64) -> AttachmentId {
    AttachmentId::new(format!("attachment-{n}"))
}
fn summary(head_seq: u64) -> SessionSummary {
    SessionSummary {
        session_id: sid(),
        head_seq,
        worker_generation: 7,
        run_state: None,
        run_id: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        provider: None,
        workspace_cwd: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: None,
        agent_metrics: None,
        last_model: None,
        cache_lifetime_hit_basis_points: None,
        cache_reread_hit_basis_points: None,
        parent_session_id: None,
        kind: None,
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
        forked_from: None,
    }
}

fn raw(seq: u64, payload: Value, ui: bool) -> RawEnvelope {
    serde_json::from_value(json!({
        "schema_version":1,"event_id":format!("event-{seq}"),"seq":seq,
        "session_id":sid(),"device_id":"fixture","authority_epoch":1,
        "worker_generation":1,"committed_at_ms":seq,
        "render":{"ui":ui,"durable":true,"prompt":"omit"},"payload":payload,
    }))
    .expect("raw envelope")
}

fn ready() -> (AppModel, LiveDriver) {
    let mut model = common::launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    let mut driver = LiveDriver::new("compat");
    handshake(&mut model, &mut driver, env!("CARGO_PKG_VERSION"), 1);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(0)],
            next_cursor: None,
        },
    );
    model.open_session(&sid());
    driver.sync_selection(&model);
    attached(&mut model, &mut driver, 0);
    model.requests.clear();
    (model, driver)
}

fn handshake(model: &mut AppModel, driver: &mut LiveDriver, version: &str, protocol: u32) {
    driver.apply(
        model,
        LiveReply::Handshake {
            features: Default::default(),
            version: version.into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            protocol,
        },
    );
}

fn attached(model: &mut AppModel, driver: &mut LiveDriver, n: u64) {
    driver.apply(
        model,
        LiveReply::Attached {
            session: sid(),
            attachment: aid(n),
            worker_generation: 1,
            replay_through_seq: 0,
        },
    );
}

fn event(
    model: &mut AppModel,
    driver: &mut LiveDriver,
    attachment: u64,
    envelope: RawEnvelope,
) -> Vec<LiveCommand> {
    live_pass(
        driver,
        model,
        Some(LiveReply::Event {
            attachment: aid(attachment),
            session: sid(),
            envelope: Box::new(envelope),
        }),
        std::time::Instant::now(),
    )
    .commands
}

fn user(seq: u64) -> RawEnvelope {
    raw(
        seq,
        json!({"type":"user_message","text":format!("row {seq}"),"attachments":[]}),
        true,
    )
}

fn caught_up(
    model: &mut AppModel,
    driver: &mut LiveDriver,
    attachment: u64,
    high_water_seq: u64,
) -> Vec<LiveCommand> {
    driver.apply(
        model,
        LiveReply::CaughtUp {
            attachment: aid(attachment),
            high_water_seq,
        },
    )
}

fn assert_diagnostic_survives_resize(model: &AppModel, required: &[&str]) {
    let mut terminal = Terminal::new(TestBackend::new(120, 48)).expect("terminal");
    for width in [120, 80, 120] {
        terminal.backend_mut().resize(width, 48);
        terminal.resize(Rect::new(0, 0, width, 48)).expect("resize");
        terminal
            .draw(|frame| {
                haider_tui::render::render(model, frame);
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..48)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .take_while(|row| row.starts_with('▏'))
            .collect();
        assert!(rows.len() > 1, "expected wrapped card at {width}: {rows:?}");
        let card = rows
            .iter()
            .map(|row| row.trim_start_matches('▏').trim())
            .collect::<Vec<_>>()
            .join(" ");
        let card = card.split_whitespace().collect::<Vec<_>>().join(" ");
        for field in required {
            assert!(
                card.contains(field),
                "missing {field:?} at {width} columns: {rows:?}"
            );
        }
        assert!(rows.iter().any(|row| row.contains("/reconnect")));
        assert!(!card.contains('…'), "diagnostic was elided: {card}");
        assert!(
            model.composer_rect.get().is_some(),
            "composer stays visible"
        );
    }
}

#[test]
fn mismatch_card_preserves_versions_gap_and_reconnect_at_120_and_80_columns() {
    let (mut model, mut driver) = ready();
    handshake(&mut model, &mut driver, "0.0.999", 2);
    event(&mut model, &mut driver, 0, user(1));
    event(&mut model, &mut driver, 0, user(4));
    assert_diagnostic_survives_resize(
        &model,
        &[
            "Client/daemon version mismatch — reconnect",
            &format!("client {} (protocol 1)", env!("CARGO_PKG_VERSION")),
            "daemon 0.0.999 (protocol 2)",
            "gap after 1, received 4 (user_message)",
            "reconnect with /reconnect",
            "client-daemon-incompatible",
        ],
    );
}

#[test]
fn malformed_resync_card_preserves_type_sequence_and_reconnect_at_120_and_80_columns() {
    let (mut model, mut driver) = ready();
    event(&mut model, &mut driver, 0, user(1));
    for attempt in 0..=u64::from(haider_tui::stream_recovery::RESYNC_ATTEMPTS) {
        if attempt > 0 {
            attached(&mut model, &mut driver, attempt);
        }
        event(
            &mut model,
            &mut driver,
            attempt,
            raw(2, json!({"type":42}), true),
        );
    }
    assert_diagnostic_survives_resize(
        &model,
        &[
            "Session resync failed — reconnect",
            "2 journal replays made no progress after 1",
            "malformed payload type <missing/empty/non-string> at seq 2",
            "reconnect with /reconnect",
            "session-resync-failed",
        ],
    );
}

#[test]
fn additive_decoder_goldens_never_count_unknown_on_active_background_or_bare_projection() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../haider-protocol/tests/fixtures");
    let mut payloads = Vec::new();
    for file in [
        "convergence_graph_facts.json",
        "convergence_graph_m2a_authority.json",
        "agent_messaged.json",
        "agent_metrics_snapshot.json",
        "task_started.json",
        "task_completed.json",
        "hook_fired.json",
        "hook_trust_changed.json",
        "session_renamed.json",
        "session_config_effort_selected.json",
        "session_config_fast_selected.json",
        "project_instructions_loaded.json",
        "branch_created.json",
        "workspace_selected.json",
        "permission_grant_needed.json",
        "permission_grant_resolved.json",
        "usage_account_tagged.json",
    ] {
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(file)).expect("golden file"))
                .expect("golden JSON");
        if file == "usage_account_tagged.json" {
            value["type"] = json!("usage");
        } else if file == "agent_metrics_snapshot.json" {
            value["type"] = json!("agent_metrics");
        }
        if let Value::Array(values) = value {
            payloads.extend(values);
        } else {
            payloads.push(value);
        }
    }
    let additional: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/compat_additive.json"))
            .expect("additive golden");
    // Public golden bodies also decode through their declared union. Private
    // monitor receipts deliberately remain opaque to clients.
    for payload in &additional {
        let kind = payload["type"].as_str().expect("tag");
        if kind.starts_with("workflow_") {
            assert!(
                haider_protocol::graph::WorkflowGraphJournalEvent::from_payload_value(payload)
                    .expect("workflow golden")
                    .is_some()
            );
        } else if kind.starts_with("child_") || kind == "queue_changed" {
            serde_json::from_value::<haider_protocol::EventPayload>(payload.clone())
                .expect("core golden");
        }
    }
    payloads.extend(additional);
    let (mut model, mut driver) = ready();
    let mut projection = haider_tui::projection::SessionProjection::default();
    for (index, payload) in payloads.into_iter().enumerate() {
        assert!(
            payload.get("type").is_some(),
            "golden must be a payload: {payload:?}"
        );
        for ui in [false, true] {
            let seq = (index * 2 + usize::from(ui)) as u64 + 1;
            let envelope = raw(seq, payload.clone(), ui);
            assert!(
                event(&mut model, &mut driver, 0, envelope.clone())
                    .iter()
                    .all(|command| !matches!(
                        command,
                        LiveCommand::SessionDiagnostic { .. } | LiveCommand::Attach { .. }
                    ))
            );
            projection.apply_raw(&envelope);
            assert_eq!(
                model.projection.unknown_payloads(),
                0,
                "type {:?}",
                payload.get("type")
            );
            assert_eq!(projection.unknown_payloads(), 0);
            assert!(model.compatibility_diagnostic.is_none());
        }
    }
    // The same raw decoder law holds after checkout to a cold session slot.
    model.back_to_launcher();
    let seq = model
        .sessions
        .iter()
        .find(|s| s.id == sid())
        .expect("session")
        .projection
        .last_applied()
        .expect("cursor")
        + 1;
    event(
        &mut model,
        &mut driver,
        0,
        raw(
            seq,
            json!({"type":"monitor_report_pending","pending":{}}),
            true,
        ),
    );
    assert_eq!(
        model
            .sessions
            .iter()
            .find(|s| s.id == sid())
            .expect("session")
            .projection
            .unknown_payloads(),
        0
    );
}

#[test]
fn same_version_monitor_burst_and_historical_false_report_never_latch() {
    let (mut model, mut driver) = ready();
    // Exact type/UI sequence preceding the real 30674 report: 30667..30669.
    for (i, kind) in [
        "monitor_report_delivered",
        "monitor_removed",
        "monitor_report_pending",
    ]
    .iter()
    .cycle()
    .take(300)
    .enumerate()
    {
        let commands = event(
            &mut model,
            &mut driver,
            0,
            raw(i as u64 + 1, json!({"type":kind}), false),
        );
        assert!(commands.is_empty());
    }
    event(
        &mut model,
        &mut driver,
        0,
        raw(
            301,
            json!({"type":"client_diagnostic", "command_id":"old", "code":"client-daemon-incompatible", "message":"sustained unknown-payload or sequence-gap mismatch — update Haider"}),
            true,
        ),
    );
    assert_eq!(model.projection.last_applied(), Some(301));
    assert!(model.compatibility_diagnostic.is_none());
    assert_eq!(model.projection.unknown_payloads(), 0);
    assert_eq!(driver.outbox_len(), 0);
}

#[test]
fn gap_replays_from_applied_cursor_and_caught_up_clears_visible_resync() {
    let (mut model, mut driver) = ready();
    event(&mut model, &mut driver, 0, user(1));
    assert_eq!(
        event(&mut model, &mut driver, 0, user(4)),
        vec![
            LiveCommand::Detach { attachment: aid(0) },
            LiveCommand::Attach {
                session: sid(),
                after_seq: 1
            }
        ]
    );
    assert!(model.resyncing.contains(&sid()));
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            haider_tui::render::render(&model, frame);
        })
        .expect("render");
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        text.contains("resyncing…"),
        "the actual renderer must expose recovery"
    );
    assert_eq!(model.projection.last_applied(), Some(1));
    assert!(model.compatibility_diagnostic.is_none());
    // A burst already queued for the retired attachment consumes no retries.
    for seq in 5..100 {
        assert!(event(&mut model, &mut driver, 0, user(seq)).is_empty());
    }
    attached(&mut model, &mut driver, 1);
    for seq in 2..=4 {
        event(&mut model, &mut driver, 1, user(seq));
    }
    assert!(caught_up(&mut model, &mut driver, 1, 4).is_empty());
    assert!(!model.resyncing.contains(&sid()));
    assert_eq!(model.projection.last_applied(), Some(4));
    assert!(model.stream_diagnostics.is_empty());
}

#[test]
fn lagged_and_missing_catch_up_tail_use_the_same_bounded_replay() {
    let (mut model, mut driver) = ready();
    event(&mut model, &mut driver, 0, user(1));
    assert_eq!(
        driver.apply(&mut model, LiveReply::Lagged { attachment: aid(0) }),
        vec![LiveCommand::Attach {
            session: sid(),
            after_seq: 1
        }]
    );
    assert!(model.resyncing.contains(&sid()));
    attached(&mut model, &mut driver, 1);
    assert_eq!(
        caught_up(&mut model, &mut driver, 1, 4),
        vec![
            LiveCommand::Detach { attachment: aid(1) },
            LiveCommand::Attach {
                session: sid(),
                after_seq: 1
            }
        ]
    );
    attached(&mut model, &mut driver, 2);
    assert_eq!(
        caught_up(&mut model, &mut driver, 2, 4),
        vec![LiveCommand::Detach { attachment: aid(2) }]
    );
    assert!(model.compatibility_diagnostic.is_none());
    let error = &model.stream_diagnostics[&sid()];
    assert_eq!(error.subcode.as_str(), "session-resync-failed");
    assert!(error.detail.contains("after 1"));
    assert!(error.detail.contains("advertised 4"));
    assert!(!error.detail.contains("update"));
    assert!(
        driver.sync_selection(&model).is_empty(),
        "failed recovery cannot spin"
    );
    common::submit(&mut model, "/reconnect");
    assert!(model.requests.contains(&AppRequest::Reconnect));
    let commands = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    assert_eq!(commands, vec![LiveCommand::Reconnect]);
    assert!(model.stream_diagnostics.is_empty());
}

#[test]
fn malformed_structure_retries_without_advancing_or_claiming_same_version_incompatibility() {
    let (mut model, mut driver) = ready();
    event(&mut model, &mut driver, 0, user(1));
    for attempt in 0..=u64::from(haider_tui::stream_recovery::RESYNC_ATTEMPTS) {
        if attempt > 0 {
            attached(&mut model, &mut driver, attempt);
        }
        let commands = event(
            &mut model,
            &mut driver,
            attempt,
            raw(2, json!({"type":42}), true),
        );
        assert_eq!(model.projection.last_applied(), Some(1));
        assert!(model.compatibility_diagnostic.is_none());
        assert_eq!(
            commands
                .iter()
                .filter(|c| matches!(c, LiveCommand::Attach { .. }))
                .count(),
            usize::from(attempt < 2)
        );
    }
    assert!(
        model.stream_diagnostics[&sid()]
            .detail
            .contains("<missing/empty/non-string>")
    );
    assert!(model.stream_diagnostics[&sid()].detail.contains("seq 2"));
}

#[test]
fn real_handshake_mismatch_names_both_versions_and_reconnect_then_healthy_pair_clears_it() {
    let (mut model, mut driver) = ready();
    handshake(&mut model, &mut driver, "0.0.999", 1);
    let error = model.compatibility_diagnostic.as_ref().expect("mismatch");
    assert!(error.detail.contains(env!("CARGO_PKG_VERSION")));
    assert!(error.detail.contains("0.0.999"));
    assert!(error.detail.contains("protocol 1"));
    assert!(
        error
            .allowed_actions
            .contains(&haider_protocol::error::ErrorAction::Reconnect)
    );
    event(&mut model, &mut driver, 0, user(1));
    event(&mut model, &mut driver, 0, user(4));
    assert!(
        model
            .compatibility_diagnostic
            .as_ref()
            .expect("mismatch")
            .detail
            .contains("gap after 1, received 4 (user_message)")
    );
    for attempt in 1..=2 {
        attached(&mut model, &mut driver, attempt);
        caught_up(&mut model, &mut driver, attempt, 4);
    }
    let detail = &model
        .compatibility_diagnostic
        .as_ref()
        .expect("mismatch")
        .detail;
    assert!(detail.contains("gap after 1, received 4 (user_message)"));
    assert!(detail.contains("catch-up gap after 1, advertised 4"));
    handshake(&mut model, &mut driver, env!("CARGO_PKG_VERSION"), 1);
    assert!(model.compatibility_diagnostic.is_none());
    handshake(&mut model, &mut driver, "0.0.999", 2);
    assert!(
        model
            .compatibility_diagnostic
            .as_ref()
            .expect("mismatch")
            .detail
            .contains("protocol 2")
    );
}

#[test]
fn same_version_never_latches_even_with_inconsistent_protocol_metadata() {
    let (mut model, mut driver) = ready();
    handshake(&mut model, &mut driver, env!("CARGO_PKG_VERSION"), 2);
    assert!(model.compatibility_diagnostic.is_none());
}

#[test]
fn only_malformed_raw_structure_increments_the_diagnostic_counter() {
    let mut projection = haider_tui::projection::SessionProjection::default();
    for (index, payload) in [
        json!(null),
        json!([]),
        json!({}),
        json!({"type":42}),
        json!({"type":"  "}),
    ]
    .into_iter()
    .enumerate()
    {
        projection.apply_raw(&raw(index as u64 + 1, payload, true));
    }
    assert_eq!(projection.unknown_payloads(), 5);
    // Unknown nested variants and future type tags are opaque, not malformed.
    projection.apply_raw(&raw(
        6,
        json!({"type":"item","event":"future_lifecycle"}),
        true,
    ));
    projection.apply_raw(&raw(7, json!({"type":"future_family"}), true));
    assert_eq!(projection.unknown_payloads(), 5);
}

#[test]
fn replay_progress_renews_the_attempt_budget_under_repeated_delivery_pressure() {
    let (mut model, mut driver) = ready();
    event(&mut model, &mut driver, 0, user(1));
    for attempt in 0..8 {
        let commands = event(&mut model, &mut driver, attempt, user(100));
        assert!(commands.contains(&LiveCommand::Attach {
            session: sid(),
            after_seq: attempt + 1
        }));
        attached(&mut model, &mut driver, attempt + 1);
        event(&mut model, &mut driver, attempt + 1, user(attempt + 2));
        assert!(model.stream_diagnostics.is_empty());
        assert!(model.compatibility_diagnostic.is_none());
    }
    caught_up(&mut model, &mut driver, 8, 9);
    assert!(model.resyncing.is_empty());
}

#[test]
fn failed_replay_rpc_is_bounded_and_preserves_cursor_for_reconnect() {
    let (mut model, mut driver) = ready();
    event(&mut model, &mut driver, 0, user(1));
    event(&mut model, &mut driver, 0, user(4));
    for attempt in 1..=2 {
        let commands = driver.apply(
            &mut model,
            LiveReply::AttachFailed {
                session: sid(),
                code: "overloaded".into(),
                message: "fixture".into(),
                retryable: true,
            },
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, LiveCommand::Attach { .. }))
                .count(),
            usize::from(attempt == 1)
        );
    }
    let detail = &model.stream_diagnostics[&sid()].detail;
    assert!(detail.contains("gap after 1, received 4 (user_message)"));
    assert!(detail.contains("replay attach failed: overloaded"));
    assert_eq!(model.projection.last_applied(), Some(1));
    assert!(model.compatibility_diagnostic.is_none());
    assert!(
        model.stream_diagnostics[&sid()]
            .detail
            .contains("overloaded")
    );
    assert!(driver.sync_selection(&model).is_empty());
}
