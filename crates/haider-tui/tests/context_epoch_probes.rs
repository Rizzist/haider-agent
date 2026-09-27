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
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
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
    // The launcher pick above is local. Model a committed selection and
    // confirmed reserve before checking request snapshot admission.
    let next = model.meter_epoch.selection_epoch.unwrap_or(0) + 1;
    model.meter_epoch.admit(
        (model.identity.provider.clone(), slug.to_owned()),
        Some(next),
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
) -> Vec<haider_tui::live::LiveCommand> {
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
    )
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
        footprint.selection_epoch = model.meter_epoch.selection_epoch;
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
        let id = session_id(name);
        model.upsert_live_session(&id);
        model.note_session_metadata_at(
            &id,
            "anthropic-oauth",
            "claude-opus-5-5",
            30_000,
            Some(0),
            Some(haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived),
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

#[test]
fn committed_session_model_becomes_the_next_launcher_default() {
    let (mut model, mut driver) = two_attached_sessions();
    let session = session_id("s-meter-a");
    let before = model.model_commits;
    driver.apply(
        &mut model,
        model_selected_reply(
            &session,
            "anthropic-oauth",
            "claude-sonnet-4-6",
            Some(budget(16_384, 16_384, false)),
        ),
    );
    assert!(
        model.model_commits > before,
        "the persistence sync must see the commit"
    );
    assert_eq!(model.launcher_identity.model_short, "claude-sonnet-4-6");
    model.checkin();
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
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
    let mut footprint = footprint.clone();
    if footprint.selection_epoch.is_none() {
        footprint.selection_epoch = model.meter_epoch.selection_epoch;
    }
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
        selection_epoch: Some(3),
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
        head_seq: 3,
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

// Independent Astra review probes; appended to an evidence-only COPY of the candidate tests.
// Assertions express the required behavior, not the candidate's observed implementation.
#[test]
fn astra_stale_same_pair_metadata_cannot_undo_a_committed_budget() {
    let (mut model, mut driver, s) = equal_window_session();
    deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    driver.apply(
        &mut model,
        model_selected_reply(
            &s,
            "eq-oauth",
            "eq-wide",
            Some(budget(20_000, 20_000, false)),
        ),
    );
    assert_eq!(model.context_meter().auto_compact_at, Some(80_000));
    // A list taken BEFORE this selection arrives after the selection reply.
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![{
                let mut row = listed_summary(&s, "eq-oauth", "eq-wide", 50_000);
                row.head_seq = 2;
                row.metadata
                    .as_mut()
                    .expect("probe fixture")
                    .selection_epoch = Some(2);
                row
            }],
            next_cursor: None,
        },
    );
    eprintln!("stale list result: {:?}", model.context_meter());
    assert_eq!(model.context_meter().auto_compact_at, Some(80_000));
}

#[test]
fn astra_late_selection_reply_cannot_roll_back_a_newer_model_fact() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-small");
    deliver_model_fact(&mut driver, &mut model, &s, next + 1, "eq-oauth", "eq-mid");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![{
                let mut row = listed_summary(&s, "eq-oauth", "eq-mid", 50_000);
                row.head_seq = next + 1;
                row
            }],
            next_cursor: None,
        },
    );
    // Delayed reply for the earlier small-model command (two clients can commit serially).
    driver.apply(
        &mut model,
        model_selected_reply(&s, "eq-oauth", "eq-small", Some(budget(8192, 8192, false))),
    );
    eprintln!(
        "late reply: model={} meter={:?}",
        model.identity.model_short,
        model.context_meter()
    );
    assert_eq!(model.identity.model_short, "eq-mid");
    assert_eq!(model.context_meter().auto_compact_at, Some(50_000));
}

#[test]
fn astra_identical_new_snapshot_is_current_by_event_not_by_value() {
    let (mut model, mut driver, s) = equal_window_session();
    let fp = user_set_footprint();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &fp);
    driver.apply(
        &mut model,
        model_selected_reply(
            &s,
            "eq-oauth",
            "eq-mid",
            Some(budget(50_000, 50_000, false)),
        ),
    );
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-mid");
    // The next request has the same usage and limits. Its event/item ID is new.
    let mut new_fp = fp.clone();
    new_fp.selection_epoch = Some(next);
    deliver_footprint(&mut driver, &mut model, &s, next + 1, &new_fp);
    eprintln!("identical fresh snapshot: {:?}", model.context_meter());
    assert!(!model.context_meter().threshold_projected);
    assert_eq!(model.context_meter().turns_to_threshold, Some(0));
}

#[test]
fn astra_account_selection_preserves_the_open_sessions_model() {
    let (mut model, mut driver) = two_attached_sessions();
    let a = session_id("s-meter-a");
    let mut account = oauth_descriptor("anthropic-oauth");
    account.alias = CredentialAlias::new("anthropic-second");
    account.active = false;
    driver.apply(
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("anthropic-oauth"), account.clone()],
            revision: Some(2),
            sources: vec![],
        },
    );
    driver.apply(
        &mut model,
        model_selected_reply(
            &a,
            "anthropic-oauth",
            "claude-sonnet-4-6",
            Some(budget(30_000, 30_000, false)),
        ),
    );
    deliver_footprint(
        &mut driver,
        &mut model,
        &a,
        1,
        &snapshot(
            49_000,
            0,
            1_000,
            Some(200_000),
            ContextFootprintTruth::Exact,
        ),
    );
    model.accounts.pending_select = Some(account.alias.as_str().to_owned());
    account.active = true;
    driver.apply(
        &mut model,
        LiveReply::AccountSelected {
            command_id: haider_rpc::CommandId::new("account-switch"),
            descriptor: account,
            revision: 3,
        },
    );
    eprintln!(
        "account switch: model={} meter={:?}",
        model.identity.model_short,
        model.context_meter()
    );
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    assert_eq!(model.context_meter().window, Some(200_000));
}

#[test]
fn astra_catalog_refresh_preserves_the_resumed_sessions_model() {
    let (mut model, mut driver) = two_attached_sessions();
    let b = session_id("resumed-sonnet");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![{
                let mut row = listed_summary(&b, "anthropic-oauth", "claude-sonnet-4-6", 30_000);
                row.head_seq = 10;
                row
            }],
            next_cursor: None,
        },
    );
    model.open_session(&b);
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    let providers = model.providers.providers.clone();
    driver.apply(
        &mut model,
        LiveReply::Providers {
            providers,
            revision: 3,
        },
    );
    eprintln!(
        "catalog refresh: model={} meter={:?}",
        model.identity.model_short,
        model.context_meter()
    );
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    assert_eq!(model.context_meter().window, Some(200_000));
}

#[test]
fn astra_metadata_after_first_open_can_bind_the_real_session() {
    let (mut model, mut driver) = two_attached_sessions();
    let b = session_id("s-meter-b");
    model
        .sessions
        .iter_mut()
        .find(|row| row.id == b)
        .expect("B row")
        .meter_epoch = Default::default();
    model.open_session(&b); // no selection yet; own metadata must bind it
    assert_eq!(model.identity.model_short, "unknown");
    assert_eq!(model.context_meter().window, None);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![{
                let mut row = listed_summary(&b, "anthropic-oauth", "claude-sonnet-4-6", 30_000);
                row.head_seq = 3;
                row
            }],
            next_cursor: None,
        },
    );
    deliver_footprint(
        &mut driver,
        &mut model,
        &b,
        1,
        &snapshot(
            49_000,
            0,
            1_000,
            Some(200_000),
            ContextFootprintTruth::Exact,
        ),
    );
    eprintln!(
        "metadata after open: model={} meter={:?}",
        model.identity.model_short,
        model.context_meter()
    );
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    assert_eq!(model.context_meter().window, Some(200_000));
}

#[test]
fn astra_metadata_budget_update_invalidates_the_old_snapshot() {
    let (mut model, mut driver, s) = equal_window_session();
    deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    // A newer list learns a committed budget while the journal fact is still catching up.
    let mut row = listed_summary(&s, "eq-oauth", "eq-wide", 20_000);
    row.head_seq = 3;
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    eprintln!("newer budget metadata: {:?}", model.context_meter());
    assert_eq!(model.context_meter().auto_compact_at, Some(80_000));
    assert!(model.context_meter().threshold_projected);
    assert_eq!(model.context_meter().turns_to_threshold, None);
}

#[test]
fn astra_noop_selection_fact_does_not_hide_current_daemon_truth() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    let mut reply = model_selected_reply(
        &s,
        "eq-oauth",
        "eq-wide",
        Some(budget(50_000, 50_000, false)),
    );
    if let LiveReply::ModelSelected { selected_seq, .. } = &mut reply {
        *selected_seq = 0;
    }
    driver.apply(&mut model, reply);
    assert!(!model.context_meter().threshold_projected);
    let fact = haider_protocol::session::ModelSelected {
        selection_epoch: Some(0),
        provider: "eq-oauth".into(),
        model: "eq-wide".into(),
    }
    .to_payload_value()
    .expect("no-op fact");
    deliver(&mut driver, &mut model, &s, next, fact);
    // Fresh metadata confirms this was a no-op, not an unseen budget-only edit.
    let mut row = listed_summary(&s, "eq-oauth", "eq-wide", 50_000);
    row.head_seq = next;
    row.metadata.as_mut().expect("metadata").selection_epoch = Some(0);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    eprintln!(
        "no-op fact after confirming metadata: {:?}",
        model.context_meter()
    );
    assert!(!model.context_meter().threshold_projected);
}

#[test]
fn astra_snapshot_before_the_selection_fact_cannot_revive_old_truth() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    driver.apply(
        &mut model,
        model_selected_reply(&s, "eq-oauth", "eq-small", Some(budget(8192, 8192, false))),
    );
    // An OLD-epoch turn committed before the switch, delivered after its RPC reply.
    let mut older = user_set_footprint();
    older.selection_epoch = Some(0);
    older.used_tokens = 51_000;
    older.input_tokens = 1_000;
    deliver_footprint(&mut driver, &mut model, &s, next, &older);
    eprintln!("late old snapshot: {:?}", model.context_meter());
    assert!(model.context_meter().threshold_projected);
    assert_eq!(model.context_meter().auto_compact_at, Some(85_000));
    assert_eq!(model.context_meter().turns_to_threshold, None);
}

fn astra_child_manifest() -> haider_protocol::agent::AgentManifest {
    use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
    use haider_protocol::ids::{AgentId, LeaseId};
    AgentManifest {
        agent: AgentId::new("astra-kid"),
        role: AgentRole::Subagent,
        task: "child-meter".into(),
        callsign: Some("astra-kid".into()),
        model_profile: "shared-model".into(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new("astra-lease"),
        fencing_epoch: 1,
        attempt: 1,
        parent: None,
        coordinates: Some(
            serde_json::json!({"provider":"q-oauth","child_session_id":"astra-child-session"}),
        ),
        cli_scope: None,
    }
}
fn astra_child_setup() -> (AppModel, LiveDriver) {
    let (mut model, mut driver) = two_attached_sessions();
    let mut providers = model.providers.providers.clone();
    providers.push(summary(
        "q-oauth",
        &[
            ("shared-model", Some(128_000)),
            ("big-model", Some(1_000_000)),
        ],
        "shared-model",
    ));
    driver.apply(
        &mut model,
        LiveReply::Providers {
            providers,
            revision: 3,
        },
    );
    haider_tui::session::apply_agent_payload(
        &mut model.chips,
        &EventPayload::AgentSpawned(astra_child_manifest()),
        0,
    );
    (model, driver)
}
#[test]
fn astra_live_child_meter_reads_its_own_durable_summary_footprint() {
    let (mut model, mut driver) = astra_child_setup();
    let mut row = listed_summary(
        &session_id("astra-child-session"),
        "q-oauth",
        "shared-model",
        16_384,
    );
    row.head_seq = 10;
    row.footprint_tokens = Some(64_000);
    row.footprint_truth = Some(ContextFootprintTruth::Exact);
    // Legacy summaries have only the exact aggregate; no rich footprint may
    // supply the expected value in this probe.
    assert!(row.latest_context_footprint.is_none());
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    let rows = panel_rows(&mut model, 118, 36);
    let child = rows
        .iter()
        .find(|row| row.contains("child-meter"))
        .expect("child row");
    eprintln!("real child summary row: {child}");
    assert!(child.contains("50%  64k/128k"), "{child}");
}
#[test]
fn astra_child_model_change_updates_the_manifest_fallback_window() {
    let (mut model, mut driver) = astra_child_setup();
    let mut row = listed_summary(
        &session_id("astra-child-session"),
        "q-oauth",
        "big-model",
        30_000,
    );
    row.head_seq = 10;
    row.footprint_tokens = Some(64_000);
    row.footprint_truth = Some(ContextFootprintTruth::Exact);
    let mut old_fp = snapshot(
        63_000,
        0,
        1_000,
        Some(128_000),
        ContextFootprintTruth::Exact,
    );
    old_fp.selection_epoch = Some(0);
    row.latest_context_footprint = Some(old_fp);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    let rows = panel_rows(&mut model, 118, 36);
    let child = rows
        .iter()
        .find(|row| row.contains("child-meter"))
        .expect("child row");
    eprintln!("changed child model row: {child}");
    // Isolate identity/window from the separate usage-join assertion; no synthetic ChipTokens feed.
    assert!(
        child.contains("big-model") && child.contains("/1M"),
        "{child}"
    );
    assert_eq!(model.child_display_model(&model.chips[0]), "big-model");
    model.subtree_collapsed = false;
    let parent_view = draw(&model, 118, 36);
    assert!(
        parent_view
            .iter()
            .any(|line| line.contains("child-meter") && line.contains("big-model")),
        "{parent_view:?}"
    );
    model.screen = Screen::Subagent;
    model.view_path = vec!["astra-kid".into()];
    let child_view = draw(&model, 118, 36);
    assert!(
        child_view
            .iter()
            .any(|line| line.contains("astra-kid") && line.contains("big-model")),
        "{child_view:?}"
    );
}

#[test]
fn astra_a_fact_during_an_inflight_list_gets_a_fresh_budget_read() {
    use haider_tui::live::LiveCommand;
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    let fact = |m: &str, epoch: u64| {
        haider_protocol::session::ModelSelected {
            selection_epoch: Some(epoch),
            provider: "eq-oauth".into(),
            model: m.into(),
        }
        .to_payload_value()
        .expect("fact")
    };
    let first = deliver(&mut driver, &mut model, &s, next, fact("eq-small", next));
    assert!(
        first
            .iter()
            .any(|c| matches!(c, LiveCommand::ListAt { .. }))
    );
    // Snapshot for this list is eq-small. A second committed model fact arrives meanwhile.
    let second = deliver(
        &mut driver,
        &mut model,
        &s,
        next + 1,
        fact("eq-mid", next + 1),
    );
    assert!(
        !second
            .iter()
            .any(|c| matches!(c, LiveCommand::ListAt { .. }))
    );
    let follow = driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![{
                let mut row = listed_summary(&s, "eq-oauth", "eq-small", 8192);
                row.head_seq = next;
                row.metadata.as_mut().expect("metadata").selection_epoch = Some(next);
                row
            }],
            next_cursor: None,
        },
    );
    eprintln!(
        "in-flight list result: {:?}; follow={follow:?}",
        model.context_meter()
    );
    assert!(
        follow
            .iter()
            .any(|c| matches!(c, LiveCommand::ListAt { .. })),
        "newer fact's budget refresh was lost"
    );
}

#[test]
fn astra_switching_branches_does_not_make_a_previous_epoch_snapshot_current() {
    use haider_protocol::branch::{BranchCreated, BranchDescriptor};
    use haider_protocol::ids::{BranchId, NodeId};
    let (mut model, mut driver, s) = equal_window_session();
    let fp = user_set_footprint();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &fp);
    let b = BranchId::new("astra-branch");
    let branch = BranchCreated {
        branch: BranchDescriptor {
            branch_id: b.clone(),
            name: "alternate".into(),
            source_branch_id: None,
            fork_node_id: NodeId::new("node-1"),
            fork_seq: 1,
            created_seq: next,
            created_at_ms: 0,
            head_node_id: NodeId::new("node-1"),
            head_seq: 1,
        },
    };
    deliver(
        &mut driver,
        &mut model,
        &s,
        next,
        branch.to_payload_value().expect("branch"),
    );
    assert!(model.switch_branch(Some(&b)).is_some());
    let mut branch_fp = fp.clone();
    branch_fp.selection_epoch = Some(0);
    branch_fp.used_tokens = 51_000;
    branch_fp.input_tokens = 1_000;
    apply_footprint(&mut model, "branch-snapshot", &branch_fp);
    assert!(model.switch_branch(None).is_some());
    driver.apply(
        &mut model,
        model_selected_reply(&s, "eq-oauth", "eq-small", Some(budget(8192, 8192, false))),
    );
    assert!(model.context_meter().threshold_projected);
    assert!(model.switch_branch(Some(&b)).is_some());
    eprintln!(
        "old branch snapshot after model switch: {:?}",
        model.context_meter()
    );
    assert!(model.context_meter().threshold_projected);
    assert_eq!(model.context_meter().auto_compact_at, Some(85_000));
}

#[test]
fn astra_legacy_reply_does_not_erase_an_already_known_committed_budget() {
    let (mut model, mut driver, s) = equal_window_session();
    deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![{
                let mut row = listed_summary(&s, "eq-oauth", "eq-wide", 50_000);
                row.head_seq = 3;
                row
            }],
            next_cursor: None,
        },
    );
    // Older daemon omits the additive output_budget reply field on a no-op reselect.
    driver.apply(
        &mut model,
        model_selected_reply(&s, "eq-oauth", "eq-wide", None),
    );
    eprintln!("legacy no-op reply: {:?}", model.context_meter());
    assert_eq!(model.context_meter().auto_compact_at, Some(50_000));
}

#[test]
fn astra_old_inflight_turn_snapshot_after_budget_fact_is_not_current() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-wide");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![{
                let mut row = listed_summary(&s, "eq-oauth", "eq-wide", 20_000);
                row.head_seq = next;
                row
            }],
            next_cursor: None,
        },
    );
    assert_eq!(model.context_meter().auto_compact_at, Some(80_000));
    // The in-flight request was pinned before the budget change. Its usage settles afterward.
    let mut old = user_set_footprint();
    old.selection_epoch = Some(0);
    old.used_tokens = 51_000;
    old.input_tokens = 1_000;
    deliver_footprint(&mut driver, &mut model, &s, next + 1, &old);
    eprintln!(
        "old running turn settled after budget fact: {:?}",
        model.context_meter()
    );
    assert!(model.context_meter().threshold_projected);
    assert_eq!(model.context_meter().auto_compact_at, Some(80_000));
    assert_eq!(model.context_meter().turns_to_threshold, None);
}
#[test]
fn astra_failed_metadata_read_releases_the_refresh_latch() {
    use haider_tui::live::LiveCommand;
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    let fact = |m: &str, epoch: u64| {
        haider_protocol::session::ModelSelected {
            selection_epoch: Some(epoch),
            provider: "eq-oauth".into(),
            model: m.into(),
        }
        .to_payload_value()
        .expect("fact")
    };
    let first = deliver(&mut driver, &mut model, &s, next, fact("eq-small", next));
    assert!(
        first
            .iter()
            .any(|c| matches!(c, LiveCommand::ListAt { .. }))
    );
    let list_epoch = driver.connection_epoch();
    driver.apply(&mut model, LiveReply::ListFailedAt { epoch: list_epoch });
    let second = deliver(
        &mut driver,
        &mut model,
        &s,
        next + 1,
        fact("eq-mid", next + 1),
    );
    eprintln!("refresh after failed list: {second:?}");
    assert!(
        second
            .iter()
            .any(|c| matches!(c, LiveCommand::ListAt { .. }))
    );
}

#[test]
fn newer_selection_dirty_during_a_failed_list_gets_one_followup() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-small");
    deliver_model_fact(&mut driver, &mut model, &s, next + 1, "eq-oauth", "eq-mid");
    let list_epoch = driver.connection_epoch();
    let follow = driver.apply(&mut model, LiveReply::ListFailedAt { epoch: list_epoch });
    assert_eq!(
        follow
            .iter()
            .filter(|command| matches!(command, LiveCommand::ListAt { cursor: None, .. }))
            .count(),
        1
    );
    assert!(
        driver
            .apply(&mut model, LiveReply::ListFailedAt { epoch: list_epoch })
            .is_empty()
    );
    let later = deliver(
        &mut driver,
        &mut model,
        &s,
        next + 2,
        haider_protocol::session::ModelSelected {
            selection_epoch: Some(next + 2),
            provider: "eq-oauth".into(),
            model: "eq-small".into(),
        }
        .to_payload_value()
        .expect("fact"),
    );
    assert!(
        later
            .iter()
            .any(|command| matches!(command, LiveCommand::ListAt { .. }))
    );
}

#[test]
fn astra_compaction_preannounce_does_not_reuse_the_previous_models_window() {
    let (mut model, mut driver) = two_attached_sessions();
    let s = session_id("s-meter-a");
    deliver_footprint(
        &mut driver,
        &mut model,
        &s,
        1,
        &snapshot(
            99_000,
            0,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    driver.apply(
        &mut model,
        model_selected_reply(
            &s,
            "anthropic-oauth",
            "claude-sonnet-4-6",
            Some(budget(30_000, 30_000, false)),
        ),
    );
    let item = TurnItem::Extension {
        kind: haider_protocol::history::COMPACTION_INTENT_EXTENSION_KIND.into(),
        data: serde_json::json!({"resume":"auto_mid_turn"}),
    };
    deliver_model_fact(
        &mut driver,
        &mut model,
        &s,
        3,
        "anthropic-oauth",
        "claude-sonnet-4-6",
    );
    deliver(
        &mut driver,
        &mut model,
        &s,
        4,
        serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("astra-compact-intent"),
            item,
        }))
        .expect("intent payload"),
    );
    let rows = draw(&model, 118, 36);
    let note = rows
        .iter()
        .find(|row| row.contains("compacting"))
        .expect("compaction note");
    eprintln!("preannounce after switch: {note}");
    assert!(
        !note.contains("context at 10%"),
        "previous model's window leaked into compaction announcement"
    );
    let recorded_note = model
        .projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            haider_tui::projection::TranscriptEntry::Note { text }
                if text.contains("compacting") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .expect("recorded compaction note");
    let mut later = model_selected_reply(
        &s,
        "anthropic-oauth",
        "claude-opus-5-5",
        Some(budget(30_000, 30_000, false)),
    );
    if let LiveReply::ModelSelected { selected_seq, .. } = &mut later {
        *selected_seq = 5;
    }
    driver.apply(&mut model, later);
    assert_eq!(model.meter_epoch.selection_epoch, Some(5));
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        haider_tui::projection::TranscriptEntry::Note { text } if text == &recorded_note
    )));
}
#[test]
fn astra_provider_rebind_and_parked_rebind_use_own_pair() {
    let (mut model, mut driver) = two_attached_sessions();
    let a = session_id("s-meter-a");
    let b = session_id("s-meter-b");
    let mut providers = model.providers.providers.clone();
    providers.push(summary(
        "rebound",
        &[("claude-opus-5-5", Some(200_000))],
        "claude-opus-5-5",
    ));
    driver.apply(
        &mut model,
        LiveReply::Providers {
            providers,
            revision: 3,
        },
    );
    let fp = snapshot(
        49_000,
        0,
        1_000,
        Some(1_000_000),
        ContextFootprintTruth::Exact,
    );
    deliver_footprint(&mut driver, &mut model, &a, 1, &fp);
    model.open_session(&b);
    model.open_session(&a); // B has a bound own pair before its background rebind
    let rebound = haider_protocol::session::SessionProviderRebound {
        selection_epoch: Some(3),
        rebind_id: "r1".into(),
        provider: "rebound".into(),
        base_url: None,
        account: None,
    }
    .to_payload_value()
    .expect("rebind");
    deliver(&mut driver, &mut model, &a, 3, rebound.clone());
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![{
                let mut row = listed_summary(&a, "rebound", "claude-opus-5-5", 60_000);
                row.head_seq = 3;
                row
            }],
            next_cursor: None,
        },
    );
    assert_eq!(model.context_meter().window, Some(200_000));
    assert_eq!(model.context_meter().auto_compact_at, Some(140_000));
    assert!(model.context_meter().threshold_projected);
    let mut rebound_b = rebound;
    rebound_b["selection_epoch"] = serde_json::json!(1);
    deliver(&mut driver, &mut model, &b, 1, rebound_b);
    assert_eq!(model.identity.provider, "rebound");
    model.open_session(&b);
    assert_eq!(model.identity.provider, "rebound");
    assert_eq!(model.identity.model_short, "claude-opus-5-5");
    assert_eq!(model.context_meter().window, Some(200_000));
}
#[test]
fn astra_duplicate_and_gapped_model_facts_do_not_mutate_identity() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-mid");
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-small");
    assert_eq!(model.identity.model_short, "eq-mid");
    deliver_model_fact(
        &mut driver,
        &mut model,
        &s,
        next + 2,
        "eq-oauth",
        "eq-small",
    );
    assert_eq!(model.identity.model_short, "eq-mid");
}

// Esc navigates away from Aura/Subagent/Loom. Put a real slash draft into
// that surface's composer and let its Enter dispatch consume it.
fn selection_slash(model: &mut AppModel, line: &str) {
    model.composer.set_text(line);
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Enter));
}

#[test]
fn astra_model_pick_from_subagent_surface_cannot_locally_rebind_parent_meter() {
    let (mut model, _driver) = astra_child_setup();
    let parent_meter = model.context_meter();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    model.screen = Screen::Subagent;
    model.view_path = vec!["astra-kid".into()];
    model.requests.clear();
    model.open_model_picker("big-model".into());
    let row = model
        .model_picker_rows()
        .into_iter()
        .find(|row| row.model == "big-model" && row.provider == "q-oauth")
        .expect("child model row");
    model.select_model_row(&row);
    eprintln!(
        "child-surface pick: parent model={} requests={:?}",
        model.identity.model_short, model.requests
    );
    assert!(model.requests.iter().any(|request| matches!(request,
        haider_tui::app::AppRequest::SelectModel { session, provider, model, .. }
        if session.as_str()=="astra-child-session" && provider=="q-oauth" && model=="big-model")));
    assert_eq!(model.identity.model_short, "claude-opus-5-5");
    assert_eq!(model.context_meter(), parent_meter);
}

#[test]
fn child_selection_waits_for_its_own_control_attachment() {
    use haider_tui::live::LiveCommand;
    let (mut model, mut driver) = astra_child_setup();
    let child = session_id("astra-child-session");
    model.upsert_live_session(&child);
    let issued = driver.handle_request(
        &mut model,
        haider_tui::app::AppRequest::SelectModel {
            session: child.clone(),
            provider: "q-oauth".into(),
            model: "big-model".into(),
            confirm_new_epoch: false,
        },
    );
    assert!(issued.iter().any(|command| matches!(command,
        LiveCommand::Attach { session, .. } if session == &child)));
    assert!(
        !issued
            .iter()
            .any(|command| matches!(command, LiveCommand::SelectModel { .. }))
    );
    let attached = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: child.clone(),
            attachment: attachment_of(&child),
            worker_generation: 7,
            replay_through_seq: 0,
            launch_origin: None,
        },
    );
    assert!(attached.iter().any(|command| matches!(command,
        LiveCommand::SelectModel { session, worker_generation: 7, .. }
        if session == &child)));
}

#[test]
fn child_selection_reattaches_before_replay_after_disconnect() {
    use haider_tui::live::LiveCommand;
    let (mut model, mut driver) = astra_child_setup();
    let child = session_id("astra-child-session");
    model.upsert_live_session(&child);
    let issued = driver.handle_request(
        &mut model,
        haider_tui::app::AppRequest::SelectModel {
            session: child.clone(),
            provider: "q-oauth".into(),
            model: "big-model".into(),
            confirm_new_epoch: false,
        },
    );
    assert!(issued.iter().any(|command| matches!(command,
        LiveCommand::Attach { session, .. } if session == &child)));
    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "before child attach".into(),
        },
    );
    let resumed = driver.apply(&mut model, LiveReply::Reconnected);
    assert!(resumed.iter().any(|command| matches!(command,
        LiveCommand::Attach { session, .. } if session == &child)));
    assert!(
        !resumed
            .iter()
            .any(|command| matches!(command, LiveCommand::SelectModel { .. }))
    );
    let attached = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: child.clone(),
            attachment: attachment_of(&child),
            worker_generation: 9,
            replay_through_seq: 0,
            launch_origin: None,
        },
    );
    assert!(attached.iter().any(|command| matches!(command,
        LiveCommand::SelectModel { session, worker_generation: 9, connection_epoch: 1, .. }
        if session == &child)));
}

#[test]
fn astra_provider_pick_from_aura_cannot_locally_rebind_parent_meter() {
    let (mut model, _driver) = two_attached_sessions();
    let parent_meter = model.context_meter();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    model.screen = Screen::Aura;
    model.requests.clear();
    selection_slash(&mut model, "/provider openai-oauth");
    eprintln!(
        "aura provider pick: parent provider={} requests={:?}",
        model.identity.provider, model.requests
    );
    assert!(model.requests.iter().any(|request| matches!(request,
        haider_tui::app::AppRequest::SelectModel { session, provider, .. }
        if session.as_str()=="s-meter-a" && provider=="openai-oauth")));
    assert_eq!(model.identity.provider, "anthropic-oauth");
    assert_eq!(model.context_meter(), parent_meter);
}

#[test]
fn subagent_provider_command_targets_child_session() {
    let (mut model, _driver) = astra_child_setup();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    model.screen = Screen::Subagent;
    model.view_path = vec!["astra-kid".into()];
    model.requests.clear();
    selection_slash(&mut model, "/provider q-oauth");
    assert!(model.requests.iter().any(|request| matches!(request,
        haider_tui::app::AppRequest::SelectModel { session, provider, .. }
        if session.as_str()=="astra-child-session" && provider=="q-oauth")));
}

#[test]
fn aura_model_picker_targets_attached_session() {
    let (mut model, _driver) = two_attached_sessions();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    model.screen = Screen::Aura;
    model.requests.clear();
    model.open_model_picker("gpt-6-sol".into());
    let row = model
        .model_picker_rows()
        .into_iter()
        .find(|row| row.model == "gpt-6-sol" && row.provider == "openai-oauth")
        .expect("Aura model row");
    model.select_model_row(&row);
    assert!(model.requests.iter().any(|request| matches!(request,
        haider_tui::app::AppRequest::SelectModel { session, provider, model, .. }
        if session.as_str()=="s-meter-a" && provider=="openai-oauth" && model=="gpt-6-sol")));
}

#[test]
fn nested_subagent_selection_targets_nested_session() {
    use haider_protocol::ids::AgentId;
    let (mut model, mut driver) = astra_child_setup();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    let mut nested = astra_child_manifest();
    nested.agent = AgentId::new("nested-kid");
    nested.coordinates =
        Some(serde_json::json!({"provider":"q-oauth","child_session_id":"nested-session"}));
    model.chips[0]
        .children
        .push(haider_tui::app::ChipModel::from_manifest(&nested));
    model.screen = Screen::Subagent;
    model.view_path = vec!["astra-kid".into(), "nested-kid".into()];
    model.requests.clear();
    selection_slash(&mut model, "/provider q-oauth");
    assert!(model.requests.iter().any(|request| matches!(request,
        haider_tui::app::AppRequest::SelectModel { session, .. }
        if session.as_str()=="nested-session")));
    let mut row = listed_summary(
        &session_id("nested-session"),
        "q-oauth",
        "shared-model",
        16_384,
    );
    row.head_seq = 10;
    row.footprint_tokens = Some(64_000);
    row.footprint_truth = Some(ContextFootprintTruth::Exact);
    let mut footprint = snapshot(
        63_000,
        0,
        1_000,
        Some(128_000),
        ContextFootprintTruth::Exact,
    );
    footprint.selection_epoch = Some(3);
    row.latest_context_footprint = Some(footprint);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    let status = status_left_string(&model, 118);
    assert!(
        status.contains("64k tok") && status.contains("50% of 128k"),
        "{status}"
    );
}

#[test]
fn astra_same_provider_endpoint_rebind_does_not_keep_the_old_routes_snapshot_window() {
    let (mut model, mut driver) = two_attached_sessions();
    let s = session_id("s-meter-a");
    // Advisory custom model absent from the catalog: only the old request knew its window.
    let mut reply = model_selected_reply(
        &s,
        "custom-proxy",
        "opaque-model",
        Some(budget(30_000, 30_000, false)),
    );
    if let LiveReply::ModelSelected { selected_seq, .. } = &mut reply {
        *selected_seq = 1;
    }
    driver.apply(&mut model, reply);
    deliver_model_fact(
        &mut driver,
        &mut model,
        &s,
        1,
        "custom-proxy",
        "opaque-model",
    );
    let next = deliver_footprint(
        &mut driver,
        &mut model,
        &s,
        2,
        &snapshot(
            49_000,
            0,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    assert_eq!(model.context_meter().window, Some(1_000_000));
    let rebound = haider_protocol::session::SessionProviderRebound {
        selection_epoch: Some(next),
        rebind_id: "new-route".into(),
        provider: "custom-proxy".into(),
        base_url: Some("http://127.0.0.1:31702/v1".into()),
        account: None,
    }
    .to_payload_value()
    .expect("rebind");
    deliver(&mut driver, &mut model, &s, next, rebound);
    eprintln!("same-provider new endpoint: {:?}", model.context_meter());
    assert_eq!(
        model.context_meter().window,
        None,
        "the old endpoint's unknown-model window is not proof for the new endpoint"
    );
    assert_eq!(model.context_meter().turns_to_threshold, None);
}

#[test]
fn astra_current_footprint_can_supply_a_window_omitted_by_legacy_catalog() {
    let (mut model, mut driver) = two_attached_sessions();
    let s = session_id("s-meter-a");
    let mut providers = model.providers.providers.clone();
    for provider in &mut providers {
        for detail in &mut provider.model_details {
            detail.context_window = None;
        }
    }
    driver.apply(
        &mut model,
        LiveReply::Providers {
            providers,
            revision: 3,
        },
    );
    // No model/route/budget switch: this new request belongs to the currently attached model.
    deliver_footprint(
        &mut driver,
        &mut model,
        &s,
        1,
        &snapshot(
            49_000,
            0,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    eprintln!(
        "current snapshot with omitted catalog window: {:?}",
        model.context_meter()
    );
    assert_eq!(model.context_meter().window, Some(1_000_000));
    assert_eq!(model.context_meter().percent(), Some(5));
}

#[test]
fn astra_child_view_status_and_mirror_use_the_viewed_childs_meter() {
    let (mut model, mut driver) = astra_child_setup();
    let s = session_id("s-meter-a");
    deliver_footprint(
        &mut driver,
        &mut model,
        &s,
        1,
        &snapshot(
            49_000,
            0,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    let child = snapshot(
        63_000,
        0,
        1_000,
        Some(128_000),
        ContextFootprintTruth::Exact,
    );
    let mut row = listed_summary(
        &session_id("astra-child-session"),
        "q-oauth",
        "shared-model",
        16_384,
    );
    row.head_seq = 10;
    // The child view must read its transcript when no summary usage exists.
    assert!(row.latest_context_footprint.is_none());
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    model.chips[0]
        .transcript
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("child-own-current-footprint"),
            item: child.extension_item().expect("child footprint"),
        }));
    model.subtree_collapsed = false;
    model.handle_hit(haider_tui::app::Hit::ChipRow("astra-kid".into()));
    assert_eq!(model.screen, Screen::Subagent);
    let line = status_left_string(&model, 118);
    eprintln!("viewed child's status/mirror: {line}");
    assert!(
        line.contains("64k tok") && line.contains("50% of 128k"),
        "{line}"
    );
}

#[test]
fn astra_missing_current_budget_does_not_invent_a_default_reserve_for_a_user_budget() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    // The other client selected eq-mid; its 60k output maximum preserves the user's 50k reserve.
    // The fact omits the budget and its list reply is still pending (or omitted by an older daemon).
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-mid");
    let meter = model.context_meter();
    eprintln!("pending current budget metadata: {meter:?}");
    assert!(
        meter.auto_compact_at.is_none() || meter.auto_compact_at == Some(50_000),
        "a 70k trigger assumes the model default, not this session's committed 50k reserve"
    );
}

#[test]
fn older_connection_selection_reply_cannot_rebind_current_session() {
    let (mut model, mut driver, s) = equal_window_session();
    let mut reply = model_selected_reply(
        &s,
        "eq-oauth",
        "eq-small",
        Some(budget(8_192, 8_192, false)),
    );
    if let LiveReply::ModelSelected {
        worker_generation,
        selected_seq,
        ..
    } = &mut reply
    {
        *worker_generation = 7;
        *selected_seq = 7;
    }
    if let LiveReply::ModelSelected {
        connection_epoch, ..
    } = &mut reply
    {
        *connection_epoch = Some(0);
    }
    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "old socket".into(),
        },
    );
    driver.apply(&mut model, LiveReply::Reconnected);
    driver.apply(&mut model, reply);
    assert_eq!(model.identity.model_short, "eq-wide");
    assert_eq!(model.meter_epoch.selection_epoch, Some(0));
}

#[test]
fn explicit_reconnect_fences_an_old_selection_reply_without_disconnect_notification() {
    let (mut model, mut driver, s) = equal_window_session();
    let old_connection = driver.connection_epoch();
    let commands = driver.handle_request(&mut model, haider_tui::app::AppRequest::Reconnect);
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Reconnect))
    );
    driver.apply(&mut model, LiveReply::Reconnected);
    let mut reply = model_selected_reply(
        &s,
        "eq-oauth",
        "eq-small",
        Some(budget(8_192, 8_192, false)),
    );
    if let LiveReply::ModelSelected {
        connection_epoch,
        selected_seq,
        ..
    } = &mut reply
    {
        *connection_epoch = Some(old_connection);
        *selected_seq = 5;
    }
    driver.apply(&mut model, reply);
    assert_eq!(model.identity.model_short, "eq-wide");
    assert_eq!(model.meter_epoch.selection_epoch, Some(0));
}

#[test]
fn older_same_pair_budget_reply_cannot_undo_new_budget() {
    let (mut model, mut driver, s) = equal_window_session();
    deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    let mut latest = model_selected_reply(
        &s,
        "eq-oauth",
        "eq-wide",
        Some(budget(20_000, 20_000, false)),
    );
    if let LiveReply::ModelSelected { selected_seq, .. } = &mut latest {
        *selected_seq = 4;
    }
    driver.apply(&mut model, latest);
    driver.apply(
        &mut model,
        model_selected_reply(
            &s,
            "eq-oauth",
            "eq-wide",
            Some(budget(50_000, 50_000, false)),
        ),
    );
    assert_eq!(model.meter_epoch.selection_epoch, Some(4));
    assert_eq!(model.context_meter().auto_compact_at, Some(80_000));
}

#[test]
fn older_daemon_footprint_without_epoch_never_claims_current_trigger() {
    let (mut model, _driver, s) = equal_window_session();
    assert_eq!(model.active_session.as_ref(), Some(&s));
    let item = user_set_footprint().extension_item().expect("old carrier");
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("legacy-epochless"),
            item,
        }));
    assert!(model.context_meter().threshold_projected);
    assert_eq!(model.context_meter().turns_to_threshold, None);
}

#[test]
fn estimated_child_summary_retains_approximation_and_missing_child_is_unknown() {
    let (mut model, mut driver) = astra_child_setup();
    let before = panel_rows(&mut model, 118, 36);
    assert!(
        before
            .iter()
            .any(|row| row.contains("child-meter") && row.contains("usage unknown"))
    );
    let mut row = listed_summary(
        &session_id("astra-child-session"),
        "q-oauth",
        "shared-model",
        16_384,
    );
    row.head_seq = 10;
    row.footprint_tokens = Some(64_000);
    row.footprint_truth = Some(ContextFootprintTruth::Estimated);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    let after = panel_rows(&mut model, 118, 36);
    assert!(
        after
            .iter()
            .any(|row| row.contains("child-meter") && row.contains("~64k/128k")),
        "{after:#?}"
    );
}

#[test]
fn child_summary_without_selection_does_not_keep_spawn_manifest_label() {
    let (mut model, mut driver) = astra_child_setup();
    let mut row = listed_summary(
        &session_id("astra-child-session"),
        "q-oauth",
        "shared-model",
        16_384,
    );
    row.head_seq = 10;
    row.metadata = None;
    row.provider = None;
    row.last_model = None;
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    let rows = panel_rows(&mut model, 118, 36);
    let child = rows
        .iter()
        .find(|row| row.contains("child-meter"))
        .expect("child row");
    assert!(
        child.contains("unknown") && !child.contains("shared-model"),
        "{child}"
    );
    model.screen = Screen::Subagent;
    model.view_path = vec!["astra-kid".into()];
    assert_eq!(
        model.surface_composer_identity(80).as_deref(),
        Some("unknown")
    );
}

#[test]
fn older_daemon_selection_fact_and_metadata_bind_pair_without_claiming_snapshot_truth() {
    let (mut model, mut driver, s) = equal_window_session();
    model.meter_epoch = Default::default();
    let mut row = listed_summary(&s, "eq-oauth", "eq-small", 8_192);
    row.metadata
        .as_mut()
        .expect("typed metadata")
        .selection_epoch = None;
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    assert_eq!(model.identity.model_short, "eq-small");
    let mut legacy = user_set_footprint();
    legacy.selection_epoch = None;
    deliver_footprint(&mut driver, &mut model, &s, 1, &legacy);
    assert!(model.context_meter().threshold_projected);
    assert_eq!(model.context_meter().turns_to_threshold, None);
    deliver(
        &mut driver,
        &mut model,
        &s,
        3,
        haider_protocol::session::ModelSelected {
            selection_epoch: None,
            provider: "eq-oauth".into(),
            model: "eq-mid".into(),
        }
        .to_payload_value()
        .expect("legacy fact"),
    );
    assert_eq!(model.identity.model_short, "eq-mid");
    assert_eq!(model.meter_epoch.selection_epoch, Some(3));
    assert!(model.context_meter().threshold_projected);
}

#[test]
fn epochless_durable_fact_after_a_selection_reply_still_advances_its_pair() {
    let (mut model, mut driver, session) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &session, 1, &user_set_footprint());
    driver.apply(
        &mut model,
        model_selected_reply(
            &session,
            "eq-oauth",
            "eq-small",
            Some(budget(8_192, 8_192, false)),
        ),
    );
    deliver_model_fact(
        &mut driver,
        &mut model,
        &session,
        next,
        "eq-oauth",
        "eq-small",
    );
    deliver(
        &mut driver,
        &mut model,
        &session,
        next + 1,
        haider_protocol::session::ModelSelected {
            selection_epoch: None,
            provider: "eq-oauth".into(),
            model: "eq-mid".into(),
        }
        .to_payload_value()
        .expect("legacy fact"),
    );
    assert_eq!(model.identity.model_short, "eq-mid");
    assert_eq!(model.meter_epoch.selection_epoch, Some(next + 1));
    assert!(model.context_meter().threshold_projected);
}

#[test]
fn old_connection_list_cannot_install_even_a_numerically_newer_selection() {
    let (mut model, mut driver, s) = equal_window_session();
    let mut row = listed_summary(&s, "eq-oauth", "eq-small", 8_192);
    row.head_seq = 99;
    row.metadata.as_mut().expect("metadata").selection_epoch = Some(99);
    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "old socket".into(),
        },
    );
    driver.apply(&mut model, LiveReply::Reconnected);
    driver.apply(
        &mut model,
        LiveReply::ListedAt {
            sessions: vec![row],
            next_cursor: None,
            epoch: 0,
        },
    );
    assert_eq!(model.identity.model_short, "eq-wide");
    assert_eq!(model.meter_epoch.selection_epoch, Some(0));
}

#[test]
fn old_connection_catalog_and_account_reads_cannot_change_attached_window_or_launcher() {
    let (mut model, mut driver, _s) = equal_window_session();
    let old_connection = driver.connection_epoch();
    let current_window = model.identity.context_window;
    let current_launcher = model.launcher_identity.clone();
    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "catalog redial".into(),
        },
    );
    driver.apply(&mut model, LiveReply::Reconnected);
    let mut stale_provider = summary("eq-oauth", &[("eq-wide", Some(2_000_000))], "eq-wide");
    stale_provider.model_details[0].max_output_tokens = Some(100_000);
    driver.apply(
        &mut model,
        LiveReply::ProvidersAt {
            providers: vec![stale_provider.clone()],
            revision: 99,
            epoch: old_connection,
        },
    );
    driver.apply(
        &mut model,
        LiveReply::ProviderModelsRefreshedAt {
            provider: stale_provider,
            revision: 99,
            epoch: old_connection,
        },
    );
    driver.apply(
        &mut model,
        LiveReply::AccountsAt {
            descriptors: vec![oauth_descriptor("openai-oauth")],
            revision: Some(99),
            sources: Vec::new(),
            epoch: old_connection,
        },
    );
    assert_eq!(model.identity.context_window, current_window);
    assert_eq!(model.launcher_identity.provider, current_launcher.provider);
    assert_eq!(
        model.launcher_identity.model_short,
        current_launcher.model_short
    );
}

#[test]
fn legacy_summary_pair_is_projected_from_that_sessions_own_fields() {
    let (mut model, mut driver) = two_attached_sessions();
    let b = session_id("s-meter-b");
    model
        .sessions
        .iter_mut()
        .find(|row| row.id == b)
        .expect("B")
        .meter_epoch = Default::default();
    model.open_session(&b);
    let mut row = listed_summary(&b, "anthropic-oauth", "claude-sonnet-4-6", 30_000);
    row.metadata = None;
    row.provider = Some("anthropic-oauth".into());
    row.last_model = Some("claude-sonnet-4-6".into());
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    assert_eq!(model.meter_epoch.selection_epoch, None);
    assert_eq!(model.context_meter().window, Some(200_000));
    assert!(model.context_meter().threshold_projected);
}

#[test]
fn late_accounts_and_model_catalog_refresh_do_not_replace_attached_pair() {
    let (mut model, mut driver, s) = equal_window_session();
    driver.apply(
        &mut model,
        model_selected_reply(
            &s,
            "eq-oauth",
            "eq-small",
            Some(budget(8_192, 8_192, false)),
        ),
    );
    let original = model.identity.model_short.clone();
    driver.apply(
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("eq-oauth")],
            revision: Some(8),
            sources: vec![],
        },
    );
    let catalog = model
        .providers
        .providers
        .iter()
        .find(|provider| provider.provider == "eq-oauth")
        .expect("catalog")
        .clone();
    driver.apply(
        &mut model,
        LiveReply::ProviderModelsRefreshed {
            provider: catalog,
            revision: 9,
        },
    );
    assert_eq!(model.identity.model_short, original);
    assert_eq!(
        model.meter_epoch.pair.as_ref().expect("probe fixture").1,
        original
    );
}

#[test]
fn detached_session_newer_metadata_replaces_its_old_pair() {
    let (mut model, mut driver) = two_attached_sessions();
    let b = session_id("s-meter-b");
    model.open_session(&b);
    model.open_session(&session_id("s-meter-a"));
    let mut row = listed_summary(&b, "anthropic-oauth", "claude-sonnet-4-6", 30_000);
    row.head_seq = 9;
    row.metadata
        .as_mut()
        .expect("probe fixture")
        .selection_epoch = Some(9);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    model.open_session(&b);
    assert_eq!(model.identity.model_short, "claude-sonnet-4-6");
    assert_eq!(model.meter_epoch.selection_epoch, Some(9));
    assert_eq!(model.context_meter().window, Some(200_000));
}

#[test]
fn a_to_b_to_a_rejects_old_same_pair_metadata_by_epoch() {
    let (mut model, mut driver, s) = equal_window_session();
    let mut b = model_selected_reply(
        &s,
        "eq-oauth",
        "eq-small",
        Some(budget(8_192, 8_192, false)),
    );
    if let LiveReply::ModelSelected { selected_seq, .. } = &mut b {
        *selected_seq = 3;
    }
    driver.apply(&mut model, b);
    let mut a = model_selected_reply(
        &s,
        "eq-oauth",
        "eq-wide",
        Some(budget(20_000, 20_000, false)),
    );
    if let LiveReply::ModelSelected { selected_seq, .. } = &mut a {
        *selected_seq = 4;
    }
    driver.apply(&mut model, a);
    let mut stale = listed_summary(&s, "eq-oauth", "eq-wide", 50_000);
    stale.head_seq = 2;
    stale
        .metadata
        .as_mut()
        .expect("probe fixture")
        .selection_epoch = Some(0);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![stale],
            next_cursor: None,
        },
    );
    assert_eq!(model.meter_epoch.selection_epoch, Some(4));
    assert_eq!(model.meter_epoch.output_budget, Some(20_000));
}

#[test]
fn parked_budget_metadata_invalidates_its_old_snapshot_before_checkout() {
    let (mut model, mut driver) = two_attached_sessions();
    let b = session_id("s-meter-b");
    deliver_footprint(
        &mut driver,
        &mut model,
        &b,
        1,
        &snapshot(
            49_000,
            0,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    let mut row = listed_summary(&b, "anthropic-oauth", "claude-sonnet-4-6", 60_000);
    row.head_seq = 3;
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![row],
            next_cursor: None,
        },
    );
    model.open_session(&b);
    assert_eq!(model.context_meter().auto_compact_at, Some(140_000));
    assert!(model.context_meter().threshold_projected);
    assert_eq!(model.context_meter().turns_to_threshold, None);
}

#[test]
fn cross_session_fact_during_paginated_list_gets_one_dirty_followup() {
    use haider_tui::live::LiveCommand;
    let (mut model, mut driver) = two_attached_sessions();
    let a = session_id("s-meter-a");
    let b = session_id("s-meter-b");
    let first = deliver(
        &mut driver,
        &mut model,
        &a,
        1,
        haider_protocol::session::ModelSelected {
            selection_epoch: Some(1),
            provider: "anthropic-oauth".into(),
            model: "claude-sonnet-4-6".into(),
        }
        .to_payload_value()
        .expect("probe fixture"),
    );
    assert_eq!(
        first
            .iter()
            .filter(|command| matches!(command, LiveCommand::ListAt { .. }))
            .count(),
        1
    );
    let page = driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![],
            next_cursor: Some("next".into()),
        },
    );
    assert_eq!(
        page.iter()
            .filter(|command| matches!(command, LiveCommand::ListAt { .. }))
            .count(),
        1
    );
    let second = deliver(
        &mut driver,
        &mut model,
        &b,
        1,
        haider_protocol::session::ModelSelected {
            selection_epoch: Some(1),
            provider: "anthropic-oauth".into(),
            model: "claude-sonnet-4-6".into(),
        }
        .to_payload_value()
        .expect("probe fixture"),
    );
    assert!(
        !second
            .iter()
            .any(|command| matches!(command, LiveCommand::ListAt { .. }))
    );
    let follow = driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![],
            next_cursor: None,
        },
    );
    assert_eq!(
        follow
            .iter()
            .filter(|command| matches!(command, LiveCommand::ListAt { cursor: None, .. }))
            .count(),
        1
    );
}

#[test]
fn parked_same_provider_route_rebind_discards_old_route_window() {
    let (mut model, mut driver) = two_attached_sessions();
    let b = session_id("s-meter-b");
    model.note_session_metadata_at(&b, "custom-proxy", "opaque-model", 30_000, Some(0), None);
    let next = deliver_footprint(
        &mut driver,
        &mut model,
        &b,
        1,
        &snapshot(
            49_000,
            0,
            1_000,
            Some(1_000_000),
            ContextFootprintTruth::Exact,
        ),
    );
    let rebound = haider_protocol::session::SessionProviderRebound {
        selection_epoch: Some(next),
        rebind_id: "route-b".into(),
        provider: "custom-proxy".into(),
        base_url: Some("http://127.0.0.1:31703/v1".into()),
        account: None,
    }
    .to_payload_value()
    .expect("probe fixture");
    deliver(&mut driver, &mut model, &b, next, rebound);
    model.open_session(&b);
    assert_eq!(model.meter_epoch.selection_epoch, Some(next));
    assert_eq!(model.context_meter().window, None);
    assert_eq!(model.context_meter().turns_to_threshold, None);
}

#[test]
fn late_estimated_compaction_snapshot_keeps_tilde_but_never_old_trigger() {
    let (mut model, mut driver, s) = equal_window_session();
    let mut old = user_set_footprint();
    old.truth = ContextFootprintTruth::Estimated;
    deliver_footprint(&mut driver, &mut model, &s, 1, &old);
    driver.apply(
        &mut model,
        model_selected_reply(
            &s,
            "eq-oauth",
            "eq-small",
            Some(budget(8_192, 8_192, false)),
        ),
    );
    old.selection_epoch = Some(0);
    old.used_tokens = 51_000;
    deliver_footprint(&mut driver, &mut model, &s, 3, &old);
    let meter = model.context_meter();
    assert!(meter.estimated);
    assert!(meter.threshold_projected);
    assert_eq!(meter.auto_compact_at, Some(85_000));
    assert_eq!(meter.turns_to_threshold, None);
}

#[test]
fn proven_current_request_is_truth_even_before_clamped_budget_metadata_arrives() {
    let (mut model, mut driver, s) = equal_window_session();
    let next = deliver_footprint(&mut driver, &mut model, &s, 1, &user_set_footprint());
    deliver_model_fact(&mut driver, &mut model, &s, next, "eq-oauth", "eq-small");
    let mut current = user_set_footprint();
    current.selection_epoch = Some(next);
    current.reserved_output_tokens = 8_192;
    current.soft_threshold_tokens = Some(85_000);
    current.estimated_turns_to_threshold = Some(6);
    deliver_footprint(&mut driver, &mut model, &s, next + 1, &current);
    let meter = model.context_meter();
    assert!(!meter.threshold_projected);
    assert_eq!(meter.auto_compact_at, Some(85_000));
    assert_eq!(meter.turns_to_threshold, Some(6));
}

#[test]
fn cold_replay_old_request_after_budget_commit_projects_until_new_request() {
    for metadata_first in [true, false] {
        let (mut model, mut driver) = two_attached_sessions();
        let s = session_id("s-meter-a");
        let admit = |model: &mut AppModel| {
            model.note_session_metadata_at(
                &s,
                "anthropic-oauth",
                "claude-sonnet-4-6",
                30_000,
                Some(2),
                Some(
                    haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                        requested: 30_000,
                    },
                ),
            )
        };
        if metadata_first {
            admit(&mut model);
        }
        let mut old = snapshot(
            2_000,
            126_000,
            2_000,
            Some(200_000),
            ContextFootprintTruth::Exact,
        );
        old.selection_epoch = Some(0);
        old.reserved_output_tokens = 60_000;
        old.soft_threshold_tokens = Some(140_000);
        old.estimated_turns_to_threshold = Some(5);
        let item = old.extension_item().expect("old request carrier");
        let id = ItemId::new("cold-old-request");
        deliver(
            &mut driver,
            &mut model,
            &s,
            1,
            serde_json::to_value(EventPayload::Item(ItemEvent::Started {
                item_id: id.clone(),
                item: item.clone(),
            }))
            .expect("start payload"),
        );
        deliver_model_fact(
            &mut driver,
            &mut model,
            &s,
            2,
            "anthropic-oauth",
            "claude-sonnet-4-6",
        );
        if !metadata_first {
            admit(&mut model);
        }
        deliver(
            &mut driver,
            &mut model,
            &s,
            3,
            serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
                item_id: id,
                item,
            }))
            .expect("complete payload"),
        );
        let projected = model.context_meter();
        assert_eq!(
            projected.auto_compact_at,
            Some(170_000),
            "metadata_first={metadata_first}"
        );
        assert_eq!(projected.auto_compact_percent(), Some(85));
        assert!(projected.threshold_projected);
        assert_eq!(projected.turns_to_threshold, None);
        let mut new = old;
        new.selection_epoch = Some(2);
        new.reserved_output_tokens = 30_000;
        new.soft_threshold_tokens = Some(170_000);
        new.estimated_turns_to_threshold = Some(4);
        deliver_footprint(&mut driver, &mut model, &s, 4, &new);
        let current = model.context_meter();
        assert!(!current.threshold_projected);
        assert_eq!(current.auto_compact_at, Some(170_000));
        assert_eq!(current.turns_to_threshold, Some(4));
    }
}

#[test]
fn legacy_metadata_without_budget_source_carries_nondefault_user_reserve() {
    let (mut model, mut driver, s) = equal_window_session();
    model.meter_epoch.user_budget = false;
    let mut legacy = listed_summary(&s, "eq-oauth", "eq-wide", 50_000);
    legacy.head_seq = 3;
    legacy.metadata.as_mut().expect("metadata").selection_epoch = Some(0);
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![legacy],
            next_cursor: None,
        },
    );
    assert!(model.meter_epoch.user_budget);
    deliver_model_fact(&mut driver, &mut model, &s, 1, "eq-oauth", "eq-mid");
    let meter = model.context_meter();
    assert_eq!(meter.auto_compact_at, Some(50_000));
    assert!(meter.reserve_assumed);
}

#[test]
fn loom_provider_command_targets_the_bound_session() {
    let (mut model, _driver) = two_attached_sessions();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    model.screen = Screen::Loom;
    model.requests.clear();
    selection_slash(&mut model, "/provider openai-oauth");
    assert!(model.requests.iter().any(|request| matches!(request,
        haider_tui::app::AppRequest::SelectModel { session, provider, .. }
        if session.as_str() == "s-meter-a" && provider == "openai-oauth")));
}

#[test]
fn account_refresh_while_attached_updates_only_future_launcher_default() {
    let (mut model, mut driver, s) = equal_window_session();
    let pair = model.meter_epoch.pair.clone();
    driver.apply(
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth")],
            revision: Some(11),
            sources: vec![],
        },
    );
    assert_eq!(model.meter_epoch.pair, pair);
    assert_eq!(model.identity.provider, "eq-oauth");
    model.checkin();
    assert_eq!(model.identity.provider, "openai-oauth");
    assert_eq!(model.active_session, None);
    assert_eq!(model.last_detached.as_ref(), Some(&s));
}

#[test]
fn launcher_create_keeps_the_account_route_unpinned_on_the_wire() {
    let (mut model, mut driver) = live_session(true);
    model.active_session = None;
    let commands = driver.handle_request(
        &mut model,
        haider_tui::app::AppRequest::CreateSession {
            text: "hello".into(),
        },
    );
    let create = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::Create { .. }))
        .expect("create");
    let LiveCommand::Create { account_alias, .. } = &create else {
        unreachable!()
    };
    assert_eq!(account_alias, &None);
    let body = haider_tui::link::request_body_for_features(create, &Default::default());
    let haider_rpc::RequestBody::SessionCreateWithPermissionOverrides { account_alias, .. } = body
    else {
        panic!("create body")
    };
    assert_eq!(account_alias, None);
}

#[test]
fn stale_list_last_model_cannot_undo_parked_roster_label() {
    let (mut model, mut driver) = two_attached_sessions();
    let b = session_id("s-meter-b");
    deliver_model_fact(
        &mut driver,
        &mut model,
        &b,
        1,
        "anthropic-oauth",
        "claude-sonnet-4-6",
    );
    let mut stale = listed_summary(&b, "anthropic-oauth", "claude-opus-5-5", 30_000);
    stale.head_seq = 0;
    stale.metadata.as_mut().expect("metadata").selection_epoch = Some(0);
    stale.last_model = Some("claude-opus-5-5".into());
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![stale],
            next_cursor: None,
        },
    );
    let row = model
        .sessions
        .iter()
        .find(|row| row.id == b)
        .expect("parked B");
    assert_eq!(row.meter_epoch.selection_epoch, Some(1));
    assert_eq!(row.model_short, "claude-sonnet-4-6");
}

#[test]
fn reconnect_retags_a_pending_selection_for_the_new_socket() {
    use haider_tui::live::LiveCommand;
    let (mut model, mut driver, s) = equal_window_session();
    let issued = driver.handle_request(
        &mut model,
        haider_tui::app::AppRequest::SelectModel {
            session: s.clone(),
            model: "eq-small".into(),
            provider: "eq-oauth".into(),
            confirm_new_epoch: false,
        },
    );
    assert!(issued.iter().any(|command| matches!(
        command,
        LiveCommand::SelectModel {
            connection_epoch: 0,
            ..
        }
    )));
    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "test reconnect".into(),
        },
    );
    driver.apply(&mut model, LiveReply::Reconnected);
    let resumed = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: s,
            attachment: haider_rpc::AttachmentId::new("reconnect-selection"),
            worker_generation: 7,
            replay_through_seq: 0,
            launch_origin: None,
        },
    );
    assert!(resumed.iter().any(|command| matches!(
        command,
        LiveCommand::SelectModel {
            connection_epoch: 1,
            ..
        }
    )));
}

#[test]
fn parked_budget_clamp_notice_stays_with_its_own_session() {
    let (mut model, mut driver) = two_attached_sessions();
    let b = session_id("s-meter-b");
    model.flash = Some("A remains visible".to_owned());
    driver.apply(
        &mut model,
        model_selected_reply(
            &b,
            "anthropic-oauth",
            "claude-sonnet-4-6",
            Some(budget(8_192, 50_000, true)),
        ),
    );
    assert_eq!(model.flash.as_deref(), Some("A remains visible"));
    let parked = model
        .sessions
        .iter()
        .find(|row| row.id == b)
        .expect("B remains parked");
    assert!(
        parked
            .projection
            .entries()
            .iter()
            .any(|entry| matches!(entry, haider_tui::projection::TranscriptEntry::Note { text } if text.contains("8192") || text.contains("8,192"))),
        "B receives its own clamp notice"
    );
}

#[test]
fn same_window_model_switch_rejects_an_old_inflight_snapshot() {
    let (mut model, mut driver, session) = equal_window_session();
    let old = user_set_footprint();
    let next = deliver_footprint(&mut driver, &mut model, &session, 1, &old);
    deliver_model_fact(
        &mut driver,
        &mut model,
        &session,
        next,
        "eq-oauth",
        "eq-mid",
    );
    let mut late = old;
    late.selection_epoch = Some(0);
    late.used_tokens = 55_000;
    late.estimated_turns_to_threshold = Some(5);
    deliver_footprint(&mut driver, &mut model, &session, next + 1, &late);
    let meter = model.context_meter();
    assert!(meter.threshold_projected);
    assert_eq!(meter.turns_to_threshold, None);
    assert_eq!(meter.window, Some(100_000));
}

#[test]
fn same_epoch_snapshot_with_conflicting_committed_reserve_is_projected() {
    let (mut model, mut driver, session) = equal_window_session();
    let mut footprint = user_set_footprint();
    footprint.reserved_output_tokens = 30_000;
    footprint.soft_threshold_tokens = Some(70_000);
    footprint.estimated_turns_to_threshold = Some(4);
    deliver_footprint(&mut driver, &mut model, &session, 1, &footprint);
    let meter = model.context_meter();
    assert_eq!(meter.auto_compact_at, Some(50_000));
    assert!(meter.threshold_projected);
    assert_eq!(meter.turns_to_threshold, None);
    for provider in &mut model.providers.providers {
        if provider.provider == "eq-oauth" {
            for detail in &mut provider.model_details {
                if detail.name == "eq-wide" {
                    detail.context_window = None;
                }
            }
        }
    }
    model.refresh_context_window();
    let meter = model.context_meter();
    assert_eq!(
        meter.window,
        Some(100_000),
        "request epoch still proves its window"
    );
    assert_eq!(meter.auto_compact_at, Some(50_000));
    assert!(meter.threshold_projected);
}

#[test]
fn boot_list_in_flight_gets_one_dirty_followup_for_a_new_selection() {
    let (mut model, mut driver, session) = equal_window_session();
    assert!(
        driver
            .boot()
            .iter()
            .any(|command| matches!(command, LiveCommand::ListAt { .. }))
    );
    let fact_commands = deliver(
        &mut driver,
        &mut model,
        &session,
        1,
        haider_protocol::session::ModelSelected {
            selection_epoch: Some(1),
            provider: "eq-oauth".into(),
            model: "eq-small".into(),
        }
        .to_payload_value()
        .expect("fact"),
    );
    assert!(
        !fact_commands
            .iter()
            .any(|command| matches!(command, LiveCommand::ListAt { .. }))
    );
    let follow = driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![],
            next_cursor: None,
        },
    );
    assert_eq!(
        follow
            .iter()
            .filter(|command| matches!(command, LiveCommand::ListAt { .. }))
            .count(),
        1
    );
}
