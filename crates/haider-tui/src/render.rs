//! Screen renderers: pure functions of ([`AppModel`], frame area) → buffer.
//! Testable headlessly via `TestBackend` (research rec 16/18). Every color
//! comes from the theme's style vocabulary — no literals (rec 12).
//! Visual authority: the `/tui` sim — typography, chips, and row shapes are
//! copied from it deliberately.

use crate::app::{AppModel, Hit, Screen};
use crate::boot::{boot_subline, check_rows, launcher_subline};
use crate::commands::{HELP_TEXT, PALETTE_MAX_ROWS};
use crate::format::{METER_CELLS_DEFAULT, fmt_tok, meter_cells};
use crate::plain::status_glyph;
use crate::projection::{ItemBlock, TranscriptEntry};
use crate::sanctum::SanctumLine;
use crate::theme::Theme;
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::item::TurnItem;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

/// Workspace version shown on boot/launcher (single source: the crate).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Render the whole frame for the current screen. Returns the frame's
/// clickable regions (hit map) for the runtime's mouse dispatch.
pub fn render(model: &AppModel, frame: &mut Frame<'_>) -> Vec<(Rect, Hit)> {
    let theme = model.theme.theme();
    let area = frame.area();
    // Ground the whole frame in the theme bg.
    frame.render_widget(Block::default().style(theme.text_style()), area);

    let mut hits: Vec<(Rect, Hit)> = Vec::new();
    let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    match model.screen {
        Screen::Boot => render_boot(model, theme, frame, body),
        Screen::Launcher => render_launcher(model, theme, frame, body, &mut hits),
        Screen::Session => render_session(model, theme, frame, body, &mut hits),
    }
    if model.help_open {
        render_help(theme, frame, body);
        hits.clear();
    }
    render_status_bar(model, theme, frame, status, &mut hits);
    hits
}

/// Sim placeholder copy (`InputBar` textarea, tui.js:3008).
const PLACEHOLDER_LAUNCHER: &str = "start a session — describe the task, or / for commands";
const PLACEHOLDER_SESSION: &str =
    "message haider — ⏎ send · ⇧⏎ newline · / commands · paste images/text";
/// Sim palette hint (`CmdMenu .chint`), pinned at the palette's BOTTOM.
const PALETTE_HINT: &str = "↑↓ options · tab complete · ⏎ run · esc dismiss";
/// Fixed command-name column (sim `.cname` flex-basis), in cells.
const PALETTE_NAME_COL: usize = 14;
/// Left/right composer padding in cells (sim InputBar `padding: … 16px`).
const COMPOSER_PAD: usize = 2;

/// A full-width one-row hit region.
fn row_rect(area: Rect, top: u16, offset: usize) -> Rect {
    Rect {
        x: area.x,
        y: top + u16::try_from(offset).unwrap_or(u16::MAX),
        width: area.width,
        height: 1,
    }
}

/// Char-truncate with a trailing ellipsis (sim `text-overflow: ellipsis`).
fn ellipsize(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if text.chars().count() <= budget {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// An outlined chip: `[ label ]` in the given style (the sim's bordered
/// pills, one-row terminal form).
fn chip<'a>(label: String, style: ratatui::style::Style) -> Vec<Span<'a>> {
    vec![
        Span::styled("[ ", style),
        Span::styled(label, style),
        Span::styled(" ]", style),
    ]
}

/// Center a block of lines; when the block is taller than the area, keep the
/// TAIL visible (the composer lives at the bottom — review r1 P2: short
/// windows must never hide the input). Returns the drawn rect and how many
/// leading lines were dropped (hit maps shift by that amount).
fn centered(frame: &mut Frame<'_>, area: Rect, mut lines: Vec<Line<'_>>) -> (Rect, usize) {
    let mut dropped = 0usize;
    if lines.len() > area.height as usize {
        dropped = lines.len() - area.height as usize;
        lines.drain(..dropped);
    }
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let top = area.height.saturating_sub(height) / 2;
    let [_, middle, _] = Layout::vertical([
        Constraint::Length(top),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        middle,
    );
    (middle, dropped)
}

/// The sim's letter-spaced wordmark.
fn spaced_wordmark() -> String {
    "HAIDER CODE"
        .chars()
        .flat_map(|c| [c, ' '])
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn render_boot(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let sanctum = SanctumLine::new(model.sanctum_tier);
    let mut lines = vec![
        Line::styled(
            sanctum.mark(),
            theme.maroon_style().add_modifier(Modifier::BOLD),
        ),
        Line::styled(spaced_wordmark(), theme.bright_style()),
        // Sim `.sub` on the boot screen is GOLD (tui.js:5102-5107).
        Line::styled(boot_subline(VERSION), theme.gold_style()),
        Line::default(),
    ];
    if let Some(checks) = model.projection.boot_checks() {
        // Sim `.checks`: a LEFT-ALIGNED column inside the centered block —
        // pad every row to the widest so the marker glyphs align.
        let rows = check_rows(checks);
        let widest = rows
            .iter()
            .map(|row| row.line().chars().count())
            .max()
            .unwrap_or(0);
        for row in rows {
            let style = match row.marker {
                crate::boot::CheckMarker::Done => theme.ok_style(),
                crate::boot::CheckMarker::Current => theme.gold_style(),
                crate::boot::CheckMarker::Pending => theme.faint_style(),
            };
            lines.push(Line::styled(format!("{:<widest$}", row.line()), style));
        }
    }
    let _ = centered(frame, area, lines);
}

fn render_launcher(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    // Sim layout (Launcher, tui.js:4237 / JSX 3219): the centered content
    // column on top, then the palette (CmdMenu) directly ABOVE the composer,
    // then the gold-ruled composer at the bottom.
    let palette = if model.palette_open() {
        palette_block(model, theme, area.width)
    } else {
        Vec::new()
    };
    let mut palette_height = u16::try_from(palette.len()).unwrap_or(0);
    // Input sacred: the palette yields entirely before the composer loses a
    // row (review r1 P2 rule, same as the session layout).
    let fixed = 1 + 1 + 1 + 1; // content min + rule + composer + gap
    if palette_height > area.height.saturating_sub(fixed) {
        palette_height = 0;
    }
    let [content_area, palette_area, rule_area, composer_area, _gap] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(palette_height),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let sanctum = SanctumLine::new(model.sanctum_tier);
    let identity = &model.identity;
    // Sim typography: big maroon mark · gold shahada · gold rule ·
    // letter-spaced wordmark · dim version line.
    let mut lines = vec![
        Line::styled(
            sanctum.mark(),
            theme
                .maroon_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Line::default(),
    ];
    // Dignity rule: the sanctum renders whole or not at all, always alone.
    if let Some(text) = sanctum.fit(area.width.saturating_sub(2) as usize) {
        lines.push(Line::styled(text, theme.gold_style()));
        lines.push(Line::default());
    }
    lines.push(Line::styled("────────────────", theme.gold_style()));
    lines.push(Line::default());
    lines.push(Line::styled(spaced_wordmark(), theme.bright_style()));
    lines.push(Line::styled(launcher_subline(VERSION), theme.dim_style()));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("provider ", theme.faint_style()),
        Span::styled(identity.provider.clone(), theme.dim_style()),
        Span::styled(" · model ", theme.faint_style()),
        Span::styled(identity.model_short.clone(), theme.dim_style()),
        Span::styled(" · account ", theme.faint_style()),
        Span::styled(identity.account.clone(), theme.dim_style()),
        Span::styled(" · device ", theme.faint_style()),
        Span::styled(identity.device.clone(), theme.dim_style()),
    ]));
    // Sim `.dirline`: dir {dir} · mesh off.
    lines.push(Line::from(vec![
        Span::styled("dir ", theme.faint_style()),
        Span::styled(identity.dir.clone(), theme.dim_style()),
        Span::styled(" · mesh ", theme.faint_style()),
        Span::styled("off", theme.dim_style()),
    ]));
    lines.push(Line::default());

    // Recent sessions — sim seed rows (`.rhead` + rows, tui.js:3239),
    // with the gold `· N running` live count (`.livehd`).
    let running = model.samples.iter().filter(|s| s.running).count();
    let mut rhead = vec![Span::styled(
        "recent sessions — click or 1-3 attach · /sessions for all",
        theme.dim_style(),
    )];
    if running > 0 {
        rhead.push(Span::styled(
            format!(" · {running} running"),
            theme.gold_style(),
        ));
    }
    lines.push(Line::from(rhead));
    let mut sample_rows: Vec<(usize, usize)> = Vec::new();
    let mut extra_rows: Vec<usize> = Vec::new();
    for (index, sample) in model.samples.iter().enumerate() {
        // Sim `.dotc`: ok-green idle dot, gold pulsing when running.
        let (dot, dot_style) = if sample.running {
            ("◉", theme.gold_style())
        } else {
            ("●", theme.ok_style())
        };
        let mut spans = vec![
            Span::styled(format!("{} ", index + 1), theme.faint_style()),
            Span::styled(format!("{dot} "), dot_style),
            Span::styled(sample.name, theme.bright_style()),
            // Sim `.hd`: the head callsign + honorific, dim.
            Span::styled(
                format!(" ▸ {} {}", sample.head, sample.honorific),
                theme.dim_style(),
            ),
        ];
        if sample.running {
            spans.push(Span::styled("  running… ·", theme.gold_style()));
        }
        let meta = format!(
            " “{}” · {} · {} {} · {} tok · {} · {} · {}",
            sample.blurb,
            if sample.branches > 1 {
                format!("{} branches", sample.branches)
            } else {
                "1 branch".to_owned()
            },
            sample.turns,
            if sample.turns == 1 { "turn" } else { "turns" },
            fmt_tok(sample.tokens),
            sample.model,
            sample.device,
            sample.ago
        );
        // Sim `.meta`: ellipsized into the remaining width, never clipped
        // mid-frame by the centered layout.
        let meta_budget = (area.width as usize).saturating_sub(Line::from(spans.clone()).width());
        spans.push(Span::styled(
            ellipsize(&meta, meta_budget),
            theme.dim_style(),
        ));
        sample_rows.push((lines.len(), index));
        lines.push(Line::from(spans));
    }
    for (glyph, name, blurb) in [
        (
            "◉",
            "Aura",
            "voice session · orchestrator — spawns & steers, never codes",
        ),
        (
            "⚿",
            "Accounts",
            "provider credentials — OAuth & API keys, harness-owned",
        ),
        (
            "⇄",
            "Peers",
            "reachability ladder — peers · sponsored nodes · shells",
        ),
    ] {
        extra_rows.push(lines.len());
        // Sim `.aurarow`: gold glyph + gold name, dim meta.
        lines.push(Line::from(vec![
            Span::styled(format!("  {glyph} "), theme.gold_style()),
            Span::styled(name, theme.gold_style()),
            Span::styled(format!("  {blurb}"), theme.dim_style()),
        ]));
    }
    let (middle, dropped) = centered(frame, content_area, lines);
    let visible = |row: usize| row.checked_sub(dropped);
    for (row, index) in sample_rows {
        if let Some(row) = visible(row) {
            hits.push((
                row_rect(content_area, middle.y, row),
                Hit::AttachSample(index),
            ));
        }
    }
    for (order, row) in extra_rows.into_iter().enumerate() {
        if let Some(row) = visible(row) {
            hits.push((
                row_rect(content_area, middle.y, row),
                Hit::ExtraRow(u8::try_from(order).unwrap_or(2)),
            ));
        }
    }
    if palette_height > 0 {
        frame.render_widget(Paragraph::new(Text::from(palette)), palette_area);
        palette_row_hits(model, palette_area, hits);
    }
    render_composer(model, theme, frame, rule_area, composer_area, hits);
}

fn render_session(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    // A blocking menu REPLACES the composer (sim §3 law) and takes its rows:
    // title + options + the bottom hint line.
    let menu = model.projection.open_menu();
    let input_height = menu.map_or(1, |m| u16::try_from(m.options.len() + 2).unwrap_or(6));
    // Short windows: the INPUT is sacred — todos, then the palette, yield
    // entirely before the composer/menu loses a row (review r1 P2).
    let fixed = 2 + 1 + 1 + input_height + 1; // header + rule + rule + input + gap
    let mut todos_height = model
        .projection
        .todos()
        .filter(|t| t.pinned)
        .map_or(0, |t| u16::try_from(t.items.len() + 1).unwrap_or(4));
    let palette = if model.palette_open() {
        palette_block(model, theme, area.width)
    } else {
        Vec::new()
    };
    let mut palette_height = u16::try_from(palette.len()).unwrap_or(0);
    let mut budget = area.height.saturating_sub(fixed + 1); // ≥1 transcript row
    if palette_height > budget {
        palette_height = 0;
    } else {
        budget -= palette_height;
    }
    if todos_height > budget {
        todos_height = 0;
    }
    let [
        header_area,
        header_rule,
        transcript_area,
        todos_area,
        palette_area,
        rule_area,
        composer_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(todos_height),
        Constraint::Length(palette_height),
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(area);

    // Header (sim SessHead, tui.js:5183): [← main] chip · mark · bold GOLD
    // product · dim version · dir / dim session line with a GOLD head
    // callsign (`.headcs`).
    let sanctum = SanctumLine::new(model.sanctum_tier);
    let identity = &model.identity;
    let title = model.session_title.as_deref().unwrap_or("session");
    let (head, honorific) = model.session_head;
    let mut header_top = chip("← main".to_owned(), theme.dim_style());
    header_top.push(Span::styled(
        format!("  {}  ", sanctum.mark()),
        theme
            .maroon_style()
            .add_modifier(ratatui::style::Modifier::BOLD),
    ));
    header_top.push(Span::styled(
        "haider",
        theme
            .gold_style()
            .add_modifier(ratatui::style::Modifier::BOLD),
    ));
    header_top.push(Span::styled(format!(" v{VERSION}"), theme.dim_style()));
    header_top.push(Span::styled(
        format!(" · {}", identity.dir),
        theme.bright_style(),
    ));
    let header_bottom = vec![
        Span::styled("         ", theme.dim_style()),
        Span::styled(title, theme.dim_style()),
        Span::styled(format!(" ▸ {head} {honorific}"), theme.gold_style()),
        Span::styled(
            format!(" · branch main · {}", identity.device),
            theme.dim_style(),
        ),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(header_top),
            Line::from(header_bottom),
        ]))
        .style(theme.text_style()),
        header_area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(header_rule.width as usize),
            theme.frame_style(),
        )),
        header_rule,
    );
    hits.push((
        Rect {
            x: header_area.x,
            y: header_area.y,
            width: 10.min(header_area.width),
            height: 1,
        },
        Hit::BackChip,
    ));

    // Transcript: bottom-anchored; wheel scroll-back offsets follow-bottom.
    let mut lines: Vec<Line<'_>> = Vec::new();
    // Remember where each user prompt row lands (sticky origin line).
    let mut user_rows: Vec<(usize, &str)> = Vec::new();
    for entry in model.projection.entries() {
        if let TranscriptEntry::User { text, .. } = entry {
            // transcript_lines pushes a spacer, then the prompt row.
            user_rows.push((lines.len() + 1, text.as_str()));
        }
        transcript_lines(&mut lines, entry, theme, transcript_area.width);
    }
    // Sim `.thinking` (tui.js:4458): a transient gold tail while thinking.
    if model.projection.is_thinking() {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("● thinking…", theme.gold_style()),
        ]));
    }
    // Wrapped-row prefix sums — the sticky line and the wheel clamp need
    // real row math, not logical line counts.
    let mut row_of_line: Vec<u16> = Vec::with_capacity(lines.len());
    let mut total: u16 = 0;
    for line in &lines {
        row_of_line.push(total);
        let height = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(transcript_area.width);
        total = total.saturating_add(u16::try_from(height).unwrap_or(1));
    }
    let max_scroll = total.saturating_sub(transcript_area.height);
    // Render feedback for wheel clamping (G16) — interior mutability by
    // design: render reads `&AppModel`.
    model.scroll_max.set(max_scroll);
    let scroll_back = model.scroll_back.min(max_scroll);
    let scroll = max_scroll - scroll_back;
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((scroll, 0)), transcript_area);
    // Sticky origin line (sim StickyLine, tui.js:3345-3349): while scrolled
    // into history, pin the user prompt that produced the top-visible
    // content; click returns to the live tail.
    if scroll_back > 0 && scroll > 0 && transcript_area.height > 0 {
        let sticky = user_rows.iter().rev().find(|(line_index, _)| {
            row_of_line
                .get(*line_index)
                .is_some_and(|row| *row < scroll)
        });
        if let Some((_, text)) = sticky {
            let sticky_rect = Rect {
                x: transcript_area.x,
                y: transcript_area.y,
                width: transcript_area.width,
                height: 1,
            };
            let budget = (transcript_area.width as usize).saturating_sub(5);
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(
                    "❯ ",
                    theme
                        .maroon_style()
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ),
                Span::styled(
                    ellipsize(text, budget),
                    theme.bright_style().add_modifier(Modifier::UNDERLINED),
                ),
            ];
            let pad =
                (transcript_area.width as usize).saturating_sub(Line::from(spans.clone()).width());
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(theme.text_style().bg(theme.bar_bg.into())),
                sticky_rect,
            );
            hits.push((sticky_rect, Hit::StickyJump));
        }
    }

    if let Some(todos) = model.projection.todos().filter(|t| t.pinned) {
        // Sim TodoPanel (tui.js:2863-2888, 4667-4709): dim header with a
        // GOLD count; rows styled by state.
        let mut todo_lines = vec![Line::from(vec![
            Span::styled("▾ todos", theme.dim_style()),
            Span::styled(
                format!(" — {}/{} done", todos.done_count(), todos.items.len()),
                theme.gold_style(),
            ),
        ])];
        for item in &todos.items {
            todo_lines.push(todo_row(item, &todos.items, theme));
        }
        frame.render_widget(Paragraph::new(Text::from(todo_lines)), todos_area);
    }

    if palette_height > 0 {
        frame.render_widget(Paragraph::new(Text::from(palette)), palette_area);
        palette_row_hits(model, palette_area, hits);
    }

    if let Some(menu) = menu {
        // Sim InputMenu (tui.js:4932): warn top border, gold-soft ground,
        // warn title with the kind glyph, ❯-cursor options, faint bottom
        // hint carrying the answer-by-id contract.
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(rule_area.width as usize),
                theme.warn_style(),
            ))
            .style(theme.text_style()),
            rule_area,
        );
        let glyph = menu_glyph(&menu.kind);
        let mut menu_lines = vec![Line::from(vec![Span::styled(
            format!(" {glyph} {}", menu.title),
            theme.warn_style(),
        )])];
        for (index, option) in menu.options.iter().enumerate() {
            let selected = index == model.menu_selection;
            let cursor = if selected { "❯" } else { " " };
            let mut spans = vec![
                Span::styled(format!(" {cursor} "), theme.gold_style()),
                Span::styled(
                    format!("{}. {}", index + 1, option.label),
                    if selected {
                        theme.bright_style()
                    } else {
                        theme.menu_style()
                    },
                ),
            ];
            menu_lines.push(if selected {
                // Selection ground spans the full row (sim `.imo.sel`).
                let pad = (composer_area.width as usize)
                    .saturating_sub(Line::from(spans.clone()).width());
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                Line::from(spans).style(theme.selection_style())
            } else {
                Line::from(spans)
            });
        }
        menu_lines.push(Line::from(vec![Span::styled(
            format!(
                " ↑↓ select · ⏎ confirm · 1-{} quick · menu {} · menu.answer(\"{}\", n) over RPC",
                menu.options.len(),
                menu.id,
                menu.id
            ),
            theme.faint_style(),
        )]));
        frame.render_widget(
            Paragraph::new(Text::from(menu_lines)).style(theme.menu_style()),
            composer_area,
        );
        // Option rows sit between the title (offset 0) and the hint (last).
        for offset in 0..menu.options.len() {
            let y = composer_area.y + 1 + u16::try_from(offset).unwrap_or(u16::MAX);
            if y < composer_area.y + composer_area.height {
                hits.push((
                    Rect {
                        x: composer_area.x,
                        y,
                        width: composer_area.width,
                        height: 1,
                    },
                    Hit::MenuOption(offset),
                ));
            }
        }
    } else {
        render_composer(model, theme, frame, rule_area, composer_area, hits);
    }
}

/// Sim `MENU_GLYPH` (tui.js:3057) mapped onto the protocol's menu kinds.
const fn menu_glyph(kind: &haider_protocol::menu::MenuKind) -> &'static str {
    use haider_protocol::menu::MenuKind;
    match kind {
        MenuKind::Recovery { .. } => "⌁",
        MenuKind::Exhausted => "⟳",
        _ => "?",
    }
}

/// The gold rule + composer row on the input ground (sim InputBar,
/// tui.js:5395: `border-top: gold`, `background: inputBg`). Pushes the
/// talk-chip hit region so the click lands exactly on the chip.
fn render_composer(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    rule_area: Rect,
    row_area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(rule_area.width as usize),
            theme.gold_style(),
        ))
        .style(theme.text_style()),
        rule_area,
    );
    let (line, chip_at) = composer_line(model, theme, row_area.width);
    frame.render_widget(Paragraph::new(line).style(theme.input_style()), row_area);
    if let Some((offset, width)) = chip_at {
        hits.push((
            Rect {
                x: row_area.x + offset,
                y: row_area.y,
                width,
                height: 1,
            },
            Hit::TalkChip,
        ));
    }
}

/// The composer row (sim InputBar): padded off the frame edge, bold gold ❯
/// sigil, typed text (bright + gold block cursor) or the dim placeholder,
/// and the right-aligned `[ ◉ talk ]` chip. Returns the line plus the chip's
/// column offset + width for the hit map.
fn composer_line<'a>(
    model: &'a AppModel,
    theme: &Theme,
    width: u16,
) -> (Line<'a>, Option<(u16, u16)>) {
    let placeholder = match model.screen {
        Screen::Launcher => PLACEHOLDER_LAUNCHER,
        _ => PLACEHOLDER_SESSION,
    };
    let mut spans = vec![
        Span::raw(" ".repeat(COMPOSER_PAD)),
        Span::styled(
            "❯ ",
            theme
                .gold_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
    ];
    if model.composer.is_empty() {
        spans.push(Span::styled("▮", theme.gold_style()));
        spans.push(Span::styled(format!(" {placeholder}"), theme.dim_style()));
    } else {
        spans.push(Span::styled(model.composer.as_str(), theme.bright_style()));
        spans.push(Span::styled("▮", theme.gold_style()));
        // Inline ghost completion (sim `.ghostline`, tui.js:3028-3034):
        // the highlighted row's remainder dim, plus a faint ⇥ tab tag.
        if let Some(ghost) = model.ghost() {
            spans.push(Span::styled(ghost, theme.dim_style()));
            spans.push(Span::styled(" ⇥ tab", theme.faint_style()));
        }
    }
    // Display-cell widths (unicode-aware — review r1 P3: CJK/emoji composers
    // must not drift the chip).
    let typed_width = Line::from(spans.clone()).width();
    let chip_spans = chip("◉ talk".to_owned(), theme.gold_style());
    let chip_width = Line::from(chip_spans.clone()).width();
    // Right-aligned talk chip when there's room, padded off the right edge.
    let total = typed_width + chip_width + COMPOSER_PAD;
    let mut chip_at = None;
    if (width as usize) > total {
        let filler = width as usize - total;
        spans.push(Span::raw(" ".repeat(filler)));
        spans.extend(chip_spans);
        chip_at = Some((
            u16::try_from(typed_width + filler).unwrap_or(0),
            u16::try_from(chip_width).unwrap_or(0),
        ));
    }
    (Line::from(spans), chip_at)
}

/// The slash palette (sim CmdMenu, tui.js:5345): a frame rule on top, then
/// fixed-width command names (maroon; GOLD when selected) beside dim
/// ellipsized descriptions — the selected row on the selection ground — and
/// the key hint pinned at the BOTTOM. Empty matches render nothing (the sim
/// hides the menu entirely).
fn palette_block(model: &AppModel, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let items = model.palette_items();
    if items.is_empty() {
        return Vec::new();
    }
    let width = width as usize;
    let mut lines = vec![Line::styled("─".repeat(width), theme.frame_style())];
    for (index, item) in items.iter().take(PALETTE_MAX_ROWS).enumerate() {
        let selected = index == model.palette_selection;
        let name_style = if selected {
            theme.gold_style()
        } else {
            theme.maroon_style()
        };
        let name = format!("{:<PALETTE_NAME_COL$}", item.label());
        let desc_budget = width.saturating_sub(2 + PALETTE_NAME_COL + 2);
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(name, name_style),
            Span::raw("  "),
            Span::styled(ellipsize(item.desc(), desc_budget), theme.dim_style()),
        ];
        if selected {
            // Fill the row so the selection ground spans the full width.
            let pad = width.saturating_sub(Line::from(spans.clone()).width());
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            lines.push(Line::from(spans).style(theme.selection_style()));
        } else {
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(PALETTE_HINT, theme.faint_style()),
    ]));
    lines
}

/// Hit regions for the palette's option rows — offsets skip the top frame
/// rule and never cover the bottom hint line.
fn palette_row_hits(model: &AppModel, area: Rect, hits: &mut Vec<(Rect, Hit)>) {
    let rows = model.palette_items().len().min(PALETTE_MAX_ROWS);
    for offset in 0..rows {
        let y = area.y + 1 + u16::try_from(offset).unwrap_or(u16::MAX);
        if y < area.y + area.height {
            hits.push((
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
                Hit::PaletteRow(offset),
            ));
        }
    }
}

/// The /help overlay: sim HELP_TEXT panel over the body.
fn render_help(theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let height = u16::try_from(HELP_TEXT.len() + 2).unwrap_or(area.height);
    let [_, panel] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height.min(area.height)),
    ])
    .areas(area);
    let mut lines = vec![Line::from(vec![
        Span::styled("help", theme.gold_style()),
        Span::styled("  esc closes", theme.faint_style()),
    ])];
    for entry in HELP_TEXT {
        lines.push(Line::styled(*entry, theme.dim_style()));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.text_style().bg(theme.bar_bg.into())),
        panel,
    );
}

/// The status bar (sim StatusBar, tui.js:5492): boxed state chip · model ·
/// provider [· branch] · meter · voice chip · right hint (launcher)/flash.
/// Pushes the /help·theme hint's hit region when the hint is displayed.
fn render_status_bar(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let badge = model.projection.badge();
    let identity = &model.identity;
    let tokens = model.projection.context_tokens();
    #[allow(clippy::cast_precision_loss)]
    let pct = if identity.context_window == 0 {
        0.0
    } else {
        tokens as f64 / identity.context_window as f64
    };
    let meter = format!(
        "{} tok · {} {}% of {}",
        fmt_tok(tokens),
        meter_cells(pct, METER_CELLS_DEFAULT),
        (pct.clamp(0.0, 1.0) * 100.0).round(),
        fmt_tok(identity.context_window)
    );

    let mut left = vec![Span::styled(" ", theme.text_style())];
    left.extend(chip(
        badge,
        theme.badge_style(model.projection.badge_tone()),
    ));
    // Sim `.mid`: model · provider, plus the branch name inside a session.
    let branch = if model.screen == Screen::Session {
        " · main"
    } else {
        ""
    };
    left.push(Span::styled(
        format!("  {} · {}{branch}", identity.model_short, identity.provider),
        theme.text_style(),
    ));
    left.push(Span::styled(format!("  {meter}  "), theme.dim_style()));
    left.extend(chip(
        format!("◉ voice · {}", identity.voice),
        theme.gold_style(),
    ));

    let hint_shown = model.flash.is_none() && model.screen == Screen::Launcher && !model.help_open;
    let right = if let Some(flash) = &model.flash {
        flash.clone()
    } else if hint_shown {
        format!(
            "/help · theme {} ",
            model.theme.theme().label.to_lowercase()
        )
    } else {
        String::new()
    };
    let right_width = u16::try_from(right.chars().count()).unwrap_or(0);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(right_width)]).areas(area);
    // The sim separates the bar with a frame border-top; a terminal row is
    // too dear for a rule here, so the bar_bg tint carries the separation.
    frame.render_widget(
        Paragraph::new(Line::from(left)).style(theme.text_style().bg(theme.bar_bg.into())),
        left_area,
    );
    if right_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(right, theme.dim_style()))
                .alignment(Alignment::Right)
                .style(theme.text_style().bg(theme.bar_bg.into())),
            right_area,
        );
        if hint_shown {
            // The hint's hit region is exactly the right-aligned text.
            hits.push((right_area, Hit::HelpHint));
        }
    }
}

fn transcript_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    entry: &'a TranscriptEntry,
    theme: &Theme,
    width: u16,
) {
    match entry {
        TranscriptEntry::User { text, attachments } => {
            lines.push(Line::default());
            // Sim UserRow (tui.js:4465-4492): MAROON bold sigil (gold ❯
            // belongs to the composer/sticky only), bright text, gold pill
            // paste tokens.
            let mut spans = vec![
                Span::raw(" "),
                Span::styled("❯ ", theme.maroon_style().add_modifier(Modifier::BOLD)),
            ];
            spans.extend(user_text_spans(text, theme));
            if *attachments > 0 {
                spans.push(Span::styled(
                    format!(" [+{attachments} attachment(s)]"),
                    theme.dim_style(),
                ));
            }
            lines.push(Line::from(spans));
        }
        TranscriptEntry::Item(block) => item_lines(lines, block, theme, width),
        TranscriptEntry::Note { text } => {
            // Sim NoteRow (tui.js:4572-4577): dim, indented off the margin.
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(text.as_str(), theme.dim_style()),
            ]));
        }
    }
}

/// User text split into spans, styling sim paste/image tokens gold on the
/// gold-soft ground (`.ptoken`, tui.js:4480-4486).
fn user_text_spans<'a>(text: &'a str, theme: &Theme) -> Vec<Span<'a>> {
    let token_style = theme.gold_style().bg(theme.gold_soft.into());
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let candidate = &rest[start..];
        if let Some(len) = paste_token_len(candidate) {
            if start > 0 {
                spans.push(Span::styled(&rest[..start], theme.bright_style()));
            }
            spans.push(Span::styled(&candidate[..len], token_style));
            rest = &candidate[len..];
        } else {
            // Not a token: emit through the bracket and keep scanning.
            spans.push(Span::styled(&rest[..=start], theme.bright_style()));
            rest = &candidate[1..];
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest, theme.bright_style()));
    }
    spans
}

/// Length of a sim paste token (`[Pasted N lines]` / `[Image #N]`) at the
/// start of `text`, if present.
fn paste_token_len(text: &str) -> Option<usize> {
    for (prefix, suffix) in [("[Pasted ", " lines]"), ("[Image #", "]")] {
        if let Some(body) = text.strip_prefix(prefix) {
            let digits = body.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 && body[digits..].starts_with(suffix) {
                return Some(prefix.len() + digits + suffix.len());
            }
        }
    }
    None
}

/// Greedy word wrap by display cells — the agent body wraps manually so
/// EVERY visual row carries the gold-soft rail (sim border-left).
fn wrap_words(text: &str, budget: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = word.chars().count();
        let sep = usize::from(!row.is_empty());
        if row_width + sep + word_width > budget && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        if !row.is_empty() {
            row.push(' ');
            row_width += 1;
        }
        row.push_str(word);
        row_width += word_width;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

/// One pinned-todo row, styled by state (sim `.trow` classes: completed
/// struck faint, processing gold+bright, listed dim, dep-blocked faint with
/// its `· after #n` tag).
fn todo_row<'a>(item: &'a TodoItem, all: &[TodoItem], theme: &Theme) -> Line<'a> {
    let blocked = item.state == TodoState::Listed
        && item.dep.is_some_and(|dep| {
            all.get(dep as usize)
                .is_some_and(|d| d.state != TodoState::Completed)
        });
    let (mark, mark_style, text_style) = match item.state {
        TodoState::Completed => (
            "✓",
            theme.ok_style(),
            theme.faint_style().add_modifier(Modifier::CROSSED_OUT),
        ),
        TodoState::Processing => ("■", theme.gold_style(), theme.bright_style()),
        TodoState::Listed if blocked => ("☐", theme.faint_style(), theme.faint_style()),
        TodoState::Listed => ("☐", theme.dim_style(), theme.dim_style()),
    };
    let mut spans = vec![
        Span::styled(format!("  {mark} "), mark_style),
        Span::styled(item.text.as_str(), text_style),
    ];
    if let Some(dep) = item.dep
        && item.state == TodoState::Listed
    {
        spans.push(Span::styled(
            format!(" · after #{}", dep + 1),
            theme.faint_style(),
        ));
    }
    Line::from(spans)
}

/// Dim tool description derived from the call args (sim ToolRow `.desc` —
/// the demo script carries path/query/glob keys).
fn tool_desc(args: &serde_json::Value) -> String {
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        return path.to_owned();
    }
    if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
        return match args.get("glob").and_then(|v| v.as_str()) {
            Some(glob) => format!("\"{query}\" {glob}"),
            None => format!("\"{query}\""),
        };
    }
    String::new()
}

fn item_lines<'a>(lines: &mut Vec<Line<'a>>, block: &'a ItemBlock, theme: &Theme, width: u16) {
    match &block.item {
        TurnItem::AgentMessage { text } => {
            // Sim AgentRow (tui.js:4494-4513): the ■ haider header is GOLD;
            // the body indents behind a gold-soft left rail on every
            // wrapped line (manual wrap keeps the rail on continuations).
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("■ haider", theme.gold_style()),
            ]));
            let budget = (width as usize).saturating_sub(3).max(8);
            let body = wrap_words(text, budget);
            let last = body.len().saturating_sub(1);
            for (index, row) in body.into_iter().enumerate() {
                let mut spans = vec![
                    Span::raw(" "),
                    Span::styled("▏ ", theme.rail_style()),
                    Span::styled(row, theme.text_style()),
                ];
                if block.streaming && index == last {
                    spans.push(Span::styled("▮", theme.gold_style()));
                }
                lines.push(Line::from(spans));
            }
        }
        TurnItem::Reasoning { summary } => {
            lines.push(Line::from(vec![
                Span::styled(" · ", theme.faint_style()),
                Span::styled(summary.as_str(), theme.dim_style()),
            ]));
        }
        TurnItem::ToolCall {
            name, status, args, ..
        } => {
            // Sim ToolRow (tui.js:3901-3908): glyph (ok / warn-running /
            // err) · MAROON name · dim ellipsized desc from the args.
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(
                    format!("{} ", status_glyph(*status)),
                    match status {
                        haider_protocol::item::ToolStatus::Failed => theme.err_style(),
                        haider_protocol::item::ToolStatus::Cancelled => theme.dim_style(),
                        haider_protocol::item::ToolStatus::Pending
                        | haider_protocol::item::ToolStatus::InProgress => theme.warn_style(),
                        haider_protocol::item::ToolStatus::Completed => theme.ok_style(),
                    },
                ),
                Span::styled(name.as_str(), theme.maroon_style()),
            ];
            let desc = tool_desc(args);
            if !desc.is_empty() {
                let used = Line::from(spans.clone()).width();
                let budget = (width as usize).saturating_sub(used + 1);
                spans.push(Span::styled(
                    format!(" {}", ellipsize(&desc, budget)),
                    theme.dim_style(),
                ));
            }
            lines.push(Line::from(spans));
        }
        TurnItem::CommandExecution {
            command,
            status,
            exit_code,
            ..
        } => {
            let mut spans = vec![
                Span::styled("  $ ", theme.gold_style()),
                Span::styled(command.as_str(), theme.text_style()),
                Span::styled(format!(" {}", status_glyph(*status)), theme.dim_style()),
            ];
            if let Some(code) = exit_code {
                spans.push(Span::styled(format!(" · exit {code}"), theme.dim_style()));
            }
            lines.push(Line::from(spans));
            for line in block.output_text().lines() {
                lines.push(Line::styled(format!("    {line}"), theme.faint_style()));
            }
            // Honesty below the tail (r2: bottom-anchored viewport).
            if block.output_truncated {
                lines.push(Line::styled(
                    "    ⋯ output above is a bounded tail — earlier output truncated",
                    theme.dim_style(),
                ));
            }
            if block.output_decode_error {
                lines.push(Line::styled(
                    "    ⚠ some output could not be decoded",
                    theme.warn_style(),
                ));
            }
        }
        TurnItem::FileChange {
            path,
            added,
            removed,
        } => {
            // Sim shape (seed rows, tui.js:480): a completed fs_patch tool
            // row — ✓ glyph · maroon name · dim path · dim +a −r meta.
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("✓ ", theme.ok_style()),
                Span::styled("fs_patch", theme.maroon_style()),
                Span::styled(format!(" {path}"), theme.dim_style()),
                Span::styled(format!(" +{added} −{removed}"), theme.dim_style()),
            ]));
        }
        TurnItem::ChildSpawn { agent } => {
            lines.push(Line::styled(
                format!("  ◉ subagent {} spawned", agent.as_str()),
                theme.dim_style(),
            ));
        }
        TurnItem::ChildResult { report } => {
            lines.push(Line::styled(
                format!("  └ subagent report — {}", report.summary),
                theme.dim_style(),
            ));
        }
        TurnItem::Plan { items } => {
            let done = items
                .iter()
                .filter(|i| i.state == TodoState::Completed)
                .count();
            if !items.is_empty() && done == items.len() {
                // Sim TodosDone card (tui.js:3925-3935): ok header + one
                // struck faint row per item.
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("☑ plan completed — {} todos", items.len()),
                        theme.ok_style(),
                    ),
                ]));
                for item in items {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled("✓ ", theme.ok_style()),
                        Span::styled(
                            item.text.as_str(),
                            theme.faint_style().add_modifier(Modifier::CROSSED_OUT),
                        ),
                    ]));
                }
            } else {
                lines.push(Line::from(vec![
                    Span::styled("  ✓ ", theme.ok_style()),
                    Span::styled(
                        format!("plan — {done}/{} done", items.len()),
                        theme.dim_style(),
                    ),
                ]));
            }
        }
        TurnItem::ContextCompaction { .. } => {
            // Sim CompactRow (tui.js:3919-3924) is a gold card. The
            // protocol item carries only the summary artifact — there are
            // no before/after token counts to show honestly.
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "⊟ context compacted — summary retained · originals stay in /tree",
                    theme.gold_style(),
                ),
            ]));
        }
        TurnItem::Extension { kind, .. } => {
            lines.push(Line::styled(format!("  ⋯ {kind}"), theme.faint_style()));
        }
    }
}
