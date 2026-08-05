//! F2d — terminal markdown for assistant text: deterministic, LINE-STABLE
//! styling on theme slots.
//!
//! The laws, in the order the brief pins them:
//! * LINE STABILITY — rendering never changes line counts vs the plain
//!   text: one rendered line per source line, and the styled wrap breaks
//!   rows exactly where plain wrapping of the RENDERED text would.
//! * BYTE-CONTENT PRESERVATION — rendered plain text equals the source
//!   minus only the marker characters actually consumed by matched pairs.
//! * span kinds — bold/italic/code/fence/heading/bullet/numbered each map
//!   to their kind (and from there to theme slots, never raw colors).
//! * honest degradation — unterminated spans render literally; nested
//!   emphasis is best-effort but never drops characters.
//! * streaming re-entrancy — every prefix renders without panic, line
//!   counts stay pinned to the prefix's own line count, and a span
//!   restyles in place when its closing marker arrives.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, ItemId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_tui::app::{AppModel, RuntimeMode};
use haider_tui::md::{MdKind, MdLine, MdSpan, render_markdown, wrap_spans};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;

mod common;
use common::launcher_model;

/// The corpus every geometry law sweeps: paragraphs, all inline kinds,
/// fences (terminated and streaming-unterminated), headings, bullets,
/// numbered lists, unicode, and pathological marker soup.
const CORPUS: &[&str] = &[
    "plain paragraph with no markers at all",
    "the answer is **97** exactly",
    "*lead* emphasis and _tail_ emphasis",
    "mix **bold** with *italic* and `code` on one line",
    "``` \nfn main() {\n    println!(\"**not bold**\");\n}\n```",
    "```rust\nlet x = 1;\n```",
    "# Heading one\n## Heading two\nbody after headings",
    "- first bullet\n- second **bold** bullet\n  - nested bullet",
    "* star bullet with `code`",
    "1. first\n2. second\n12) twelfth",
    "unterminated **bold runs off",
    "unterminated `code runs off",
    "lone * star and 2 * 3 * 4 math",
    "snake_case_name stays_literal",
    "***both*** married markers",
    "nested **outer *inner* outer**",
    "```\nunclosed fence tail",
    "عنوان **عريض** بالعربية",
    "",
    "\n\n",
    "a\n\nb",
];

fn plain_of(lines: &[MdLine]) -> String {
    lines
        .iter()
        .map(MdLine::plain)
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_span(text: &str) -> Vec<MdSpan> {
    vec![MdSpan {
        text: text.to_owned(),
        kind: MdKind::Text,
    }]
}

/// MUTATION CHECK (F2d): make fence delimiters consume their line (skip
/// the push in `render_markdown`). Expected runtime failure: the fenced
/// corpus entries render fewer lines than their sources and the equality
/// below names the sample.
#[test]
fn line_stability_one_rendered_line_per_source_line() {
    for sample in CORPUS {
        let rendered = render_markdown(sample);
        assert_eq!(
            rendered.len(),
            sample.split('\n').count(),
            "line count must equal the source's for {sample:?}"
        );
    }
}

/// The styled wrap breaks rows exactly where plain wrapping of the
/// RENDERED text breaks them — styling adds or removes ZERO rows. Swept
/// across the corpus at every interesting budget.
///
/// MUTATION CHECK (F2d): wrap the SOURCE line (markers included) instead
/// of the rendered spans. Expected runtime failure: `**`-heavy samples
/// produce extra rows at narrow budgets and the count equality names the
/// sample and width.
#[test]
fn styled_wrap_never_moves_a_break() {
    for sample in CORPUS {
        for budget in [1usize, 4, 9, 20, 38, 80] {
            for line in render_markdown(sample) {
                let styled = wrap_spans(&line.spans, budget);
                let plain = wrap_spans(&text_span(&line.plain()), budget);
                assert_eq!(
                    styled.len(),
                    plain.len(),
                    "styled wrap changed row counts for {:?} at width {budget}",
                    line.plain()
                );
                // Row-by-row, the visible characters are identical too.
                for (styled_row, plain_row) in styled.iter().zip(&plain) {
                    let styled_text: String =
                        styled_row.iter().map(|span| span.text.as_str()).collect();
                    let plain_text: String =
                        plain_row.iter().map(|span| span.text.as_str()).collect();
                    assert_eq!(styled_text, plain_text);
                }
            }
        }
    }
}

/// The styled wrap against a HAND-COMPUTED oracle — independent literals,
/// not the wrapper's own output (the self-consistency law above cannot
/// see a global budget shift; this one can).
///
/// MUTATION CHECK (F2d): shrink the walker's budget by one. Expected
/// runtime failure: every literal row below breaks one cell early.
#[test]
fn styled_wrap_matches_the_hand_computed_oracle() {
    let rows = |spans: &[MdSpan], budget: usize| -> Vec<String> {
        wrap_spans(spans, budget)
            .iter()
            .map(|row| row.iter().map(|span| span.text.as_str()).collect())
            .collect()
    };
    // Pre-wrap: the break lands at the space-run boundary; the space that
    // crosses the edge fills to it and carries over.
    assert_eq!(
        rows(&text_span("the answer is 97 exactly"), 17),
        vec!["the answer is 97 ".to_owned(), "exactly".to_owned()]
    );
    assert_eq!(
        rows(&text_span("aaa bbb ccc"), 7),
        vec!["aaa bbb".to_owned(), " ccc".to_owned()]
    );
    // An overlong unbreakable run hard-splits at the cell boundary.
    assert_eq!(
        rows(&text_span("abcdefghij"), 4),
        vec!["abcd".to_owned(), "efgh".to_owned(), "ij".to_owned()]
    );
    // Styled spans break at the SAME columns: bold 97 changes nothing.
    let styled = render_markdown("the answer is **97** exactly");
    assert_eq!(
        rows(&styled[0].spans, 17),
        vec!["the answer is 97 ".to_owned(), "exactly".to_owned()]
    );
}

/// Byte-content preservation: rendered plain text is the source minus
/// ONLY marker characters (`*`, `_`, `` ` ``) consumed by matched pairs —
/// an in-order subsequence with nothing else missing and nothing added.
///
/// MUTATION CHECK (F2d): make the bold arm drop its content's first
/// character. Expected runtime failure: the subsequence walk exhausts the
/// source before matching every rendered char and the law names the
/// sample.
#[test]
fn byte_content_preservation_only_matched_markers_are_consumed() {
    for sample in CORPUS {
        let rendered = plain_of(&render_markdown(sample));
        // Walk the source; every rendered char must appear in order, and
        // every skipped source char must be a marker character.
        let mut source = sample.chars().peekable();
        for ch in rendered.chars() {
            loop {
                match source.next() {
                    Some(s) if s == ch => break,
                    Some(s) => assert!(
                        matches!(s, '*' | '_' | '`'),
                        "non-marker {s:?} went missing from {sample:?}"
                    ),
                    None => panic!("rendered char {ch:?} not found in source {sample:?}"),
                }
            }
        }
        for leftover in source {
            assert!(
                matches!(leftover, '*' | '_' | '`'),
                "non-marker {leftover:?} went missing from the tail of {sample:?}"
            );
        }
    }
}

/// Marker-free text renders byte-identical — the renderer must never
/// invent or eat characters when there is nothing to style.
#[test]
fn marker_free_text_renders_byte_identical() {
    for sample in [
        "no markers here at all",
        "punctuation: dashes - and (parens) and #hash mid-line",
        "tabs\tand  double  spaces   survive",
    ] {
        assert_eq!(plain_of(&render_markdown(sample)), sample);
    }
}

/// `**97**` renders as a bold span with the markers consumed — the
/// owner's literal acceptance case.
#[test]
fn double_star_pair_emboldens_and_consumes_markers() {
    let lines = render_markdown("the answer is **97** exactly");
    assert_eq!(
        lines[0].spans,
        vec![
            MdSpan {
                text: "the answer is ".to_owned(),
                kind: MdKind::Text
            },
            MdSpan {
                text: "97".to_owned(),
                kind: MdKind::Bold
            },
            MdSpan {
                text: " exactly".to_owned(),
                kind: MdKind::Text
            },
        ]
    );
}

/// `*em*` and `_em_` italicize; math stars and snake_case stay literal.
#[test]
fn single_marker_emphasis_is_word_shaped() {
    let lines = render_markdown("*lead* and _mid_ emphasis");
    assert_eq!(lines[0].spans[0].kind, MdKind::Italic);
    assert_eq!(lines[0].spans[0].text, "lead");
    let kinds: Vec<MdKind> = lines[0].spans.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&MdKind::Italic));

    // `2 * 3 * 4` — spaced stars are arithmetic, not emphasis.
    let math = render_markdown("2 * 3 * 4");
    assert_eq!(math[0].plain(), "2 * 3 * 4");
    assert!(math[0].spans.iter().all(|s| s.kind == MdKind::Text));

    // snake_case survives the underscore rule (word-boundary law).
    let snake = render_markdown("snake_case_name stays");
    assert_eq!(snake[0].plain(), "snake_case_name stays");
    assert!(snake[0].spans.iter().all(|s| s.kind == MdKind::Text));
}

/// `` `code` `` becomes a Code span (backticks consumed); an unmatched
/// backtick stays literal.
#[test]
fn inline_code_pill_and_literal_stray_backtick() {
    let lines = render_markdown("run `cargo test` now");
    assert_eq!(
        lines[0].spans[1],
        MdSpan {
            text: "cargo test".to_owned(),
            kind: MdKind::Code
        }
    );
    let stray = render_markdown("a ` alone");
    assert_eq!(stray[0].plain(), "a ` alone");
    assert!(stray[0].spans.iter().all(|s| s.kind == MdKind::Text));
}

/// Fences: delimiter lines are preserved VERBATIM (language line
/// included) and restyled; interior lines carry the block kind with no
/// inline parsing; the block never adds or removes a line.
#[test]
fn fenced_code_restyles_without_touching_line_geometry() {
    let sample = "before\n```rust\nlet x = \"**not bold**\";\n```\nafter";
    let lines = render_markdown(sample);
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[1].plain(), "```rust");
    assert_eq!(lines[1].spans[0].kind, MdKind::Fence);
    assert_eq!(lines[2].plain(), "let x = \"**not bold**\";");
    assert_eq!(lines[2].spans[0].kind, MdKind::CodeBlock);
    assert_eq!(lines[3].plain(), "```");
    assert_eq!(lines[3].spans[0].kind, MdKind::Fence);
    assert_eq!(lines[4].plain(), "after");
}

/// Headings keep their `#` marks (restyled, never consumed) and carry the
/// emphasis hierarchy on the text.
#[test]
fn headings_keep_marks_and_style_the_text() {
    let lines = render_markdown("# Top\n### Deep heading\n#not-a-heading");
    assert_eq!(
        lines[0].spans,
        vec![
            MdSpan {
                text: "# ".to_owned(),
                kind: MdKind::HeadingMark
            },
            MdSpan {
                text: "Top".to_owned(),
                kind: MdKind::Heading
            },
        ]
    );
    assert_eq!(lines[1].spans[0].text, "### ");
    assert_eq!(lines[1].spans[0].kind, MdKind::HeadingMark);
    // No space after the hash run → not a heading, byte-identical.
    assert_eq!(lines[2].plain(), "#not-a-heading");
    assert!(lines[2].spans.iter().all(|s| s.kind == MdKind::Text));
}

/// Bullets and numbered lists: the marker (indentation included) is a
/// ListMark span, alignment exactly as authored, content still parsed.
#[test]
fn bullets_and_numbers_mark_aligned() {
    let lines = render_markdown("- top\n  - nested **b**\n1. one\n12) twelve\n*not a bullet*");
    assert_eq!(lines[0].spans[0].text, "- ");
    assert_eq!(lines[0].spans[0].kind, MdKind::ListMark);
    assert_eq!(lines[1].spans[0].text, "  - ");
    assert_eq!(lines[1].spans[0].kind, MdKind::ListMark);
    assert_eq!(lines[1].spans[2].kind, MdKind::Bold);
    assert_eq!(lines[2].spans[0].text, "1. ");
    assert_eq!(lines[2].spans[0].kind, MdKind::ListMark);
    assert_eq!(lines[3].spans[0].text, "12) ");
    assert_eq!(lines[3].spans[0].kind, MdKind::ListMark);
    // `*emphasis*` at a line start is emphasis, never a bullet (the
    // bullet demands its space).
    assert_eq!(lines[4].spans[0].kind, MdKind::Italic);
}

/// Unterminated spans render literally — the streaming case — and the
/// SAME line restyles once the closing marker arrives (re-entrancy).
#[test]
fn unterminated_spans_render_literally_then_restyle_in_place() {
    let open = render_markdown("counting **9");
    assert_eq!(open[0].plain(), "counting **9");
    assert!(open[0].spans.iter().all(|s| s.kind == MdKind::Text));

    let closed = render_markdown("counting **97**");
    assert_eq!(closed.len(), open.len(), "closing a span moves no lines");
    assert_eq!(
        closed[0].spans[1],
        MdSpan {
            text: "97".to_owned(),
            kind: MdKind::Bold
        }
    );
}

/// Every char-boundary prefix of every corpus sample renders without
/// panic and keeps the prefix's own line count — the streaming law.
#[test]
fn every_streaming_prefix_renders_line_stable() {
    for sample in CORPUS {
        let mut prefix = String::new();
        for ch in sample.chars() {
            prefix.push(ch);
            let rendered = render_markdown(&prefix);
            assert_eq!(
                rendered.len(),
                prefix.split('\n').count(),
                "prefix {prefix:?} broke line stability"
            );
        }
    }
}

/// Nested emphasis is best-effort — `**a *b* c**` marries to BoldItalic —
/// and never drops characters.
#[test]
fn nested_emphasis_marries_kinds_and_keeps_every_char() {
    let lines = render_markdown("**a *b* c**");
    assert_eq!(lines[0].plain(), "a b c");
    let kinds: Vec<(String, MdKind)> = lines[0]
        .spans
        .iter()
        .map(|s| (s.text.clone(), s.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("a ".to_owned(), MdKind::Bold),
            ("b".to_owned(), MdKind::BoldItalic),
            (" c".to_owned(), MdKind::Bold),
        ]
    );
    let triple = render_markdown("***both***");
    assert_eq!(triple[0].plain(), "both");
    assert_eq!(triple[0].spans[0].kind, MdKind::BoldItalic);
}

/// Code wins inside emphasis: `**a `x` b**` keeps the pill.
#[test]
fn code_keeps_its_pill_inside_emphasis() {
    let lines = render_markdown("**a `x` b**");
    assert_eq!(lines[0].plain(), "a x b");
    assert_eq!(lines[0].spans[1].kind, MdKind::Code);
    assert_eq!(lines[0].spans[1].text, "x");
}

// ---- render integration: the session transcript wears the styling ----

fn sid() -> SessionId {
    SessionId::new("f2-md-session")
}

fn raw(seq: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-f2md-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("f2-device"),
        authority_epoch: 1,
        worker_generation: 1,
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

fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.requests.clear();
    model
}

fn agent_message(seq_base: u64, model: &mut AppModel, text: &str, complete: bool) {
    let item = ItemId::new(format!("item-{seq_base}"));
    model.route_raw(&raw(
        seq_base,
        &EventPayload::Item(ItemEvent::Started {
            item_id: item.clone(),
            item: TurnItem::AgentMessage {
                text: String::new(),
            },
        }),
    ));
    model.route_raw(&raw(
        seq_base + 1,
        &EventPayload::Item(ItemEvent::Delta {
            item_id: item.clone(),
            delta: ItemDelta::Text {
                text: text.to_owned(),
            },
        }),
    ));
    if complete {
        model.route_raw(&raw(
            seq_base + 2,
            &EventPayload::Item(ItemEvent::Completed {
                item_id: item,
                item: TurnItem::AgentMessage {
                    text: text.to_owned(),
                },
            }),
        ));
    }
}

fn draw_cells(model: &AppModel, width: u16, height: u16) -> Vec<Vec<(String, Modifier)>> {
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
                .map(|x| {
                    let cell = &buffer[(x, y)];
                    (cell.symbol().to_owned(), cell.style().add_modifier)
                })
                .collect()
        })
        .collect()
}

/// The CELL column of `needle` on a rendered row — every glyph on these
/// rows is width-1, so the char index is the buffer column (a byte
/// `find` would mis-aim past the multi-byte `▏` rail).
fn char_col(row: &str, needle: &str) -> usize {
    let chars: Vec<char> = row.chars().collect();
    let target: Vec<char> = needle.chars().collect();
    (0..chars.len())
        .find(|&i| chars[i..].starts_with(&target[..]))
        .unwrap_or_else(|| panic!("{needle:?} not on row {row:?}"))
}

/// MUTATION CHECK (F2d): route the AgentMessage arm back through the
/// plain single-span path. Expected runtime failure: the literal `**97**`
/// reappears on screen and the bold cell assertion finds no BOLD
/// modifier.
#[test]
fn transcript_renders_bold_without_the_markers() {
    let mut model = live_session();
    agent_message(10, &mut model, "the answer is **97** exactly", true);
    let cells = draw_cells(&model, 80, 24);
    let rows: Vec<String> = cells
        .iter()
        .map(|row| row.iter().map(|(sym, _)| sym.as_str()).collect())
        .collect();
    let row_index = rows
        .iter()
        .position(|row| row.contains("the answer is 97 exactly"))
        .expect("the stripped markdown row renders");
    assert!(
        !rows.iter().any(|row| row.contains("**")),
        "markers must be consumed, not painted"
    );
    // The `97` cells carry BOLD; their neighbors do not.
    let row = &cells[row_index];
    let col = char_col(&rows[row_index], "97");
    assert!(
        row[col].1.contains(Modifier::BOLD) && row[col + 1].1.contains(Modifier::BOLD),
        "**97** must render bold"
    );
    assert!(
        !row[col - 2].1.contains(Modifier::BOLD),
        "plain neighbors stay unbolded"
    );
}

/// A streaming message mid-`**` renders the literal markers and the gold
/// cursor; the next delta that closes the pair restyles the same line.
#[test]
fn streaming_tail_restyles_when_the_span_closes() {
    let mut model = live_session();
    agent_message(10, &mut model, "counting **9", false);
    let cells = draw_cells(&model, 80, 24);
    let rows: Vec<String> = cells
        .iter()
        .map(|row| row.iter().map(|(sym, _)| sym.as_str()).collect())
        .collect();
    assert!(
        rows.iter().any(|row| row.contains("counting **9▮")),
        "the open span renders literally with the cursor riding it"
    );

    let item = ItemId::new("item-10");
    model.route_raw(&raw(
        12,
        &EventPayload::Item(ItemEvent::Delta {
            item_id: item,
            delta: ItemDelta::Text {
                text: "7** done".to_owned(),
            },
        }),
    ));
    let cells = draw_cells(&model, 80, 24);
    let rows: Vec<String> = cells
        .iter()
        .map(|row| row.iter().map(|(sym, _)| sym.as_str()).collect())
        .collect();
    let row_index = rows
        .iter()
        .position(|row| row.contains("counting 97 done"))
        .expect("closed span restyles in place");
    let col = char_col(&rows[row_index], "97");
    assert!(
        cells[row_index][col].1.contains(Modifier::BOLD),
        "the closed span is bold"
    );
}
