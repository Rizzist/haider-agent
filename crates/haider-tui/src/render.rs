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
    // The status row is the FIRST chrome to yield when a session's sacred
    // input — a blocking menu's options OR the composer's cursor row
    // (review r5 P2-1 + r6 P2-1) — cannot otherwise fit. Minimal need
    // with the full 4-row chrome is status(1) + chrome(4) + floor.
    let status_height: u16 = if matches!(model.screen, Screen::Session | Screen::Subagent) {
        let input_floor = if model.screen == Screen::Session {
            model
                .projection
                .open_menu()
                .map_or(1, |menu| menu.options.len())
        } else {
            model
                .viewed_chip()
                .and_then(crate::app::ChipModel::question_menu)
                .map_or(1, |menu| menu.options.len())
        };
        if (area.height as usize) < 1 + 4 + input_floor {
            0
        } else {
            1
        }
    } else {
        1
    };
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(status_height)]).areas(area);
    match model.screen {
        Screen::Boot => render_boot(model, theme, frame, body),
        Screen::Launcher => render_launcher(model, theme, frame, body, &mut hits),
        Screen::Session => render_session(model, theme, frame, body, &mut hits),
        Screen::Subagent => render_subagent(model, theme, frame, body, &mut hits),
        Screen::Aura => render_aura(model, theme, frame, body, &mut hits),
    }
    if model.help_open {
        render_help(theme, frame, body);
        hits.clear();
    }
    if status_height > 0 {
        render_status_bar(model, theme, frame, status, &mut hits);
    }
    // Hit-map seam guard (review r6 P2-1b): a hit must be a real, visible
    // region — non-empty and fully inside the frame. Shed or starved
    // regions can never leak phantom click targets, at ANY size.
    hits.retain(|(rect, _)| {
        rect.width > 0
            && rect.height > 0
            && rect.x.saturating_add(rect.width) <= area.x.saturating_add(area.width)
            && rect.y.saturating_add(rect.height) <= area.y.saturating_add(area.height)
    });
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
/// Max composer rows before it scrolls internally (sim textarea autoGrow
/// cap `max-height: 128px`, tui.js:2799-2803 + 5431).
const COMPOSER_MAX_ROWS: usize = 5;

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

/// A chip whose CHROME (the `[ ]` border stand-in) and label carry
/// different inks — the sim's frame-bordered pills with colored text
/// (`.mic`, `.backbtn`, `.voice`: border frame, label gold/dim).
fn chip_two_tone<'a>(
    label: String,
    chrome: ratatui::style::Style,
    label_style: ratatui::style::Style,
) -> Vec<Span<'a>> {
    vec![
        Span::styled("[ ", chrome),
        Span::styled(label, label_style),
        Span::styled(" ]", chrome),
    ]
}

/// Pad a span row to a fixed display width — the launcher's `.recent`
/// column trick: uniform-width lines center to one shared left edge, and
/// hover bands span the whole column.
fn pad_spans_to<'s>(mut spans: Vec<Span<'s>>, width: usize) -> Vec<Span<'s>> {
    let pad = width.saturating_sub(Line::from(spans.clone()).width());
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans
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
    // The mark gets breathing room per sim proportions (52px line-height;
    // a terminal cell cannot scale the glyph — noted divergence).
    let mut lines = vec![
        Line::default(),
        Line::styled(
            sanctum.mark(),
            theme.maroon_style().add_modifier(Modifier::BOLD),
        ),
        Line::default(),
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
    // Sacred-input ledger (review r3 P2-1a, launcher form; r6 P2-1: same
    // shed ladder as the session): the composer grows up to its need but
    // tail-windows to whatever the height allows — the cursor row is never
    // hidden. Under pressure the gap yields, then the content's sacred
    // row, then the rule, before the composer loses its row.
    let needed = composer_height(model);
    let mut gap: u16 = 1;
    let mut content_min: u16 = 1;
    let mut rule_h: u16 = 1;
    let mut input_avail = area.height.saturating_sub(content_min + rule_h + gap);
    if input_avail < 1 {
        gap = 0;
        input_avail = area.height.saturating_sub(content_min + rule_h);
    }
    if input_avail < 1 {
        content_min = 0;
        input_avail = area.height.saturating_sub(rule_h);
    }
    if input_avail < 1 {
        rule_h = 0;
        input_avail = area.height;
    }
    let composer_rows = needed.min(input_avail).clamp(1, area.height.max(1));
    let fixed = content_min + rule_h + composer_rows + gap;
    if palette_height > area.height.saturating_sub(fixed) {
        palette_height = 0;
    }
    let [content_area, palette_area, rule_area, composer_area, _gap] = Layout::vertical([
        Constraint::Min(content_min),
        Constraint::Length(palette_height),
        Constraint::Length(rule_h),
        Constraint::Length(composer_rows),
        Constraint::Length(gap),
    ])
    .areas(area);

    let sanctum = SanctumLine::new(model.sanctum_tier);
    let identity = &model.identity;
    // Sim typography (.center, tui.js:4243-4308): big maroon mark with
    // breathing room above and below · gold (NOT bold) shahada · the gold
    // half-strength rule · bright letter-spaced wordmark · dim version
    // line · info lines with DIM labels and BRIGHT values.
    let mut lines = vec![
        Line::default(),
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
    lines.push(Line::styled("────────────────", theme.rule_style()));
    lines.push(Line::default());
    lines.push(Line::styled(spaced_wordmark(), theme.bright_style()));
    lines.push(Line::styled(launcher_subline(VERSION), theme.dim_style()));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("provider ", theme.dim_style()),
        Span::styled(identity.provider.clone(), theme.bright_style()),
        Span::styled(" · model ", theme.dim_style()),
        Span::styled(identity.model_short.clone(), theme.bright_style()),
        Span::styled(" · account ", theme.dim_style()),
        Span::styled(identity.account.clone(), theme.bright_style()),
        Span::styled(" · device ", theme.dim_style()),
        Span::styled(identity.device.clone(), theme.bright_style()),
    ]));
    // Sim `.dirline`: dir {dir} · mesh off — `cd` on the launcher
    // retargets this dir (shell builtins, §4).
    lines.push(Line::from(vec![
        Span::styled("dir ", theme.dim_style()),
        Span::styled(model.launcher_dir.clone(), theme.bright_style()),
        Span::styled(" · mesh ", theme.dim_style()),
        Span::styled("off", theme.bright_style()),
    ]));
    lines.push(Line::default());

    // ---- The `.recent` COLUMN (tui.js:4331-4334): a fixed-width block,
    // centered as a whole, text-align LEFT inside — every session row and
    // Aura/Accounts/Peers row starts at the SAME left column. The column
    // is the widest row's content, capped by the frame (sim
    // min(560px, 92%)); every line pads to it so per-line centering yields
    // one shared left edge and hover bands span the full column.
    let area_cap = (area.width as usize).saturating_sub(4).max(10);
    // Sim `.rhead` verbatim + gold `· N running` (`.livehd`).
    let running = model.samples.iter().filter(|s| s.running).count();
    let mut rhead = vec![Span::styled(
        "recent sessions — click to attach · /sessions for all",
        theme.dim_style(),
    )];
    if running > 0 {
        rhead.push(Span::styled(
            format!(" · {running} running"),
            theme.gold_style(),
        ));
    }
    // Pass 1: build every row's spans, metas ellipsized to the frame cap.
    let mut recent: Vec<(Vec<Span<'_>>, Option<Hit>)> = vec![(rhead, None)];
    for (index, sample) in model.samples.iter().enumerate() {
        // Sim row anatomy (tui.js:3252-3277): dot (ok; gold running) ·
        // name BRIGHT bold · `▸ head hon` DIM (.hd) · meta DIM ellipsized.
        // No digit prefix (the 1-3 keys stay as silent bindings).
        let (dot, dot_style) = if sample.running {
            ("◉", theme.gold_style())
        } else {
            ("●", theme.ok_style())
        };
        let mut spans = vec![
            Span::styled(format!("{dot} "), dot_style),
            Span::styled(
                sample.name,
                theme
                    .bright_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
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
        // Sim `.meta`: ellipsized into the column, never clipped.
        let meta_budget = area_cap.saturating_sub(Line::from(spans.clone()).width());
        spans.push(Span::styled(
            ellipsize(&meta, meta_budget),
            theme.dim_style(),
        ));
        recent.push((spans, Some(Hit::AttachSample(index))));
    }
    for (order, (glyph, name, blurb)) in [
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
    ]
    .into_iter()
    .enumerate()
    {
        // Sim `.aurarow` (tui.js:4403-4413): gold glyph, gold name, dim
        // meta — its frame border-top rule is inserted in pass 2.
        let spans = vec![
            Span::styled(format!("{glyph} "), theme.gold_style()),
            Span::styled(name, theme.gold_style()),
            Span::styled(
                ellipsize(
                    &format!("  {blurb}"),
                    area_cap.saturating_sub(name.chars().count() + 2),
                ),
                theme.dim_style(),
            ),
        ];
        recent.push((spans, Some(Hit::ExtraRow(u8::try_from(order).unwrap_or(2)))));
    }
    // Pass 2: one shared column = widest row, capped by the frame.
    let column = recent
        .iter()
        .map(|(spans, _)| Line::from(spans.clone()).width())
        .max()
        .unwrap_or(10)
        .clamp(10, area_cap);
    let mut sample_rows: Vec<(usize, usize)> = Vec::new();
    let mut extra_rows: Vec<usize> = Vec::new();
    for (spans, hit) in recent {
        if matches!(hit, Some(Hit::ExtraRow(_))) {
            // The `.aurarow` frame border-top, spanning the column.
            lines.push(Line::styled("─".repeat(column), theme.frame_style()));
        }
        let mut line = Line::from(pad_spans_to(spans, column));
        // Hover band (sim `.recent button:hover`: selBg across the row).
        if hit.is_some() && model.hovered == hit {
            line = line.style(theme.hover_style());
        }
        match hit {
            Some(Hit::AttachSample(index)) => sample_rows.push((lines.len(), index)),
            Some(Hit::ExtraRow(_)) => extra_rows.push(lines.len()),
            _ => {}
        }
        lines.push(line);
    }
    // The `.shellout` block (sim tui.js:3302-3308): the last shell
    // builtin's `$ cmd` + output, under the recent list, same column.
    if let Some((cmd, out)) = &model.launcher_shellout {
        lines.push(Line::default());
        lines.push(Line::from(pad_spans_to(
            vec![
                Span::styled("$ ", theme.gold_style()),
                Span::styled(cmd.clone(), theme.bright_style()),
            ],
            column,
        )));
        for row in out.split('\n') {
            lines.push(Line::from(pad_spans_to(
                vec![Span::styled(row.to_owned(), theme.dim_style())],
                column,
            )));
        }
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
    // A blocking menu REPLACES the composer (sim §3 law) and takes its rows.
    //
    // Sacred-input height ledger (review r3 P2-1 + r4 P2-1 + r5 P2-1) —
    // invariants at ANY size:
    // (a) the composer's CURSOR row is visible: growth steals from the
    //     transcript first (up to 5 rows, sim autoGrow); when even that
    //     cannot fit, the composer tail-windows to its allocation instead
    //     of hiding the cursor;
    // (b) a menu's OPTIONS are always visible — they outrank EVERYTHING,
    //     chrome included. Full shed order under pressure: gap → hint →
    //     body-from-top → title-later → the transcript's sacred row (the
    //     sim's flex transcript collapses, tui.js:4444) → status row
    //     (handled in `render`) → header line 2 → header rule → input rule
    //     → header line 1 → options never (below option count the menu
    //     WINDOWS them around the selection with ⋮ markers).
    let menu = model.projection.open_menu();
    let menu_wrapped_body_rows = menu.map_or(0, |m| wrapped_menu_body(m, area.width).len());
    let needed_input = menu.map_or_else(
        || composer_height(model),
        |m| u16::try_from(1 + menu_wrapped_body_rows + m.options.len() + 1).unwrap_or(u16::MAX),
    );
    // What the input may claim: everything beyond header(2) + header
    // rule(1) + input rule(1) + gap(1) + one sacred transcript row.
    let mut gap: u16 = 1;
    let mut transcript_min: u16 = 1;
    let mut header_h: u16 = 2;
    let mut header_rule_h: u16 = 1;
    let mut input_rule_h: u16 = 1;
    let floor_input = menu.map_or(1, |m| u16::try_from(m.options.len().max(1)).unwrap_or(1));
    let mut input_avail = area
        .height
        .saturating_sub(header_h + header_rule_h + input_rule_h + gap + transcript_min);
    if input_avail < floor_input {
        // The spacer gap yields before any sacred row does.
        gap = 0;
        input_avail = area
            .height
            .saturating_sub(header_h + header_rule_h + input_rule_h + transcript_min);
    }
    // The sacred input — a menu's options OR the composer's cursor row —
    // outranks the transcript's sacred row (r4 P2-1) and then the chrome
    // itself, piece by piece (r5 P2-1, extended to the composer path by
    // r6 P2-1: the menu-close transition must never starve the composer):
    // session line → header rule → input rule → product line.
    if input_avail < floor_input {
        transcript_min = 0;
    }
    if area
        .height
        .saturating_sub(header_h + header_rule_h + input_rule_h)
        < floor_input
    {
        header_h = 1;
    }
    if area
        .height
        .saturating_sub(header_h + header_rule_h + input_rule_h)
        < floor_input
    {
        header_rule_h = 0;
    }
    if area
        .height
        .saturating_sub(header_h + header_rule_h + input_rule_h)
        < floor_input
    {
        input_rule_h = 0;
    }
    if area
        .height
        .saturating_sub(header_h + header_rule_h + input_rule_h)
        < floor_input
    {
        header_h = 0;
    }
    input_avail = area
        .height
        .saturating_sub(header_h + header_rule_h + input_rule_h + gap + transcript_min);
    let chrome = header_h + header_rule_h + input_rule_h;
    let input_height = needed_input
        .min(input_avail)
        .max(floor_input.min(area.height.saturating_sub(chrome)))
        .clamp(1, area.height.max(1));
    // The optional-panel ledger (review r1 P2; TUI3b adds the SubTree slot).
    // Each panel claims from `budget` in PRIORITY order below, so the LAST
    // claimant is the FIRST to vanish. Priority, survives-longest first:
    //
    //   palette  →  ⧗ queue  →  SubTree  →  todos
    //
    // Rationale for the SubTree's slot: it is a MAP of live work, so it
    // outranks the todos (whose plan the transcript re-prints when it
    // unpins) but yields to the ⧗ queue — which holds UNSENT user input,
    // the only rows here that would otherwise be silently lost — and to the
    // palette, a live interaction under the cursor. All four shed ENTIRELY
    // before the composer's cursor row or a blocking menu's options give up
    // a single row; those stay sacred at any size.
    let fixed = chrome + input_height + gap;
    let mut todos_height = model
        .projection
        .todos()
        .filter(|t| t.pinned)
        .map_or(0, |t| u16::try_from(t.items.len() + 1).unwrap_or(4));
    let mut queue_height = if model.msg_queue.is_empty() {
        0
    } else {
        u16::try_from(model.msg_queue.len() + 1).unwrap_or(4)
    };
    let mut subtree_height = subtree_needed(model, false);
    let palette = if model.palette_open() {
        palette_block(model, theme, area.width)
    } else {
        Vec::new()
    };
    let mut palette_height = u16::try_from(palette.len()).unwrap_or(0);
    let mut budget = area.height.saturating_sub(fixed + transcript_min);
    if palette_height > budget {
        palette_height = 0;
    } else {
        budget -= palette_height;
    }
    if queue_height > budget {
        queue_height = 0;
    } else {
        budget -= queue_height;
    }
    if subtree_height > budget {
        subtree_height = 0;
    } else {
        budget -= subtree_height;
    }
    if todos_height > budget {
        todos_height = 0;
    }
    let [
        header_area,
        header_rule,
        transcript_area,
        todos_area,
        queue_area,
        palette_area,
        rule_area,
        composer_area,
        subtree_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Length(header_rule_h),
        Constraint::Min(transcript_min),
        Constraint::Length(todos_height),
        Constraint::Length(queue_height),
        Constraint::Length(palette_height),
        Constraint::Length(input_rule_h),
        Constraint::Length(input_height),
        Constraint::Length(subtree_height),
        Constraint::Length(gap),
    ])
    .areas(area);

    // Header (sim SessHead, tui.js:5183): [← main] chip · mark · bold GOLD
    // product · dim version · dir / dim session line with a GOLD head
    // callsign (`.headcs`).
    let sanctum = SanctumLine::new(model.sanctum_tier);
    let identity = &model.identity;
    // The header shows the session's slug NAME (sim `session.name`); the
    // auto-title blurb lives in the `· session titled` note only.
    let title = model.display_name();
    let (head, honorific) = model.session_head;
    // Sim `.backbtn` (tui.js:5190-5205): FRAME border, dim label; hover
    // turns text and border gold.
    let back_hovered = model.hovered == Some(Hit::BackChip);
    let mut header_top = if back_hovered {
        chip_two_tone("← main".to_owned(), theme.gold_style(), theme.gold_style())
    } else {
        chip_two_tone("← main".to_owned(), theme.frame_style(), theme.dim_style())
    };
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
    // The session's working dir — `cd` retargets it (sim: "the agent
    // works elsewhere while the session stays global").
    header_top.push(Span::styled(
        format!(" · {}", model.session_dir),
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
    // Shed chrome renders nothing: a 1-row header keeps only the product
    // line (the area clips line 2), a 0-row header/rule disappears whole.
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
    if header_area.height > 0 {
        hits.push((
            Rect {
                x: header_area.x,
                y: header_area.y,
                width: 10.min(header_area.width),
                height: 1,
            },
            Hit::BackChip,
        ));
    }

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
    // RENDER is the single scroll authority (review r3 P2-2): the frame
    // writes the true max AND reconciles the model's offset against it, so
    // resizes/new content can never leave invisible debt banked anywhere.
    model.scroll_max.set(max_scroll);
    model
        .scroll_back
        .set(model.scroll_back.get().min(max_scroll));
    let scroll_back = model.scroll_back.get();
    let scroll = max_scroll - scroll_back;
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((scroll, 0)), transcript_area);
    // Sticky origin line (sim StickyLine, tui.js:3345-3349 / 4597-4623):
    // while scrolled into history, pin the user prompt that produced the
    // top-visible content. Chrome per sim: near-opaque THEME ground (no bar
    // tint), bright nowrap-ellipsized text, maroon bold sigil — no
    // underline. Click keeps the reader AT the prompt (jumpToSticky,
    // tui.js:2637-2645): the hit carries the scroll-back that puts the
    // prompt's first row at the viewport top; after a jump the sticky is
    // SUPPRESSED until the next real wheel so it never covers the row it
    // just revealed.
    if scroll_back > 0 && scroll > 0 && transcript_area.height > 0 && !model.sticky_suppressed {
        let sticky = user_rows.iter().rev().find(|(line_index, _)| {
            row_of_line
                .get(*line_index)
                .is_some_and(|row| *row < scroll)
        });
        if let Some((line_index, text)) = sticky {
            let jump = max_scroll
                .saturating_sub(row_of_line.get(*line_index).copied().unwrap_or(max_scroll));
            let sticky_rect = Rect {
                x: transcript_area.x,
                y: transcript_area.y,
                width: transcript_area.width,
                height: 1,
            };
            let budget = (transcript_area.width as usize).saturating_sub(5);
            let mut spans = vec![
                Span::raw(" "),
                Span::styled("❯ ", theme.maroon_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    ellipsize(&text.replace('\n', " "), budget),
                    theme.bright_style(),
                ),
            ];
            let pad =
                (transcript_area.width as usize).saturating_sub(Line::from(spans.clone()).width());
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(theme.text_style()),
                sticky_rect,
            );
            hits.push((sticky_rect, Hit::StickyJump(jump)));
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

    if queue_height > 0 {
        // The ⧗ queued panel (sim tui.js:2891-2906): header + numbered
        // rows, text truncated at 72 chars.
        let count = model.msg_queue.len();
        let plural = if count == 1 { "" } else { "s" };
        let mut queue_lines = vec![Line::from(vec![
            Span::styled("⧗ queued", theme.gold_style()),
            Span::styled(
                format!(" — {count} message{plural} · consumed at turn end, no idle between"),
                theme.dim_style(),
            ),
        ])];
        for (index, text) in model.msg_queue.iter().enumerate() {
            let shown: String = if text.chars().count() > 72 {
                format!("{}…", text.chars().take(72).collect::<String>())
            } else {
                text.clone()
            };
            queue_lines.push(Line::from(vec![
                Span::styled(format!("  {}. ", index + 1), theme.faint_style()),
                Span::styled(shown, theme.dim_style()),
            ]));
        }
        frame.render_widget(Paragraph::new(Text::from(queue_lines)), queue_area);
    }

    if palette_height > 0 {
        frame.render_widget(Paragraph::new(Text::from(palette)), palette_area);
        palette_row_hits(model, palette_area, hits);
    }

    if let Some(menu) = menu {
        // Sim InputMenu (tui.js:4932): warn top border, gold-soft ground.
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(rule_area.width as usize),
                theme.warn_style(),
            ))
            .style(theme.text_style()),
            rule_area,
        );
        let footer = format!(
            " ↑↓ select · ⏎ confirm · 1-{} quick · menu {} · menu.answer(\"{}\", n) over RPC",
            menu.options.len(),
            menu.id,
            menu.id
        );
        let (menu_lines, option_rows) =
            menu_block(menu, model.menu_selection, theme, composer_area, &footer);
        frame.render_widget(
            Paragraph::new(Text::from(menu_lines)).style(theme.menu_style()),
            composer_area,
        );
        // Hit rows come from what actually RENDERED (review r3 P2-1b/P2-4)
        // and carry the menu id (review r2 P2-2).
        for (row_offset, option_index) in option_rows {
            let y = composer_area.y + row_offset;
            if y < composer_area.y + composer_area.height {
                hits.push((
                    Rect {
                        x: composer_area.x,
                        y,
                        width: composer_area.width,
                        height: 1,
                    },
                    Hit::MenuOption {
                        menu: menu.id.clone(),
                        index: option_index,
                    },
                ));
            }
        }
    } else {
        render_composer(model, theme, frame, rule_area, composer_area, hits);
    }
    if subtree_height > 0 {
        render_subtree(model, theme, frame, subtree_area, false, hits);
    }
}

/// The SubTree panel's needed height (0 when there are no chips):
/// header + one row per (uncollapsed) tree node, plus the `⌂` home row on
/// the subagent screen.
fn subtree_needed(model: &AppModel, on_subagent: bool) -> u16 {
    if model.chips.is_empty() {
        return 0;
    }
    if model.subtree_collapsed {
        return 1;
    }
    let rows = crate::app::flatten_chips(&model.chips).len() + usize::from(on_subagent);
    u16::try_from(rows + 1).unwrap_or(u16::MAX)
}

/// `subCounts` (tui.js:2908-2944): non-zero categories joined ` · `.
fn subtree_counts(model: &AppModel) -> String {
    let rows = crate::app::flatten_chips(&model.chips);
    let mut needs_input = 0;
    let mut waiting = 0;
    let mut working = 0;
    let mut done = 0;
    let mut failed = 0;
    let mut idle = 0;
    let mut closing = 0;
    for (_, chip) in &rows {
        if chip.closed {
            closing += 1;
            continue;
        }
        use crate::script::ChipDisplayState as S;
        match chip.display_state() {
            S::InputRequired => needs_input += 1,
            S::Waiting => waiting += 1,
            S::Running | S::Tool | S::Thinking | S::Streaming => working += 1,
            S::Done => done += 1,
            S::Error => failed += 1,
            S::Idle => idle += 1,
        }
    }
    let mut parts = Vec::new();
    for (count, label) in [
        (needs_input, "? {} needs input"),
        (waiting, "◔ {} waiting"),
        (working, "◐ {} working"),
        (done, "✓ {} done"),
        (failed, "✗ {} failed"),
        (idle, "○ {} idle"),
        (closing, "⊘ {} closing"),
    ] {
        if count > 0 {
            parts.push(label.replace("{}", &count.to_string()));
        }
    }
    parts.join(" · ")
}

/// The SubTree panel (§2.9): header toggle + depth-first rows with
/// connectors; every row opens its chip's view. Shared by the session and
/// subagent screens (the map is one surface).
fn render_subtree(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    on_subagent: bool,
    hits: &mut Vec<(Rect, Hit)>,
) {
    if area.height == 0 {
        return;
    }
    let arrow = if model.subtree_collapsed {
        "▸"
    } else {
        "▾"
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{arrow} subagents"), theme.gold_style()),
        Span::styled(format!(" — {}", subtree_counts(model)), theme.dim_style()),
    ])];
    let mut row_hits: Vec<(usize, Hit)> = vec![(0, Hit::SubTreeToggle)];
    if !model.subtree_collapsed {
        if on_subagent {
            row_hits.push((lines.len(), Hit::SessionHome));
            lines.push(Line::from(vec![
                Span::styled(" ⌂ ", theme.gold_style()),
                Span::styled(
                    format!("{} — back to the main transcript", model.display_name()),
                    theme.dim_style(),
                ),
            ]));
        }
        let rows = crate::app::flatten_chips(&model.chips);
        let total = rows.len();
        for (index, (depth, chip)) in rows.iter().enumerate() {
            let connector = if chip.closed {
                "⊘"
            } else if index + 1 == total {
                "└─"
            } else {
                "├─"
            };
            let indent = if *depth > 0 {
                " │  ".repeat(*depth)
            } else {
                String::new()
            };
            let display = chip.display_state();
            let glyph = if chip.closed { "⊘" } else { display.glyph() };
            // Sim: `viewing` needs the subagent SCREEN too (tui.js:2925) —
            // a remembered view path must not mark a row on the session.
            let viewing = on_subagent && model.view_path.last() == Some(&chip.agent);
            let activity = if viewing {
                "viewing ←".to_owned()
            } else {
                chip.activity()
            };
            let ink = if chip.closed {
                theme.faint_style()
            } else {
                theme.dim_style()
            };
            let mut spans = vec![
                Span::styled(format!(" {indent}{connector} "), theme.faint_style()),
                Span::styled(format!("{glyph} "), theme.gold_style()),
                Span::styled(
                    format!("{} {}", chip.callsign, chip.hon),
                    if chip.closed {
                        theme.faint_style()
                    } else {
                        theme.bright_style()
                    },
                ),
                Span::styled(format!(" · {} · {}", chip.name, chip.model), ink),
            ];
            if *depth == 0 {
                spans.push(Span::styled(format!(" · {}", chip.device), ink));
            }
            spans.push(Span::styled(format!(" — {activity}"), ink));
            let mut line = Line::from(spans);
            if model.hovered == Some(Hit::ChipRow(chip.agent.clone())) {
                let pad = (area.width as usize).saturating_sub(line.width());
                if pad > 0 {
                    line.push_span(Span::raw(" ".repeat(pad)));
                }
                line = line.style(theme.hover_style());
            }
            row_hits.push((lines.len(), Hit::ChipRow(chip.agent.clone())));
            lines.push(line);
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
    for (offset, hit) in row_hits {
        let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
        if y < area.y + area.height {
            hits.push((
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
                hit,
            ));
        }
    }
}

/// The subagent view (§2.10): breadcrumb head, the chip's OWN transcript,
/// the shared SubTree map, and a composer that steers THIS chip — its
/// question card replaces the composer, but the parent is never blocked
/// (esc always walks back).
fn render_subagent(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let Some(chip) = model.viewed_chip() else {
        // The viewed chip left the tree (5 s removal) — the session view
        // is the honest fallback.
        render_session(model, theme, frame, area, hits);
        return;
    };
    let menu = chip.question_menu();
    let needed_input = menu.map_or_else(
        || composer_height(model),
        |m| {
            u16::try_from(1 + wrapped_menu_body(m, area.width).len() + m.options.len() + 1)
                .unwrap_or(u16::MAX)
        },
    );
    let floor_input = menu.map_or(1, |m| u16::try_from(m.options.len().max(1)).unwrap_or(1));
    // Compact ledger (the session screen's shed order, condensed): gap →
    // subtree → transcript row → header line 2 → rules → header line 1;
    // the input floor never yields.
    let mut gap: u16 = 1;
    let mut transcript_min: u16 = 1;
    let mut header_h: u16 = 2;
    let mut header_rule_h: u16 = 1;
    let mut input_rule_h: u16 = 1;
    let mut subtree_height = subtree_needed(model, true);
    let over = |header_h: u16, rules: u16, extras: u16, area: Rect| {
        area.height.saturating_sub(header_h + rules + extras) < floor_input
    };
    if over(
        header_h,
        header_rule_h + input_rule_h,
        gap + transcript_min + subtree_height,
        area,
    ) {
        subtree_height = 0;
    }
    if over(
        header_h,
        header_rule_h + input_rule_h,
        gap + transcript_min,
        area,
    ) {
        gap = 0;
    }
    if over(header_h, header_rule_h + input_rule_h, transcript_min, area) {
        transcript_min = 0;
    }
    if over(header_h, header_rule_h + input_rule_h, 0, area) {
        header_h = 1;
    }
    if over(header_h, header_rule_h + input_rule_h, 0, area) {
        header_rule_h = 0;
        input_rule_h = 0;
    }
    if over(header_h, 0, 0, area) {
        header_h = 0;
    }
    let chrome = header_h + header_rule_h + input_rule_h;
    let input_avail = area
        .height
        .saturating_sub(chrome + gap + transcript_min + subtree_height);
    let input_height = needed_input
        .min(input_avail)
        .max(floor_input.min(area.height.saturating_sub(chrome)))
        .clamp(1, area.height.max(1));
    let [
        header_area,
        header_rule,
        transcript_area,
        rule_area,
        composer_area,
        subtree_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Length(header_rule_h),
        Constraint::Min(transcript_min),
        Constraint::Length(input_rule_h),
        Constraint::Length(input_height),
        Constraint::Length(subtree_height),
        Constraint::Length(gap),
    ])
    .areas(area);

    // ---- SubHead breadcrumb (tui.js:3430-3483) ----
    let mut crumb_spans: Vec<Span<'_>> = vec![Span::raw(" ")];
    let mut crumb_hits: Vec<(usize, usize, Hit)> = Vec::new(); // (x, width, hit)
    let mut x = 1usize;
    let session_name = model.display_name().to_owned();
    crumb_hits.push((x, session_name.chars().count(), Hit::ChipCrumb(Vec::new())));
    crumb_spans.push(Span::styled(session_name.clone(), theme.dim_style()));
    x += session_name.chars().count();
    for (index, agent) in model.view_path.iter().enumerate() {
        crumb_spans.push(Span::styled(" ▸ ", theme.faint_style()));
        x += 3;
        let Some(hop) = crate::app::find_chip(&model.chips, agent) else {
            continue;
        };
        let last = index + 1 == model.view_path.len();
        if last {
            let label = format!("{} {}", hop.callsign, hop.hon);
            crumb_spans.push(Span::styled(
                label.clone(),
                theme.bright_style().add_modifier(Modifier::BOLD),
            ));
            x += label.chars().count();
        } else {
            let path: Vec<String> = model.view_path[..=index].to_vec();
            crumb_hits.push((x, hop.callsign.chars().count(), Hit::ChipCrumb(path)));
            crumb_spans.push(Span::styled(hop.callsign.clone(), theme.dim_style()));
            x += hop.callsign.chars().count();
        }
    }
    if !chip.closed {
        let close_label = "✕ close";
        crumb_spans.push(Span::raw("  "));
        x += 2;
        crumb_hits.push((
            x,
            close_label.chars().count(),
            Hit::ChipCloseBtn(chip.agent.clone()),
        ));
        let close_hovered = model.hovered == Some(Hit::ChipCloseBtn(chip.agent.clone()));
        crumb_spans.push(Span::styled(
            close_label,
            if close_hovered {
                theme.err_style()
            } else {
                theme.dim_style()
            },
        ));
    }
    // Header line 2: meta + state badge.
    let display = chip.display_state();
    let live_children = crate::app::tree_live_count(&chip.children);
    let badge_label = if chip.closed {
        "⊘ CLOSED".to_owned()
    } else if display == crate::script::ChipDisplayState::Waiting && live_children > 0 {
        format!("◔ WAITING · {live_children} child")
    } else {
        format!("{} {}", display.glyph(), display.label())
    };
    let mut header_bottom = vec![Span::styled(
        format!(
            " {} · {} · {} · {}  ",
            chip.full, chip.name, chip.model, chip.device
        ),
        theme.dim_style(),
    )];
    header_bottom.extend(chip_two_tone(
        badge_label,
        theme.frame_style(),
        theme.gold_style(),
    ));
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(crumb_spans),
            Line::from(header_bottom),
        ]))
        .style(theme.text_style()),
        header_area,
    );
    if header_area.height > 0 {
        for (col, width, hit) in crumb_hits {
            hits.push((
                Rect {
                    x: header_area.x + u16::try_from(col).unwrap_or(u16::MAX),
                    y: header_area.y,
                    width: u16::try_from(width).unwrap_or(1),
                    height: 1,
                },
                hit,
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(header_rule.width as usize),
            theme.frame_style(),
        )),
        header_rule,
    );

    // ---- The chip's transcript (same Entry renderer) + tail lines ----
    let mut lines: Vec<Line<'_>> = Vec::new();
    for entry in chip.transcript.entries() {
        transcript_lines(&mut lines, entry, theme, transcript_area.width);
    }
    if chip.state == crate::script::ChipDisplayState::Thinking {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("● thinking…", theme.gold_style()),
        ]));
    }
    if display == crate::script::ChipDisplayState::Waiting && live_children > 0 {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("◔ waiting on {live_children} child subagent — this session waits too"),
                theme.dim_style(),
            ),
        ]));
    }
    let mut total: u16 = 0;
    for line in &lines {
        let height = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(transcript_area.width);
        total = total.saturating_add(u16::try_from(height).unwrap_or(1));
    }
    let max_scroll = total.saturating_sub(transcript_area.height);
    model.scroll_max.set(max_scroll);
    model
        .scroll_back
        .set(model.scroll_back.get().min(max_scroll));
    let scroll = max_scroll - model.scroll_back.get();
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        transcript_area,
    );

    // ---- Composer / question card ----
    if let Some(menu) = menu {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(rule_area.width as usize),
                theme.warn_style(),
            ))
            .style(theme.text_style()),
            rule_area,
        );
        let footer = format!(
            " ↑↓ select · ⏎ confirm · 1-{} quick · the parent turn is not blocked · esc back to session",
            menu.options.len()
        );
        let (menu_lines, option_rows) =
            menu_block(menu, model.menu_selection, theme, composer_area, &footer);
        frame.render_widget(
            Paragraph::new(Text::from(menu_lines)).style(theme.menu_style()),
            composer_area,
        );
        for (row_offset, option_index) in option_rows {
            let y = composer_area.y + row_offset;
            if y < composer_area.y + composer_area.height {
                hits.push((
                    Rect {
                        x: composer_area.x,
                        y,
                        width: composer_area.width,
                        height: 1,
                    },
                    Hit::MenuOption {
                        menu: menu.id.clone(),
                        index: option_index,
                    },
                ));
            }
        }
    } else {
        render_composer(model, theme, frame, rule_area, composer_area, hits);
    }
    if subtree_height > 0 {
        render_subtree(model, theme, frame, subtree_area, true, hits);
    }
}

/// The aura stage (§3.3): top bar chips, the orb block (a terminal cell
/// cannot animate the sim's CSS orb — a per-state glyph stands in,
/// documented divergence), the two columns, the tail-following transcript
/// and the aura composer.
fn render_aura(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let aura = &model.aura;
    let left_rows = 1 + aura.roster.len().max(1);
    let right_rows = 1 + aura.log.len().min(7);
    let mut columns_h = u16::try_from(left_rows.max(right_rows)).unwrap_or(8).min(8);
    let mut orb_h: u16 = 4;
    // Small windows: columns shed first, then the orb — the composer and
    // transcript row survive.
    if area.height < orb_h + columns_h + 7 {
        columns_h = 0;
    }
    if area.height < orb_h + 7 {
        orb_h = 0;
    }
    let [
        bar_area,
        bar_rule,
        orb_area,
        columns_area,
        transcript_area,
        rule_area,
        composer_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(orb_h),
        Constraint::Length(columns_h),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(composer_height(model)),
        Constraint::Length(1),
    ])
    .areas(area);

    // ---- Top bar: ◉ AURA + chips (engine ⇄ · audio · exit ⤶) ----
    let mut bar = vec![
        Span::raw(" "),
        Span::styled("◉ AURA", theme.gold_style().add_modifier(Modifier::BOLD)),
        Span::styled("  voice session · orchestrator  ", theme.dim_style()),
    ];
    let mut bar_x = Line::from(bar.clone()).width();
    let chip_hit = |bar: &mut Vec<Span<'static>>,
                    bar_x: &mut usize,
                    label: String,
                    hit: Hit,
                    hits: &mut Vec<(Rect, Hit)>,
                    hovered: bool| {
        let chrome = if hovered {
            theme.gold_style()
        } else {
            theme.frame_style()
        };
        let spans = chip_two_tone(label, chrome, theme.gold_style());
        let width = Line::from(spans.clone()).width();
        if bar_area.height > 0 {
            hits.push((
                Rect {
                    x: bar_area.x + u16::try_from(*bar_x).unwrap_or(u16::MAX),
                    y: bar_area.y,
                    width: u16::try_from(width).unwrap_or(1),
                    height: 1,
                },
                hit,
            ));
        }
        bar.extend(spans);
        bar.push(Span::raw(" "));
        *bar_x += width + 1;
    };
    chip_hit(
        &mut bar,
        &mut bar_x,
        format!("engine · {} ⇄", aura.engine_label()),
        Hit::AuraEngine,
        hits,
        model.hovered == Some(Hit::AuraEngine),
    );
    chip_hit(
        &mut bar,
        &mut bar_x,
        if aura.muted {
            "⨂ audio muted".to_owned()
        } else {
            "♪ audio on".to_owned()
        },
        Hit::AuraMute,
        hits,
        model.hovered == Some(Hit::AuraMute),
    );
    chip_hit(
        &mut bar,
        &mut bar_x,
        "exit ⤶".to_owned(),
        Hit::AuraExit,
        hits,
        model.hovered == Some(Hit::AuraExit),
    );
    frame.render_widget(Paragraph::new(Line::from(bar)), bar_area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(bar_rule.width as usize),
            theme.frame_style(),
        )),
        bar_rule,
    );

    // ---- Orb block ----
    if orb_h > 0 {
        let talk_label = if aura.state == crate::script::AuraState::Listening {
            "◉ listening…"
        } else {
            "◉ hold to talk"
        };
        let talk_chrome = if model.hovered == Some(Hit::AuraTalkBtn) {
            theme.gold_style()
        } else {
            theme.frame_style()
        };
        let talk_spans = chip_two_tone(talk_label.to_owned(), talk_chrome, theme.gold_style());
        let talk_width = Line::from(talk_spans.clone()).width();
        let orb_lines = vec![
            Line::from(Span::styled(
                format!("{} {}", aura.state.orb(), aura.state.label()),
                theme.gold_style().add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            Line::from(Span::styled(
                format!(
                    "{} · never writes code — it spawns and steers sessions on your devices",
                    aura.engine_kind()
                ),
                theme.dim_style(),
            ))
            .alignment(Alignment::Center),
            Line::default(),
            Line::from(talk_spans).alignment(Alignment::Center),
        ];
        frame.render_widget(Paragraph::new(Text::from(orb_lines)), orb_area);
        // The centered chip's hit rect (row 4 of the orb block).
        if orb_area.height >= 4 {
            let left_pad = (orb_area.width as usize).saturating_sub(talk_width) / 2;
            hits.push((
                Rect {
                    x: orb_area.x + u16::try_from(left_pad).unwrap_or(0),
                    y: orb_area.y + 3,
                    width: u16::try_from(talk_width).unwrap_or(1),
                    height: 1,
                },
                Hit::AuraTalkBtn,
            ));
        }
    }

    // ---- Two columns: controlled sessions / activity ----
    if columns_h > 0 {
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(columns_area);
        let mut left = vec![Line::from(Span::styled(
            " controlled sessions",
            theme.dim_style(),
        ))];
        if aura.roster.is_empty() {
            left.push(Line::from(Span::styled(
                "  — none yet —",
                theme.faint_style(),
            )));
        }
        for row in &aura.roster {
            left.push(Line::from(vec![
                Span::styled(format!("  {} ", row.state.glyph()), theme.gold_style()),
                Span::styled(
                    format!("{} · {}", row.name, row.device),
                    theme.bright_style(),
                ),
                Span::styled(format!(" — {}", row.activity), theme.dim_style()),
            ]));
        }
        let mut right = vec![Line::from(Span::styled(
            " activity — doing / done",
            theme.dim_style(),
        ))];
        for text in aura.log.iter().rev().take(7).rev() {
            right.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("· {text}"), theme.dim_style()),
            ]));
        }
        frame.render_widget(Paragraph::new(Text::from(left)), left_area);
        frame.render_widget(Paragraph::new(Text::from(right)), right_area);
    }

    // ---- Transcript: last 40 entries, tail-following ----
    let entries = aura.transcript.entries();
    let start = entries.len().saturating_sub(40);
    let mut lines: Vec<Line<'_>> = Vec::new();
    for entry in &entries[start..] {
        match entry {
            TranscriptEntry::User { text, voice, .. } => {
                lines.push(Line::default());
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        if *voice { "◉ " } else { "❯ " },
                        theme.maroon_style().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(text.as_str(), theme.bright_style()),
                ]));
            }
            TranscriptEntry::Note { text } => {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(text.as_str(), theme.dim_style()),
                ]));
            }
            TranscriptEntry::Item(block) => {
                if let TurnItem::AgentMessage { text } = &block.item {
                    lines.push(Line::default());
                    lines.push(Line::from(vec![
                        Span::raw(" "),
                        Span::styled("■ aura", theme.gold_style()),
                        Span::styled(
                            if block.spoken { " · ♪" } else { " · muted" },
                            theme.faint_style(),
                        ),
                    ]));
                    let budget = (transcript_area.width as usize).saturating_sub(3);
                    let body = if block.streaming {
                        wrap_body(&format!("{text}▮"), budget.max(1))
                    } else {
                        wrap_body(text, budget.max(1))
                    };
                    for row in body {
                        lines.push(Line::from(vec![
                            Span::raw(" "),
                            Span::styled("▏ ", theme.rail_style()),
                            Span::styled(row, theme.text_style()),
                        ]));
                    }
                }
            }
            TranscriptEntry::Shell { .. } => {}
        }
    }
    let mut total: u16 = 0;
    for line in &lines {
        let height = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(transcript_area.width);
        total = total.saturating_add(u16::try_from(height).unwrap_or(1));
    }
    let scroll = total.saturating_sub(transcript_area.height);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        transcript_area,
    );

    render_composer(model, theme, frame, rule_area, composer_area, hits);
}

/// The menu's body lines pre-wrapped by display cells into the menu's
/// content width (sim `.iml` white-space: pre-wrap, tui.js:4946).
fn wrapped_menu_body(menu: &haider_protocol::menu::Menu, width: u16) -> Vec<String> {
    let budget = (width as usize).saturating_sub(2).max(1);
    menu.body
        .iter()
        .flat_map(|body_line| wrap_body(body_line, budget))
        .collect()
}

/// The blocking menu's rows within the allocated area (sim InputMenu under
/// the sacred-input law, review r3 P2-1b): warn title with the kind glyph,
/// dim pre-wrapped body, ❯-cursor options, faint answer-by-id hint. Under
/// height pressure rows shed in order — hint first, then body rows from the
/// TOP (the last ones carry the live context), then the title; options
/// NEVER. Returns the rendered lines plus each option's (row offset, option
/// index) for the hit map.
fn menu_block(
    menu: &haider_protocol::menu::Menu,
    selection: usize,
    theme: &Theme,
    area: Rect,
    footer: &str,
) -> (Vec<Line<'static>>, Vec<(u16, usize)>) {
    let allocated = area.height as usize;
    if allocated == 0 {
        return (Vec::new(), Vec::new());
    }
    let selection = selection.min(menu.options.len().saturating_sub(1));
    let mut body_rows = wrapped_menu_body(menu, area.width);
    let needed = 1 + body_rows.len() + menu.options.len() + 1;
    let mut show_title = true;
    let mut show_hint = true;
    let mut over = needed.saturating_sub(allocated);
    if over > 0 {
        show_hint = false;
        over -= 1;
    }
    let shed = over.min(body_rows.len());
    body_rows.drain(..shed); // shed from the top; keep the freshest context
    over -= shed;
    if over > 0 {
        show_title = false;
    }
    // Floor case (review r5 P2-1): fewer rows than options even with all
    // chrome shed — WINDOW the options around the selection (the palette
    // viewport pattern); ⋮ marks hidden neighbors. Every option stays
    // reachable by ↑↓ (the window follows) and answerable by digit.
    let option_slots = allocated
        .saturating_sub(usize::from(show_title) + body_rows.len() + usize::from(show_hint))
        .max(1);
    let window_len = menu.options.len().min(option_slots);
    let start = selection.min(menu.options.len().saturating_sub(window_len));
    let hidden_above = start > 0;
    let hidden_below = start + window_len < menu.options.len();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut option_rows: Vec<(u16, usize)> = Vec::new();
    if show_title {
        let glyph = menu_glyph(menu);
        lines.push(Line::from(vec![Span::styled(
            format!(" {glyph} {}", menu.title),
            theme.warn_style(),
        )]));
    }
    for body_row in body_rows {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(body_row, theme.dim_style()),
        ]));
    }
    for (offset, option) in menu.options.iter().skip(start).take(window_len).enumerate() {
        let index = start + offset;
        let selected = index == selection;
        let cursor = if selected { "❯" } else { " " };
        // The gutter's first cell carries the ⋮ viewport marker on edge
        // rows adjacent to hidden options — none may vanish silently.
        let edge = (offset == 0 && hidden_above) || (offset + 1 == window_len && hidden_below);
        let mut spans = vec![
            Span::styled(if edge { "⋮" } else { " " }, theme.faint_style()),
            Span::styled(format!("{cursor} "), theme.gold_style()),
            Span::styled(
                format!("{}. {}", index + 1, option.label),
                if selected {
                    theme.bright_style()
                } else {
                    theme.menu_style()
                },
            ),
        ];
        option_rows.push((u16::try_from(lines.len()).unwrap_or(u16::MAX), index));
        lines.push(if selected {
            // Selection ground spans the full row (sim `.imo.sel`).
            let pad = (area.width as usize).saturating_sub(Line::from(spans.clone()).width());
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            Line::from(spans).style(theme.selection_style())
        } else {
            Line::from(spans)
        });
    }
    if show_hint {
        lines.push(Line::from(vec![Span::styled(
            footer.to_owned(),
            theme.faint_style(),
        )]));
    }
    (lines, option_rows)
}

/// Sim `MENU_GLYPH` (tui.js:3057) mapped onto the protocol's menu kinds.
/// The command cards (`voice` ◉ / `tools` ⚒) are `Choice` menus — their
/// free-form `origin` tag carries the sim kind (MenuKind is frozen).
fn menu_glyph(menu: &haider_protocol::menu::Menu) -> &'static str {
    use haider_protocol::menu::MenuKind;
    match &menu.kind {
        MenuKind::Recovery { .. } => "⌁",
        MenuKind::Exhausted => "⟳",
        MenuKind::Choice if menu.origin == "voice" => "◉",
        MenuKind::Choice if menu.origin == "tools" => "⚒",
        _ => "?",
    }
}

/// Composer rows currently needed: one per line, capped at
/// [`COMPOSER_MAX_ROWS`] (sim textarea autoGrow, tui.js:2799-2803); beyond
/// the cap the composer scrolls to its tail.
fn composer_height(model: &AppModel) -> u16 {
    let rows = model
        .composer
        .split('\n')
        .count()
        .clamp(1, COMPOSER_MAX_ROWS);
    u16::try_from(rows).unwrap_or(1)
}

/// Keep the editable TAIL of an overlong composer line visible (sim: the
/// textarea scrolls its caret into view): a leading … plus the last cells
/// that fit.
fn tail_window(text: &str, budget: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if budget == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= budget {
        return text.to_owned();
    }
    let keep = budget.saturating_sub(1);
    let mut cells = 0usize;
    let mut reversed: Vec<char> = Vec::new();
    for ch in text.chars().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cells + ch_width > keep {
            break;
        }
        cells += ch_width;
        reversed.push(ch);
    }
    let tail: String = reversed.into_iter().rev().collect();
    format!("…{tail}")
}

/// The gold rule + composer rows on the input ground (sim InputBar,
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
    // Source-region guard (mirrors menu_block): an empty area renders
    // nothing and emits NO hits — never a phantom chip over another row.
    if row_area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(rule_area.width as usize),
            theme.gold_style(),
        ))
        .style(theme.text_style()),
        rule_area,
    );
    let (lines, chip_at) = composer_lines(model, theme, row_area.width, row_area.height);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.input_style()),
        row_area,
    );
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

/// The composer rows (sim InputBar textarea): padded off the frame edge,
/// bold gold ❯ sigil, REAL newlines on their own rows, a horizontal
/// tail-window on any overlong line so the editable end stays visible,
/// typed text bright with a gold block cursor (or the dim placeholder +
/// ghost completion), and the right-aligned `[ ◉ talk ]` chip on the first
/// row.
///
/// `allocated` is the height the layout actually granted: the composer
/// VERTICALLY tail-windows to it (last lines win — the cursor row is
/// sacred at any size, review r3 P2-1a), with a faint ⋮ gutter marker when
/// rows are hidden above. Returns the rows plus the chip's column offset +
/// width.
fn composer_lines<'a>(
    model: &'a AppModel,
    theme: &Theme,
    width: u16,
    allocated: u16,
) -> (Vec<Line<'a>>, Option<(u16, u16)>) {
    let sigil = Span::styled(
        "❯ ",
        theme
            .gold_style()
            .add_modifier(ratatui::style::Modifier::BOLD),
    );
    // Sim `.mic` (tui.js:5467-5489): FRAME chrome, gold label; hover
    // turns the border gold; a live hold shows `◉ listening…`.
    let talk_chrome = if model.hovered == Some(Hit::TalkChip) {
        theme.gold_style()
    } else {
        theme.frame_style()
    };
    let talk_label = if model.listening {
        "◉ listening…"
    } else {
        "◉ talk"
    };
    let chip_spans = chip_two_tone(talk_label.to_owned(), talk_chrome, theme.gold_style());
    let chip_width = Line::from(chip_spans.clone()).width();
    // Right-aligned talk chip when the row leaves room (2-col right pad).
    // Hidden on the subagent and aura screens (sim §4.1 — aura has its own
    // hold-to-talk button).
    let talk_here = matches!(model.screen, Screen::Session | Screen::Launcher);
    let chip_fit = |spans: &mut Vec<Span<'a>>| -> Option<(u16, u16)> {
        if !talk_here {
            return None;
        }
        let used = Line::from(spans.clone()).width();
        let total = used + chip_width + COMPOSER_PAD;
        if (width as usize) > total {
            let filler = width as usize - total;
            spans.push(Span::raw(" ".repeat(filler)));
            spans.extend(chip_spans.clone());
            Some((
                u16::try_from(used + filler).unwrap_or(0),
                u16::try_from(chip_width).unwrap_or(0),
            ))
        } else {
            None
        }
    };

    if model.composer.is_empty() {
        let placeholder = match model.screen {
            Screen::Launcher => PLACEHOLDER_LAUNCHER.to_owned(),
            // Sim SubComposer placeholder (tui.js:3430-3483).
            Screen::Subagent => model.viewed_chip().map_or_else(
                || PLACEHOLDER_SESSION.to_owned(),
                |chip| format!("message {} — steer this subagent · ⏎ send", chip.callsign),
            ),
            // Sim aura composer placeholder (tui.js:3508-3586), verbatim.
            Screen::Aura => {
                "speak or type — e.g. “spin up billing-service on workstation and run its tests”"
                    .to_owned()
            }
            _ => PLACEHOLDER_SESSION.to_owned(),
        };
        let mut spans = vec![
            Span::raw(" ".repeat(COMPOSER_PAD)),
            sigil,
            Span::styled("▮", theme.gold_style()),
            Span::styled(format!(" {placeholder}"), theme.dim_style()),
        ];
        let chip_at = chip_fit(&mut spans);
        return (vec![Line::from(spans)], chip_at);
    }

    let all: Vec<&str> = model.composer.split('\n').collect();
    let window = (allocated.max(1) as usize).min(COMPOSER_MAX_ROWS);
    let skip = all.len().saturating_sub(window);
    let visible = &all[skip..];
    let last = visible.len().saturating_sub(1);
    let mut rows = Vec::new();
    let mut chip_at = None;
    for (index, segment) in visible.iter().enumerate() {
        let first_row = index == 0;
        let last_row = index == last;
        let mut spans = vec![Span::raw(" ".repeat(COMPOSER_PAD))];
        if first_row && skip == 0 {
            spans.push(sigil.clone());
        } else if first_row {
            // Earlier lines are scrolled out above (vertical tail window).
            spans.push(Span::styled("⋮ ", theme.faint_style()));
        } else {
            spans.push(Span::raw("  "));
        }
        // Horizontal tail-window: the cursor/editable end must stay
        // visible on the cursor row; long earlier rows tail-window too.
        let reserve = COMPOSER_PAD + 2 + usize::from(last_row);
        let budget = (width as usize).saturating_sub(reserve);
        spans.push(Span::styled(
            tail_window(segment, budget),
            theme.bright_style(),
        ));
        if last_row {
            spans.push(Span::styled("▮", theme.gold_style()));
            // Inline ghost completion (sim `.ghostline`, tui.js:3028-3034)
            // — palette queries are single-line, so this is also row 0.
            if let Some(ghost) = model.ghost() {
                spans.push(Span::styled(ghost, theme.dim_style()));
                spans.push(Span::styled(" ⇥ tab", theme.faint_style()));
            }
        }
        if first_row {
            chip_at = chip_fit(&mut spans);
        }
        rows.push(Line::from(spans));
    }
    (rows, chip_at)
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
    // A scroll WINDOW over the full match list (sim internal scroll,
    // tui.js:2710-2718) — the reducer keeps the selection inside it.
    let start = model.palette_scroll.min(items.len().saturating_sub(1));
    let width = width as usize;
    let mut lines = vec![Line::styled("─".repeat(width), theme.frame_style())];
    for (offset, item) in items.iter().skip(start).take(PALETTE_MAX_ROWS).enumerate() {
        let selected = start + offset == model.palette_selection;
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

/// Hit regions for the palette's visible rows — offsets skip the top frame
/// rule and never cover the bottom hint line. Each hit carries the row's
/// VALUE (review r2 P2-2): stale maps activate what was drawn or nothing.
fn palette_row_hits(model: &AppModel, area: Rect, hits: &mut Vec<(Rect, Hit)>) {
    let items = model.palette_items();
    if items.is_empty() {
        return;
    }
    let start = model.palette_scroll.min(items.len().saturating_sub(1));
    for (offset, item) in items.iter().skip(start).take(PALETTE_MAX_ROWS).enumerate() {
        let y = area.y + 1 + u16::try_from(offset).unwrap_or(u16::MAX);
        if y < area.y + area.height {
            hits.push((
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
                Hit::PaletteRow(*item),
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
    // The derived WAITING-on-subagents badge overlays plain IDLE (§2.6).
    let (badge, tone) = model.status_badge();
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
    // Sim Badge (tui.js:5541-5547): IDLE wears a FRAME border with dim
    // ink; other outlined states border in their own tone; fills carry the
    // fill on chrome and label alike.
    let badge_chrome = if matches!(tone, crate::projection::BadgeTone::Idle) {
        theme.frame_style()
    } else {
        theme.badge_style(tone)
    };
    left.extend(chip_two_tone(badge, badge_chrome, theme.badge_style(tone)));
    // Sim `.mid`: model · provider, plus the branch name inside a session,
    // plus ` · q:turn` while queue mode holds (tui.js:2840-2842).
    let branch = if model.screen == Screen::Session {
        " · main"
    } else {
        ""
    };
    let queue_tag = if model.queue_mode && model.screen == Screen::Session {
        " · q:turn"
    } else {
        ""
    };
    left.push(Span::styled(
        format!(
            "  {} · {}{branch}{queue_tag}",
            identity.model_short, identity.provider
        ),
        theme.text_style(),
    ));
    left.push(Span::styled(format!("  {meter}  "), theme.dim_style()));
    // Sim `.voice` (tui.js:5511-5520): FRAME border, gold label —
    // `◉ listening…` during a talk hold, the pipeline label otherwise
    // (tui.js:2846-2850); hidden entirely while voice is off.
    if model.listening {
        left.extend(chip_two_tone(
            "◉ listening…".to_owned(),
            theme.frame_style(),
            theme.gold_style(),
        ));
    } else if model.voice.enabled {
        left.extend(chip_two_tone(
            format!("◉ voice · {}", model.voice.bar_label()),
            theme.frame_style(),
            theme.gold_style(),
        ));
    }

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
    // Sim StatusBar (tui.js:5492-5499): TRANSPARENT ground (its frame
    // border-top has no row budget here; the dim ink carries the bar) —
    // the owner's "tan band" was our former bar_bg tint. No tinted rows
    // may bracket the composer.
    frame.render_widget(
        Paragraph::new(Line::from(left)).style(theme.text_style()),
        left_area,
    );
    if right_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(right, theme.dim_style()))
                .alignment(Alignment::Right)
                .style(theme.text_style()),
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
        TranscriptEntry::User {
            text,
            attachments,
            voice,
        } => {
            lines.push(Line::default());
            // Sim UserRow (tui.js:4465-4492): MAROON bold sigil (gold ❯
            // belongs to the composer/sticky only), bright pre-wrap text
            // (multi-line submits keep their newlines), gold pill paste
            // tokens. Voice rows swap the sigil for ◉ and tag ` · spoken`
            // (tui.js:3884-3890).
            let sigil = if *voice { "◉ " } else { "❯ " };
            let last_segment = text.split('\n').count().saturating_sub(1);
            for (index, segment) in text.split('\n').enumerate() {
                let mut spans = if index == 0 {
                    vec![
                        Span::raw(" "),
                        Span::styled(sigil, theme.maroon_style().add_modifier(Modifier::BOLD)),
                    ]
                } else {
                    vec![Span::raw("   ")]
                };
                spans.extend(user_text_spans(segment, theme));
                if index == last_segment {
                    if *attachments > 0 {
                        spans.push(Span::styled(
                            format!(" [+{attachments} attachment(s)]"),
                            theme.dim_style(),
                        ));
                    }
                    if *voice {
                        spans.push(Span::styled(" · spoken", theme.faint_style()));
                    }
                }
                lines.push(Line::from(spans));
            }
        }
        TranscriptEntry::Item(block) => item_lines(lines, block, theme, width),
        TranscriptEntry::Note { text } => {
            // Sim NoteRow (tui.js:4572-4577): dim, indented off the margin.
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(text.as_str(), theme.dim_style()),
            ]));
        }
        TranscriptEntry::Shell { cmd, out } => {
            // Sim ShellRow (tui.js:3910-3918): `$ {cmd}` + output line.
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("$ ", theme.gold_style()),
                Span::styled(cmd.as_str(), theme.bright_style()),
            ]));
            for row in out.split('\n') {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(row, theme.dim_style()),
                ]));
            }
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

/// TRUE pre-wrap by DISPLAY CELLS (sim AgentRow `white-space: pre-wrap`,
/// tui.js:4508-4513 — review r3 P2-5): explicit newlines survive; EVERY
/// space run is preserved exactly (leading indentation, internal runs,
/// trailing whitespace); lines break at the last breakable point within
/// the budget (a space-run boundary), overlong unbreakable runs hard-split
/// at the cell boundary. Tabs expand to a fixed 4 cells — the ONE
/// deliberate divergence from pre-wrap, since a terminal buffer cell
/// cannot render `\t`. Every produced row fits the budget, so ratatui
/// never implicitly wraps and the gold-soft rail survives any width.
fn wrap_body(text: &str, budget: usize) -> Vec<String> {
    let budget = budget.max(1);
    let expanded = text.replace('\t', "    ");
    let mut rows = Vec::new();
    for source in expanded.split('\n') {
        wrap_pre_line(source, budget, &mut rows);
    }
    debug_assert!(
        rows.iter()
            .all(|row| unicode_width::UnicodeWidthStr::width(row.as_str()) <= budget),
        "wrap_body produced a row wider than its budget"
    );
    rows
}

/// Wrap one newline-free line, preserving every space (pre-wrap).
fn wrap_pre_line(line: &str, budget: usize, rows: &mut Vec<String>) {
    use unicode_width::UnicodeWidthChar;
    let mut row = String::new();
    let mut row_width = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(&first) = chars.peek() {
        let is_space = first == ' ';
        // Collect one run (all-spaces or no-spaces).
        let mut run = String::new();
        let mut run_width = 0usize;
        while let Some(&ch) = chars.peek() {
            if (ch == ' ') != is_space {
                break;
            }
            chars.next();
            run.push(ch);
            run_width += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
        if row_width + run_width <= budget {
            row.push_str(&run);
            row_width += run_width;
            continue;
        }
        if is_space {
            // A space run crossing the edge: fill to the edge, break, and
            // carry the REMAINING spaces onto the next row — none are lost.
            let mut remaining = run.chars();
            loop {
                while row_width < budget {
                    if remaining.next().is_none() {
                        break;
                    }
                    row.push(' ');
                    row_width += 1;
                }
                if remaining.as_str().is_empty() {
                    break;
                }
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            continue;
        }
        // A non-space run: break at the run boundary (the last breakable
        // point) when it can fit on a fresh row; hard-split otherwise.
        if run_width <= budget {
            rows.push(std::mem::take(&mut row));
            row = run;
            row_width = run_width;
            continue;
        }
        if row_width > 0 {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        for ch in run.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch_width > budget {
                // Unrepresentable at this width (e.g. CJK beside a rail in
                // a 3-col frame) — dropping is the only honest option that
                // keeps the no-implicit-wrap invariant.
                continue;
            }
            if row_width + ch_width > budget {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push(ch);
            row_width += ch_width;
        }
    }
    // The final row lands even when empty (blank pre-wrap lines keep their
    // rail row) or when it ends in preserved trailing whitespace.
    rows.push(row);
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

/// Dim tool description derived from the call args (sim ToolRow `.desc`).
/// The turn engine carries the sim's desc/meta via the args convention
/// (`{"desc": …, "meta": …}` — §6); legacy scripts carry path/query/glob.
fn tool_desc(args: &serde_json::Value) -> String {
    if let Some(desc) = args.get("desc").and_then(|v| v.as_str()) {
        return desc.to_owned();
    }
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

/// The tool row's meta text (sim `.meta`): `running…` while in progress,
/// else the completed args' meta (tui.js:3901-3909).
fn tool_meta(args: &serde_json::Value, status: haider_protocol::item::ToolStatus) -> String {
    if status == haider_protocol::item::ToolStatus::InProgress {
        return "running…".to_owned();
    }
    args.get("meta")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

fn item_lines<'a>(lines: &mut Vec<Line<'a>>, block: &'a ItemBlock, theme: &Theme, width: u16) {
    match &block.item {
        TurnItem::AgentMessage { text } => {
            // Sim AgentRow (tui.js:4494-4513): the ■ haider header is GOLD;
            // the body indents behind a gold-soft left rail on every
            // wrapped line (manual wrap keeps the rail on continuations).
            // Voice turns tag the header ` · ♪ speaking` (tui.js:3895-3897).
            lines.push(Line::default());
            let mut head = vec![Span::raw(" "), Span::styled("■ haider", theme.gold_style())];
            if block.spoken {
                head.push(Span::styled(" · ♪ speaking", theme.faint_style()));
            }
            lines.push(Line::from(head));
            // Content width = area minus margin+rail; never wider — every
            // produced row fits the frame, so ratatui never implicitly
            // wraps and the rail survives ANY width (review r3 P2-5). At
            // widths ≤ 3 there is no content column at all: the rail
            // stands alone.
            //
            // The streaming cursor is wrapped WITH the text (review r4
            // P2-2): its cell is accounted for by the walker, so a last
            // row that exactly fills the budget pushes the ▮ onto its own
            // RAILED row instead of overflowing rail-less. It is split
            // back out below to keep its gold ink.
            let budget = (width as usize).saturating_sub(3);
            let body = if budget == 0 {
                vec![String::new()]
            } else if block.streaming {
                wrap_body(&format!("{text}▮"), budget)
            } else {
                wrap_body(text, budget)
            };
            let last = body.len().saturating_sub(1);
            for (index, mut row) in body.into_iter().enumerate() {
                let cursor_here =
                    block.streaming && index == last && budget > 0 && row.ends_with('▮');
                if cursor_here {
                    row.pop();
                }
                let mut spans = vec![
                    Span::raw(" "),
                    Span::styled("▏ ", theme.rail_style()),
                    Span::styled(row, theme.text_style()),
                ];
                if cursor_here {
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
            let meta = tool_meta(args, *status);
            if !desc.is_empty() {
                let used = Line::from(spans.clone()).width();
                let reserve = if meta.is_empty() {
                    1
                } else {
                    meta.chars().count() + 3
                };
                let budget = (width as usize).saturating_sub(used + reserve);
                spans.push(Span::styled(
                    format!(" {}", ellipsize(&desc, budget)),
                    theme.dim_style(),
                ));
            }
            if !meta.is_empty() {
                spans.push(Span::styled(format!("  {meta}"), theme.faint_style()));
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
        TurnItem::ContextCompaction {
            tokens_before,
            tokens_after,
            ..
        } => {
            // Sim CompactRow (tui.js:3919-3924), gold: the additive
            // optional token counts render the sim string exactly; items
            // without numbers keep the honest count-free row.
            let text = match (tokens_before, tokens_after) {
                (Some(before), Some(after)) => format!(
                    "⊟ compacted {} → {} · summary retained · originals stay in /tree",
                    fmt_tok(*before),
                    fmt_tok(*after)
                ),
                _ => "⊟ context compacted — summary retained · originals stay in /tree".to_owned(),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(text, theme.gold_style()),
            ]));
        }
        TurnItem::Extension { kind, .. } => {
            lines.push(Line::styled(format!("  ⋯ {kind}"), theme.faint_style()));
        }
    }
}
