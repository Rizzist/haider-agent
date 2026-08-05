//! T2 — the toggle-to-talk state machine's reducer laws: chip/`/talk`
//! start, Esc cancels (discards), Enter commits + submits, typing commits
//! into the composer and keeps editing, generation staleness, the
//! Deepgram key round-trip, honest error routing, and the ghost row's
//! chrome-not-content law.
#![allow(clippy::expect_used)]

use haider_rpc::SecretWire;
use haider_stt::config::TranscriptionEngine;
use haider_stt::{SttError, TranscriptFrame, TranscriptionResult};
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::live::TranscriptionOp;
use haider_tui::talk::{
    CommitIntent, SecretIntent, SetupStage, TalkEngineSpec, TalkEvent, TalkPhase, TalkShellCommand,
};
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, submit};

fn sid() -> haider_protocol::ids::SessionId {
    haider_protocol::ids::SessionId::new("s-talk")
}

/// A live session on the session screen, talk idle.
fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    assert_eq!(model.screen, Screen::Session);
    model.requests.clear();
    model
}

/// The same, advanced to LISTENING through the real event path.
fn listening() -> AppModel {
    let mut model = live_session();
    model.talk_toggle();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Started {
        generation,
        sample_rate: 48_000,
    });
    assert_eq!(model.talk.phase, TalkPhase::Listening);
    model.requests.clear();
    model
}

fn local_frame(text: &str) -> TranscriptFrame {
    TranscriptFrame {
        provider: haider_stt::EngineKind::WhisperLocal,
        text: text.to_owned(),
        is_final: false,
        speech_final: true,
    }
}

fn talk_shell(requests: &[AppRequest]) -> Vec<&TalkShellCommand> {
    requests
        .iter()
        .filter_map(|request| match request {
            AppRequest::TalkShell(command) => Some(command),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Starting
// ---------------------------------------------------------------------------

/// MUTATION CHECK: restore the old `refuse_demo_only("push-to-talk")`
/// arm on the live TalkChip. Expected failure: no `TalkShell(Start)` is
/// requested and `listening` never arms.
#[test]
fn live_chip_press_starts_a_local_session() {
    let mut model = live_session();
    model.handle_hit(Hit::TalkChip);
    assert_eq!(model.talk.phase, TalkPhase::Starting);
    assert!(model.listening, "the chip chrome reads the shared flag");
    let commands = talk_shell(&model.requests);
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0],
        &TalkShellCommand::Start {
            generation: model.talk.generation,
            engine: TalkEngineSpec::Local { model_id: None },
        }
    );
}

/// MUTATION CHECK: drop the `"talk"` arm from `execute_slash`. Expected
/// failure: bare `/talk` produces no start request.
#[test]
fn slash_talk_toggles_like_the_chip() {
    let mut model = live_session();
    submit(&mut model, "/talk");
    assert_eq!(model.talk.phase, TalkPhase::Starting);
    assert_eq!(talk_shell(&model.requests).len(), 1);
}

/// MUTATION CHECK: let demo mode fall through into `talk_start`. Expected
/// failure: a demo `/talk` pushes a `TalkShell` request the demo loop can
/// only discard silently.
#[test]
fn demo_slash_talk_refuses_honestly() {
    let mut model = launcher_model();
    model.screen = Screen::Session;
    submit(&mut model, "/talk");
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(talk_shell(&model.requests).is_empty());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|f| f.contains("live only")),
        "honest refusal: {:?}",
        model.flash
    );
}

/// MUTATION CHECK: accept `Started` regardless of generation. Expected
/// failure: the stale start below flips a settled machine back to
/// listening.
#[test]
fn started_flips_to_listening_only_for_the_live_generation() {
    let mut model = live_session();
    model.talk_toggle();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Started {
        generation: generation + 7,
        sample_rate: 48_000,
    });
    assert_eq!(model.talk.phase, TalkPhase::Starting, "stale start dropped");
    model.handle_talk(TalkEvent::Started {
        generation,
        sample_rate: 48_000,
    });
    assert_eq!(model.talk.phase, TalkPhase::Listening);
}

// ---------------------------------------------------------------------------
// The three gestures
// ---------------------------------------------------------------------------

/// MUTATION CHECK: make Esc realize the ghost before settling. Expected
/// failure: the discarded words appear in the composer.
#[test]
fn esc_cancels_and_discards_everything() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: local_frame("do not keep this"),
    });
    assert_eq!(model.talk.ghost, "do not keep this");
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(!model.listening);
    assert!(model.talk.ghost.is_empty());
    assert!(model.composer.is_empty(), "discard means DISCARD");
    assert_eq!(
        talk_shell(&model.requests),
        vec![&TalkShellCommand::Cancel { generation }]
    );
}

/// MUTATION CHECK: make Enter realize the GHOST instead of waiting for
/// the engine's definitive result. Expected failure: the submit below
/// carries the partial text, not the engine's assembled transcript.
#[test]
fn enter_commits_the_engine_result_and_submits() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: local_frame("partial view"),
    });
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.talk.phase, TalkPhase::Finishing);
    assert_eq!(model.talk.intent, CommitIntent::Submit);
    assert_eq!(
        talk_shell(&model.requests),
        vec![&TalkShellCommand::Finish { generation }]
    );
    model.requests.clear();
    model.handle_talk(TalkEvent::Finished {
        generation,
        result: Ok(TranscriptionResult {
            text: "the full assembled transcript".to_owned(),
            segments: 2,
            duration_ms: 3200,
        }),
    });
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(!model.listening);
    let submitted = model.requests.iter().find_map(|request| match request {
        AppRequest::SubmitText { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(
        submitted.as_deref(),
        Some("the full assembled transcript"),
        "commit + submit rides the ONE composer submit path"
    );
    assert!(
        model.composer.is_empty(),
        "the submit consumed the realized draft"
    );
}

/// MUTATION CHECK: swallow the typed character after the commit (return
/// true from `talk_key`'s char arm). Expected failure: the composer ends
/// without the `x` the user typed.
#[test]
fn typing_commits_the_ghost_and_keeps_editing() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: local_frame("hello world"),
    });
    model.handle(key(KeyCode::Char('x')));
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert_eq!(
        model.composer.text(),
        "hello world x",
        "ghost realized (one separating space), typing continues"
    );
    assert_eq!(
        talk_shell(&model.requests),
        vec![&TalkShellCommand::Cancel { generation }],
        "the engine tail is discarded — what you saw is what you keep"
    );
}

/// MUTATION CHECK: skip the ghost realization on paste. Expected
/// failure: the pasted text lands without the spoken words before it.
#[test]
fn pasting_commits_the_ghost_first() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: local_frame("spoken"),
    });
    model.handle(AppEvent::Paste(haider_tui::app::Pasted::new(
        "pasted".to_owned(),
    )));
    assert_eq!(model.composer.text(), "spoken pasted");
    assert_eq!(model.talk.phase, TalkPhase::Idle);
}

/// The contract stays three gestures wide: arrows and backspace are inert
/// while listening.
///
/// MUTATION CHECK: let Backspace fall through to the composer. Expected
/// failure: the draft under the session loses a character.
#[test]
fn other_keys_are_inert_while_listening() {
    let mut model = listening();
    model.composer.insert_str("draft");
    model.handle(key(KeyCode::Backspace));
    model.handle(key(KeyCode::Up));
    model.handle(key(KeyCode::Tab));
    assert_eq!(model.composer.text(), "draft");
    assert_eq!(model.talk.phase, TalkPhase::Listening);
}

/// MUTATION CHECK: drop the generation gate on `Finished`. Expected
/// failure: the canceled session's late result lands in the composer.
#[test]
fn a_late_finished_after_cancel_is_dropped_whole() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.handle(key(KeyCode::Esc));
    model.requests.clear();
    model.handle_talk(TalkEvent::Finished {
        generation,
        result: Ok(TranscriptionResult {
            text: "sneaky late transcript".to_owned(),
            segments: 1,
            duration_ms: 900,
        }),
    });
    assert!(model.composer.is_empty());
    assert!(model.requests.is_empty());
}

/// MUTATION CHECK: give `CapReached` the Submit intent. Expected
/// failure: the cap auto-submits a turn the user never confirmed.
#[test]
fn the_capture_cap_finishes_into_the_composer_without_submitting() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::CapReached { generation });
    assert_eq!(model.talk.phase, TalkPhase::Finishing);
    assert_eq!(model.talk.intent, CommitIntent::Insert);
    model.requests.clear();
    model.handle_talk(TalkEvent::Finished {
        generation,
        result: Ok(TranscriptionResult {
            text: "capped speech".to_owned(),
            segments: 1,
            duration_ms: 900_000,
        }),
    });
    assert_eq!(model.composer.text(), "capped speech");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SubmitText { .. })),
        "no auto-submit at the cap"
    );
}

/// MUTATION CHECK: discard the ghost on an engine failure. Expected
/// failure: the words the user watched land are gone with the error.
#[test]
fn a_finish_error_keeps_the_watched_words_and_reports() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: local_frame("watched words"),
    });
    model.handle(key(KeyCode::Enter));
    model.handle_talk(TalkEvent::Finished {
        generation,
        result: Err(SttError::Endpoint("whisper-cli failed: boom".to_owned())),
    });
    assert_eq!(model.composer.text(), "watched words ");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|f| f.contains("talk failed")),
        "honest error: {:?}",
        model.flash
    );
}

/// MUTATION CHECK: remove the talk teardown from the stash seam.
/// Expected failure: leaving the session keeps the machine engaged with
/// no surface to land on.
#[test]
fn leaving_the_surface_cancels_the_session() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.back_to_launcher();
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(!model.listening);
    assert!(talk_shell(&model.requests).contains(&&TalkShellCommand::Cancel { generation }));
}

// ---------------------------------------------------------------------------
// Ghost assembly + chrome law
// ---------------------------------------------------------------------------

/// MUTATION CHECK: append local frames instead of replacing. Expected
/// failure: the cumulative engine text doubles up.
#[test]
fn local_frames_replace_the_ghost_cumulatively() {
    let mut model = listening();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: local_frame("one"),
    });
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: local_frame("one two"),
    });
    assert_eq!(model.talk.ghost, "one two");
}

/// MUTATION CHECK: treat Deepgram finals like interims (replace instead
/// of append). Expected failure: the second final erases the first.
#[test]
fn deepgram_finals_accumulate_and_interims_replace() {
    let mut model = live_session();
    model.talk_config.engine = TranscriptionEngine::Deepgram;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned());
    model.talk_toggle();
    model.talk_secret_answer(Some(SecretWire::new("dg-key")));
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Started {
        generation,
        sample_rate: 48_000,
    });
    let frame = |text: &str, is_final: bool| TranscriptFrame {
        provider: haider_stt::EngineKind::Deepgram,
        text: text.to_owned(),
        is_final,
        speech_final: is_final,
    };
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: frame("hel", false),
    });
    assert_eq!(model.talk.ghost, "hel");
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: frame("hello", false),
    });
    assert_eq!(model.talk.ghost, "hello", "interims replace");
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: frame("hello there", true),
    });
    assert_eq!(model.talk.ghost, "hello there", "final absorbs the interim");
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: frame("friend", false),
    });
    assert_eq!(model.talk.ghost, "hello there friend");
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: frame("friend.", true),
    });
    assert_eq!(model.talk.ghost, "hello there friend.");
}

/// THE CHROME LAW: the ghost row is band chrome — no partial ever enters
/// the transcript projection (F2's line stability holds by construction).
///
/// MUTATION CHECK: push the partial into `projection` as a note. Expected
/// failure: the entry count moves while dictating.
#[test]
fn the_ghost_row_is_chrome_never_content() {
    let mut model = listening();
    let generation = model.talk.generation;
    let entries_before = model.projection.entries().len();
    for round in 0..12 {
        model.handle_talk(TalkEvent::Partial {
            generation,
            frame: local_frame(&format!("cumulative text round {round}")),
        });
    }
    assert_eq!(
        model.projection.entries().len(),
        entries_before,
        "partials never become transcript entries"
    );
    assert!(model.talk_ghost_visible());
    model.handle(key(KeyCode::Esc));
    assert!(!model.talk_ghost_visible());
    assert_eq!(model.projection.entries().len(), entries_before);
}

// ---------------------------------------------------------------------------
// Deepgram key round-trip + honest errors
// ---------------------------------------------------------------------------

/// MUTATION CHECK: start the Deepgram engine without the vault read
/// (invent an empty key). Expected failure: no
/// `TranscriptionSecretRead` is requested.
#[test]
fn deepgram_start_reads_the_vaulted_key_first() {
    let mut model = live_session();
    model.talk_config.engine = TranscriptionEngine::Deepgram;
    model.talk_config.deepgram_model = Some("nova-3-medical".to_owned());
    model.talk_config.language = "en-US".to_owned();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned());
    model.talk_toggle();
    assert_eq!(model.talk.phase, TalkPhase::Starting);
    assert_eq!(model.talk.secret_intent, Some(SecretIntent::Start));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::TranscriptionSecretRead]
    ));
    model.requests.clear();
    let secret = SecretWire::new("dg-live-key");
    model.talk_secret_answer(Some(secret.clone()));
    assert_eq!(
        talk_shell(&model.requests),
        vec![&TalkShellCommand::Start {
            generation: model.talk.generation,
            engine: TalkEngineSpec::Deepgram {
                secret,
                model: Some("nova-3-medical".to_owned()),
                language: "en-US".to_owned(),
            },
        }]
    );
}

/// MUTATION CHECK: on an absent key, keep the machine in `Starting`.
/// Expected failure: the chip pulses forever with no engine behind it.
#[test]
fn deepgram_start_without_a_key_opens_setup() {
    let mut model = live_session();
    model.talk_config.engine = TranscriptionEngine::Deepgram;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned());
    model.talk_toggle();
    model.talk_secret_answer(None);
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(!model.listening);
    let card = model.talk_setup.as_ref().expect("setup opened");
    assert_eq!(card.stage, SetupStage::DeepgramKey);
    assert!(
        card.error
            .as_deref()
            .is_some_and(|e| e.contains("no Deepgram key"))
    );
}

/// MUTATION CHECK: drop the feature gate. Expected failure: the request
/// is issued against a daemon that cannot serve it.
#[test]
fn deepgram_without_the_daemon_feature_refuses_honestly() {
    let mut model = live_session();
    model.talk_config.engine = TranscriptionEngine::Deepgram;
    model.talk_toggle();
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(model.requests.is_empty());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|f| f.contains("transcription secrets")),
        "honest gate: {:?}",
        model.flash
    );
}

/// MUTATION CHECK: map `ModelMissing` to a bare flash. Expected failure:
/// the reinstall surface (setup at the whisper stage) never opens.
#[test]
fn model_missing_opens_the_reinstall_surface() {
    let mut model = live_session();
    model.talk_toggle();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::StartFailed {
        generation,
        error: SttError::ModelMissing {
            model_id: "base.en".to_owned(),
        },
    });
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    let card = model.talk_setup.as_ref().expect("setup opened");
    assert_eq!(card.stage, SetupStage::Local);
    assert!(card.error.as_deref().is_some_and(|e| e.contains("base.en")));
}

/// MUTATION CHECK: swallow `MicUnavailable`. Expected failure: a denied
/// mic looks like a hung chip instead of the TCC hint naming the
/// terminal app.
#[test]
fn mic_denied_surfaces_the_terminal_grant_hint() {
    let mut model = live_session();
    model.talk_toggle();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::StartFailed {
        generation,
        error: SttError::MicUnavailable {
            hint: "no microphone signal — grant microphone access to iTerm2 (your terminal app)"
                .to_owned(),
        },
    });
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(!model.listening);
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|f| f.contains("grant microphone access")),
        "the TCC hint lands: {:?}",
        model.flash
    );
}

/// MUTATION CHECK: leave the secret-read failure un-settling. Expected
/// failure: a vault refusal wedges the machine in `Starting`.
#[test]
fn a_secret_read_failure_settles_the_start() {
    let mut model = live_session();
    model.talk_config.engine = TranscriptionEngine::Deepgram;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned());
    model.talk_toggle();
    model.talk_secret_failed(TranscriptionOp::Get, "vault unavailable".to_owned());
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(!model.listening);
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|f| f.contains("vault unavailable"))
    );
}

/// The realize cap: a pathological transcript cannot flood the composer
/// past the ADE insert cap.
///
/// MUTATION CHECK: drop `clamp_realized`. Expected failure: the composer
/// holds 9000 chars.
#[test]
fn realized_transcripts_are_capped() {
    let mut model = listening();
    let generation = model.talk.generation;
    let huge = "a".repeat(9_000);
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: local_frame(&huge),
    });
    model.handle(key(KeyCode::Char('!')));
    assert_eq!(
        model.composer.text().chars().count(),
        haider_tui::talk::MAX_REALIZED_CHARS + 2,
        "capped ghost + separating space + the typed char"
    );
}
