//! W5g-6 — the owner's second-turn report, all three surfaces:
//!
//! 1. a failed run's PUBLIC reason renders as a transcript row (three
//!    silent ✗ ERRORED badges preceded this — the reason was always in
//!    the envelope, never on screen);
//! 2. the live launcher shows at most FOUR recent sessions;
//! 3. a terminal resize clears the stale hover highlight (a stationary
//!    pointer kept the OLD target's highlight at its NEW row).
//!
//! The root cause behind the report itself — assistant history replayed
//! as `input_text`, a hard 400 on every turn after the first reply — is
//! pinned provider-side in `openai_tests::assistant_history_replays_as_
//! output_text`.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::error::ErrorCode;
use haider_protocol::state::RunState;
use haider_tui::app::{Hit, LIVE_LAUNCHER_ROWS, RuntimeMode};
use haider_tui::projection::{SessionProjection, TranscriptEntry};

mod common;
use common::launcher_model;

/// MUTATION CHECK (W5g-6): return `RunFailed` to the swallowed-payload
/// arm. Expected runtime failure: no Error entry joins the transcript —
/// the exact three-times-reported silent ERRORED badge.
#[test]
fn a_failed_run_writes_its_reason_into_the_transcript() {
    let mut projection = SessionProjection::default();
    projection.apply(&EventPayload::RunFailed {
        code: ErrorCode::ProviderError,
        message: "InvalidRequest: OpenAI HTTP 400 returned an invalid-request error".to_owned(),
        retryable: false,
    });
    projection.apply(&EventPayload::RunState(RunState::Errored));
    assert!(
        projection.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Error { text }
                if text.contains("provider_error") && text.contains("HTTP 400")
        )),
        "the failure reason is a transcript row, not a bare badge: {:?}",
        projection.entries()
    );
}

/// MUTATION CHECK (W5g-6): restore `LIVE_LAUNCHER_ROWS = 9`. Expected
/// runtime failure: the cap assertion below (owner ask: max 4 recents;
/// /sessions lists the rest).
#[test]
fn the_live_launcher_caps_at_four_recent_sessions() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    assert_eq!(LIVE_LAUNCHER_ROWS, 4);
    assert_eq!(model.launcher_rows(), 4);
}

/// MUTATION CHECK (W5g-6): drop the `hovered = None` reset from
/// `handle_resize`. Expected runtime failure: the stale highlight below
/// survives the resize — the owner's mismatched-hover report.
#[test]
fn a_resize_clears_the_stale_hover_highlight() {
    let mut model = launcher_model();
    model.handle_hover(Some(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    )));
    assert!(model.hovered.is_some(), "hover armed");
    model.handle_resize();
    assert!(
        model.hovered.is_none(),
        "a resize moves every target under a stationary pointer — the old highlight must die until real motion re-arms it"
    );
}
