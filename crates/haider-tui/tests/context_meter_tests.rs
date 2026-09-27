//! 973-context-meter — the session context meter per model.
//!
//! Owner report (2026-09-24): "the context limit and % and compaction UI for
//! a session are wrong/incorrect per model (it's always full 100%)". Root
//! cause: the live identity was seeded with the profile's 4,096-token OUTPUT
//! budget as its context window and an undeclared model kept that seed, so
//! the meter divided a real prompt by 4,096. These laws pin the fix end to
//! end through the live driver: declared windows per model, an honest
//! "window unknown", the auto-compaction trigger, and updates after a model
//! switch and after a compaction. `context_meter.golden` pins the rendered
//! status line and `/tokens` panel at 118x36 and 80x24
//! (`UPDATE_CONTEXT_METER_GOLDEN=1` regenerates it; review the diff).
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::{CredentialAlias, ItemId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::app::{AppModel, RuntimeMode, Screen};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::render::{render, status_left_string};
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{launcher_model, run_slash};

fn oauth_descriptor(provider: &str) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(provider),
        provider: provider.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "owner@example.test".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

fn summary(
    provider: &str,
    entries: &[(&str, Option<u64>)],
    default: &str,
) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiResponses,
        endpoint: None,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: entries.iter().map(|(slug, _)| (*slug).to_owned()).collect(),
        model_details: entries
            .iter()
            .map(|(slug, window)| haider_rpc::ModelDetailWire {
                name: (*slug).to_owned(),
                display_name: None,
                context_window: *window,
                max_output_tokens: None,
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
                supports_vision: None,
                source: None,
            })
            .collect(),
        catalog: haider_rpc::ProviderCatalogKindWire::Unknown,
        inventory: haider_rpc::ModelInventoryWire::Static,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
        auth_methods: vec![AuthMethod::OAuth],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some(default.to_owned()),
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

/// A live model on the Anthropic subscription, the OUTPUT-budget seed in
/// place exactly as the live launcher used to seed it, with a catalog of
/// realistic identities: two Claude rows, two GPT rows, and
/// a custom model the catalog lists without a window.
fn live_session(anthropic_windows: bool) -> (AppModel, LiveDriver) {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.identity.provider = "anthropic-oauth".to_owned();
    model.identity.model_short = "claude-opus-5-5".to_owned();
    model.identity.context_window = 4_096;
    let mut driver = LiveDriver::new("context-meter");
    let pass = |driver: &mut LiveDriver, model: &mut AppModel, reply| {
        live_pass(driver, model, Some(reply), std::time::Instant::now());
    };
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![
                oauth_descriptor("anthropic-oauth"),
                oauth_descriptor("openai-oauth"),
            ],
            revision: Some(1),
            sources: Vec::new(),
        },
    );
    let (opus, sonnet) = if anthropic_windows {
        (Some(1_000_000), Some(200_000))
    } else {
        (None, None)
    };
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![
                summary(
                    "anthropic-oauth",
                    &[
                        ("claude-opus-5-5", opus),
                        ("claude-sonnet-4-6", sonnet),
                        ("my-custom-model", None),
                    ],
                    "claude-opus-5-5",
                ),
                summary(
                    "openai-oauth",
                    &[("gpt-5.6-sol", Some(272_000)), ("gpt-6-sol", Some(400_000))],
                    "gpt-5.6-sol",
                ),
            ],
            revision: 1,
        },
    );
    model.identity.provider = "anthropic-oauth".to_owned();
    model.identity.model_short = "claude-opus-5-5".to_owned();
    model.refresh_context_window();
    // These synthetic request snapshots stand in for a session whose
    // initial daemon selection has already committed.
    model.meter_epoch.admit(
        ("anthropic-oauth".into(), "claude-opus-5-5".into()),
        Some(0),
        Some(30_000),
        Some(false),
    );
    (model, driver)
}

fn snapshot(
    input: u64,
    cached: u64,
    output: u64,
    window: Option<u64>,
    truth: ContextFootprintTruth,
) -> ContextFootprint {
    let reserved = 30_000;
    ContextFootprint {
        selection_epoch: None,
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        used_tokens: input + cached + output,
        context_window: window,
        reserved_output_tokens: reserved,
        soft_threshold_tokens: window.and_then(|window| {
            haider_protocol::context::context_soft_threshold_tokens(window, reserved)
        }),
        estimated_turns_to_threshold: None,
        truth,
        accounting: None,
    }
}

fn apply_footprint(model: &mut AppModel, id: &str, footprint: &ContextFootprint) {
    let mut footprint = footprint.clone();
    if footprint.selection_epoch.is_none() {
        footprint.selection_epoch = model.meter_epoch.selection_epoch;
    }
    let item: TurnItem = footprint.extension_item().expect("carrier");
    for payload in [
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new(id),
            item: item.clone(),
        }),
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item,
        }),
    ] {
        model.projection.apply(&payload);
    }
}

fn pick_model(model: &mut AppModel, slug: &str) {
    run_slash(model, &format!("/model {slug}"));
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Enter));
    assert_eq!(model.identity.model_short, slug, "picker selected {slug}");
    // The launcher pick above is local. These meter tests then model the
    // daemon's committed selection before checking snapshot admission.
    let next_epoch = model.meter_epoch.selection_epoch.unwrap_or(0) + 1;
    model.meter_epoch.admit(
        (
            model.identity.provider.clone(),
            model.identity.model_short.clone(),
        ),
        Some(next_epoch),
        Some(30_000),
        Some(false),
    );
}

fn draw(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// REGRESSION (owner's "always 100%"): the Anthropic subscription catalog
/// declares no windows; a normal ~20k prompt must not read `100% of 4.1k`.
///
/// MUTATION CHECK: restore the old `refresh_context_window` (keep the seed
/// when undeclared). Expected runtime failure: the line reads
/// `100% of 4.1k`.
#[test]
fn regression_an_undeclared_window_never_reads_one_hundred_percent() {
    let (mut model, _driver) = live_session(false);
    apply_footprint(
        &mut model,
        "fp-1",
        &snapshot(1_200, 18_000, 400, None, ContextFootprintTruth::Exact),
    );
    let line = status_left_string(&model, 118);
    assert!(!line.contains("100%"), "{line}");
    assert!(!line.contains("4.1k"), "{line}");
    assert!(line.contains("20k tok · window unknown"), "{line}");
}

#[test]
fn each_model_meters_against_its_own_window_and_trigger() {
    let (mut model, _driver) = live_session(true);
    // (provider, model, expected window text, expected percent, trigger)
    let cases = [
        (
            "anthropic-oauth",
            "claude-opus-5-5",
            "of 1M",
            "4%",
            "compact at 85%",
        ),
        (
            "anthropic-oauth",
            "claude-sonnet-4-6",
            "of 200k",
            "20%",
            "compact at 85%",
        ),
        (
            "openai-oauth",
            "gpt-5.6-sol",
            "of 272k",
            "15%",
            "compact at 85%",
        ),
        (
            "openai-oauth",
            "gpt-6-sol",
            "of 400k",
            "10%",
            "compact at 85%",
        ),
    ];
    for (provider, slug, window, percent, trigger) in cases {
        model.identity.provider = provider.to_owned();
        model.identity.model_short = slug.to_owned();
        model.refresh_context_window();
        let next_epoch = model.meter_epoch.selection_epoch.unwrap_or(0) + 1;
        model.meter_epoch.admit(
            (provider.to_owned(), slug.to_owned()),
            Some(next_epoch),
            Some(30_000),
            Some(false),
        );
        let window_tokens = model.identity.context_window;
        // The daemon's snapshot for THIS model's request.
        apply_footprint(
            &mut model,
            &format!("fp-{slug}"),
            &snapshot(
                1_000,
                38_000,
                1_000,
                Some(window_tokens),
                ContextFootprintTruth::Exact,
            ),
        );
        let line = status_left_string(&model, 118);
        assert!(line.contains("40k tok"), "{slug}: {line}");
        assert!(line.contains(window), "{slug}: {line}");
        assert!(line.contains(&format!(" {percent} ")), "{slug}: {line}");
        assert!(line.contains(trigger), "{slug}: {line}");
        assert!(
            !line.contains('~'),
            "{slug}: daemon trigger is exact: {line}"
        );
    }
}

/// MUTATION CHECK: make the meter prefer the snapshot window over the
/// current identity's. Expected runtime failure: after the switch the line
/// still reads `of 1M`.
#[test]
fn a_model_switch_rebases_the_window_and_projects_the_trigger() {
    let (mut model, _driver) = live_session(true);
    apply_footprint(
        &mut model,
        "fp-opus",
        &snapshot(
            2_000,
            100_000,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    let before = status_left_string(&model, 118);
    assert!(
        before.contains("103k tok") && before.contains("10% of 1M"),
        "{before}"
    );
    assert!(before.contains("compact at 85%"), "{before}");

    pick_model(&mut model, "claude-sonnet-4-6");
    let after = status_left_string(&model, 118);
    assert!(after.contains("52% of 200k"), "{after}");
    assert!(
        !after.contains("compact at"),
        "the bar shows only the daemon's trigger; the new model has none yet: {after}"
    );
    let meter = model.context_meter();
    assert_eq!(meter.auto_compact_at, Some(170_000));
    assert!(meter.threshold_projected, "the panel's projected trigger");

    // Switching to a listed model WITHOUT a window never borrows the
    // previous snapshot's.
    pick_model(&mut model, "my-custom-model");
    let custom = status_left_string(&model, 118);
    assert!(custom.contains("103k tok · window unknown"), "{custom}");
}

/// 973-context-meter verify HOLD (regression): switching to an
/// unknown-window model before its first turn left the previous model's
/// `≈N turns to auto-compaction` in the ⌃G panel.
///
/// MUTATION CHECK: take `turns_to_threshold` whenever the trigger is not
/// projected. Expected runtime failure: the panel keeps `≈1139 turns`.
#[test]
fn a_switch_to_an_unknown_window_drops_the_turns_estimate_in_the_panel() {
    let (mut model, _driver) = live_session(true);
    let mut opus = snapshot(
        2_700,
        50_000,
        700,
        Some(1_000_000),
        ContextFootprintTruth::Exact,
    );
    opus.estimated_turns_to_threshold = Some(1_139);
    apply_footprint(&mut model, "fp-opus-turns", &opus);
    assert!(
        panel_rows(&mut model, 118, 36)
            .iter()
            .any(|row| row.contains("≈1139 turns"))
    );

    pick_model(&mut model, "my-custom-model");
    let rows = panel_rows(&mut model, 118, 36);
    assert!(
        rows.iter().any(|row| row.contains("window unknown")),
        "{rows:#?}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("turns to auto-compaction")),
        "stale turns estimate after the switch: {rows:#?}"
    );
}

/// Models OUTSIDE the catalog (the daemon knows their window, the client
/// has no row): the snapshot's window serves — but never across a switch.
///
/// MUTATION CHECK: drop selection-epoch equality from `context_meter`.
/// Expected runtime failure: after the switch the line keeps `of 1M`.
#[test]
fn a_switch_outside_the_catalog_never_keeps_the_previous_snapshot_window() {
    let (mut model, _driver) = live_session(true);
    model.identity.provider = "anthropic".to_owned();
    model.identity.model_short = "claude-opus-5-5".to_owned();
    model.refresh_context_window();
    model.meter_epoch.admit(
        ("anthropic".into(), "claude-opus-5-5".into()),
        Some(1),
        Some(30_000),
        Some(false),
    );
    apply_footprint(
        &mut model,
        "fp-unlisted",
        &snapshot(
            2_000,
            60_000,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    let before = status_left_string(&model, 118);
    assert!(before.contains("6% of 1M · compact at 85%"), "{before}");

    model.identity.model_short = "claude-sonnet-4-6".to_owned();
    model.refresh_context_window();
    model.meter_epoch.admit(
        ("anthropic".into(), "claude-sonnet-4-6".into()),
        Some(2),
        Some(30_000),
        Some(false),
    );
    let switched = status_left_string(&model, 118);
    assert!(switched.contains("63k tok · window unknown"), "{switched}");

    apply_footprint(
        &mut model,
        "fp-unlisted-2",
        &snapshot(
            2_000,
            80_000,
            1_000,
            Some(200_000),
            ContextFootprintTruth::Exact,
        ),
    );
    let next = status_left_string(&model, 118);
    assert!(next.contains("42% of 200k · compact at 85%"), "{next}");
}

#[test]
fn a_compaction_snapshot_lowers_the_meter() {
    let (mut model, _driver) = live_session(true);
    pick_model(&mut model, "claude-sonnet-4-6");
    apply_footprint(
        &mut model,
        "fp-full",
        &snapshot(
            4_000,
            170_000,
            2_000,
            Some(200_000),
            ContextFootprintTruth::Exact,
        ),
    );
    let before = status_left_string(&model, 118);
    assert!(before.contains("88% of 200k"), "{before}");
    // The daemon's post-compaction snapshot is an estimate of the new
    // prompt; the meter follows it and wears `~`.
    apply_footprint(
        &mut model,
        "fp-compacted",
        &snapshot(
            12_000,
            0,
            0,
            Some(200_000),
            ContextFootprintTruth::Estimated,
        ),
    );
    let after = status_left_string(&model, 118);
    assert!(
        after.contains("~12k tok") && after.contains("6% of 200k"),
        "{after}"
    );
    assert!(after.contains("compact at 85%"), "{after}");
}

fn panel_rows(model: &mut AppModel, width: u16, height: u16) -> Vec<String> {
    let screen = model.screen;
    model.screen = Screen::Session;
    model.token_panel = true;
    let rows = draw(model, width, height);
    model.token_panel = false;
    model.screen = screen;
    rows
}

fn golden_case(model: &mut AppModel, label: &str, out: &mut String) {
    for (width, height) in [(118_u16, 36_u16), (80, 24)] {
        out.push_str(&format!("== {label} @ {width}x{height}\n"));
        out.push_str("status: ");
        out.push_str(&status_left_string(model, width));
        out.push('\n');
        // The panel is session-scoped; the launcher keeps picks local.
        let screen = model.screen;
        model.screen = Screen::Session;
        // A transient flash (e.g. `· model → …`) overlays the bar; the
        // golden pins the meter, not the flash timing.
        model.flash = None;
        model.token_panel = true;
        let rows = draw(model, width, height);
        model.token_panel = false;
        model.screen = screen;
        for row in rows.iter().filter(|row| {
            row.contains("context by model")
                || row.contains("● ")
                || row.starts_with(" │    ")
                || row.contains("tok ·")
        }) {
            out.push_str(row);
            out.push('\n');
        }
    }
}

#[test]
fn context_meter_golden() {
    let mut out = String::new();
    let (mut model, _driver) = live_session(false);
    golden_case(
        &mut model,
        "anthropic-oauth undeclared · no snapshot",
        &mut out,
    );
    apply_footprint(
        &mut model,
        "fp-1",
        &snapshot(1_200, 18_000, 400, None, ContextFootprintTruth::Exact),
    );
    golden_case(
        &mut model,
        "anthropic-oauth undeclared · snapshot",
        &mut out,
    );

    let (mut model, _driver) = live_session(true);
    golden_case(&mut model, "claude-opus-5-5 · session start", &mut out);
    apply_footprint(
        &mut model,
        "fp-opus",
        &snapshot(
            2_000,
            100_000,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    golden_case(&mut model, "claude-opus-5-5 · after a turn", &mut out);
    let mut opus = snapshot(
        2_700,
        50_000,
        700,
        Some(1_000_000),
        ContextFootprintTruth::Exact,
    );
    opus.estimated_turns_to_threshold = Some(1_139);
    apply_footprint(&mut model, "fp-opus-2", &opus);
    golden_case(&mut model, "claude-opus-5-5 · turns estimate", &mut out);
    pick_model(&mut model, "my-custom-model");
    golden_case(
        &mut model,
        "my-custom-model · after switch (unknown window)",
        &mut out,
    );
    pick_model(&mut model, "claude-sonnet-4-6");
    golden_case(&mut model, "claude-sonnet-4-6 · after switch", &mut out);
    let next_epoch = model.meter_epoch.selection_epoch.unwrap_or_default() + 1;
    model.meter_epoch.admit(
        ("openai-oauth".into(), "gpt-6-sol".into()),
        Some(next_epoch),
        Some(30_000),
        Some(false),
    );
    model.identity.provider = "openai-oauth".to_owned();
    model.identity.model_short = "gpt-6-sol".to_owned();
    model.refresh_context_window();
    apply_footprint(
        &mut model,
        "fp-gpt",
        &snapshot(
            3_000,
            100_000,
            1_500,
            Some(400_000),
            ContextFootprintTruth::Exact,
        ),
    );
    golden_case(&mut model, "gpt-6-sol · after a turn", &mut out);
    apply_footprint(
        &mut model,
        "fp-gpt-compacted",
        &snapshot(9_000, 0, 0, Some(400_000), ContextFootprintTruth::Estimated),
    );
    golden_case(&mut model, "gpt-6-sol · after compaction", &mut out);

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/context_meter/context_meter.golden");
    if std::env::var_os("UPDATE_CONTEXT_METER_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("fixture dir")).expect("fixture dir");
        std::fs::write(&path, &out).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).expect("golden exists");
    assert_eq!(out, expected, "context meter golden drifted");
}

// ---- 973-context-meter-fixes: the meter is SESSION state ----------------

fn session_id(name: &str) -> haider_protocol::ids::SessionId {
    haider_protocol::ids::SessionId::new(name)
}

fn attachment_of(session: &haider_protocol::ids::SessionId) -> haider_rpc::AttachmentId {
    haider_rpc::AttachmentId::new(format!("att-{}", session.as_str()))
}

/// One committed envelope for `session` through the live driver's event
/// route (the attached/background route every daemon event takes).
fn deliver(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    session: &haider_protocol::ids::SessionId,
    seq: u64,
    payload: serde_json::Value,
) {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets};
    let envelope = EventEnvelope {
        schema_version: 1,
        event_id: haider_protocol::ids::EventId::new(format!("evt-{}-{seq}", session.as_str())),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: haider_protocol::ids::DeviceId::new("meter-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    };
    driver.apply(
        model,
        LiveReply::Event {
            attachment: attachment_of(session),
            session: session.clone(),
            envelope: Box::new(envelope),
        },
    );
}

/// The daemon's request-boundary snapshot for `session`, as the two item
/// envelopes it commits. Returns the next free sequence.
fn deliver_footprint(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    session: &haider_protocol::ids::SessionId,
    seq: u64,
    footprint: &ContextFootprint,
) -> u64 {
    let mut footprint = footprint.clone();
    if footprint.selection_epoch.is_none() {
        footprint.selection_epoch = if model.active_session.as_ref() == Some(session) {
            model.meter_epoch.selection_epoch
        } else {
            model
                .sessions
                .iter()
                .find(|row| &row.id == session)
                .and_then(|row| row.meter_epoch.selection_epoch)
        };
    }
    let item: TurnItem = footprint.extension_item().expect("carrier");
    let id = ItemId::new(format!("fp-{}-{seq}", session.as_str()));
    for (offset, payload) in [
        EventPayload::Item(ItemEvent::Started {
            item_id: id.clone(),
            item: item.clone(),
        }),
        EventPayload::Item(ItemEvent::Completed { item_id: id, item }),
    ]
    .iter()
    .enumerate()
    {
        deliver(
            driver,
            model,
            session,
            seq + offset as u64,
            serde_json::to_value(payload).expect("payload"),
        );
    }
    seq + 2
}

fn deliver_model_fact(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    session: &haider_protocol::ids::SessionId,
    seq: u64,
    provider: &str,
    slug: &str,
) {
    let fact = haider_protocol::session::ModelSelected {
        selection_epoch: Some(seq),
        provider: provider.to_owned(),
        model: slug.to_owned(),
    };
    deliver(
        driver,
        model,
        session,
        seq,
        fact.to_payload_value().expect("fact"),
    );
}

fn model_selected_reply(
    session: &haider_protocol::ids::SessionId,
    provider: &str,
    slug: &str,
    output_budget: Option<haider_protocol::output_budget::SessionOutputBudgetV1>,
) -> LiveReply {
    model_selected_reply_at(session, provider, slug, output_budget, 3)
}

fn model_selected_reply_at(
    session: &haider_protocol::ids::SessionId,
    provider: &str,
    slug: &str,
    output_budget: Option<haider_protocol::output_budget::SessionOutputBudgetV1>,
    selected_seq: u64,
) -> LiveReply {
    LiveReply::ModelSelected {
        connection_epoch: None,
        selected_seq,
        command_id: haider_rpc::CommandId::new(format!("select-{slug}")),
        session: session.clone(),
        provider: provider.to_owned(),
        model: slug.to_owned(),
        worker_generation: 7,
        output_budget,
    }
}

/// Two live sessions A and B, both attached through the driver, viewing A
/// on Opus (1M).
fn two_attached_sessions() -> (AppModel, LiveDriver) {
    let (mut model, mut driver) = live_session(true);
    model.sessions.clear();
    for name in ["s-meter-b", "s-meter-a"] {
        let session = session_id(name);
        model.upsert_live_session(&session);
        model.note_session_metadata_at(
            &session,
            "anthropic-oauth",
            "claude-opus-5-5",
            30_000,
            Some(0),
            None,
        );
    }
    model.open_session(&session_id("s-meter-a"));
    for name in ["s-meter-a", "s-meter-b"] {
        let session = session_id(name);
        driver.apply(
            &mut model,
            LiveReply::Attached {
                launch_origin: None,
                attachment: attachment_of(&session),
                session,
                worker_generation: 7,
                replay_through_seq: 0,
            },
        );
    }
    (model, driver)
}

/// 973-context-meter-fixes B1 (Astra): session A has a 1M window and a
/// durable 50k footprint. Visit B and select a 200k model there, then
/// reopen A while both stay attached. A must read `5% of 1M` — its own
/// model — not `25% of 200k` from the identity B left behind.
///
/// MUTATION CHECK: drop the epoch/identity restore from `open_session`.
/// Expected runtime failure: A reads `25% of 200k` after the return.
#[test]
fn a_reopened_session_meters_against_its_own_model_a_b_a() {
    let (mut model, mut driver) = two_attached_sessions();
    let a = session_id("s-meter-a");
    let b = session_id("s-meter-b");
    deliver_footprint(
        &mut driver,
        &mut model,
        &a,
        1,
        &snapshot(
            0,
            49_000,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    let on_a = status_left_string(&model, 118);
    assert!(
        on_a.contains("50k tok") && on_a.contains("5% of 1M"),
        "{on_a}"
    );
    assert!(on_a.contains("compact at 85%"), "{on_a}");

    model.open_session(&b);
    driver.apply(
        &mut model,
        model_selected_reply(&b, "anthropic-oauth", "claude-sonnet-4-6", None),
    );
    deliver_model_fact(
        &mut driver,
        &mut model,
        &b,
        1,
        "anthropic-oauth",
        "claude-sonnet-4-6",
    );
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    let on_b = status_left_string(&model, 118);
    assert!(on_b.contains("of 200k"), "{on_b}");

    model.open_session(&a);
    assert_eq!(
        model.identity.model_short, "claude-opus-5-5",
        "A's own model"
    );
    assert_eq!(model.identity.context_window, 1_000_000);
    let back = status_left_string(&model, 118);
    assert!(
        back.contains("50k tok") && back.contains("5% of 1M"),
        "{back}"
    );
    assert!(
        back.contains("compact at 85%"),
        "A's daemon trigger stands: {back}"
    );
    assert!(!back.contains("200k"), "{back}");
    let meter = model.context_meter();
    assert_eq!(meter.auto_compact_at, Some(850_000));
    assert!(!meter.threshold_projected);

    // And B keeps ITS model when revisited.
    model.open_session(&b);
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    assert!(status_left_string(&model, 118).contains("of 200k"));
}

/// B1: a BACKGROUND model change (another client switches parked session B)
/// binds B's own epoch: the viewed A is untouched, and reopening B (which
/// replays nothing — it stays attached) meters B's pre-switch snapshot
/// against the new model's window with a projected trigger. A selection
/// reply that lands after the user left B binds B, not the viewed A.
///
/// MUTATION CHECK: make `apply_tuning_fact` ignore parked sessions again.
/// Expected runtime failure: reopened B still reads `of 1M` on Opus.
#[test]
fn a_background_model_change_binds_the_parked_session() {
    let (mut model, mut driver) = two_attached_sessions();
    let a = session_id("s-meter-a");
    let b = session_id("s-meter-b");
    // B ran a turn on Opus while viewed.
    model.open_session(&b);
    deliver_footprint(
        &mut driver,
        &mut model,
        &b,
        1,
        &snapshot(
            0,
            99_000,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    assert!(status_left_string(&model, 118).contains("10% of 1M"));
    model.open_session(&a);

    // Another surface switches B to Sonnet while A is on screen.
    deliver_model_fact(
        &mut driver,
        &mut model,
        &b,
        3,
        "anthropic-oauth",
        "claude-sonnet-4-6",
    );
    assert_eq!(model.identity.model_short, "claude-opus-5-5", "A untouched");
    assert!(status_left_string(&model, 118).contains("of 1M"));

    model.open_session(&b);
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    let on_b = status_left_string(&model, 118);
    assert!(
        on_b.contains("100k tok") && on_b.contains("50% of 200k"),
        "{on_b}"
    );
    assert!(!on_b.contains("compact at"), "trigger is projected: {on_b}");
    let meter = model.context_meter();
    assert_eq!(meter.auto_compact_at, Some(170_000));
    assert!(meter.threshold_projected);
    assert!(
        draw(&model, 118, 36)
            .iter()
            .any(|row| row.contains("⇄ model → claude-sonnet-4-6")),
        "the switch note reached the parked transcript"
    );

    // A late reply for B while A is viewed binds B only.
    model.open_session(&a);
    driver.apply(
        &mut model,
        model_selected_reply_at(&b, "openai-oauth", "gpt-6-sol", None, 4),
    );
    assert_eq!(model.identity.model_short, "claude-opus-5-5", "A untouched");
    model.open_session(&b);
    assert_eq!(model.identity.provider, "openai-oauth");
    assert_eq!(model.identity.model_short, "gpt-6-sol");
    assert!(status_left_string(&model, 118).contains("25% of 400k"));
}

fn detail(slug: &str, window: u64, max_output: u64) -> haider_rpc::ModelDetailWire {
    haider_rpc::ModelDetailWire {
        name: slug.to_owned(),
        display_name: None,
        context_window: Some(window),
        max_output_tokens: Some(max_output),
        supported_efforts: Vec::new(),
        default_effort: None,
        supported_speeds: Vec::new(),
        supports_thinking_type: None,
        supports_vision: None,
        source: None,
    }
}

/// A viewed live session on `eq-wide` (100k window, 60k max output) with
/// two more 100k models: `eq-small` (8,192 max) and `eq-mid` (60k max).
fn equal_window_session() -> (AppModel, LiveDriver, haider_protocol::ids::SessionId) {
    let (mut model, mut driver) = live_session(true);
    let mut provider = summary("eq-oauth", &[], "eq-wide");
    provider.models = vec!["eq-wide".into(), "eq-small".into(), "eq-mid".into()];
    provider.model_details = vec![
        detail("eq-wide", 100_000, 60_000),
        detail("eq-small", 100_000, 8_192),
        detail("eq-mid", 100_000, 60_000),
    ];
    let mut providers = model.providers.providers.clone();
    providers.push(provider);
    driver.apply(
        &mut model,
        LiveReply::Providers {
            providers,
            revision: 2,
        },
    );
    model.identity.provider = "eq-oauth".to_owned();
    model.identity.model_short = "eq-wide".to_owned();
    model.refresh_context_window();
    let s = session_id("s-meter-eq");
    model.sessions.clear();
    model.upsert_live_session(&s);
    model.note_session_metadata_at(
        &s,
        "eq-oauth",
        "eq-wide",
        50_000,
        Some(0),
        Some(
            haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                requested: 50_000,
            },
        ),
    );
    model.open_session(&s);
    driver.apply(
        &mut model,
        LiveReply::Attached {
            launch_origin: None,
            attachment: attachment_of(&s),
            session: s.clone(),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    (model, driver, s)
}

fn budget(
    max_tokens: u64,
    requested: u64,
    clamped: bool,
) -> haider_protocol::output_budget::SessionOutputBudgetV1 {
    haider_protocol::output_budget::SessionOutputBudgetV1 {
        max_tokens,
        source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet { requested },
        clamped: clamped.then_some(haider_protocol::output_budget::OutputBudgetClampV1 {
            requested,
            max_output_tokens: max_tokens,
        }),
    }
}

/// A user-set 50k reserve on a 100k model: 50k used, trigger 50k, ≈0 turns.
fn user_set_footprint() -> ContextFootprint {
    let mut footprint = snapshot(
        0,
        49_000,
        1_000,
        Some(100_000),
        ContextFootprintTruth::Exact,
    );
    footprint.reserved_output_tokens = 50_000;
    footprint.soft_threshold_tokens =
        haider_protocol::context::context_soft_threshold_tokens(100_000, 50_000);
    footprint.estimated_turns_to_threshold = Some(0);
    footprint
}

/// 973-context-meter-fixes B3 (Astra): EQUAL windows, different output
/// limits. After switching to the 8,192-max model the daemon clamps the
/// user's 50k reserve; before that model's first snapshot the meter must
/// not announce `compact at 50%` / `≈0 turns` as current — it projects
/// 85k from the COMMITTED budget in the selection reply.
///
/// MUTATION CHECK: drop `snapshot_predates` from `context_meter` (treat
/// every snapshot as current). Expected runtime failure: the status line
/// keeps `compact at 50%` and the panel `≈0 turns`.
#[test]
fn a_same_window_switch_projects_from_the_committed_budget() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    let before = status_left_string(&model, 118);
    assert!(before.contains("50% of 100k · compact at 50%"), "{before}");
    assert!(
        panel_rows(&mut model, 118, 36)
            .iter()
            .any(|row| row.contains("≈0 turns"))
    );

    // Clamped: user-set 50k does not fit eq-small's 8,192.
    driver.apply(
        &mut model,
        model_selected_reply(
            &s,
            "eq-oauth",
            "eq-small",
            Some(budget(8_192, 50_000, true)),
        ),
    );
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-small");
    let after = status_left_string(&model, 118);
    assert!(after.contains("50% of 100k"), "{after}");
    assert!(
        !after.contains("compact at"),
        "no stale daemon trigger: {after}"
    );
    let meter = model.context_meter();
    assert_eq!(meter.auto_compact_at, Some(85_000));
    assert!(meter.threshold_projected);
    assert_eq!(meter.turns_to_threshold, None);
    let rows = panel_rows(&mut model, 118, 36);
    assert!(
        rows.iter()
            .any(|row| row.contains("auto-compact at 85k (85%) (projected")),
        "{rows:#?}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("turns to auto-compaction")),
        "{rows:#?}"
    );

    // Not clamped: the user's 50k fits eq-mid (60k max) and SURVIVES — the
    // derivation alone (30k) would project 70k.
    driver.apply(
        &mut model,
        model_selected_reply_at(
            &s,
            "eq-oauth",
            "eq-mid",
            Some(budget(50_000, 50_000, false)),
            next + 1,
        ),
    );
    deliver_model_fact(&mut driver, &mut model, &s, next + 1, "eq-oauth", "eq-mid");
    let meter = model.context_meter();
    assert_eq!(meter.auto_compact_at, Some(50_000));
    assert!(meter.threshold_projected);

    // The new epoch's first snapshot governs again.
    let mut fresh = user_set_footprint();
    fresh.estimated_turns_to_threshold = Some(3);
    fresh.used_tokens = 40_000;
    fresh.cached_input_tokens = 39_000;
    deliver_footprint(&mut driver, &mut model, &s, next + 2, &fresh);
    let settled = status_left_string(&model, 118);
    assert!(
        settled.contains("40% of 100k · compact at 50%"),
        "{settled}"
    );
    assert_eq!(model.context_meter().turns_to_threshold, Some(3));
}

/// Carried follow-up: a different-window switch projects with the NEW
/// model's reserve — 400k/30k → 128k with a 16,384 max projects 108,800.
#[test]
fn a_window_switch_projects_with_the_new_models_reserve() {
    let (mut model, mut driver, s) = equal_window_session();
    let mut providers = model.providers.providers.clone();
    let eq = providers
        .iter_mut()
        .find(|provider| provider.provider == "eq-oauth")
        .expect("eq provider");
    eq.models
        .extend(["big-400k".to_owned(), "small-128k".to_owned()]);
    eq.model_details.extend([
        detail("big-400k", 400_000, 128_000),
        detail("small-128k", 128_000, 16_384),
    ]);
    driver.apply(
        &mut model,
        LiveReply::Providers {
            providers,
            revision: 3,
        },
    );
    driver.apply(
        &mut model,
        model_selected_reply(&s, "eq-oauth", "big-400k", None),
    );
    deliver_footprint(
        &mut driver,
        &mut model,
        &s,
        1,
        &snapshot(
            0,
            60_000,
            1_000,
            Some(400_000),
            ContextFootprintTruth::Exact,
        ),
    );
    assert_eq!(model.context_meter().auto_compact_at, Some(340_000));
    driver.apply(
        &mut model,
        model_selected_reply_at(&s, "eq-oauth", "small-128k", None, 4),
    );
    let meter = model.context_meter();
    assert_eq!(meter.window, Some(128_000));
    assert_eq!(meter.auto_compact_at, Some(108_800));
    assert!(meter.threshold_projected);
}

/// 973-context-meter-fixes B2 (Astra): the SAME model slug on two
/// providers. The parent runs `shared-model` on P (1M); a child runs it on
/// Q (128k) and has used 64k. The child's row reads 50% of 128k — never
/// P's 1M — from its own snapshot, from Q's catalog row without one, and
/// with NO catalog row at all when the manifest carries no provider.
///
/// MUTATION CHECK: look the chip's model up under `identity.provider`
/// again. Expected runtime failure: the snapshot-less child reads `of 1M`.
#[test]
fn a_cross_provider_child_meters_against_its_own_provider() {
    use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
    use haider_protocol::ids::{AgentId, LeaseId};
    let (mut model, mut driver) = live_session(true);
    let mut p = summary(
        "p-oauth",
        &[("shared-model", Some(1_000_000))],
        "shared-model",
    );
    p.model_details[0].max_output_tokens = Some(128_000);
    let mut q = summary(
        "q-oauth",
        &[("shared-model", Some(128_000))],
        "shared-model",
    );
    q.model_details[0].max_output_tokens = Some(16_384);
    driver.apply(
        &mut model,
        LiveReply::Providers {
            providers: vec![p, q],
            revision: 2,
        },
    );
    model.identity.provider = "p-oauth".to_owned();
    model.identity.model_short = "shared-model".to_owned();
    model.refresh_context_window();
    let manifest = |agent: &str, provider: Option<&str>| {
        let mut coordinates = serde_json::json!({});
        if let Some(provider) = provider {
            coordinates["provider"] = serde_json::json!(provider);
        }
        AgentManifest {
            agent: AgentId::new(agent),
            role: AgentRole::Subagent,
            task: format!("task {agent}"),
            callsign: Some(agent.to_owned()),
            model_profile: "shared-model".to_owned(),
            grant: Grant {
                tools: vec![],
                effect_ceiling: vec![],
            },
            budget_tokens: None,
            placement: Placement::Local,
            lease: LeaseId::new(format!("lease-{agent}")),
            fencing_epoch: 1,
            attempt: 1,
            parent: None,
            coordinates: Some(coordinates),
            cli_scope: None,
        }
    };
    for (agent, provider) in [("kid-q", Some("q-oauth")), ("kid-none", None)] {
        haider_tui::session::apply_agent_payload(
            &mut model.chips,
            &EventPayload::AgentSpawned(manifest(agent, provider)),
            0,
        );
    }
    for chip in &mut model.chips {
        chip.tokens = 64_000;
    }
    let rows = panel_rows(&mut model, 118, 36);
    let row = |agent: &str| {
        rows.iter()
            .find(|row| row.contains(&format!("task {agent}")))
            .cloned()
            .unwrap_or_else(|| panic!("row for {agent}: {rows:#?}"))
    };
    // No child snapshot: Q's catalog row (128k), never P's (1M).
    assert!(row("kid-q").contains("50%  64k/128k"), "{rows:#?}");
    // No provider provenance: no borrowed declaration — window unknown.
    assert!(
        row("kid-none").contains("64k tok · window unknown"),
        "{rows:#?}"
    );

    // With the child's own snapshot, that snapshot is authoritative.
    let kid = model
        .chips
        .iter_mut()
        .find(|chip| chip.agent == "kid-none")
        .expect("chip");
    let footprint = snapshot(
        0,
        63_000,
        1_000,
        Some(128_000),
        ContextFootprintTruth::Exact,
    );
    let item: TurnItem = footprint.extension_item().expect("carrier");
    for payload in [
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("kid-fp"),
            item: item.clone(),
        }),
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("kid-fp"),
            item,
        }),
    ] {
        kid.transcript.apply(&payload);
    }
    let rows = panel_rows(&mut model, 118, 36);
    let kid_row = rows
        .iter()
        .find(|row| row.contains("task kid-none"))
        .expect("row");
    assert!(kid_row.contains("50%  64k/128k"), "{rows:#?}");
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("└") && row.contains("1M")),
        "{rows:#?}"
    );
}

/// B1: the FIRST open of a session this process never viewed meters it
/// against its own committed model — seeded from the daemon's typed
/// `session.list` metadata — not the identity the viewed session left.
///
/// MUTATION CHECK: drop `note_session_metadata` from the `Listed` arm.
/// Expected runtime failure: the listed Sonnet session opens on Opus.
#[test]
fn a_listed_session_first_opens_on_its_own_model() {
    let (mut model, mut driver) = two_attached_sessions();
    let listed = session_id("s-meter-listed");
    let summary = listed_summary(&listed, "anthropic-oauth", "claude-sonnet-4-6", 30_000);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary],
            next_cursor: None,
        },
    );
    assert_eq!(model.identity.model_short, "claude-opus-5-5", "viewing A");
    model.open_session(&listed);
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    assert!(status_left_string(&model, 118).contains("of 200k"));
}

/// The daemon's `session.list` row for `session` with typed metadata.
fn listed_summary(
    session: &haider_protocol::ids::SessionId,
    provider: &str,
    slug: &str,
    max_tokens: u64,
) -> haider_rpc::SessionSummary {
    let metadata = haider_protocol::session::SessionMetadataV1 {
        selection_epoch: None,
        resolved_route_alias: None,
        resolved_route_seen: false,
        launch_origin: None,
        workspace_allocation: None,
        provider_base_url: None,
        provider_rebind_id: None,
        cwd: "/tmp/meter".to_owned(),
        provider: provider.into(),
        account_alias: None,
        model: slug.to_owned(),
        max_tokens,
        max_tokens_source: None,
        system_prompt_version: Some("test-v1".into()),
        permission_overrides: None,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1::Interactive,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        agent_type: None,
        context_economy: Default::default(),
        created_at_ms: 1,
    };
    haider_rpc::SessionSummary {
        latest_context_footprint: None,
        session_id: session.clone(),
        head_seq: 0,
        worker_generation: 7,
        run_state: None,
        run_id: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        metadata: Some(metadata),
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

/// B3 across surfaces: a model switched from ANOTHER surface lands as a
/// bare journal fact (no budget). The daemon's typed metadata for the same
/// pair (the `session.list` the driver asks for after a model fact) then
/// supplies the committed budget: a user-set 50k that fits eq-mid projects
/// 50k, not the derivation's 70k.
#[test]
fn a_fact_switch_learns_the_committed_budget_from_session_metadata() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-mid");
    let meter = model.context_meter();
    assert!(meter.threshold_projected);
    assert_eq!(
        meter.auto_compact_at,
        Some(50_000),
        "carried user reserve is assumed until the list"
    );
    assert_eq!(meter.turns_to_threshold, None);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![listed_summary(&s, "eq-oauth", "eq-mid", 50_000)],
            next_cursor: None,
        },
    );
    let meter = model.context_meter();
    assert!(meter.threshold_projected);
    assert_eq!(meter.auto_compact_at, Some(50_000));
    // Metadata for a DIFFERENT pair never rewrites this epoch's budget.
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![listed_summary(&s, "eq-oauth", "eq-small", 8_192)],
            next_cursor: None,
        },
    );
    assert_eq!(model.context_meter().auto_compact_at, Some(50_000));
}
