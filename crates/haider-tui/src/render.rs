//! Screen renderers: pure functions of ([`AppModel`], frame area) → buffer.
//! Testable headlessly via `TestBackend` (research rec 16/18). Every color
//! comes from the theme's style vocabulary — no literals (rec 12).

use crate::app::{AppModel, Screen};
use crate::boot::{boot_subline, check_rows, launcher_subline};
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

/// Render the whole frame for the current screen.
pub fn render(model: &AppModel, frame: &mut Frame<'_>) {
    let theme = model.theme.theme();
    let area = frame.area();
    // Ground the whole frame in the theme bg.
    frame.render_widget(Block::default().style(theme.text_style()), area);

    let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    match model.screen {
        Screen::Boot => render_boot(model, theme, frame, body),
        Screen::Launcher => render_launcher(model, theme, frame, body),
        Screen::Session => render_session(model, theme, frame, body),
    }
    render_status_bar(model, theme, frame, status);
}

fn centered(frame: &mut Frame<'_>, area: Rect, lines: Vec<Line<'_>>) {
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
}

fn render_boot(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let sanctum = SanctumLine::new(model.sanctum_tier);
    let mut lines = vec![
        Line::styled(sanctum.mark(), theme.gold_style()),
        Line::styled("HAIDER CODE", theme.bright_style()),
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
    centered(frame, area, lines);
}

fn render_launcher(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let sanctum = SanctumLine::new(model.sanctum_tier);
    let identity = &model.identity;
    let mut lines = vec![Line::styled(sanctum.mark(), theme.gold_style())];
    // Dignity rule: the sanctum renders whole or not at all, always alone.
    if let Some(text) = sanctum.fit(area.width.saturating_sub(2) as usize) {
        lines.push(Line::styled(text, theme.maroon_style()));
    }
    lines.push(Line::styled("──────────", theme.faint_style()));
    lines.push(Line::styled("HAIDER CODE", theme.bright_style()));
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
    lines.push(Line::default());
    if model.projection.entries().is_empty() {
        lines.push(Line::styled(
            "no sessions yet — your first message starts one",
            theme.dim_style(),
        ));
    } else {
        lines.push(Line::styled(
            "session live — ⏎ attaches · esc returns here",
            theme.dim_style(),
        ));
    }
    lines.push(Line::default());
    lines.push(composer_line(model, theme));
    centered(frame, area, lines);
}

fn render_session(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let todos_height = model
        .projection
        .todos()
        .filter(|t| t.pinned)
        .map_or(0, |t| u16::try_from(t.items.len() + 1).unwrap_or(4));
    // A blocking menu REPLACES the composer (sim §3 law) and takes its rows.
    let menu = model.projection.open_menu();
    let input_height = menu.map_or(1, |m| u16::try_from(m.options.len() + 1).unwrap_or(6));
    // Header (sim parity): mark · "haider vX · dir" / session line, then a
    // frame rule; one spacer row keeps the composer OFF the status bar.
    let [
        header_area,
        header_rule,
        transcript_area,
        todos_area,
        rule_area,
        composer_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(todos_height),
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(area);

    let sanctum = SanctumLine::new(model.sanctum_tier);
    let identity = &model.identity;
    let title = model.session_title.as_deref().unwrap_or("session");
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled(format!(" {} ", sanctum.mark()), theme.gold_style()),
                Span::styled("haider", theme.bright_style()),
                Span::styled(format!(" v{VERSION} · {}", identity.dir), theme.dim_style()),
            ]),
            Line::from(vec![
                Span::styled(" ← esc ", theme.faint_style()),
                Span::styled("· ", theme.faint_style()),
                Span::styled(title, theme.maroon_style()),
                Span::styled(format!(" · {}", identity.device), theme.dim_style()),
            ]),
        ]))
        .style(theme.text_style().bg(theme.bar_bg.into())),
        header_area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(header_rule.width as usize),
            theme.frame_style(),
        )),
        header_rule,
    );

    // Transcript: bottom-anchored, follow-bottom (scroll state is a later
    // slice — rec 10).
    let mut lines: Vec<Line<'_>> = Vec::new();
    for entry in model.projection.entries() {
        transcript_lines(&mut lines, entry, theme);
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(transcript_area.width)).unwrap_or(u16::MAX);
    let scroll = total.saturating_sub(transcript_area.height);
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

    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(rule_area.width as usize),
            theme.faint_style(),
        )),
        rule_area,
    );
    if let Some(menu) = menu {
        let mut menu_lines = vec![Line::from(vec![
            Span::styled("? ", theme.warn_style()),
            Span::styled(menu.title.as_str(), theme.bright_style()),
            Span::styled("  ↑↓ select · ⏎ confirm · 1-9 quick", theme.faint_style()),
        ])];
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
                        theme.dim_style()
                    },
                ),
            ]);
            menu_lines.push(if selected {
                line.style(theme.selection_style())
            } else {
                line
            });
        }
        frame.render_widget(Paragraph::new(Text::from(menu_lines)), composer_area);
    } else {
        frame.render_widget(
            Paragraph::new(composer_line(model, theme)).style(theme.input_style()),
            composer_area,
        );
    }
}

fn composer_line<'a>(model: &'a AppModel, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled("❯ ", theme.gold_style()),
        Span::styled(model.composer.as_str(), theme.bright_style()),
        Span::styled("▮", theme.gold_style()),
    ])
}

fn render_status_bar(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
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
        "{} tok · {} {}% of {} ",
        fmt_tok(tokens),
        meter_cells(pct, METER_CELLS_DEFAULT),
        (pct.clamp(0.0, 1.0) * 100.0).round(),
        fmt_tok(identity.context_window)
    );
    // Narrow-mode policy (review r1 P3): the badge is primary state — the
    // meter yields entirely before the badge loses a single cell.
    let mut meter_width = u16::try_from(meter.chars().count()).unwrap_or(0);
    let badge_min = u16::try_from(badge.chars().count() + 4).unwrap_or(u16::MAX);
    if meter_width.saturating_add(badge_min) > area.width {
        meter_width = 0;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(meter_width)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {badge} "),
                theme.badge_style(model.projection.badge_tone()),
            ),
            Span::styled(
                format!(" {} · {}", identity.model_short, identity.provider),
                theme.dim_style(),
            ),
        ]))
        .style(theme.text_style().bg(theme.bar_bg.into())),
        left,
    );
    if meter_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(meter, theme.dim_style()))
                .alignment(Alignment::Right)
                .style(theme.text_style().bg(theme.bar_bg.into())),
            right,
        );
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
                Span::styled("⚒ ", theme.maroon_style()),
                Span::styled(name.as_str(), theme.dim_style()),
                Span::styled(format!(" {}", status_glyph(*status)), theme.ok_style()),
            ]));
        }
        TurnItem::CommandExecution {
            command,
            status,
            exit_code,
            ..
        } => {
            let mut spans = vec![
                Span::styled("$ ", theme.gold_style()),
                Span::styled(command.as_str(), theme.text_style()),
                Span::styled(format!(" {}", status_glyph(*status)), theme.dim_style()),
            ];
            if let Some(code) = exit_code {
                spans.push(Span::styled(format!(" · exit {code}"), theme.dim_style()));
            }
            lines.push(Line::from(spans));
            for line in block.output_text().lines() {
                lines.push(Line::styled(format!("  {line}"), theme.faint_style()));
            }
            // Honesty notices go BELOW the tail: the transcript is
            // bottom-anchored, so only the last lines are guaranteed visible —
            // a long tail must never scroll the warnings away (review r2 P2).
            if block.output_truncated {
                lines.push(Line::styled(
                    "  ⋯ output above is a bounded tail — earlier output truncated",
                    theme.dim_style(),
                ));
            }
            if block.output_decode_error {
                lines.push(Line::styled(
                    "  ⚠ some output could not be decoded",
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
                Span::styled("± ", theme.gold_style()),
                Span::styled(path.as_str(), theme.text_style()),
                Span::styled(format!(" +{added}"), theme.ok_style()),
                Span::styled(format!(" -{removed}"), theme.err_style()),
            ]));
        }
        TurnItem::ChildSpawn { agent } => {
            lines.push(Line::styled(
                format!("◉ subagent {} spawned", agent.as_str()),
                theme.dim_style(),
            ));
        }
        TurnItem::ChildResult { report } => {
            lines.push(Line::styled(
                format!("└ subagent report — {}", report.summary),
                theme.dim_style(),
            ));
        }
        TurnItem::Plan { items } => {
            let done = items
                .iter()
                .filter(|i| i.state == TodoState::Completed)
                .count();
            lines.push(Line::from(vec![
                Span::styled("✓ ", theme.ok_style()),
                Span::styled(
                    format!("plan — {done}/{} done", items.len()),
                    theme.dim_style(),
                ),
            ]));
        }
        TurnItem::ContextCompaction { .. } => {
            lines.push(Line::styled("⊟ context compacted", theme.dim_style()));
        }
        TurnItem::Extension { kind, .. } => {
            lines.push(Line::styled(format!("⋯ {kind}"), theme.faint_style()));
        }
    }
}
