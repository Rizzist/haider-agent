//! Lane `monitorui` (v0.0.970) — monitors in the band's task line,
//! clickable, controllable, agent-editable.
//!
//! Owner direction (2026-09-03): "it should render to the right of the
//! subagents line (x shells / x monitors) like Claude Code. fix that. …
//! I should be able to click a monitor, and shut it down etc. and I should
//! even be able to tell the AI agent to edit the monitor and what it's
//! monitoring."
//!
//! These pins cover the four deliverables: the band-row counts and their
//! click targets, the overlay's row actions and keyboard parity, the fired
//! monitor's ambient note plus `firing` chip, and the `/monitors` control
//! subcommands. The band's PIXELS are pinned by the `tuivirt` goldens; this
//! file pins the behaviour behind them.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod tuivirt_common;

use haider_protocol::EventPayload;
use haider_protocol::ids::SessionId;
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::taskrows::{BandCountKind, band_counts, band_counts_text};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tuivirt_common::{SIZES, draw, session_model};

// ---------------------------------------------------------------- fixtures --

fn monitor_session() -> AppModel {
    let mut model = session_model();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Session;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_MONITOR_CONTROL_V1.into());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SHELL_REGISTRY_V1.into());
    model.requests.clear();
    model
}

fn shell(id: &str) -> haider_rpc::ShellWire {
    haider_rpc::ShellWire {
        id: id.into(),
        kind: haider_rpc::ShellKindWire::Local,
        status: haider_rpc::ShellStatusWire::Running,
        title: "tests".into(),
        cwd_or_host: "/workspace".into(),
        created_at_ms: 1,
        last_activity_ms: 2,
        bytes_out: 3,
    }
}

pub fn monitor(
    id: &str,
    source: haider_rpc::MonitorSourceWire,
    state: haider_rpc::MonitorStateWire,
) -> haider_rpc::MonitorRegistrationWire {
    haider_rpc::MonitorRegistrationWire {
        monitor_id: id.to_owned(),
        session_id: SessionId::new("tuivirt-session"),
        branch_id: None,
        agent_id: None,
        source,
        filter: None,
        action: haider_rpc::MonitorActionWire {
            report: true,
            follow_up: None,
        },
        occurrence: haider_rpc::MonitorOccurrenceWire::Every,
        created_at_ms: 1_000,
        start_source_sequence: 0,
        expires_at_ms: None,
        state,
        last_event: None,
        fire_count: 0,
        next_fire_at_ms: None,
        source_summary: String::new(),
    }
}

fn timer(id: &str, state: haider_rpc::MonitorStateWire) -> haider_rpc::MonitorRegistrationWire {
    monitor(
        id,
        haider_rpc::MonitorSourceWire::Timer {
            interval_ms: 60_000,
        },
        state,
    )
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(key(code)));
}

/// Type a slash line and send it, the way the shared `common::submit`
/// helper does — the public key path, never a private reducer entry.
fn submit(model: &mut AppModel, text: &str) {
    for c in text.chars() {
        model.handle(AppEvent::Key(key(KeyCode::Char(c))));
    }
    model.handle(AppEvent::Key(key(KeyCode::Enter)));
}

// ------------------------------------------------- item 1: the task line --

#[test]
fn band_counts_omit_zero_and_pluralise_one_and_many_separately() {
    // The pluralisation contract the retired status segment used to own.
    assert!(band_counts(0, 0).is_empty());
    assert_eq!(
        band_counts_text(&band_counts(1, 1)),
        "· 1 shell · 1 monitor"
    );
    assert_eq!(
        band_counts_text(&band_counts(2, 3)),
        "· 2 shells · 3 monitors"
    );
    // Each half is omitted on its own, never rendered as `· 0 shells`.
    assert_eq!(band_counts_text(&band_counts(0, 2)), "· 2 monitors");
    assert_eq!(band_counts_text(&band_counts(4, 0)), "· 4 shells");
}

#[test]
fn band_counts_carry_their_own_overlay_kind() {
    let counts = band_counts(1, 1);
    assert_eq!(counts[0].kind, BandCountKind::Shells);
    assert_eq!(counts[1].kind, BandCountKind::Monitors);
}

#[test]
fn live_shell_count_ignores_exited_and_closed_shells() {
    let mut model = monitor_session();
    model.shells.push(shell("sh-run"));
    let mut exited = shell("sh-done");
    exited.status = haider_rpc::ShellStatusWire::Exited { code: Some(0) };
    model.shells.push(exited);
    let mut closed = shell("sh-shut");
    closed.status = haider_rpc::ShellStatusWire::Closed;
    model.shells.push(closed);
    // Three rows in the registry, ONE of them live — the band counts the
    // live ones, exactly as the retired segment did.
    assert_eq!(model.shells.len(), 3);
    assert_eq!(model.live_shell_count(), 1);
    assert_eq!(band_counts_text(&model.band_counts()), "· 1 shell");
}

#[test]
fn the_band_row_carries_the_counts_right_aligned_at_every_width() {
    let mut model = monitor_session();
    model.shells.push(shell("sh-one"));
    model.shells.push(shell("sh-two"));
    model.monitor_count = 1;
    for (width, height) in SIZES {
        let frame = draw(&model, width, height);
        let row = frame
            .row_containing("· 2 shells")
            .expect("the band row carries the counts");
        let text = &frame.rows[row];
        assert!(
            text.contains("· 2 shells · 1 monitor"),
            "counts must read as one run at {width}x{height}: {text}"
        );
        // Right-aligned: the run ends at the row's last non-blank cell.
        assert_eq!(
            text.trim_end().chars().count(),
            text.trim_end().len().min(text.trim_end().chars().count()),
            "row must not be padded past its counts at {width}x{height}"
        );
        assert!(
            text.trim_end().ends_with("· 1 monitor"),
            "counts must be RIGHT-aligned at {width}x{height}: {text}"
        );
    }
}

#[test]
fn each_band_count_is_its_own_click_target() {
    let mut model = monitor_session();
    model.shells.push(shell("sh-one"));
    model.monitor_count = 2;
    for (width, height) in SIZES {
        let frame = draw(&model, width, height);
        let (shell_rect, _) = frame
            .find_hit(|hit| matches!(hit, Hit::ShellStatus))
            .unwrap_or_else(|| panic!("shell count clickable at {width}x{height}"));
        let (monitor_rect, _) = frame
            .find_hit(|hit| matches!(hit, Hit::MonitorStatus))
            .unwrap_or_else(|| panic!("monitor count clickable at {width}x{height}"));
        // Same row, disjoint, and in reading order — one click can never
        // mean both counts.
        assert_eq!(shell_rect.y, monitor_rect.y);
        assert!(
            shell_rect.x + shell_rect.width <= monitor_rect.x,
            "count rects overlap at {width}x{height}"
        );
    }
}

#[test]
fn the_counts_row_stands_alone_when_nothing_is_delegated() {
    // No subagents at all: the band row still owes the counts, because the
    // status-bar segment that used to carry them is gone.
    let mut model = monitor_session();
    model.monitor_count = 1;
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("· 1 monitor"));
    assert!(
        frame.has_hit(|hit| matches!(hit, Hit::MonitorStatus)),
        "the lone counts row stays clickable"
    );
    // …and with nothing running the row collapses entirely, as before.
    model.monitor_count = 0;
    let quiet = draw(&model, 118, 36);
    assert!(!quiet.contains("monitor"));
    assert!(!quiet.contains("▾ subagents"));
}

#[test]
fn the_band_counts_click_through_to_their_overlays() {
    let mut model = monitor_session();
    model.shells.push(shell("sh-one"));
    model.monitor_count = 1;

    model.handle_hit(Hit::MonitorStatus);
    assert!(model.monitors_open);
    assert!(!model.shells_open);
    assert!(matches!(
        model.requests.last(),
        Some(AppRequest::MonitorList)
    ));

    model.monitors_open = false;
    model.handle_hit(Hit::ShellStatus);
    assert!(model.shells_open);
    assert!(!model.monitors_open);
}

// ------------------------------------------- item 2: overlay row actions --

#[test]
fn every_row_action_is_a_distinct_click_target_on_the_selected_row() {
    let mut model = monitor_session();
    model.monitors = vec![
        timer("mon-a", haider_rpc::MonitorStateWire::Armed),
        timer("mon-b", haider_rpc::MonitorStateWire::Paused),
    ];
    model.monitor_count = 2;
    model.monitors_open = true;
    let frame = draw(&model, 118, 36);

    // Both rows are selectable by id.
    for id in ["mon-a", "mon-b"] {
        assert!(
            frame.has_hit(|hit| matches!(hit, Hit::MonitorRow(row) if row == id)),
            "row {id} must be clickable"
        );
    }
    // The SELECTED row (cursor 0) carries all five actions, each for mon-a.
    assert!(frame.has_hit(|hit| matches!(hit, Hit::MonitorStop(id) if id == "mon-a")));
    assert!(frame.has_hit(|hit| matches!(hit, Hit::MonitorPause(id) if id == "mon-a")));
    assert!(frame.has_hit(|hit| matches!(hit, Hit::MonitorTrigger(id) if id == "mon-a")));
    assert!(frame.has_hit(|hit| matches!(hit, Hit::MonitorEdit(id) if id == "mon-a")));
    assert!(frame.has_hit(|hit| matches!(hit, Hit::MonitorCopyId(id) if id == "mon-a")));
    // The UNSELECTED row carries none of them.
    assert!(!frame.has_hit(|hit| matches!(hit, Hit::MonitorStop(id) if id == "mon-b")));
}

#[test]
fn an_action_rect_wins_over_the_row_rect_that_contains_it() {
    // `hit_rect_at` takes the FIRST containing rect, so the action targets
    // must be pushed ahead of the row-select rect they sit inside.
    let mut model = monitor_session();
    model.monitors = vec![timer("mon-a", haider_rpc::MonitorStateWire::Armed)];
    model.monitor_count = 1;
    model.monitors_open = true;
    let frame = draw(&model, 118, 36);
    let stop = frame
        .hits
        .iter()
        .position(|(_, hit)| matches!(hit, Hit::MonitorStop(_)))
        .expect("stop target");
    let row = frame
        .hits
        .iter()
        .position(|(_, hit)| matches!(hit, Hit::MonitorRow(_)))
        .expect("row target");
    assert!(
        stop < row,
        "the stop rect must precede the row rect in the hit map"
    );
}

#[test]
fn clicking_a_row_selects_it_without_acting_on_it() {
    let mut model = monitor_session();
    model.monitors = vec![
        timer("mon-a", haider_rpc::MonitorStateWire::Armed),
        timer("mon-b", haider_rpc::MonitorStateWire::Armed),
    ];
    model.monitor_count = 2;
    model.monitors_open = true;
    model.requests.clear();

    model.handle_hit(Hit::MonitorRow("mon-b".to_owned()));
    assert_eq!(model.monitors_cursor, 1);
    assert_eq!(model.monitors_selected_id().as_deref(), Some("mon-b"));
    assert!(model.requests.is_empty(), "selection is not an action");
    assert!(model.monitors_open, "selecting must not close the overlay");
}

#[test]
fn stop_is_armed_then_confirmed_and_only_then_leaves_as_a_request() {
    let mut model = monitor_session();
    model.monitors = vec![timer("mon-a", haider_rpc::MonitorStateWire::Armed)];
    model.monitor_count = 1;
    model.monitors_open = true;
    model.requests.clear();

    model.handle_hit(Hit::MonitorStop("mon-a".to_owned()));
    assert!(model.requests.is_empty(), "the first stop only arms");
    assert_eq!(model.monitors_stop_armed.as_deref(), Some("mon-a"));

    model.handle_hit(Hit::MonitorStop("mon-a".to_owned()));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorRemove { monitor_id }] if monitor_id == "mon-a"
    ));
    assert!(model.monitors_stop_armed.is_none());
}

#[test]
fn pause_and_resume_are_chosen_by_the_rows_own_state() {
    let mut model = monitor_session();
    model.monitors = vec![
        timer("mon-armed", haider_rpc::MonitorStateWire::Armed),
        timer("mon-paused", haider_rpc::MonitorStateWire::Paused),
    ];
    model.monitor_count = 2;
    model.monitors_open = true;
    model.requests.clear();

    model.handle_hit(Hit::MonitorPause("mon-armed".to_owned()));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorPause { monitor_id }] if monitor_id == "mon-armed"
    ));

    model.requests.clear();
    model.handle_hit(Hit::MonitorPause("mon-paused".to_owned()));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorResume { monitor_id }] if monitor_id == "mon-paused"
    ));
}

#[test]
fn trigger_and_copy_id_leave_as_their_own_requests() {
    let mut model = monitor_session();
    model.monitors = vec![timer("mon-a", haider_rpc::MonitorStateWire::Armed)];
    model.monitor_count = 1;
    model.monitors_open = true;
    model.requests.clear();

    model.handle_hit(Hit::MonitorTrigger("mon-a".to_owned()));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorTrigger { monitor_id }] if monitor_id == "mon-a"
    ));

    model.requests.clear();
    model.handle_hit(Hit::MonitorCopyId("mon-a".to_owned()));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::CopyText(text)] if text == "mon-a"
    ));
}

#[test]
fn edit_with_agent_prefills_the_composer_and_closes_the_overlay() {
    let mut model = monitor_session();
    model.monitors = vec![timer("gh-ci-42", haider_rpc::MonitorStateWire::Armed)];
    model.monitor_count = 1;
    model.monitors_open = true;
    model.requests.clear();

    model.handle_hit(Hit::MonitorEdit("gh-ci-42".to_owned()));
    // THE prefill text, pinned exactly: the user describes the change in
    // prose after the colon and the AGENT calls monitor.update.
    assert_eq!(model.composer.text(), "/monitor edit gh-ci-42: ");
    assert_eq!(
        AppModel::monitor_edit_prefill("gh-ci-42"),
        "/monitor edit gh-ci-42: "
    );
    // The cursor waits at the end, ready for prose.
    assert_eq!(model.composer.cursor(), model.composer.text().len());
    assert!(!model.monitors_open, "the overlay yields to the composer");
    assert!(
        model.requests.is_empty(),
        "an agent edit sends no control RPC of its own"
    );
}

#[test]
fn the_overlay_swallows_hits_that_are_not_its_own() {
    let mut model = monitor_session();
    model.monitors_open = true;
    model.subtree_collapsed = false;
    model.handle_hit(Hit::SubTreeToggle);
    assert!(
        model.monitors_open,
        "a covered frame's hit must not act through the overlay"
    );
    assert!(!model.subtree_collapsed);
}

// -------------------------------------------------- item 2: the keyboard --

#[test]
fn keyboard_parity_selects_and_acts_exactly_as_the_clicks_do() {
    let mut model = monitor_session();
    model.monitors = vec![
        timer("mon-a", haider_rpc::MonitorStateWire::Armed),
        timer("mon-b", haider_rpc::MonitorStateWire::Paused),
    ];
    model.monitor_count = 2;
    model.monitors_open = true;
    model.requests.clear();

    // j/k move and clamp at both ends.
    press(&mut model, KeyCode::Char('j'));
    assert_eq!(model.monitors_cursor, 1);
    press(&mut model, KeyCode::Char('j'));
    assert_eq!(model.monitors_cursor, 1, "cursor clamps at the last row");
    press(&mut model, KeyCode::Char('k'));
    assert_eq!(model.monitors_cursor, 0);
    press(&mut model, KeyCode::Char('k'));
    assert_eq!(model.monitors_cursor, 0, "cursor clamps at the first row");

    // t triggers the SELECTED row.
    press(&mut model, KeyCode::Char('t'));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorTrigger { monitor_id }] if monitor_id == "mon-a"
    ));

    // p pauses an armed row; on the paused row it resumes.
    model.requests.clear();
    press(&mut model, KeyCode::Char('p'));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorPause { monitor_id }] if monitor_id == "mon-a"
    ));
    model.requests.clear();
    press(&mut model, KeyCode::Char('j'));
    press(&mut model, KeyCode::Char('p'));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorResume { monitor_id }] if monitor_id == "mon-b"
    ));

    // y copies the selected id.
    model.requests.clear();
    press(&mut model, KeyCode::Char('y'));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::CopyText(text)] if text == "mon-b"
    ));
}

#[test]
fn the_x_key_arms_before_it_stops_and_a_cursor_move_disarms() {
    let mut model = monitor_session();
    model.monitors = vec![
        timer("mon-a", haider_rpc::MonitorStateWire::Armed),
        timer("mon-b", haider_rpc::MonitorStateWire::Armed),
    ];
    model.monitor_count = 2;
    model.monitors_open = true;
    model.requests.clear();

    press(&mut model, KeyCode::Char('x'));
    assert!(model.requests.is_empty());
    assert_eq!(model.monitors_stop_armed.as_deref(), Some("mon-a"));

    // Moving away disarms, so a second `x` elsewhere cannot stop the row
    // the user armed.
    press(&mut model, KeyCode::Char('j'));
    assert!(model.monitors_stop_armed.is_none());
    press(&mut model, KeyCode::Char('x'));
    assert!(model.requests.is_empty());
    press(&mut model, KeyCode::Char('x'));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorRemove { monitor_id }] if monitor_id == "mon-b"
    ));
}

#[test]
fn the_e_key_hands_the_edit_to_the_agent_and_esc_closes() {
    let mut model = monitor_session();
    model.monitors = vec![timer("mon-a", haider_rpc::MonitorStateWire::Armed)];
    model.monitor_count = 1;
    model.monitors_open = true;

    press(&mut model, KeyCode::Char('e'));
    assert_eq!(model.composer.text(), "/monitor edit mon-a: ");
    assert!(!model.monitors_open);

    model.monitors_open = true;
    press(&mut model, KeyCode::Esc);
    assert!(!model.monitors_open);
}

// ------------------------------------------ item 3: a fired monitor's note --

fn report(monitor_id: &str, cursor: u64) -> haider_rpc::MonitorDeliveryReportWire {
    haider_rpc::MonitorDeliveryReportWire {
        report_id: format!("rep-{monitor_id}"),
        monitor_id: monitor_id.to_owned(),
        session_id: SessionId::new("tuivirt-session"),
        branch_id: None,
        agent_id: None,
        source: haider_rpc::MonitorSourceKindWire::Timer,
        status: haider_rpc::MonitorReportStatusWire::Matched,
        events: vec![haider_rpc::MonitorEventWire {
            sequence: 1,
            observed_at_ms: 5_000,
            payload: haider_rpc::MonitorEventPayloadWire::Timer {
                tick: 12,
                fired_at_ms: 5_000,
            },
        }],
        coalesced_count: 1,
        omitted_count: 0,
        action: haider_rpc::MonitorActionWire {
            report: true,
            follow_up: None,
        },
        cursor,
        dedupe: haider_rpc::MonitorDeliveryDedupeWire {
            delivery_key: format!("d-{cursor}"),
            report_key: format!("r-{monitor_id}"),
        },
    }
}

#[test]
fn a_fired_monitor_leaves_an_ambient_note_and_never_a_modal() {
    let mut model = monitor_session();
    model.monitors = vec![timer("timer-60s", haider_rpc::MonitorStateWire::Armed)];
    model.monitor_count = 1;
    model.apply_monitor_fired(&report("timer-60s", 7));

    let frame = draw(&model, 118, 36);
    assert!(
        frame.contains("◉ monitor timer-60s fired"),
        "the fire lands as an ambient transcript row"
    );
    // Ambient means ambient: no menu, no overlay, nothing modal opened.
    assert!(!model.monitors_open);
    assert!(model.projection.open_menu().is_none());
}

#[test]
fn a_fired_row_reads_firing_until_the_woken_subturn_completes() {
    let mut model = monitor_session();
    let armed = timer("timer-60s", haider_rpc::MonitorStateWire::Armed);
    model.monitors = vec![armed.clone()];
    model.monitor_count = 1;

    assert_eq!(
        model.monitor_row_state(&armed),
        haider_rpc::MonitorStateWire::Armed
    );

    model.apply_monitor_fired(&report("timer-60s", 7));
    assert_eq!(
        model.monitor_row_state(&model.monitors[0]),
        haider_rpc::MonitorStateWire::Firing,
        "the chip reads firing while the woken subturn runs"
    );
    model.monitors_open = true;
    assert!(draw(&model, 118, 36).contains("[firing]"));

    // The subturn ends → the row falls back to daemon truth.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        haider_protocol::state::RunState::Done,
    ))));
    assert_eq!(
        model.monitor_row_state(&model.monitors[0]),
        haider_rpc::MonitorStateWire::Armed
    );
}

#[test]
fn the_fired_note_names_what_the_monitor_actually_saw() {
    let mut base = report("gh-ci", 3);
    base.events[0].payload = haider_rpc::MonitorEventPayloadWire::Poll {
        payload: "conclusion: success".to_owned(),
    };
    assert_eq!(
        haider_tui::app::monitor_fired_note(&base),
        "◉ monitor gh-ci fired → conclusion: success"
    );
    // A coalesced revision says so rather than pretending it was one event.
    base.coalesced_count = 4;
    assert_eq!(
        haider_tui::app::monitor_fired_note(&base),
        "◉ monitor gh-ci fired · 4 events → conclusion: success"
    );
}

// ---------------------------------------- item 4: the control subcommands --

#[test]
fn monitors_subcommands_stop_pause_and_resume_one_row() {
    let mut model = monitor_session();
    model.monitors = vec![timer("mon-a", haider_rpc::MonitorStateWire::Armed)];
    model.monitor_count = 1;

    for (line, expected) in [
        ("/monitors stop mon-a", "stop"),
        ("/monitors pause mon-a", "pause"),
        ("/monitors resume mon-a", "resume"),
    ] {
        model.requests.clear();
        submit(&mut model, line);
        let matched = match (expected, model.requests.as_slice()) {
            ("stop", [AppRequest::MonitorRemove { monitor_id }])
            | ("pause", [AppRequest::MonitorPause { monitor_id }])
            | ("resume", [AppRequest::MonitorResume { monitor_id }]) => monitor_id == "mon-a",
            _ => false,
        };
        assert!(matched, "`{line}` must emit the {expected} request");
        // A subcommand acts on the row; it does not open the overlay.
        assert!(!model.monitors_open, "`{line}` must not open the overlay");
    }
}

#[test]
fn a_bare_monitors_command_still_opens_the_overlay_against_fresh_truth() {
    let mut model = monitor_session();
    model.requests.clear();
    submit(&mut model, "/monitors");
    assert!(model.monitors_open);
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::MonitorList]
    ));
}

#[test]
fn a_subcommand_without_an_id_says_what_it_wanted() {
    let mut model = monitor_session();
    model.requests.clear();
    submit(&mut model, "/monitors stop");
    assert!(model.requests.is_empty());
    assert!(!model.monitors_open);
    assert_eq!(
        model.flash.as_deref(),
        Some("· /monitors [stop|pause|resume <id>]")
    );
}

// -------------------------------------------------- receipts and summaries --

#[test]
fn a_remove_receipt_drops_the_row_and_clamps_the_cursor() {
    let mut model = monitor_session();
    model.monitors = vec![
        timer("mon-a", haider_rpc::MonitorStateWire::Armed),
        timer("mon-b", haider_rpc::MonitorStateWire::Armed),
    ];
    model.monitor_count = 2;
    model.monitors_cursor = 1;

    model.apply_monitor_remove(haider_rpc::MonitorRemoveReceiptWire {
        command_id: haider_rpc::CommandId::new("c-1"),
        session_id: SessionId::new("tuivirt-session"),
        worker_generation: 1,
        policy: policy(),
        sources: Vec::new(),
        outcome: haider_rpc::MonitorRemoveOutcomeWire::Removed {
            monitor_id: "mon-b".to_owned(),
        },
    });
    assert_eq!(model.monitors.len(), 1);
    assert_eq!(model.monitor_count, 1);
    assert_eq!(model.monitors_cursor, 0, "the cursor cannot dangle");
}

#[test]
fn a_pause_receipt_replaces_the_row_with_daemon_truth() {
    let mut model = monitor_session();
    model.monitors = vec![timer("mon-a", haider_rpc::MonitorStateWire::Armed)];
    model.monitor_count = 1;
    // A local `firing` overlay must yield to a real receipt.
    model.apply_monitor_fired(&report("mon-a", 1));

    let paused = timer("mon-a", haider_rpc::MonitorStateWire::Paused);
    model.apply_monitor_mutate(haider_rpc::MonitorMutateReceiptWire {
        command_id: haider_rpc::CommandId::new("c-2"),
        session_id: SessionId::new("tuivirt-session"),
        worker_generation: 1,
        policy: policy(),
        sources: Vec::new(),
        outcome: haider_rpc::MonitorMutateOutcomeWire::Paused { monitor: paused },
    });
    assert_eq!(
        model.monitor_row_state(&model.monitors[0]),
        haider_rpc::MonitorStateWire::Paused
    );
}

#[test]
fn a_refusal_is_worded_never_dumped() {
    let mut model = monitor_session();
    model.apply_monitor_mutate(haider_rpc::MonitorMutateReceiptWire {
        command_id: haider_rpc::CommandId::new("c-3"),
        session_id: SessionId::new("tuivirt-session"),
        worker_generation: 1,
        policy: policy(),
        sources: Vec::new(),
        outcome: haider_rpc::MonitorMutateOutcomeWire::Rejected {
            rejection: haider_rpc::MonitorControlRejectionWire::NotFound {
                monitor_id: "ghost".to_owned(),
            },
        },
    });
    assert_eq!(model.flash.as_deref(), Some("· no monitor ghost"));
}

#[test]
fn the_source_summary_says_what_each_kind_watches() {
    let timer_row = timer("t", haider_rpc::MonitorStateWire::Armed);
    assert_eq!(
        haider_tui::app::monitor_source_summary(&timer_row),
        "timer 60s"
    );

    let file_row = monitor(
        "f",
        haider_rpc::MonitorSourceWire::File {
            path: "src/x.rs".to_owned(),
        },
        haider_rpc::MonitorStateWire::Armed,
    );
    assert_eq!(
        haider_tui::app::monitor_source_summary(&file_row),
        "file src/x.rs"
    );

    let poll_row = monitor(
        "p",
        haider_rpc::MonitorSourceWire::Poll {
            command: "gh run 123".to_owned(),
            interval_ms: 30_000,
            until: haider_rpc::MonitorPollUntilWire::StdoutChanged,
            cwd: None,
            env_passthrough: Vec::new(),
        },
        haider_rpc::MonitorStateWire::Armed,
    );
    assert_eq!(
        haider_tui::app::monitor_source_summary(&poll_row),
        "poll gh run 123 · 30s · until changed"
    );

    let cli_row = monitor(
        "c",
        haider_rpc::MonitorSourceWire::Cli {
            preset: haider_rpc::MonitorCliPresetWire::Codex,
            argv: vec!["exec".to_owned(), "--full-auto".to_owned()],
            env_passthrough: Vec::new(),
            cwd: None,
            interval_ms: None,
        },
        haider_rpc::MonitorStateWire::Armed,
    );
    assert_eq!(
        haider_tui::app::monitor_source_summary(&cli_row),
        "cli codex exec --full-auto"
    );

    // The daemon's OWN summary wins when it sent one.
    let mut daemon_said = timer_row.clone();
    daemon_said.source_summary = "every minute, on the minute".to_owned();
    assert_eq!(
        haider_tui::app::monitor_source_summary(&daemon_said),
        "every minute, on the minute"
    );
}

fn policy() -> haider_rpc::MonitorControlPolicyWire {
    use haider_rpc::Capability;
    haider_rpc::MonitorControlPolicyWire {
        list: Capability::View,
        register: Capability::Control,
        register_requires_control_attachment: true,
        remove: Capability::Control,
        remove_requires_control_attachment: true,
        update: Capability::Control,
        update_requires_control_attachment: true,
        pause: Capability::Control,
        pause_requires_control_attachment: true,
        resume: Capability::Control,
        resume_requires_control_attachment: true,
        trigger: Capability::Control,
        trigger_requires_control_attachment: true,
        watch: Capability::View,
    }
}
