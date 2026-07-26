//! Screen renderers: pure functions of ([`AppModel`], frame area) → buffer.
//! Testable headlessly via `TestBackend` (research rec 16/18). Every color
//! comes from the theme's style vocabulary — no literals (rec 12).
//! Visual authority: the `/tui` sim — typography, chips, and row shapes are
//! copied from it deliberately.

use crate::app::{AppModel, Hit, Screen};
use crate::boot::{boot_subline, check_rows, launcher_subline};
use crate::commands::HELP_TEXT;
use crate::format::{METER_CELLS_DEFAULT, fmt_tok, meter_cells};
use crate::plain::status_glyph;
use crate::projection::{ItemBlock, TranscriptEntry};
use crate::sanctum::SanctumLine;
use crate::theme::Theme;
use haider_protocol::history::TodoState;
use haider_protocol::item::TurnItem;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
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
/// Palette rows shown at once (the sim scrolls beyond its max-height).
const PALETTE_MAX_ROWS: usize = 8;
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
            theme
                .maroon_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Line::styled(spaced_wordmark(), theme.bright_style()),
        Line::styled(boot_subline(VERSION), theme.dim_style()),
        Line::default(),
    ];
    if let Some(checks) = model.projection.boot_checks() {
        for row in check_rows(checks) {
            let style = match row.marker {
                crate::boot::CheckMarker::Done => theme.ok_style(),
                crate::boot::CheckMarker::Current => theme.gold_style(),
                crate::boot::CheckMarker::Pending => theme.faint_style(),
            };
            lines.push(Line::styled(row.line(), style));
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

    // Recent sessions — sim seed rows (`.rhead` + rows, tui.js:3239).
    lines.push(Line::from(vec![Span::styled(
        "recent sessions — click or 1-3 attach · /sessions for all",
        theme.dim_style(),
    )]));
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
    for entry in model.projection.entries() {
        transcript_lines(&mut lines, entry, theme);
    }
    // Sim `.thinking` (tui.js:4458): a transient gold tail while thinking.
    if model.projection.is_thinking() {
        lines.push(Line::styled("● thinking…", theme.gold_style()));
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(transcript_area.width)).unwrap_or(u16::MAX);
    let scroll = total
        .saturating_sub(transcript_area.height)
        .saturating_sub(model.scroll_back);
    frame.render_widget(paragraph.scroll((scroll, 0)), transcript_area);

    if let Some(todos) = model.projection.todos().filter(|t| t.pinned) {
        let mut todo_lines = vec![Line::from(vec![
            Span::styled("▾ todos", theme.gold_style()),
            Span::styled(
                format!(" — {}/{} done", todos.done_count(), todos.items.len()),
                theme.dim_style(),
            ),
        ])];
        for item in &todos.items {
            let (mark, style) = match item.state {
                TodoState::Completed => ("✓", theme.ok_style()),
                TodoState::Processing => ("■", theme.gold_style()),
                TodoState::Listed => ("☐", theme.dim_style()),
            };
            todo_lines.push(Line::from(vec![
                Span::styled(format!("  {mark} "), style),
                Span::styled(item.text.clone(), theme.text_style()),
            ]));
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
            let line = Line::from(vec![
                Span::styled(format!(" {cursor} "), theme.gold_style()),
                Span::styled(
                    format!("{}. {}", index + 1, option.label),
                    if selected {
                        theme.bright_style()
                    } else {
                        theme.menu_style()
                    },
                ),
            ]);
            menu_lines.push(if selected {
                line.style(theme.selection_style())
            } else {
                line
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
    for (index, spec) in items.iter().take(PALETTE_MAX_ROWS).enumerate() {
        let selected = index == model.palette_selection;
        let name_style = if selected {
            theme.gold_style()
        } else {
            theme.maroon_style()
        };
        let name = format!("{:<PALETTE_NAME_COL$}", format!("/{}", spec.name));
        let desc_budget = width.saturating_sub(2 + PALETTE_NAME_COL + 2);
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(name, name_style),
            Span::raw("  "),
            Span::styled(ellipsize(spec.desc, desc_budget), theme.dim_style()),
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

fn transcript_lines<'a>(lines: &mut Vec<Line<'a>>, entry: &'a TranscriptEntry, theme: &Theme) {
    match entry {
        TranscriptEntry::User { text, attachments } => {
            lines.push(Line::default());
            let mut spans = vec![
                Span::styled("❯ ", theme.gold_style()),
                Span::styled(text.as_str(), theme.bright_style()),
            ];
            if *attachments > 0 {
                spans.push(Span::styled(
                    format!(" [+{attachments} attachment(s)]"),
                    theme.dim_style(),
                ));
            }
            lines.push(Line::from(spans));
        }
        TranscriptEntry::Item(block) => item_lines(lines, block, theme),
    }
}

fn item_lines<'a>(lines: &mut Vec<Line<'a>>, block: &'a ItemBlock, theme: &Theme) {
    match &block.item {
        TurnItem::AgentMessage { text } => {
            // Sim parity: agent blocks carry a "■ haider" name header.
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled("■ ", theme.maroon_style()),
                Span::styled("haider", theme.dim_style()),
            ]));
            let mut spans = vec![Span::styled(text.as_str(), theme.text_style())];
            if block.streaming {
                spans.push(Span::styled("▮", theme.gold_style()));
            }
            lines.push(Line::from(spans));
        }
        TurnItem::Reasoning { summary } => {
            lines.push(Line::from(vec![
                Span::styled("· ", theme.faint_style()),
                Span::styled(summary.as_str(), theme.dim_style()),
            ]));
        }
        TurnItem::ToolCall { name, status, .. } => {
            lines.push(Line::from(vec![
                Span::styled("  ", theme.text_style()),
                Span::styled(
                    format!("{} ", status_glyph(*status)),
                    match status {
                        haider_protocol::item::ToolStatus::Failed => theme.err_style(),
                        haider_protocol::item::ToolStatus::Cancelled => theme.dim_style(),
                        _ => theme.ok_style(),
                    },
                ),
                Span::styled(name.as_str(), theme.bright_style()),
            ]));
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
            lines.push(Line::from(vec![
                Span::styled("  ± ", theme.gold_style()),
                Span::styled(path.as_str(), theme.text_style()),
                Span::styled(format!(" +{added}"), theme.ok_style()),
                Span::styled(format!(" -{removed}"), theme.err_style()),
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
            lines.push(Line::from(vec![
                Span::styled("  ✓ ", theme.ok_style()),
                Span::styled(
                    format!("plan — {done}/{} done", items.len()),
                    theme.dim_style(),
                ),
            ]));
        }
        TurnItem::ContextCompaction { .. } => {
            lines.push(Line::styled("  ⊟ context compacted", theme.dim_style()));
        }
        TurnItem::Extension { kind, .. } => {
            lines.push(Line::styled(format!("  ⋯ {kind}"), theme.faint_style()));
        }
    }
}
