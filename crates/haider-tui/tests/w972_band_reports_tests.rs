//! 972-band-reports — the read-only report screens on the shared bottom
//! band (the footer redesign that CLOSES the documented exclusion in
//! `w971_tui_fixes_tests::all_views`).
//!
//! Every report — `/providers`, `/usage`, `/tools`, `/hooks`, `/tree`, the
//! fleet, graph and session browsers — now frames its bottom through
//! `render::report_band`: the shared band geometry with the screen's KEY
//! MAP in the one-row slot (the `/accounts` owner ruling — geometry and
//! framing, no composer), the background-task line and both rules drawn by
//! the band itself. The maps degrade by WIDTH so `esc back` — the way out —
//! survives at 80 columns.
//!
//! NAV RETIREMENT LAW (the parity-nav discipline this lane extends): every
//! band view enters and exits through `switch_surface`, so the overlays
//! whose input and geometry belong to the surface being left — transcript
//! search, the search-jump latch, `@` completion, the inbox — retire on
//! every report navigation. The typed doors used to write `self.screen`
//! directly (the "Tools/Tree precedent"), which skipped retirement.
//!
//! MUTATION CHECKS. Revert any report's `report_band` call to its old
//! bespoke footer and the band/slot assertions here fail for that view
//! (and `w971_tui_fixes_tests` fails its cross-view equality). Restore a
//! direct `self.screen = …` write on any typed door and the retirement
//! probe fails: the stale `search_jump` latch survives the navigation.
//! Drop the `/fleet` catalog row and the palette test fails. Widen a
//! narrow hint past the frame and the width sweep fails.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1,
};
use haider_tui::app::{AppEvent, AppModel, Hit, RuntimeMode, Screen};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use haider_tui::script::{ChipDisplayState, ChipSeed};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

mod common;
use common::{launcher_model, run_slash};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn ctrl(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL)));
}

/// Draw one frame and return its rows plus the hit map.
fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect();
    (rows, hits)
}

/// A live model with accounts, providers and every report feature seeded.
fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_version = Some("0.0.972".to_owned());
    model.daemon_features = [
        haider_rpc::FEATURE_HOOKS_V1.to_owned(),
        haider_rpc::FEATURE_USAGE_REPORT_V1.to_owned(),
        haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_FLEET_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model.requests.clear();
    model
}

/// A live model ATTACHED to a session — the state the session-only doors
/// (`/tools`, `/tree`, `/hooks`, `/graph`, fleet) require.
fn live_session_model() -> AppModel {
    let mut model = live_model();
    model.sessions.clear();
    let id = haider_protocol::ids::SessionId::new("band-session");
    model.upsert_live_session(&id);
    model.open_session(&id);
    model.requests.clear();
    model.screen = Screen::Session;
    model
}

fn chip(agent: &str, name: &str) -> haider_tui::app::ChipModel {
    haider_tui::app::ChipModel::from_seed(ChipSeed {
        agent: agent.to_owned(),
        parent: None,
        ros: None,
        callsign: "Ammar".to_owned(),
        hon: "(r)",
        full: "Ammar ibn Yasir".to_owned(),
        name: name.to_owned(),
        model: "fable-5".to_owned(),
        device: "test-lion-box".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })
}

fn on_screen(screen: Screen) -> AppModel {
    let mut model = live_model();
    model.screen = screen;
    model
}

/// Every report screen with enough state to render something real.
fn report_views() -> Vec<(&'static str, AppModel)> {
    let mut fleet = live_session_model();
    fleet.chips.push(chip("t1-audit", "audit"));
    fleet.screen = Screen::Fleet;
    vec![
        ("providers", on_screen(Screen::Providers)),
        ("usage", on_screen(Screen::Usage)),
        ("tools", on_screen(Screen::Tools)),
        ("hooks", on_screen(Screen::Hooks)),
        ("tree", on_screen(Screen::Tree)),
        ("sessions", on_screen(Screen::Sessions)),
        ("fleet", fleet),
        ("graph", on_screen(Screen::Graph)),
    ]
}

/// The two sizes the F2 suite pins plus the wide desktop pane.
const SIZES: [(u16, u16); 3] = [(120, 40), (80, 24), (160, 50)];

// ---------------------------------------------------------------------------
// The band, per view
// ---------------------------------------------------------------------------

/// Every report draws the shared band with its KEY MAP in the slot — and
/// the way OUT of the screen (`esc back…`) never falls off the frame.
#[test]
fn every_report_screen_carries_the_band_with_its_key_map_in_the_slot() {
    for (width, height) in SIZES {
        for (name, model) in report_views() {
            let (rows, _) = draw(&model, width, height);
            let slot = model
                .band_rect
                .get()
                .unwrap_or_else(|| panic!("{name} at {width}x{height}: drew no band"));
            let hint = &rows[usize::from(slot.y)];
            assert!(
                hint.contains("esc back") || hint.contains("esc up one level"),
                "{name} at {width}x{height}: the key map is not in the band slot, got {hint:?}"
            );
            // The band closes with a rule under the slot (+ the task line
            // when the model carries one), and the status row is below it.
            let line = model.tasks_line_rect.get().map_or(0, |rect| rect.height);
            let close = usize::from(slot.y + slot.height + line);
            let closing = rows[close].trim_end();
            assert!(
                !closing.is_empty() && closing.chars().all(|c| c == '─' || c == ' '),
                "{name} at {width}x{height}: no closing rule under the slot, got {closing:?}"
            );
            assert_eq!(
                close,
                usize::from(height) - 2,
                "{name} at {width}x{height}: the closing rule is not the last body row"
            );
        }
    }
}

/// The wide maps say more; the narrow maps keep the way out. Nothing the
/// slot shows may overrun the frame at the 80-column floor.
#[test]
fn report_key_maps_degrade_by_width_and_keep_the_way_out() {
    for (name, model) in report_views() {
        let (narrow_rows, _) = draw(&model, 80, 24);
        let slot = model.band_rect.get().expect("band");
        let narrow = narrow_rows[usize::from(slot.y)].trim_end().to_owned();
        assert!(
            narrow.chars().count() <= 80,
            "{name}: the narrow key map overruns 80 columns: {narrow:?}"
        );
        assert!(
            narrow.contains("esc back") || narrow.contains("esc up one level"),
            "{name}: the 80-column key map lost the way out: {narrow:?}"
        );
    }
    // The width-variant maps: wide says the long form, narrow the short.
    let tools = on_screen(Screen::Tools);
    let (wide_rows, _) = draw(&tools, 160, 50);
    let slot = tools.band_rect.get().expect("band");
    assert!(wide_rows[usize::from(slot.y)].contains("bounded supervised process"));
    let (narrow_rows, _) = draw(&tools, 80, 24);
    let slot = tools.band_rect.get().expect("band");
    assert!(narrow_rows[usize::from(slot.y)].contains("read-only — not a sandbox"));

    let sessions = on_screen(Screen::Sessions);
    let (wide_rows, _) = draw(&sessions, 160, 50);
    let slot = sessions.band_rect.get().expect("band");
    assert!(wide_rows[usize::from(slot.y)].contains("PgUp/PgDn page"));
    let (narrow_rows, _) = draw(&sessions, 80, 24);
    let slot = sessions.band_rect.get().expect("band");
    assert!(narrow_rows[usize::from(slot.y)].contains("⏎ open"));
}

// ---------------------------------------------------------------------------
// Overlay retirement on navigation
// ---------------------------------------------------------------------------

/// Every TYPED door into a report view retires the surface overlays. The
/// probe is the search-jump latch: it is exactly what
/// `retire_surface_overlays` clears and what a bare `self.screen = …`
/// write (the pre-lane doors) leaves stale.
#[test]
fn every_report_door_retires_the_surface_overlays() {
    let doors: [(&str, Screen); 7] = [
        ("/providers", Screen::Providers),
        ("/usage", Screen::Usage),
        ("/tools", Screen::Tools),
        ("/hooks", Screen::Hooks),
        ("/tree", Screen::Tree),
        ("/graph", Screen::Graph),
        ("/resume", Screen::Sessions),
    ];
    for (command, target) in doors {
        let mut model = live_session_model();
        *model.search_jump.borrow_mut() = Some(3);
        run_slash(&mut model, command);
        assert_eq!(
            model.screen, target,
            "{command}: the door did not open its screen"
        );
        assert_eq!(
            *model.search_jump.borrow(),
            None,
            "{command}: the stale search-jump latch survived the navigation"
        );
        assert!(model.transcript_search.is_none());
        assert!(model.mention_completion.is_none());
    }
    // The fleet door — with a subagent to show.
    let mut model = live_session_model();
    model.chips.push(chip("t1-audit", "audit"));
    *model.search_jump.borrow_mut() = Some(1);
    run_slash(&mut model, "/fleet");
    assert_eq!(model.screen, Screen::Fleet);
    assert_eq!(*model.search_jump.borrow(), None);
}

/// The EXIT doors retire too: leaving a report by esc must not carry a
/// stale latch back onto the session.
#[test]
fn report_exits_retire_the_surface_overlays() {
    for (command, escapes) in [
        ("/tools", 1),
        ("/tree", 1),
        ("/hooks", 1),
        ("/graph", 1),
        ("/resume", 1),
    ] {
        let mut model = live_session_model();
        run_slash(&mut model, command);
        assert_ne!(model.screen, Screen::Session, "{command} opened");
        *model.search_jump.borrow_mut() = Some(2);
        for _ in 0..escapes {
            key(&mut model, KeyCode::Esc);
        }
        assert_eq!(
            model.screen,
            Screen::Session,
            "{command}: esc walks back to the session"
        );
        assert_eq!(
            *model.search_jump.borrow(),
            None,
            "{command}: the stale latch survived the esc exit"
        );
    }
}

/// A LIVE transcript search retires when the mouse walks to a report
/// surface — the graph strip is the report door a click can reach.
#[test]
fn a_live_search_retires_when_a_hit_opens_the_graph() {
    let mut model = live_session_model();
    ctrl(&mut model, KeyCode::Char('f'));
    assert!(model.transcript_search.is_some());
    model.handle_hit(Hit::GraphStrip);
    assert_eq!(model.screen, Screen::Graph);
    assert!(
        model.transcript_search.is_none(),
        "the transcript search belongs to the session surface — it must retire"
    );
}

// ---------------------------------------------------------------------------
// The /fleet command
// ---------------------------------------------------------------------------

/// `/fleet` is registered in the shared RPC command catalog (the same
/// authority `/inbox` lives in) and opens the session-born fleet view;
/// without subagents it keeps `open_fleet`'s honest refusal.
#[test]
fn fleet_command_is_cataloged_and_opens_the_fleet() {
    let spec = haider_rpc::COMMANDS
        .iter()
        .find(|spec| spec.name == "fleet")
        .expect("/fleet is in the shared command catalog");
    assert!(spec.session_only, "the fleet is session-born");

    let mut model = live_session_model();
    run_slash(&mut model, "/fleet");
    assert_eq!(model.screen, Screen::Session, "no subagents — no fleet");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("no subagents")),
        "the refusal names the reason: {:?}",
        model.flash
    );

    model.chips.push(chip("t1-audit", "audit"));
    run_slash(&mut model, "/fleet");
    assert_eq!(model.screen, Screen::Fleet);
}

// ---------------------------------------------------------------------------
// Row activation
// ---------------------------------------------------------------------------

/// The sessions browser's natural row target: ⏎ opens the selected
/// session through the existing `open_session` action — no new verbs.
#[test]
fn sessions_row_activation_switches_to_the_selected_session() {
    let mut model = live_model();
    model.sessions.clear();
    let first = haider_protocol::ids::SessionId::new("band-first");
    let second = haider_protocol::ids::SessionId::new("band-second");
    model.upsert_live_session(&first);
    model.upsert_live_session(&second);
    model.open_session(&first);
    model.requests.clear();
    model.screen = Screen::Session;
    run_slash(&mut model, "/resume");
    assert_eq!(model.screen, Screen::Sessions);

    let rows = model.session_browser_rows();
    assert_eq!(rows.len(), 2);
    let target = rows[1].id.clone();
    key(&mut model, KeyCode::Down);
    key(&mut model, KeyCode::Enter);
    assert_eq!(
        model.active_session.as_ref(),
        Some(&target),
        "⏎ opens the selected session"
    );

    // The row is also a HIT — the mouse takes the same door.
    let mut model = live_model();
    model.sessions.clear();
    model.upsert_live_session(&first);
    model.open_session(&first);
    model.requests.clear();
    model.screen = Screen::Sessions;
    let (_, hits) = draw(&model, 120, 40);
    assert!(
        hits.iter()
            .any(|(_, hit)| matches!(hit, Hit::AttachSession(id) if id == &first)),
        "each browser row emits its AttachSession hit"
    );
}

// ---------------------------------------------------------------------------
// Plain-accessibility grammar
// ---------------------------------------------------------------------------

fn usage_report() -> UsageReportV1 {
    UsageReportV1 {
        generated_at_ms: 1_726_000_000_000,
        accounts: vec![
            AccountUsageReportV1 {
                provider: "anthropic-oauth".to_owned(),
                alias: haider_protocol::ids::CredentialAlias::new("anthropic"),
                identity: Some("max@example.com".to_owned()),
                plan: Some("max_20x".to_owned()),
                auth_method: haider_protocol::credential::AuthMethod::OAuth,
                meter: AccountMeterStateV1::Unavailable {
                    reason: "http_status_401".to_owned(),
                },
                local: LocalUsageStatsV1::default(),
            },
            AccountUsageReportV1 {
                provider: "openai".to_owned(),
                alias: haider_protocol::ids::CredentialAlias::new("openai"),
                identity: None,
                plan: None,
                auth_method: haider_protocol::credential::AuthMethod::ApiKey,
                meter: AccountMeterStateV1::LocalOnly,
                local: LocalUsageStatsV1::default(),
            },
        ],
    }
}

/// Each report speaks a plain grammar — the styled surface's information
/// in words (the `fleet_plain` precedent): availability, defaults, trust
/// and attention states are SPELLED, never left to a glyph.
#[test]
fn reports_carry_a_plain_grammar_that_spells_the_glyphs() {
    // Providers: availability words + the named default + the account
    // projection.
    let model = live_model();
    let plain = haider_tui::plain::providers_plain(&model);
    assert!(plain.contains("PROVIDERS — registry truth"));
    assert!(plain.contains("available") || plain.contains("unknown"));
    assert!(plain.contains("(default)"), "{plain}");
    assert!(plain.contains("account:"), "{plain}");

    // Usage: the demo honesty line, then the live meter states in words.
    let mut demo = launcher_model();
    demo.screen = Screen::Usage;
    let plain = haider_tui::plain::usage_plain(&demo);
    assert!(plain.contains("demo — usage is live daemon truth"));
    let mut live = live_model();
    live.usage.apply_report(usage_report());
    let plain = haider_tui::plain::usage_plain(&live);
    assert!(plain.contains("meter unavailable — "), "{plain}");
    assert!(plain.contains("local only"), "{plain}");

    // Tools: fetching honesty, then names + defaults + the containment
    // disclosure.
    let mut model = live_session_model();
    model.tools_inventory = None;
    assert!(haider_tui::plain::tools_plain(&model).contains("fetching"));
    model.tools_inventory = Some(haider_protocol::tool::ToolInventorySnapshot {
        tools: vec![haider_protocol::tool::ToolInventoryEntry {
            manifest: haider_protocol::tool::ToolManifest {
                name: "process_exec".into(),
                description: "run one supervised shell command".into(),
                effects: vec![haider_protocol::effect::EffectClass::ProcessExec],
                dispatch: haider_protocol::tool::DispatchMode::Await,
                input_schema: serde_json::json!({}),
            },
            default: haider_protocol::tool::ToolPermissionDefault::Ask,
        }],
        remembered_grants: Vec::new(),
    });
    let plain = haider_tui::plain::tools_plain(&model);
    assert!(
        plain.contains("process_exec — effects processexec; default ask"),
        "{plain}"
    );
    assert!(plain.contains("not a sandbox"));

    // Hooks: trust states in words.
    let mut model = live_session_model();
    model.hooks.rows = Some(vec![haider_tui::hooks::HookRow {
        name: "fmt-check".to_owned(),
        digest: "ab".repeat(32),
        source: "hooks.json".to_owned(),
        kind: "exec".to_owned(),
        event: "pre_commit".to_owned(),
        trusted: true,
        trust_state: None,
        decision: false,
        timeout_ms: 5_000,
    }]);
    let plain = haider_tui::plain::hooks_plain(&model);
    assert!(plain.contains("fmt-check — exec:pre_commit"), "{plain}");
    assert!(plain.contains("trusted"), "{plain}");
    assert!(plain.contains("recent firings"), "{plain}");

    // Tree: the crumb and the rows' own labels.
    let model = live_session_model();
    let plain = haider_tui::plain::tree_plain(&model);
    assert!(plain.starts_with("SESSION TREE — "), "{plain}");

    // Sessions: counts + honest empty state.
    let mut model = live_model();
    model.sessions.clear();
    model.screen = Screen::Sessions;
    let plain = haider_tui::plain::sessions_plain(&model);
    assert!(plain.contains("0 on this machine"), "{plain}");
    assert!(plain.contains("no sessions yet"), "{plain}");
}
