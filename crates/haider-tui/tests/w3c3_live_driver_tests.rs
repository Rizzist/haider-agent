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
use haider_protocol::ids::{AgentId, DeviceId, EventId, MenuId, SessionId};
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
    let resent: Vec<&CommandId> = resumed.iter().filter_map(LiveCommand::command_id).collect();
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
    let again = driver.apply(&mut model, LiveReply::Reconnected);
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
fn a_gap_on_a_live_attachment_detaches_and_reattaches_from_the_cursor() {
    // The driver's half of the strict gap law: the reducer stops, and the
    // driver re-establishes the subscription AFTER the last fully applied
    // sequence so the daemon replays the hole.
    //
    // MUTATION CHECK: return `Vec::new()` for `RawOutcome::Gap` in
    // `LiveDriver::on_event` and no reattach is issued — the session
    // silently stops updating.
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
    let commands = driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 5, &user("five"))),
        },
    );
    assert_eq!(
        commands,
        vec![
            LiveCommand::Detach {
                attachment: attachment(0)
            },
            LiveCommand::Attach {
                session: sid(0),
                after_seq: 1
            },
        ]
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

    // The turn is submitted EXACTLY once: a second attach (a reconnect's
    // reattach, say) must not resubmit it.
    let again = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(1),
            attachment: attachment(1),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    assert!(
        !again
            .iter()
            .any(|command| matches!(command, LiveCommand::Submit { .. })),
        "a later attach never replays the first turn"
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
    // MUTATION CHECK: delete the `driver.sync_selection(&model)` call in
    // `runtime::run_live` — headlessly, make `sync_selection` return
    // `Vec::new()` — and the attach below never happens.
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
fn a_cold_session_reads_through_the_same_reducer_as_a_live_one() {
    // R11 cut 4: cold sessions are represented by list/read metadata. Their
    // transcript must be built by the SAME router as an attachment's — a
    // second projector is a second set of bugs.
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
    assert_eq!(model.sessions.len(), 1, "a cold session is still listable");

    for seq in 1..=2 {
        driver.apply(
            &mut model,
            LiveReply::ColdRead {
                session: sid(5),
                envelope: Box::new(envelope(&sid(5), seq, &user("cold"))),
            },
        );
    }
    assert_eq!(rows(&model, &sid(5)), 2, "…and readable");
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
