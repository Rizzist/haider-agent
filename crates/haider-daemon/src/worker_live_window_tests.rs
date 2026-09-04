//! The daemon's live-turn nudge dedupe set must follow the live window, not
//! the length of the durable transcript.

use super::fold_live_turn_nudge_seq;
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::state::SessionState;
use std::collections::HashSet;

fn user_message(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: Vec::new(),
        mode: DeliveryMode::default(),
    }
}

/// MUTATION CHECK: drop the `Idle` arm and the rebuilt set grows by one entry
/// per user message the session has ever journaled, so a restarted daemon's
/// dedupe memory follows transcript age instead of the live turn.
#[test]
fn an_idle_fact_ends_the_live_turn_dedupe_window() {
    let mut sequences = HashSet::new();
    for seq in 1..=200 {
        fold_live_turn_nudge_seq(&mut sequences, seq, &user_message("nudge"));
        if seq % 4 == 0 {
            fold_live_turn_nudge_seq(
                &mut sequences,
                seq,
                &EventPayload::SessionState(SessionState::Idle { interrupted: false }),
            );
        }
    }
    assert!(
        sequences.is_empty(),
        "a settled transcript leaves no live-turn dedupe state"
    );
}

/// MUTATION CHECK: replace `sequences.clear()` with a no-op on interruption
/// and a cancelled turn keeps its delivered sequences resident forever.
#[test]
fn an_interrupted_idle_also_closes_the_window() {
    let mut sequences = HashSet::new();
    fold_live_turn_nudge_seq(&mut sequences, 1, &user_message("first"));
    fold_live_turn_nudge_seq(&mut sequences, 2, &user_message("second"));
    assert_eq!(sequences.len(), 2);
    fold_live_turn_nudge_seq(
        &mut sequences,
        3,
        &EventPayload::SessionState(SessionState::Idle { interrupted: true }),
    );
    assert!(sequences.is_empty());
}

/// The messages of the one live turn are still deduped: only facts after the
/// last Idle survive, which is exactly what a mid-turn redelivery consults.
#[test]
fn the_live_turn_keeps_exactly_its_own_delivered_sequences() {
    let mut sequences = HashSet::new();
    fold_live_turn_nudge_seq(&mut sequences, 1, &user_message("old"));
    fold_live_turn_nudge_seq(
        &mut sequences,
        2,
        &EventPayload::SessionState(SessionState::Idle { interrupted: false }),
    );
    fold_live_turn_nudge_seq(&mut sequences, 3, &user_message("live"));
    fold_live_turn_nudge_seq(&mut sequences, 4, &user_message("live nudge"));
    fold_live_turn_nudge_seq(
        &mut sequences,
        5,
        &EventPayload::SessionState(SessionState::ActiveRun),
    );
    assert_eq!(sequences, HashSet::from([3, 4]));
}
