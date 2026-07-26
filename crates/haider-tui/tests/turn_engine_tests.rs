//! TUI3b turn engine: the sim respond() port (script.rs) + the driver's
//! beat player, AwaitMenu arms, token meter, turn-end law and timers —
//! beat-level shape/verbatim tests plus paused-time driver integration
//! through the production wiring (channel, generations, consume).
#![allow(clippy::expect_used)]

use haider_protocol::state::RunState;
use haider_protocol::{DeliveryMode, EventPayload};
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, Screen};
use haider_tui::projection::TranscriptEntry;
use haider_tui::runtime::DemoDriver;
use haider_tui::script::{
    Beat, DemoEvent, GENERIC_INTROS, GENERIC_OUTROS, TALK_PHRASE, compaction_beats, respond_beats,
    roster_at, title_note,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn launcher_model() -> AppModel {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        haider_protocol::state::HarnessStatus::Ready,
    ))));
    model
}

/// Build a turn's beats with fresh sim counters (genRef 0, rosterRef 3).
fn beats_for(text: &str) -> Vec<Beat> {
    let (mut generic, mut roster) = (0, 3);
    respond_beats(
        text,
        false,
        DeliveryMode::Steer,
        1,
        &mut generic,
        &mut roster,
    )
}

/// Every emitted payload, in order (arms excluded).
fn emits(beats: &[Beat]) -> Vec<&EventPayload> {
    beats
        .iter()
        .filter_map(|beat| match beat {
            Beat::Emit(payload) => Some(payload),
            _ => None,
        })
        .collect()
}

/// Completed agent-message texts, in order (the stream() outputs).
fn agent_texts(beats: &[Beat]) -> Vec<String> {
    emits(beats)
        .into_iter()
        .filter_map(|payload| match payload {
            EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                item: haider_protocol::item::TurnItem::AgentMessage { text },
                ..
            }) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Completed tool calls as (name, desc, meta, ok), in order.
fn tool_rows(beats: &[Beat]) -> Vec<(String, String, String, bool)> {
    emits(beats)
        .into_iter()
        .filter_map(|payload| match payload {
            EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                item:
                    haider_protocol::item::TurnItem::ToolCall {
                        name, args, status, ..
                    },
                ..
            }) => Some((
                name.clone(),
                args["desc"].as_str().unwrap_or("").to_owned(),
                args["meta"].as_str().unwrap_or("").to_owned(),
                *status == haider_protocol::item::ToolStatus::Completed,
            )),
            _ => None,
        })
        .collect()
}

fn notes(beats: &[Beat]) -> Vec<&str> {
    beats
        .iter()
        .filter_map(|beat| match beat {
            Beat::Note(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn await_menu(beats: &[Beat]) -> (&haider_protocol::menu::Menu, &Vec<Vec<Beat>>) {
    let menu_id = beats
        .iter()
        .find_map(|beat| match beat {
            Beat::AwaitMenu { menu, .. } => Some(menu.clone()),
            _ => None,
        })
        .expect("branch parks on a menu");
    let menu = emits(beats)
        .into_iter()
        .find_map(|payload| match payload {
            EventPayload::MenuOpened(menu) if menu.id == menu_id => Some(menu),
            _ => None,
        })
        .expect("the parked menu was opened");
    let arms = beats
        .iter()
        .find_map(|beat| match beat {
            Beat::AwaitMenu { arms, .. } => Some(arms),
            _ => None,
        })
        .expect("arms");
    (menu, arms)
}

// ---- §1.0 spine ----

#[test]
fn preamble_is_user_tokens_thinking_750_streaming_and_ends_with_turn_end() {
    let beats = beats_for("hello there");
    // Beat 0: the user row (typed turns; voice turns skip it).
    assert!(matches!(
        &beats[0],
        Beat::Emit(EventPayload::UserMessage { text, mode: DeliveryMode::Steer, .. })
            if text == "hello there"
    ));
    // Beat 1: 9 tokens/char including spaces ("hello there" = 11 chars).
    assert!(matches!(
        beats[1],
        Beat::Tokens {
            n: 99,
            output: false
        }
    ));
    assert!(matches!(
        beats[2],
        Beat::Emit(EventPayload::RunState(RunState::Thinking))
    ));
    assert!(matches!(beats[3], Beat::Sleep(750)));
    assert!(matches!(
        beats[4],
        Beat::Emit(EventPayload::RunState(RunState::Streaming))
    ));
    assert!(
        matches!(beats.last(), Some(Beat::TurnEnd)),
        "every branch ends with the turn-end law"
    );
}

#[test]
fn voice_turns_wrap_in_voice_tags_and_skip_the_user_row() {
    let (mut generic, mut roster) = (0, 3);
    let beats = respond_beats(
        "hello",
        true,
        DeliveryMode::Steer,
        1,
        &mut generic,
        &mut roster,
    );
    assert!(matches!(beats[0], Beat::Voice(true)));
    assert!(
        !emits(&beats)
            .iter()
            .any(|payload| matches!(payload, EventPayload::UserMessage { .. })),
        "the reducer already pushed the ◉ row"
    );
    let voice_off = beats
        .iter()
        .position(|beat| matches!(beat, Beat::Voice(false)))
        .expect("voice tag closes");
    assert!(
        matches!(beats[voice_off + 1], Beat::TurnEnd),
        "Voice(false) lands right before TurnEnd"
    );
}

#[test]
fn stream_paces_word_tokens_at_22ms_with_9_tokens_per_char() {
    // "On it. Scanning the workspace for the modules this touches." —
    // split(/(\s+)/) keeps words AND whitespace runs: 10 words + 9 gaps.
    let beats = beats_for("zzz");
    let delta_count = emits(&beats)
        .iter()
        .filter(|payload| {
            matches!(
                payload,
                EventPayload::Item(haider_protocol::item::ItemEvent::Delta { .. })
            )
        })
        .count();
    let intro_tokens = 19;
    let outro_words = GENERIC_OUTROS[0].split_whitespace().count();
    assert_eq!(
        delta_count,
        intro_tokens + outro_words * 2 - 1,
        "one Text delta per word-token (words + whitespace runs)"
    );
    // Each stream token pairs Tokens{output:true} + Sleep(22).
    let output_token_beats = beats
        .iter()
        .filter(|beat| matches!(beat, Beat::Tokens { output: true, .. }))
        .count();
    assert_eq!(output_token_beats, delta_count);
    assert_eq!(
        beats
            .iter()
            .filter(|beat| matches!(beat, Beat::Sleep(22)))
            .count(),
        delta_count
    );
}

// ---- Branch routing (first match wins, JS word boundaries) ----

#[test]
fn routing_follows_the_sim_order_and_word_boundaries() {
    // Order: "use a subagent to cover the webhook tests" contains `test`
    // but hits branch 1.
    assert!(
        agent_texts(&beats_for("use a subagent to cover the webhook tests"))[0]
            .starts_with("Spinning up a subagent"),
        "subagent outranks test"
    );
    // \bprod\b: "producing" must NOT hit the prod branch.
    assert_eq!(
        agent_texts(&beats_for("producing artifacts"))[0],
        GENERIC_INTROS[0]
    );
    assert!(
        agent_texts(&beats_for("ship it to prod"))[0].starts_with("This touches production"),
        "\\bprod\\b matches the bare word"
    );
    // ci\b: "circus" must not match; bare "ci" must.
    assert!(agent_texts(&beats_for("the circus is in town"))[0].starts_with("On it."));
    assert!(
        agent_texts(&beats_for("run ci again"))[0].starts_with("Running the suite first"),
        "ci\\b matches"
    );
    // rate.?limit: joined and separated forms both match.
    assert!(
        agent_texts(&beats_for("we hit a ratelimit"))[0].starts_with("Kicking the heavy sweep"),
    );
    assert!(
        agent_texts(&beats_for("hitting rate limits again"))[0]
            .starts_with("Kicking the heavy sweep"),
    );
}

#[test]
fn generic_rotation_post_increments_and_test_branch_reads_without_increment() {
    let (mut generic, mut roster) = (0, 3);
    let mut turn = |text: &str| {
        let beats = respond_beats(
            text,
            false,
            DeliveryMode::Steer,
            1,
            &mut generic,
            &mut roster,
        );
        agent_texts(&beats)
    };
    let first = turn("hello");
    assert_eq!(first[0], GENERIC_INTROS[0]);
    assert_eq!(first.last().expect("outro"), GENERIC_OUTROS[0]);
    let second = turn("hello again");
    assert_eq!(second[0], GENERIC_INTROS[1]);
    assert_eq!(second.last().expect("outro"), GENERIC_OUTROS[1]);
    // The test branch reads genRef % 3 WITHOUT incrementing (tui.js:1411).
    let suite = turn("the tests are flaky");
    assert_eq!(suite.last().expect("outro"), &GENERIC_OUTROS[2]);
    let after = turn("hello third time");
    assert_eq!(
        after[0], GENERIC_INTROS[2],
        "the test branch did not advance the counter"
    );
}

// ---- §1.2 crash ----

#[test]
fn crash_branch_is_verbatim_with_three_arms() {
    let beats = beats_for("the migration is unstable");
    assert_eq!(
        agent_texts(&beats)[0],
        "Reproducing the failure path with the real job — if it dies mid-write, we reconcile from the effect journal instead of guessing."
    );
    let tools = tool_rows(&beats);
    assert_eq!(tools[0].0, "process_exec");
    assert_eq!(tools[0].2, "exit 137 · connection lost mid-write");
    assert!(!tools[0].3, "err status");
    assert!(emits(&beats).iter().any(|payload| matches!(
        payload,
        EventPayload::RunState(RunState::EffectOutcomeUnknown)
    )),);
    assert_eq!(
        notes(&beats)[0],
        "· effect_outcome_unknown — the write may or may not have committed"
    );
    let (menu, arms) = await_menu(&beats);
    assert_eq!(menu.title, "recovery — process_exec outcome unknown");
    assert_eq!(
        menu.body,
        vec![
            "cargo run --bin migrate -- --batch 7 died mid-write (exit 137)",
            "effect class: externally transactional · idempotency key present",
        ]
    );
    let labels: Vec<&str> = menu.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "probe & reconcile from the journal (recommended)",
            "retry from checkpoint ◇7",
            "mark run errored — stop here",
        ]
    );
    assert_eq!(arms.len(), 3);
    // Probe arm reconciles from the journal first; retry goes straight to
    // the shared tail.
    assert_eq!(tool_rows(&arms[0])[0].0, "fs_read");
    assert_eq!(tool_rows(&arms[0])[0].2, "not committed ✓ safe to retry");
    assert_eq!(tool_rows(&arms[1])[0].0, "process_exec");
    assert_eq!(
        agent_texts(&arms[0])[0],
        "Reconciled and retried — the journal proved the first attempt never committed, so no double-write was possible."
    );
    // The errored arm: note → ERRORED → 1800 ms hold → Done; it RETURNS —
    // no TurnEnd, no queue consume, no compaction check.
    let errored = &arms[2];
    assert_eq!(
        notes(errored)[0],
        "· run → errored · terminal state is honest — nothing was retried"
    );
    assert!(
        errored.iter().any(|beat| matches!(beat, Beat::Sleep(1800))),
        "ERRORED holds 1800 ms"
    );
    assert!(matches!(
        errored.last(),
        Some(Beat::Emit(EventPayload::RunState(RunState::Done)))
    ));
    assert!(
        !errored.iter().any(|beat| matches!(beat, Beat::TurnEnd)),
        "the errored arm returns without the turn-end law"
    );
}

// ---- §1.3 prod ----

#[test]
fn prod_branch_permission_card_and_arms_are_verbatim() {
    let beats = beats_for("deploy the migration to prod");
    assert_eq!(
        agent_texts(&beats)[0],
        "This touches production — the tool call needs your approval before anything runs."
    );
    assert!(emits(&beats).iter().any(|payload| matches!(
        payload,
        EventPayload::RunState(RunState::PermissionRequired { .. })
    )));
    let (menu, arms) = await_menu(&beats);
    assert_eq!(menu.title, "process_exec requests approval");
    assert_eq!(
        menu.body[2],
        "an \"always\" answer creates rule: process_exec(cargo run --bin migrate:*)"
    );
    let labels: Vec<&str> = menu.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "allow once",
            "allow for this session — adds the rule above",
            "deny — tell the agent why",
        ]
    );
    // c=1 adds the session rule note FIRST; c=0 does not.
    assert_eq!(
        notes(&arms[1])[0],
        "· session rule added: process_exec(cargo run --bin migrate:*)"
    );
    assert!(notes(&arms[0]).is_empty());
    assert_eq!(
        agent_texts(&arms[0])[0],
        "Migration applied cleanly — journaled under its idempotency key."
    );
    // Deny: note + stream, then the NORMAL turn end.
    assert_eq!(
        notes(&arms[2])[0],
        "· denied with reason — the model is told, not just blocked"
    );
    assert_eq!(
        agent_texts(&arms[2])[0],
        "Understood — leaving production untouched. I'll stage the migration as a reviewed patch instead."
    );
    assert!(matches!(arms[2].last(), Some(Beat::TurnEnd)));
}

// ---- §1.5 custom tools ----

#[test]
fn custom_tool_branch_parks_waiting_with_the_verbatim_reason() {
    let beats = beats_for("show me the dispatch modes");
    let branch_notes = notes(&beats);
    assert_eq!(
        branch_notes,
        vec![
            "· dispatch = fire-and-forget — the turn continued the instant it was sent",
            "· dispatch = await — blocked in TOOL_RUNNING until the env id came back",
            "· dispatch = deferred — parking in WAITING(dependency) on ct-91 · still messageable",
            "· callback resolved ct-91 — the deferred tool woke the turn back up",
        ]
    );
    assert!(emits(&beats).iter().any(|payload| matches!(
        payload,
        EventPayload::RunState(RunState::Waiting {
            reason: haider_protocol::state::WaitReason::Other { tag }
        }) if tag == "dependency · custom tool ct-91"
    )));
    assert!(beats.iter().any(|beat| matches!(beat, Beat::Sleep(2600))));
    // The deferred callback row lands already-complete with NO +2400
    // token accrual (sim pushes it directly, tui.js:1398): 3 tool() calls
    // → exactly 3 input-bucket 2400 beats despite 4 completed tool rows.
    let tool_token_beats = beats
        .iter()
        .filter(|beat| {
            matches!(
                beat,
                Beat::Tokens {
                    n: 2400,
                    output: false
                }
            )
        })
        .count();
    assert_eq!(tool_token_beats, 3);
    assert_eq!(tool_rows(&beats).len(), 4);
    assert_eq!(tool_rows(&beats)[3].1, "◇ ct-91 → tool_result");
    assert_eq!(
        agent_texts(&beats).last().expect("closing stream"),
        "All three landed: a fire-and-forget notice, an awaited deploy, and a deferred smoke run that called back green. Preview is live at pv-5521."
    );
}

// ---- §1.7 rate limit ----

#[test]
fn rate_limit_branch_rotation_menu_and_arms_are_verbatim() {
    let beats = beats_for("we keep hitting 429 quota");
    assert_eq!(
        notes(&beats),
        vec![
            "· 5h limit on openai/work-chatgpt (Codex oauth) — rotating, oauth preferred",
            "· account → openai/billing-key (api key) · mid-session, like a model change",
            "· weekly cap now hit on BOTH openai accounts — 5h waits won't help, weekly is spent",
        ]
    );
    assert!(beats.iter().any(|beat| matches!(beat, Beat::Sleep(900))));
    let (menu, arms) = await_menu(&beats);
    assert_eq!(menu.title, "openai accounts weekly-capped — rate limited");
    // Inner spacing is verbatim (aligned columns).
    assert_eq!(
        menu.body[0],
        "work-chatgpt (Codex oauth)   weekly: 0 left · natural reset Mon 00:00 · manual reset available"
    );
    assert_eq!(
        menu.body[1],
        "billing-key (api)            weekly: 0 left · natural reset Mon 00:00"
    );
    // c=1 (wait): park with the verbatim reason, 3000 ms, auto-resume.
    assert!(arms[1].iter().any(|beat| matches!(
        beat,
        Beat::Emit(EventPayload::RunState(RunState::Waiting {
            reason: haider_protocol::state::WaitReason::Other { tag }
        })) if tag == "weekly reset · openai Mon 00:00"
    )));
    assert!(arms[1].iter().any(|beat| matches!(beat, Beat::Sleep(3000))));
    assert_eq!(
        notes(&arms[1])[1],
        "· reset passed — auto-resumed on openai/work-chatgpt (oauth), no human needed"
    );
    // c=0 (burn) and c=1 share the resumed tail.
    assert_eq!(
        agent_texts(&arms[0])[0],
        "Resumed exactly where the limiter cut us off — the sweep is done."
    );
    // c=2 (stop): note + IDLE, no turn-end law.
    assert_eq!(
        notes(&arms[2])[0],
        "· run stopped — accounts stay weekly-capped until Monday"
    );
    assert!(matches!(
        arms[2].last(),
        Some(Beat::Emit(EventPayload::RunState(RunState::Done)))
    ));
}

// ---- §1.8 plan todos ----

#[test]
fn plan_todo_branch_pins_a_dep_chain_and_unpins_all_completed() {
    let beats = beats_for("plan todo the harness work");
    let plans: Vec<&Vec<haider_protocol::history::TodoItem>> = emits(&beats)
        .into_iter()
        .filter_map(|payload| match payload {
            EventPayload::Item(
                haider_protocol::item::ItemEvent::Started {
                    item: haider_protocol::item::TurnItem::Plan { items },
                    ..
                }
                | haider_protocol::item::ItemEvent::Completed {
                    item: haider_protocol::item::TurnItem::Plan { items },
                    ..
                },
            ) => Some(items),
            _ => None,
        })
        .collect();
    let first = plans.first().expect("pin");
    assert_eq!(
        first.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
        vec![
            "scope the harness entrypoints",
            "patch the run loop for typed states",
            "wire WAITING propagation through subagents",
            "run the suite and report",
        ]
    );
    assert_eq!(
        first.iter().map(|t| t.dep).collect::<Vec<_>>(),
        vec![None, Some(0), Some(1), Some(2)],
        "dep chain: each unlocks the next"
    );
    let last = plans.last().expect("unpin");
    assert!(
        last.iter()
            .all(|t| t.state == haider_protocol::history::TodoState::Completed),
        "the final Plan re-emit completes everything (projection unpins)"
    );
    assert_eq!(
        agent_texts(&beats).last().expect("closing stream"),
        "All four todos done — the completed plan just unpinned into the transcript."
    );
}

// ---- §1.9 paste/image tokens ----

#[test]
fn paste_and_image_tokens_pick_the_default_branch_subpaths() {
    let beats = beats_for("please look at [Pasted 12 lines] and fix it");
    assert_eq!(
        agent_texts(&beats)[0],
        "Parsing the pasted block (12 lines) — treating it as reference, not instructions."
    );
    assert_eq!(tool_rows(&beats)[0].1, "ingest [Pasted 12 lines] → CAS");
    assert_eq!(tool_rows(&beats)[0].2, "txt_19c2");

    let beats = beats_for("what is on [Image #3] here");
    assert_eq!(
        agent_texts(&beats)[0],
        "Reading the pasted screenshot first — extracting the UI regions before touching code."
    );
    assert_eq!(tool_rows(&beats)[0].1, "ingest [Image #3] → CAS");
    assert_eq!(tool_rows(&beats)[0].2, "img_7f3a · 214 KB");
}

// ---- §1.1 / §1.4 roster claims ----

#[test]
fn roster_claims_draw_in_order_from_index_three_and_wrap_with_roman() {
    assert_eq!(roster_at(0).cs(), "Muhammad ﷺ");
    assert_eq!(roster_at(3).cs(), "Hasan (a)");
    assert_eq!(roster_at(13).cs(), "Mahdi (aj)");
    assert_eq!(roster_at(15).cs(), "Salman (r)");
    let wrapped = roster_at(38 + 3);
    assert_eq!(wrapped.callsign, "Hasan II");
    assert_eq!(wrapped.full, "Imam Hasan al-Mujtaba II");
    // First live claim is index 3 (seed heads hold 0-2): the subagent
    // branch names Hasan (a) then Husayn (a).
    let beats = beats_for("use two subagents for this");
    assert!(agent_texts(&beats)[0].starts_with(
        "Spinning up two subagents — Hasan (a) on the tests, Husayn (a) on the docs."
    ));
    assert_eq!(tool_rows(&beats)[0].1, "Hasan · tests → local · gpt-5.6");
    assert_eq!(tool_rows(&beats)[1].1, "Husayn · docs → local · gemini-3");
    // §1.4 claims the NEXT name.
    let (mut generic, mut roster) = (0, 3);
    let _ = respond_beats(
        "use a subagent here",
        false,
        DeliveryMode::Steer,
        1,
        &mut generic,
        &mut roster,
    );
    let auth = respond_beats(
        "split the auth work",
        false,
        DeliveryMode::Steer,
        2,
        &mut generic,
        &mut roster,
    );
    assert!(
        agent_texts(&auth)[0]
            .starts_with("Splitting this: Husayn (a) takes the service core on hetzner-1"),
        "claims persist across turns (post-increment)"
    );
}

// ---- §5 compaction beats + title note ----

#[test]
fn compaction_beats_carry_the_numbers_and_reenter_the_turn_end_law() {
    let auto = compaction_beats(170_000, 12_000, false);
    assert!(matches!(
        &auto[0],
        Beat::Note(text)
            if text == "· context at 85% — compacting (dead branches first, live path last)"
    ));
    assert!(matches!(
        auto[1],
        Beat::Emit(EventPayload::RunState(RunState::Compacting))
    ));
    assert!(matches!(auto[2], Beat::Sleep(1400)));
    assert!(auto.iter().any(|beat| matches!(
        beat,
        Beat::Emit(EventPayload::Item(
            haider_protocol::item::ItemEvent::Completed {
                item: haider_protocol::item::TurnItem::ContextCompaction {
                    tokens_before: Some(170_000),
                    tokens_after: Some(12_000),
                    ..
                },
                ..
            }
        ))
    )));
    assert!(
        auto.iter()
            .any(|beat| matches!(beat, Beat::TokensReset(12_000)))
    );
    assert!(
        matches!(auto.last(), Some(Beat::TurnEnd)),
        "auto path re-runs finishTurn (queued input may consume here too)"
    );
    let manual = compaction_beats(30_000, 12_000, true);
    assert!(
        !manual.iter().any(|beat| matches!(beat, Beat::Note(_))),
        "manual /compact has no 85% note"
    );
    assert!(manual.iter().any(|beat| matches!(beat, Beat::Sleep(1200))));
    assert!(matches!(
        manual.last(),
        Some(Beat::Emit(EventPayload::RunState(RunState::Done)))
    ));
}

#[test]
fn title_note_is_the_sim_full_text() {
    assert_eq!(
        title_note("Fix the flaky suite"),
        "· session titled — “Fix the flaky suite” (background micro-call · never enters the prompt)"
    );
}

// ---- Driver integration (paused time, production wiring) ----

fn drain(driver: &mut DemoDriver, model: &mut AppModel) {
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    for request in requests {
        driver.handle_request(model, request);
    }
}

/// The event loop's outbox echo (runtime.rs): answers ride the channel
/// tagged with the CURRENT generation.
fn echo_answers(driver: &DemoDriver, model: &mut AppModel) {
    while !model.outbox.is_empty() {
        let answer = model.outbox.remove(0);
        driver
            .sender()
            .try_send((
                driver.generation(),
                DemoEvent::Envelope(EventPayload::MenuAnswered(answer)),
            ))
            .expect("echo");
    }
}

async fn pump_one(
    driver: &mut DemoDriver,
    rx: &mut tokio::sync::mpsc::Receiver<(u64, DemoEvent)>,
    model: &mut AppModel,
) {
    let (generation, event) = tokio::time::timeout(std::time::Duration::from_secs(3600), rx.recv())
        .await
        .expect("pump: no event arrived on virtual time")
        .expect("channel open");
    driver.consume(model, generation, event);
    drain(driver, model);
    echo_answers(driver, model);
}

async fn pump_until(
    driver: &mut DemoDriver,
    rx: &mut tokio::sync::mpsc::Receiver<(u64, DemoEvent)>,
    model: &mut AppModel,
    what: &str,
    stop: impl Fn(&AppModel) -> bool,
) {
    drain(driver, model);
    echo_answers(driver, model);
    for _ in 0..200_000 {
        if stop(model) {
            return;
        }
        pump_one(driver, rx, model).await;
    }
    panic!("pump_until({what}): condition never satisfied");
}

fn submit(model: &mut AppModel, text: &str) {
    for c in text.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
}

#[tokio::test(start_paused = true)]
async fn generic_turn_plays_end_to_end_with_the_delayed_title_note() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "turn done", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    // Transcript: user row → intro → 4 tools → outro, GENERIC index 0.
    let texts: Vec<String> = model
        .projection
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Item(block) => match &block.item {
                haider_protocol::item::TurnItem::AgentMessage { text } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec![GENERIC_INTROS[0], GENERIC_OUTROS[0]]);
    let tools: Vec<String> = model
        .projection
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Item(block) => match &block.item {
                haider_protocol::item::TurnItem::ToolCall { name, .. } => Some(name.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        tools,
        vec!["fs_search", "fs_read", "fs_patch", "process_exec"]
    );
    // The 1.5 s auto-title note landed with the sim's FULL text.
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· session titled — “Hello world” (background micro-call · never enters the prompt)"
    )));
    // The meter is Usage-authoritative and matches the driver's counters.
    assert!(model.projection.context_tokens() > 0);
    assert_eq!(model.projection.context_tokens(), driver.tokens_total());
}

#[tokio::test(start_paused = true)]
async fn crash_menu_errored_arm_holds_then_idles_without_compaction() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "reproduce the crash please");
    pump_until(&mut driver, &mut rx, &mut model, "menu open", |m| {
        m.projection.open_menu().is_some()
    })
    .await;
    assert_eq!(model.projection.badge(), "⌁ EFFECT_UNKNOWN");
    // Answer option 3: mark run errored — stop here.
    model.handle(key(KeyCode::Char('3')));
    let mut saw_errored = false;
    drain(&mut driver, &mut model);
    echo_answers(&driver, &mut model);
    for _ in 0..10_000 {
        if model.projection.badge() == "✗ ERRORED" {
            saw_errored = true;
        }
        if saw_errored && !model.turn_active && model.projection.badge() == "IDLE" {
            break;
        }
        pump_one(&mut driver, &mut rx, &mut model).await;
    }
    assert!(
        saw_errored,
        "the ERRORED badge held before decaying to idle"
    );
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· run → errored · terminal state is honest — nothing was retried"
    )));
    assert!(
        !model.projection.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Item(block)
                if matches!(block.item, haider_protocol::item::TurnItem::ContextCompaction { .. })
        )),
        "the errored arm returns — no compaction check"
    );
}

#[tokio::test(start_paused = true)]
async fn queued_input_consumes_at_turn_end_without_passing_through_idle() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "turn running", |m| {
        m.turn_active && m.screen == Screen::Session
    })
    .await;
    // Switch to queue mode and queue a second message mid-turn.
    submit(&mut model, "/queue turn");
    assert!(model.queue_mode);
    submit(&mut model, "and then run the suite");
    assert_eq!(model.msg_queue.len(), 1);
    // Pump to the very end, recording whether IDLE ever showed once the
    // first turn was RUNNING and before the queued turn's row landed
    // (the pre-Thinking instant right after the user row is turn start,
    // not turn end — excluded via the turn1_ran latch).
    let mut users_seen = 0;
    let mut turn1_ran = false;
    let mut idle_between = false;
    drain(&mut driver, &mut model);
    for _ in 0..200_000 {
        users_seen = model
            .projection
            .entries()
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::User { .. }))
            .count();
        if users_seen == 1 {
            if model.projection.badge() == "IDLE" {
                if turn1_ran {
                    idle_between = true;
                }
            } else {
                turn1_ran = true;
            }
        }
        if users_seen == 2 && !model.turn_active && model.projection.badge() == "IDLE" {
            break;
        }
        pump_one(&mut driver, &mut rx, &mut model).await;
    }
    assert_eq!(users_seen, 2, "the queued text became its own turn");
    assert!(!idle_between, "the session never passed through idle");
    assert!(model.msg_queue.is_empty(), "queue drained");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· turn ended with queued input — consuming it directly, no idle"
    )));
    // The queued turn's user row carries DeliveryMode::Queue — its
    // envelope is protocol-honest (asserted via the queue-mode note
    // preceding the second user row in entry order).
    let note_index = model
        .projection
        .entries()
        .iter()
        .position(|entry| {
            matches!(
                entry,
                TranscriptEntry::Note { text }
                    if text == "· turn ended with queued input — consuming it directly, no idle"
            )
        })
        .expect("consume note");
    let second_user = model
        .projection
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, entry)| matches!(entry, TranscriptEntry::User { .. }))
        .nth(1)
        .expect("second user row")
        .0;
    assert!(note_index < second_user, "note lands before the queued row");
}

#[tokio::test(start_paused = true)]
async fn auto_compaction_fires_at_85_percent_and_drops_the_meter_to_6_percent() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    // A demo-sized window makes one generic turn cross 85%.
    model.identity.context_window = 2_000;
    submit(&mut model, "hello there friend");
    let mut saw_compacting = false;
    drain(&mut driver, &mut model);
    for _ in 0..200_000 {
        if model.projection.badge() == "⊟ COMPACTING" {
            saw_compacting = true;
        }
        if saw_compacting && !model.turn_active && model.projection.badge() == "IDLE" {
            break;
        }
        pump_one(&mut driver, &mut rx, &mut model).await;
    }
    assert!(saw_compacting, "the compacting badge showed");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· context at 85% — compacting (dead branches first, live path last)"
    )));
    let (before, after) = model
        .projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Item(block) => match block.item {
                haider_protocol::item::TurnItem::ContextCompaction {
                    tokens_before,
                    tokens_after,
                    ..
                } => Some((tokens_before, tokens_after)),
                _ => None,
            },
            _ => None,
        })
        .expect("compaction row landed");
    assert!(before.expect("before") * 100 >= 2_000 * 85, "trigger math");
    assert_eq!(after, Some(120), "6% of the 2k window");
    assert_eq!(model.projection.context_tokens(), 120, "the meter dropped");
    assert_eq!(driver.tokens_total(), 120, "driver counters reset too");
}

#[tokio::test(start_paused = true)]
async fn manual_compact_runs_1200ms_and_lands_the_numbers() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "turn done", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    let before_tokens = driver.tokens_total();
    submit(&mut model, "/compact");
    pump_until(&mut driver, &mut rx, &mut model, "compacted", |m| {
        !m.turn_active
            && m.projection.entries().iter().any(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::Item(block)
                        if matches!(block.item, haider_protocol::item::TurnItem::ContextCompaction { .. })
                )
            })
    })
    .await;
    let (before, after) = model
        .projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Item(block) => match block.item {
                haider_protocol::item::TurnItem::ContextCompaction {
                    tokens_before,
                    tokens_after,
                    ..
                } => Some((tokens_before, tokens_after)),
                _ => None,
            },
            _ => None,
        })
        .expect("compaction row");
    assert_eq!(before, Some(before_tokens));
    assert_eq!(after, Some(12_000), "6% of the 200k window");
    assert_eq!(model.projection.context_tokens(), 12_000);
}

#[tokio::test(start_paused = true)]
async fn talk_hold_fires_the_canned_phrase_through_the_voice_path() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    model.handle_hit(Hit::TalkChip);
    assert!(model.listening);
    pump_until(&mut driver, &mut rx, &mut model, "voice turn done", |m| {
        !m.listening
            && !m.turn_active
            && !m.projection.entries().is_empty()
            && m.projection.badge() == "IDLE"
    })
    .await;
    // The ◉ row + heard note (sim voice path).
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::User { text, voice: true, .. } if text == TALK_PHRASE
    )));
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "◉ heard · whisper-large-v3"
    )));
    // Agent rows of the voice turn are spoken; the tag clears after.
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Item(block)
            if block.spoken
                && matches!(block.item, haider_protocol::item::TurnItem::AgentMessage { .. })
    )));
    assert!(
        !model.projection.voice_live(),
        "Voice(false) closed the tag"
    );
}

#[tokio::test(start_paused = true)]
async fn stop_scripts_cancels_parked_menu_arms() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "this is unreliable");
    pump_until(&mut driver, &mut rx, &mut model, "menu open", |m| {
        m.projection.open_menu().is_some()
    })
    .await;
    let menu_id = model.projection.open_menu().expect("open").id.clone();
    // A fresh session bumps the generation and clears parked arms.
    driver.handle_request(&mut model, AppRequest::StopScripts);
    driver
        .sender()
        .try_send((
            driver.generation(),
            DemoEvent::Envelope(EventPayload::MenuAnswered(
                haider_protocol::menu::MenuAnswer {
                    menu: menu_id,
                    option_key: None,
                    option_index: 0,
                    value: None,
                    via: haider_protocol::menu::AnswerVia::Tui,
                },
            )),
        ))
        .expect("send");
    let (generation, event) = rx.recv().await.expect("the answer echo itself");
    driver.consume(&mut model, generation, event);
    // Let any (wrong) continuation surface on virtual time, then assert
    // the channel is silent — the arm never played.
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    tokio::task::yield_now().await;
    assert!(
        rx.try_recv().is_err(),
        "no arm beats after StopScripts — the park was cancelled"
    );
    assert_eq!(driver.tokens_total(), 0, "fresh session, fresh meter");
}
