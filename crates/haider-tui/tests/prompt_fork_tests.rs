//! 967 — fork from a previous prompt (the owner's Esc-Esc ask).
//!
//! The engine shipped in v0.0.966; this pins the surface that reaches it.
//! Five laws:
//!
//! 1. a SINGLE Esc is still the interrupt — the fork verb never appears
//!    inside the gesture that stops a turn;
//! 2. the rapid SECOND Esc opens the chooser and `f` there (or `/fork <n>`)
//!    issues the exclusive prompt cut with the DURABLE journal sequence;
//! 3. the original session — its row, transcript and parked draft — is
//!    untouched, and the child opens as a NEW surface;
//! 4. the returned prompt lands as an EDITABLE draft whose typed attachment
//!    blocks survive byte-identical;
//! 5. a hand-off that cannot open the child names `haider resume <id>` —
//!    the child exists daemon-side and is never silently lost.
#![allow(clippy::expect_used)]

use std::time::{Duration, Instant};

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, SessionId};
use haider_protocol::session_fork::{SessionForkDraft, SessionForkPromptSelector};
use haider_protocol::tool::{AttachmentBlock, PdfDeliveryMode};
use haider_rpc::{AttachmentId, CommandId, RequestBody, ResponseBody};
use haider_tui::app::{AppEvent, AppModel, AppRequest, RuntimeMode};
use haider_tui::link::{CommandContext, command_required_features, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::projection::RawOutcome;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn sid(name: &str) -> SessionId {
    SessionId::new(name)
}

fn raw(session: &SessionId, seq: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("fork-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("fork-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

fn user(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

/// A live session whose daemon serves BOTH fork tokens, with a non-prompt
/// envelope at sequence 1 and committed prompts at 2 and 3.
///
/// The offset is deliberate: the newest row is chooser index 0 but journal
/// sequence 3, so an implementation that sent the LIST INDEX as the cut
/// cannot pass by coincidence.
fn forkable_model() -> (AppModel, SessionId) {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    let session = sid("source");
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_FORK_V1.to_owned());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_PROMPT_FORK_V1.to_owned());
    model.upsert_live_session(&session);
    model.open_session(&session);
    assert_eq!(
        model.route_raw(&raw(&session, 1, &EventPayload::IdleDecayed)),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(&session, 2, &user("oldest\nverbatim"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(&session, 3, &user("newest prompt"))),
        RawOutcome::Applied
    );
    model.turn_active = false;
    model.requests.clear();
    model.flash = None;
    (model, session)
}

fn open_chooser(model: &mut AppModel, now: Instant) {
    model.handle_at(key(KeyCode::Esc), now);
    assert!(model.backtrack.is_none(), "the first Esc only arms");
    model.handle_at(key(KeyCode::Esc), now + Duration::from_millis(100));
    assert!(model.backtrack.is_some(), "the rapid second Esc opens");
}

fn fork_request(model: &AppModel) -> Option<(SessionId, u64)> {
    model.requests.iter().find_map(|request| match request {
        AppRequest::ForkFromPrompt { session, seq, .. } => Some((session.clone(), *seq)),
        _ => None,
    })
}

fn draw(model: &AppModel) -> String {
    let backend = TestBackend::new(110, 34);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            let _ = render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

// ---- law 1: single Esc remains the interrupt --------------------------

/// MUTATION CHECK: move the `f` arm out of `handle_backtrack_key` into the
/// session key handler. Expected failure: `f` mid-turn stops being a
/// composer keystroke, and a mid-turn Esc could reach the fork door.
#[test]
fn single_escape_still_interrupts_and_never_forks() {
    let (mut model, _) = forkable_model();
    let now = Instant::now();
    model.turn_active = true;
    model.handle_at(key(KeyCode::Esc), now);
    assert!(!model.turn_active, "esc mid-turn still interrupts");
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::Interrupt { .. })),
        "the interrupt request survives the fork wave"
    );
    assert!(model.backtrack.is_none(), "no chooser during a turn");
    assert_eq!(fork_request(&model), None, "an interrupt never forks");

    // Spamming Esc to interrupt cannot false-positive into a fork: the
    // post-interrupt Esc is only the FIRST idle press.
    model.handle_at(key(KeyCode::Esc), now + Duration::from_millis(40));
    assert!(model.backtrack.is_none());
    assert_eq!(fork_request(&model), None);
}

/// `f` outside the chooser is ordinary composer text — the fork verb is
/// modal, exactly like the chooser's `j`/`k`.
#[test]
fn f_outside_the_chooser_types_into_the_composer() {
    let (mut model, _) = forkable_model();
    model.handle(key(KeyCode::Char('f')));
    assert_eq!(model.composer.text(), "f");
    assert_eq!(fork_request(&model), None);
}

// ---- law 2: the second Esc opens, `f` cuts at the durable sequence ----

/// MUTATION CHECK: have `issue_prompt_fork` send the chooser INDEX instead
/// of `entry.seq`. Expected failure: the request carries 0, not 3.
#[test]
fn double_escape_then_f_forks_at_the_durable_journal_sequence() {
    let (mut model, session) = forkable_model();
    open_chooser(&mut model, Instant::now());
    model.handle(key(KeyCode::Char('f')));
    assert_eq!(
        fork_request(&model),
        Some((session, 3)),
        "the newest prompt's committed sequence is the cut"
    );
    assert!(model.backtrack.is_none(), "issuing closes the chooser");
    assert_eq!(
        model.flash.as_deref(),
        Some("· forking at prompt 1 — this session stays open")
    );
}

/// Walking to an older row cuts THERE. `/fork <n>` is the same door for a
/// terminal that cannot convey rapid double-Esc timing, exactly as
/// `/history <n>` is for the recall.
#[test]
fn older_rows_and_the_slash_command_reach_the_same_cut() {
    let (mut model, session) = forkable_model();
    let now = Instant::now();
    open_chooser(&mut model, now);
    model.handle_at(key(KeyCode::Esc), now + Duration::from_millis(200));
    assert_eq!(model.backtrack.expect("chooser").selection, 1);
    model.handle(key(KeyCode::Char('f')));
    assert_eq!(fork_request(&model), Some((session.clone(), 2)));

    let (mut model, session) = forkable_model();
    run_slash(&mut model, "/fork 2");
    assert_eq!(fork_request(&model), Some((session, 2)));
}

/// NEVER FABRICATE A CUT: the demo twin's recalled prompts carry no
/// committed sequence, so the verb refuses honestly and the hint that
/// offers it is not even drawn.
#[test]
fn a_prompt_without_a_committed_sequence_is_refused_not_invented() {
    let mut model = launcher_model();
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "demo prompt".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    model.turn_active = false;
    model.requests.clear();
    assert_eq!(
        model.prompt_history[0].seq, None,
        "a payload with no envelope has no durable coordinate"
    );
    assert!(!model.prompt_fork_offered());

    open_chooser(&mut model, Instant::now());
    assert!(
        !draw(&model).contains("f fork"),
        "the hint must not advertise a verb that can only refuse"
    );
    model.handle(key(KeyCode::Char('f')));
    assert_eq!(fork_request(&model), None);
    assert_eq!(
        model.flash.as_deref(),
        Some("· fork — live only; the new session is daemon truth")
    );
}

/// The same law where the demo gate cannot mask it: a LIVE session holding
/// a recalled prompt with no committed sequence refuses by saying so.
///
/// MUTATION CHECK: replace `entry.seq` with `entry.seq.unwrap_or(0)`.
/// Expected failure: sequence 0 — a coordinate no journal ever issued —
/// travels to the daemon as a fork cut.
#[test]
fn a_live_prompt_without_a_sequence_refuses_rather_than_inventing_zero() {
    let (mut model, _) = forkable_model();
    model
        .prompt_history
        .push_front(haider_tui::session::PromptEntry::local(
            "no durable coordinate".to_owned(),
        ));
    run_slash(&mut model, "/fork 1");
    assert_eq!(fork_request(&model), None, "no cut is invented");
    assert_eq!(
        model.flash.as_deref(),
        Some("· fork — that prompt carries no committed sequence")
    );
}

/// The absence law at the surface: a daemon serving only the shipped
/// exact-node fork cannot honor a prompt cut, and nothing is sent.
#[test]
fn a_daemon_without_both_tokens_refuses_before_anything_is_issued() {
    let (mut model, _) = forkable_model();
    model
        .daemon_features
        .remove(haider_rpc::FEATURE_SESSION_PROMPT_FORK_V1);
    assert!(!model.prompt_fork_offered());
    run_slash(&mut model, "/fork 1");
    assert_eq!(fork_request(&model), None);
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("forking at a previous prompt")),
        "the honest stale-daemon note names what is missing: {:?}",
        model.flash
    );
}

/// The chooser advertises the verb only where it would work, and the rows
/// stay the scannable one-line form the recall already renders.
#[test]
fn the_chooser_shows_the_fork_verb_and_flattens_multiline_prompts() {
    let (mut model, _) = forkable_model();
    open_chooser(&mut model, Instant::now());
    let screen = draw(&model);
    assert!(screen.contains("1. newest prompt"));
    assert!(screen.contains("2. oldest verbatim"));
    assert!(screen.contains("f fork into a new session"));
    assert_eq!(
        model.prompt_history[1].text, "oldest\nverbatim",
        "flattening is display only — the journal bytes are untouched"
    );
}

// ---- the wire: exact coordinates, both feature tokens ------------------

/// MUTATION CHECK: give the request a `fork_node_id`/`fork_seq`. Expected
/// failure: the body stops being the exclusive prompt cut and an older
/// daemon would silently honor it as a legacy exact-node fork.
#[test]
fn issuance_travels_the_exact_prompt_cut_to_the_wire() {
    let mut model = forkable_model().0;
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid("source"),
            attachment: AttachmentId::new("att-0"),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    let commands = driver.handle_request(
        &mut model,
        AppRequest::ForkFromPrompt {
            session: sid("source"),
            source_branch: None,
            seq: 3,
        },
    );
    let command = commands.first().expect("one fork command").clone();
    let LiveCommand::SessionForkPrompt {
        command_id,
        session,
        worker_generation,
        source_branch,
        seq,
    } = command.clone()
    else {
        panic!("expected LiveCommand::SessionForkPrompt, got {command:?}");
    };
    assert_eq!(session, sid("source"));
    assert_eq!(worker_generation, 7);
    assert_eq!(source_branch, None);
    assert_eq!(seq, 3);
    assert_eq!(
        command_required_features(&command),
        &[
            haider_rpc::FEATURE_SESSION_FORK_V1,
            haider_rpc::FEATURE_SESSION_PROMPT_FORK_V1
        ],
        "the send-time gate demands the additive shape token too"
    );
    assert_eq!(
        request_body(command),
        RequestBody::SessionFork {
            command_id,
            session_id: sid("source"),
            worker_generation: 7,
            source_branch_id: None,
            fork_node_id: None,
            fork_seq: None,
            prompt: Some(SessionForkPromptSelector { seq: 3 }),
            name: None,
        }
    );
}

// ---- laws 3+4: the child opens, the source survives, the draft edits --

fn attachments() -> Vec<AttachmentBlock> {
    vec![
        AttachmentBlock::Image {
            artifact: ArtifactRef::new("blake3:image"),
            mime: "image/png".to_owned(),
            width: Some(1_920),
            height: Some(1_080),
        },
        AttachmentBlock::Pdf {
            artifact: ArtifactRef::new("blake3:pdf"),
            name: "brief.pdf".to_owned(),
            pages: 12,
            delivery: PdfDeliveryMode::NativeDocument,
        },
    ]
}

fn forked_reply(child: &str) -> LiveReply {
    LiveReply::PromptForked {
        command_id: CommandId::new("cmd-prompt-fork"),
        source_session: sid("source"),
        session: sid(child),
        worker_generation: 11,
        draft: SessionForkDraft {
            text: "newest prompt".to_owned(),
            attachments: attachments(),
        },
    }
}

/// The owner's requirement, whole: the source keeps its row, its transcript
/// and its parked draft; the child becomes the surface.
///
/// MUTATION CHECK: make the reply's arm call `open_session` on
/// `source_session`. Expected failure: the active surface is the source and
/// the child never opens.
#[test]
fn forking_opens_the_child_and_leaves_the_original_untouched() {
    let (mut model, session) = forkable_model();
    // A parked draft on the source: it must come back if the user returns.
    model.composer.set_text("half-typed source draft");
    let source_entries = model.projection.entries().len();
    let source_prompts = model.prompt_history.clone();
    let mut driver = LiveDriver::new("test");
    driver.apply(&mut model, forked_reply("child"));

    assert_eq!(
        model.active_session.as_ref(),
        Some(&sid("child")),
        "a fork opens a NEW surface"
    );
    assert!(
        model.sessions.iter().any(|row| row.id == session),
        "the original session stays on the roster"
    );
    let source = model
        .sessions
        .iter()
        .find(|row| row.id == session)
        .expect("source row");
    assert_eq!(
        source.projection.entries().len(),
        source_entries,
        "forking must not mutate the original transcript"
    );
    assert_eq!(
        source.prompt_history, source_prompts,
        "the original session's prompt history is untouched"
    );

    // Returning parks the child's draft and revives the source's own.
    model.open_session(&session);
    assert_eq!(model.composer.text(), "half-typed source draft");
}

/// The prompt arrives as a DRAFT, not a turn: editable, unsent, and its
/// typed attachment blocks survive byte-identical (image dimensions and the
/// PDF delivery mode are exactly what re-deriving a block would drop).
///
/// MUTATION CHECK: have `PendingAttachment::ready_block` rebuild from
/// `kind` instead of returning `carried`. Expected failure: the submitted
/// blocks lose `width`/`height` and fall back to `ExtractedText`.
#[test]
fn the_draft_is_editable_and_preserves_typed_attachments() {
    let (mut model, _) = forkable_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(&mut model, forked_reply("child"));

    assert_eq!(model.composer.text(), "newest prompt");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SubmitText { .. })),
        "a fork draft is UNSENT"
    );
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::AttachUpload { .. })),
        "carried blocks are already in the CAS — nothing re-uploads"
    );
    assert_eq!(model.composer.attachments().len(), 2);

    // Edit it, then submit: the exact blocks ride the turn.
    model.handle(key(KeyCode::Char('!')));
    assert_eq!(model.composer.text(), "newest prompt!");
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    let submitted = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::SubmitText {
                text, attachments, ..
            } => Some((text.clone(), attachments.clone())),
            _ => None,
        })
        .expect("the edited draft submits");
    assert_eq!(submitted.0, "newest prompt!");
    assert_eq!(submitted.1, attachments());
}

// ---- law 5: a failed hand-off never loses the child -------------------

/// A DEAD SOCKET TAKES NO ATTACH: the fork already committed daemon-side,
/// so an unreachable child is announced with the exact resume command and
/// its draft is still parked where a reattach will find it.
///
/// MUTATION CHECK: pass `true` unconditionally for `reachable` in the
/// `PromptForked` arm. Expected failure: the notice claims an attached
/// session this client cannot reach, and the resume door disappears.
#[test]
fn an_unattachable_child_names_the_resume_command_instead_of_a_silent_loss() {
    let (mut model, _) = forkable_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "socket closed".to_owned(),
        },
    );
    driver.apply(&mut model, forked_reply("child"));

    assert_eq!(
        model.flash.as_deref(),
        Some("· forked — new session, not attached here · `haider resume child`")
    );
    assert!(
        model.sessions.iter().any(|row| row.id == sid("child")),
        "the daemon-minted child exists whether or not this client reached it"
    );
    assert_eq!(
        model.composer.text(),
        "newest prompt",
        "the draft is parked for the reattach, never dropped"
    );
}

/// A receipt naming the SOURCE as its own child would make the hand-off
/// overwrite the original's composer. Refused — the source is untouchable.
#[test]
fn a_receipt_that_names_the_source_as_its_child_is_refused() {
    let (mut model, session) = forkable_model();
    model.composer.set_text("half-typed source draft");
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::PromptForked {
            command_id: CommandId::new("cmd-prompt-fork"),
            source_session: session.clone(),
            session,
            worker_generation: 11,
            draft: SessionForkDraft {
                text: "newest prompt".to_owned(),
                attachments: attachments(),
            },
        },
    );
    assert_eq!(
        model.composer.text(),
        "half-typed source draft",
        "the source draft is never overwritten by a fork hand-off"
    );
    assert!(model.composer.attachments().is_empty());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("named the source as its own child")),
        "the refusal is honest: {:?}",
        model.flash
    );
}

/// A `session.fork` answered WITHOUT prompt provenance and a draft is the
/// legacy exact-node shape: the link reports a typed failure rather than
/// opening a child with an invented empty draft.
#[test]
fn a_legacy_fork_response_never_becomes_an_empty_draft() {
    let context = CommandContext::of(&LiveCommand::SessionForkPrompt {
        command_id: CommandId::new("cmd-prompt-fork"),
        session: sid("source"),
        worker_generation: 7,
        source_branch: None,
        seq: 3,
    });
    let metadata = serde_json::from_value(serde_json::json!({
        "cwd": "/workspace",
        "provider": "fake",
        "model": "fake-model",
        "max_tokens": 1_024_u64,
        "created_at_ms": 7_u64,
    }))
    .expect("metadata fixture decodes");
    let replies = map_response(
        &context,
        ResponseBody::SessionFork {
            session_id: sid("child"),
            source_session_id: sid("source"),
            source_branch_id: None,
            fork_node_id: haider_protocol::ids::NodeId::new("node"),
            fork_seq: 8,
            created_seq: 1,
            worker_generation: 11,
            metadata,
            forked_from: None,
            draft: None,
        },
    );
    assert!(
        matches!(replies.as_slice(), [LiveReply::Failed { .. }]),
        "expected a typed failure, got {replies:?}"
    );
}
