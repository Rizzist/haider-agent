//! The transcript-tail `● thinking…` indicator is up for the WHOLE running
//! turn, not just the THINKING beat.
//!
//! Owner report (with a screenshot): the badge read `▮ STREAMING` with the
//! model plainly producing text, and NOTHING sat above the composer. The tail
//! was gated on `is_thinking()`, so it blanked the instant the run left
//! `Thinking` — `Streaming`, `RunningTool`, `Concluding`, `Verifying` and
//! `Compacting` all rendered nothing. Requirement: "even if its streaming, as
//! long as its not in idle/waiting it should be there".
//!
//! The laws pinned here:
//! * The indicator shows for the Active / Tool / Compacting badge groups.
//! * It stays DARK for idle/terminal, for the waiting family (the owner
//!   excluded waiting), and for the blocked-on-user family (a menu is on
//!   screen).
//! * `Retrying` never doubles up: it owns the dedicated retry tail row, and
//!   must never render alongside a second indicator.
//! * The shared animation clock tracks the render gate, so the row BREATHES
//!   for the whole run instead of painting frozen.
//! * The chip/sim surface obeys the same law, so the demo cannot drift from
//!   the real UI.

#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::MenuId;
use haider_protocol::state::{RunState, VerifyStep, WaitReason};
use haider_tui::app::{AppEvent, AppModel};
use haider_tui::projection::SessionProjection;
use haider_tui::render::render;
use haider_tui::script::ChipDisplayState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{hit_session_named, launcher_model};

/// The rendered tail row, at `anim_phase == 0` (the `●` half of the ● ↔ ◌
/// breath). The verb itself is fixed by law — the owner asked to read
/// "thinking", whatever beat the run is actually in.
const INDICATOR: &str = "● thinking…";

/// EVERY `RunState` variant, once. Kept beside the exhaustive oracle below:
/// the oracle makes a NEW variant a compile error, and the length assertion
/// in the table test makes a variant missing from this list a test failure.
/// Both halves fail loudly, which is the point.
fn every_run_state() -> Vec<RunState> {
    vec![
        RunState::Queued,
        RunState::Thinking,
        RunState::Streaming,
        RunState::RunningTool,
        RunState::Waiting {
            reason: WaitReason::Dependency,
        },
        RunState::Retrying {
            attempt: 2,
            max: 5,
            delay_ms: 3_000,
            reason: WaitReason::ProviderBackoff,
        },
        RunState::InputRequired {
            menu: MenuId::new("m"),
        },
        RunState::PermissionRequired {
            menu: MenuId::new("m"),
        },
        RunState::Compacting,
        RunState::Verifying {
            step: VerifyStep::Check,
        },
        RunState::Concluding,
        RunState::EffectOutcomeUnknown,
        RunState::Cancelling,
        RunState::Done,
        RunState::Errored,
        RunState::Cancelled,
    ]
}

/// The independent oracle: does this state show the tail indicator?
///
/// Deliberately written as an EXHAUSTIVE match with no wildcard arm. Adding a
/// `RunState` variant fails this file to compile until someone decides,
/// explicitly, whether the new state is a running turn. That is the guard the
/// production code's `badge_tone()`-derived predicate is paired with.
fn expected_indicator(state: &RunState) -> bool {
    match state {
        // Active / Tool / Compacting — a turn is genuinely running.
        RunState::Thinking
        | RunState::Streaming
        | RunState::Concluding
        | RunState::Verifying { .. }
        | RunState::RunningTool
        | RunState::Cancelling
        | RunState::Compacting => true,
        // Idle and terminal — nothing is running.
        RunState::Done | RunState::Cancelled => false,
        // The waiting family — the owner excluded waiting explicitly, and
        // `Retrying` already owns its own tail row.
        RunState::Queued | RunState::Waiting { .. } | RunState::Retrying { .. } => false,
        // Blocked on the user, with a menu on screen.
        RunState::InputRequired { .. } | RunState::PermissionRequired { .. } => false,
        // Honesty/failure states are not work.
        RunState::EffectOutcomeUnknown | RunState::Errored => false,
    }
}

fn draw(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            let _ = render(model, frame);
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

/// A session attached on the Session screen, driven to `state`, with the
/// animation clock parked at phase 0 so the dot is deterministically `●`.
fn model_in(state: RunState) -> AppModel {
    let mut model = launcher_model();
    hit_session_named(&mut model, "billing-service");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(state))));
    model.anim_phase = 0;
    model
}

#[test]
fn indicator_covers_every_running_run_state() {
    let states = every_run_state();
    assert_eq!(
        states.len(),
        16,
        "every RunState variant must appear in the table — add the new one \
         (the oracle's exhaustive match will already have refused to compile)"
    );
    for state in states {
        let mut projection = SessionProjection::new();
        projection.apply(&EventPayload::RunState(state.clone()));
        assert_eq!(
            projection.is_turn_active(),
            expected_indicator(&state),
            "{state:?} classified wrongly for the transcript-tail indicator"
        );
    }
}

#[test]
fn the_owners_bug_a_streaming_turn_shows_the_indicator() {
    // The exact screenshot state: `▮ STREAMING`, model producing text.
    let model = model_in(RunState::Streaming);
    let rows = draw(&model, 118, 34);
    assert!(
        rows.iter().any(|row| row.contains("▮ STREAMING")),
        "precondition: the badge is the owner's streaming badge"
    );
    assert!(
        rows.iter().any(|row| row.contains(INDICATOR)),
        "a streaming turn must show the activity indicator above the composer"
    );
}

#[test]
fn indicator_renders_for_the_other_running_states() {
    for state in [
        RunState::Thinking,
        RunState::RunningTool,
        RunState::Concluding,
        RunState::Compacting,
        RunState::Cancelling,
        RunState::Verifying {
            step: VerifyStep::Test,
        },
    ] {
        let model = model_in(state.clone());
        let rows = draw(&model, 118, 34);
        assert!(
            rows.iter().any(|row| row.contains(INDICATOR)),
            "{state:?} is a running turn — the indicator must be up"
        );
    }
}

#[test]
fn indicator_stays_dark_when_the_turn_is_not_running() {
    for state in [
        RunState::Done,
        RunState::Cancelled,
        RunState::Queued,
        RunState::Waiting {
            reason: WaitReason::LocalChild,
        },
        RunState::Errored,
        RunState::EffectOutcomeUnknown,
        RunState::PermissionRequired {
            menu: MenuId::new("m"),
        },
        RunState::InputRequired {
            menu: MenuId::new("m"),
        },
    ] {
        let model = model_in(state.clone());
        let rows = draw(&model, 118, 34);
        assert!(
            !rows.iter().any(|row| row.contains(INDICATOR)),
            "{state:?} is idle/waiting/blocked/terminal — the indicator must be dark"
        );
    }
}

#[test]
fn retrying_shows_its_own_row_and_never_two_indicators() {
    let model = model_in(RunState::Retrying {
        attempt: 2,
        max: 5,
        delay_ms: 3_000,
        reason: WaitReason::ProviderBackoff,
    });
    let rows = draw(&model, 118, 34);
    assert!(
        rows.iter().any(|row| row.contains("Retrying in")),
        "the dedicated retry row is the one indicator for this state"
    );
    assert!(
        !rows.iter().any(|row| row.contains(INDICATOR)),
        "the thinking tail must not stack on top of the retry row"
    );
}

#[test]
fn the_clock_runs_while_the_indicator_is_up() {
    // The indicator BREATHES. `animated()` is the only thing that advances
    // `anim_phase`, so a render gate wider than the animation gate would paint
    // a frozen dot and a static shimmer for the whole stream.
    //
    // The `Done` half keeps this test honest: if some unrelated live element
    // (a chip, a task) were animating in this fixture, it would fail here
    // rather than making the `Streaming` half vacuously true.
    let idle = model_in(RunState::Done);
    assert!(
        !idle.animated(),
        "precondition: nothing else in this fixture animates at rest"
    );
    let streaming = model_in(RunState::Streaming);
    assert!(
        streaming.animated(),
        "the shared clock must tick while the streaming indicator is on screen"
    );
}

#[test]
fn chip_display_states_follow_the_same_law() {
    // The sim/subagent surface must not drift from the session surface.
    // Exhaustive match, same reasoning as the RunState oracle.
    for state in [
        ChipDisplayState::Idle,
        ChipDisplayState::Thinking,
        ChipDisplayState::Streaming,
        ChipDisplayState::Running,
        ChipDisplayState::Tool,
        ChipDisplayState::InputRequired,
        ChipDisplayState::Waiting,
        ChipDisplayState::Done,
        ChipDisplayState::Error,
    ] {
        let expected = match state {
            ChipDisplayState::Thinking
            | ChipDisplayState::Streaming
            | ChipDisplayState::Running
            | ChipDisplayState::Tool => true,
            ChipDisplayState::Idle
            | ChipDisplayState::InputRequired
            | ChipDisplayState::Waiting
            | ChipDisplayState::Done
            | ChipDisplayState::Error => false,
        };
        assert_eq!(
            state.is_turn_active(),
            expected,
            "{state:?} classified wrongly for the chip-view tail indicator"
        );
    }
}
