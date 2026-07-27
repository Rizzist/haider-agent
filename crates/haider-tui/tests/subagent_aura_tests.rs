//! TUI3b commit 2: the subagent chip system (§2) and aura mode (§3) —
//! beat-level verbatim tests plus paused-time driver lifecycles (question
//! → answer → collect → auto-resume, recovery, close/removal, nested
//! delegation's INTENDED flow) and the render surfaces (SubTree panel,
//! subagent view, aura stage).
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::menu::{AnswerVia, MenuAnswer};
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, Screen, tree_live_count};
use haider_tui::projection::TranscriptEntry;
use haider_tui::render::render;
use haider_tui::runtime::DemoDriver;
use haider_tui::script::{
    AuraState, Beat, ChipDisplayState, DemoEvent, aura_is_status, aura_spawn_beats, aura_target,
    auto_resume_beats, child_run_docs, child_run_tests, respond_chip_beats,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn launcher_model() -> AppModel {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model
}

fn submit(model: &mut AppModel, text: &str) {
    for c in text.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
}

fn draw(
    model: &AppModel,
    width: u16,
    height: u16,
) -> (Vec<String>, Vec<(ratatui::layout::Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, hits)
}

fn drain(driver: &mut DemoDriver, model: &mut AppModel) {
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    for request in requests {
        driver.handle_request(model, request);
    }
}

fn echo_answers(driver: &DemoDriver, model: &mut AppModel) {
    while !model.outbox.is_empty() {
        let pending = model.outbox.remove(0);
        driver
            .sender()
            .try_send((
                driver.control_tag(),
                DemoEvent::Answer {
                    origin: pending.origin,
                    answer: pending.answer,
                },
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
    for _ in 0..400_000 {
        if stop(model) {
            return;
        }
        pump_one(driver, rx, model).await;
    }
    panic!("pump_until({what}): condition never satisfied");
}

/// Answer a chip question card directly (the chip-view digit path pushes
/// the same MenuAnswer).
fn answer_chip_menu(model: &mut AppModel, menu: &str, index: u32) {
    model.outbox.push(haider_tui::app::OutboundAnswer {
        origin: model.session_epoch,
        answer: MenuAnswer {
            menu: haider_protocol::ids::MenuId::new(menu),
            option_key: None,
            option_index: index,
            value: None,
            via: AnswerVia::Tui,
        },
    });
}

// ---- Beat-level: chip scripts + aura scripts are sim-verbatim ----

#[test]
fn child_scripts_are_verbatim_with_question_arms() {
    let tests = child_run_tests("t1-tests", 1);
    // The question + parent note, verbatim.
    assert!(tests.iter().any(|beat| matches!(
        beat,
        Beat::ChipQuestion { recovery: false, text, options, .. }
            if text == "Run the suite against testcontainers or mocks?"
                && options[0] == "testcontainers — real db, slower"
                && options[1] == "mocks — fast, less coverage"
    )));
    assert!(tests.iter().any(|beat| matches!(
        beat,
        Beat::Note(text)
            if text == "· subagent tests needs input — its chip is holding an amber ? — click it to answer"
    )));
    let arms = tests
        .iter()
        .find_map(|beat| match beat {
            Beat::AwaitMenu { arms, .. } => Some(arms),
            _ => None,
        })
        .expect("question park");
    assert_eq!(arms.len(), 2);
    // Per-arm suite command (tui.js:1057-1074).
    let arm_cmd = |arm: &[Beat]| {
        arm.iter()
            .find_map(|beat| match beat {
                Beat::ChipEmit {
                    payload:
                        EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                            item: haider_protocol::item::TurnItem::ToolCall { args, .. },
                            ..
                        }),
                    ..
                } => args["desc"].as_str().map(str::to_owned),
                _ => None,
            })
            .expect("arm tool")
    };
    assert_eq!(
        arm_cmd(&arms[0]),
        "cargo test -p billing --tests -- --ignored"
    );
    assert_eq!(arm_cmd(&arms[1]), "cargo test -p billing --tests");
    assert!(
        arms.iter()
            .all(|arm| matches!(arm.last(), Some(Beat::AutoResume))),
        "both arms end in the auto-resume check"
    );

    let docs = child_run_docs("t1-docs", 1);
    assert!(docs.iter().any(|beat| matches!(
        beat,
        Beat::ChipQuestion { recovery: true, text, .. }
            if text == "cargo doc failed (exit 101 — the docs feature flag is missing). How should I recover?"
    )));
    assert!(docs.iter().any(|beat| matches!(
        beat,
        Beat::Note(text)
            if text == "· subagent docs failed (✗) — open its row to pick a recovery"
    )));
    let docs_arms = docs
        .iter()
        .find_map(|beat| match beat {
            Beat::AwaitMenu { arms, .. } => Some(arms),
            _ => None,
        })
        .expect("recovery park");
    assert!(matches!(
        docs_arms[1].as_slice(),
        [Beat::ChipClose { agent }] if agent == "t1-docs"
    ));
}

#[test]
fn respond_chip_nested_dead_ends_exactly_as_the_shipped_sim_does() {
    // TUI3.1 P2-14: tui.js is the authority. The sim wraps the nested
    // agent_spawn in `if (!(await ops.cTool(...))) return;` and `cTool`
    // resolves to undefined — so the turn ALWAYS returns there. TUI3b
    // shipped the "intended" continuation; this reverts to the sim.
    let roster = std::sync::atomic::AtomicU64::new(5);
    let beats = respond_chip_beats(
        "t1-tests",
        "Hasan",
        "gpt-5.6",
        "local",
        "please spawn a helper",
        7,
        &roster,
    );
    // Nested chip added under the parent with the spawn note prefilled.
    let seed = beats
        .iter()
        .find_map(|beat| match beat {
            Beat::ChipAdd(seed) => Some(seed),
            _ => None,
        })
        .expect("nested chip");
    assert_eq!(seed.parent.as_deref(), Some("t1-tests"));
    assert_eq!(seed.name, "subtask");
    assert_eq!(seed.tokens, 400);
    assert_eq!(seed.state, ChipDisplayState::Running);
    // The LAST beat is the agent_spawn tool's token accrual: nothing after
    // the sim's early return exists.
    assert!(
        matches!(beats.last(), Some(Beat::ChipTokens { agent, n: 1800 }) if agent == "t1-tests"),
        "the script stops at the spawn tool"
    );
    for forbidden in ["Waiting", "Done"] {
        assert!(
            !beats.iter().any(|beat| matches!(
                beat,
                Beat::ChipState { state, .. } if format!("{state:?}") == forbidden
            )),
            "no {forbidden} beat survives the sim's early return"
        );
    }
    assert!(
        !beats.iter().any(|beat| matches!(beat, Beat::AutoResume)),
        "the parent turn is never discharged — that is the sim bug"
    );
    // The NON-nested path is unaffected and still completes + resumes.
    let plain = respond_chip_beats(
        "t1-tests",
        "Hasan",
        "gpt-5.6",
        "local",
        "tighten the assertions",
        8,
        &roster,
    );
    assert!(matches!(plain.last(), Some(Beat::AutoResume)));
}

#[test]
fn auto_resume_and_aura_builders_are_verbatim() {
    let resume = auto_resume_beats(2, 9);
    assert!(matches!(
        &resume[0],
        Beat::Note(text)
            if text == "· all subagents reported — resuming the parked turn (waiting → thinking, never idle)"
    ));
    assert!(resume.iter().any(|beat| matches!(
        beat,
        Beat::Emit(EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
            item: haider_protocol::item::TurnItem::AgentMessage { text },
            ..
        })) if text == "Folding the 2 subagent reports into the main line — results merged, and the turn can now commit."
    )));
    assert!(matches!(resume.last(), Some(Beat::TurnEnd)));

    // Aura matchers (tui.js:2058-2124).
    assert!(aura_is_status("what are you doing"));
    assert!(aura_is_status("status report please"));
    assert!(!aura_is_status("spin up billing"));
    assert_eq!(
        aura_target("spin up the auth service on hetzner-1 and run its tests"),
        ("auth".to_owned(), "hetzner-1".to_owned())
    );
    assert_eq!(
        aura_target("start billing-service please"),
        ("billing".to_owned(), "workstation".to_owned())
    );
    let spawn = aura_spawn_beats(true, "auth", "hetzner-1", 1);
    assert!(spawn.iter().any(|beat| matches!(
        beat,
        Beat::AuraEmit(EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
            item: haider_protocol::item::TurnItem::AgentMessage { text },
            ..
        })) if text == "On it — I'll place a auth session on hetzner-1, start the work, and report back. I don't touch the code myself."
    )));
    assert!(spawn.iter().any(|beat| matches!(
        beat,
        Beat::AuraLog(text) if text == "agent_spawn — auth → hetzner-1 · lease ok"
    )));
    assert!(spawn.iter().any(|beat| matches!(
        beat,
        Beat::AuraLog(text) if text == "auth on hetzner-1: tests green ✓"
    )));
}

// ---- Driver lifecycles (paused time) ----

#[tokio::test(start_paused = true)]
async fn two_subagents_question_recovery_collect_and_auto_resume() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "use two subagents to split this work");
    // The tests chip parks on its question; the parent turn finishes.
    pump_until(&mut driver, &mut rx, &mut model, "tests question", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.state == ChipDisplayState::InputRequired)
            && !m.turn_active
    })
    .await;
    assert_eq!(model.chips.len(), 2, "both chips landed");
    // Derived WAITING badge: parent idle, children live (§2.6).
    let (badge, _) = model.status_badge();
    assert!(
        badge.starts_with("◔ WAITING · "),
        "idle + live chips → derived WAITING, got {badge}"
    );
    // The tests chip's question card lives in ITS transcript (Subagent
    // scope), never blocking the session composer.
    assert!(model.projection.open_menu().is_none());
    let tests_menu = haider_tui::app::find_chip(&model.chips, "t1-tests")
        .and_then(|chip| chip.transcript.open_menu())
        .expect("chip card open");
    assert_eq!(
        tests_menu.title,
        "Run the suite against testcontainers or mocks?"
    );
    // Answer "mocks"; then let the docs failure park and answer "retry".
    answer_chip_menu(&mut model, "t1-tests-q", 1);
    pump_until(&mut driver, &mut rx, &mut model, "docs recovery", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-docs")
            .is_some_and(|chip| chip.state == ChipDisplayState::Error)
    })
    .await;
    answer_chip_menu(&mut model, "t1-docs-q", 0);
    // Everything settles: chips done, auto-resume folds the reports.
    pump_until(&mut driver, &mut rx, &mut model, "auto resume done", |m| {
        tree_live_count(&m.chips) == 0
            && !m.turn_active
            && m.projection.entries().iter().any(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::Item(block)
                        if matches!(
                            &block.item,
                            haider_protocol::item::TurnItem::AgentMessage { text }
                                if text.starts_with("Folding the 2 subagent reports")
                        )
                )
            })
            && m.projection.badge() == "IDLE"
    })
    .await;
    // Parent transcript: both collect rows + the verbatim notes.
    for note in [
        "· subagent tests finished — report merged",
        "· all subagents reported — resuming the parked turn (waiting → thinking, never idle)",
    ] {
        assert!(
            model.projection.entries().iter().any(|entry| matches!(
                entry,
                TranscriptEntry::Note { text } if text == note
            )),
            "missing note: {note}"
        );
    }
    let collects = model
        .projection
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                TranscriptEntry::Item(block)
                    if matches!(
                        &block.item,
                        haider_protocol::item::TurnItem::ToolCall { name, .. } if name == "agent_control"
                    )
            )
        })
        .count();
    assert_eq!(collects, 2, "collect rows for tests + docs");
    // Chip transcripts carry the chosen option + resolution note.
    let tests_chip = haider_tui::app::find_chip(&model.chips, "t1-tests").expect("tests");
    assert!(tests_chip.transcript.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::User { text, .. } if text == "mocks — fast, less coverage"
    )));
    assert!(tests_chip.transcript.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· input resolved — continuing"
    )));
    assert!(tests_chip.tokens > 1200, "chip token law accrued");
    let (badge, _) = model.status_badge();
    assert_eq!(badge, "IDLE", "no live chips → no derived WAITING");
}

#[tokio::test(start_paused = true)]
async fn docs_close_arm_runs_the_close_lifecycle_and_5s_removal() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "use two subagents please");
    pump_until(&mut driver, &mut rx, &mut model, "docs recovery", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-docs")
            .is_some_and(|chip| chip.state == ChipDisplayState::Error)
    })
    .await;
    // Answer the tests question too so the tree can settle later.
    answer_chip_menu(&mut model, "t1-tests-q", 0);
    // Recovery idx 1: close this subagent — keep the patch.
    answer_chip_menu(&mut model, "t1-docs-q", 1);
    pump_until(&mut driver, &mut rx, &mut model, "docs closed", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-docs").is_some_and(|chip| chip.closed)
    })
    .await;
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text.starts_with("· subagent ") && text.ends_with(" closed — leaving the tree in 5s")
    )));
    // The closed row renders ⊘ with the closing activity.
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter()
            .any(|row| row.contains("closing · leaves in 5s")),
        "closed row activity"
    );
    // After the 5 s timer the chip leaves the tree.
    pump_until(&mut driver, &mut rx, &mut model, "docs removed", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-docs").is_none()
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn respond_chip_steers_queue_when_blocked_and_delegates_nested_when_asked() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "use a subagent for the webhook tests");
    pump_until(&mut driver, &mut rx, &mut model, "tests question", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.state == ChipDisplayState::InputRequired)
    })
    .await;
    // Steering while the question is pending: the sim-honest note (never
    // actually delivered — ported as-is).
    model.requests.push(AppRequest::ChipSubmit {
        agent: "t1-tests".to_owned(),
        text: "also check the retries".to_owned(),
    });
    pump_until(&mut driver, &mut rx, &mut model, "steer note", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests").is_some_and(|chip| {
            chip.transcript.entries().iter().any(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::Note { text }
                        if text == "· steer queued — delivered when the pending question resolves"
                )
            })
        })
    })
    .await;
    // Resolve the question, let the chip finish, then delegate NESTED.
    answer_chip_menu(&mut model, "t1-tests-q", 0);
    pump_until(&mut driver, &mut rx, &mut model, "tests done", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.state == ChipDisplayState::Done)
            && !m.turn_active
            && m.projection.badge() == "IDLE"
    })
    .await;
    model.requests.push(AppRequest::ChipSubmit {
        agent: "t1-tests".to_owned(),
        text: "delegate part of this".to_owned(),
    });
    // The sim's nested flow DEAD-ENDS at tui.js:1137 (P2-14): the parent
    // chip is left `streaming`, the nested child `running`, and the live
    // descendant keeps the whole session in the derived WAITING badge.
    pump_until(&mut driver, &mut rx, &mut model, "nested parked", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| !chip.children.is_empty())
    })
    .await;
    let tests_chip = haider_tui::app::find_chip(&model.chips, "t1-tests").expect("tests");
    let nested = &tests_chip.children[0];
    assert_eq!(nested.name, "subtask");
    assert!(nested.transcript.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· spawned by Hasan — nested delegation"
    )));
    assert_eq!(
        nested.state,
        ChipDisplayState::Running,
        "child never finishes"
    );
    assert_eq!(
        tests_chip.state,
        ChipDisplayState::Streaming,
        "parent is stranded mid-stream, exactly as the sim strands it"
    );
    assert!(tree_live_count(&model.chips) > 0);
    let (badge, _) = model.status_badge();
    assert!(
        badge.starts_with("◔ WAITING · "),
        "session waits, got {badge}"
    );
    // Closing the stranded parent takes its subtree with it and discharges
    // the wait — the only way out, as in the sim.
    model.view_path = vec!["t1-tests".to_owned()];
    model.screen = Screen::Subagent;
    model.handle_hit(Hit::ChipCloseBtn("t1-tests".to_owned()));
    pump_until(&mut driver, &mut rx, &mut model, "closed", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests").is_some_and(|chip| chip.closed)
    })
    .await;
    // Sim-true: the nested child is not itself `closed`, so it keeps the
    // tree live until the 5 s removal takes the whole subtree out.
    assert_eq!(
        tree_live_count(&model.chips),
        1,
        "the child leaves with the subtree"
    );
    pump_until(&mut driver, &mut rx, &mut model, "subtree gone", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests").is_none()
    })
    .await;
    assert_eq!(
        tree_live_count(&model.chips),
        0,
        "the wait discharges with the subtree"
    );
}

#[tokio::test(start_paused = true)]
async fn aura_orchestrates_spawn_and_status_with_talk_and_toggles() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "/aura");
    assert_eq!(model.screen, Screen::Aura);
    assert!(model.window_title().ends_with("· aura"));
    // Seed state (tui.js:121-138).
    assert_eq!(model.aura.roster.len(), 1);
    assert_eq!(model.aura.log.len(), 2);
    // Spawn branch, typed.
    submit(
        &mut model,
        "spin up billing on workstation and run its tests",
    );
    pump_until(&mut driver, &mut rx, &mut model, "aura spawn done", |m| {
        m.aura.state == AuraState::Idle && m.aura.roster.len() == 2
    })
    .await;
    let row = &model.aura.roster[1];
    assert_eq!(row.name, "billing");
    assert_eq!(row.device, "workstation");
    assert_eq!(row.state, ChipDisplayState::Done);
    assert_eq!(row.activity, "tests green");
    assert!(
        model
            .aura
            .log
            .iter()
            .any(|line| line == "billing on workstation: tests green ✓")
    );
    // Spoken agent rows (audio on) tag ♪.
    assert!(model.aura.transcript.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Item(block) if block.spoken
    )));
    // Status branch.
    submit(&mut model, "what are you doing");
    pump_until(&mut driver, &mut rx, &mut model, "status reply", |m| {
        m.aura.state == AuraState::Idle
            && m.aura.transcript.entries().iter().any(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::Item(block)
                        if matches!(
                            &block.item,
                            haider_protocol::item::TurnItem::AgentMessage { text }
                                if text.starts_with("Current roster: ")
                                    && text.ends_with(". Say the word to spin up more.")
                        )
                )
            })
    })
    .await;
    // Engine swap + mute notes (verbatim).
    model.handle_hit(Hit::AuraEngine);
    assert!(model.aura.transcript.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· engine hot-swapped → whisper → gpt-5.6 → openai · dialogue kept"
    )));
    model.handle_hit(Hit::AuraMute);
    assert!(model.aura.transcript.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· audio output muted — orchestrating silently, activity still shown"
    )));
    // Muted runs stream unspoken (`■ aura · muted`).
    submit(&mut model, "spin up the api service on phone");
    pump_until(&mut driver, &mut rx, &mut model, "muted spawn", |m| {
        m.aura.state == AuraState::Idle && m.aura.roster.len() == 3
    })
    .await;
    let unspoken = model
        .aura
        .transcript
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            TranscriptEntry::Item(block) => Some(block.spoken),
            _ => None,
        })
        .expect("muted agent row");
    assert!(!unspoken, "muted turn is not spoken");
    model.handle_hit(Hit::AuraMute);
    // Talk: listening → 1100 ms → the canned phrase as a ◉ voice run.
    model.handle_hit(Hit::AuraTalkBtn);
    assert_eq!(model.aura.state, AuraState::Listening);
    pump_until(&mut driver, &mut rx, &mut model, "talk spawn", |m| {
        m.aura.state == AuraState::Idle && m.aura.roster.iter().any(|row| row.name == "auth")
    })
    .await;
    assert!(model.aura.transcript.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::User { text, voice: true, .. }
            if text == "spin up the auth service on hetzner-1 and run its tests"
    )));
    // Esc: no session attached → launcher; aura state persists.
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Launcher);
    assert_eq!(model.aura.roster.len(), 4);
}

// ---- Render surfaces ----

#[tokio::test(start_paused = true)]
async fn subtree_panel_and_subagent_view_render_the_sim_anatomy() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "use two subagents for this");
    pump_until(&mut driver, &mut rx, &mut model, "tests question", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.state == ChipDisplayState::InputRequired)
            && !m.turn_active
    })
    .await;
    // SubTree panel on the SESSION screen: header counts + rows.
    let (rows, hits) = draw(&model, 130, 40);
    // NB: the parent's own stream text contains "subagents —" ("Spinning up
    // two subagents — …"), so the panel header is found by its ▾ arrow.
    let header = rows
        .iter()
        .find(|row| row.contains("▾ subagents"))
        .expect("subtree header");
    assert!(header.contains("? 1 needs input"));
    assert!(
        rows.iter()
            .any(|row| row.contains("Hasan (a) · tests · gpt-5.6 · local"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("├─") || row.contains("└─")),
        "tree connectors"
    );
    assert!(
        hits.iter()
            .any(|(_, hit)| matches!(hit, Hit::ChipRow(agent) if agent == "t1-tests")),
        "chip rows are clickable"
    );
    // Collapse via the header toggle.
    model.handle_hit(Hit::SubTreeToggle);
    let (rows, _) = draw(&model, 130, 40);
    assert!(rows.iter().any(|row| row.contains("▸ subagents")));
    assert!(!rows.iter().any(|row| row.contains("├─")));
    model.handle_hit(Hit::SubTreeToggle);
    // Open the tests chip view: breadcrumb, question card, placeholder law.
    model.handle_hit(Hit::ChipRow("t1-tests".to_owned()));
    assert_eq!(model.screen, Screen::Subagent);
    let (rows, hits) = draw(&model, 130, 40);
    assert!(
        rows.iter()
            .any(|row| row.contains("Hasan (a)") && row.contains("✕ close")),
        "breadcrumb head + close"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("? Run the suite against testcontainers or mocks?")),
        "the question replaces the chip composer"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("the parent turn is not blocked · esc back to session")),
        "chip-menu footer law"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("⌂") && row.contains("back to the main transcript")),
        "the ⌂ home row joins the panel on the subagent screen"
    );
    assert!(
        rows.iter().any(|row| row.contains("viewing ←")),
        "the viewed row is marked"
    );
    assert!(
        hits.iter().any(|(_, hit)| matches!(hit, Hit::SessionHome)),
        "home is clickable"
    );
    // Esc walks back without touching the parent.
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session);
}

#[tokio::test(start_paused = true)]
async fn subtree_sheds_after_todos_but_before_the_composer_at_90x10() {
    let (mut driver, mut rx) = DemoDriver::new(64);
    let mut model = launcher_model();
    submit(&mut model, "use a subagent here");
    pump_until(&mut driver, &mut rx, &mut model, "chip live", |m| {
        !m.chips.is_empty()
    })
    .await;
    // 90×10 budget: status(1) + header(2) + rule(1) + transcript(1) +
    // input rule(1) + composer(1) + gap(1) leaves 2 rows — a 1-chip panel
    // (header + row = 2) fits; the composer never yields.
    let (rows, _) = draw(&model, 90, 10);
    assert!(
        rows.iter().any(|row| row.contains("subagents —")),
        "panel fits"
    );
    assert!(
        rows.iter().any(|row| row.contains("❯ ▮")),
        "composer intact"
    );
    // With a TODOS panel competing, the todos shed and the SubTree holds —
    // the map of live work outranks a plan the transcript will re-print
    // (ledger order documented in render.rs).
    model.projection.apply(&EventPayload::Item(
        haider_protocol::item::ItemEvent::Started {
            item_id: haider_protocol::ids::ItemId::new("ledger-plan"),
            item: haider_protocol::item::TurnItem::Plan {
                items: (0..4)
                    .map(|id| haider_protocol::history::TodoItem {
                        id,
                        text: format!("step {id}"),
                        state: haider_protocol::history::TodoState::Listed,
                        dep: id.checked_sub(1),
                    })
                    .collect(),
            },
        },
    ));
    let (rows, _) = draw(&model, 90, 10);
    assert!(
        rows.iter().any(|row| row.contains("▾ subagents")),
        "the SubTree outranks the todos"
    );
    assert!(!rows.iter().any(|row| row.contains("▾ todos")));
    assert!(
        rows.iter().any(|row| row.contains("❯ ▮")),
        "composer intact"
    );
    // With a queue panel competing, the SUBTREE sheds first (the queue
    // holds unsent user input — ledger order documented in render.rs).
    model.queue_mode = true;
    model.msg_queue.push("queued line".to_owned());
    let (rows, _) = draw(&model, 90, 10);
    assert!(rows.iter().any(|row| row.contains("⧗ queued — 1 message")));
    assert!(!rows.iter().any(|row| row.contains("▾ subagents")));
    assert!(
        rows.iter().any(|row| row.contains("❯ ▮")),
        "composer intact"
    );
}

#[test]
fn aura_stage_renders_bar_orb_columns_and_transcript() {
    let mut model = launcher_model();
    submit(&mut model, "/aura");
    let (rows, hits) = draw(&model, 130, 40);
    assert!(
        rows.iter()
            .any(|row| row.contains("◉ AURA") && row.contains("voice session · orchestrator"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("[ engine · gpt-realtime-2 ⇄ ]"))
    );
    assert!(rows.iter().any(|row| row.contains("[ ♪ audio on ]")));
    assert!(rows.iter().any(|row| row.contains("[ exit ⤶ ]")));
    assert!(rows.iter().any(|row| row.contains("◌ IDLE")));
    assert!(rows.iter().any(|row| row.contains(
        "native duplex · never writes code — it spawns and steers sessions on your devices"
    )));
    assert!(rows.iter().any(|row| row.contains("[ ◉ hold to talk ]")));
    assert!(rows.iter().any(|row| row.contains("controlled sessions")));
    assert!(
        rows.iter()
            .any(|row| row.contains("✓ billing-service · workstation — webhook tests green"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("activity — doing / done"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("· spawned billing-service on workstation"))
    );
    assert!(rows.iter().any(|row| row.contains("■ aura · ♪")));
    assert!(
        rows.iter()
            .any(|row| row.contains("Aura online. I orchestrate sessions across your"))
    );
    assert!(rows.iter().any(|row| row.contains(
        "speak or type — e.g. “spin up billing-service on workstation and run its tests”"
    )));
    for hit in [
        Hit::AuraEngine,
        Hit::AuraMute,
        Hit::AuraExit,
        Hit::AuraTalkBtn,
    ] {
        assert!(
            hits.iter().any(|(_, candidate)| *candidate == hit),
            "missing hit {hit:?}"
        );
    }
    // The session talk chip stays off the aura composer (aura has its own
    // hold-to-talk button).
    assert!(!rows.iter().any(|row| row.contains("[ ◉ talk ]")));
}
