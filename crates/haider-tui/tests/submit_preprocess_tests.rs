//! TUI3b §4 submit() preprocessing: shell builtins + VFS, slug session
//! names, /say voice turns, /queue steer-vs-queue with the ⧗ panel in the
//! sacred-height ledger, the /voice + /tools command cards, and the
//! status-bar segments they drive.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_tui::app::{
    AppEvent, AppModel, AppRequest, Screen, resolve_path, run_shell, slug_name, vfs_seed,
};
use haider_tui::projection::TranscriptEntry;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, submit};

fn session_model() -> AppModel {
    let mut model = launcher_model();
    submit(&mut model, "hello world");
    // The demo turn request is queued for a driver; for pure-model tests
    // the session is simply live with no script attached.
    model.requests.clear();
    model.turn_active = false;
    model
}

fn draw(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// Echo one outbox answer back as the runtime does (menu consequences
/// apply on the echoed envelope).
fn echo_answer(model: &mut AppModel) {
    let answer = model.outbox.remove(0).answer;
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuAnswered(
        answer,
    ))));
}

// ---- Slug session names (sim tui.js:2014-2016) ----

#[test]
fn slug_name_takes_three_words_lowercased_and_bounded() {
    assert_eq!(slug_name("Fix the Parser bug now"), "fix-the-parser");
    assert_eq!(slug_name("hello"), "hello");
    assert_eq!(slug_name("héllo wörld"), "hllo-wrld", "[a-z0-9-] only");
    // Sim-exact: the join hyphens survive the [^a-z0-9-] strip, so
    // punctuation-only input keeps them; only an EMPTY slug falls back.
    assert_eq!(slug_name("!!! ??? ..."), "--");
    assert_eq!(slug_name(""), "session", "fallback");
    assert_eq!(slug_name("¡¡¡"), "session", "fallback after strip");
    assert_eq!(
        slug_name("supercalifragilisticexpialidocious extremely long"),
        "supercalifragilisticexpialid",
        "max 28 chars"
    );
}

// ---- Shell builtins + VFS (sim tui.js:418-462, 1993-2008) ----

#[test]
fn resolve_path_handles_home_dot_dotdot_with_a_floor() {
    assert_eq!(resolve_path("", "~/dev/diffforge"), "~/dev");
    assert_eq!(resolve_path("~", "~/dev"), "~");
    assert_eq!(resolve_path("~/dev/web", "~"), "~/dev/web");
    assert_eq!(
        resolve_path("cloud", "~/dev/diffforge"),
        "~/dev/diffforge/cloud"
    );
    assert_eq!(resolve_path("..", "~/dev/diffforge"), "~/dev");
    assert_eq!(resolve_path("../..", "~/dev/diffforge"), "~");
    assert_eq!(resolve_path("../../..", "~/dev"), "~", "floor: one segment");
    assert_eq!(
        resolve_path("./cloud/./src", "~/dev/diffforge"),
        "~/dev/diffforge/cloud/src"
    );
}

#[test]
fn run_shell_matches_the_sim_outputs() {
    let mut vfs = vfs_seed();
    let (out, _) = run_shell("ls", "~/dev", &mut vfs);
    assert_eq!(out, "diffforge/  enterprise-suite/  haider-code/  notes.md");
    let (out, _) = run_shell("pwd", "~/dev/diffforge", &mut vfs);
    assert_eq!(out, "~/dev/diffforge");
    let (out, dir) = run_shell("cd cloud", "~/dev/diffforge", &mut vfs);
    assert_eq!(out, "→ ~/dev/diffforge/cloud");
    assert_eq!(dir.as_deref(), Some("~/dev/diffforge/cloud"));
    // Unknown dirs list the defaults.
    let (out, _) = run_shell("ls", "~/somewhere/else", &mut vfs);
    assert_eq!(out, "src/  README.md");
    // mkdir/touch: usage, create, already-exists.
    let (out, _) = run_shell("mkdir", "~/dev", &mut vfs);
    assert_eq!(out, "usage: mkdir <name>");
    let (out, _) = run_shell("mkdir pbx2", "~/dev", &mut vfs);
    assert_eq!(out, "created pbx2/");
    let (out, _) = run_shell("mkdir pbx2", "~/dev", &mut vfs);
    assert_eq!(out, "pbx2/ already exists");
    let (out, _) = run_shell("touch notes.md", "~/dev", &mut vfs);
    assert_eq!(out, "notes.md already exists");
    let (out, _) = run_shell("touch todo.md", "~/dev", &mut vfs);
    assert_eq!(out, "created todo.md");
}

#[test]
fn launcher_shell_builtins_never_start_a_session_and_fill_the_shellout() {
    let mut model = launcher_model();
    submit(&mut model, "ls");
    assert_eq!(model.screen, Screen::Launcher, "no session started");
    assert!(model.requests.is_empty(), "no turn request");
    assert_eq!(
        model.launcher_shellout,
        Some((
            "ls".to_owned(),
            "services/  web/  infra/  README.md".to_owned()
        ))
    );
    let rows = draw(&model, 118, 34);
    assert!(rows.iter().any(|row| row.contains("$ ls")));
    assert!(
        rows.iter()
            .any(|row| row.contains("services/  web/  infra/  README.md"))
    );
    // cd retargets the LAUNCHER dir (shown in the dirline).
    submit(&mut model, "cd ..");
    assert_eq!(model.launcher_dir, "~/dev");
    let rows = draw(&model, 118, 34);
    assert!(rows.iter().any(|row| row.contains("dir ~/dev · mesh off")));
}

#[test]
fn session_shell_builtins_render_transcript_rows_and_retarget_the_header_dir() {
    let mut model = session_model();
    submit(&mut model, "pwd");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Shell { cmd, out }
            if cmd == "pwd" && out == "~/dev/enterprise-suite"
    )));
    assert!(!model.turn_active, "shell rows never start a model turn");
    submit(&mut model, "cd web");
    assert_eq!(model.session_dir, "~/dev/enterprise-suite/web");
    let rows = draw(&model, 118, 34);
    assert!(
        rows.iter()
            .any(|row| row.contains("· ~/dev/enterprise-suite/web")),
        "the session header shows the retargeted dir"
    );
    assert!(rows.iter().any(|row| row.contains("$ cd web")));
    assert!(
        rows.iter()
            .any(|row| row.contains("→ ~/dev/enterprise-suite/web"))
    );
}

// ---- /say voice turns (sim tui.js:1865-1875) ----

#[test]
fn say_guards_then_submits_a_voice_turn() {
    let mut model = session_model();
    // Busy: the sim-honest note promises a queue that never happens.
    model.turn_active = true;
    submit(&mut model, "/say hello there");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· busy — voice turn queues once idle"
    )));
    model.turn_active = false;
    // Empty words.
    submit(&mut model, "/say");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· /say <words> — what should I hear?"
    )));
    // Valid: ◉ row + heard note + a voice-tagged request.
    submit(&mut model, "/say walk the tree");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::User { text, voice: true, .. } if text == "walk the tree"
    )));
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "◉ heard · whisper-large-v3"
    )));
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::SubmitText { voice: true, text, .. } if text == "walk the tree"
    )));
    // Voice off → enable-first note.
    let mut model = session_model();
    model.voice.enabled = false;
    submit(&mut model, "/say hi");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· enable voice first with /voice"
    )));
    assert!(model.requests.is_empty());
}

// ---- /queue + the ⧗ panel (sim tui.js:1810-1817, 2891-2906) ----

#[test]
fn queue_command_switches_modes_with_the_sim_notes() {
    let mut model = session_model();
    submit(&mut model, "/queue");
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· mid-turn input mode is steer (safe boundary) — /queue steer|subturn|turn"
    )));
    submit(&mut model, "/queue subturn");
    assert!(!model.queue_mode);
    assert!(model.subturn_mode);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· mid-turn input → SUBTURN — held for the next tool call, then injected before execution"
    )));
    submit(&mut model, "/queue turn");
    assert!(model.queue_mode);
    assert!(!model.subturn_mode);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· mid-turn input → QUEUE — held until the turn ends, then consumed without idling"
    )));
    submit(&mut model, "/queue steer");
    assert!(!model.queue_mode);
    assert!(!model.subturn_mode);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· mid-turn input → STEER — delivered at the next safe boundary"
    )));
}

#[test]
fn queue_panel_renders_between_todos_and_composer_with_the_verbatim_header() {
    let mut model = session_model();
    model.turn_active = true;
    model.queue_mode = true;
    submit(&mut model, "first queued message");
    submit(&mut model, "second queued message");
    assert_eq!(model.msg_queue.len(), 2);
    let rows = draw(&model, 118, 34);
    let panel_row = rows
        .iter()
        .position(|row| {
            row.contains("⧗ queued — 2 messages · consumed at turn end, no idle between")
        })
        .expect("panel header");
    assert!(rows[panel_row + 1].contains("1. first queued message"));
    assert!(rows[panel_row + 2].contains("2. second queued message"));
    let composer_row = rows
        .iter()
        // Directed (TUI5 item 1): the empty composer's appended ▮ became a styled
        // CELL over a space — "❯  " (sigil + cursor cell) is the signature now.
        .position(|row| row.contains("❯  "))
        .expect("composer");
    assert!(panel_row < composer_row, "panel sits above the composer");
    // Singular header.
    model.msg_queue.pop();
    let rows = draw(&model, 118, 34);
    assert!(rows.iter().any(|row| {
        row.contains("⧗ queued — 1 message · consumed at turn end, no idle between")
    }));
    // Overlong rows truncate at 72 chars + …
    model.msg_queue[0] = "x".repeat(80);
    let rows = draw(&model, 118, 34);
    let long_row = rows
        .iter()
        .find(|row| row.contains("1. x"))
        .expect("long row");
    assert!(long_row.contains(&format!("{}…", "x".repeat(72))));
    assert!(!long_row.contains(&"x".repeat(73)));
}

#[test]
fn queue_panel_joins_the_sacred_ledger_at_90x10() {
    // Ledger at 90×11 (status 1 + header 2 + rule 1 + transcript 1 +
    // input rule 1 + composer 1 + gap 1 + the RESERVED closing rule): the
    // leftover budget is 2 rows — a 1-message panel (2 rows) fits; a
    // 2-message panel (3 rows) SHEDS whole, before the composer or the
    // transcript's sacred row yields.
    //
    // Directed (TUI6.1 fix 2, review r1 finding 2): this pin sat at
    // 90×10 when the ⧗ panel could outbid the band's closing rule. The
    // reservation law ranks the rule ahead of EVERY optional panel — the
    // queue included — so at 90×10 the rule takes the first budget row
    // and the 2-row panel no longer fits; the fits-case moves up exactly
    // one row. The panel's own law (sheds WHOLE, never the composer) is
    // unchanged and re-asserted at both heights.
    let mut model = session_model();
    model.turn_active = true;
    model.queue_mode = true;
    submit(&mut model, "only queued line");
    let rows = draw(&model, 90, 11);
    assert!(
        rows.iter().any(|row| row.contains("⧗ queued — 1 message")),
        "a 1-message panel fits the 90×11 budget (post-rule reserve)"
    );
    assert!(
        // Directed (TUI5 item 1): the empty composer's appended ▮ became a styled
        // CELL over a space — "❯  " (sigil + cursor cell) is the signature now.
        rows.iter().any(|row| row.contains("❯  ")),
        "composer intact"
    );
    let rows_ten = draw(&model, 90, 10);
    assert!(
        !rows_ten.iter().any(|row| row.contains("⧗ queued")),
        "at 90×10 the RESERVED closing rule outbids the panel (TUI6.1)"
    );
    assert!(
        rows_ten.iter().any(|row| row.contains("❯  ")),
        "composer intact at 90×10 too"
    );
    submit(&mut model, "second line");
    let rows = draw(&model, 90, 11);
    assert!(
        !rows.iter().any(|row| row.contains("⧗ queued")),
        "over budget the panel sheds WHOLE — never the composer"
    );
    assert!(
        // Directed (TUI5 item 1): the empty composer's appended ▮ became a styled
        // CELL over a space — "❯  " (sigil + cursor cell) is the signature now.
        rows.iter().any(|row| row.contains("❯  ")),
        "composer intact"
    );
    // With a pinned plan competing, todos yield FIRST (shed order:
    // todos → ⧗ queue → palette → …, coordinator ledger rule).
    model.msg_queue.truncate(1);
    model.projection.apply(&EventPayload::Item(
        haider_protocol::item::ItemEvent::Started {
            item_id: haider_protocol::ids::ItemId::new("plan-1"),
            item: haider_protocol::item::TurnItem::Plan {
                items: vec![haider_protocol::history::TodoItem {
                    id: 0,
                    text: "only todo".to_owned(),
                    state: haider_protocol::history::TodoState::Processing,
                    dep: None,
                }],
            },
        },
    ));
    // (90×11 for the same one-row shift: the reserved closing rule holds
    // the first budget row at 90×10 — TUI6.1 fix 2.)
    let rows = draw(&model, 90, 11);
    assert!(
        rows.iter().any(|row| row.contains("⧗ queued — 1 message")),
        "the queue panel outranks the todos"
    );
    assert!(
        !rows.iter().any(|row| row.contains("▾ todos")),
        "todos shed before the queue panel"
    );
}

#[test]
fn interrupt_drops_the_held_queue() {
    let mut model = session_model();
    model.turn_active = true;
    model.queue_mode = true;
    submit(&mut model, "queued while running");
    assert_eq!(model.msg_queue.len(), 1);
    model.handle(key(KeyCode::Esc));
    assert!(
        model.msg_queue.is_empty(),
        "sim tui.js:1557: interrupt clears"
    );
    assert!(
        model
            .requests
            .contains(&AppRequest::Interrupt { branch: None })
    );
}

// ---- /voice + /tools cards (sim tui.js:1824-1906) ----

#[test]
fn voice_card_is_verbatim_and_answers_apply() {
    let mut model = session_model();
    submit(&mut model, "/voice");
    let menu = model.projection.open_menu().expect("card open").clone();
    assert_eq!(menu.title, "voice — enable duplex speech for this session");
    assert_eq!(
        menu.body,
        vec![
            "input    STT provider transcribes mic → a normal user turn",
            "output   TTS provider speaks each assistant turn",
            "duplex   gpt-realtime handles both natively (barge-in, no round-trip)",
            "privacy  audio streams to the chosen provider only — never to the mesh",
        ]
    );
    let labels: Vec<&str> = menu.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "enable — Whisper STT · OpenAI TTS",
            "enable — Deepgram STT · ElevenLabs TTS",
            "enable — gpt-realtime (native duplex STT+TTS)",
            "disable voice",
        ],
        "voice ships ON → the last option disables"
    );
    assert!(!menu.blocking, "command cards are non-blocking");
    // The card replaces the composer; its glyph is the origin's ◉.
    let rows = draw(&model, 118, 34);
    assert!(
        rows.iter()
            .any(|row| row.contains("◉ voice — enable duplex speech for this session"))
    );
    // Answer 2 → Deepgram/ElevenLabs, with the sim's note.
    model.handle(key(KeyCode::Char('2')));
    echo_answer(&mut model);
    assert!(model.projection.open_menu().is_none(), "card closed");
    assert_eq!(model.voice.stt, "deepgram-nova-3");
    assert_eq!(model.voice.tts, "elevenlabs");
    assert!(!model.voice.duplex);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· voice enabled · deepgram-nova-3 → elevenlabs · hold-to-talk under the input, or /say <words>"
    )));
    // Status bar follows: first words of stt→tts.
    let rows = draw(&model, 118, 34);
    assert!(
        rows.iter()
            .any(|row| row.contains("[ ◉ voice · deepgram→elevenlabs ]"))
    );
    // Duplex swaps the whole segment for the engine name.
    submit(&mut model, "/voice");
    model.handle(key(KeyCode::Char('3')));
    echo_answer(&mut model);
    assert!(model.voice.duplex);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· voice enabled · gpt-realtime native duplex · hold-to-talk under the input, or /say <words>"
    )));
    let rows = draw(&model, 118, 34);
    assert!(
        rows.iter()
            .any(|row| row.contains("[ ◉ voice · gpt-realtime ]"))
    );
    // Disable, then keep-off.
    submit(&mut model, "/voice");
    model.handle(key(KeyCode::Char('4')));
    echo_answer(&mut model);
    assert!(!model.voice.enabled);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· voice disabled"
    )));
    let rows = draw(&model, 118, 34);
    assert!(
        !rows.iter().any(|row| row.contains("◉ voice ·")),
        "the voice chip hides while voice is off"
    );
    submit(&mut model, "/voice");
    let menu = model.projection.open_menu().expect("card open").clone();
    assert_eq!(menu.options[3].label, "keep voice off");
    model.handle(key(KeyCode::Char('4')));
    echo_answer(&mut model);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· voice stays off"
    )));
}

/// Narrow-terminal pin (retires OPTIMIZATIONS.md D3-10): the persistent voice
/// route chip moved from the status bar to the header top-right
/// (`render_header_voice_chip`). It draws into a rect sized exactly to the chip
/// (so a mid-chip clip is structurally impossible) and — the real fix — DROPS
/// the chip whole when the header's product line leaves no room, instead of
/// overwriting that line the way the old status-bar chip clipped at 90 cols.
///
/// The wide leg proves the chip renders whole when it fits (non-vacuous). The
/// sub-threshold leg proves the drop: at 100 cols this fixture's header (a
/// breadcrumb + logo + `haider vX · <path>`) fills the row, so the chip is
/// ABSENT rather than painted over the product line.
///
/// MUTATION CHECK: drop the `<= header_area.width` guard in
/// `render_header_voice_chip` → at 100 cols the chip renders right-aligned over
/// the product line, so "absent at 100" fails here. (Verified red-then-green.)
#[test]
fn voice_chip_drops_whole_when_the_header_is_too_narrow() {
    let mut model = session_model();
    // Enable voice (answer 2 → deepgram → elevenlabs), same setup as the card test.
    submit(&mut model, "/voice");
    model.handle(key(KeyCode::Char('2')));
    echo_answer(&mut model);
    assert!(
        model.voice.enabled,
        "voice is on, so the header chip is eligible to render"
    );

    // Fits on a wide terminal → the whole chip renders (non-vacuous).
    assert!(
        draw(&model, 120, 12)
            .iter()
            .any(|row| row.contains("[ ◉ voice") && row.contains(" ]")),
        "at 120 cols the whole voice chip renders"
    );

    // The header's product line fills a 100-col row, so the chip must DROP whole
    // rather than clip/overwrite it (the D3-10 fix): absent, never a fragment.
    assert!(
        !draw(&model, 100, 12)
            .iter()
            .any(|row| row.contains("◉ voice")),
        "at 100 cols the voice chip drops whole rather than overwriting the header (D3-10)"
    );
}

#[test]
fn tools_card_is_verbatim_and_registers_with_dispatch_notes() {
    let mut model = session_model();
    submit(&mut model, "/tools");
    let menu = model.projection.open_menu().expect("card open").clone();
    assert_eq!(menu.title, "tools — core surface + custom tools");
    assert_eq!(
        menu.body[0],
        "core     fs_read fs_edit process_exec agent_spawn request_input … (13, always on)"
    );
    assert_eq!(
        menu.body[1],
        "custom   notify_slack (fire-and-forget) · preview_deploy (await) · preview_smoke (deferred)"
    );
    let rows = draw(&model, 118, 34);
    assert!(
        rows.iter()
            .any(|row| row.contains("⚒ tools — core surface + custom tools")),
        "the ⚒ glyph rides the origin tag"
    );
    model.handle(key(KeyCode::Char('1')));
    echo_answer(&mut model);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text }
            if text == "· custom tool registered · dispatch = fire-and-forget — the turn continues the instant it dispatches"
    )));
    submit(&mut model, "/tools");
    model.handle(key(KeyCode::Char('4')));
    echo_answer(&mut model);
    assert!(model.projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text == "· tools card closed"
    )));
}

#[test]
fn esc_dismisses_non_blocking_cards_but_never_blocking_ones() {
    let mut model = session_model();
    submit(&mut model, "/tools");
    assert!(model.projection.open_menu().is_some());
    model.handle(key(KeyCode::Esc));
    assert!(
        model.projection.open_menu().is_none(),
        "esc dismisses the non-blocking card"
    );
    assert_eq!(model.screen, Screen::Session, "esc consumed by the card");
    // A blocking card swallows esc (sim menu law).
    model
        .projection
        .apply(&EventPayload::MenuOpened(haider_protocol::menu::Menu {
            id: haider_protocol::ids::MenuId::new("block-1"),
            kind: haider_protocol::menu::MenuKind::Exhausted,
            title: "blocking".to_owned(),
            body: vec![],
            options: vec![haider_protocol::menu::MenuOption {
                key: "one".to_owned(),
                label: "one".to_owned(),
                detail: None,
                decision: None,
            }],
            blocking: true,
            scope: haider_protocol::menu::MenuScope::Session,
            origin: "test".to_owned(),
            ttl_ms: None,
            timeout_option: None,
        }));
    model.handle(key(KeyCode::Esc));
    // OWNER DIRECTIVE (supersedes the sim swallow law): esc on a blocking
    // card INTERRUPTS — the request goes to the daemon and the demo paints
    // the cancellation locally (menu closed, run cancelled, note).
    assert!(
        model.projection.open_menu().is_none(),
        "blocking-card esc interrupts and closes the card"
    );
    assert!(
        model.projection.interrupted(),
        "idle (i) after the interrupt"
    );
}

// ---- Steer + status segments ----

#[test]
fn steer_is_default_and_lands_the_row_with_the_note() {
    let mut model = session_model();
    model.turn_active = true;
    submit(&mut model, "also update the changelog");
    assert!(model.msg_queue.is_empty(), "steer never holds");
    let entries = model.projection.entries();
    assert!(matches!(
        &entries[entries.len() - 2],
        TranscriptEntry::User { text, voice: false, .. } if text == "also update the changelog"
    ));
    assert!(matches!(
        &entries[entries.len() - 1],
        TranscriptEntry::Note { text }
            if text == "· steered — delivered at the next safe boundary of the current turn"
    ));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SubmitText { .. })),
        "steer is display-only — the running script is not altered"
    );
}

#[test]
fn status_bar_shows_q_turn_while_queue_mode_holds() {
    let mut model = session_model();
    let rows = draw(&model, 118, 34);
    assert!(
        !rows.iter().any(|row| row.contains("· q:turn")),
        "steer default: no tag"
    );
    model.queue_mode = true;
    let rows = draw(&model, 118, 34);
    // F2c: the identity block moved to the composer rule; the bar keeps
    // state · tokens · branch, and the queue tag rides the branch.
    assert!(rows.iter().any(|row| row.contains("· main · q:turn")));
    model.queue_mode = false;
    model.subturn_mode = true;
    let rows = draw(&model, 118, 34);
    assert!(rows.iter().any(|row| row.contains("· main · q:subturn")));
}

/// MUTATION CHECK: drop the zero-option ask interception from the
/// composer submit. Expected RUNTIME failure: the typed answer becomes a
/// model TURN instead of the menu answer (owner report: the resistor
/// question was unanswerable).
#[test]
fn a_zero_option_ask_consumes_the_composer_text_as_its_answer() {
    let mut model = session_model();
    model.mode = haider_tui::app::RuntimeMode::Live;
    model
        .projection
        .apply(&EventPayload::MenuOpened(haider_protocol::menu::Menu {
            id: haider_protocol::ids::MenuId::new("ask-1"),
            kind: haider_protocol::menu::MenuKind::Question,
            title: "which board?".to_owned(),
            body: vec![],
            options: vec![],
            blocking: true,
            scope: haider_protocol::menu::MenuScope::Session,
            origin: "request_input".to_owned(),
            ttl_ms: None,
            timeout_option: None,
        }));
    for c in "blinkyboard R1".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    let answer = model.outbox.pop().expect("the ask consumed the text");
    assert_eq!(answer.answer.value.as_deref(), Some("blinkyboard R1"));
    assert_eq!(answer.answer.option_index, 0);
    assert!(answer.answer.option_key.is_none());
    assert!(
        model.requests.is_empty(),
        "an ask answer is never a turn: {:?}",
        model.requests
    );
}
