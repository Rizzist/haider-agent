//! 973-activity-icons — one activity glyph LEFT of each subagent's name,
//! showing what it is DOING (not its state).
//!
//! Laws under test:
//!
//! * derivation is typed (tool identity on journal items routed to the chip)
//!   and live — the glyph follows calls as they start, the highest-priority
//!   hot activity wins, and a quiet agent falls back to its most recent
//!   activity, then to none;
//! * rows keep NAMES aligned: every row reserves the same measured slot
//!   (emoji graphemes with VS16 are two columns), and no activity anywhere
//!   means no slot at all;
//! * `/icons text` renders ASCII labels, `/icons off` renders nothing;
//!   plain output always speaks the text labels;
//! * the fleet list and grid wear the glyph; the member detail lists every
//!   activity used so far;
//! * goldens pin the chip panel and the fleet list at 80x24 and 118x36 in
//!   dark and light themes, with emoji and with text labels.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::EventPayload;
use haider_protocol::activity::{
    ACTIVITY_HOT_WINDOW_MS, ACTIVITY_LINGER_MS, ActivityDisplay, ActivityKind,
};
use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::EffectId;
use haider_protocol::ids::{AgentId, DeviceId, EventId, ItemId, LeaseId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_tui::app::{AppEvent, AppModel, Screen};
use haider_tui::theme::ThemeKey;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

mod tuivirt_common;
use tuivirt_common::{assert_same_frame, check_golden, draw, session_id, session_model};

const T0: u64 = 1_800_000_000_000;

const AMINA: &str = "agent-amina";
const BILAL: &str = "agent-bilal";
const KAWTHAR: &str = "agent-kawthar";
const DAWUD: &str = "agent-dawud";
const SUMAYYA: &str = "agent-sumayya";

fn manifest(agent: &str, callsign: &str, task: &str, parent: Option<&str>) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: task.to_owned(),
        callsign: Some(callsign.to_owned()),
        model_profile: "fable-5".to_owned(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new(format!("lease-{agent}")),
        fencing_epoch: 1,
        attempt: 0,
        parent: parent.map(AgentId::new),
        coordinates: Some(serde_json::json!({
            "parent_session_id": session_id().as_str(),
            "child_session_id": format!("child-{agent}"),
        })),
        cli_scope: None,
    }
}

fn raw(seq: u64, agent: Option<&str>, at_ms: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-activity-{seq}")),
        seq,
        session_id: session_id(),
        branch_id: None,
        run_id: None,
        agent_id: agent.map(AgentId::new),
        device_id: DeviceId::new("activity-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: at_ms,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload)
            .expect("payload serializes")
            .into(),
    }
}

fn call(id: &str, name: &str) -> TurnItem {
    TurnItem::ToolCall {
        call_id: format!("call-{id}"),
        name: name.to_owned(),
        args: serde_json::json!({}),
        status: ToolStatus::InProgress,
    }
}

struct Fixture {
    model: AppModel,
    seq: u64,
}

impl Fixture {
    fn route(&mut self, agent: Option<&str>, at_ms: u64, payload: EventPayload) {
        self.seq += 1;
        self.model.route_raw(&raw(self.seq, agent, at_ms, &payload));
    }

    fn spawn(&mut self, agent: &str, callsign: &str, task: &str, parent: Option<&str>) {
        self.route(
            None,
            T0,
            EventPayload::AgentSpawned(manifest(agent, callsign, task, parent)),
        );
    }

    /// A REAL call: the model's item starts, and the daemon journals the
    /// typed effect intent, the allow verdict and the dispatch.
    fn start(&mut self, agent: &str, id: &str, name: &str, at_ms: u64) {
        self.route(
            Some(agent),
            at_ms,
            EventPayload::Item(ItemEvent::Started {
                item_id: ItemId::new(id),
                item: call(id, name),
            }),
        );
        let effect = EffectId::new(format!("effect-{id}"));
        self.route(
            Some(agent),
            at_ms,
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: effect.clone(),
                class: class_of(name),
                summary: format!("{name} call"),
                args_digest: "digest".into(),
                workspace_revision: None,
            })),
        );
        self.route(
            Some(agent),
            at_ms,
            EventPayload::Effect(EffectPhase::Authorized {
                effect: effect.clone(),
                verdict: AuthorizationVerdict::Allow,
            }),
        );
        self.route(
            Some(agent),
            at_ms,
            EventPayload::Effect(EffectPhase::Dispatched { effect }),
        );
    }

    fn finish(&mut self, agent: &str, id: &str, name: &str, at_ms: u64) {
        self.route(
            Some(agent),
            at_ms,
            EventPayload::Effect(EffectPhase::Outcome {
                effect: EffectId::new(format!("effect-{id}")),
                outcome: EffectOutcome::Ok,
                freshness: None,
                workspace_mutation: None,
            }),
        );
        let mut item = call(id, name);
        if let TurnItem::ToolCall { status, .. } = &mut item {
            *status = ToolStatus::Completed;
        }
        self.route(
            Some(agent),
            at_ms,
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(id),
                item,
            }),
        );
    }

    /// A SPOOFED call: the model names a tool it was never granted; the item
    /// starts and fails, and no effect is ever dispatched.
    fn spoof(&mut self, agent: &str, id: &str, name: &str, at_ms: u64) {
        self.route(
            Some(agent),
            at_ms,
            EventPayload::Item(ItemEvent::Started {
                item_id: ItemId::new(id),
                item: call(id, name),
            }),
        );
        let mut item = call(id, name);
        if let TurnItem::ToolCall { status, .. } = &mut item {
            *status = ToolStatus::Failed;
        }
        self.route(
            Some(agent),
            at_ms + 1,
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(id),
                item,
            }),
        );
    }
}

fn class_of(name: &str) -> EffectClass {
    match name {
        "computer" => EffectClass::ScreenControl,
        "web_search" | "web_fetch" => EffectClass::Network {
            host: "example.test".into(),
        },
        "fs_edit" => EffectClass::FsWrite,
        "test_run" | "process_exec" => EffectClass::ProcessExec,
        _ => EffectClass::FsRead,
    }
}

/// Four subagents doing different things (the chip panel stays expanded
/// under the five-node fleet collapse): Amina read → edit (edit open),
/// Bilal on the web, Kawthar orchestrating her own child Dawud (reading).
fn busy_session() -> Fixture {
    let mut fixture = Fixture {
        model: session_model(),
        seq: 0,
    };
    fixture.spawn(AMINA, "Amina", "patch the parser", None);
    fixture.spawn(BILAL, "Bilal", "check the release notes", None);
    fixture.spawn(KAWTHAR, "Kawthar", "coordinate the audit", None);
    fixture.spawn(DAWUD, "Dawud", "read the frame tests", Some(KAWTHAR));
    for chip in &mut fixture.model.chips {
        chip.device = "test-box".into();
    }
    fixture.start(AMINA, "a1", "fs_read", T0 + 1_000);
    fixture.finish(AMINA, "a1", "fs_read", T0 + 1_200);
    fixture.start(AMINA, "a2", "fs_edit", T0 + 2_000);
    fixture.start(BILAL, "b1", "web_search", T0 + 1_500);
    fixture.start(DAWUD, "d1", "fs_search", T0 + 2_500);
    fixture.model.clock_ms = T0 + 3_000;
    fixture.model.requests.clear();
    fixture
}

fn chip<'m>(model: &'m AppModel, agent: &str) -> &'m haider_tui::app::ChipModel {
    haider_tui::app::find_chip(&model.chips, agent).expect("chip")
}

fn row<'f>(frame: &'f tuivirt_common::Snapshot, needle: &str) -> &'f str {
    let index = frame
        .row_containing(needle)
        .unwrap_or_else(|| panic!("no row containing {needle:?}:\n{}", frame.rows.join("\n")));
    &frame.rows[index]
}

/// Display column where `needle` starts in `row` (measured, not chars).
fn column(row: &str, needle: &str) -> usize {
    let byte = row.find(needle).expect("needle in row");
    UnicodeWidthStr::width(&row[..byte])
}

fn submit(model: &mut AppModel, text: &str) {
    for c in text.chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
}

#[test]
fn chips_derive_activity_from_typed_calls_in_priority_order() {
    let fixture = busy_session();
    let model = &fixture.model;
    assert_eq!(
        model.chip_activity(chip(model, AMINA)),
        Some(ActivityKind::Edit),
        "read then edit inside the hot window: edit outranks read"
    );
    assert_eq!(
        model.chip_activity(chip(model, BILAL)),
        Some(ActivityKind::Web)
    );
    assert_eq!(
        model.chip_activity(chip(model, KAWTHAR)),
        Some(ActivityKind::Orchestrate),
        "a quiet parent with a live child is orchestrating"
    );
    assert_eq!(
        model.chip_activity(chip(model, DAWUD)),
        Some(ActivityKind::Read)
    );
    assert_eq!(
        chip(model, AMINA).activity.used(),
        vec![ActivityKind::Edit, ActivityKind::Read]
    );
}

#[test]
fn glyph_sits_left_of_the_name_and_names_stay_aligned() {
    let fixture = busy_session();
    for (width, height) in [(118, 36), (80, 24)] {
        let frame = draw(&fixture.model, width, height);
        let amina = row(&frame, "Amina");
        let bilal = row(&frame, "Bilal");
        let kawthar = row(&frame, "Kawthar");
        let dawud = row(&frame, "Dawud");
        assert!(amina.contains("\u{270F}\u{FE0F} Amina"), "{amina}");
        assert!(bilal.contains("\u{1F310} Bilal"), "{bilal}");
        assert!(kawthar.contains("\u{1F9ED} Kawthar"), "{kawthar}");
        assert!(dawud.contains("\u{1F50D} Dawud"), "{dawud}");
        // Same depth → same name column, whatever the glyph (VS16 emoji
        // measure two columns like the others).
        assert_eq!(column(amina, "Amina"), column(bilal, "Bilal"), "{width}");
        assert_eq!(column(amina, "Amina"), column(kawthar, "Kawthar"));
        // The state glyph still leads: activity never replaces state.
        assert!(
            column(amina, "\u{270F}") > 2,
            "state glyph precedes the activity glyph: {amina}"
        );
        // No row overflows the terminal.
        for text in &frame.rows {
            assert!(UnicodeWidthStr::width(text.as_str()) <= usize::from(width));
        }
    }
}

#[test]
fn live_updates_follow_new_calls_and_quiet_falls_back() {
    let mut fixture = busy_session();
    // Amina's edit finishes; after the hot window a test run starts.
    fixture.finish(AMINA, "a2", "fs_edit", T0 + 3_100);
    let later = T0 + 3_100 + ACTIVITY_HOT_WINDOW_MS + 1;
    fixture.start(AMINA, "a3", "test_run", later);
    fixture.model.clock_ms = later;
    assert_eq!(
        fixture.model.chip_activity(chip(&fixture.model, AMINA)),
        Some(ActivityKind::Run)
    );
    let frame = draw(&fixture.model, 118, 36);
    assert!(row(&frame, "Amina").contains("\u{2699}\u{FE0F} Amina"));
    // The run finishes and everything goes quiet: most recent first …
    fixture.finish(AMINA, "a3", "test_run", later + 10);
    fixture.model.clock_ms = later + 10 + ACTIVITY_HOT_WINDOW_MS + 1;
    assert_eq!(
        fixture.model.chip_activity(chip(&fixture.model, AMINA)),
        Some(ActivityKind::Run)
    );
    // … then nothing after the linger.
    fixture.model.clock_ms = later + 10 + ACTIVITY_LINGER_MS + 1;
    assert_eq!(
        fixture.model.chip_activity(chip(&fixture.model, AMINA)),
        None
    );
}

#[test]
fn terminal_chips_show_no_activity_and_no_activity_means_no_slot() {
    let fixture = busy_session();
    let mut quiet = busy_session();
    quiet.model.clock_ms = T0 + 10 * ACTIVITY_LINGER_MS;
    // Open calls keep Amina/Bilal/Dawud current, so close them first.
    quiet.finish(AMINA, "a2", "fs_edit", T0 + 3_000);
    quiet.finish(BILAL, "b1", "web_search", T0 + 3_000);
    quiet.finish(DAWUD, "d1", "fs_search", T0 + 3_000);
    // Kawthar still orchestrates while her child Dawud is live …
    assert_eq!(
        quiet.model.chip_activity(chip(&quiet.model, KAWTHAR)),
        Some(ActivityKind::Orchestrate)
    );
    // … and goes quiet once Dawud is done.
    haider_tui::app::find_chip_mut(&mut quiet.model.chips, DAWUD)
        .expect("chip")
        .state = haider_tui::script::ChipDisplayState::Done;
    for agent in [AMINA, BILAL, KAWTHAR, DAWUD] {
        assert_eq!(quiet.model.chip_activity(chip(&quiet.model, agent)), None);
    }
    // With no activity anywhere the rows draw exactly as a model that never
    // saw a tool call (no reserved slot).
    let mut untouched = Fixture {
        model: session_model(),
        seq: 0,
    };
    untouched.spawn(AMINA, "Amina", "patch the parser", None);
    untouched.spawn(BILAL, "Bilal", "check the release notes", None);
    untouched.spawn(KAWTHAR, "Kawthar", "coordinate the audit", None);
    untouched.spawn(DAWUD, "Dawud", "read the frame tests", Some(KAWTHAR));
    for chip in &mut untouched.model.chips {
        chip.device = "test-box".into();
    }
    haider_tui::app::find_chip_mut(&mut untouched.model.chips, DAWUD)
        .expect("chip")
        .state = haider_tui::script::ChipDisplayState::Done;
    untouched.model.clock_ms = quiet.model.clock_ms;
    untouched.model.requests.clear();
    let quiet_rows = draw(&quiet.model, 118, 36);
    let untouched_rows = draw(&untouched.model, 118, 36);
    for name in ["Amina", "Bilal", "Kawthar", "Dawud"] {
        // Everything up to and including the name is identical (the act
        // slot after the name narrates the transcript and legitimately
        // differs).
        let prefix = |text: &str| text[..text.find(name).expect("name") + name.len()].to_owned();
        assert_eq!(
            prefix(row(&quiet_rows, name)),
            prefix(row(&untouched_rows, name))
        );
    }
    // A terminal chip (done) wears no activity even with an open call.
    let mut done = fixture;
    haider_tui::app::find_chip_mut(&mut done.model.chips, AMINA)
        .expect("chip")
        .state = haider_tui::script::ChipDisplayState::Done;
    assert_eq!(done.model.chip_activity(chip(&done.model, AMINA)), None);
}

#[test]
fn icons_command_switches_text_labels_and_off() {
    let mut fixture = busy_session();
    submit(&mut fixture.model, "/icons text");
    assert_eq!(fixture.model.activity_display, ActivityDisplay::Text);
    assert_eq!(fixture.model.activity_display_commits, 1);
    let frame = draw(&fixture.model, 118, 36);
    assert!(row(&frame, "Amina").contains("[edit] Amina"));
    assert!(row(&frame, "Bilal").contains("[web]"));
    assert!(row(&frame, "Kawthar").contains("[orch] Kawthar"));
    // Text labels differ in width; the slot pads to the widest so names
    // still align.
    assert_eq!(
        column(row(&frame, "Amina"), "Amina"),
        column(row(&frame, "Bilal"), "Bilal")
    );
    submit(&mut fixture.model, "/icons off");
    assert_eq!(fixture.model.activity_display, ActivityDisplay::Off);
    let off = draw(&fixture.model, 118, 36);
    for text in &off.rows {
        assert!(
            !text.contains("[edit]") && !text.contains('\u{270F}'),
            "{text}"
        );
    }
    // Off draws exactly the rows of a model without any activity glyphs.
    let mut bare = busy_session();
    bare.model.activity_display = ActivityDisplay::Off;
    bare.model.flash = fixture.model.flash.clone();
    assert_same_frame("icons off", &off, &draw(&bare.model, 118, 36));
    // Bare /icons cycles back to emoji; unknown values are refused.
    submit(&mut fixture.model, "/icons");
    assert_eq!(fixture.model.activity_display, ActivityDisplay::Emoji);
    submit(&mut fixture.model, "/icons sparkles");
    assert_eq!(fixture.model.activity_display, ActivityDisplay::Emoji);
}

fn fleet_open(fixture: &mut Fixture) {
    let snapshot = haider_tui::fleet::snapshot_from_chips(
        &fixture.model.chips,
        &session_id(),
        fixture.model.clock_ms,
    );
    fixture.model.fleet.snapshot = Some(snapshot);
    fixture.model.screen = Screen::Fleet;
}

/// Five members for the fleet: the busy four plus Sumayya on the computer.
fn busy_fleet() -> Fixture {
    let mut fixture = busy_session();
    fixture.spawn(SUMAYYA, "Sumayya", "click through the installer", None);
    haider_tui::app::find_chip_mut(&mut fixture.model.chips, SUMAYYA)
        .expect("chip")
        .device = "test-box".into();
    fixture.start(SUMAYYA, "s1", "computer", T0 + 2_900);
    fixture.model.clock_ms = T0 + 3_000;
    fleet_open(&mut fixture);
    fixture
}

#[test]
fn fleet_list_and_detail_carry_activity() {
    let mut fixture = busy_fleet();
    let frame = draw(&fixture.model, 118, 36);
    assert!(row(&frame, "Sumayya").contains("\u{1F5A5}\u{FE0F} Sumayya"));
    assert!(row(&frame, "Amina").contains("\u{270F}\u{FE0F} Amina"));
    assert_eq!(
        column(row(&frame, "Sumayya"), "Sumayya"),
        column(row(&frame, "Amina"), "Amina")
    );
    // The collapsed entry row in the session panel (≥ 5 nodes) is unchanged
    // grammar; the fleet is where the per-member glyphs live.
    let snapshot = fixture.model.fleet.snapshot.clone().expect("snapshot");
    let amina = haider_tui::fleet::find_node(&snapshot, AMINA).expect("node");
    assert_eq!(amina.activity, Some(ActivityKind::Edit));
    assert_eq!(
        amina.activities,
        vec![ActivityKind::Edit, ActivityKind::Read]
    );
    // Detail view lists every activity used so far.
    fixture.model.fleet.detail = Some(AgentId::new(AMINA));
    let detail = draw(&fixture.model, 118, 36);
    assert!(detail.contains("activity"), "{}", detail.rows.join("\n"));
    assert!(detail.contains("now  \u{270F}\u{FE0F} editing files"));
    assert!(detail.contains("used \u{270F}\u{FE0F} editing files · \u{1F50D} reading"));
    let kawthar_used = fixture
        .model
        .node_activities(haider_tui::fleet::find_node(&snapshot, KAWTHAR).expect("node"));
    assert_eq!(kawthar_used, vec![ActivityKind::Orchestrate]);
}

#[test]
fn plain_fleet_speaks_text_labels_even_in_emoji_mode() {
    let fixture = busy_fleet();
    let plain = haider_tui::fleet::fleet_plain(&fixture.model);
    assert!(plain.contains("[screen] Sumayya"), "{plain}");
    assert!(plain.contains("[edit] Amina"), "{plain}");
    assert!(plain.contains("[orch] Kawthar"), "{plain}");
    assert!(plain.contains("[read] Dawud"), "{plain}");
    assert!(!plain.contains('\u{270F}'), "{plain}");
    let mut off = busy_fleet();
    off.model.activity_display = ActivityDisplay::Off;
    let plain = haider_tui::fleet::fleet_plain(&off.model);
    assert!(!plain.contains("[edit]"), "{plain}");
}

#[test]
fn goldens_chip_panel_and_fleet_list_both_sizes_themes_and_modes() {
    for (theme_name, theme) in [("dark", ThemeKey::Dark), ("light", ThemeKey::Light)] {
        for (mode_name, mode) in [
            ("emoji", ActivityDisplay::Emoji),
            ("text", ActivityDisplay::Text),
        ] {
            let mut chips = busy_session();
            chips.model.theme = theme;
            chips.model.activity_display = mode;
            let mut fleet = busy_fleet();
            fleet.model.theme = theme;
            fleet.model.activity_display = mode;
            for (width, height) in [(80, 24), (118, 36)] {
                check_golden(
                    &format!("activity_chips_{theme_name}_{mode_name}"),
                    &draw(&chips.model, width, height),
                );
                check_golden(
                    &format!("activity_fleet_{theme_name}_{mode_name}"),
                    &draw(&fleet.model, width, height),
                );
            }
        }
    }
}

#[test]
fn settings_persist_the_icons_choice_additively() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(haider_tui::settings::SETTINGS_FILE);
    // A pre-lane file loads with no choice (env heuristics apply).
    std::fs::write(
        &path,
        br#"{"version":1,"theme":"dark","notifications":true}"#,
    )
    .expect("seed");
    let mut store = haider_tui::settings::SettingsStore::at(path.clone());
    assert_eq!(store.load_activity_icons(), None);
    store.set_notifications(false);
    store.save_activity_icons_if_changed(
        haider_tui::theme::ThemeChoice::Fixed(ThemeKey::Dark),
        ActivityDisplay::Text,
    );
    let reopened = haider_tui::settings::SettingsStore::at(path.clone());
    assert_eq!(reopened.load_activity_icons(), Some(ActivityDisplay::Text));
    assert!(!reopened.load_notifications(), "other settings ride along");
    let raw = std::fs::read_to_string(&path).expect("settings");
    assert!(raw.contains(r#""activity_icons":"text""#), "{raw}");
    // An unknown value degrades to "not chosen", never a crash.
    std::fs::write(
        &path,
        br#"{"version":1,"theme":"dark","activity_icons":"sparkles"}"#,
    )
    .expect("seed");
    assert_eq!(
        haider_tui::settings::SettingsStore::at(path).load_activity_icons(),
        None
    );
}

#[test]
fn live_mode_chips_read_the_childs_published_activity_state() {
    // Live daemons keep a child's tool items in the CHILD session; the chip
    // learns them from the child's agent metrics (session summary), whose
    // activity state the TUI evaluates on its own clock.
    use haider_protocol::activity::{ActivityStateV1, ActivityTouchV1};
    let mut fixture = Fixture {
        model: session_model(),
        seq: 0,
    };
    fixture.spawn(AMINA, "Amina", "patch the parser", None);
    fixture.model.chips[0].device = "test-box".into();
    let state = ActivityStateV1 {
        open: vec![ActivityKind::Remote],
        touched: vec![
            ActivityTouchV1 {
                kind: ActivityKind::Remote,
                at_ms: T0 + 1_000,
            },
            ActivityTouchV1 {
                kind: ActivityKind::Read,
                at_ms: T0 + 500,
            },
        ],
        latest: Some(ActivityTouchV1 {
            kind: ActivityKind::Remote,
            at_ms: T0 + 1_000,
        }),
        terminal: false,
    };
    fixture.model.chips[0].metrics = Some(haider_protocol::agent::AgentMetricsSnapshot {
        agent: Some(AgentId::new(AMINA)),
        session_id: haider_protocol::ids::SessionId::new(format!("child-{AMINA}")),
        head_seq: 4,
        started_at_ms: T0,
        terminal_at_ms: None,
        live: true,
        tool_attempts: 2,
        usage: None,
        activity: Some(state.clone()),
    });
    fixture.model.clock_ms = T0 + 5 * ACTIVITY_LINGER_MS;
    assert!(chip(&fixture.model, AMINA).activity.is_empty());
    assert_eq!(
        fixture.model.chip_activity(chip(&fixture.model, AMINA)),
        Some(ActivityKind::Remote),
        "an open ssh_shell stays current however long it runs"
    );
    let frame = draw(&fixture.model, 80, 24);
    assert!(row(&frame, "Amina").contains("\u{1F50C} Amina"));
    // The call closes: the state decays on the TUI clock with no new frame.
    let mut closed = state;
    closed.open.clear();
    fixture.model.chips[0]
        .metrics
        .as_mut()
        .expect("metrics")
        .activity = Some(closed);
    fixture.model.clock_ms = T0 + 1_000 + ACTIVITY_HOT_WINDOW_MS + 1;
    assert_eq!(
        fixture.model.chip_activity(chip(&fixture.model, AMINA)),
        Some(ActivityKind::Remote)
    );
    fixture.model.clock_ms = T0 + 1_000 + ACTIVITY_LINGER_MS + 1;
    assert_eq!(
        fixture.model.chip_activity(chip(&fixture.model, AMINA)),
        None
    );
}

#[test]
fn a_spoofed_tool_name_never_shows_an_icon() {
    // F1: Bilal (no computer use) names `computer`; the call starts, is
    // refused, and never dispatches. No 🖥️, no history, no slot.
    let mut fixture = Fixture {
        model: session_model(),
        seq: 0,
    };
    fixture.spawn(BILAL, "Bilal", "check the release notes", None);
    fixture.model.chips[0].device = "test-box".into();
    fixture.spoof(BILAL, "x1", "computer", T0 + 1_000);
    fixture.spoof(BILAL, "x2", "mobile", T0 + 1_100);
    fixture.model.clock_ms = T0 + 1_200;
    assert_eq!(
        fixture.model.chip_activity(chip(&fixture.model, BILAL)),
        None
    );
    assert!(chip(&fixture.model, BILAL).activity.used().is_empty());
    let frame = draw(&fixture.model, 118, 36);
    let bilal = row(&frame, "Bilal");
    assert!(
        !bilal.contains('\u{1F5A5}') && !bilal.contains('\u{1F4F1}'),
        "{bilal}"
    );
}
