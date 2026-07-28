//! W3c3.2 — the W3c3.1 verifier's late findings, pinned through the loop
//! the binary runs.
//!
//! P1-A: EVERY demo-only surface is live-handled or honestly refused — the
//! silent discard is how `/compact` set `turn_active` and wedged a live
//! session forever. P1-B: the working set holds no ghosts — a retryable
//! `AttachFailed` on a non-active session used to leave an lru member with
//! no attachment and nothing in flight, which at cap starved the SELECTED
//! session permanently. P2-B: the login deadline is the driver's own
//! wakeup, not a hope that unrelated traffic arrives. P2-D: `/sessions
//! <n|id>` opens rows past the launcher's digit span. Plus the r2
//! completion pass: the demo `/sessions` stub stays a known command, and
//! the demo VFS never paints in live mode.
#![allow(clippy::expect_used)]

use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, DeviceId, EventId, LeaseId, SessionId};
use haider_rpc::{AttachmentId, SessionSummary};
use haider_tui::app::{AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::live::{ATTACHMENT_CAP, LOGIN_STAGE_TIMEOUT, LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, run_slash};

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model
}

fn sid(n: usize) -> SessionId {
    SessionId::new(format!("s-{n}"))
}

fn att(n: usize) -> AttachmentId {
    AttachmentId::new(format!("att-{n}"))
}

fn envelope_of(
    session: &SessionId,
    seq: u64,
    payload: haider_protocol::EventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("live-device"),
        authority_epoch: 1,
        worker_generation: 9,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

fn envelope(session: &SessionId, seq: u64) -> RawEnvelope {
    envelope_of(
        session,
        seq,
        haider_protocol::EventPayload::UserMessage {
            text: format!("row {seq}"),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        },
    )
}

fn pass(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    reply: Option<LiveReply>,
) -> Vec<LiveCommand> {
    live_pass(driver, model, reply, std::time::Instant::now()).commands
}

fn listed(n: usize) -> LiveReply {
    LiveReply::Listed {
        sessions: (0..n)
            .map(|index| SessionSummary {
                session_id: sid(index),
                head_seq: 0,
                worker_generation: 7,
                metadata: None,
            })
            .collect(),
        next_cursor: None,
    }
}

/// A live model with one attached session on screen.
fn attached_session(driver: &mut LiveDriver, model: &mut AppModel) {
    pass(driver, model, Some(listed(1)));
    model.open_session(&sid(0));
    pass(driver, model, None);
    pass(
        driver,
        model,
        Some(LiveReply::Attached {
            session: sid(0),
            attachment: att(0),
            worker_generation: 7,
            replay_through_seq: 0,
        }),
    );
}

// ---- P1-B: the working set holds no ghosts ------------------------------

/// A responsive fake daemon: every issued `Attach` is queued for an
/// `Attached` answer on a later pass — except the ones `refuse_once`
/// names, which are answered with ONE retryable `AttachFailed` each.
struct Responder {
    next_att: usize,
    replies: Vec<LiveReply>,
    refuse_once: Vec<SessionId>,
}

impl Responder {
    fn absorb(&mut self, issued: Vec<LiveCommand>) {
        for command in issued {
            if let LiveCommand::Attach { session, .. } = command {
                if let Some(slot) = self.refuse_once.iter().position(|held| *held == session) {
                    self.refuse_once.remove(slot);
                    self.replies.push(LiveReply::AttachFailed {
                        session,
                        code: "overloaded".into(),
                        message: "cap".into(),
                        retryable: true,
                    });
                } else {
                    self.next_att += 1;
                    self.replies.push(LiveReply::Attached {
                        session,
                        attachment: att(self.next_att),
                        worker_generation: 7,
                        replay_through_seq: 0,
                    });
                }
            }
        }
    }
}

/// Drain the fake daemon's queued answers through the loop until quiet.
fn settle(driver: &mut LiveDriver, model: &mut AppModel, daemon: &mut Responder) {
    while !daemon.replies.is_empty() {
        for reply in std::mem::take(&mut daemon.replies) {
            let issued = pass(driver, model, Some(reply));
            daemon.absorb(issued);
        }
    }
}

#[test]
fn a_failed_background_attach_never_starves_the_selected_session() {
    // VERIFIER P1-B. `ensure_attached` claims the lru slot at REQUEST time
    // (new in W3c3.1), but the `AttachFailed` arm released only the latch
    // for a retryable failure — leaving a member that was neither attached
    // nor attaching. At cap the ghost is the coldest slot, eviction picks
    // it, `attachments.get(&victim)` is `None`, and `ensure_attached`
    // refuses every later attach. The ghost holds no in-flight attach, so
    // no daemon response can EVER release it: the selected session starves
    // permanently, with nothing on screen to say why. Reproduced against
    // 4b280de exactly this way (0 attaches over 6 responsive passes).
    //
    // MUTATION CHECK: in `LiveDriver::apply`'s `AttachFailed` arm, replace
    // `self.release_slot(&session)` with `self.attaching.remove(&session)`
    // and this starves exactly that way.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(&mut driver, &mut model, Some(listed(ATTACHMENT_CAP + 2)));
    let mut daemon = Responder {
        next_att: 0,
        replies: Vec::new(),
        refuse_once: vec![sid(16)],
    };
    // Fill the working set to cap.
    for n in 0..ATTACHMENT_CAP {
        model.open_session(&sid(n));
        let issued = pass(&mut driver, &mut model, None);
        daemon.absorb(issued);
        settle(&mut driver, &mut model, &mut daemon);
    }
    assert_eq!(driver.working_set().len(), ATTACHMENT_CAP);
    // Select session 16; its attach is refused RETRYABLE while the user
    // has already moved back to session 0 — the failure lands on a session
    // that is no longer active, so no selection retry covers it.
    model.open_session(&sid(16));
    daemon.absorb(pass(&mut driver, &mut model, None));
    model.open_session(&sid(0));
    settle(&mut driver, &mut model, &mut daemon);
    assert!(
        driver.ghost_slots().is_empty(),
        "no lru member may sit with no attachment and nothing in flight: {:?}",
        driver.ghost_slots()
    );
    // Traffic on every attached member drifts any ghost to the lru FRONT —
    // the coldest, first-evicted slot.
    for n in 0..ATTACHMENT_CAP {
        if driver.is_attached(&sid(n)) {
            for candidate in 1..=daemon.next_att {
                let issued = pass(
                    &mut driver,
                    &mut model,
                    Some(LiveReply::Event {
                        attachment: att(candidate),
                        session: sid(n),
                        envelope: Box::new(envelope(&sid(n), 1)),
                    }),
                );
                daemon.absorb(issued);
            }
        }
    }
    settle(&mut driver, &mut model, &mut daemon);
    // Select session 17 against the responsive daemon.
    model.open_session(&sid(17));
    for _ in 0..6 {
        let issued = pass(&mut driver, &mut model, None);
        daemon.absorb(issued);
        settle(&mut driver, &mut model, &mut daemon);
        if driver.is_attached(&sid(17)) {
            break;
        }
    }
    assert!(
        driver.is_attached(&sid(17)),
        "the SELECTED session must attach against a responsive daemon; \
         working_set={:?} ghosts={:?}",
        driver.working_set(),
        driver.ghost_slots()
    );
    assert!(driver.ghost_slots().is_empty());
    // And the refused session went COLD, reachable by the next selection.
    assert!(
        driver.is_cold(&sid(16)),
        "the refused session is cold, not lost"
    );
}

#[test]
fn a_retryable_failure_on_the_attached_surface_retries_with_the_slot_reclaimed() {
    // The other half of the P1-B arm: the ACTIVE session's retry goes back
    // through `ensure_attached`, which re-claims slot and latch together.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(&mut driver, &mut model, Some(listed(1)));
    model.open_session(&sid(0));
    pass(&mut driver, &mut model, None);
    let issued = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::AttachFailed {
            session: sid(0),
            code: "overloaded".into(),
            message: "busy".into(),
            retryable: true,
        }),
    );
    assert_eq!(
        issued
            .iter()
            .filter(|command| matches!(command, LiveCommand::Attach { .. }))
            .count(),
        1,
        "the attached surface retries exactly once per failure: {issued:?}"
    );
    assert!(driver.ghost_slots().is_empty());
    // A PERMANENT failure releases the slot, deselects the surface, and
    // stays cold — no retry, THROUGH THE LOOP: the tail's `sync_selection`
    // must find nothing selected, or every failure reply becomes the next
    // attach and the pair ping-pong at wire speed forever (W3c3.2).
    //
    // MUTATION CHECK: delete the `!retryable` deselect branch from the
    // `AttachFailed` arm and the pass below issues an attach.
    let issued = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::AttachFailed {
            session: sid(0),
            code: "not_found".into(),
            message: "gone".into(),
            retryable: false,
        }),
    );
    assert!(
        issued.is_empty(),
        "a permanent failure is reported, never retried: {issued:?}"
    );
    assert!(
        model.active_session.is_none(),
        "the refused surface is deselected — the user's next selection is the retry"
    );
    assert!(driver.ghost_slots().is_empty());
    assert!(driver.is_cold(&sid(0)));
}

// ---- P1-A: every demo-only request is refused with a voice --------------

#[test]
fn the_driver_refuses_demo_vocabulary_aloud_and_unwinds_the_optimistic_state() {
    // VERIFIER P1-A, the backstop half. The reducer's gates make these
    // unreachable; if a future path forgets its gate, the driver must
    // flash and unwind rather than silently discard — the silent discard
    // is how `/compact` wedged a live session mid-turn forever.
    //
    // MUTATION CHECK: fold these arms back into the `ResetAllSessions`
    // no-op arm of `LiveDriver::handle_request` and every case below sees
    // `turn_active` stuck true with no flash.
    let cases: Vec<AppRequest> = vec![
        AppRequest::Compact,
        AppRequest::Talk,
        AppRequest::ChipSubmit {
            agent: "a1".into(),
            text: "steer".into(),
        },
        AppRequest::ChipClose { agent: "a1".into() },
        AppRequest::AuraSubmit {
            text: "orchestrate".into(),
            voice: false,
        },
        AppRequest::AuraTalk,
        AppRequest::ResetAura,
    ];
    for request in cases {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        attached_session(&mut driver, &mut model);
        // The fabrication a missed gate would have left behind.
        model.turn_active = true;
        model.listening = true;
        model.flash = None;
        let label = format!("{request:?}");
        model.requests.push(request);
        let issued = pass(&mut driver, &mut model, None);
        assert!(issued.is_empty(), "{label}: no live command exists for it");
        assert!(
            model
                .flash
                .as_deref()
                .is_some_and(|flash| flash.contains("demo only")),
            "{label}: the refusal must be audible, got {:?}",
            model.flash
        );
        assert!(!model.turn_active, "{label}: the fake mid-turn is unwound");
        assert!(!model.listening, "{label}: the fake hold is unwound");
    }
}

#[test]
fn live_compact_refuses_before_it_fabricates() {
    // VERIFIER P1-A, the named case: `/compact` set `turn_active = true`
    // and handed the driver a request it discarded — the session sat
    // mid-turn forever, with `/compact` itself answering "wait for the
    // turn to end".
    //
    // MUTATION CHECK: remove the `!self.mode.fabricates_locally()` branch
    // from the `"compact"` arm of `execute_slash` and `turn_active` below
    // sticks true.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attached_session(&mut driver, &mut model);
    run_slash(&mut model, "/compact");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("demo only")),
        "got {:?}",
        model.flash
    );
    assert!(!model.turn_active, "nothing was fabricated");
    assert!(
        pass(&mut driver, &mut model, None).is_empty(),
        "nothing was promised to the daemon"
    );
}

#[test]
fn live_push_to_talk_refuses_instead_of_listening_forever() {
    // The hold's 1.3 s timer lives in the demo driver; live mode would set
    // `listening` and nothing would ever clear it.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attached_session(&mut driver, &mut model);
    model.voice.enabled = true;
    model.handle_hit(Hit::TalkChip);
    assert!(!model.listening, "no un-clearable hold");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("demo only")),
        "got {:?}",
        model.flash
    );
    assert!(pass(&mut driver, &mut model, None).is_empty());
}

#[test]
fn live_aura_door_is_closed_at_the_one_entrance() {
    // `/aura` and the launcher's Aura row share `enter_aura` — the ONE
    // door. Everything behind it (`AuraSubmit`, `AuraTalk`, `ResetAura`)
    // is demo-driver vocabulary, so the stage would take a hold and sit in
    // `Listening` forever.
    let mut model = live_model();
    run_slash(&mut model, "/aura");
    assert_ne!(model.screen, Screen::Aura, "the stage is not entered");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("demo only")),
        "got {:?}",
        model.flash
    );
    model.flash = None;
    model.handle_hit(Hit::ExtraRow(haider_tui::app::LauncherRow::Aura));
    assert_ne!(
        model.screen,
        Screen::Aura,
        "the launcher row is the same door"
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|f| f.contains("demo only"))
    );
}

#[test]
fn live_subagent_steer_and_close_refuse_instead_of_destroying_the_text() {
    // Live chips are REAL (committed `AgentSpawned` envelopes route into
    // the chip tree), but steering and closing them are demo-driver beats
    // — there is no `agent.steer` RPC yet, so the typed text was silently
    // destroyed and the ✕ silently discarded.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attached_session(&mut driver, &mut model);
    let manifest = AgentManifest {
        agent: AgentId::new("a1"),
        role: AgentRole::Subagent,
        callsign: Some("Ammar".to_owned()),
        model_profile: "fable-5".to_owned(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new("lease-1"),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: None,
    };
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Event {
            attachment: att(0),
            session: sid(0),
            envelope: Box::new(envelope_of(
                &sid(0),
                1,
                haider_protocol::EventPayload::AgentSpawned(manifest),
            )),
        }),
    );
    assert!(!model.chips.is_empty(), "the live chip exists");
    model.handle_hit(Hit::ChipRow("a1".to_owned()));
    assert_eq!(model.screen, Screen::Subagent);
    // Steer: the typed text must be refused aloud, not destroyed.
    common::submit(&mut model, "steer the subagent");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("demo only")),
        "got {:?}",
        model.flash
    );
    // Close: the ✕ is refused; a live chip closes when its committed
    // `AgentChipState` says so.
    model.flash = None;
    model.handle_hit(Hit::ChipCloseBtn("a1".to_owned()));
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("demo only")),
        "got {:?}",
        model.flash
    );
    assert!(
        !model.chips[0].closed,
        "the chip's lifecycle belongs to the committed stream"
    );
    assert!(pass(&mut driver, &mut model, None).is_empty());
}

#[test]
fn live_esc_mid_turn_paints_nothing_the_daemon_did_not_commit() {
    // W3c3.1 r2: painting `Cancelled` + the note at Esc time says the run
    // ended before `turn.cancel` was even sent — and a daemon that rejects
    // the cancel leaves the screen lying. The committed `RunState` paints
    // it, exactly as it paints every other state.
    //
    // MUTATION CHECK: drop the `fabricates_locally` gate from the Esc arm
    // and the row count below grows by the fabricated note.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attached_session(&mut driver, &mut model);
    model.turn_active = true;
    let before = model.projection.entries().len();
    model.handle(key(KeyCode::Esc));
    assert_eq!(
        model.projection.entries().len(),
        before,
        "no locally painted cancellation"
    );
    assert!(
        model.requests.is_empty()
            || model
                .requests
                .iter()
                .all(|request| matches!(request, AppRequest::Interrupt)),
        "Esc raised at most the Interrupt request"
    );
    // DEMO still paints the settled interrupt locally (the sim's beat).
    let mut demo = launcher_model();
    common::hit_session_named(&mut demo, "billing-service");
    demo.turn_active = true;
    let before = demo.projection.entries().len();
    demo.handle(key(KeyCode::Esc));
    assert!(
        demo.projection.entries().len() > before,
        "demo settles the interrupt locally, as the sim does"
    );
}

#[test]
fn live_shell_builtins_refuse_instead_of_painting_the_fake_vfs() {
    // r2 sibling sweep: `ls`/`cd`/`mkdir` ran against the demo's FAKE
    // filesystem in live mode — invented files presented as the user's
    // real cwd, and `cd` retargeted the dir shown on real session rows.
    //
    // MUTATION CHECK: drop the `fabricates_locally` gate from the
    // `SHELL_CMDS` branch of `handle_submit` and the shellout below paints
    // the seeded VFS.
    let mut model = live_model();
    common::submit(&mut model, "ls");
    assert!(model.launcher_shellout.is_none(), "no fake listing painted");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("demo only")),
        "got {:?}",
        model.flash
    );
    // DEMO keeps the VFS: the sim's shell builtins are its own law.
    let mut demo = launcher_model();
    common::submit(&mut demo, "ls");
    assert!(
        demo.launcher_shellout.is_some(),
        "the demo VFS still answers"
    );
}

// ---- P2-B: the login deadline is the driver's own wakeup ----------------

#[test]
fn the_login_deadline_is_exposed_to_the_shell_as_a_wakeup() {
    // VERIFIER P2-B. `expire_login` runs only when `live_pass` runs, and
    // the select loop wakes on a keypress, a reply, or a tick gated on
    // `dirty`/`animated()` — none of which a quiet terminal facing a
    // wedged daemon produces. `next_deadline` is the arm `run_live` now
    // selects on, so the timeout fires by itself.
    //
    // MUTATION CHECK: make `LiveDriver::next_deadline` return `None`
    // unconditionally and the armed half below fails.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    assert!(
        driver.next_deadline().is_none(),
        "no card, no deadline, no busy wakeup"
    );
    let start = std::time::Instant::now();
    live_pass(&mut driver, &mut model, None, start);
    model.requests.push(AppRequest::LoginApi {
        provider: "anthropic".to_owned(),
        alias: None,
        secret: haider_rpc::SecretWire::new("sk-test-000000".to_owned()),
    });
    let issued = live_pass(&mut driver, &mut model, None, start).commands;
    assert!(
        issued
            .iter()
            .any(|command| matches!(command, LiveCommand::Stage { .. })),
        "the stage went out: {issued:?}"
    );
    let deadline = driver
        .next_deadline()
        .expect("an in-flight login arms the wakeup");
    assert_eq!(
        deadline,
        start + LOGIN_STAGE_TIMEOUT,
        "the deadline is the stage instant plus the documented timeout"
    );
    // The deadline firing expires the card THROUGH THE PASS — and disarms.
    live_pass(&mut driver, &mut model, None, start + LOGIN_STAGE_TIMEOUT);
    assert!(
        driver.next_deadline().is_none(),
        "an expired login disarms the wakeup — no busy loop"
    );
}

// ---- P2-D: /sessions opens what it lists --------------------------------

#[test]
fn sessions_opens_rows_past_the_digit_span_by_ordinal_and_id() {
    // VERIFIER P2-D. Raising the launcher to nine rows moved P1-6's
    // defect rather than closing it: row ten had no hit target, no digit
    // and no opener. `/sessions <n|id>` is the opener; the read is the
    // attach's own replay.
    //
    // MUTATION CHECK: make `open_listed_session` parse only ordinals and
    // the id half below fails.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(&mut driver, &mut model, Some(listed(12)));
    // Rows insert newest-first: ordinal 1 is s-11, ordinal 12 is s-0.
    run_slash(&mut model, "/sessions 12");
    assert_eq!(model.active_session.as_ref(), Some(&sid(0)));
    let issued = pass(&mut driver, &mut model, None);
    assert!(
        issued
            .iter()
            .any(|command| matches!(command, LiveCommand::Attach { .. })),
        "opening past the digit span attaches: {issued:?}"
    );
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/sessions s-5");
    assert_eq!(
        model.active_session.as_ref(),
        Some(&sid(5)),
        "the wire id is a coordinate too"
    );
    run_slash(&mut model, "/sessions nonsense");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("no such row")),
        "a miss says so: {:?}",
        model.flash
    );
}

#[test]
fn demo_sessions_is_a_known_stub_not_a_typo() {
    // r2 completion: the first cut removed the unconditional listing arm
    // and let demo `/sessions` fall into the typo catch-all — "unknown
    // command /sessions" while `/help` lists it.
    //
    // MUTATION CHECK: delete the demo `"sessions"` arm from
    // `execute_slash` and the assertion below reads "unknown command".
    let mut model = launcher_model();
    run_slash(&mut model, "/sessions");
    let flash = model.flash.as_deref().expect("the stub answers");
    assert!(
        flash.contains("demo stub"),
        "the stub names itself honestly: {flash}"
    );
    assert!(
        !flash.contains("unknown command"),
        "a documented command is never a typo: {flash}"
    );
    assert!(
        model.launcher_shellout.is_none(),
        "demo does not invent the listing the sim never had"
    );
}
