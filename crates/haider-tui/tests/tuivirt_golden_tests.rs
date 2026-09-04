//! tuivirt golden frames (v0.0.970 behaviour-preservation pins).
//!
//! The transcript viewport is being re-architected — viewport-only layout,
//! estimated row heights corrected on measurement, a bounded render cache
//! (`docs/testing/v0.0.970/tuivirt-analysis.md`). It must not change what
//! the user sees. Every test here draws a representative transcript into a
//! `TestBackend` at three terminal sizes and compares the frame — text AND
//! per-cell style — against `tests/fixtures/tuivirt/*.golden`.
//!
//! Regenerate ONLY deliberately: `UPDATE_TUIVIRT_GOLDENS=1 cargo test -p
//! haider-tui --test tuivirt_golden_tests`, then review the fixture diff.
//! A golden that changes without an owner-approved visual change is a
//! regression in the re-architecture, not a stale fixture.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::ids::{ItemId, MenuId};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
use haider_protocol::state::RunState;
use haider_tui::app::{AppEvent, AppModel, Hit};
use haider_tui::mock::demo_script;
use haider_tui::projection::OUTPUT_TAIL_MAX;

mod tuivirt_common;
use tuivirt_common::{SIZES, apply, check_golden, draw, push_agent, push_user, session_model};

/// Pin one model at every size.
fn pin(name: &str, model: &AppModel) {
    for (width, height) in SIZES {
        let frame = draw(model, width, height);
        check_golden(name, &frame);
    }
}

fn tool_call(id: &str, name: &str, args: serde_json::Value, status: ToolStatus) -> EventPayload {
    EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(id),
        item: TurnItem::ToolCall {
            call_id: format!("call-{id}"),
            name: name.to_owned(),
            args,
            status,
        },
    })
}

fn tool_started(id: &str, name: &str, args: serde_json::Value) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new(id),
        item: TurnItem::ToolCall {
            call_id: format!("call-{id}"),
            name: name.to_owned(),
            args,
            status: ToolStatus::InProgress,
        },
    })
}

fn output_delta(id: &str, bytes: &[u8]) -> EventPayload {
    EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new(id),
        delta: ItemDelta::CommandOutput {
            stream: OutputStream::Stdout,
            chunk_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    })
}

fn question_menu(options: usize) -> Menu {
    Menu {
        id: MenuId::new("tuivirt-question-1"),
        kind: MenuKind::Question,
        title: "Which migration strategy should the agent take?".to_owned(),
        body: vec![
            "The event store has two candidate schemas.".to_owned(),
            "Pick one — the agent is blocked until you answer.".to_owned(),
        ],
        options: (0..options)
            .map(|n| MenuOption {
                key: format!("opt-{n}"),
                label: match n {
                    0 => "Rewrite in place (fast, riskier)".to_owned(),
                    1 => "Shadow table + backfill".to_owned(),
                    _ => format!("Option {n}"),
                },
                detail: None,
                decision: None,
            })
            .collect(),
        blocking: true,
        scope: MenuScope::Session,
        origin: "agent".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

#[test]
fn empty_session_frames() {
    let model = session_model();
    pin("empty_session", &model);
}

#[test]
fn markdown_headings_and_lists_frames() {
    let mut model = session_model();
    push_user(&mut model, "summarise the plan as markdown");
    push_agent(
        &mut model,
        "md-1",
        "# Migration plan\n\nA short intro paragraph with **bold**, *emphasis* and `inline code`.\n\n\
         ## Steps\n\n- first bullet\n- second bullet with a longer clause that should wrap at a \
         narrow width and keep its hanging indent intact\n  - nested bullet\n\n1. numbered one\n\
         2. numbered two\n3. numbered three\n\n### Notes\n\n> a quoted line\n\nTrailing paragraph.",
    );
    pin("markdown_headings_lists", &model);
}

#[test]
fn fenced_code_blocks_frames() {
    let mut model = session_model();
    push_user(&mut model, "show me the fix");
    push_agent(
        &mut model,
        "code-1",
        "Here is the patch:\n\n```rust\nfn boundary(seq: u64) -> bool {\n    // seq 0 is valid — \
         the fixture starts there\n    seq <= MAX_SEQ\n}\n```\n\nand the shell check:\n\n```\n$ cargo \
         test -p haider-store boundary\nrunning 1 test\ntest boundary ... ok\n```\n\nA line that is \
         **not** code follows the fence with `inline` styling preserved.",
    );
    pin("fenced_code_blocks", &model);
}

#[test]
fn tool_call_boxes_collapsed_and_expanded_frames() {
    let mut model = session_model();
    push_user(&mut model, "read the store and run the tests");
    // Collapsed: a completed call is exactly one row (glyph · name · desc).
    apply(
        &mut model,
        tool_call(
            "t-read",
            "fs_read",
            serde_json::json!({"path": "crates/haider-store/src/event_store.rs"}),
            ToolStatus::Completed,
        ),
    );
    // Running: the pulsing glyph (anim_phase pinned at 0).
    apply(
        &mut model,
        tool_started(
            "t-fetch",
            "web_fetch",
            serde_json::json!({"url": "https://example.invalid/very/long/path/that/needs/ellipsizing/at/narrow/widths"}),
        ),
    );
    // Failed: the err glyph.
    apply(
        &mut model,
        tool_call(
            "t-fail",
            "fs_edit",
            serde_json::json!({"path": "src/lib.rs"}),
            ToolStatus::Failed,
        ),
    );
    // Expanded: a process call with its retained output tail.
    apply(
        &mut model,
        tool_started(
            "t-exec",
            "process_exec",
            serde_json::json!({"command": "cargo test -p haider-store"}),
        ),
    );
    apply(
        &mut model,
        output_delta(
            "t-exec",
            b"   Compiling haider-store v0.0.969\n    Finished test profile\n     Running unittests\n",
        ),
    );
    apply(
        &mut model,
        output_delta(
            "t-exec",
            b"test boundary::seq_zero ... ok\ntest result: ok. 1 passed\n",
        ),
    );
    apply(
        &mut model,
        tool_call(
            "t-exec",
            "process_exec",
            serde_json::json!({"command": "cargo test -p haider-store"}),
            ToolStatus::Completed,
        ),
    );
    // Expanded + truncated: output past the bounded tail shows the honesty
    // marker under the last OUTPUT_TAIL_MAX bytes.
    apply(
        &mut model,
        tool_started(
            "t-big",
            "process_exec",
            serde_json::json!({"command": "yes | head -c 20000"}),
        ),
    );
    let mut big = Vec::new();
    let mut n = 0usize;
    while big.len() < OUTPUT_TAIL_MAX + 2048 {
        big.extend_from_slice(format!("output line {n:04} — bounded tail probe\n").as_bytes());
        n += 1;
    }
    apply(&mut model, output_delta("t-big", &big));
    apply(
        &mut model,
        tool_call(
            "t-big",
            "process_exec",
            serde_json::json!({"command": "yes | head -c 20000"}),
            ToolStatus::Completed,
        ),
    );
    // A direct command row and a file change close the block.
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("t-cmd"),
            item: TurnItem::CommandExecution {
                call_id: "call-cmd".to_owned(),
                command: "git status --short".to_owned(),
                status: ToolStatus::Completed,
                exit_code: Some(0),
            },
        }),
    );
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("t-change"),
            item: TurnItem::FileChange {
                path: "crates/haider-store/src/event_store.rs".to_owned(),
                added: 4,
                removed: 1,
            },
        }),
    );
    pin("tool_call_boxes", &model);
}

#[test]
fn wide_tables_frames() {
    let mut model = session_model();
    push_user(&mut model, "compare the options in a table");
    push_agent(
        &mut model,
        "table-1",
        "| Feature | What it does | Why it matters | Effort |\n| --- | --- | --- | --- |\n\
         | Plate calculator | Tap a weight → which plates per side | Beloved quality-of-life touch | Small |\n\
         | Rest timer | Auto-starts between sets | Keeps sessions honest | Medium |\n\n\
         And a six-column ledger:\n\n\
         | id | provider | model | in | out | est. cost |\n| :-- | :-- | :-- | --: | --: | --: |\n\
         | 1 | anthropic | claude-opus-4 | 18,400 | 2,100 | $0.42 |\n\
         | 2 | openai | gpt-5 | 9,912 | 1,204 | $0.11 |\n\
         | 3 | deepseek | deepseek-v4-pro-long-name | 120,000 | 30,000 | $0.09 |",
    );
    pin("wide_tables", &model);
}

#[test]
fn long_wrapped_lines_frames() {
    let mut model = session_model();
    push_user(
        &mut model,
        &format!(
            "here is a very long prompt without any break opportunities {} and then some words",
            "x".repeat(300)
        ),
    );
    let mut paragraph = String::new();
    for n in 0..40 {
        paragraph.push_str(&format!("clause {n} of a single very long logical line, "));
    }
    push_agent(
        &mut model,
        "long-1",
        &format!(
            "{paragraph}end.\n\nhttps://example.invalid/{}/end\n\nshort tail line",
            "segment/".repeat(60)
        ),
    );
    pin("long_wrapped_lines", &model);
}

#[test]
fn extreme_logical_line_is_capped_with_raw_export_expander() {
    let mut model = session_model();
    push_agent(&mut model, "extreme-line", &"x".repeat(1 << 20));
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("extreme line truncated"));
    assert!(
        frame.contains("/export expands"),
        "a pathological logical line exposes the raw-text expander cue"
    );
}

#[test]
fn cjk_emoji_combining_frames() {
    let mut model = session_model();
    push_user(
        &mut model,
        "日本語のプロンプト：このテストは幅二列の文字が正しく折り返されるか確認します 🎉 한국어도 포함합니다",
    );
    push_agent(
        &mut model,
        "uni-1",
        "Mixed-width text: 中文字符 next to ASCII, emoji 👩‍💻🧑🏽‍🚀🇯🇵 and combining marks: e\u{301} a\u{308} \
         n\u{303} o\u{31b}\u{323} — नमस्ते दुनिया — ﷽ — a very long CJK run to force wrapping: \
         これは非常に長い日本語の文章であり、端末の幅を超えて折り返される必要があります。さらに続きます。\
         한국어 문장도 길게 이어져서 줄바꿈이 필요합니다.\n\n- 箇条書き one\n- **太字** two\n\n\
         `코드 span` and a `wide 全角 pill`.",
    );
    pin("cjk_emoji_combining", &model);
}

#[test]
fn megabyte_reply_frames() {
    let mut model = session_model();
    push_user(&mut model, "dump the whole log");
    let mut text = String::with_capacity(1 << 20);
    let mut n = 0usize;
    while text.len() < (1 << 20) {
        text.push_str(&format!(
            "{n:05} the quick brown fox jumps over the lazy dog — lorem ipsum dolor sit amet, \
             consectetur adipiscing elit, sed do eiusmod\n"
        ));
        n += 1;
    }
    assert!(text.len() >= 1 << 20, "the reply is at least 1 MiB");
    push_agent(&mut model, "mega-1", &text);
    // The tail (bottom-anchored) at every size, then the top of history at
    // the bench size: the far end of a huge entry must render exactly.
    pin("megabyte_reply_tail", &model);
    let frame = draw(&model, 118, 36);
    assert!(
        model.scroll_max.get() > 0,
        "a 1 MiB reply overflows the viewport"
    );
    let _ = frame;
    model.scroll_back.set(model.scroll_max.get());
    let top = draw(&model, 118, 36);
    check_golden("megabyte_reply_top", &top);
}

#[test]
fn input_required_menu_frames() {
    let mut model = session_model();
    push_user(&mut model, "migrate the store");
    push_agent(&mut model, "ask-1", "I need a decision before continuing.");
    let menu = question_menu(3);
    let id = menu.id.clone();
    apply(&mut model, EventPayload::MenuOpened(menu));
    apply(
        &mut model,
        EventPayload::RunState(RunState::InputRequired { menu: id }),
    );
    pin("input_required_menu", &model);
}

#[test]
fn input_required_ask_frames() {
    // A zero-option ask keeps the composer as the answer line; the
    // question renders above it.
    let mut model = session_model();
    push_user(&mut model, "migrate the store");
    push_agent(&mut model, "ask-2", "One question first.");
    let menu = question_menu(0);
    let id = menu.id.clone();
    apply(&mut model, EventPayload::MenuOpened(menu));
    apply(
        &mut model,
        EventPayload::RunState(RunState::InputRequired { menu: id }),
    );
    pin("input_required_ask", &model);
}

#[test]
fn demo_session_frames() {
    // The scripted demo turn: plan, streamed text, tool call, answered
    // permission menu, file change, usage — the realistic mix.
    let mut model = AppModel::new();
    model.identity.device = "test-lion-box".to_owned();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    pin("demo_session", &model);
}

#[test]
fn streaming_tail_frames() {
    let mut model = session_model();
    push_user(&mut model, "keep going");
    apply(&mut model, EventPayload::RunState(RunState::Streaming));
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("stream-1"),
            item: TurnItem::AgentMessage {
                text: String::new(),
            },
        }),
    );
    apply(
        &mut model,
        EventPayload::Item(ItemEvent::Delta {
            item_id: ItemId::new("stream-1"),
            delta: ItemDelta::Text {
                text: "Streaming a partial reply with an **unterminated bold span".to_owned(),
            },
        }),
    );
    pin("streaming_tail", &model);
}

#[test]
fn scrolled_history_with_sticky_and_jump_chip_frames() {
    let mut model = session_model();
    for turn in 0..12 {
        push_user(
            &mut model,
            &format!("prompt {turn} — please continue the work"),
        );
        for n in 0..4 {
            push_agent(
                &mut model,
                &format!("hist-{turn}-{n}"),
                &tuivirt_common::agent_row(turn * 4 + n),
            );
        }
    }
    for (width, height) in SIZES {
        // A following frame first (the watermark stamps), then the middle
        // of history: sticky band on top, bare jump chip at the bottom.
        let _ = draw(&model, width, height);
        let max = model.scroll_max.get();
        assert!(max > 0, "history overflows {width}x{height}");
        model.scroll_back.set(max / 2);
        model.sticky_suppressed.set(false);
        let frame = draw(&model, width, height);
        assert!(frame.has_hit(|hit| matches!(hit, Hit::StickyJump(_))));
        assert!(frame.has_hit(|hit| matches!(hit, Hit::JumpToBottom)));
        check_golden("scrolled_history", &frame);
        model.scroll_back.set(0);
    }
}

// ---------------------------------------------------------------------------
// 970 owner bugs — the band's anatomy, pinned at the three sizes.
// ---------------------------------------------------------------------------

/// One running subagent, so the `▾ subagents` panel is on screen and the
/// band's closing anatomy is actually observable.
fn with_subagents(model: &mut AppModel) {
    model.chips = vec![haider_tui::app::ChipModel::from_seed(
        haider_tui::script::ChipSeed {
            agent: "t1-docs".to_owned(),
            parent: None,
            ros: None,
            callsign: "Husayn".to_owned(),
            hon: "(r)",
            full: "Husayn ibn Ali".to_owned(),
            name: "docs".to_owned(),
            model: "fable-5".to_owned(),
            device: "macbook".to_owned(),
            state: haider_tui::script::ChipDisplayState::Running,
            tokens: 100,
            prefill: Vec::new(),
        },
    )];
}

#[test]
fn band_closes_straight_into_the_subagents_panel_frames() {
    // 970 owner bug 1. The band used to reach `▾ subagents` through its
    // closing rule AND a `lead_subtree` breathing blank; the blank is gone,
    // so these frames show `❯ message haider …` / rule / `▾ subagents` on
    // three consecutive rows — the anatomy `render_subagent` always had.
    let mut model = session_model();
    push_user(&mut model, "spin up a docs pass");
    push_agent(&mut model, "band-1", "Delegating to a subagent now.");
    with_subagents(&mut model);
    pin("band_with_subagents", &model);
}

#[test]
fn composer_image_notice_frames() {
    // 970 owner bug 2. A pair that DECLARES no vision refuses a pasted
    // image and says so on its own row inside the band, directly above the
    // draft it deliberately kept.
    let mut model = session_model();
    push_agent(&mut model, "notice-1", "Ready when you are.");
    model.composer.set_text("what is in this screenshot?".to_owned());
    model.composer_notice = Some(haider_tui::app::ImageNotice::NoVision {
        model: "deepseek-v4".to_owned(),
    });
    pin("composer_image_notice", &model);
}
