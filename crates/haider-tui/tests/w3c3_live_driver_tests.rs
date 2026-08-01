//! W3c3 M2 — `LiveDriver` laws (report R11 cut 4, §6.3's live test list).
//!
//! The driver is a pure state machine on purpose, so every law here is
//! pinned WITHOUT a daemon: the bounded priority working set and its LRU
//! eviction, the reconnect resume from per-session cursors, the launcher's
//! create→attach→submit order, unknown-attachment rejection, and menu
//! answers at their exact committed opening coordinates.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, DeviceId, EventId, MenuId, RunId, SessionId};
use haider_protocol::menu::{
    AnswerVia, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_rpc::{AttachmentId, CommandId, SessionSummary, SubmitDisposition};
use haider_tui::app::{AppModel, AppRequest, OutboundAnswer, RuntimeMode, Screen};
use haider_tui::live::{ATTACHMENT_CAP, LiveCommand, LiveDriver, LiveReply};

mod common;
use common::{key, launcher_model};

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model
}

fn sid(n: usize) -> SessionId {
    SessionId::new(format!("s-{n}"))
}

fn attachment(n: usize) -> AttachmentId {
    AttachmentId::new(format!("att-{n}"))
}

fn summary(n: usize, head_seq: u64) -> SessionSummary {
    SessionSummary {
        session_id: sid(n),
        head_seq,
        worker_generation: 7,
        metadata: None,
    }
}

fn envelope(session: &SessionId, seq: u64, payload: &EventPayload) -> RawEnvelope {
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

fn user(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

/// Attach `n` sessions through the real reply path (list → attach → attached).
fn attach_all(driver: &mut LiveDriver, model: &mut AppModel, count: usize) {
    let sessions = (0..count).map(|n| summary(n, 0)).collect();
    driver.apply(
        model,
        LiveReply::Listed {
            sessions,
            next_cursor: None,
        },
    );
    for n in 0..count {
        let commands = driver.ensure_attached(model, &sid(n));
        assert!(
            commands.contains(&LiveCommand::Attach {
                session: sid(n),
                after_seq: 0
            }),
            "session {n} must be attached"
        );
        driver.apply(
            model,
            LiveReply::Attached {
                session: sid(n),
                attachment: attachment(n),
                worker_generation: 7,
                replay_through_seq: 0,
            },
        );
    }
}

// ---- the bounded priority working set --------------------------------

#[test]
fn the_working_set_lru_detaches_before_the_seventeenth_attach() {
    // Report §6.3: "LRU-detaches before the 17th attach, and leaves cold
    // sessions listable/readable". The daemon's per-connection ceiling is
    // 16; a client that simply asks for a 17th gets rejected and silently
    // stops receiving one session's events.
    //
    // MUTATION CHECK: change the `self.lru.len() >= ATTACHMENT_CAP` test in
    // `LiveDriver::ensure_attached` to `>` and the Detach disappears from
    // the 17th attach's commands.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, ATTACHMENT_CAP);
    assert_eq!(driver.working_set().len(), ATTACHMENT_CAP);

    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(99, 0)],
            next_cursor: None,
        },
    );
    let commands = driver.ensure_attached(&model, &sid(99));
    assert_eq!(
        commands,
        vec![
            // The COLDEST holder goes first — session 0 was touched first.
            LiveCommand::Detach {
                attachment: attachment(0)
            },
            LiveCommand::Attach {
                session: sid(99),
                after_seq: 0
            },
        ],
        "the detach must precede the attach, so 17 are never held at once"
    );
    assert!(
        !driver.is_attached(&sid(0)),
        "the evicted session released its attachment"
    );
    assert!(
        driver.is_cold(&sid(0)),
        "…and stays listable/readable as a cold session"
    );
    assert!(driver.working_set().len() <= ATTACHMENT_CAP);
}

#[test]
fn eviction_never_takes_the_attached_surface_or_a_session_awaiting_you() {
    // R11 cut 4's priority: active first, then running/pending-menu. Losing
    // the attachment of a session holding an unanswered card means missing
    // exactly the events the user is waiting on.
    //
    // MUTATION CHECK: delete the `is_hot` branch in `LiveDriver::evictable`
    // and session 1 (the pending-menu holder) is chosen as the victim.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, ATTACHMENT_CAP);

    // Session 0 (the coldest) is ATTACHED; session 1 holds an open card.
    model.open_session(&sid(0));
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(1),
            session: sid(1),
            envelope: Box::new(envelope(
                &sid(1),
                1,
                &EventPayload::MenuOpened(card("hot-card", MenuScope::Session)),
            )),
        },
    );

    let commands = driver.ensure_attached(&model, &sid(99));
    let victim = commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::Detach { attachment } => Some(attachment.clone()),
            _ => None,
        })
        .expect("an eviction happened");
    assert_ne!(
        victim,
        attachment(0),
        "the ATTACHED surface is never evicted"
    );
    assert_ne!(
        victim,
        attachment(1),
        "a session holding an unanswered card is never evicted while colder ones exist"
    );
}

// ---- reconnect --------------------------------------------------------

#[test]
fn reconnect_restores_the_working_set_after_each_sessions_last_applied_cursor() {
    // Report §6.3: "reconnect restores the bounded priority working set
    // after its last applied cursors". Resuming from anything else — the
    // server's `last_queued_seq`, a driver-side copy — either replays
    // applied history or skips history that was never applied.
    //
    // MUTATION CHECK: make `cursor_of` return `None` unconditionally (i.e.
    // resume from 0) and the after_seq assertions below fail.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, 3);
    model.open_session(&sid(1));

    // Different progress per session.
    for seq in 1..=4 {
        driver.apply(
            &mut model,
            LiveReply::Event {
                attachment: attachment(1),
                session: sid(1),
                envelope: Box::new(envelope(&sid(1), seq, &user("attached"))),
            },
        );
    }
    for seq in 1..=2 {
        driver.apply(
            &mut model,
            LiveReply::Event {
                attachment: attachment(2),
                session: sid(2),
                envelope: Box::new(envelope(&sid(2), seq, &user("background"))),
            },
        );
    }

    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "peer closed".to_owned(),
        },
    );
    assert!(
        (0..3).all(|n| !driver.is_attached(&sid(n))),
        "every attachment died with the socket"
    );
    assert_eq!(
        driver.working_set().len(),
        3,
        "…but the working SET — what to restore — survives the disconnect"
    );

    let commands = driver.apply(&mut model, LiveReply::Reconnected);
    assert_eq!(
        commands.first(),
        Some(&LiveCommand::List { cursor: None }),
        "a fresh connection re-lists first"
    );
    let attaches: Vec<(SessionId, u64)> = commands
        .iter()
        .filter_map(|command| match command {
            LiveCommand::Attach { session, after_seq } => Some((session.clone(), *after_seq)),
            _ => None,
        })
        .collect();
    assert_eq!(
        attaches.first(),
        Some(&(sid(1), 4)),
        "the ATTACHED surface reattaches first, after its own last applied seq"
    );
    assert!(
        attaches.contains(&(sid(2), 2)),
        "a background session resumes after ITS cursor, not the attached one's"
    );
    assert!(
        attaches.contains(&(sid(0), 0)),
        "a session that applied nothing resumes from zero"
    );
}

#[test]
fn a_reconnect_resends_the_outbox_under_the_same_durable_command_ids() {
    // §6.4: "a lost mutation response does not duplicate session, turn,
    // cancel, or login". The daemon deduplicates by durable receipt, so the
    // resend MUST carry the original command id — a fresh id is a second
    // command, and a second command is a second turn.
    //
    // MUTATION CHECK: mint a fresh id in `LiveDriver::resume` instead of
    // replaying `self.outbox` verbatim and the id equality below fails.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, 1);
    model.open_session(&sid(0));

    let issued = driver.handle_request(
        &mut model,
        AppRequest::SubmitText {
            text: "do the thing".to_owned(),
            voice: false,
            title: false,
            branch: None,
        },
    );
    let original = issued
        .iter()
        .find_map(|command| command.command_id().cloned())
        .expect("the submit is durable");
    assert_eq!(driver.outbox_len(), 1, "the mutation is in the outbox");

    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "pong timeout".to_owned(),
        },
    );
    let resumed = driver.apply(&mut model, LiveReply::Reconnected);
    assert!(
        !resumed
            .iter()
            .any(|command| matches!(command, LiveCommand::Submit { .. })),
        "a session-scoped mutation must NOT ride the reconnect: `turn.submit` \
         needs an ESTABLISHED control attachment, and racing the attach earns \
         a non-retryable capability_denied that would retire the user's turn \
         (review P1-4)"
    );
    // It rides the ATTACH RESPONSE instead — the same discipline the
    // create→attach→submit path already had.
    let resent_on_attach = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(0),
            attachment: attachment(0),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    let resent: Vec<&CommandId> = resent_on_attach
        .iter()
        .filter_map(LiveCommand::command_id)
        .collect();
    assert_eq!(
        resent,
        vec![&original],
        "the outbox resends the SAME durable command id, exactly once"
    );

    // The response finally lands: the outbox retires and a later reconnect
    // resends nothing.
    driver.apply(
        &mut model,
        LiveReply::Submitted {
            command_id: original,
            session: sid(0),
            worker_generation: 7,
            disposition: SubmitDisposition::Started,
        },
    );
    assert_eq!(driver.outbox_len(), 0);
    driver.apply(&mut model, LiveReply::Reconnected);
    let again = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(0),
            attachment: attachment(0),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    assert!(
        again.iter().all(|command| command.command_id().is_none()),
        "a retired mutation is never resent"
    );
}

// ---- attachment routing ----------------------------------------------

#[test]
fn events_for_an_unknown_attachment_are_rejected() {
    // Report §6.3: "attach response precedes first event and unknown
    // attachment ids are rejected". An event routed by session id alone
    // would resurrect a session we deliberately let go cold.
    //
    // MUTATION CHECK: drop the `self.routes.get(attachment)` lookup in
    // `LiveDriver::on_event` and the transcript grows a row.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, 1);
    let before = rows(&model, &sid(0));

    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(404),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 1, &user("ghost"))),
        },
    );
    assert_eq!(
        rows(&model, &sid(0)),
        before,
        "an event on an attachment we do not hold changes nothing"
    );

    // The same envelope through the attachment we DO hold applies.
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 1, &user("real"))),
        },
    );
    assert_eq!(rows(&model, &sid(0)), before + 1);
}

#[test]
fn a_gap_reattaches_exactly_once_through_the_single_authority() {
    // The strict gap law END TO END, through the SAME wiring `run_live`
    // uses: the reducer stops and emits `AppRequest::Reattach`; the driver
    // performs detach+attach when that request is drained. Exactly one
    // pair, ever.
    //
    // This composes both halves deliberately. They used to be tested
    // separately — the driver's `[Detach, Attach]` here, the model's one
    // `Reattach` in w3c3_router_tests — and BOTH fired in production, so a
    // single gap opened two attachments and detached only one: a permanent
    // slot against the daemon's 16-per-connection ceiling plus duplicate
    // delivery of every later envelope (review P1-3). A seam-blind pair of
    // green tests is exactly how that survived.
    //
    // MUTATION CHECK: re-add a `[Detach, Attach]` return to the
    // `RawOutcome::Gap` arm of `LiveDriver::on_event` and the "exactly one"
    // count below becomes two.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, 1);
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 1, &user("one"))),
        },
    );
    model.requests.clear();

    let from_driver = driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 5, &user("five"))),
        },
    );
    assert!(
        from_driver.is_empty(),
        "the driver observes the gap; it does not act on it independently"
    );

    // Now drain the reducer's request. NOTE (W3c3.1): this hand-drains the
    // request to isolate `handle_request`; hand-copying the loop is exactly
    // how the SECOND attach authority hid, so the composed law now lives in
    // `w3c31_fix_tests`, which calls `runtime::live_pass` — the function the
    // shipping loop calls.
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    assert_eq!(
        requests,
        vec![AppRequest::Reattach {
            session: sid(0),
            after_seq: 1
        }],
        "one gap, one reattach request"
    );
    let mut issued = Vec::new();
    for request in requests {
        issued.extend(driver.handle_request(&mut model, request));
    }
    assert_eq!(
        issued,
        vec![
            LiveCommand::Detach {
                attachment: attachment(0)
            },
            LiveCommand::Attach {
                session: sid(0),
                after_seq: 1
            },
        ],
        "…and exactly one detach+attach pair, in that order"
    );
    assert_eq!(
        issued
            .iter()
            .filter(|command| matches!(command, LiveCommand::Attach { .. }))
            .count(),
        1,
        "never two attaches for one gap — the second would leak a slot"
    );
}

// ---- the launcher's create → attach → submit order -------------------

#[test]
fn the_live_launcher_creates_no_row_or_session_until_the_daemon_answers() {
    // Report §6.3: "launcher does not create a row/session until daemon
    // responses/events arrive". A locally fabricated row has to be
    // reconciled with the truth that follows — and reconciliation is where
    // duplicate rows and orphaned drafts come from.
    //
    // MUTATION CHECK: delete the `RuntimeMode::Live` branch in
    // `submit_composer`'s launcher arm (so `new_session` runs) and the
    // "nothing local happened" assertions below fail immediately.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    assert_eq!(model.sessions.len(), 0);

    for c in "ship the thing".chars() {
        model.handle(key(ratatui::crossterm::event::KeyCode::Char(c)));
    }
    model.handle(key(ratatui::crossterm::event::KeyCode::Enter));

    assert_eq!(model.sessions.len(), 0, "no row was fabricated");
    assert!(model.active_session.is_none(), "no session was attached");
    assert_eq!(model.screen, Screen::Launcher, "the screen did not flip");
    assert!(!model.turn_active, "no turn was started locally");
    let create = model
        .requests
        .iter()
        .find(|request| matches!(request, AppRequest::CreateSession { .. }))
        .cloned()
        .expect("the launcher asked the daemon to create a session");

    // The daemon's create is issued…
    let commands = driver.handle_request(&mut model, create);
    let command_id = commands
        .iter()
        .find_map(LiveCommand::command_id)
        .cloned()
        .expect("session.create is durable");
    assert!(matches!(commands.first(), Some(LiveCommand::Create { .. })));
    assert_eq!(model.sessions.len(), 0, "still nothing local");

    // …and only its RESPONSE brings a row, an attach and the first turn,
    // in that order.
    let after = driver.apply(
        &mut model,
        LiveReply::Created {
            command_id,
            session: sid(1),
            worker_generation: 7,
            cwd: "~/dev".to_owned(),
            model: "fable-5".to_owned(),
        },
    );
    assert_eq!(model.sessions.len(), 1, "the daemon's session became a row");
    assert_eq!(model.sessions[0].id, sid(1), "…under the DAEMON's id");
    assert_eq!(model.active_session.as_ref(), Some(&sid(1)));
    assert_eq!(
        after,
        vec![LiveCommand::Attach {
            session: sid(1),
            after_seq: 0
        }],
        "the create response asks for the ATTACHMENT and nothing else"
    );
    assert!(
        !after
            .iter()
            .any(|command| matches!(command, LiveCommand::Submit { .. })),
        "the turn must NOT ride the create response: `turn.submit` requires an \
         ESTABLISHED control attachment, and issuing both at once races the \
         daemon into `capability_denied` (found by pty-probe-live)"
    );

    // …and the turn follows the ATTACH RESPONSE — create → attach → submit,
    // each waiting for the last.
    let submitted = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(1),
            attachment: attachment(1),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    assert!(
        matches!(
            submitted.first(),
            Some(LiveCommand::Submit { text, session, .. })
                if text == "ship the thing" && *session == sid(1)
        ),
        "the first turn carries the launcher's text, once the attachment exists"
    );

    // A later attach (a reconnect's reattach, say) may RESEND the turn —
    // it is still unacknowledged and lives in the outbox — but only ever
    // under the SAME durable command id. A fresh id would be a second
    // command, and a second command is a second turn.
    let first_submit = submitted
        .iter()
        .find_map(LiveCommand::command_id)
        .cloned()
        .expect("the submit is durable");
    let again = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(1),
            attachment: attachment(1),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    assert_eq!(
        again
            .iter()
            .filter_map(LiveCommand::command_id)
            .collect::<Vec<_>>(),
        vec![&first_submit],
        "a later attach resends the SAME turn, never a second one"
    );
    assert_eq!(
        driver.outbox_len(),
        1,
        "…and it is still the one unacknowledged mutation"
    );
}

#[test]
fn a_cold_session_attaches_only_when_it_is_selected_and_only_once() {
    // R11 cut 4: "entering live mode … attaches only on selection". A
    // launcher that eagerly attached to everything it listed would burn the
    // whole working set before the user chose anything — and a SECOND
    // terminal would never see a session's history at all if selection did
    // not attach (found by pty-probe-live's §6.4 second-terminal row: the
    // row opened to an empty transcript).
    //
    // MUTATION CHECK: make `sync_selection` return `Vec::new()` and the
    // attach below never happens. (The call site itself is
    // `runtime::live_pass`, and `w3c31_fix_tests` drives THAT — this test
    // pins the driver half in isolation.)
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(4, 9), summary(5, 3)],
            next_cursor: None,
        },
    );
    assert!(
        driver.sync_selection(&model).is_empty(),
        "listing alone attaches nothing"
    );
    assert!(driver.is_cold(&sid(4)) && driver.is_cold(&sid(5)));

    model.open_session(&sid(4));
    assert_eq!(
        driver.sync_selection(&model),
        vec![LiveCommand::Attach {
            session: sid(4),
            after_seq: 0
        }],
        "selecting a cold session attaches it from its own cursor"
    );
    // The loop calls this every pass: it must not re-issue while the first
    // attach is still in flight.
    assert!(
        driver.sync_selection(&model).is_empty(),
        "one attach per selection, not one per frame"
    );
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(4),
            attachment: attachment(4),
            worker_generation: 7,
            replay_through_seq: 9,
        },
    );
    assert!(
        driver.sync_selection(&model).is_empty(),
        "…and none once it is attached"
    );
    assert!(driver.is_attached(&sid(4)));
    assert!(
        driver.is_cold(&sid(5)),
        "the session nobody chose stays cold"
    );
}

#[test]
fn esc_cancels_the_run_the_committed_stream_says_is_running_or_nothing() {
    // §6.4: "turn cancellation … reaches one terminal state". The client's
    // half is naming the RIGHT run: a cancel carries a `run_id`, and the
    // only honest source is the committed envelope stream. An invented id
    // is a command the daemon can only reject — which the user reads as
    // "Esc did nothing" — and a STALE id could name a run that already
    // ended while a later one is live.
    //
    // MUTATION CHECK: in `LiveDriver::handle_request`'s Interrupt arm,
    // replace the `active_run` lookup with `RunId::new(String::new())` and
    // both the no-run and the correct-run assertions below fail.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, 1);
    model.open_session(&sid(0));

    // Nothing running: Esc issues nothing at all.
    assert!(
        driver
            .handle_request(&mut model, AppRequest::Interrupt { branch: None })
            .is_empty(),
        "with no live run there is nothing to cancel"
    );

    // A run starts, named by the committed envelope.
    let mut running = envelope(
        &sid(0),
        1,
        &EventPayload::RunState(haider_protocol::state::RunState::Streaming),
    );
    running.run_id = Some(RunId::new("run-live-1"));
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(running),
        },
    );
    let commands = driver.handle_request(&mut model, AppRequest::Interrupt { branch: None });
    assert!(
        matches!(
            commands.first(),
            Some(LiveCommand::Cancel { run_id, session, .. })
                if *run_id == RunId::new("run-live-1") && *session == sid(0)
        ),
        "Esc cancels the run the stream named, got {commands:?}"
    );

    // The run terminalizes: Esc goes back to cancelling nothing, rather
    // than re-cancelling a finished run.
    let mut done = envelope(
        &sid(0),
        2,
        &EventPayload::RunState(haider_protocol::state::RunState::Done),
    );
    done.run_id = Some(RunId::new("run-live-1"));
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(done),
        },
    );
    assert!(
        driver
            .handle_request(&mut model, AppRequest::Interrupt { branch: None })
            .is_empty(),
        "a terminal run releases the cancel target"
    );
}

#[test]
fn live_mid_turn_input_reaches_the_daemon_instead_of_a_local_queue() {
    // REVIEW P1-1. The mid-turn arm of `submit_composer` is reached in both
    // modes. The demo parks the text in `msg_queue` (drained only by
    // `DemoDriver::finish_turn`) or paints a local steer row with a note
    // promising delivery — neither of which exists live, so in live mode
    // every follow-up the user typed while the agent worked was silently
    // destroyed, and the steer branch fabricated a transcript row the
    // daemon never committed (a direct R11 cut 4 violation).
    //
    // MUTATION CHECK: delete the `RuntimeMode::Live` branch from
    // `submit_composer`'s mid-turn arm and both assertions below fail — the
    // request never appears and the local row does.
    for (queue_mode, expected) in [
        (false, haider_protocol::DeliveryMode::Steer),
        (true, haider_protocol::DeliveryMode::Queue),
    ] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        attach_all(&mut driver, &mut model, 1);
        model.open_session(&sid(0));
        model.screen = Screen::Session;
        model.turn_active = true;
        model.queue_mode = queue_mode;
        let rows_before = model.projection.entries().len();

        for c in "one more thing".chars() {
            model.handle(key(ratatui::crossterm::event::KeyCode::Char(c)));
        }
        model.handle(key(ratatui::crossterm::event::KeyCode::Enter));

        assert!(
            model.msg_queue.is_empty(),
            "live mid-turn input must not be parked in a queue nothing drains"
        );
        assert_eq!(
            model.projection.entries().len(),
            rows_before,
            "…nor painted as a row the daemon never committed"
        );
        let requests: Vec<AppRequest> = model.requests.drain(..).collect();
        let mut issued = Vec::new();
        for request in requests {
            issued.extend(driver.handle_request(&mut model, request));
        }
        assert!(
            matches!(
                issued.first(),
                Some(LiveCommand::Submit { text, mode, .. })
                    if text == "one more thing" && *mode == expected
            ),
            "mid-turn input rides turn.submit with the user's delivery mode, \
             got {issued:?}"
        );
    }
}

#[test]
fn a_failed_attach_releases_its_latch_so_the_session_is_not_wedged() {
    // REVIEW P1-5. `sync_selection` latches one attach per selection so an
    // idle loop pass cannot re-issue it every frame. An attach that FAILS
    // used to clear nothing and attaches are not in the outbox, so the
    // selected session was un-attachable for the life of the connection —
    // an empty transcript behind a flash that had already scrolled away.
    //
    // MUTATION CHECK: delete `self.attaching.remove(&session)` from the
    // `AttachFailed` arm and the retry below never happens.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(3, 0)],
            next_cursor: None,
        },
    );
    model.open_session(&sid(3));
    assert_eq!(driver.sync_selection(&model).len(), 1, "one attach issued");
    assert!(driver.sync_selection(&model).is_empty(), "latched");

    // A RETRYABLE failure releases the latch and retries at once.
    let retried = driver.apply(
        &mut model,
        LiveReply::AttachFailed {
            session: sid(3),
            code: haider_rpc::ERROR_CODE_OVERLOADED.to_owned(),
            message: "attachment budget".to_owned(),
            retryable: true,
        },
    );
    assert_eq!(
        retried,
        vec![LiveCommand::Attach {
            session: sid(3),
            after_seq: 0
        }],
        "a retryable attach failure retries rather than wedging"
    );

    // A PERMANENT one reports, leaves the row cold, and DESELECTS the
    // surface (W3c3.2): the daemon said retrying is futile, and a
    // still-selected row would make the loop tail's `sync_selection`
    // re-attach on every pass — an infinite attach/fail ping-pong. The
    // latch is still released, so a LATER selection — the user's own —
    // is not poisoned by this one.
    driver.apply(
        &mut model,
        LiveReply::AttachFailed {
            session: sid(3),
            code: haider_rpc::ERROR_CODE_NOT_FOUND.to_owned(),
            message: "no such session".to_owned(),
            retryable: false,
        },
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("not_found")),
        "the failure is reported, not swallowed"
    );
    assert!(
        model.active_session.is_none(),
        "the permanently refused surface is deselected, not ping-ponged"
    );
    assert!(
        driver.sync_selection(&model).is_empty(),
        "…so the loop tail re-attaches nothing"
    );
    model.open_session(&sid(3));
    assert_eq!(
        driver.sync_selection(&model).len(),
        1,
        "a fresh selection attaches cleanly — the latch was released"
    );
}

// ---- menu coordinates -------------------------------------------------

#[test]
fn a_live_menu_answer_carries_the_exact_opening_sequence_and_generation() {
    // Report §6.3: "menu answer includes exact opening sequence/generation
    // and same-command retry". The daemon's compare-and-set fences a stale
    // answer with `request_seq` + `worker_generation`; the demo's
    // epoch-only `OutboundAnswer` is demo-only.
    //
    // MUTATION CHECK: record `request_seq: 0` in `LiveDriver::record_menu`
    // and the coordinate assertion below fails.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, 1);
    model.open_session(&sid(0));

    let mut opening = envelope(
        &sid(0),
        1,
        &EventPayload::MenuOpened(card("live-card", MenuScope::Session)),
    );
    opening.worker_generation = 42;
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(opening),
        },
    );

    model.outbox.push(OutboundAnswer {
        origin: model.ui_generation(),
        branch: None,
        answer: MenuAnswer {
            menu: MenuId::new("live-card"),
            option_key: Some("yes".to_owned()),
            option_index: 0,
            value: None,
            via: AnswerVia::Tui,
        },
    });
    let commands = driver.drain_answers(&mut model);
    let LiveCommand::Answer {
        command_id,
        session,
        menu,
        request_seq,
        worker_generation,
        option_key,
        ..
    } = commands.first().cloned().expect("one answer")
    else {
        panic!("the drain produced something other than an answer");
    };
    assert_eq!(session, sid(0));
    assert_eq!(menu, MenuId::new("live-card"));
    assert_eq!(request_seq, 1, "the COMMITTED opening sequence");
    assert_eq!(worker_generation, 42, "…and its worker generation");
    assert_eq!(option_key, "yes");
    assert!(model.outbox.is_empty(), "the outbox drained");

    // SAME-COMMAND RETRY: a resend after a lost response must reuse the id,
    // or the daemon sees a second command and answers `already_resolved`
    // for a card the user answered once.
    //
    // MUTATION CHECK: mint a fresh id per drain in
    // `LiveDriver::answer_command` (drop the `coordinates.command_id`
    // reuse) and the equality below fails.
    model.outbox.push(OutboundAnswer {
        origin: model.ui_generation(),
        branch: None,
        answer: MenuAnswer {
            menu: MenuId::new("live-card"),
            option_key: Some("yes".to_owned()),
            option_index: 0,
            value: None,
            via: AnswerVia::Tui,
        },
    });
    let retry = driver.drain_answers(&mut model);
    assert_eq!(
        retry.first().and_then(LiveCommand::command_id),
        Some(&command_id),
        "the retry reuses the menu's durable command id"
    );

    // The COMMITTED resolution is what retires it — not a correlated echo.
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(envelope(
                &sid(0),
                2,
                &EventPayload::MenuClosed {
                    menu: MenuId::new("live-card"),
                    reason: MenuCloseReason::Cancelled,
                },
            )),
        },
    );
    assert!(
        driver.menu_coordinates(&MenuId::new("live-card")).is_none(),
        "a resolved menu's coordinates are released"
    );
    assert_eq!(driver.outbox_len(), 0, "…and its answer left the outbox");
}

#[test]
fn an_answer_for_a_menu_this_connection_never_saw_open_is_not_invented() {
    // Coordinates come from the COMMITTED opening envelope or not at all.
    // A card restored from a previous process, or one whose opening we
    // missed, has no `request_seq`/`worker_generation` to fence with —
    // guessing them would send an answer the CAS cannot arbitrate.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    attach_all(&mut driver, &mut model, 1);
    model.outbox.push(OutboundAnswer {
        origin: model.ui_generation(),
        branch: None,
        answer: MenuAnswer {
            menu: MenuId::new("unseen"),
            option_key: Some("yes".to_owned()),
            option_index: 0,
            value: None,
            via: AnswerVia::Tui,
        },
    });
    assert!(
        driver.drain_answers(&mut model).is_empty(),
        "no coordinates, no answer"
    );
}

// ---- cold sessions ----------------------------------------------------

#[test]
fn a_cold_session_is_listable_with_its_head_and_readable_by_selection() {
    // R11 cut 4: "cold sessions represented by list/read metadata". The
    // READ is the attach's own replay — selecting a cold session attaches
    // it from cursor 0 and the daemon replays its full history through the
    // SAME router a hot session uses, so no second, divergent projector
    // exists.
    //
    // (This test used to hand-feed a `LiveReply::ColdRead` that nothing in
    // production could emit — it proved the reducer path, not the feature.
    // The unreachable `session.read` plumbing went with it; what is left is
    // the path a user actually takes — review P1-6.)
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary(5, 2)],
            next_cursor: None,
        },
    );
    assert!(driver.is_cold(&sid(5)));
    assert!(!driver.is_attached(&sid(5)));
    assert_eq!(model.sessions.len(), 1, "a cold session is listable");
    assert_eq!(
        driver.cold_head_seq(&sid(5)),
        Some(2),
        "…with the committed head the daemon reported"
    );
    assert_eq!(rows(&model, &sid(5)), 0, "and no invented transcript");

    // Selecting it attaches from zero; the replay lands through the router.
    model.open_session(&sid(5));
    assert_eq!(
        driver.sync_selection(&model),
        vec![LiveCommand::Attach {
            session: sid(5),
            after_seq: 0
        }],
        "selection is what makes a cold session readable"
    );
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(5),
            attachment: attachment(5),
            worker_generation: 7,
            replay_through_seq: 2,
        },
    );
    for seq in 1..=2 {
        driver.apply(
            &mut model,
            LiveReply::Event {
                attachment: attachment(5),
                session: sid(5),
                envelope: Box::new(envelope(&sid(5), seq, &user("replayed"))),
            },
        );
    }
    assert_eq!(rows(&model, &sid(5)), 2, "…and the replay is its history");
    assert!(!driver.is_cold(&sid(5)), "it is hot now");
}

#[test]
fn listing_the_same_sessions_again_neither_duplicates_rows_nor_regenerates_them() {
    // Every reconnect re-lists. A list that minted a fresh row (or a fresh
    // generation) would duplicate the launcher and strand every draft,
    // arm and meter keyed by the old generation.
    //
    // MUTATION CHECK: drop the early return in
    // `AppModel::upsert_live_session` and the row count doubles.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let listed = LiveReply::Listed {
        sessions: vec![summary(1, 0), summary(2, 0)],
        next_cursor: None,
    };
    driver.apply(&mut model, listed.clone());
    let generations: Vec<_> = model.sessions.iter().map(|row| row.ui_gen).collect();
    driver.apply(&mut model, listed);
    assert_eq!(model.sessions.len(), 2, "no duplicate rows");
    assert_eq!(
        model
            .sessions
            .iter()
            .map(|row| row.ui_gen)
            .collect::<Vec<_>>(),
        generations,
        "…and no regenerated identities"
    );
}

// ---- helpers ----------------------------------------------------------

fn rows(model: &AppModel, session: &SessionId) -> usize {
    if model.active_session.as_ref() == Some(session) {
        return model.projection.entries().len();
    }
    model
        .sessions
        .iter()
        .find(|row| &row.id == session)
        .expect("session row")
        .projection
        .entries()
        .len()
}

fn card(id: &str, scope: MenuScope) -> Menu {
    Menu {
        id: MenuId::new(id),
        kind: MenuKind::Choice,
        scope,
        title: "pick".to_owned(),
        body: vec![],
        options: vec![MenuOption {
            key: "yes".to_owned(),
            label: "yes".to_owned(),
            detail: None,
            decision: None,
        }],
        blocking: true,
        origin: "live".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

#[allow(dead_code)]
fn unused(_: AgentId) {}
