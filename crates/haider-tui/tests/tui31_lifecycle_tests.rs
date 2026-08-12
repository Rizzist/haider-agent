//! TUI3.1 — the review's stale-work and invisible-but-active classes.
//!
//! Every P1 here is "work spawned on one surface lands after the user left
//! it", so each test drives the PRODUCTION dispatch (keys, hits, requests)
//! and then keeps pumping the real driver channel: a regression shows up as
//! a mutation that arrives after the teardown, never as an internal call.
#![allow(clippy::expect_used)]

use haider_protocol::menu::{AnswerVia, MenuAnswer};
use haider_tui::app::{AppModel, AppRequest, Hit, LauncherRow, Screen, tree_live_count};
use haider_tui::projection::TranscriptEntry;
use haider_tui::render::render;
use haider_tui::runtime::DemoDriver;
use haider_tui::script::{AuraState, ChipDisplayState, DemoEvent};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{drain, driver_for, key, launcher_model, submit};

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
        let (generation, event) =
            tokio::time::timeout(std::time::Duration::from_secs(3600), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("pump_until({what}): the driver went silent"))
                .expect("channel open");
        driver.consume(model, generation, event);
        drain(driver, model);
        echo_answers(driver, model);
    }
    panic!("pump_until({what}): condition never satisfied");
}

/// Consume everything the driver still has to say (paused time makes this
/// exact: the loop ends only when no timer can fire any more). This is the
/// stale-work probe — anything that lands here landed AFTER the teardown.
async fn pump_quiet(
    driver: &mut DemoDriver,
    rx: &mut tokio::sync::mpsc::Receiver<(u64, DemoEvent)>,
    model: &mut AppModel,
    budget: usize,
) {
    drain(driver, model);
    echo_answers(driver, model);
    for _ in 0..budget {
        let got = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv()).await;
        let Ok(Some((generation, event))) = got else {
            return;
        };
        driver.consume(model, generation, event);
        drain(driver, model);
        echo_answers(driver, model);
    }
}

fn answer_menu(model: &mut AppModel, menu: &str, index: u32) {
    model.outbox.push(haider_tui::app::OutboundAnswer {
        origin: model.ui_generation(),
        branch: None,
        answer: MenuAnswer {
            menu: haider_protocol::ids::MenuId::new(menu),
            option_key: None,
            option_index: index,
            value: None,
            via: AnswerVia::Tui,
        },
    });
}

fn chip_entries(model: &AppModel, agent: &str) -> usize {
    haider_tui::app::find_chip(&model.chips, agent)
        .map_or(0, |chip| chip.transcript.entries().len())
}

// ---- P1-1: chip lifecycles ----

#[tokio::test(start_paused = true)]
async fn interrupt_leaves_chips_running_and_their_card_still_resolves() {
    // Review P1-1's real defect was the HYBRID: chip beats kept running
    // while their parked continuations were thrown away, so answering a
    // chip's card after an interrupt closed the menu and blocked the chip
    // forever. tui.js is the authority on WHICH half to keep: `interrupt`
    // (tui.js:1551-1567) touches only the run token, the queue and the
    // note, so children outlive the cancelled parent turn — and now their
    // parked arms do too.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "use a subagent for the webhook tests");
    pump_until(&mut driver, &mut rx, &mut model, "chip card", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.state == ChipDisplayState::InputRequired)
    })
    .await;
    assert!(model.turn_active, "the parent turn is still running");
    // Esc mid-turn: the SESSION's work stops.
    model.handle(key(KeyCode::Esc));
    drain(&mut driver, &mut model);
    assert!(model.projection.interrupted(), "idle (i)");
    let session_rows = model.projection.entries().len();

    // The chip's card still answers — the whole point.
    answer_menu(&mut model, "t1-tests-q", 1);
    pump_until(&mut driver, &mut rx, &mut model, "chip resolved", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.state == ChipDisplayState::Done)
    })
    .await;
    let chip = haider_tui::app::find_chip(&model.chips, "t1-tests").expect("chip");
    assert!(chip.transcript.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· input resolved — continuing"
    )));
    assert!(
        chip.question.as_ref().is_some_and(|q| q.resolved),
        "never blocked forever"
    );
    // The cancelled SESSION turn stays cancelled: no stream beat of the
    // interrupted turn may re-arm it.
    pump_quiet(&mut driver, &mut rx, &mut model, 4_000).await;
    assert!(!model.turn_active, "the interrupted turn never resumes");
    assert!(
        model.projection.entries().len() > session_rows,
        "the chip's own collect rows still reach the parent"
    );
}

#[tokio::test(start_paused = true)]
async fn closing_a_chip_stops_its_script_at_once() {
    // Review P1-1: closing only set closed/removing while the chip's script
    // kept streaming into its transcript.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "use two subagents to split this work");
    pump_until(&mut driver, &mut rx, &mut model, "docs streaming", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-docs")
            .is_some_and(|chip| chip.transcript.entries().len() >= 2)
            && haider_tui::app::find_chip(&m.chips, "t1-docs")
                .is_some_and(|chip| chip.state != ChipDisplayState::Error)
    })
    .await;
    model.requests.push(AppRequest::ChipClose {
        agent: "t1-docs".to_owned(),
    });
    drain(&mut driver, &mut model);
    let frozen = chip_entries(&model, "t1-docs");
    pump_quiet(&mut driver, &mut rx, &mut model, 6_000).await;
    // The chip may have LEFT the tree (its 5 s removal is a fresh arm that
    // survives the cancellation on purpose); if it is still there, its
    // transcript must not have grown by a single row.
    if haider_tui::app::find_chip(&model.chips, "t1-docs").is_some() {
        assert_eq!(
            chip_entries(&model, "t1-docs"),
            frozen,
            "a closed chip's script must not keep writing"
        );
    }
    assert!(
        model.projection.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Note { text }
                if text.starts_with("· subagent ") && text.ends_with("closed — leaving the tree in 5s")
        )),
        "the close note still lands"
    );
}

#[tokio::test(start_paused = true)]
async fn a_fresh_session_cancels_chip_work() {
    // Review r1 P1-1: chip arms were only half-guarded, so a torn-down
    // session's chip scripts bled into the next one.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "use two subagents to split this work");
    pump_until(&mut driver, &mut rx, &mut model, "chips live", |m| {
        m.chips.len() == 2
    })
    .await;
    // /clear tears the session down.
    submit(&mut model, "/clear");
    drain(&mut driver, &mut model);
    assert_eq!(model.screen, Screen::Launcher);
    assert!(model.chips.is_empty(), "the tree went with the session");
    pump_quiet(&mut driver, &mut rx, &mut model, 8_000).await;
    assert!(
        model.projection.entries().is_empty(),
        "no stale note or row leaked into the fresh projection"
    );
    assert!(model.chips.is_empty(), "no chip re-materialized");
    assert!(!model.turn_active);
}

// ---- P1-2: aura lifecycle ----

#[tokio::test(start_paused = true)]
async fn aura_runs_survive_clear_but_reset_stops_them() {
    // Review r2 P2-5 corrected round 1: the sim's `/clear` and main-session
    // interrupt do NOT advance `auraRunRef` (tui.js:1950-1955) — only
    // `/reset` (tui.js:1930) and the next `orchestrate` do. A background
    // orchestration must therefore FINISH across navigation.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "/aura");
    submit(
        &mut model,
        "spin up billing on workstation and run its tests",
    );
    pump_until(&mut driver, &mut rx, &mut model, "run started", |m| {
        m.aura.roster.len() == 2
    })
    .await;
    let log_at_clear = model.aura.log.len();
    submit(&mut model, "/clear");
    drain(&mut driver, &mut model);
    assert_eq!(model.screen, Screen::Launcher);
    pump_until(&mut driver, &mut rx, &mut model, "run finished", |m| {
        m.aura.state == AuraState::Idle && m.aura.roster[1].activity == "tests green"
    })
    .await;
    assert!(
        model.aura.log.len() > log_at_clear,
        "the orchestration kept reporting after /clear, as the sim does"
    );
    assert!(
        model
            .aura
            .log
            .iter()
            .any(|line| line == "billing on local: tests green ✓"),
        "and ran to its final log line"
    );

    // /reset IS the cancel point — and it reseeds.
    submit(&mut model, "/aura");
    submit(&mut model, "spin up the api service on phone");
    pump_until(
        &mut driver,
        &mut rx,
        &mut model,
        "second run started",
        |m| m.aura.roster.iter().any(|row| row.name == "api"),
    )
    .await;
    submit(&mut model, "/reset");
    drain(&mut driver, &mut model);
    assert_eq!(model.aura.roster.len(), 1, "reseeded");
    assert_eq!(model.aura.state, AuraState::Idle);
    let log = model.aura.log.len();
    pump_quiet(&mut driver, &mut rx, &mut model, 8_000).await;
    assert_eq!(
        model.aura.roster.len(),
        1,
        "the cancelled run added nothing"
    );
    assert_eq!(model.aura.log.len(), log);
}

#[tokio::test(start_paused = true)]
async fn a_session_interrupt_leaves_a_background_orchestration_running() {
    // The other half of review r2 P2-5: an interrupt is session-scoped.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "/aura");
    submit(
        &mut model,
        "spin up billing on workstation and run its tests",
    );
    pump_until(&mut driver, &mut rx, &mut model, "run started", |m| {
        m.aura.roster.len() == 2
    })
    .await;
    // Leave the stage, start a session, then interrupt THAT turn.
    model.handle(key(KeyCode::Esc));
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "thinking", |m| {
        m.projection.badge() == "● THINKING"
    })
    .await;
    model.handle(key(KeyCode::Esc));
    drain(&mut driver, &mut model);
    assert!(model.projection.interrupted());
    pump_until(&mut driver, &mut rx, &mut model, "aura finished", |m| {
        m.aura.state == AuraState::Idle && m.aura.roster[1].activity == "tests green"
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn two_rapid_aura_submits_cannot_interleave() {
    // Review P1-2: the state stayed Idle until an async beat, so the sim's
    // `state === "idle"` gate let a second submit start concurrently.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "/aura");
    submit(&mut model, "spin up billing on workstation");
    assert_ne!(model.aura.state, AuraState::Idle, "the orb leaves idle NOW");
    let requests = model.requests.len();
    submit(&mut model, "spin up api on phone");
    assert_eq!(
        model.requests.len(),
        requests,
        "the second submit is refused while a run owns the orb"
    );
    pump_until(&mut driver, &mut rx, &mut model, "run done", |m| {
        m.aura.state == AuraState::Idle
    })
    .await;
    assert_eq!(model.aura.roster.len(), 2, "exactly one run landed");
    assert_eq!(model.aura.roster[1].name, "billing");
}

// ---- P1-3: the talk hold ----

#[tokio::test(start_paused = true)]
async fn a_talk_hold_cancelled_by_esc_never_starts_a_session() {
    // Review P1-3: Esc navigated to the Launcher without touching the 1.3 s
    // timer, and TalkFire then called fresh_session from there.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "turn done", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    model.handle_hit(Hit::TalkChip);
    assert!(model.listening);
    drain(&mut driver, &mut model);
    // Idle Esc stays session-scoped (owner directive) — and still
    // cancels the hold (P1-3's core law).
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session, "esc never navigates");
    assert!(!model.listening, "Esc cancels the hold");
    let rows = model.projection.entries().len();
    pump_quiet(&mut driver, &mut rx, &mut model, 4_000).await;
    assert_eq!(model.screen, Screen::Session, "no navigation was conjured");
    assert_eq!(model.projection.entries().len(), rows, "no canned turn ran");
    assert!(!model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::User { text, .. } if text == haider_tui::script::TALK_PHRASE
    )));
}

// ---- P1-4: chip item ids ----

#[tokio::test(start_paused = true)]
async fn a_second_message_to_a_chip_renders_in_full() {
    // Review P1-4: fixed `g1`/`g2` suffixes made the second chip turn reuse
    // closed item ids, and the projection dropped every row of it.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "use a subagent for the webhook tests");
    pump_until(&mut driver, &mut rx, &mut model, "chip card", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.state == ChipDisplayState::InputRequired)
    })
    .await;
    answer_menu(&mut model, "t1-tests-q", 1);
    pump_until(&mut driver, &mut rx, &mut model, "chip done", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.state == ChipDisplayState::Done)
    })
    .await;
    for text in ["tighten the assertions", "and re-run the suite"] {
        model.requests.push(AppRequest::ChipSubmit {
            agent: "t1-tests".to_owned(),
            text: text.to_owned(),
        });
        pump_until(&mut driver, &mut rx, &mut model, text, |m| {
            haider_tui::app::find_chip(&m.chips, "t1-tests").is_some_and(|chip| {
                chip.state == ChipDisplayState::Done
                    && chip.transcript.entries().iter().any(|entry| {
                        matches!(
                            entry,
                            TranscriptEntry::User { text: row, .. } if row == text
                        )
                    })
            })
        })
        .await;
    }
    let chip = haider_tui::app::find_chip(&model.chips, "t1-tests").expect("chip");
    let acks = chip
        .transcript
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                TranscriptEntry::Item(block)
                    if matches!(
                        &block.item,
                        haider_protocol::item::TurnItem::AgentMessage { text }
                            if text == "Acknowledged — folding that into the current step."
                    )
            )
        })
        .count();
    assert_eq!(acks, 2, "BOTH chip turns rendered their assistant row");
    let reads = chip
        .transcript
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                TranscriptEntry::Item(block)
                    if matches!(
                        &block.item,
                        haider_protocol::item::TurnItem::ToolCall { name, args, .. }
                            if name == "fs_read"
                                && args["desc"].as_str() == Some("src/target.rs")
                    )
            )
        })
        .count();
    assert_eq!(reads, 2, "and BOTH `fs_read src/target.rs` tool rows");
    assert_eq!(chip.transcript.duplicate_items(), 0, "no id was reused");
}

// ---- P1-5: owning-surface guards on the new hits ----

#[test]
fn stale_hits_from_another_surface_are_dropped() {
    // Review P1-5: chip rows, the subtree toggle, chip close/crumb and every
    // Aura action acted without checking the screen they were rendered on.
    let mut model = launcher_model();
    // Launcher: nothing chip- or aura-shaped may act.
    let before = model.screen;
    model.handle_hit(Hit::SubTreeToggle);
    assert!(!model.subtree_collapsed, "no panel on the launcher");
    model.handle_hit(Hit::SessionHome);
    assert_eq!(model.screen, before);
    model.handle_hit(Hit::ChipRow("ghost".to_owned()));
    assert_eq!(model.screen, before);
    model.handle_hit(Hit::ChipCloseBtn("ghost".to_owned()));
    assert!(
        model.requests.is_empty(),
        "no close request from a stale rect"
    );
    model.handle_hit(Hit::AuraMute);
    assert!(!model.aura.muted, "aura chrome is inert off the aura stage");
    model.handle_hit(Hit::AuraEngine);
    assert!(model.aura.realtime, "engine untouched");
    model.handle_hit(Hit::AuraTalkBtn);
    assert_eq!(model.aura.state, AuraState::Idle);
    assert!(model.requests.is_empty());

    // Aura: the launcher's own rows are inert.
    submit(&mut model, "/aura");
    model.requests.clear();
    common::hit_session_named(&mut model, "billing-service");
    assert_eq!(model.screen, Screen::Aura, "no attach from a stale rect");
    assert!(model.requests.is_empty());
    model.handle_hit(Hit::ExtraRow(LauncherRow::Accounts));
    assert!(
        model.flash.is_none(),
        "launcher rows are inert on the stage"
    );
    // …and the aura chrome works while it IS the owning surface.
    model.handle_hit(Hit::AuraMute);
    assert!(model.aura.muted);
}

// ---- P1-6: the aura sacred-row ledger ----

#[test]
fn the_aura_stage_never_hides_a_live_composer() {
    // Review P1-6: Aura hard-allocated its chrome, so at 90×1 the input was
    // invisible but still accepted typing, and 90×5 painted no bar.
    let mut model = launcher_model();
    submit(&mut model, "/aura");
    for (width, height) in [(90, 10), (90, 5), (90, 1)] {
        let (rows, hits) = draw(&model, width, height);
        assert!(
            // Directed (TUI5 item 1): the empty composer's appended ▮ became a styled
            // CELL over a space — "❯  " (sigil + cursor cell) is the signature now.
            rows.iter().any(|row| row.contains("❯  ")),
            "the composer's cursor row survives {width}×{height}"
        );
        // Nothing interactive may be clickable while unpainted.
        // The invisible-but-active law is an IMPLICATION: nothing may be
        // clickable that was not painted. (The converse can fail honestly —
        // a chip clipped by a narrow frame is dropped by the seam filter.)
        let bar_painted = rows.iter().any(|row| row.contains("◉ AURA"));
        for (label, hit) in [
            ("engine", Hit::AuraEngine),
            ("mute", Hit::AuraMute),
            ("exit", Hit::AuraExit),
        ] {
            if hits.iter().any(|(_, candidate)| *candidate == hit) {
                assert!(
                    bar_painted,
                    "{label} chip clickable while unpainted at {width}×{height}"
                );
            }
        }
        if hits
            .iter()
            .any(|(_, candidate)| *candidate == Hit::AuraTalkBtn)
        {
            assert!(
                rows.iter()
                    .any(|row| row.contains("hold to talk") || row.contains("listening…")),
                "hold-to-talk clickable while unpainted at {width}×{height}"
            );
        }
    }
    // The bar survives at 90×5 (the size the review caught painting none).
    let (rows, _) = draw(&model, 90, 5);
    assert!(rows.iter().any(|row| row.contains("◉ AURA")));
    // At 90×1 only the sacred composer row is left — and NOTHING else is
    // clickable, so no control is invisible-but-active.
    let (rows, hits) = draw(&model, 90, 1);
    // Directed (TUI5 item 1): the empty composer's appended ▮ became a styled
    // CELL over a space — "❯  " (sigil + cursor cell) is the signature now.
    assert!(rows.iter().any(|row| row.contains("❯  ")));
    assert!(
        !hits.iter().any(|(_, hit)| matches!(
            hit,
            Hit::AuraEngine | Hit::AuraMute | Hit::AuraExit | Hit::AuraTalkBtn
        )),
        "every aura control shed with its chrome at 90×1"
    );
}

// ---- P2-7: the chip question card answers by click AND hover ----

#[tokio::test(start_paused = true)]
async fn chip_question_rows_answer_on_click_and_move_on_hover() {
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "use a subagent for the webhook tests");
    pump_until(&mut driver, &mut rx, &mut model, "chip card", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests")
            .is_some_and(|chip| chip.question_menu().is_some())
    })
    .await;
    model.handle_hit(Hit::ChipRow("t1-tests".to_owned()));
    assert_eq!(model.screen, Screen::Subagent);
    let (_, hits) = draw(&model, 118, 34);
    let option_hits: Vec<Hit> = hits
        .iter()
        .filter(|(_, hit)| matches!(hit, Hit::MenuOption { .. }))
        .map(|(_, hit)| hit.clone())
        .collect();
    assert_eq!(option_hits.len(), 2, "both option rows are clickable");
    // Hover moves the selection (sim `onMouseEnter` on `.imo`).
    model.handle_hover(Some(option_hits[1].clone()));
    assert_eq!(model.menu_selection, 1, "hover moved the cursor");
    // Click answers the CHIP's card, not the (absent) session card.
    model.handle_hit(option_hits[1].clone());
    assert_eq!(model.outbox.len(), 1, "the click produced an answer");
    assert_eq!(model.outbox[0].option_index, 1);
    pump_until(&mut driver, &mut rx, &mut model, "resolved", |m| {
        haider_tui::app::find_chip(&m.chips, "t1-tests").is_some_and(|chip| {
            chip.transcript.entries().iter().any(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::User { text, .. } if text == "mocks — fast, less coverage"
                )
            })
        })
    })
    .await;
    let chip = haider_tui::app::find_chip(&model.chips, "t1-tests").expect("chip");
    assert!(
        chip.question.as_ref().is_some_and(|q| q.resolved),
        "the clicked card resolved"
    );
    assert!(chip.question_menu().is_none(), "and the card closed");
}

// ---- P2-10: the voice tag ends with the turn, not with a tail beat ----

#[tokio::test(start_paused = true)]
async fn the_voice_tag_clears_even_when_the_branch_parks_on_a_menu() {
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "first turn", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    // A VOICE turn whose branch parks on the permission card.
    submit(&mut model, "/say deploy to prod");
    pump_until(&mut driver, &mut rx, &mut model, "permission card", |m| {
        m.projection.open_menu().is_some()
    })
    .await;
    assert!(model.projection.voice_live(), "still speaking while parked");
    let menu = model.projection.open_menu().expect("card").id.clone();
    answer_menu(&mut model, menu.as_str(), 2);
    pump_until(&mut driver, &mut rx, &mut model, "turn end", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    assert!(
        !model.projection.voice_live(),
        "the terminal run state closed the tag, tail beat or not"
    );
    // A later ordinary turn is NOT spoken.
    submit(&mut model, "hello again");
    pump_until(&mut driver, &mut rx, &mut model, "third turn", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    let last_spoken = model
        .projection
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            TranscriptEntry::Item(block)
                if matches!(
                    block.item,
                    haider_protocol::item::TurnItem::AgentMessage { .. }
                ) =>
            {
                Some(block.spoken)
            }
            _ => None,
        })
        .expect("an agent row");
    assert!(!last_spoken, "ordinary rows are not tagged ♪ speaking");
}

// ---- P2-11: token + routing laws ----

#[tokio::test(start_paused = true)]
async fn token_counts_use_utf16_units_like_the_sim() {
    // JS `String.length` counts UTF-16 code units: a non-BMP emoji is 2, so
    // it costs 18 tokens, not the 9 a Unicode-scalar count would charge.
    // Driven through production dispatch (review r2 P3-9): the FIRST usage
    // frame of the turn is the preamble's user-text accrual.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "\u{1F600}");
    pump_until(&mut driver, &mut rx, &mut model, "first usage", |m| {
        m.projection.context_tokens() > 0
    })
    .await;
    assert_eq!(
        model.projection.context_tokens(),
        18,
        "one emoji = 2 UTF-16 units × 9"
    );
}

#[tokio::test(start_paused = true)]
async fn trailing_word_boundaries_route_like_the_sim() {
    // `/ci\b/` has NO leading boundary, so `pci` IS a test-branch hit — the
    // Rust port used to require both sides and routed it generic. (NB
    // "ascii" is NOT a hit in either engine: it ends `ii`.) Driven through
    // production dispatch (review r2 P3-9).
    async fn opening_line(text: &str) -> String {
        let mut model = launcher_model();
        let (mut driver, mut rx) = driver_for(&model);
        submit(&mut model, text);
        pump_until(&mut driver, &mut rx, &mut model, "opening line", |m| {
            m.projection.entries().iter().any(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::Item(block)
                        if !block.streaming
                            && matches!(
                                &block.item,
                                haider_protocol::item::TurnItem::AgentMessage { text }
                                    if !text.is_empty()
                            )
                )
            })
        })
        .await;
        model
            .projection
            .entries()
            .iter()
            .find_map(|entry| match entry {
                TranscriptEntry::Item(block) if !block.streaming => match &block.item {
                    haider_protocol::item::TurnItem::AgentMessage { text } if !text.is_empty() => {
                        Some(text.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("an opening line")
    }
    let got = opening_line("audit the pci flow").await;
    assert!(
        got.starts_with("Running the suite first"),
        "`pci` ends on a boundary → the test branch, got {got:?}"
    );
    // `/subagents\b/` decides PLURAL only; the branch gate is a bare
    // /subagent/, so the singular text still spins exactly one up.
    assert!(
        opening_line("use a subagent here")
            .await
            .starts_with("Spinning up a subagent — ")
    );
    assert!(
        opening_line("use two subagents here")
            .await
            .starts_with("Spinning up two subagents — ")
    );
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_think_window_burns_no_intro_and_no_callsign() {
    // Review P2-11: the counters advanced while BUILDING beats, so a turn
    // killed inside its 750 ms think window still consumed GENERIC_INTROS[0]
    // and a roster callsign. The sim picks the branch after the window.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "thinking", |m| {
        m.projection.badge() == "● THINKING"
    })
    .await;
    model.handle(key(KeyCode::Esc));
    drain(&mut driver, &mut model);
    assert!(model.projection.interrupted());
    // A brand-new session: its generic turn must still be intro #0.
    submit(&mut model, "/clear");
    drain(&mut driver, &mut model);
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "turn done", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    let first = model
        .projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Item(block) => match &block.item {
                haider_protocol::item::TurnItem::AgentMessage { text } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("an intro");
    assert_eq!(
        first,
        haider_tui::script::GENERIC_INTROS[0],
        "the killed turn burned no rotation index"
    );
}

// ---- P2-13: repeated compaction rows ----

#[tokio::test(start_paused = true)]
async fn two_manual_compactions_both_land_their_rows() {
    // Review P2-13: `compact-{before}` repeated when the meter had not
    // moved, and the projection dropped the second row.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "turn done", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    for _ in 0..2 {
        submit(&mut model, "/compact");
        pump_until(&mut driver, &mut rx, &mut model, "compacted", |m| {
            !m.turn_active && m.projection.badge() == "IDLE"
        })
        .await;
    }
    let rows = model
        .projection
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                TranscriptEntry::Item(block)
                    if matches!(
                        block.item,
                        haider_protocol::item::TurnItem::ContextCompaction { .. }
                    )
            )
        })
        .count();
    assert_eq!(rows, 2, "both compactions rendered");
}

// ---- P2-8 / P2-9: launcher truth ----

#[test]
fn launcher_liveness_and_metas_follow_the_sim_seeds() {
    let model = launcher_model();
    let (rows, hits) = draw(&model, 130, 40);
    let row_with = |needle: &str| {
        rows.iter()
            .find(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row with {needle:?}"))
            .clone()
    };
    // The L1 seed owns the running web-index chip (tui.js:556-572).
    let l1 = row_with("l1-remote-projects");
    assert!(l1.contains("◉"), "the live session wears the gold dot");
    assert!(l1.contains("1 live subagent ·"), "sim `.live` copy");
    assert!(
        row_with("cellular-pool-fix").contains("●"),
        "no fabricated liveness elsewhere"
    );
    assert!(row_with("recent sessions").contains("· 1 running"));
    // TUI4 item 5 caps the column at 70 cells, so these verbatim metas now
    // ELLIPSIZE into it exactly as the sim's `.meta` does (text-overflow:
    // ellipsis inside `min(560px, 92%)`). The visible prefix is still the
    // sim's text character for character.
    assert!(row_with("◉ Aura").contains("voice session · orchestrator — spawns & steers"));
    assert!(row_with("⚿ Accounts").contains("provider credentials — OAuth & API keys, har"));
    assert!(row_with("⇄ Peers").contains("remote placement — not supported · Haider runs local"));
    // P2-9: hits carry identity, never a mutable ordinal.
    let l1 = common::session_named(&model, "l1-remote-projects");
    assert!(hits.iter().any(|(_, hit)| matches!(
        hit,
        Hit::AttachSession(id) if *id == l1
    )));
    assert!(
        hits.iter()
            .any(|(_, hit)| *hit == Hit::ExtraRow(LauncherRow::Peers))
    );
}

#[test]
fn an_attach_hit_follows_its_session_when_the_list_reorders() {
    // The value-carrying law: a one-frame-stale rect must attach exactly the
    // row that was clicked, or nothing.
    // TUI4c (directed): rows live in the SESSION MAP now — same law,
    // exercised against `model.sessions`.
    let mut model = launcher_model();
    model.sessions.reverse();
    common::hit_session_named(&mut model, "billing-service");
    assert_eq!(
        model.session_name.as_deref(),
        Some("billing-service"),
        "identity, not ordinal"
    );
    let mut model = launcher_model();
    // The hit was built from the frame that HAD the row; the row is gone by
    // the time the click resolves.
    let vanished = common::session_named(&model, "billing-service");
    model.sessions.clear();
    model.handle_hit(Hit::AttachSession(vanished));
    assert_eq!(model.session_name, None, "a vanished row attaches nothing");
    assert_eq!(tree_live_count(&model.chips), 0);
}

// ---- Review r2 P1-1: control-tag answers must prove their origin ----

#[tokio::test(start_paused = true)]
async fn a_stale_card_answer_cannot_reconfigure_a_replacement_session() {
    // Repro from the review: answer an old /voice card, then leave for a
    // replacement session BEFORE the answer is consumed. The answer rides
    // the never-cancelled control tag, so it still arrives — and must be
    // dropped on identity, not applied to whoever is showing now.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "turn done", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    submit(&mut model, "/voice");
    let card = model.projection.open_menu().expect("voice card").id.clone();
    assert!(
        card.as_str().starts_with("voice-card-"),
        "cards mint a per-open id, got {card}"
    );
    // Answer option 1 (Deepgram · ElevenLabs) but do NOT let the driver
    // consume it yet: hand it to the channel, then replace the session.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    let pending = model.outbox.remove(0);
    assert_eq!(pending.origin, model.ui_generation());
    let pending_origin = pending.origin;
    driver
        .sender()
        .try_send((
            driver.control_tag(),
            DemoEvent::Answer {
                origin: pending.origin,
                answer: pending.answer,
            },
        ))
        .expect("queued");
    // Leave for the launcher — the card is still open (its MenuClosed rides
    // the answer we are holding), so navigation goes through the chrome,
    // exactly as a user would — then start a REPLACEMENT session.
    model.handle_hit(Hit::BackChip);
    assert_eq!(model.screen, Screen::Launcher);
    submit(&mut model, "a different task entirely");
    drain(&mut driver, &mut model);
    assert_ne!(
        model.ui_generation(),
        pending_origin,
        "a new surface generation"
    );
    let epoch = model.ui_generation();
    let voice_before = model.voice.clone();
    pump_quiet(&mut driver, &mut rx, &mut model, 6_000).await;
    assert_eq!(model.ui_generation(), epoch);
    assert_eq!(
        model.voice, voice_before,
        "the stale answer must not reconfigure the replacement session"
    );
    let notes: Vec<String> = model
        .projection
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Note { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !notes.iter().any(|text| text.starts_with("· voice enabled")),
        "and lands no consequence note in it, got {notes:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_card_answered_in_its_own_session_still_applies() {
    // The identity gate must not break the ordinary path.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "hello world");
    pump_until(&mut driver, &mut rx, &mut model, "turn done", |m| {
        !m.turn_active && m.projection.badge() == "IDLE"
    })
    .await;
    submit(&mut model, "/voice");
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    pump_until(&mut driver, &mut rx, &mut model, "voice applied", |m| {
        m.voice.stt == "deepgram-nova-3"
    })
    .await;
    assert_eq!(model.voice.tts, "elevenlabs");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text.starts_with("· voice enabled · deepgram-nova-3 → elevenlabs")
    )));
}

#[test]
fn each_card_open_mints_a_fresh_id() {
    // Fixed ids let an answer to card N drive card N+1's consequences.
    let mut model = launcher_model();
    submit(&mut model, "hello world");
    model.turn_active = false;
    let mut ids = Vec::new();
    for _ in 0..2 {
        submit(&mut model, "/voice");
        ids.push(
            model
                .projection
                .open_menu()
                .expect("voice card")
                .id
                .to_string(),
        );
        model.handle(key(KeyCode::Esc));
    }
    assert_ne!(ids[0], ids[1], "per-open ids");
    for _ in 0..2 {
        submit(&mut model, "/tools");
        ids.push(
            model
                .projection
                .open_menu()
                .expect("tools card")
                .id
                .to_string(),
        );
        model.handle(key(KeyCode::Esc));
    }
    assert_ne!(ids[2], ids[3]);
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        4,
        "no id repeats across opens"
    );
}

// ---- Review r2 P1-2: the session card dies with its surface ----

#[tokio::test(start_paused = true)]
async fn a_menu_option_hit_is_inert_once_its_surface_is_gone() {
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "this is unreliable");
    pump_until(&mut driver, &mut rx, &mut model, "recovery card", |m| {
        m.projection.open_menu().is_some()
    })
    .await;
    let (_, hits) = draw(&model, 118, 34);
    let option = hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::MenuOption { .. }))
        .map(|(_, hit)| hit.clone())
        .expect("an option row");
    // Back to the launcher: TUI4c (directed) — the projection (and its
    // card) check into the session's SLOT; the live surface is neutral,
    // so a queued click must not answer it.
    model.handle_hit(Hit::BackChip);
    assert_eq!(model.screen, Screen::Launcher);
    let sid = model.last_detached.clone().expect("detached id");
    let slot_menu = |m: &haider_tui::app::AppModel| {
        m.sessions
            .iter()
            .find(|entry| entry.id == sid)
            .and_then(|entry| entry.projection.open_menu().cloned())
    };
    assert!(slot_menu(&model).is_some(), "card still parked in the slot");
    model.handle_hit(option.clone());
    assert!(model.outbox.is_empty(), "no answer from an invisible card");
    model.handle_hover(Some(option.clone()));
    assert_eq!(model.menu_selection, 0, "hover is inert too");
    let entries = model
        .sessions
        .iter()
        .find(|entry| entry.id == sid)
        .map(|entry| entry.projection.entries().len())
        .expect("slot");
    pump_quiet(&mut driver, &mut rx, &mut model, 2_000).await;
    assert_eq!(
        model
            .sessions
            .iter()
            .find(|entry| entry.id == sid)
            .map(|entry| entry.projection.entries().len())
            .expect("slot"),
        entries,
        "no parked continuation started"
    );
    // Return to the session and the very same rect works again.
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session);
    model.handle_hit(option);
    assert_eq!(model.outbox.len(), 1, "answerable on its own surface");
}

// ---- Review r2 P2-6: the auto-title micro-call outlives an interrupt ----

#[tokio::test(start_paused = true)]
async fn the_auto_title_micro_call_lands_after_an_interrupt() {
    // The sim's 1.5 s timeout is a bare setTimeout: interrupting the turn
    // does not cancel it (tui.js:1219-1227).
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "please fix the flaky boundary test suite");
    pump_until(&mut driver, &mut rx, &mut model, "thinking", |m| {
        m.projection.badge() == "● THINKING"
    })
    .await;
    model.handle(key(KeyCode::Esc));
    drain(&mut driver, &mut model);
    assert!(model.projection.interrupted());
    assert_eq!(model.session_title, None, "not titled yet");
    pump_until(&mut driver, &mut rx, &mut model, "titled", |m| {
        m.session_title.is_some()
    })
    .await;
    assert_eq!(
        model.session_title.as_deref(),
        Some("Please fix the flaky boundary test suite")
    );
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· session titled — “Please fix the flaky boundary test suite” (background micro-call · never enters the prompt)"
    )));
}

#[tokio::test(start_paused = true)]
async fn the_auto_title_micro_call_never_names_a_replacement_session() {
    // …but a session REPLACEMENT does make it irrelevant: the sim's
    // callback looks its session up by id and finds it gone.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    submit(&mut model, "please fix the flaky boundary test suite");
    pump_until(&mut driver, &mut rx, &mut model, "thinking", |m| {
        m.projection.badge() == "● THINKING"
    })
    .await;
    submit(&mut model, "/clear");
    drain(&mut driver, &mut model);
    submit(&mut model, "cd web");
    pump_quiet(&mut driver, &mut rx, &mut model, 4_000).await;
    assert_eq!(model.session_title, None, "the replacement stays untitled");
    assert!(
        !model.projection.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Note { text } if text.starts_with("· session titled")
        )),
        "and takes no note from the dead session's micro-call"
    );
}

// ---- Review r2 P2-3/P2-4: mic + help owning surfaces ----

#[test]
fn the_launcher_mic_renders_but_does_nothing() {
    // Sim `speak` returns unless a session is attached (tui.js:2045).
    let mut model = launcher_model();
    let (rows, hits) = draw(&model, 118, 34);
    assert!(
        rows.iter().any(|row| row.contains("[ ◉ talk ]")),
        "the mic still RENDERS on the launcher"
    );
    assert!(hits.iter().any(|(_, hit)| *hit == Hit::TalkChip));
    model.handle_hit(Hit::TalkChip);
    assert!(!model.listening, "but pressing it starts nothing");
    assert!(model.requests.is_empty());
}

#[test]
fn stale_mic_and_help_hits_respect_their_surfaces() {
    let mut model = launcher_model();
    submit(&mut model, "/aura");
    model.handle_hit(Hit::TalkChip);
    assert!(
        !model.listening,
        "a stale mic rect must not create invisible listening state on aura"
    );
    assert!(model.requests.is_empty());
    // A stale launcher help hit must not open Help over a session.
    let mut model = launcher_model();
    submit(&mut model, "hello world");
    assert_eq!(model.screen, Screen::Session);
    model.handle_hit(Hit::HelpHint);
    assert!(!model.help_open, "help belongs to the launcher");
}
