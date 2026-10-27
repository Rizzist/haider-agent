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
        payload: serde_json::to_value(payload)
            .expect("payload serializes")
            .into(),
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
            launch_origin: None,
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

fn origin_replay_variant(snapshot: bool) {
    use haider_protocol::session::{
        LaunchOriginPathKindV1, LaunchOriginPathV1, LaunchOriginV1, SessionLaunchOriginSelected,
    };
    let (mut model, _) = forkable_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(&mut model, forked_reply("origin-child"));
    let child = sid("origin-child");
    let attachment = AttachmentId::new("origin-child-attachment");
    let path = LaunchOriginPathV1 {
        kind: LaunchOriginPathKindV1::HomeRelative,
        display: Some("~/child".into()),
    };
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: child.clone(),
            attachment: attachment.clone(),
            worker_generation: 11,
            replay_through_seq: 4,
            launch_origin: snapshot.then(|| LaunchOriginV1 {
                subject_session_id: child.to_string(),
                open_id: "child-open".into(),
                revision: 1,
                path: path.clone(),
                recorded_at_ms: 1,
                selected_seq: 2,
            }),
        },
    );
    let initial = model.launch_origin.clone();
    let mut deliver = |seq, subject: &str, revision, display: &str| {
        let fact = SessionLaunchOriginSelected {
            subject_session_id: subject.into(),
            open_id: format!("open-{subject}-{revision}"),
            revision,
            path: LaunchOriginPathV1 {
                kind: LaunchOriginPathKindV1::HomeRelative,
                display: Some(display.into()),
            },
            recorded_at_ms: seq,
            workspace_materialized: Some(false),
        };
        let mut envelope = raw(&child, seq, &EventPayload::IdleDecayed);
        envelope.worker_generation = 11;
        envelope.render.prompt = PromptRender::Omit;
        envelope.payload = fact.to_payload_value().expect("origin payload").into();
        driver.apply(
            &mut model,
            LiveReply::Event {
                session: child.clone(),
                attachment: attachment.clone(),
                envelope: Box::new(envelope),
            },
        );
        model.launch_origin.clone()
    };
    assert_eq!(
        deliver(1, "source", 99, "~/parent"),
        initial,
        "copied parent history cannot claim a child open"
    );
    assert_eq!(
        deliver(2, child.as_str(), 1, "~/child"),
        Some((1, path.display))
    );
    assert_eq!(
        deliver(3, child.as_str(), 2, "~/child-reopened"),
        Some((2, Some("~/child-reopened".into())))
    );
    assert_eq!(
        deliver(4, child.as_str(), 1, "~/stale"),
        Some((2, Some("~/child-reopened".into()))),
        "older same-session replay cannot replace a newer revision"
    );
}

#[test]
fn origin_replay_ignores_parent_claim_without_attach_snapshot() {
    origin_replay_variant(false);
}

#[test]
fn origin_replay_ignores_parent_claim_above_child_snapshot_revision() {
    origin_replay_variant(true);
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

fn owner_summary(session: &SessionId, owner: &str) -> haider_rpc::SessionSummary {
    serde_json::from_value(serde_json::json!({
        "session_id": session, "head_seq": 0, "worker_generation": 7,
        "kind": "subagent", "owner_agent_id": owner
    }))
    .expect("owning-agent summary")
}

/// F2: root, delegated, and nested delegated prompt lanes have identical
/// chooser semantics on live delivery and cold/parked replay.
#[test]
fn owning_agent_prompts_survive_active_and_cold_replay() {
    for owner in [None, Some("delegated-owner"), Some("nested-owner")] {
        for cold in [false, true] {
            for surface in ["ordinal", "bare", "escape", "recall"] {
                let (mut model, session) = forkable_model();
                // Fresh row/replay so no unrelated history can hide a missing own prompt.
                model.back_to_launcher();
                model.sessions.clear();
                model.upsert_live_session(&session);
                if let Some(owner) = owner {
                    model.note_summary_counts(&owner_summary(&session, owner));
                }
                if !cold {
                    model.open_session(&session);
                }
                let mut prompt = raw(&session, 1, &user("own prompt"));
                prompt.agent_id = owner.map(haider_protocol::ids::AgentId::new);
                assert_eq!(model.route_raw(&prompt), RawOutcome::Applied);
                let mut other = raw(&session, 2, &user("other child's prompt"));
                other.agent_id = Some(haider_protocol::ids::AgentId::new("other-child"));
                assert_eq!(model.route_raw(&other), RawOutcome::Applied);
                if cold {
                    model.open_session(&session);
                }
                model.turn_active = false;
                model.requests.clear();
                assert_eq!(model.prompt_history.len(), 1, "{owner:?}/{cold}/{surface}");
                assert_eq!(model.prompt_history[0].text, "own prompt");
                match surface {
                    "ordinal" => run_slash(&mut model, "/fork 1"),
                    "bare" => {
                        run_slash(&mut model, "/fork");
                        assert!(model.backtrack.is_some());
                        model.handle_at(key(KeyCode::Char('f')), Instant::now());
                    }
                    "escape" => {
                        open_chooser(&mut model, Instant::now());
                        model.handle_at(key(KeyCode::Char('f')), Instant::now());
                    }
                    _ => {
                        run_slash(&mut model, "/history 1");
                        assert_eq!(model.composer.text(), "own prompt");
                        assert!(fork_request(&model).is_none());
                        continue;
                    }
                }
                assert_eq!(
                    fork_request(&model),
                    Some((session, 1)),
                    "{owner:?}/{cold}/{surface}"
                );
            }
        }
    }
}

/// A selected prompt keeps its source branch after either direction of a
/// branch switch; inactive-branch replay cannot retarget the cut.
#[test]
fn prompt_fork_uses_source_branch_after_switch_and_reopen() {
    for cold in [false, true] {
        for source in [None, Some("branch-b")] {
            let (mut model, session) = forkable_model();
            if cold {
                model.back_to_launcher();
            }
            assert_eq!(
                model.route_raw(&branch_created(&session, 4, "branch-b", "B")),
                RawOutcome::Applied
            );
            let mut prompt = raw(&session, 5, &user("source branch prompt"));
            prompt.branch_id = source.map(haider_protocol::ids::BranchId::new);
            assert_eq!(model.route_raw(&prompt), RawOutcome::Applied);
            if cold {
                model.open_session(&session);
            }
            model.turn_active = false;
            assert!(
                model
                    .switch_branch(Some(&haider_protocol::ids::BranchId::new("branch-b")))
                    .is_some()
            );
            if source.is_some() {
                assert!(model.switch_branch(None).is_some());
            }
            model.requests.clear();
            run_slash(&mut model, "/fork 1");
            let request = model.requests.iter().find_map(|r| match r {
                AppRequest::ForkFromPrompt {
                    source_branch, seq, ..
                } => Some((source_branch.clone(), *seq)),
                _ => None,
            });
            assert_eq!(
                request,
                Some((prompt.branch_id, 5)),
                "cold={cold} source={source:?}"
            );
        }
    }
}

fn branch_created(session: &SessionId, seq: u64, branch: &str, name: &str) -> RawEnvelope {
    let created = haider_protocol::branch::BranchCreated {
        branch: haider_protocol::branch::BranchDescriptor {
            branch_id: haider_protocol::ids::BranchId::new(branch),
            name: name.to_owned(),
            source_branch_id: None,
            fork_node_id: haider_protocol::ids::NodeId::new("node-1"),
            fork_seq: 1,
            created_seq: seq,
            created_at_ms: 0,
            head_node_id: haider_protocol::ids::NodeId::new("node-1"),
            head_seq: 1,
        },
    };
    let mut envelope = raw(session, seq, &EventPayload::IdleDecayed);
    envelope.payload = created
        .to_payload_value()
        .expect("branch fact serializes")
        .into();
    envelope
}

#[test]
fn child_chip_prompt_history_precedes_the_first_list_summary() {
    let (mut model, parent) = forkable_model();
    let child = sid("child-not-listed-yet");
    let mut chip = haider_tui::app::ChipModel::from_seed(
        haider_tui::mock::sample_seed_chip(2).expect("chip fixture"),
    );
    chip.agent = "early-owner".into();
    chip.child_session = Some(child.as_str().into());
    chip.children.clear();
    model.chips.push(chip);
    model.handle_hit(haider_tui::app::Hit::ChipRow("early-owner".into()));
    assert_eq!(model.screen, haider_tui::app::Screen::Subagent);
    let older_summary = serde_json::from_value(serde_json::json!({
        "session_id": child, "head_seq": 0, "worker_generation": 7, "kind": "subagent"
    }))
    .expect("older summary without owner or metrics");
    model.note_summary_counts(&older_summary);
    let mut prompt = raw(&child, 1, &user("early own prompt"));
    prompt.agent_id = Some(haider_protocol::ids::AgentId::new("early-owner"));
    assert_eq!(model.route_raw(&prompt), RawOutcome::Applied);
    assert_eq!(
        model.prompt_history.len(),
        2,
        "child replay stays out of the parent history"
    );
    model.open_session(&child);
    model.turn_active = false;
    model.requests.clear();
    assert_eq!(model.prompt_history.len(), 1);
    assert_eq!(model.prompt_history[0].text, "early own prompt");
    run_slash(&mut model, "/fork 1");
    assert_eq!(fork_request(&model), Some((child, 1)));
    model.open_session(&parent);
    assert_eq!(model.prompt_history.len(), 2);
}

#[test]
fn untyped_metrics_cannot_replace_the_parent_prompt_owner() {
    let (mut model, parent) = forkable_model();
    let older_summary = serde_json::from_value(serde_json::json!({
        "session_id": parent, "head_seq": 3, "worker_generation": 7,
        "agent_metrics": {"agent": "other-child", "session_id": parent,
            "head_seq": 3, "started_at_ms": 1, "live": true, "tool_attempts": 0}
    }))
    .expect("summary without lineage authority");
    model.note_summary_counts(&older_summary);
    assert_eq!(
        model.route_raw(&raw(&parent, 4, &user("parent remains owner"))),
        RawOutcome::Applied
    );
    let mut other = raw(&parent, 5, &user("other child remains excluded"));
    other.agent_id = Some(haider_protocol::ids::AgentId::new("other-child"));
    assert_eq!(model.route_raw(&other), RawOutcome::Applied);
    assert_eq!(model.prompt_history.len(), 3);
    assert_eq!(model.prompt_history[0].text, "parent remains owner");
    model.turn_active = false;
    model.requests.clear();
    run_slash(&mut model, "/fork 1");
    assert_eq!(fork_request(&model), Some((parent, 4)));
}

#[test]
fn stale_root_summary_cannot_erase_a_committed_manifest_owner() {
    let (mut model, _) = forkable_model();
    let child = sid("manifest-before-roster");
    let mut chip = haider_tui::app::ChipModel::from_seed(
        haider_tui::mock::sample_seed_chip(2).expect("chip fixture"),
    );
    chip.agent = "manifest-owner".into();
    chip.child_session = Some(child.as_str().into());
    chip.children.clear();
    model.chips.push(chip);
    model.handle_hit(haider_tui::app::Hit::ChipRow("manifest-owner".into()));
    let stale = serde_json::from_value(serde_json::json!({
        "session_id": child,
        "head_seq": 1,
        "worker_generation": 7,
        "kind": "root"
    }))
    .expect("roster read before delegation was committed");
    model.note_summary_counts(&stale);
    let mut prompt = raw(&child, 1, &user("owned despite delayed roster"));
    prompt.agent_id = Some(haider_protocol::ids::AgentId::new("manifest-owner"));
    assert_eq!(model.route_raw(&prompt), RawOutcome::Applied);
    model.note_summary_counts(&owner_summary(&child, "manifest-owner"));
    model.open_session(&child);
    model.turn_active = false;
    model.requests.clear();
    assert_eq!(model.prompt_history.len(), 1);
    run_slash(&mut model, "/fork 1");
    assert_eq!(fork_request(&model), Some((child, 1)));
}

/// Replay can beat the ownership roster, including after a cold reopen. Never
/// discard scoped prompts just because their owning summary has not arrived.
#[test]
fn late_owner_summary_recovers_scoped_prompt_history() {
    for cold in [false, true] {
        let (mut model, session) = forkable_model();
        model.back_to_launcher();
        model.sessions.clear();
        model.upsert_live_session(&session);
        if !cold {
            model.open_session(&session);
        }
        let mut prompt = raw(&session, 1, &user("replayed before owner"));
        prompt.agent_id = Some(haider_protocol::ids::AgentId::new("late-owner"));
        assert_eq!(model.route_raw(&prompt), RawOutcome::Applied);
        model.note_summary_counts(&owner_summary(&session, "late-owner"));
        if cold {
            model.open_session(&session);
        }
        model.turn_active = false;
        model.requests.clear();
        assert_eq!(model.prompt_history.len(), 1, "cold={cold}");
        run_slash(&mut model, "/fork 1");
        assert_eq!(fork_request(&model), Some((session, 1)));
    }
}

/// Deleting a delegation parent can leave the child as a Root. The latest
/// explicit role must replace the old owner without mixing agent histories.
#[test]
fn fresh_root_summary_reconciles_ownership_active_and_cold() {
    for cold in [false, true] {
        let (mut model, session) = forkable_model();
        model.back_to_launcher();
        model.sessions.clear();
        model.upsert_live_session(&session);
        model.note_summary_counts(&owner_summary(&session, "former-owner"));
        if !cold {
            model.open_session(&session);
        }
        let mut prompt = raw(&session, 1, &user("delegated history"));
        prompt.agent_id = Some(haider_protocol::ids::AgentId::new("former-owner"));
        assert_eq!(model.route_raw(&prompt), RawOutcome::Applied);
        let root = serde_json::from_value(serde_json::json!({
            "session_id": session, "head_seq": 1, "worker_generation": 7, "kind": "root"
        }))
        .expect("fresh Root snapshot");
        model.note_summary_counts(&root);
        assert_eq!(
            model.route_raw(&raw(&session, 2, &user("root history"))),
            RawOutcome::Applied
        );
        if cold {
            model.open_session(&session);
        }
        assert_eq!(model.prompt_history.len(), 1, "cold={cold}");
        assert_eq!(model.prompt_history[0].text, "root history");
        model.turn_active = false;
        model.requests.clear();
        run_slash(&mut model, "/fork 1");
        assert_eq!(fork_request(&model), Some((session.clone(), 2)));
        // Positive ownership metadata can arrive after replay too, and must
        // restore only that scope, not the more recent unscoped prompt.
        model.note_summary_counts(&owner_summary(&session, "former-owner"));
        assert_eq!(model.prompt_history.len(), 1);
        assert_eq!(model.prompt_history[0].text, "delegated history");
    }
}

#[test]
fn stale_root_summary_behind_replay_cannot_revoke_owner() {
    for cold in [false, true] {
        let (mut model, session) = forkable_model();
        model.back_to_launcher();
        model.sessions.clear();
        model.upsert_live_session(&session);
        model.note_summary_counts(&owner_summary(&session, "current-owner"));
        if !cold {
            model.open_session(&session);
        }
        let mut prompt = raw(&session, 1, &user("still owned"));
        prompt.agent_id = Some(haider_protocol::ids::AgentId::new("current-owner"));
        assert_eq!(model.route_raw(&prompt), RawOutcome::Applied);
        let root = serde_json::from_value(serde_json::json!({
            "session_id": session, "head_seq": 0, "worker_generation": 7, "kind": "root"
        }))
        .expect("stale Root snapshot");
        model.note_summary_counts(&root);
        if cold {
            model.open_session(&session);
        }
        assert_eq!(model.prompt_history.len(), 1, "cold={cold}");
    }
}

#[test]
fn scoped_retraction_cannot_resurrect_pending_or_remove_another_agent() {
    for cold in [false, true] {
        for retract_owner in ["late-owner", "another-owner"] {
            let (mut model, session) = forkable_model();
            model.back_to_launcher();
            model.sessions.clear();
            model.upsert_live_session(&session);
            if !cold {
                model.open_session(&session);
            }
            let mut prompt = raw(&session, 1, &user("pending prompt"));
            prompt.agent_id = Some(haider_protocol::ids::AgentId::new("late-owner"));
            assert_eq!(model.route_raw(&prompt), RawOutcome::Applied);
            let mut retracted = raw(&session, 2, &EventPayload::IdleDecayed);
            retracted.agent_id = Some(haider_protocol::ids::AgentId::new(retract_owner));
            retracted.payload = haider_protocol::retraction::PromptRetractedV1 {
                prompt_seq: 1,
                prompt_node_id: haider_protocol::ids::NodeId::new("pending-node"),
                text: "pending prompt".into(),
                attachments: vec![],
            }
            .to_payload_value()
            .expect("scoped retraction")
            .into();
            assert_eq!(model.route_raw(&retracted), RawOutcome::Applied);
            model.note_summary_counts(&owner_summary(&session, "late-owner"));
            if cold {
                model.open_session(&session);
            }
            assert_eq!(
                model.prompt_history.len(),
                usize::from(retract_owner != "late-owner"),
                "cold={cold} retractor={retract_owner}"
            );
        }
    }
}
