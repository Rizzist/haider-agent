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
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
                supports_vision: None,
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

/// Models OUTSIDE the catalog (the daemon knows their window, the client
/// has no row): the snapshot's window serves — but never across a switch.
///
/// MUTATION CHECK: drop `snapshot_predates_model` from `context_meter`.
/// Expected runtime failure: after the switch the line keeps `of 1M`.
#[test]
fn a_switch_outside_the_catalog_never_keeps_the_previous_snapshot_window() {
    let (mut model, _driver) = live_session(true);
    model.identity.provider = "anthropic".to_owned();
    model.identity.model_short = "claude-opus-5-5".to_owned();
    model.refresh_context_window();
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
    pick_model(&mut model, "claude-sonnet-4-6");
    golden_case(&mut model, "claude-sonnet-4-6 · after switch", &mut out);
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
