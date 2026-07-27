//! Dev tool: dump each screen as plain text at a fixed size for visual
//! review without a live terminal. `cargo run -p haider-tui --example
//! dump_screens`.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, AuraAgentRow, ChipModel, ChipQuestion, Hit, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::script::{ChipDisplayState, ChipPrefill, ChipSeed};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn dump_at(model: &AppModel, label: &str, width: u16, height: u16) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    println!("──── {label} ────");
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    println!();
}

fn dump(model: &AppModel, label: &str) {
    dump_at(model, label, 118, 34);
}

fn main() {
    let mut model = AppModel::new();
    let script = demo_script();
    // Boot mid-checks.
    for payload in script.iter().take(3) {
        model.handle(AppEvent::Envelope(Box::new(payload.clone())));
    }
    dump(&model, "boot");
    // Launcher.
    for payload in script.iter().skip(3).take(3) {
        model.handle(AppEvent::Envelope(Box::new(payload.clone())));
    }
    dump(&model, "launcher");
    // Palette open.
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('/'),
        KeyModifiers::NONE,
    )));
    dump(&model, "launcher + palette");
    for _ in 0..2 {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
    }
    // Blocking menu (a separate model: the demo turn up to, not including,
    // its self-answer — the main model must not see events twice).
    let mut menu_model = AppModel::new();
    for payload in &script {
        if matches!(payload, haider_protocol::EventPayload::MenuAnswered(_)) {
            break;
        }
        menu_model.handle(AppEvent::Envelope(Box::new(payload.clone())));
    }
    dump(&menu_model, "session + blocking menu");
    // Sacred options at short heights (review r3 P2-1b): hint + body shed,
    // options never.
    dump_at(&menu_model, "session + blocking menu @ 90×10", 90, 10);
    // Chrome yields to the blocking card below 90×7 (review r5 P2-1):
    // status row + session line shed, both options intact.
    dump_at(&menu_model, "session + blocking menu @ 90×5", 90, 5);
    // The menu-close transition (review r6 P2-1): the composer inherits
    // the ladder — the answered card gives way to an EDITABLE composer.
    let mut answered_model = menu_model;
    answered_model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::MenuAnswered(haider_protocol::menu::MenuAnswer {
            menu: haider_protocol::ids::MenuId::new("t0-menu-1"),
            option_key: Some("allow".to_owned()),
            option_index: 0,
            value: None,
            via: haider_protocol::menu::AnswerVia::Tui,
        }),
    )));
    dump_at(&answered_model, "session @ 90×5 · menu answered", 90, 5);
    // Full session.
    for payload in script.iter().skip(6) {
        model.handle(AppEvent::Envelope(Box::new(payload.clone())));
    }
    dump(&model, "session (end of demo)");
    // Session palette (session-only commands included) — the ghost
    // completion trails the cursor.
    for c in "/t".chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    dump(&model, "session + palette");
    // /theme argument slot (G12 slice).
    for c in "heme ".chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    dump(&model, "session + theme args");
    // Multi-line composer (⇧⏎/⌥⏎ newlines, review r2 P2-4).
    for _ in 0..7 {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
    }
    for (index, line) in [
        "draft the migration plan",
        "then apply it to staging",
        "and verify",
    ]
    .iter()
    .enumerate()
    {
        if index > 0 {
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::ALT,
            )));
        }
        for c in line.chars() {
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
    }
    dump(&model, "session + multi-line composer");

    // ---- TUI3b turn-engine frames ----
    let ready =
        haider_protocol::EventPayload::HarnessStatus(haider_protocol::state::HarnessStatus::Ready);
    let submit = |model: &mut AppModel, text: &str| {
        for c in text.chars() {
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    };

    // Todos pinned mid-chain: the plan-todo branch's beats applied up to
    // the second completed work tool.
    let mut todos_model = AppModel::new();
    todos_model.handle(AppEvent::Envelope(Box::new(ready.clone())));
    submit(&mut todos_model, "plan todo the harness work");
    todos_model.requests.clear();
    let (generic, roster) = (
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(3),
    );
    let beats = haider_tui::script::respond_beats(
        "plan todo the harness work",
        false,
        haider_protocol::DeliveryMode::Steer,
        1,
        &generic,
        &roster,
    );
    let mut tools_done = 0;
    for beat in &beats {
        if let haider_tui::script::Beat::Emit(payload) = beat {
            todos_model.handle(AppEvent::Envelope(Box::new(payload.clone())));
            if matches!(
                payload,
                haider_protocol::EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                    item: haider_protocol::item::TurnItem::ToolCall { .. },
                    ..
                })
            ) {
                tools_done += 1;
                if tools_done == 2 {
                    break;
                }
            }
        }
    }
    dump(&todos_model, "session + todos pinned (dep chain)");

    // The ⧗ queue panel between the todos and the composer.
    todos_model.queue_mode = true;
    todos_model
        .msg_queue
        .push("and then re-run the whole suite".to_owned());
    todos_model
        .msg_queue
        .push("finally draft the release notes".to_owned());
    dump(&todos_model, "session + ⧗ queue panel (q:turn)");
    // The ledger at 90×10: todos shed first, the panel holds if it fits.
    todos_model.msg_queue.truncate(1);
    dump_at(&todos_model, "session + ⧗ queue @ 90×10", 90, 10);

    // Shell rows, a voice turn and the compaction numbers row.
    let mut engine_model = AppModel::new();
    engine_model.handle(AppEvent::Envelope(Box::new(ready.clone())));
    submit(&mut engine_model, "walk the harness with me");
    engine_model.requests.clear();
    engine_model.turn_active = false;
    submit(&mut engine_model, "cd web");
    submit(&mut engine_model, "ls");
    engine_model
        .projection
        .push_user_voice("walk me through the harness entrypoints".to_owned());
    engine_model
        .projection
        .push_note("◉ heard · whisper-large-v3".to_owned());
    engine_model.projection.set_voice_live(true);
    engine_model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("spoken-1"),
            item: haider_protocol::item::TurnItem::AgentMessage {
                text: "Starting at the run loop — the harness owns every state write.".to_owned(),
            },
        }),
    )));
    engine_model.projection.set_voice_live(false);
    engine_model.projection.push_note(
        "· context at 85% — compacting (dead branches first, live path last)".to_owned(),
    );
    engine_model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("compact-demo"),
            item: haider_protocol::item::TurnItem::ContextCompaction {
                summary_artifact: haider_protocol::ids::ArtifactRef::new("blake3:demo"),
                tokens_before: Some(170_000),
                tokens_after: Some(12_000),
            },
        }),
    )));
    engine_model.turn_active = false;
    dump(&engine_model, "session — shell · voice · ⊟ compaction rows");

    // The /voice and /tools command cards (◉/⚒ glyphs via origin).
    submit(&mut engine_model, "/voice");
    dump(&engine_model, "session + /voice card");
    engine_model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    submit(&mut engine_model, "/tools");
    dump(&engine_model, "session + /tools card");

    // The launcher .shellout block under the recent list.
    let mut shell_launcher = AppModel::new();
    shell_launcher.handle(AppEvent::Envelope(Box::new(ready.clone())));
    submit(&mut shell_launcher, "ls");
    dump(&shell_launcher, "launcher + shellout");

    // ---- TUI3b commit 2: subagent chips (§2) + aura mode (§3) ----
    let mut sub_model = AppModel::new();
    sub_model.handle(AppEvent::Envelope(Box::new(ready)));
    submit(&mut sub_model, "use two subagents to split this work");
    sub_model.requests.clear();
    // Replay the parent turn's own envelopes so the transcript above the
    // panel is the real §1.1 branch (the chips below are hand-seeded).
    let (sub_generic, sub_roster) = (
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(3),
    );
    for beat in &haider_tui::script::respond_beats(
        "use two subagents to split this work",
        false,
        haider_protocol::DeliveryMode::Steer,
        1,
        &sub_generic,
        &sub_roster,
    ) {
        if let haider_tui::script::Beat::Emit(payload) = beat {
            sub_model.handle(AppEvent::Envelope(Box::new(payload.clone())));
        }
    }
    sub_model.turn_active = false;
    sub_model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::RunState(haider_protocol::state::RunState::Done),
    )));
    // A three-node tree: a chip WAITING on its nested child, a failed chip
    // holding the ⌁ recovery card, and a closed chip on its way out.
    let mut tests = chip(
        "t1-tests",
        "Hasan",
        "(a)",
        "Imam Hasan al-Mujtaba",
        "tests",
        "gpt-5.6",
        "local",
        ChipDisplayState::Tool,
        4800,
        &[
            ChipPrefill::Agent(
                "Picking up the lease — scoping the billing test surface before writing anything."
                    .to_owned(),
            ),
            ChipPrefill::ToolOk {
                name: "fs_patch".to_owned(),
                desc: "cloud/tests/billing/webhooks.rs".to_owned(),
                meta: "+96 −4".to_owned(),
            },
        ],
    );
    tests.children.push(chip(
        "t1-tests-sub",
        "Husayn",
        "(a)",
        "Imam Husayn",
        "subtask",
        "gpt-5.6",
        "local",
        ChipDisplayState::Running,
        400,
        &[ChipPrefill::Note(
            "· spawned by Hasan — nested delegation".to_owned(),
        )],
    ));
    let mut docs = chip(
        "t1-docs",
        "Salman",
        "(r)",
        "Salman al-Farsi",
        "docs",
        "gemini-3",
        "local",
        ChipDisplayState::Error,
        3900,
        &[
            ChipPrefill::Agent(
                "Drafting API docs for the new webhook endpoint from the patched source."
                    .to_owned(),
            ),
            ChipPrefill::ToolOk {
                name: "fs_patch".to_owned(),
                desc: "docs/api/billing-webhooks.md".to_owned(),
                meta: "+140 −0".to_owned(),
            },
        ],
    );
    let recovery_text =
        "cargo doc failed (exit 101 — the docs feature flag is missing). How should I recover?";
    let recovery_options = [
        "retry with --features docs",
        "close this subagent — keep the patch",
    ];
    docs.question = Some(ChipQuestion {
        recovery: true,
        text: recovery_text.to_owned(),
        options: recovery_options.iter().map(|o| (*o).to_owned()).collect(),
        resolved: false,
    });
    docs.transcript
        .apply(&haider_protocol::EventPayload::MenuOpened(
            haider_protocol::menu::Menu {
                id: haider_protocol::ids::MenuId::new("t1-docs-q"),
                kind: haider_protocol::menu::MenuKind::Recovery {
                    effect: haider_protocol::ids::EffectId::new("e-t1-docs"),
                },
                title: recovery_text.to_owned(),
                body: vec![],
                options: recovery_options
                    .iter()
                    .enumerate()
                    .map(|(index, label)| haider_protocol::menu::MenuOption {
                        key: format!("o{index}"),
                        label: (*label).to_owned(),
                        detail: None,
                        decision: None,
                    })
                    .collect(),
                blocking: false,
                scope: haider_protocol::menu::MenuScope::Subagent {
                    agent: haider_protocol::ids::AgentId::new("t1-docs"),
                },
                origin: "subagent".to_owned(),
                ttl_ms: None,
                timeout_option: None,
            },
        ));
    let mut lint = chip(
        "t1-lint",
        "Miqdad",
        "(r)",
        "Miqdad ibn al-Aswad",
        "lint",
        "fable-5",
        "hetzner-1",
        ChipDisplayState::Done,
        1500,
        &[],
    );
    lint.closed = true;
    lint.removing = true;
    sub_model.chips = vec![tests, docs, lint];
    dump(&sub_model, "session + SubTree panel (live chips)");

    // The subagent view: breadcrumb head, the chip's own transcript, and the
    // question card replacing ITS composer (the parent is never blocked).
    sub_model.screen = Screen::Subagent;
    sub_model.view_path = vec!["t1-docs".to_owned()];
    dump(&sub_model, "subagent view + ⌁ recovery card");
    // A chip WAITING on its nested child: ◔ badge, waiting tail line, and
    // the nested row's ` │  ` indent in the shared map.
    sub_model.view_path = vec!["t1-tests".to_owned()];
    dump(&sub_model, "subagent view — ◔ waiting on a nested child");

    // The aura stage: orb block, both columns, the spoken transcript.
    let mut aura_model = AppModel::new();
    aura_model.screen = Screen::Aura;
    aura_model.aura.roster.push(AuraAgentRow {
        name: "auth".to_owned(),
        device: "hetzner-1".to_owned(),
        state: ChipDisplayState::Done,
        activity: "tests green".to_owned(),
    });
    for line in [
        "agent_spawn — auth → hetzner-1 · lease ok",
        "agent_control — auth: run tests",
        "auth on hetzner-1: tests green ✓",
    ] {
        aura_model.aura.log.push(line.to_owned());
    }
    aura_model
        .aura
        .transcript
        .push_user_voice("spin up the auth service on hetzner-1 and run its tests".to_owned());
    aura_model.aura.transcript.set_voice_live(true);
    aura_model
        .aura
        .transcript
        .apply(&haider_protocol::EventPayload::Item(
            haider_protocol::item::ItemEvent::Completed {
                item_id: haider_protocol::ids::ItemId::new("aura-r1-done"),
                item: haider_protocol::item::TurnItem::AgentMessage {
                    text: "Done — auth is live on hetzner-1 and its tests are green. Open it, or spin up another?".to_owned(),
                },
            },
        ));
    aura_model.aura.transcript.set_voice_live(false);
    dump(&aura_model, "aura stage (◉ AURA · orb · columns)");
    aura_model.handle_hit(Hit::AuraEngine);
    aura_model.handle_hit(Hit::AuraMute);
    dump(&aura_model, "aura stage — engine swapped · audio muted");
    // TUI3.1 P1-6: the aura stage joins the sacred-row ladder — chrome
    // sheds in order and the composer's cursor row survives every size.
    dump_at(&aura_model, "aura @ 90×10 (columns shed)", 90, 10);
    dump_at(&aura_model, "aura @ 90×5 (orb shed, bar intact)", 90, 5);
    dump_at(&aura_model, "aura @ 90×1 (cursor row only)", 90, 1);
}

/// Build one chip straight from a seed (the driver's `ChipAdd` path). The
/// argument list mirrors `ChipSeed`'s display fields one-for-one — a dump
/// helper, not an API.
#[allow(clippy::too_many_arguments)]
fn chip(
    agent: &str,
    callsign: &str,
    hon: &'static str,
    full: &str,
    name: &str,
    model: &str,
    device: &str,
    state: ChipDisplayState,
    tokens: u64,
    prefill: &[ChipPrefill],
) -> ChipModel {
    ChipModel::from_seed(ChipSeed {
        agent: agent.to_owned(),
        parent: None,
        callsign: callsign.to_owned(),
        hon,
        full: full.to_owned(),
        name: name.to_owned(),
        model: model.to_owned(),
        device: device.to_owned(),
        state,
        tokens,
        prefill: prefill.to_vec(),
    })
}
