//! W3c3.1 — the dual review's fix round (`docs/briefs/W3c3-review-1-NO_SHIP.md`).
//!
//! Two LAWS were violated and one whole defect class was invisible. What
//! makes this file different from `w3c3_live_driver_tests.rs` is where it
//! stands: these tests drive [`haider_tui::runtime::live_pass`] — the
//! function `run_live` itself calls — instead of re-typing the loop tail.
//! The re-typed copy is exactly how a double attach on every gap,
//! `Lagged` and reconnect stayed green through an adversarial review
//! (P1-3 == D1-1): the copy omitted `sync_selection`, which was the second
//! attach authority.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_rpc::{AttachmentId, CommandId, SessionSummary};
use haider_tui::app::{AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::identity::UiGeneration;
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::{ShellRequest, live_pass};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model};

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_SEEN_V1.to_owned());
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
        run_state: None,
        run_id: None,
        seen_at_ms: None,
        last_activity_ms: Some(u64::try_from(n).expect("index fits")),
        waiting_why: None,
        needs_input: None,
        metadata: None,
        provider: None,
        workspace_cwd: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: None,
        agent_metrics: None,
        last_model: None,
        cache_lifetime_hit_basis_points: None,
        cache_reread_hit_basis_points: None,
        parent_session_id: None,
        kind: None,
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
        forked_from: None,
    }
}

fn envelope(session: &SessionId, seq: u64) -> RawEnvelope {
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
        payload: serde_json::to_value(EventPayload::UserMessage {
            text: format!("row {seq}"),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        })
        .expect("payload serializes"),
    }
}

/// One loop pass at a fixed instant — what `run_live` does every iteration.
fn pass(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    reply: Option<LiveReply>,
) -> Vec<LiveCommand> {
    live_pass(driver, model, reply, std::time::Instant::now()).commands
}

fn attaches(commands: &[LiveCommand]) -> usize {
    commands
        .iter()
        .filter(|command| matches!(command, LiveCommand::Attach { .. }))
        .count()
}

fn detaches(commands: &[LiveCommand]) -> usize {
    commands
        .iter()
        .filter(|command| matches!(command, LiveCommand::Detach { .. }))
        .count()
}

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

/// List one session, select it, and let the loop attach it.
fn open_live(driver: &mut LiveDriver, model: &mut AppModel, n: usize, head_seq: u64) {
    pass(
        driver,
        model,
        Some(LiveReply::Listed {
            sessions: vec![summary(n, head_seq)],
            next_cursor: None,
        }),
    );
    model.open_session(&sid(n));
    let issued = pass(driver, model, None);
    assert_eq!(attaches(&issued), 1, "selection attaches exactly once");
}

// ---- GROUP A1: the strict gap covers the FIRST envelope ----------------

#[test]
fn a_fresh_attachs_first_envelope_is_gap_checked_against_the_cursor_it_asked_from() {
    // LAW: STRICT GAP. A gap STOPS reduction and asks for a reattach BEFORE
    // any later state mutates. Continuity used to be checked only once
    // `last_seq` was set (`SessionProjection::admit`), so the FIRST envelope
    // after a fresh attach was accepted whatever its sequence: a client that
    // attached `after_seq = 0` and was answered with seq 2 painted seq 2 as
    // its first row and lost seq 1 forever — a hole with no later sequence
    // able to expose it. `set_last_applied` existed and had no production
    // caller at all (review P1-1).
    //
    // MUTATION CHECK: delete the `model.seed_cursor(&session, after_seq)`
    // call from the `LiveReply::Attached` arm of `LiveDriver::apply` and the
    // row below appears while the reattach does not.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    open_live(&mut driver, &mut model, 0, 0);
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Attached {
            session: sid(0),
            attachment: attachment(0),
            worker_generation: 7,
            replay_through_seq: 0,
        }),
    );

    // The replay opens at seq 2. Sequence 1 is MISSING.
    let issued = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 2)),
        }),
    );
    assert_eq!(
        rows(&model, &sid(0)),
        0,
        "a gapped first envelope paints NOTHING"
    );
    assert_eq!(
        model.projection.last_applied(),
        Some(0),
        "…and the cursor stays where the attach asked from"
    );
    assert_eq!(
        issued,
        vec![
            LiveCommand::Detach {
                attachment: attachment(0)
            },
            LiveCommand::Attach {
                session: sid(0),
                after_seq: 0
            },
        ],
        "…and the pass reattaches after the last FULLY APPLIED sequence"
    );
}

#[test]
fn a_contiguous_first_envelope_after_a_seeded_cursor_still_applies() {
    // The other half of the same law: seeding must not make a CORRECT
    // replay look gapped. An attach after seq 4 answered with seq 5 is
    // contiguous and applies.
    //
    // MUTATION CHECK: seed with `after_seq + 1` instead of `after_seq` in
    // `AppModel::seed_cursor`'s caller and this row disappears.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    open_live(&mut driver, &mut model, 0, 0);
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Attached {
            session: sid(0),
            attachment: attachment(0),
            worker_generation: 7,
            replay_through_seq: 0,
        }),
    );
    for seq in 1..=4 {
        pass(
            &mut driver,
            &mut model,
            Some(LiveReply::Event {
                attachment: attachment(0),
                session: sid(0),
                envelope: Box::new(envelope(&sid(0), seq)),
            }),
        );
    }
    assert_eq!(rows(&model, &sid(0)), 4);

    // A reattach from 4, answered with 5.
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Lagged {
            attachment: attachment(0),
        }),
    );
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Attached {
            session: sid(0),
            attachment: attachment(1),
            worker_generation: 7,
            replay_through_seq: 5,
        }),
    );
    let issued = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Event {
            attachment: attachment(1),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 5)),
        }),
    );
    assert_eq!(rows(&model, &sid(0)), 5, "the contiguous envelope applies");
    assert!(issued.is_empty(), "…and asks for nothing");
}

// ---- GROUP A2: identity is never reworn --------------------------------

#[test]
fn reset_reseeds_the_demo_world_with_generations_that_have_never_been_worn() {
    // LAW: IDENTITY MONOTONIC / NEVER REUSED. `next_ui_generation` is
    // monotonic for the process lifetime precisely so a generation-keyed
    // callback — the auto-title micro-call, the answer outbox's `origin`, a
    // `DraftKey::Session` — can never find a REPLACEMENT wearing a dead
    // generation. `/reset` defeated that at the only site that matters: the
    // replacement seeds were minted with a hardcoded 1-3, so every reset
    // reissued the three generations it had just retired (review P1-5).
    //
    // MUTATION CHECK: pass `UiGeneration::FIRST.get()` instead of
    // `self.next_ui_generation` to `seed_session_states` in the `"reset"`
    // arm of `execute_slash` and the disjointness below fails.
    let mut model = launcher_model();
    let before: Vec<UiGeneration> = model.sessions.iter().map(|row| row.ui_gen).collect();
    let before_ids: Vec<SessionId> = model.sessions.iter().map(|row| row.id.clone()).collect();
    assert_eq!(before.len(), 3, "the demo world seeds three");

    common::submit(&mut model, "/reset");

    let after: Vec<UiGeneration> = model.sessions.iter().map(|row| row.ui_gen).collect();
    let after_ids: Vec<SessionId> = model.sessions.iter().map(|row| row.id.clone()).collect();
    assert_eq!(after.len(), 3);
    assert!(
        after.iter().all(|generation| !before.contains(generation)),
        "no replacement wears a retired generation: {before:?} then {after:?}"
    );
    assert!(
        after.iter().all(|generation| {
            before
                .iter()
                .all(|retired| generation.get() > retired.get())
        }),
        "…and the allocator only ever moves forward"
    );
    assert_eq!(
        after_ids, before_ids,
        "the SESSION IDS are stable: the reseeded world is the same world, \
         and the demo store's v1 upcaster maps a legacy `id: 2` onto exactly \
         `demo-session-2` (report R11 cut 1 — an id is not a generation)"
    );

    // And again: a second reset must not collide with the first's.
    common::submit(&mut model, "/reset");
    let third: Vec<UiGeneration> = model.sessions.iter().map(|row| row.ui_gen).collect();
    assert!(
        third
            .iter()
            .all(|generation| !after.contains(generation) && !before.contains(generation)),
        "every reseed draws fresh"
    );
}

// ---- GROUP B: ONE attach authority, pinned through the SHIPPING loop ----

#[test]
fn a_gap_produces_exactly_one_attach_through_the_loop_the_binary_runs() {
    // REVIEW P1-3 == D1-1. `sync_selection` owned the in-flight latch while
    // FOUR sites emitted `Attach`; `run_live`'s loop tail then called
    // `sync_selection` unconditionally, saw the session unattached, and
    // attached a SECOND time. The daemon charges a slot per `session.attach`
    // with no dedup and the driver keeps only the last attachment id, so the
    // orphan was never detached yet still accepted events: sixteen gaps
    // wedge the connection at `max_attachments_per_connection`.
    //
    // The latch now lives INSIDE `ensure_attached`, the single emitter.
    //
    // MUTATION CHECK: delete the `if self.attaching.contains_key(session)`
    // early return from `LiveDriver::ensure_attached` and every count below
    // becomes 2.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    open_live(&mut driver, &mut model, 0, 0);
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Attached {
            session: sid(0),
            attachment: attachment(0),
            worker_generation: 7,
            replay_through_seq: 0,
        }),
    );
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 1)),
        }),
    );

    let issued = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Event {
            attachment: attachment(0),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 5)),
        }),
    );
    assert_eq!(attaches(&issued), 1, "ONE attach for one gap: {issued:?}");
    assert_eq!(detaches(&issued), 1, "…and one detach");
    assert!(
        matches!(issued.first(), Some(LiveCommand::Detach { .. })),
        "the detach is FIRST — the daemon must free the slot before the \
         replacement asks for one"
    );
    // The loop keeps running while the attach is in flight: no pass may
    // add another.
    for _ in 0..5 {
        assert!(
            pass(&mut driver, &mut model, None).is_empty(),
            "an idle pass re-issues nothing while the attach is in flight"
        );
    }
}

#[test]
fn a_lagged_attachment_produces_exactly_one_attach_through_the_loop() {
    // Same law, the `Lagged` path (review D1-1 case B: measured 2 attaches,
    // 0 detaches).
    //
    // MUTATION CHECK: as above.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    open_live(&mut driver, &mut model, 0, 0);
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Attached {
            session: sid(0),
            attachment: attachment(0),
            worker_generation: 7,
            replay_through_seq: 0,
        }),
    );

    let issued = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Lagged {
            attachment: attachment(0),
        }),
    );
    assert_eq!(attaches(&issued), 1, "ONE attach for one lag: {issued:?}");
    assert!(
        pass(&mut driver, &mut model, None).is_empty(),
        "and the loop tail adds none"
    );
}

#[test]
fn a_reconnect_produces_exactly_one_attach_per_working_set_member() {
    // Same law, the reconnect path (review D1-1 case C: measured 2 attaches
    // for the active session, 0 detaches). `resume` rebuilds the working set
    // and attaches every member; the loop tail then ran `sync_selection`,
    // which saw the active session unattached — the attach was in flight,
    // not landed — and attached it again.
    //
    // MUTATION CHECK: as above.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    for n in 0..3 {
        pass(
            &mut driver,
            &mut model,
            Some(LiveReply::Listed {
                sessions: vec![summary(n, 0)],
                next_cursor: None,
            }),
        );
        model.open_session(&sid(n));
        pass(&mut driver, &mut model, None);
        pass(
            &mut driver,
            &mut model,
            Some(LiveReply::Attached {
                session: sid(n),
                attachment: attachment(n),
                worker_generation: 7,
                replay_through_seq: 0,
            }),
        );
    }
    assert_eq!(driver.working_set().len(), 3);

    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Disconnected {
            reason: "peer closed".to_owned(),
        }),
    );
    let issued = pass(&mut driver, &mut model, Some(LiveReply::Reconnected));
    assert_eq!(
        attaches(&issued),
        3,
        "one attach per working-set member, never two for the active one: {issued:?}"
    );
    let active: Vec<&LiveCommand> = issued
        .iter()
        .filter(
            |command| matches!(command, LiveCommand::Attach { session, .. } if *session == sid(2)),
        )
        .collect();
    assert_eq!(
        active.len(),
        1,
        "the ATTACHED surface attaches exactly once"
    );
    assert!(
        pass(&mut driver, &mut model, None).is_empty(),
        "and the loop tail adds none"
    );
}

#[test]
fn a_created_session_attaches_exactly_once_through_the_loop() {
    // Same law, the create path (review P1-3: create emitted an attach
    // without setting the latch, then the loop tail attached again).
    //
    // MUTATION CHECK: as above.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    common::submit(&mut model, "ship the thing");
    let issued = pass(&mut driver, &mut model, None);
    let command_id = issued
        .iter()
        .find_map(LiveCommand::command_id)
        .cloned()
        .expect("session.create is durable");

    let after = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Created {
            command_id,
            session: sid(1),
            worker_generation: 7,
            cwd: "~/dev".to_owned(),
            model: "fable-5".to_owned(),
        }),
    );
    assert_eq!(
        attaches(&after),
        1,
        "the create response asks for ONE attachment: {after:?}"
    );
    assert!(
        pass(&mut driver, &mut model, None).is_empty(),
        "and the loop tail adds none while it is in flight"
    );
}

// ---- GROUP C1 (driver half): a drop must become a gap, never silence ----

#[test]
fn a_catch_up_short_of_its_high_water_mark_reattaches_instead_of_trusting_it() {
    // REVIEW P1-2. `AttachCaughtUp` used to be discarded at the link under a
    // comment claiming it "carries no state the cursor law needs". It is the
    // only thing that can expose a replay whose LAST envelopes were lost: a
    // hole in the middle is caught by the next sequence, a hole at the END
    // has no next sequence at all — a dropped terminal `RunState` or
    // `MenuOpened` would simply never arrive and nothing would say so.
    //
    // MUTATION CHECK: make the `LiveReply::CaughtUp` arm of
    // `LiveDriver::apply` return `Vec::new()` unconditionally and the
    // reattach below disappears.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    open_live(&mut driver, &mut model, 0, 0);
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Attached {
            session: sid(0),
            attachment: attachment(0),
            worker_generation: 7,
            replay_through_seq: 3,
        }),
    );
    for seq in 1..=2 {
        pass(
            &mut driver,
            &mut model,
            Some(LiveReply::Event {
                attachment: attachment(0),
                session: sid(0),
                envelope: Box::new(envelope(&sid(0), seq)),
            }),
        );
    }

    // The daemon says it replayed through 3; we only ever applied 2.
    let issued = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::CaughtUp {
            attachment: attachment(0),
            high_water_seq: 3,
        }),
    );
    assert_eq!(
        issued,
        vec![
            LiveCommand::Detach {
                attachment: attachment(0)
            },
            LiveCommand::Attach {
                session: sid(0),
                after_seq: 2
            },
        ],
        "the missing tail is fetched from what we really applied"
    );

    // A catch-up we DID reach asks for nothing.
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Attached {
            session: sid(0),
            attachment: attachment(1),
            worker_generation: 7,
            replay_through_seq: 3,
        }),
    );
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Event {
            attachment: attachment(1),
            session: sid(0),
            envelope: Box::new(envelope(&sid(0), 3)),
        }),
    );
    assert!(
        pass(
            &mut driver,
            &mut model,
            Some(LiveReply::CaughtUp {
                attachment: attachment(1),
                high_water_seq: 3,
            })
        )
        .is_empty(),
        "a satisfied catch-up is a no-op"
    );
}

#[test]
fn dropped_frames_reattach_every_held_attachment_rather_than_vanishing() {
    // REVIEW P1-2. `RpcClient` counts uncorrelated frames it dropped under
    // backpressure in `lost_events()` — which had NO production caller, so a
    // drop was perfect silence. We cannot know which attachment lost what,
    // so every held one reattaches from its own cursor.
    //
    // MUTATION CHECK: make the `LiveReply::EventsLost` arm of
    // `LiveDriver::apply` return `Vec::new()` and the reattaches below
    // disappear.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    for n in 0..2 {
        pass(
            &mut driver,
            &mut model,
            Some(LiveReply::Listed {
                sessions: vec![summary(n, 0)],
                next_cursor: None,
            }),
        );
        model.open_session(&sid(n));
        pass(&mut driver, &mut model, None);
        pass(
            &mut driver,
            &mut model,
            Some(LiveReply::Attached {
                session: sid(n),
                attachment: attachment(n),
                worker_generation: 7,
                replay_through_seq: 0,
            }),
        );
    }

    let issued = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::EventsLost { count: 3 }),
    );
    assert_eq!(attaches(&issued), 2, "both attachments resynchronize");
    assert_eq!(detaches(&issued), 2, "…each releasing its old slot first");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("dropped")),
        "…and the user is told, rather than left with a silently stale screen"
    );
}

// ---- GROUP C2: one durable login command, and a card that cannot wedge --

#[test]
fn a_login_retry_restages_under_the_original_command_id() {
    // REVIEW P1-4 == D2-5. Every `Staged` response minted a NEW `CommandId`
    // while the retryable failure kept the original pending in the outbox
    // and dropped its login correlation, so a retype produced a second
    // durable login with the first still outstanding. `LoginApi`'s own
    // charter always claimed the opposite — that its identity excludes the
    // ephemeral `vault_reference` SO THAT a retry can supply a fresh
    // reference under the same id. Now it does.
    //
    // MUTATION CHECK: replace `self.login_command.clone().unwrap_or_else(..)`
    // with `self.mint()` in the `LiveReply::Staged` arm and both the id
    // equality and the outbox count below fail.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    common::run_slash(&mut model, "/login anthropic api");
    for c in "sk-test-key".chars() {
        model.handle(key(ratatui::crossterm::event::KeyCode::Char(c)));
    }
    model.handle(key(ratatui::crossterm::event::KeyCode::Enter));
    let staged = pass(&mut driver, &mut model, None);
    assert!(matches!(staged.first(), Some(LiveCommand::Stage { .. })));

    // Directed (TUI6.5): the card open mints identity 1 and the SUBMIT
    // re-mints — every submit is a new stage issuance — so the first
    // stage reply factually carries attempt 2.
    let first = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "vault-1".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: 2,
        }),
    );
    let original: CommandId = first
        .iter()
        .find_map(LiveCommand::command_id)
        .cloned()
        .expect("the login is durable");
    assert_eq!(driver.outbox_len(), 1);

    // A RETRYABLE failure: the card asks for the key again.
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(original.clone()),
            code: haider_rpc::ERROR_CODE_OVERLOADED.to_owned(),
            message: "busy".to_owned(),
            retryable: true,
            presentation: None,
        }),
    );
    for c in "sk-test-key".chars() {
        model.handle(key(ratatui::crossterm::event::KeyCode::Char(c)));
    }
    model.handle(key(ratatui::crossterm::event::KeyCode::Enter));
    pass(&mut driver, &mut model, None);
    // Directed (TUI6.5, review r5): the retype is a NEW stage issuance
    // with a FRESH identity (the third mint: open=1, submit=2,
    // retype=3) — the durable-command law under test is orthogonal (the
    // COMMAND id persists across issuances; only the issuance tag
    // moves).
    let retried = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "vault-2".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: 3,
        }),
    );
    assert_eq!(
        retried.iter().find_map(LiveCommand::command_id),
        Some(&original),
        "the retry re-stages under the ORIGINAL durable id"
    );
    assert_eq!(
        driver.outbox_len(),
        1,
        "…and replaces the stale entry rather than queueing a second login \
         the daemon can only ever answer once"
    );
}

#[test]
fn a_login_that_never_answers_stops_saying_validating() {
    // REVIEW D2-5. `LoginStage::Submitting` had no timeout and
    // `accepts_input()` is false there, so a `vault.stage` swallowed by a
    // dead socket parked the card at "validating…" until the user found Esc.
    // Staging is deliberately non-durable — no receipt may hold a secret —
    // so NOTHING retries it: the card must time itself out.
    //
    // MUTATION CHECK: make `LiveDriver::expire_login` return immediately and
    // the card below stays in `Submitting` forever.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    common::run_slash(&mut model, "/login anthropic api");
    for c in "sk-test-key".chars() {
        model.handle(key(ratatui::crossterm::event::KeyCode::Char(c)));
    }
    model.handle(key(ratatui::crossterm::event::KeyCode::Enter));

    let base = std::time::Instant::now();
    live_pass(&mut driver, &mut model, None, base);
    assert_eq!(
        model.login.as_ref().map(|card| card.stage.clone()),
        Some(haider_tui::app::LoginStage::Submitting),
        "the card is closed to input while the stage is in flight"
    );

    live_pass(
        &mut driver,
        &mut model,
        None,
        base + haider_tui::live::LOGIN_STAGE_TIMEOUT + std::time::Duration::from_secs(1),
    );
    let card = model.login.as_ref().expect("the card is still open");
    assert!(
        matches!(card.stage, haider_tui::app::LoginStage::Failed(_)),
        "the deadline hands the card an honest failure, got {:?}",
        card.stage
    );
    assert!(
        card.accepts_input(),
        "…and it takes the key again rather than dead-ending"
    );
}

#[test]
fn a_disconnect_mid_validation_recovers_the_card_and_retires_the_dead_login() {
    // REVIEW D2-5, the other half. The staged reference is single-use AND
    // connection-scoped, and the card wipes its copy of the key on submit,
    // so a fresh socket has neither the reference nor anything to re-stage.
    // Resending the pending `LoginApi` verbatim on every reconnect buys a
    // guaranteed `restage_required` and leaves the entry pending forever.
    //
    // MUTATION CHECK: delete the `self.abandon_login(..)` call from the
    // `LiveReply::Disconnected` arm and the card stays in `Submitting` while
    // the outbox keeps the dead command.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    common::run_slash(&mut model, "/login anthropic api");
    for c in "sk-test-key".chars() {
        model.handle(key(ratatui::crossterm::event::KeyCode::Char(c)));
    }
    model.handle(key(ratatui::crossterm::event::KeyCode::Enter));
    pass(&mut driver, &mut model, None);
    // Directed (TUI6.5): open mints 1, the submit re-mints — the reply
    // factually carries issuance 2.
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "vault-1".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: 2,
        }),
    );
    assert_eq!(driver.outbox_len(), 1);

    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Disconnected {
            reason: "peer closed".to_owned(),
        }),
    );
    let card = model.login.as_ref().expect("the card is still open");
    assert!(
        matches!(card.stage, haider_tui::app::LoginStage::Failed(_)) && card.accepts_input(),
        "the card recovers to entry, got {:?}",
        card.stage
    );
    assert_eq!(
        driver.outbox_len(),
        0,
        "…and the un-committable login is retired, not resent forever"
    );
    let resumed = pass(&mut driver, &mut model, Some(LiveReply::Reconnected));
    assert!(
        !resumed
            .iter()
            .any(|command| matches!(command, LiveCommand::LoginApi { .. })),
        "a reconnect is not a retry"
    );
}

// ---- GROUP C3: live rows are reachable ---------------------------------

fn launcher_hits(model: &AppModel) -> Vec<Hit> {
    let backend = TestBackend::new(118, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = haider_tui::render::render(model, frame))
        .expect("draw");
    hits.into_iter().map(|(_, hit)| hit).collect()
}

#[test]
fn a_live_launcher_row_is_clickable_although_it_has_no_display_name() {
    // REVIEW P1-6. `Hit::AttachSample` carried the row's display NAME.
    // Live rows have none — the daemon's `session.list` reports ids —
    // so render substituted the literal `"session"` into every live hit
    // and the click handler then searched `model.sessions` for a row
    // actually named that. Every live row was a no-op click, and R11's
    // explicit "hit targets carry `SessionId`" requirement was unmet.
    //
    // MUTATION CHECK: build the hit from `entry.name` (falling back to
    // `"session"`) in `render::launcher` and the attach below stops
    // happening.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Listed {
            sessions: vec![summary(7, 3)],
            next_cursor: None,
        }),
    );
    assert!(
        model.sessions[0].name.is_none(),
        "a live row genuinely has no display name"
    );

    let hit = launcher_hits(&model)
        .into_iter()
        .find(|hit| matches!(hit, Hit::AttachSession(_)))
        .expect("the live row is a hit target");
    assert_eq!(
        hit,
        Hit::AttachSession(sid(7)),
        "the hit carries the WIRE coordinate"
    );
    model.handle_hit(hit);
    assert_eq!(
        model.active_session.as_ref(),
        Some(&sid(7)),
        "clicking a live row attaches it"
    );
    assert_eq!(model.screen, Screen::Session);
}

#[test]
fn live_mode_lists_and_reaches_the_cold_sessions_the_driver_tracks() {
    // REVIEW P1-6, the other half. The launcher painted the sim's THREE
    // rows and bound digits 1-3 in BOTH modes, while `/sessions` was a
    // stub — so a profile with five sessions had two that `session.list`
    // knew about, the driver held cold, and nothing could reach.
    //
    // MUTATION CHECK: return `3` unconditionally from
    // `AppModel::launcher_rows` and the fifth row below becomes
    // unreachable by both click and digit.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Listed {
            sessions: (0..5).map(|n| summary(n, 0)).collect(),
            next_cursor: None,
        }),
    );
    assert_eq!(model.sessions.len(), 5, "the DRIVER tracks all five");

    // W5g-6 (owner ask): the launcher PAINTS at most LIVE_LAUNCHER_ROWS
    // recents — /sessions lists the rest — but every painted row stays a
    // hit target and digit-reachable.
    let targets: Vec<Hit> = launcher_hits(&model)
        .into_iter()
        .filter(|hit| matches!(hit, Hit::AttachSession(_)))
        .collect();
    assert_eq!(
        targets.len(),
        haider_tui::app::LIVE_LAUNCHER_ROWS,
        "every PAINTED row is a hit target"
    );

    // …and the last painted row answers to its digit.
    // `upsert_live_session` inserts newest-first, so row 4 is the second
    // session listed (index 1).
    model.handle(key(ratatui::crossterm::event::KeyCode::Char('4')));
    assert_eq!(
        model.active_session.as_ref(),
        Some(&sid(1)),
        "the last painted row answers to its digit"
    );
}

#[test]
fn sessions_lists_every_session_including_the_ones_past_the_launcher() {
    // The launcher header has always promised "/sessions for all". With
    // more sessions than rows it is the only surface that shows the rest,
    // so it is no longer a stub (review P1-6).
    //
    // MUTATION CHECK: restore `"sessions"` to the stub list in
    // `execute_slash`'s catch-all and the listing below is a flash instead.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Listed {
            sessions: (0..12).map(|n| summary(n, 0)).collect(),
            next_cursor: None,
        }),
    );
    common::run_slash(&mut model, "/sessions");
    assert_eq!(model.screen, Screen::Sessions);
    let rows = model.session_browser_rows();
    assert_eq!(
        rows.len(),
        12,
        "every session is listed, not just the painted rows"
    );
    for n in 0..12 {
        assert!(
            rows.iter().any(|row| row.id == sid(n)),
            "session {n} appears by its wire id"
        );
    }
}

// ---- GROUP C4: the reducer may not open a card it cannot close ----------

#[test]
fn live_mode_opens_no_local_card_it_has_no_way_to_close() {
    // REVIEW D1-2. `/voice` and `/tools` minted a LOCAL `MenuOpened` with no
    // `RuntimeMode` gate. Answering it produced NO live command —
    // `LiveDriver::answer_command` has no committed coordinates for a card
    // this connection never saw open, so the answer was silently dropped —
    // the card never closed, and it then blocked every later menu including
    // the daemon's own `request_input`. Its footer even promised an RPC live
    // mode never sends. THE GENERAL RULE: the reducer may not open a card it
    // cannot close.
    //
    // MUTATION CHECK: delete the
    // `"voice" | "say" if !self.mode.fabricates_locally()` arm from
    // `execute_slash` and the card appears, unanswerable. (`/tools` left
    // this loop in W8b: it now performs a REAL daemon inventory read and
    // opens a read-only SCREEN — still no card; see the W8b test below.)
    for command in ["/voice", "/say hello"] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        pass(
            &mut driver,
            &mut model,
            Some(LiveReply::Listed {
                sessions: vec![summary(0, 0)],
                next_cursor: None,
            }),
        );
        model.open_session(&sid(0));
        model.screen = Screen::Session;
        pass(&mut driver, &mut model, None);

        common::run_slash(&mut model, command);
        assert!(
            model.projection.open_menu().is_none(),
            "{command} opened a card live mode cannot answer"
        );
        assert!(
            model
                .flash
                .as_deref()
                .is_some_and(|flash| flash.contains("demo only")),
            "{command} says so honestly, exactly as /reset does"
        );
        assert!(
            pass(&mut driver, &mut model, None).is_empty(),
            "{command} promised the daemon nothing"
        );
    }
}

/// MUTATION CHECK: route live `/tools` back to `refuse_demo_only`.
/// Expected runtime failure: the issued-command assertion below (no
/// `tools.inventory` read reaches the daemon).
#[test]
fn live_tools_reads_the_daemon_inventory_and_opens_no_card() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Listed {
            sessions: vec![summary(0, 0)],
            next_cursor: None,
        }),
    );
    model.open_session(&sid(0));
    model.screen = Screen::Session;
    pass(&mut driver, &mut model, None);

    common::run_slash(&mut model, "/tools");
    assert!(
        model.projection.open_menu().is_none(),
        "no locally minted card (the D1-2 half of the law still holds)"
    );
    assert_eq!(model.screen, Screen::Tools, "the read-only screen opens");
    let issued = pass(&mut driver, &mut model, None);
    assert!(
        issued
            .iter()
            .any(|command| matches!(command, LiveCommand::ToolsInventory { .. })),
        "W8b: /tools promises exactly the daemon inventory read, got {issued:?}"
    );
}

/// MUTATION CHECK: delete the leading-`!` arm from the composer submit
/// path. Expected runtime failure: the text lands as a model turn (a
/// Submit command) instead of a `shell.exec` — the matches below fail.
#[test]
fn live_bang_routes_one_exact_command_to_shell_exec_and_never_a_turn() {
    let mut model = live_model();
    model.daemon_features = haider_client::required_user_command_features();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Listed {
            sessions: vec![summary(0, 0)],
            next_cursor: None,
        }),
    );
    model.open_session(&sid(0));
    model.screen = Screen::Session;
    pass(&mut driver, &mut model, None);

    // Exactly one leading ! is stripped; the rest travels verbatim.
    for c in "!!echo hi".chars() {
        model.handle(common::key(KeyCode::Char(c)));
    }
    model.handle(common::key(KeyCode::Enter));
    let issued = pass(&mut driver, &mut model, None);
    assert!(
        issued.iter().any(|command| matches!(
            command,
            LiveCommand::ShellExec {
                command,
                branch: None,
                ..
            } if command == "!echo hi"
        )),
        "one ! stripped and main stays unbranched: {issued:?}"
    );
    assert!(
        !issued
            .iter()
            .any(|command| matches!(command, LiveCommand::Submit { .. })),
        "a ! escape is never a provider turn"
    );

    // A bare `!` is a harmless validation flash — nothing is promised.
    for c in "!".chars() {
        model.handle(common::key(KeyCode::Char(c)));
    }
    model.handle(common::key(KeyCode::Enter));
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("type a command")),
        "got {:?}",
        model.flash
    );
    assert!(
        pass(&mut driver, &mut model, None).is_empty(),
        "bare ! promises nothing"
    );
}

/// MUTATION CHECK: drop any one of the typed client's three user-command
/// feature requirements from the TUI gate. Expected runtime failure: that
/// deficient daemon receives a mutating shell request below.
#[test]
fn live_bang_requires_the_typed_clients_complete_feature_set() {
    let required = haider_client::required_user_command_features();
    assert_eq!(
        required.len(),
        3,
        "the shared gate stays explicit and complete"
    );

    for missing in &required {
        let mut model = live_model();
        model.daemon_features = required.clone();
        model.daemon_features.remove(missing);
        model.daemon_version = Some("0.0.961".to_owned());
        model.upsert_live_session(&sid(0));
        model.open_session(&sid(0));
        model.screen = Screen::Session;

        common::submit(&mut model, "!echo gated");
        assert!(
            !model
                .requests
                .iter()
                .any(|request| matches!(request, AppRequest::ShellExec { .. })),
            "missing {missing} must send no shell request"
        );
        assert!(
            model
                .flash
                .as_deref()
                .is_some_and(|flash| flash.contains("needs a newer daemon")),
            "missing {missing} gets the typed compatibility refusal: {:?}",
            model.flash
        );
    }
}

#[test]
fn a_live_voice_submit_fabricates_no_row_on_the_session_screen_either() {
    // REVIEW D2-2. `submit_voice`'s live branch returned only for the
    // LAUNCHER; on the session screen it fell through to `push_user_voice`
    // plus a `◉ heard` note, and the authoritative `UserMessage` envelope
    // would then paint the same row again. P1-1's exact shape, left
    // un-swept because `/say` sits behind `/voice`.
    //
    // MUTATION CHECK: restore the `Screen::Launcher`-only early return in
    // `submit_voice` and the row count below grows.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Listed {
            sessions: vec![summary(0, 0)],
            next_cursor: None,
        }),
    );
    model.open_session(&sid(0));
    model.screen = Screen::Session;
    pass(&mut driver, &mut model, None);
    let before = model.projection.entries().len();

    model.voice.enabled = true;
    model.listening = true;
    model.talk_fire();

    assert_eq!(
        model.projection.entries().len(),
        before,
        "no locally fabricated ◉ row: the committed UserMessage paints it"
    );
    let issued = pass(&mut driver, &mut model, None);
    assert!(
        issued
            .iter()
            .any(|command| matches!(command, LiveCommand::Submit { .. })),
        "…and the text still reaches the daemon: {issued:?}"
    );
}

// ---- the pass itself ---------------------------------------------------

#[test]
fn the_loop_pass_hands_back_the_shell_owned_requests_and_translates_the_rest() {
    // `live_pass` exists so the shipping loop is executable by a test. It
    // must therefore be TOTAL: every request either becomes a command or
    // comes back as shell-owned work. A request that silently vanished here
    // would be a feature that works in demo and does nothing live.
    //
    // MUTATION CHECK: drop `AppRequest::Quit` from `live_pass`'s shell arm
    // and the quit below never reaches the runtime.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    model.requests.push(AppRequest::Quit);
    model.requests.push(AppRequest::CopyText("x".to_owned()));
    let produced = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    assert_eq!(
        produced.shell,
        vec![ShellRequest::Quit, ShellRequest::CopyText("x".to_owned())],
        "the terminal/process work comes back for the shell to perform"
    );
    assert!(
        model.requests.is_empty(),
        "…and nothing is left un-drained for a later pass to re-run"
    );
}
