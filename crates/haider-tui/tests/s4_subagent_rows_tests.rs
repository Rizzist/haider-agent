//! S4 — subagent rows wear a RIGHT-ALIGNED `elapsed · ↓ tokens` meta
//! (owner directive, Claude Code screenshot as the model).
//!
//! Laws under test:
//!
//! * duration format is h/m/s (`42s` · `25m 18s` · `1h 4m 9s`), token
//!   figures reuse the ONE shared `fmt_tok` k/M vocabulary;
//! * live children TICK on the existing anim clock's `clock_ms` (no new
//!   timer); terminal children are FROZEN at journal timestamps
//!   (`AgentSpawned committed_at_ms` → the terminal envelope's);
//! * the token join is by the chip's OWN manifest `child_session_id`,
//!   exact-match — a wrong child never wears another child's tokens;
//! * unknown is never rendered as zero — a missing source DROPS its
//!   segment;
//! * width degradation drops WHOLE segments, tokens first, then elapsed
//!   (the F2c pattern) — never a mid-segment truncation.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, AgentRole, ChipState, Grant, Placement};
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, DeviceId, EventId, ItemId, LeaseId, SessionId};
use haider_protocol::item::ItemEvent;
use haider_tui::app::{AppModel, ChipModel, RuntimeMode, Screen, chip_row_tokens};
use haider_tui::format::fmt_elapsed;
use haider_tui::render::render;
use haider_tui::script::ChipDisplayState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::launcher_model;

const CHILD_A: &str = "agent-s4-a";
const CHILD_B: &str = "agent-s4-b";
const CHILD_A_SESSION: &str = "session-child-s4-a";
const CHILD_B_SESSION: &str = "session-child-s4-b";
/// The journal's spawn instant (epoch ms) all the timing tests build on.
const SPAWN_MS: u64 = 1_000_000_000;
/// `25m 18s` — the owner example's figure.
const RUN_MS: u64 = 25 * 60_000 + 18_000;

fn sid() -> SessionId {
    SessionId::new("s-s4")
}

fn manifest(agent: &str, task: &str, child_session: Option<&str>) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: task.to_owned(),
        callsign: Some("Ammar".to_owned()),
        model_profile: "fable-5".to_owned(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        // A NAMED device: `Placement::Local` renders the host's own name,
        // which would make every width assertion host-dependent.
        placement: Placement::Device {
            device: DeviceId::new("test-box"),
        },
        lease: LeaseId::new(format!("lease-{agent}")),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: child_session.map(|child| {
            serde_json::json!({
                "parent_session_id": "s-s4",
                "child_session_id": child,
            })
        }),
    }
}

fn raw(seq: u64, agent: Option<&str>, at_ms: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-s4-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: agent.map(AgentId::new),
        device_id: DeviceId::new("s4-device"),
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
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

/// A live session with one child spawned at [`SPAWN_MS`], per the journal.
fn live_session_with_chip() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.route_raw(&raw(
        1,
        None,
        SPAWN_MS,
        &EventPayload::AgentSpawned(manifest(CHILD_A, "stitch", Some(CHILD_A_SESSION))),
    ));
    model.requests.clear();
    model
}

fn draw_rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
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
        })
        .collect()
}

fn chip_row(rows: &[String]) -> String {
    rows.iter()
        .find(|row| row.contains("Ammar (r)"))
        .cloned()
        .expect("the subtree renders the chip row")
}

// ------------------------------------------------------------ formats ----

/// MUTATION CHECK (S4): compact the hour tier (`1h 42s`) or zero-pad the
/// minutes. Expected runtime failure: the pinned tier strings below.
#[test]
fn fmt_elapsed_is_law_pinned_h_m_s() {
    assert_eq!(fmt_elapsed(0), "0s");
    assert_eq!(fmt_elapsed(42_000), "42s");
    // Truncation, not rounding: a second shows once fully elapsed.
    assert_eq!(fmt_elapsed(59_999), "59s");
    assert_eq!(fmt_elapsed(60_000), "1m 0s");
    assert_eq!(fmt_elapsed(RUN_MS), "25m 18s");
    assert_eq!(fmt_elapsed(3_600_000), "1h 0m 0s");
    assert_eq!(fmt_elapsed((3600 + 4 * 60 + 9) * 1000), "1h 4m 9s");
    assert_eq!(
        fmt_elapsed((26 * 3600 + 59 * 60 + 59) * 1000),
        "26h 59m 59s"
    );
}

// ---------------------------------------------------- live-tick law ----

/// MUTATION CHECK (S4): freeze live chips too (read `last_event_at_ms`
/// instead of the render clock). Expected runtime failure: the figure
/// below never moves off `0s` when the clock advances.
#[test]
fn live_chip_elapsed_ticks_on_the_render_clock() {
    let mut model = live_session_with_chip();
    model.clock_ms = SPAWN_MS + RUN_MS;
    let row = chip_row(&draw_rows(&model, 118, 36));
    assert!(
        row.contains("25m 18s"),
        "the live figure is clock − spawn: {row:?}"
    );
    // Right-aligned in the row, with a real gap before it — never glued
    // onto the activity text.
    let trimmed = row.trim_end();
    assert!(
        trimmed.ends_with("25m 18s"),
        "the meta sits at the row's right end: {row:?}"
    );
    assert!(
        trimmed.contains("  "),
        "a pad gap separates content from the meta: {row:?}"
    );
    // The tick: one more second on the shared clock moves the figure.
    model.clock_ms += 1000;
    let row = chip_row(&draw_rows(&model, 118, 36));
    assert!(row.contains("25m 19s"), "the figure ticks: {row:?}");
}

/// The route-raw path advances the render clock from committed journal
/// time, so the first paint after a spawn is already in the journal's own
/// time base (no tick required first).
#[test]
fn applied_envelopes_advance_the_render_clock() {
    let model = live_session_with_chip();
    assert_eq!(model.clock_ms, SPAWN_MS);
}

/// S4 gate law: ANY live chip keeps the anim clock running on the session
/// screen — the elapsed figure must tick even when no glyph pulses.
///
/// MUTATION CHECK (S4): drop the `tree_live_count` arm from
/// `AppModel::animated`. Expected runtime failure: a streaming parent
/// with an idle child stops animating below (streaming does not pulse,
/// idle chips are outside the pulse set).
#[test]
fn a_live_chip_keeps_the_anim_clock_running() {
    let mut model = live_session_with_chip();
    assert_eq!(model.screen, Screen::Session);
    // A non-pulsing badge: STREAMING neither pulses nor is overlaid by
    // the derived WAITING badge (that overlay rides IDLE only).
    model.route_raw(&raw(
        2,
        None,
        SPAWN_MS + 1,
        &EventPayload::RunState(haider_protocol::state::RunState::Streaming),
    ));
    assert!(
        model.animated(),
        "a live idle chip must tick its elapsed figure"
    );
    // Terminal chips freeze — and the gate closes with them.
    model.route_raw(&raw(
        3,
        Some(CHILD_A),
        SPAWN_MS + 2,
        &EventPayload::AgentChipState {
            agent: AgentId::new(CHILD_A),
            chip: ChipState::Done,
        },
    ));
    model.route_raw(&raw(
        4,
        None,
        SPAWN_MS + 3,
        &EventPayload::RunState(haider_protocol::state::RunState::Done),
    ));
    assert!(!model.animated(), "a frozen tree takes no periodic wakeups");
}

// -------------------------------------------------- frozen-final law ----

/// MUTATION CHECK (S4): keep terminal chips on the live formula
/// (`clock − spawn`). Expected runtime failure: the figure below moves
/// when the clock advances past the terminal envelope.
#[test]
fn terminal_chip_elapsed_freezes_at_the_terminal_envelope() {
    let mut model = live_session_with_chip();
    model.route_raw(&raw(
        2,
        Some(CHILD_A),
        SPAWN_MS + 10_000,
        &EventPayload::AgentChipState {
            agent: AgentId::new(CHILD_A),
            chip: ChipState::Streaming,
        },
    ));
    model.route_raw(&raw(
        3,
        Some(CHILD_A),
        SPAWN_MS + RUN_MS,
        &EventPayload::AgentChipState {
            agent: AgentId::new(CHILD_A),
            chip: ChipState::Done,
        },
    ));
    // The render clock races an hour ahead: the figure must not follow.
    model.clock_ms = SPAWN_MS + RUN_MS + 3_600_000;
    let row = chip_row(&draw_rows(&model, 118, 36));
    assert!(
        row.contains("25m 18s"),
        "frozen at spawn → terminal envelope: {row:?}"
    );
    assert!(
        !row.contains("1h "),
        "the render clock never leaks into a frozen figure: {row:?}"
    );
}

/// MUTATION CHECK (S4): let `note_event_at` keep advancing after the
/// terminal transition. Expected runtime failure: the post-Done report
/// envelope below moves the frozen final.
#[test]
fn later_envelopes_never_move_a_frozen_final() {
    let mut model = live_session_with_chip();
    model.route_raw(&raw(
        2,
        Some(CHILD_A),
        SPAWN_MS + RUN_MS,
        &EventPayload::AgentChipState {
            agent: AgentId::new(CHILD_A),
            chip: ChipState::Done,
        },
    ));
    // A report five minutes later (collection lag) — the measure is the
    // WORK, spawn → terminal, not spawn → paperwork.
    model.route_raw(&raw(
        3,
        Some(CHILD_A),
        SPAWN_MS + RUN_MS + 300_000,
        &EventPayload::AgentReport(haider_protocol::agent::ChildReport {
            agent: AgentId::new(CHILD_A),
            summary: "done".into(),
            verified: haider_protocol::agent::ReportVerification::Unverified,
            workspace_revision: None,
        }),
    ));
    model.clock_ms = SPAWN_MS + RUN_MS + 600_000;
    let row = chip_row(&draw_rows(&model, 118, 36));
    assert!(row.contains("25m 18s"), "the final stays frozen: {row:?}");
    assert!(!row.contains("30m 18s"), "paperwork never counts: {row:?}");
}

/// The chip-level clock unit laws: monotone max while live, refused once
/// terminal, and `elapsed_ms` picks the honest formula per state.
#[test]
fn chip_clock_is_monotone_and_stops_at_terminal() {
    let mut chip = ChipModel::from_manifest(&manifest(CHILD_A, "stitch", None));
    chip.spawned_at_ms = Some(100);
    chip.note_event_at(500);
    chip.note_event_at(300);
    assert_eq!(chip.last_event_at_ms, Some(500), "monotone max, no rewind");
    chip.set_state_at(ChipDisplayState::Done, 700);
    assert_eq!(chip.elapsed_ms(9_999), Some(600), "frozen at the terminal");
    chip.note_event_at(2_000);
    assert_eq!(
        chip.elapsed_ms(9_999),
        Some(600),
        "the stopped clock refuses"
    );
    // No spawn instant → no figure, never a guess.
    let bare = ChipModel::from_manifest(&manifest(CHILD_B, "bare", None));
    assert_eq!(bare.elapsed_ms(9_999), None);
}

// -------------------------------------------------------- token join ----

fn summary(session_id: &str, head_seq: u64, tokens: u64) -> haider_rpc::SessionSummary {
    haider_rpc::SessionSummary {
        session_id: SessionId::new(session_id),
        head_seq,
        worker_generation: 7,
        metadata: None,
        turn_count: Some(1),
        footprint_tokens: Some(tokens),
        footprint_truth: Some(ContextFootprintTruth::Exact),
    }
}

/// MUTATION CHECK (S4, join-correctness law): join positionally (first
/// summary row) or by callsign. Expected runtime failure: chip B below
/// wears chip A's 265.9k figure.
#[test]
fn tokens_join_by_the_chips_own_child_session_id() {
    let mut model = live_session_with_chip();
    model.route_raw(&raw(
        2,
        None,
        SPAWN_MS + 1,
        &EventPayload::AgentSpawned(manifest(CHILD_B, "verify", Some(CHILD_B_SESSION))),
    ));
    // The roster knows both child sessions (session.list lists children —
    // they are full sessions), each with ITS OWN footprint truth.
    for (child, tokens) in [(CHILD_A_SESSION, 265_900), (CHILD_B_SESSION, 1_234)] {
        model.upsert_live_session(&SessionId::new(child));
        model.note_summary_counts(&summary(child, 9, tokens));
    }
    model.clock_ms = SPAWN_MS + RUN_MS;
    let rows = draw_rows(&model, 118, 36);
    let row_a = rows
        .iter()
        .find(|row| row.contains("stitch"))
        .expect("chip A row");
    let row_b = rows
        .iter()
        .find(|row| row.contains("verify"))
        .expect("chip B row");
    assert!(
        row_a.contains("↓ 266k tokens"),
        "chip A wears its own total: {row_a:?}"
    );
    assert!(
        row_b.contains("↓ 1.2k tokens"),
        "chip B wears its own total: {row_b:?}"
    );
    assert!(
        !row_b.contains("266k"),
        "a wrong child never wears another child's tokens: {row_b:?}"
    );
}

/// MUTATION CHECK (S4, honesty law): render `↓ 0 tokens` when no source
/// has truth. Expected runtime failure: the row below grows a token
/// segment although every source is empty.
#[test]
fn unknown_tokens_render_no_token_segment() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    // No coordinates → no join; live streams feed no chip counter.
    model.route_raw(&raw(
        1,
        None,
        SPAWN_MS,
        &EventPayload::AgentSpawned(manifest(CHILD_A, "stitch", None)),
    ));
    model.clock_ms = SPAWN_MS + RUN_MS;
    let row = chip_row(&draw_rows(&model, 118, 36));
    assert!(
        row.contains("25m 18s"),
        "the elapsed segment still renders alone: {row:?}"
    );
    assert!(
        !row.contains('↓') && !row.contains("tokens"),
        "unknown is never rendered as zero: {row:?}"
    );
}

/// The truth chain: the chip transcript's own footprint outranks the
/// demo counter, the counter outranks the roster join, and the join only
/// speaks when the row actually knows (summary or applied usage).
#[test]
fn chip_row_tokens_is_truth_ordered() {
    let mut model = live_session_with_chip();
    model.upsert_live_session(&SessionId::new(CHILD_A_SESSION));
    model.note_summary_counts(&summary(CHILD_A_SESSION, 9, 55_000));
    // Join only: the summary speaks.
    let chip = model.chips.first().expect("chip");
    assert_eq!(chip_row_tokens(&model.sessions, chip), Some(55_000));
    // The demo counter outranks the join…
    let chip = model.chips.first_mut().expect("chip");
    chip.tokens = 2_000;
    let chip = model.chips.first().expect("chip");
    assert_eq!(chip_row_tokens(&model.sessions, chip), Some(2_000));
    // …and a chip-scoped durable footprint outranks everything.
    let footprint = ContextFootprint {
        input_tokens: 900,
        output_tokens: 50,
        cached_input_tokens: 50,
        used_tokens: 1_000,
        context_window: None,
        reserved_output_tokens: 0,
        soft_threshold_tokens: None,
        estimated_turns_to_threshold: None,
        truth: ContextFootprintTruth::Exact,
    };
    let item = footprint.extension_item().expect("extension item");
    let chip = model.chips.first_mut().expect("chip");
    chip.transcript
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("s4-footprint"),
            item,
        }));
    let chip = model.chips.first().expect("chip");
    assert_eq!(chip_row_tokens(&model.sessions, chip), Some(1_000));
    // A chip with NO source anywhere says nothing.
    let bare = ChipModel::from_manifest(&manifest(CHILD_B, "bare", None));
    assert_eq!(chip_row_tokens(&model.sessions, &bare), None);
}

// -------------------------------------------------- width degradation ----

/// MUTATION CHECK (S4, F2c law): truncate the meta to the budget instead
/// of dropping whole segments. Expected runtime failure: a narrow frame
/// below carries a cut fragment (`↓ 26`) instead of elapsed-only, and
/// the narrowest carries a cut elapsed instead of nothing.
#[test]
fn width_degradation_drops_tokens_first_then_elapsed_whole() {
    let mut model = live_session_with_chip();
    model.upsert_live_session(&SessionId::new(CHILD_A_SESSION));
    model.note_summary_counts(&summary(CHILD_A_SESSION, 9, 265_900));
    model.clock_ms = SPAWN_MS + RUN_MS;
    // Wide: both segments, in the owner's order.
    let row = chip_row(&draw_rows(&model, 118, 36));
    assert!(
        row.contains("25m 18s · ↓ 266k tokens"),
        "the full meta at width: {row:?}"
    );
    // Narrow: the token segment yields WHOLE — elapsed survives alone.
    let row = chip_row(&draw_rows(&model, 76, 36));
    assert!(
        row.contains("25m 18s"),
        "elapsed survives the first drop: {row:?}"
    );
    assert!(
        !row.contains('↓') && !row.contains("tokens") && !row.contains("266"),
        "tokens dropped whole, never truncated: {row:?}"
    );
    // Narrower: elapsed yields too — the row carries no meta fragment.
    let row = chip_row(&draw_rows(&model, 62, 36));
    assert!(
        !row.contains("25m") && !row.contains("18s") && !row.contains('↓'),
        "both segments gone whole below budget: {row:?}"
    );
}

// ------------------------------------------------------------- style ----

/// The meta rides the DIM theme slot (owner directive) — the row's right
/// end wears `theme.dim` ink, not the bright/gold identity inks.
#[test]
fn the_meta_wears_the_dim_slot() {
    let mut model = live_session_with_chip();
    model.clock_ms = SPAWN_MS + RUN_MS;
    let backend = TestBackend::new(118, 36);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(&model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let theme = model.theme.theme();
    let row_y = (0..buffer.area.height)
        .find(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains("Ammar (r)")
        })
        .expect("chip row");
    let row: String = (0..buffer.area.width)
        .map(|x| buffer[(x, row_y)].symbol())
        .collect();
    let meta_start = row.find("25m 18s").expect("meta rendered");
    // Cell column, not byte offset — the connectors before the meta are
    // multi-byte glyphs (every symbol in this row is single-WIDTH).
    let x = u16::try_from(row[..meta_start].chars().count()).expect("column fits");
    assert_eq!(
        buffer[(x, row_y)].style().fg,
        Some(theme.dim.into()),
        "the meta ink is the dim slot"
    );
}
