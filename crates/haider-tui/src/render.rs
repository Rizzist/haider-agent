//! Screen renderers: pure functions of ([`AppModel`], frame area) → buffer.
//! Testable headlessly via `TestBackend` (research rec 16/18). Every color
//! comes from the theme's style vocabulary — no literals (rec 12).
//! Visual authority: the `/tui` sim — typography, chips, and row shapes are
//! copied from it deliberately.

use crate::app::{AppModel, Hit, LauncherRow, Screen};
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
    // TUI6.1 fix 1: every frame advances the geometry epoch and stamps
    // its composer hits with the NEW value — only hits from the LATEST
    // frame with no intervening resize are consumable (handle_resize
    // bumps the same counter). The Cell is the scroll_max discipline:
    // frame feedback through a shared borrow, never reducer state.
    model
        .geometry_epoch
        .set(model.geometry_epoch.get().wrapping_add(1));
    let theme = model.theme.theme();
    let area = frame.area();
    // Ground the whole frame in the theme bg.
    frame.render_widget(Block::default().style(theme.text_style()), area);

    let mut hits: Vec<(Rect, Hit)> = Vec::new();
    // The status row is the FIRST chrome to yield when a session's sacred
    // input — a blocking menu's options OR the composer's cursor row
    // (review r5 P2-1 + r6 P2-1) — cannot otherwise fit. Minimal need
    // with the full 4-row chrome is status(1) + chrome(4) + floor.
    let status_height: u16 = match model.screen {
        // Aura joins the same ladder (review P1-6): its composer's cursor
        // row is sacred, so the status row yields before the stage can
        // squeeze it out. Minimal need = status + bar + 2 rules + floor.
        Screen::Session | Screen::Subagent | Screen::Aura => {
            let input_floor = match model.screen {
                Screen::Session => model
                    .projection
                    .open_menu()
                    .map_or(1, |menu| menu.options.len()),
                // Title + options since TUI6.2 fix 4 (matches the
                // subagent ledger's floor_input).
                Screen::Subagent => model
                    .viewed_chip()
                    .and_then(crate::app::ChipModel::question_menu)
                    .map_or(1, |menu| menu.options.len() + 1),
                _ => 1,
            };
            u16::from((area.height as usize) >= 1 + 4 + input_floor)
        }
        Screen::Boot
        | Screen::Launcher
        | Screen::Accounts
        | Screen::Providers
        | Screen::Tree
        | Screen::Tools
        | Screen::Hooks => 1,
    };
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(status_height)]).areas(area);
    match model.screen {
        Screen::Boot => render_boot(model, theme, frame, body),
        Screen::Launcher => render_launcher(model, theme, frame, body, &mut hits),
        Screen::Session => render_session(model, theme, frame, body, &mut hits),
        Screen::Tree => render_tree(model, theme, frame, body, &mut hits),
        Screen::Tools => render_tools(model, theme, frame, body),
        Screen::Subagent => render_subagent(model, theme, frame, body, &mut hits),
        Screen::Aura => render_aura(model, theme, frame, body, &mut hits),
        Screen::Accounts => render_accounts(model, theme, frame, body, &mut hits),
        Screen::Providers => render_providers(model, theme, frame, body, &mut hits),
        Screen::Hooks => render_hooks(model, theme, frame, body, &mut hits),
    }
    if model.help_open {
        render_help(theme, frame, body);
        hits.clear();
    }
    if status_height > 0 {
        render_status_bar(model, theme, frame, status, &mut hits);
    }
    // The in-app selection highlight (owner item 9) paints LAST, over
    // every widget on every screen — a screen-space overlay, ground only
    // (the hover-band law: selBg shifts the ground, never the ink).
    if let Some(selection) = &model.selection {
        crate::select::apply_highlight(frame.buffer_mut(), selection, theme);
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
/// The centered launcher column's cell cap — the sim's `min(560px, 92%)`
/// `.recent` block translated to mono cells (see `render_launcher`).
const LAUNCHER_COLS: usize = 70;

/// The `[ ← main ]` chip's cells — the header's second line indents past it.
const BACK_CHIP_COLS: usize = 10;
/// Cells the header reserves around the mark before it may be drawn: the
/// back chip, its margins, and a readable minimum for the info block.
const HEADER_MARK_RESERVED: u16 = 38;
/// The launcher header's reservation — the session's minus the back chip
/// (the launcher has nowhere to go back to): lead cell, margins, and the
/// same readable minimum for the wordmark/info block.
const LAUNCHER_HEADER_RESERVED: u16 = 28;

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

/// Ellipsize a SPAN row to `cap` cells, never a flattened string: mixed
/// rows keep their law of DIM labels beside BRIGHT values (TUI3a) — cutting
/// through a styled span keeps its style, and the `…` wears dim ink.
fn ellipsize_spans<'s>(spans: Vec<Span<'s>>, cap: usize, theme: &Theme) -> Vec<Span<'s>> {
    if Line::from(spans.clone()).width() <= cap {
        return spans;
    }
    let mut used = 0usize;
    let mut kept: Vec<Span<'s>> = Vec::new();
    for span in spans {
        let width = span.content.chars().count();
        if used + width <= cap.saturating_sub(1) {
            used += width;
            kept.push(span);
            continue;
        }
        let room = cap.saturating_sub(1).saturating_sub(used);
        if room > 0 {
            let cut: String = span.content.chars().take(room).collect();
            kept.push(Span::styled(cut, span.style));
        }
        kept.push(Span::styled("…", theme.dim_style()));
        break;
    }
    kept
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
/// The `● thinking…` tail (sim `.thinking`, tui.js:4458-4462): gold ink
/// pulsing on the shared clock, the dot breathing ● ↔ ◌ with it (glyph
/// alternation is a port taste-call — a dimmed cell alone can read flat on
/// low-contrast terminals; one law for the session and chip views).
fn thinking_line(theme: &Theme, phase: u8) -> Line<'static> {
    let dot = if phase.is_multiple_of(2) {
        "●"
    } else {
        "◌"
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("{dot} thinking…"),
            theme.pulse_ink(theme.gold, phase),
        ),
    ])
}

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

/// The wordmark block for a centered surface: the half-block banner when the
/// frame can hold it WHOLE, else the single-line text mark.
///
/// DIGNITY (sanctum rule 2, `mark` module): the art is never clipped or
/// scaled to fit. Below the threshold the mark steps down a tier — exactly
/// as the shahada does — rather than rendering a mangled version of itself.
fn mark_lines(model: &AppModel, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    mark_lines_within(model, theme, width, u16::MAX)
}

/// The mark block, HEIGHT-aware: the banner also steps down to the text mark
/// when the surrounding block would not fit the frame. The art is the FIRST
/// thing to yield — it must never push the sanctum, the wordmark or the
/// recent list off the screen (TUI4 item 6: the banner joins the ledger).
fn mark_lines_within(
    model: &AppModel,
    theme: &Theme,
    width: u16,
    banner_budget: u16,
) -> Vec<Line<'static>> {
    let ink = theme.maroon_style().add_modifier(Modifier::BOLD);
    if crate::mark::banner_fits(width) && banner_budget >= crate::mark::BANNER_ROWS {
        return crate::mark::banner_rows()
            .into_iter()
            .map(|row| Line::styled(row, ink))
            .collect();
    }
    vec![Line::styled(
        SanctumLine::new(model.sanctum_tier).mark().to_owned(),
        ink,
    )]
}

fn render_boot(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let sanctum = SanctumLine::new(model.sanctum_tier);
    // The mark gets breathing room per sim proportions (52px line-height;
    // a terminal cell cannot scale the glyph — noted divergence).
    let mark_block = mark_lines(model, theme, area.width);
    let mark_rows = mark_block.len();
    let mut lines = vec![Line::default()];
    lines.extend(mark_block);
    lines.push(Line::default());
    // UI-themes wave (owner spec §1): the BIG art + shahada ceremony lives
    // on the boot/loading splash — the settled launcher wears the compact
    // header band instead. The ceremony stack moved here verbatim: gold
    // (NOT bold) shahada under the mark, then the gold half-strength
    // dignity rule. Dignity rule 2 travels with it: the sanctum renders
    // whole or not at all, always alone.
    if let Some(text) = sanctum.fit(area.width.saturating_sub(2) as usize) {
        lines.push(Line::styled(text, theme.gold_style()));
        lines.push(Line::default());
    }
    lines.extend([
        Line::styled("────────────────", theme.rule_style()),
        Line::default(),
        Line::styled(spaced_wordmark(), theme.bright_style()),
        // Sim `.sub` on the boot screen is GOLD and PULSES while the
        // harness starts (tui.js:5104-5108, `pulse` 1.4s).
        Line::styled(
            boot_subline(VERSION),
            theme.pulse_ink(theme.gold, model.anim_phase),
        ),
        Line::default(),
    ]);
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
                // The RUNNING check breathes with the `.sub` line — a port
                // taste-extension (the sim's `.cur` is static gold; the ◌
                // marker is the boot screen's one moving part and reads
                // dead without it), on the same shared clock.
                crate::boot::CheckMarker::Current => theme.pulse_ink(theme.gold, model.anim_phase),
                crate::boot::CheckMarker::Pending => theme.faint_style(),
            };
            lines.push(Line::styled(format!("{:<widest$}", row.line()), style));
        }
    }
    // The banner yields before any other boot row does. The rebuild skips
    // exactly the mark block it emitted — when the head was already the
    // one-line text mark this is the identity, never an eaten row.
    if lines.len() > area.height as usize {
        let mut compact = vec![Line::default()];
        compact.extend(mark_lines_within(model, theme, area.width, 0));
        compact.extend(lines.into_iter().skip(1 + mark_rows));
        let _ = centered(frame, area, compact);
        return;
    }
    let (block, _) = centered(frame, area, lines);
    // On a graphics terminal, replace the half-block banner with the crisp
    // حيدر image — but only when the banner (not the one-line text mark) was
    // the tier drawn, so the dignity/step-down rule still governs the footprint.
    if mark_rows == crate::mark::BANNER_ROWS as usize {
        overlay_wordmark(
            model,
            block,
            1,
            crate::mark::BANNER_COLS,
            crate::mark::BANNER_ROWS,
            frame,
        );
    }
}

/// Overlay the graphics wordmark (when the terminal speaks a graphics protocol)
/// over the `cols`×`rows` mark cells at `top_offset` inside the centered
/// `block`, replacing the half-block art with the bundled image. No-op when
/// there is no graphics protocol (the half-block art stays) or the block is too
/// small — the mark then reads identically to before this feature.
fn overlay_wordmark(
    model: &AppModel,
    block: Rect,
    top_offset: u16,
    cols: u16,
    rows: u16,
    frame: &mut Frame<'_>,
) {
    if block.width < cols || block.height < top_offset + rows {
        return;
    }
    let rect = Rect {
        x: block.x + (block.width - cols) / 2,
        y: block.y + top_offset,
        width: cols,
        height: rows,
    };
    draw_wordmark_image(model, rect, frame);
}

/// Draw the graphics wordmark into `rect`, replacing whatever was drawn there.
/// No-op when the terminal has no graphics protocol (`model.wordmark` is None).
fn draw_wordmark_image(model: &AppModel, rect: Rect, frame: &mut Frame<'_>) {
    let mut slot = model.wordmark.borrow_mut();
    let Some(wordmark) = slot.as_mut() else {
        return;
    };
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    // Wipe the cells first (the half-block art the caller drew), then draw the
    // image over them so no block ink bleeds around the aspect-fitted wordmark.
    frame.render_widget(ratatui::widgets::Clear, rect);
    wordmark.render_into(rect, frame.buffer_mut());
}

fn render_launcher(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    // UI-themes wave (owner spec §1): the launcher is a SESSION-SHAPED
    // surface — a compact header band on top (wordmark · version · device,
    // the session header's anatomy without the back chip), a rule, then the
    // top-aligned content column (recent sessions, Aura/Accounts/Peers,
    // shellout), then the palette directly above the gold-ruled composer.
    // The BIG centered mark and the shahada belong to the boot splash
    // alone now — people open many terminals; the settled launcher scans
    // like a working surface, not a poster.
    let palette = if model.palette_open() {
        palette_block(model, theme, area.width)
    } else {
        Vec::new()
    };
    let mut palette_height = u16::try_from(palette.len()).unwrap_or(0);
    // Sacred-input ledger (review r3 P2-1a, launcher form; r6 P2-1: same
    // shed ladder as the session): the composer grows up to its need but
    // tail-windows to whatever the height allows — the cursor row is never
    // hidden. Shed order under pressure, first to yield first: content →
    // the closing-rule row (the gap) → header line 2 → header rule → input
    // rule → header line 1 — the composer's cursor row never.
    let needed = composer_height(model, area.width);
    let mut header_h: u16 = 2;
    let mut header_rule_h: u16 = 1;
    let mut content_min: u16 = 1;
    let mut rule_h: u16 = 1;
    // TUI6.2 fix 6 (review r2 finding 6): the closing-rule row (the gap,
    // TUI5's net-zero trick) is DERIVED from `band_rule_reserve` — the
    // function is the runtime authority here, not a debug-only tie. The
    // content column yields first when keeping it would starve the
    // reserved rule (r1's launcher 90×4), then the header band sheds
    // line 2 → rule → line 1: the band triple (top rule · composer ·
    // closing rule) outlives the WHOLE header, so the launcher's floor
    // frame is the same triple the reviewer pinned before the band
    // existed (launcher 90×4).
    let starves_rule = |header_h: u16, header_rule_h: u16, content_min: u16, rule_h: u16| {
        band_rule_reserve(
            area.height,
            header_h + header_rule_h + content_min + rule_h + 1,
            rule_h,
        ) == 0
    };
    if starves_rule(header_h, header_rule_h, content_min, rule_h) {
        content_min = 0;
    }
    if starves_rule(header_h, header_rule_h, content_min, rule_h) {
        header_h = 1;
    }
    if starves_rule(header_h, header_rule_h, content_min, rule_h) {
        header_rule_h = 0;
    }
    if starves_rule(header_h, header_rule_h, content_min, rule_h) {
        header_h = 0;
    }
    let gap = band_rule_reserve(
        area.height,
        header_h + header_rule_h + content_min + rule_h + 1,
        rule_h,
    );
    let chrome = header_h + header_rule_h + content_min + gap;
    let mut input_avail = area.height.saturating_sub(chrome + rule_h);
    if input_avail < 1 {
        rule_h = 0;
        input_avail = area.height.saturating_sub(chrome);
    }
    if input_avail < 1 {
        header_h = 0;
        header_rule_h = 0;
        input_avail = area.height;
    }
    let composer_rows = needed.min(input_avail).clamp(1, area.height.max(1));
    let fixed = header_h + header_rule_h + content_min + gap + rule_h + composer_rows;
    if palette_height > area.height.saturating_sub(fixed) {
        palette_height = 0;
    }
    // TUI5 item 1b: the launcher band CLOSES — the owner's "line under
    // it". Sim geometry wins the anatomy: the launcher's InputBar is
    // followed DIRECTLY by the StatusBar, whose `border-top: frame`
    // (tui.js:5497) is that line — no pad row (that pad belongs to the
    // session band, where the SubTree follows instead). The old blank gap
    // row becomes the rule, net zero rows; it sheds first under pressure
    // exactly as the gap did.
    let band_rule_h = gap;
    let [
        header_area,
        header_rule,
        content_area,
        palette_area,
        rule_area,
        composer_area,
        band_rule_area,
    ] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Length(header_rule_h),
        Constraint::Min(content_min),
        Constraint::Length(palette_height),
        Constraint::Length(rule_h),
        Constraint::Length(composer_rows),
        Constraint::Length(band_rule_h),
    ])
    .areas(area);

    let sanctum = SanctumLine::new(model.sanctum_tier);
    let identity = &model.identity;
    // ---- The header band: the session header's typography without the
    // back chip. Line 1: mark · bold GOLD product · dim version/moniker ·
    // BRIGHT device (the owner's header contract: wordmark, version,
    // device). Line 2: the identity info — DIM labels beside BRIGHT values
    // (TUI3a) — with the working dir, ellipsized to the band.
    let mark_ink = theme.maroon_style().add_modifier(Modifier::BOLD);
    let mut header_top: Vec<Span<'_>> = vec![Span::raw(" ")];
    let mut header_bottom: Vec<Span<'_>> = vec![Span::raw(" ")];
    let header_art = crate::mark::header_fits(area.width, LAUNCHER_HEADER_RESERVED);
    if header_art {
        // The compact cut of the big art: the SAME GeezaPro-derived حيدر
        // letterforms at header scale (16×2 — `mark::HEADER`), spanning
        // both band lines exactly as it does beside a session's info block.
        let rows = crate::mark::header_rows();
        header_top.push(Span::styled(rows[0].clone(), mark_ink));
        header_top.push(Span::raw("  "));
        header_bottom.push(Span::styled(rows[1].clone(), mark_ink));
        header_bottom.push(Span::raw("  "));
    } else {
        header_top.push(Span::styled(format!("{}  ", sanctum.mark()), mark_ink));
        header_bottom.push(Span::raw(" ".repeat(sanctum.mark().chars().count() + 2)));
    }
    header_top.push(Span::styled(
        "haider",
        theme
            .gold_style()
            .add_modifier(ratatui::style::Modifier::BOLD),
    ));
    header_top.push(Span::styled(
        format!(" {}", launcher_subline(VERSION)),
        theme.dim_style(),
    ));
    header_top.push(Span::styled(" · ", theme.dim_style()));
    header_top.push(Span::styled(identity.device.clone(), theme.bright_style()));
    header_bottom.extend([
        Span::styled("provider ", theme.dim_style()),
        Span::styled(identity.provider.clone(), theme.bright_style()),
        Span::styled(" · model ", theme.dim_style()),
        Span::styled(identity.model_short.clone(), theme.bright_style()),
        Span::styled(" · account ", theme.dim_style()),
        Span::styled(identity.account.clone(), theme.bright_style()),
        Span::styled(" · dir ", theme.dim_style()),
        Span::styled(model.launcher_dir.clone(), theme.bright_style()),
        Span::styled(" · mesh ", theme.dim_style()),
        Span::styled("off", theme.bright_style()),
    ]);
    let band_cap = area.width as usize;
    let header_top = ellipsize_spans(header_top, band_cap, theme);
    let header_bottom = ellipsize_spans(header_bottom, band_cap, theme);
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
    // Replace the half-block header mark with the crisp حيدر image on a
    // graphics terminal — same 16×2 footprint at the band's lead cell.
    if header_art && header_area.height >= crate::mark::HEADER_ROWS {
        draw_wordmark_image(
            model,
            Rect {
                x: header_area.x + 1,
                y: header_area.y,
                width: crate::mark::HEADER_COLS,
                height: crate::mark::HEADER_ROWS,
            },
            frame,
        );
    }
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(header_rule.width as usize),
            theme.frame_style(),
        )),
        header_rule,
    );

    // ---- The content column: TOP-ALIGNED under the band (the session
    // surface's reading order), one breathing row first. The shared-column
    // trick survives: every row pads to the widest so hover bands span the
    // block; the block hugs the band's left edge instead of centering.
    let mut lines: Vec<Line<'_>> = vec![Line::default()];
    // Sim `.recent { width: min(560px, 92%) }` (tui.js:4331-4334) at 12.5px
    // mono ≈ 7.5px/cell → ~74 cells. Capped at LAUNCHER_COLS so a wide
    // terminal keeps the sim's proportions instead of letting the block span
    // the frame (owner item 5 — his screenshot was ~165 cols).
    let area_cap = (area.width as usize)
        .saturating_sub(4)
        .clamp(10, LAUNCHER_COLS);
    // Sim `.rhead` verbatim + gold `· N running` (`.livehd`). The count is
    // `sessionBusy` = live subagents OR a busy run state (tui.js:789-792).
    // TUI4c: rows derive from the LIVE session map — seeds and user
    // sessions alike; a busy background session shows its liveness HERE
    // (gold pulsing-dot semantics), never in the global badge (item 12).
    let running = model.sessions.iter().filter(|s| s.busy()).count();
    // TUI4d item 14 — every row of the block leads with ONE rail cell so
    // the sim's `.rail` sliver has a home (tui.js:4370-4394: absolute in
    // the row's left padding, transparent unless running). Idle rows and
    // the head/extra rows carry a space there — the shared left edge law
    // holds because EVERY row pays the cell.
    let mut rhead = vec![
        Span::raw(" "),
        Span::styled(
            "recent sessions — click to attach · /sessions for all",
            theme.dim_style(),
        ),
    ];
    if running > 0 {
        rhead.push(Span::styled(
            format!(" · {running} running"),
            theme.gold_style(),
        ));
    }
    // Pass 1: build every row's spans, metas ellipsized to the frame cap.
    // HOW MANY rows is the MODEL's policy, not the renderer's — the sim's
    // three in demo, the reachable digit span live (see
    // `AppModel::launcher_rows`). Render stays source-agnostic: it asks.
    let mut recent: Vec<(Vec<Span<'_>>, Option<Hit>)> = vec![(rhead, None)];
    for entry in model.sessions.iter().take(model.launcher_rows()) {
        // Sim row anatomy (tui.js:3252-3277): rail · dot (ok; gold
        // PULSING when running, tui.js:4392-4394) · name BRIGHT bold ·
        // `▸ head hon` DIM (.hd) · meta DIM ellipsized. No digit prefix
        // (the 1-3 keys stay as silent bindings).
        let busy = entry.busy();
        let live = entry.live();
        let errored = entry.errored();
        let (rail, dot, dot_style) = if busy {
            (
                // The gold→maroon→gold gradient crossing the sliver
                // (railShimmer, 1.8s — three phases of the shared clock).
                Span::styled("▎", theme.rail_shimmer_style(model.anim_phase)),
                "◉",
                theme.pulse_ink(theme.gold, model.anim_phase),
            )
        } else if errored {
            // The turn DIED — the badge's ✗ in the badge's warn tone,
            // still (nothing pulses for a corpse). Owner report, W5f-0.
            (Span::raw(" "), "✗", theme.warn_style())
        } else {
            (Span::raw(" "), "●", theme.ok_style())
        };
        let name = entry.name.clone().unwrap_or_else(|| "session".to_owned());
        let mut spans = vec![
            rail,
            Span::styled(format!("{dot} "), dot_style),
            Span::styled(
                name.clone(),
                theme
                    .bright_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(
                format!(" ▸ {} {}", entry.head.0, entry.head.1),
                theme.dim_style(),
            ),
        ];
        // Sim `.live` (tui.js:3259-3266): live subagents name themselves;
        // a busy session WITHOUT chips falls back to `running… · `.
        if live > 0 {
            let plural = if live > 1 { "s" } else { "" };
            spans.push(Span::styled(
                format!("  {live} live subagent{plural} ·"),
                theme.gold_style(),
            ));
        } else if busy {
            spans.push(Span::styled("  running… ·", theme.gold_style()));
        } else if errored {
            spans.push(Span::styled("  errored ·", theme.warn_style()));
        }
        let turns = entry.turns();
        // Sim renders the blurb segment only when a blurb exists
        // (tui.js:3267 `s.blurb ? … : null`).
        let blurb = entry
            .title
            .as_ref()
            .map(|title| format!(" “{title}” ·"))
            .unwrap_or_default();
        let meta = format!(
            "{blurb} {} · {} {} · {} tok · {} · {} · {}",
            // DERIVED (B2b): the seed static plus daemon-installed named
            // branches — the launcher aggregate counts all branches.
            if entry.branches() > 1 {
                format!("{} branches", entry.branches())
            } else {
                "1 branch".to_owned()
            },
            turns,
            if turns == 1 { "turn" } else { "turns" },
            fmt_tok(entry.projection.context_tokens()),
            entry.model_short,
            entry.device,
            entry.ago
        );
        // Sim `.meta`: ellipsized into the column, never clipped.
        let meta_budget = area_cap.saturating_sub(Line::from(spans.clone()).width());
        spans.push(Span::styled(
            ellipsize(&meta, meta_budget),
            theme.dim_style(),
        ));
        recent.push((spans, Some(Hit::AttachSession(entry.id.clone()))));
    }
    // Sim `.aurarow` metas VERBATIM (tui.js:3278-3300) — the earlier port
    // abbreviated all three (review P2-8). The Accounts/Peers counts come
    // from the sim's own seed lists: 7 credentials across 5 providers
    // (tui.js:146-154), and 3 host-capable nodes of the 4 seeded — the
    // `shell` rung does not host (tui.js:165-174).
    for (row, glyph, name, blurb) in [
        (
            LauncherRow::Aura,
            "◉",
            "Aura",
            "voice session · orchestrator — spawns & steers sessions across devices, never codes"
                .to_owned(),
        ),
        (
            LauncherRow::Accounts,
            "⚿",
            "Accounts",
            format!(
                "provider credentials — OAuth & API keys, harness-owned · {} across {} providers",
                crate::mock::SEED_ACCOUNTS,
                crate::mock::SEED_ACCOUNT_PROVIDERS
            ),
        ),
        (
            LauncherRow::Peers,
            "⇄",
            "Peers",
            format!(
                "reachability ladder — enrolled peers · sponsored SSH nodes · shell targets · {} host-capable",
                crate::mock::SEED_HOST_CAPABLE_PEERS
            ),
        ),
    ] {
        // Sim `.aurarow` (tui.js:4403-4413): gold glyph, gold name, dim
        // meta — its frame border-top rule is inserted in pass 2. The
        // leading cell keeps the block's shared left edge beside the
        // session rows' rail column (the sim's aurarow has no rail).
        let spans = vec![
            Span::raw(" "),
            Span::styled(format!("{glyph} "), theme.gold_style()),
            Span::styled(name, theme.gold_style()),
            Span::styled(
                ellipsize(
                    &format!("  {blurb}"),
                    area_cap.saturating_sub(name.chars().count() + 3),
                ),
                theme.dim_style(),
            ),
        ];
        recent.push((spans, Some(Hit::ExtraRow(row))));
    }
    // Pass 2: one shared column = widest row, capped by the frame.
    let column = recent
        .iter()
        .map(|(spans, _)| Line::from(spans.clone()).width())
        .max()
        .unwrap_or(10)
        .clamp(10, area_cap);
    let mut sample_rows: Vec<(usize, haider_protocol::ids::SessionId)> = Vec::new();
    let mut extra_rows: Vec<(usize, LauncherRow)> = Vec::new();
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
        match &hit {
            Some(Hit::AttachSession(id)) => sample_rows.push((lines.len(), id.clone())),
            Some(Hit::ExtraRow(row)) => extra_rows.push((lines.len(), *row)),
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
                // The block's rail-cell lead (shared left edge).
                Span::styled(" $ ", theme.gold_style()),
                Span::styled(cmd.clone(), theme.bright_style()),
            ],
            column,
        )));
        for row in out.split('\n') {
            lines.push(Line::from(pad_spans_to(
                vec![Span::styled(format!(" {row}"), theme.dim_style())],
                column,
            )));
        }
    }
    // Top-aligned under the band: rows render from the header rule down
    // and CLIP at the bottom under pressure (the shed ladder already gave
    // the content column up first; the composer band below stays sacred).
    // No centering, no compaction — a hit row IS its painted row (the
    // W5g-7 hover-offset class of bug is unrepresentable here).
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.text_style()),
        content_area,
    );
    let visible = |row: usize| (row < content_area.height as usize).then_some(row);
    for (row, id) in sample_rows {
        if let Some(row) = visible(row) {
            hits.push((
                row_rect(content_area, content_area.y, row),
                Hit::AttachSession(id),
            ));
        }
    }
    for (row, which) in extra_rows {
        if let Some(row) = visible(row) {
            hits.push((
                row_rect(content_area, content_area.y, row),
                Hit::ExtraRow(which),
            ));
        }
    }
    if palette_height > 0 {
        frame.render_widget(Paragraph::new(Text::from(palette)), palette_area);
        palette_row_hits(model, palette_area, hits);
    }
    if theme_picker_showing(model) {
        render_theme_picker(model, theme, frame, rule_area, composer_area, hits);
    } else {
        render_composer(model, theme, frame, rule_area, composer_area, hits);
    }
    // TUI5 item 1b: the frame rule under the launcher band (the sim
    // StatusBar's border-top — the owner's missing "line under it").
    if band_rule_h > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(band_rule_area.width as usize),
                theme.frame_style(),
            ))
            .style(theme.text_style()),
            band_rule_area,
        );
    }
}

/// `/accounts` (W5d) — the sim's harness-owned credential list, 1:1
/// hierarchy (tui.js:3588-3688): head · optional action message · provider
/// groups (base URL on the group header) · rows with ●/○ + AUTH_LABEL +
/// identity + status + "in use" · ONE global add row after all groups ·
/// hints. The dot NEVER moves at render time — rows are daemon truth.
/// The custom-provider card lines (add, edit, or the HF preset) —
/// shared by the /accounts and /providers renderers (W10b: the card
/// opens from either screen and must be VISIBLE from either).
fn push_custom_card_lines<'a>(model: &'a AppModel, theme: &Theme, lines_out: &mut Vec<Line<'a>>) {
    if let Some(card) = &model.custom_add {
        lines_out.push(Line::from(vec![
            Span::styled("◉ ", theme.gold_style()),
            Span::styled(
                "add a custom provider — OpenAI-compatible",
                theme.warn_style(),
            ),
        ]));
        if model.mode.fabricates_locally() {
            for line in [
                "  base URL + key — works with any OpenAI-compatible server",
                "  vLLM · Ollama · LM Studio · LiteLLM · TGI · your own gateway",
                "  capability probed from /v1/models · stored in the vault by alias",
            ] {
                lines_out.push(Line::styled(line, theme.dim_style()));
            }
            lines_out.push(Line::styled(
                "  [1] add http://127.0.0.1:8000/v1 (demo) · [2] cancel",
                theme.gold_style(),
            ));
            lines_out.push(Line::styled(
                "  accounts.add over RPC — the ADE renders this same card",
                theme.faint_style(),
            ));
        } else {
            let editing = matches!(card.phase, crate::app::CustomPhase::Editing { .. });
            if let crate::app::CustomPhase::Editing { error: Some(error) } = &card.phase {
                lines_out.push(Line::styled(format!("  ✗ {error}"), theme.err_style()));
            }
            let caret = |focused: bool| if focused { "▏" } else { "" };
            lines_out.push(Line::styled(
                format!(
                    "  name   ❯ {}{}",
                    card.name,
                    caret(editing && card.focus == crate::app::CustomField::Name)
                ),
                theme.text_style(),
            ));
            lines_out.push(Line::styled(
                format!(
                    "  origin ❯ {}{}",
                    card.origin,
                    caret(editing && card.focus == crate::app::CustomField::Origin)
                ),
                theme.text_style(),
            ));
            lines_out.push(Line::styled(
                format!(
                    "  model  ❯ {}{}",
                    card.model,
                    caret(editing && card.focus == crate::app::CustomField::Model)
                ),
                theme.text_style(),
            ));
            if editing {
                lines_out.push(Line::styled(
                    "  the model the server serves (e.g. llama3.1:8b) · the key is asked next",
                    theme.dim_style(),
                ));
                lines_out.push(Line::styled(
                    "  ⏎ create · tab field · esc cancel",
                    theme.gold_style(),
                ));
            } else {
                lines_out.push(Line::styled(
                    "  committing the provider…",
                    theme.pulse_ink(theme.gold, model.anim_phase),
                ));
            }
        }
        lines_out.push(Line::raw(""));
    }
}

/// The account add-button rows (OAuth/API/Kimi/Gemini/HF/custom) with
/// per-button relative hit rects — shared by /accounts and /providers
/// (owner ask: providers should offer the SAME add options in place).
/// B6b: the two new providers slot between the sim rows and the HF/custom
/// tail — OpenAI stays first, Custom stays last (sim button order,
/// tui.js:3621-3628, preserved at the edges).
fn push_account_add_buttons<'a>(
    model: &'a AppModel,
    theme: &Theme,
    lines_out: &mut Vec<Line<'a>>,
    rects_out: &mut Vec<(usize, u16, u16, Hit)>,
) {
    let rows: [&[(&str, crate::app::AccountAddKind)]; 3] = [
        &[
            ("+ OpenAI (OAuth)", crate::app::AccountAddKind::OpenAiOAuth),
            (
                "+ Anthropic (OAuth)",
                crate::app::AccountAddKind::AnthropicOAuth,
            ),
            ("+ OpenAI (API)", crate::app::AccountAddKind::OpenAiApi),
        ],
        &[
            (
                "+ Anthropic (API)",
                crate::app::AccountAddKind::AnthropicApi,
            ),
            ("+ Kimi (OAuth)", crate::app::AccountAddKind::KimiOAuth),
            ("+ Gemini (API)", crate::app::AccountAddKind::GeminiApi),
        ],
        &[
            ("+ HuggingFace", crate::app::AccountAddKind::HuggingFace),
            (
                "+ Custom (OpenAI-compatible)",
                crate::app::AccountAddKind::Custom,
            ),
        ],
    ];
    for chunk in rows {
        // One hit per BUTTON: per-button column rects, hover-aware (owner
        // ask): the hovered button renders on the hover band.
        let mut spans: Vec<Span<'_>> = Vec::new();
        let mut offset = 0u16;
        for &(label, kind) in chunk {
            let hit = Hit::AccountAdd(kind);
            let hovered = model.hovered.as_ref() == Some(&hit);
            let width = label.chars().count() as u16 + 2;
            rects_out.push((lines_out.len(), offset, width, hit));
            spans.push(Span::styled(
                format!("[{label}]"),
                if hovered {
                    theme.hover_style().patch(theme.gold_style())
                } else {
                    theme.gold_style()
                },
            ));
            spans.push(Span::raw("  "));
            offset += width + 2;
        }
        lines_out.push(Line::from(spans));
    }
}

fn render_accounts(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let mut lines: Vec<Line<'_>> = Vec::new();
    // (line index, hit) pairs resolved to rects after layout.
    let mut line_hits: Vec<(usize, Hit)> = Vec::new();
    // Add-row buttons: (footer line, column offset, width, hit).
    let mut add_button_rects: Vec<(usize, u16, u16, Hit)> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            "ACCOUNTS",
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            " — auth is harness-owned · the ADE reads this list",
            theme.dim_style(),
        ),
    ]));
    if let Some(message) = &model.accounts.message {
        lines.push(Line::styled(message.clone(), theme.gold_style()));
    }
    lines.push(Line::raw(""));

    // Provider groups in FIRST-SEEN order (sim: Set insertion order over
    // the account list, tui.js:3593).
    let mut providers: Vec<&str> = Vec::new();
    for row in &model.accounts.rows {
        if !providers.contains(&row.provider.as_str()) {
            providers.push(&row.provider);
        }
    }
    if model.accounts.rows.is_empty() {
        lines.push(Line::styled(
            "  no accounts yet — add one below, or /login <provider> api",
            theme.dim_style(),
        ));
    }
    for provider in providers {
        // Sim tui.js:3596-3599: the group header shows the FIRST base URL
        // any of its accounts carries.
        let base_url = model
            .accounts
            .rows
            .iter()
            .find(|row| row.provider == provider && row.base_url.is_some())
            .and_then(|row| row.base_url.clone());
        let mut header = vec![Span::styled(
            provider.to_owned(),
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        )];
        if let Some(url) = base_url {
            header.push(Span::styled(format!(" · {url}"), theme.faint_style()));
        }
        lines.push(Line::from(header));
        for (index, row) in model
            .accounts
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.provider == provider)
        {
            let selected = row.selected;
            let pending = model
                .accounts
                .pending_select
                .as_deref()
                .is_some_and(|alias| alias == row.alias);
            let dot_style = if selected {
                theme.gold_style()
            } else {
                theme.dim_style()
            };
            let status_text = match row.status {
                haider_protocol::credential::CredentialStatus::Ok => "active".to_owned(),
                haider_protocol::credential::CredentialStatus::Limited { .. } => {
                    "rate-limited".to_owned()
                }
                haider_protocol::credential::CredentialStatus::Expired => "expired".to_owned(),
                haider_protocol::credential::CredentialStatus::Revoked => "revoked".to_owned(),
            };
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!("{} ", if selected { "●" } else { "○" }), dot_style),
                Span::styled(
                    row.alias.clone(),
                    theme
                        .bright_style()
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
                Span::styled(
                    format!(" [{}]", crate::app::auth_label(row.method)),
                    if row.method == haider_protocol::credential::AuthMethod::OAuth {
                        theme.gold_style()
                    } else {
                        theme.dim_style()
                    },
                ),
                Span::styled(
                    format!(" · {} · {status_text}", row.identity),
                    theme.dim_style(),
                ),
            ];
            if selected {
                spans.push(Span::styled(" · in use", theme.gold_style()));
            }
            if pending {
                // Visible in-flight feedback WITHOUT moving the dot
                // (forbidden optimism, report §5.1).
                spans.push(Span::styled(
                    " …",
                    theme.pulse_ink(theme.gold, model.anim_phase),
                ));
            }
            let mut line = Line::from(spans);
            // Hover chrome: the mouse band (owner ask) OR the keyboard
            // cursor — the same hover band either way.
            let row_hit = Hit::AccountRow(row.alias.clone());
            if model.accounts.cursor == index || model.hovered.as_ref() == Some(&row_hit) {
                line = hover_band(line, true, area.width, theme);
            }
            line_hits.push((lines.len(), row_hit));
            lines.push(line);
        }
    }

    // The ONE global add row (sim tui.js:3621-3628) + hints — anchored to
    // the BOTTOM of the screen (owner ask 2026-07-30: with few or zero
    // accounts the flowed position sat awkwardly high). `footer_lines`
    // renders at area.bottom − its height; the list keeps the top.
    let mut footer_lines: Vec<Line<'_>> = Vec::new();
    // The masked key card, when open on THIS screen — the `+ … (API)`
    // buttons and the custom card's chain both land it here, and the
    // composer band that usually hosts it does not exist on /accounts.
    // Without this block the card is an INVISIBLE total-modal trap (the
    // W5g-5 live probe found it: keys vanished into a card no frame
    // drew).
    if let Some(card) = model.login.as_ref() {
        footer_lines.extend(login_lines(card, theme, area.width));
        footer_lines.push(Line::raw(""));
    }
    // The OAuth add card (W5e-1, sim authFlow MenuBox tui.js:3629-3682) —
    // rendered with the bottom chrome, above the add row.
    if let Some(card) = &model.oauth_add {
        // B6b: name the flow honestly — Kimi is a device-code grant, not a
        // loopback PKCE exchange (the daemon owns both; the card only
        // reports).
        let flow = if card.provider == "kimi-oauth" {
            "OAuth (device code)"
        } else {
            "OAuth (loopback PKCE)"
        };
        footer_lines.push(Line::from(vec![
            Span::styled("◉ ", theme.gold_style()),
            Span::styled(
                format!("authorize {} — {flow}", card.title),
                theme.warn_style(),
            ),
        ]));
        match &card.phase {
            crate::app::OAuthAddPhase::Starting => {
                footer_lines.push(Line::styled(
                    "  starting the loopback flow…",
                    theme.dim_style(),
                ));
            }
            crate::app::OAuthAddPhase::WaitingBrowser { origin, .. } => {
                footer_lines.push(Line::styled(
                    format!(
                        "  your browser opened {} — approve there; tokens land in the vault",
                        if origin.is_empty() {
                            "the provider"
                        } else {
                            origin
                        }
                    ),
                    theme.dim_style(),
                ));
                footer_lines.push(Line::styled(
                    format!("  alias: {} · usage billed to the subscription", card.alias),
                    theme.faint_style(),
                ));
                footer_lines.push(Line::from(vec![Span::styled(
                    "  [1] open the link again · [2] cancel",
                    theme.gold_style(),
                )]));
            }
            crate::app::OAuthAddPhase::WaitingDevice { url, .. } => {
                // Device-honest copy (B2b-m3 polish c): a device grant has
                // no loopback listening — the user enters the code at the
                // verification URL and the daemon polls until approval.
                footer_lines.push(Line::styled(
                    format!(
                        "  enter the code at {} — the daemon polls until you approve",
                        if url.is_empty() {
                            "the verification page"
                        } else {
                            url
                        }
                    ),
                    theme.dim_style(),
                ));
                footer_lines.push(Line::styled(
                    format!("  alias: {} · usage billed to the subscription", card.alias),
                    theme.faint_style(),
                ));
                footer_lines.push(Line::from(vec![Span::styled(
                    "  [1] open the link again · [2] cancel",
                    theme.gold_style(),
                )]));
            }
            crate::app::OAuthAddPhase::Exchanging => {
                footer_lines.push(Line::styled(
                    "  approved — exchanging the code…",
                    theme.pulse_ink(theme.gold, model.anim_phase),
                ));
            }
            crate::app::OAuthAddPhase::Adding => {
                footer_lines.push(Line::styled(
                    "  committing the account…",
                    theme.pulse_ink(theme.gold, model.anim_phase),
                ));
            }
            crate::app::OAuthAddPhase::Failed { message } => {
                footer_lines.push(Line::styled(format!("  ✗ {message}"), theme.err_style()));
                // §5.3 collision recovery: the alias is editable in place
                // and ⏎ retries the flow under it (digits are alias
                // characters, so no `[1]`/`[2]` key map here).
                footer_lines.push(Line::styled(
                    format!("  alias ❯ {}▏", card.alias),
                    theme.text_style(),
                ));
                footer_lines.push(Line::styled(
                    "  ⏎ try again with this alias · esc close",
                    theme.gold_style(),
                ));
            }
        }
        footer_lines.push(Line::raw(""));
    }
    // The `+ Custom (OpenAI-compatible)` card (W5g-4; sim MenuBox
    // tui.js:3629-3682). Demo = the sim's verbatim fabrication card; live
    // = the editable name/origin fields (the provider.configure front
    // door).
    push_custom_card_lines(model, theme, &mut footer_lines);
    push_account_add_buttons(model, theme, &mut footer_lines, &mut add_button_rects);
    footer_lines.push(Line::raw(""));
    footer_lines.push(Line::styled(
        "click an account to make it active · + adds via OAuth / API · x removes · esc back",
        theme.faint_style(),
    ));

    let footer_height = footer_lines.len() as u16;
    let footer_top = area.y + area.height.saturating_sub(footer_height);
    // The list gets everything above the footer (truncated if it would
    // collide; the footer is the fixed chrome).
    let list_height = footer_top.saturating_sub(area.y);
    lines.truncate(list_height as usize);
    frame.render_widget(Paragraph::new(lines.clone()), area);
    let footer_area = Rect {
        x: area.x,
        y: footer_top,
        width: area.width,
        height: footer_height.min(area.height),
    };
    frame.render_widget(Paragraph::new(footer_lines), footer_area);

    // Resolve hits: full-width rows for accounts (top block coordinates),
    // column rects for the bottom-anchored buttons (footer coordinates).
    for (line_index, hit) in line_hits {
        let y = area.y + line_index as u16;
        if y >= footer_top {
            continue; // truncated behind the footer
        }
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
    for (footer_line, x, width, hit) in add_button_rects {
        let y = footer_top + footer_line as u16;
        if y >= area.y + area.height || x >= area.width {
            continue;
        }
        hits.push((
            Rect {
                x: area.x + x,
                y,
                width: width.min(area.width - x),
                height: 1,
            },
            hit,
        ));
    }
}

/// `/providers` (W5d, report §5.2) — registry truth. The sim has NO such
/// screen: this layout is owner-directed and PROVISIONAL until the v0.0.15
/// install-probe sign-off (the brief records the gate). Rows are daemon
/// truth; the default marker moves only on the correlated reply.
fn render_providers(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let mut lines: Vec<Line<'_>> = Vec::new();
    // (line, column, width, hit) — chips resolved to rects after layout.
    let mut chip_hits: Vec<(usize, u16, u16, Hit)> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            "PROVIDERS",
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            " — registry truth · accounts live in /accounts",
            theme.dim_style(),
        ),
    ]));
    push_custom_card_lines(model, theme, &mut lines);
    if let Some(message) = &model.providers.message {
        lines.push(Line::styled(message.clone(), theme.gold_style()));
    }
    lines.push(Line::raw(""));

    if model.providers.providers.is_empty() {
        lines.push(Line::styled(
            "  no providers in the registry snapshot yet",
            theme.dim_style(),
        ));
    }
    for (index, summary) in model.providers.providers.iter().enumerate() {
        use haider_rpc::ProviderAvailabilityWire;
        let (dot, dot_style, health) = match summary.availability {
            ProviderAvailabilityWire::Available => ("●", theme.ok_style(), "available".to_owned()),
            ProviderAvailabilityWire::Unavailable => (
                "○",
                theme.dim_style(),
                summary
                    .availability_reason
                    .clone()
                    .unwrap_or_else(|| "unavailable".to_owned()),
            ),
            ProviderAvailabilityWire::Unknown => ("◌", theme.dim_style(), "unknown".to_owned()),
            _ => ("◌", theme.dim_style(), "unknown".to_owned()),
        };
        let mut header = Line::from(vec![
            Span::styled(
                summary.provider.clone(),
                theme
                    .bright_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(format!("{dot} {health}"), dot_style),
        ]);
        if model.providers.cursor == index {
            header = hover_band(header, true, area.width, theme);
        }
        lines.push(header);

        // API family · endpoint (safe display — never interpolated into a
        // command).
        let family = match summary.api_family {
            haider_rpc::ProviderApiFamilyWire::AnthropicMessages => "messages",
            haider_rpc::ProviderApiFamilyWire::OpenAiResponses => "responses",
            haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions => "openai-compatible",
            // B6a landed the adapter; without this arm the gemini row wears
            // "unknown api" against a registry that KNOWS the family.
            haider_rpc::ProviderApiFamilyWire::GeminiGenerateContent => "gemini",
            _ => "unknown api",
        };
        let endpoint = summary.endpoint.clone().unwrap_or_else(|| "—".to_owned());
        lines.push(Line::styled(
            format!("    {family} · {endpoint}"),
            theme.faint_style(),
        ));

        // Model chips: the default carries `*`; clicking a chip requests
        // the default change (CAS-fenced, never optimistic).
        if summary.models.is_empty() {
            lines.push(Line::styled("    models: —", theme.dim_style()));
        } else {
            let mut spans = vec![Span::styled("    models: ", theme.dim_style())];
            let mut offset = 4 + "models: ".len() as u16;
            let pending = model
                .providers
                .pending_default
                .as_ref()
                .filter(|(provider, _)| *provider == summary.provider);
            for model_name in &summary.models {
                let is_default = summary.default_model.as_deref() == Some(model_name);
                let is_pending =
                    pending.is_some_and(|(_, pending_model)| pending_model == model_name);
                let label = if is_default {
                    format!("{model_name}*")
                } else if is_pending {
                    format!("{model_name}…")
                } else {
                    model_name.clone()
                };
                let width = label.chars().count() as u16;
                chip_hits.push((
                    lines.len(),
                    offset,
                    width,
                    Hit::ProviderModel {
                        provider: summary.provider.clone(),
                        model: model_name.clone(),
                    },
                ));
                spans.push(Span::styled(
                    label,
                    if is_default {
                        theme.gold_style()
                    } else if is_pending {
                        theme.pulse_ink(theme.gold, model.anim_phase)
                    } else {
                        theme.text_style()
                    },
                ));
                spans.push(Span::raw("  "));
                offset += width + 2;
            }
            lines.push(Line::from(spans));
        }

        // Active account projection (from the accounts snapshot when the
        // user has visited /accounts; an em-dash otherwise — never a guess).
        let account_line = model
            .accounts
            .rows
            .iter()
            .find(|row| row.provider == summary.provider && row.selected)
            .map_or_else(
                || "    account: — (/accounts)".to_owned(),
                |row| {
                    format!(
                        "    account: {} · {} · in use",
                        row.alias,
                        crate::app::auth_label(row.method)
                    )
                },
            );
        let accounts_label = "[accounts]";
        let account_offset = account_line.chars().count() as u16 + 2;
        chip_hits.push((
            lines.len(),
            account_offset,
            accounts_label.chars().count() as u16,
            Hit::ProviderAccounts,
        ));
        lines.push(Line::from(vec![
            Span::styled(account_line, theme.dim_style()),
            Span::raw("  "),
            Span::styled(accounts_label, theme.gold_style()),
        ]));
        lines.push(Line::raw(""));
    }

    lines.push(Line::raw(""));
    push_account_add_buttons(model, theme, &mut lines, &mut chip_hits);
    lines.push(Line::styled(
        "click a model to set the default · e edits · x removes · h HuggingFace · esc back",
        theme.faint_style(),
    ));

    frame.render_widget(Paragraph::new(lines), area);

    for (line_index, x, width, hit) in chip_hits {
        let y = area.y + line_index as u16;
        if y >= area.y + area.height || x >= area.width {
            continue;
        }
        hits.push((
            Rect {
                x: area.x + x,
                y,
                width: width.min(area.width - x),
                height: 1,
            },
            hit,
        ));
    }
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
    // TUI6.2c (verifier finding 3): the login card OUTRANKS a blocking
    // menu on the band — the keys already prefer the card (login_key),
    // and a band that renders the menu while the card owns the keyboard
    // turns menu answers into typed secret bytes (a `1` meant for an
    // option landed in the mask; Enter staged a garbage credential). The
    // menu waits, unrendered and unclickable, until the card closes.
    let menu = if model.login.is_some() {
        None
    } else {
        // A zero-option ask never REPLACES the composer — the composer is
        // its answer line (owner report: select-only chrome rendered an
        // unanswerable question). Its title/body render above the input.
        model
            .projection
            .open_menu()
            .filter(|menu| !menu.options.is_empty())
    };
    let ask_menu = if model.login.is_some() {
        None
    } else {
        model
            .projection
            .open_menu()
            .filter(|menu| menu.options.is_empty())
    };
    let ask_rows: u16 = ask_menu.map_or(0, |menu| {
        u16::try_from(2 + menu.body.len()).unwrap_or(u16::MAX)
    });
    let menu_wrapped_body_rows = menu.map_or(0, |m| wrapped_menu_body(m, area.width).len());
    let needed_input = menu.map_or_else(
        || composer_height(model, area.width).saturating_add(ask_rows),
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
    // BREATHING ROWS (owner item 8a, Claude Code's rhythm): distinct blocks
    // in the lower region are separated by a blank row so the stream, the
    // waiting line, the todos and the SubTree read as separate things
    // instead of one wall. They are the FIRST thing to shed — before any
    // panel, long before a sacred row.
    let fixed = chrome + input_height + gap;
    let waiting_line = waiting_for_agents(model);
    let mut waiting_height = u16::from(waiting_line.is_some());
    let mut todos_height = model
        .projection
        .todos()
        .filter(|t| t.pinned)
        .map_or(0, |t| {
            if model.todos_collapsed {
                1
            } else {
                u16::try_from(t.items.len() + 1).unwrap_or(4)
            }
        });
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
    // TUI6.1 fix 2: the closing rule claims FIRST — before EVERY optional
    // panel — per `band_rule_reserve`'s law: reserved whenever chrome +
    // input + the transcript's sacred row leave it a row. It takes a
    // budget row when one exists, else the spacer gap row (review r1:
    // session menu 90×10 kept a blank gap where the rule fit; session
    // with chip 90×11 funded the SubTree and left the band open).
    let band_rule_h = band_rule_reserve(
        area.height,
        chrome + input_height + transcript_min,
        input_rule_h,
    );
    if band_rule_h > 0 {
        if budget > 0 {
            budget -= 1;
        } else {
            gap = 0;
        }
    }
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
    if waiting_height > budget {
        waiting_height = 0;
    } else {
        budget -= waiting_height;
    }
    if todos_height > budget {
        todos_height = 0;
    } else {
        budget -= todos_height;
    }
    // The closing rule was reserved ABOVE, before the panels (TUI6.1
    // fix 2 — sim anatomy: the border-top of whatever follows the
    // InputBar, SubTree tui.js:4764 / StatusBar tui.js:5497). The PAD is
    // the InputBar's bottom padding and stays behind every panel but
    // ahead of the breathing rows (TUI6 item 6 / TUI6d).
    let band_pad = u16::from(budget > 0 && input_rule_h > 0);
    if band_pad > 0 {
        budget -= band_pad;
    }
    // One breathing row above each block that is actually present, taken
    // last and given up first.
    let want_lead = u16::from(waiting_height > 0);
    let want_todos_lead = u16::from(todos_height > 0);
    let want_subtree_lead = u16::from(subtree_height > 0);
    let breathe = |want: u16, budget: &mut u16| -> u16 {
        if want > 0 && *budget >= want {
            *budget -= want;
            want
        } else {
            0
        }
    };
    let lead_waiting = breathe(want_lead, &mut budget);
    let lead_todos = breathe(want_todos_lead, &mut budget);
    let lead_subtree = breathe(want_subtree_lead, &mut budget);
    let [
        header_area,
        header_rule,
        transcript_area,
        _lead_waiting,
        waiting_area,
        _lead_todos,
        todos_area,
        queue_area,
        palette_area,
        rule_area,
        composer_area,
        band_pad_area,
        band_rule_area,
        _lead_subtree,
        subtree_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Length(header_rule_h),
        Constraint::Min(transcript_min),
        Constraint::Length(lead_waiting),
        Constraint::Length(waiting_height),
        Constraint::Length(lead_todos),
        Constraint::Length(todos_height),
        Constraint::Length(queue_height),
        Constraint::Length(palette_height),
        Constraint::Length(input_rule_h),
        Constraint::Length(input_height),
        Constraint::Length(band_pad),
        Constraint::Length(band_rule_h),
        Constraint::Length(lead_subtree),
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
    let (head, honorific) = (&model.session_head.0, &model.session_head.1);
    // Sim `.backbtn` (tui.js:5190-5205): FRAME border, dim label; hover
    // turns text and border gold.
    let back_hovered = model.hovered == Some(Hit::BackChip);
    let mut header_top = if back_hovered {
        chip_two_tone("← main".to_owned(), theme.gold_style(), theme.gold_style())
    } else {
        chip_two_tone("← main".to_owned(), theme.frame_style(), theme.dim_style())
    };
    let mut header_bottom: Vec<Span<'_>> = vec![Span::raw(" ".repeat(BACK_CHIP_COLS))];
    // The mark spans BOTH header lines (owner item 6): the half-block art in
    // a fixed slot, with the info block beside it. Whole-or-nothing — a
    // header too tight for the art keeps the one-line text mark inline.
    let mark_ink = theme.maroon_style().add_modifier(Modifier::BOLD);
    if crate::mark::header_fits(area.width, HEADER_MARK_RESERVED) {
        let rows = crate::mark::header_rows();
        header_top.push(Span::raw("  "));
        header_top.push(Span::styled(rows[0].clone(), mark_ink));
        header_top.push(Span::raw("  "));
        header_bottom.push(Span::raw("  "));
        header_bottom.push(Span::styled(rows[1].clone(), mark_ink));
        header_bottom.push(Span::raw("  "));
    } else {
        header_top.push(Span::styled(format!("  {}  ", sanctum.mark()), mark_ink));
        header_bottom.push(Span::raw(" ".repeat(sanctum.mark().chars().count() + 4)));
    }
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
    header_bottom.extend([
        Span::styled(title, theme.dim_style()),
        Span::styled(format!(" ▸ {head} {honorific}"), theme.gold_style()),
        Span::styled(
            format!(" · branch main · {}", identity.device),
            theme.dim_style(),
        ),
    ]);
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
    // Replace the half-block header mark with the crisp حيدر image on a graphics
    // terminal — same 16×2 footprint, at the fixed slot after the back chip and
    // its two-space gap. No-op (half-block art stays) when header_fits chose the
    // art tier and there is no graphics protocol.
    if crate::mark::header_fits(area.width, HEADER_MARK_RESERVED) {
        draw_wordmark_image(
            model,
            Rect {
                x: header_area.x + BACK_CHIP_COLS as u16 + 2,
                y: header_area.y,
                width: crate::mark::HEADER_COLS,
                height: crate::mark::HEADER_ROWS,
            },
            frame,
        );
    }
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
    // B2b-m3: every entry's starting LOGICAL line, for the render-resolved
    // jump. A user row anchors its actual prompt line, not the preceding
    // spacer — the sticky map's convention (research §Q3).
    let mut entry_lines: Vec<usize> = Vec::with_capacity(model.projection.entries().len());
    for entry in model.projection.entries() {
        if let TranscriptEntry::User { text, .. } = entry {
            // transcript_lines pushes a spacer, then the prompt row.
            user_rows.push((lines.len() + 1, text.as_str()));
            entry_lines.push(lines.len() + 1);
        } else {
            entry_lines.push(lines.len());
        }
        transcript_lines(
            &mut lines,
            entry,
            theme,
            transcript_area.width,
            model.anim_phase,
        );
    }
    // Sim `.thinking` (tui.js:4458-4462): a transient gold tail while
    // thinking, pulsing (1.4s). The port also breathes the dot ● ↔ ◌ on
    // the shared clock — the owner's marquee "alive" element.
    if model.projection.is_thinking() {
        lines.push(thinking_line(theme, model.anim_phase));
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
    // B2b-m3: resolve an armed tree jump IN THIS FRAME — node → display
    // entry → logical line → wrapped row, every step through the
    // renderer's OWN width and prefix sums (research §Q3: wrapped-row
    // offsets are never cached, so a resize simply resolves against the
    // new geometry). The anchor clears only when it LANDS.
    // (A taken jump whose branch is no longer displayed stays dropped: it
    // is never resolved against another branch's rows.)
    if let Some(jump) = model.pending_jump.take()
        && jump.branch.as_ref() == model.branch_state.active()
    {
        match model.projection.entry_of_node(&jump.node) {
            Some(entry) => {
                let line = entry_lines.get(entry).copied().unwrap_or(0);
                let row = row_of_line.get(line).copied().unwrap_or(0);
                // A near-tail target cannot be top-aligned without fake
                // padding: clamp honestly and let it sit where the real
                // rows put it.
                let target_top = row.min(max_scroll);
                model.scroll_back.set(max_scroll - target_top);
                // The sticky must not cover the revealed row — same
                // suppression as a sticky jump, until a real wheel.
                model.sticky_suppressed.set(true);
            }
            // Replay has not materialized the node yet: keep the anchor
            // armed for catch-up — never guess another entry.
            None => *model.pending_jump.borrow_mut() = Some(jump),
        }
    }
    let scroll_back = model.scroll_back.get();
    let scroll = max_scroll - scroll_back;
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((scroll, 0)), transcript_area);
    // Sticky origin line (sim StickyLine, tui.js:3345-3349 / 4597-4623):
    // while scrolled into history, pin the user prompt that produced the
    // top-visible content. Chrome per the sim's ACTUAL CSS (owner item 11 —
    // a review round once stripped this to bare theme ground; the CSS says
    // otherwise): a distinct band with a bottom frame edge (`border-bottom:
    // 1px solid frame` → the underline), bright nowrap-ellipsized text,
    // maroon bold sigil, and a REAL `:hover` (opaque ground + maroon ink,
    // tui.js:4614-4617) through the standard hover path. Click keeps the
    // reader AT the prompt (jumpToSticky, tui.js:2637-2645): the hit
    // carries the scroll-back that puts the prompt's first row at the
    // viewport top; after a jump the sticky is SUPPRESSED until the next
    // real wheel so it never covers the row it just revealed.
    if scroll_back > 0 && scroll > 0 && transcript_area.height > 0 && !model.sticky_suppressed.get()
    {
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
            // The band style carries the text ink (bright, maroon on
            // hover), so the prompt text inherits it; the sigil pins its
            // own maroon bold. The pad span stretches the band — and its
            // bottom edge — across the full row.
            let hovered = model.hovered == Some(Hit::StickyJump(jump));
            let band = if hovered {
                theme.sticky_hover_style()
            } else {
                theme.sticky_style()
            };
            let mut spans = vec![
                Span::raw(" "),
                Span::styled("❯ ", theme.maroon_style().add_modifier(Modifier::BOLD)),
                Span::raw(ellipsize(&text.replace('\n', " "), budget)),
            ];
            let pad =
                (transcript_area.width as usize).saturating_sub(Line::from(spans.clone()).width());
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)).style(band), sticky_rect);
            hits.push((sticky_rect, Hit::StickyJump(jump)));
        }
    }

    if let Some(todos) = model.projection.todos().filter(|t| t.pinned) {
        // Sim TodoPanel (tui.js:2863-2888, 4667-4709): the header is a BUTTON
        // that toggles collapse, and the collapsed form summarises the item
        // being worked. Owner item 7 promotes it from the deferred ledger and
        // adds hover chrome on the header and every row.
        let arrow = if model.todos_collapsed { "▸" } else { "▾" };
        let mut header = vec![
            Span::styled(format!("{arrow} todos"), theme.dim_style()),
            Span::styled(
                format!(" — {}/{} done", todos.done_count(), todos.items.len()),
                theme.gold_style(),
            ),
        ];
        if model.todos_collapsed {
            let current = todos
                .items
                .iter()
                .find(|item| item.state == TodoState::Processing)
                .or_else(|| {
                    todos
                        .items
                        .iter()
                        .find(|item| item.state != TodoState::Completed)
                });
            if let Some(item) = current {
                header.push(Span::styled(
                    format!(" · ■ {}", item.text),
                    theme.bright_style(),
                ));
            }
        }
        let mut todo_lines = vec![hover_band(
            Line::from(header),
            model.hovered == Some(Hit::TodosToggle),
            todos_area.width,
            theme,
        )];
        if !model.todos_collapsed {
            for item in &todos.items {
                todo_lines.push(hover_band(
                    todo_row(item, &todos.items, theme, model.anim_phase),
                    model.hovered == Some(Hit::TodoRow(item.id)),
                    todos_area.width,
                    theme,
                ));
            }
        }
        frame.render_widget(Paragraph::new(Text::from(todo_lines)), todos_area);
        if todos_area.height > 0 {
            hits.push((
                Rect {
                    x: todos_area.x,
                    y: todos_area.y,
                    width: todos_area.width,
                    height: 1,
                },
                Hit::TodosToggle,
            ));
            if !model.todos_collapsed {
                for (index, item) in todos.items.iter().enumerate() {
                    let y = todos_area.y + u16::try_from(index + 1).unwrap_or(u16::MAX);
                    if y < todos_area.y + todos_area.height {
                        hits.push((
                            Rect {
                                x: todos_area.x,
                                y,
                                width: todos_area.width,
                                height: 1,
                            },
                            Hit::TodoRow(item.id),
                        ));
                    }
                }
            }
        }
    }

    // The background-agent waiting line (item 8b) — plain text, never a hit.
    if let Some(line) = &waiting_line
        && waiting_area.height > 0
    {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled("✳", theme.gold_style()),
                Span::styled(
                    line.strip_prefix('✳').unwrap_or(line).to_owned(),
                    theme.dim_style(),
                ),
            ])),
            waiting_area,
        );
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
    } else if let Some(ask) = ask_menu.filter(|_| composer_area.height > ask_rows) {
        let ask_area = Rect {
            x: composer_area.x,
            y: composer_area.y,
            width: composer_area.width,
            height: ask_rows.min(composer_area.height),
        };
        let mut ask_lines = vec![Line::from(vec![
            Span::styled("? ", theme.warn_style()),
            Span::styled(ask.title.clone(), theme.warn_style()),
        ])];
        for line in &ask.body {
            ask_lines.push(Line::styled(line.clone(), theme.text_style()));
        }
        ask_lines.push(Line::styled(
            "type your answer below · ⏎ answers · esc interrupts",
            theme.dim_style(),
        ));
        frame.render_widget(Paragraph::new(ask_lines), ask_area);
        let shifted = Rect {
            x: composer_area.x,
            y: composer_area.y + ask_area.height,
            width: composer_area.width,
            height: composer_area.height - ask_area.height,
        };
        render_composer(model, theme, frame, rule_area, shifted, hits);
    } else if theme_picker_showing(model) {
        render_theme_picker(model, theme, frame, rule_area, composer_area, hits);
    } else {
        render_composer(model, theme, frame, rule_area, composer_area, hits);
    }
    // The inputBg band is one panel: the composer rows AND the padding row
    // below them carry it edge to edge (owner item 2 — the band used to sit
    // behind the text row only, so it read as "cut in half").
    if band_pad > 0 {
        frame.render_widget(Block::default().style(theme.input_style()), band_pad_area);
    }
    if band_rule_h > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(band_rule_area.width as usize),
                theme.frame_style(),
            ))
            .style(theme.text_style()),
            band_rule_area,
        );
    }
    if subtree_height > 0 {
        render_subtree(model, theme, frame, subtree_area, false, hits);
    }
    if model.token_panel {
        render_token_panel(model, theme, frame, area, rule_area.y);
    }
}

/// ⌃G / `/tokens` — context by model (sim tui.js:2946-2977), floated
/// above the input band. Live rows carry the W7b footprint truth: an
/// EXACT snapshot prints plain splits, an ESTIMATED one keeps the sim's
/// `~` prefixes; with no snapshot yet the sim's fabricated 62/28/10
/// split stands in (demo parity).
fn render_token_panel(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    input_top: u16,
) {
    struct PanelRow {
        label: String,
        tokens: u64,
        window: u64,
        detail: String,
    }
    let identity = &model.identity;
    let main_label = format!("● {} · {}", identity.model_short, identity.provider);
    let main = model.projection.latest_footprint().map_or_else(
        || {
            // Sim math (tui.js:2951-2953): fabricated splits + burn-rate
            // turns estimate from the transcript's user rows.
            let tokens = model.projection.context_tokens();
            let window = identity.context_window;
            let turns = u64::from(model.projection.user_row_count().max(1));
            let burn = (tokens / turns).max(6000);
            let to_threshold = (window.saturating_mul(85) / 100).saturating_sub(tokens);
            let detail = format!(
                "in ~{} · out ~{} · cached ~{} · ≈{} turns to auto-compaction",
                fmt_tok(tokens.saturating_mul(62) / 100),
                fmt_tok(tokens.saturating_mul(28) / 100),
                fmt_tok(tokens.saturating_mul(10) / 100),
                to_threshold.div_ceil(burn).max(1),
            );
            PanelRow {
                label: main_label.clone(),
                tokens,
                window,
                detail,
            }
        },
        |footprint| {
            let approx = match footprint.truth {
                haider_protocol::context::ContextFootprintTruth::Exact => "",
                haider_protocol::context::ContextFootprintTruth::Estimated => "~",
            };
            let mut detail = format!(
                "in {approx}{} · out {approx}{} · cached {approx}{}",
                fmt_tok(footprint.input_tokens),
                fmt_tok(footprint.output_tokens),
                fmt_tok(footprint.cached_input_tokens),
            );
            if let Some(turns) = footprint.estimated_turns_to_threshold {
                detail.push_str(&format!(" · ≈{turns} turns to auto-compaction"));
            }
            PanelRow {
                label: main_label.clone(),
                tokens: footprint.used_tokens,
                window: footprint.context_window.unwrap_or(identity.context_window),
                detail,
            }
        },
    );
    let mut rows = vec![main];
    for (_, chip) in crate::app::flatten_chips(&model.chips) {
        let (tokens, window, detail) = chip.transcript.latest_footprint().map_or_else(
            || (chip.tokens, identity.context_window, String::new()),
            |footprint| {
                (
                    footprint.used_tokens,
                    footprint.context_window.unwrap_or(identity.context_window),
                    String::new(),
                )
            },
        );
        rows.push(PanelRow {
            label: format!("└ {} · {}", chip.name, chip.model),
            tokens,
            window,
            detail,
        });
    }
    let mut lines = vec![Line::styled(
        "context by model — ⌃G · /tokens · esc closes",
        theme.dim_style(),
    )];
    for row in &rows {
        #[allow(clippy::cast_precision_loss)]
        let pct = if row.window == 0 {
            0.0
        } else {
            row.tokens as f64 / row.window as f64
        };
        let mut text = format!(
            "{}  {} {}%  {}/{}",
            row.label,
            meter_cells(pct, 12),
            (pct.clamp(0.0, 1.0) * 100.0).round(),
            fmt_tok(row.tokens),
            fmt_tok(row.window),
        );
        if !row.detail.is_empty() {
            text.push_str(" · ");
            text.push_str(&row.detail);
        }
        lines.push(Line::styled(text, theme.text_style()));
    }
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let width = area.width.saturating_sub(2).max(24);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: input_top.saturating_sub(height).max(area.y),
        width,
        height: height.min(area.height),
    };
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(theme.frame_style())
                .style(theme.text_style()),
        ),
        rect,
    );
}

/// `/tree` — the session tree (B2b-m3; sim tui.js:3366-3430). ONE branch
/// at a time: the viewed branch's header (● follows the session's ACTIVE
/// branch), a node row per user turn / ⊟ compaction, each fork marker
/// immediately under its exact fork node, and the root→viewed breadcrumb
/// in the head line. Hits carry the row VALUE (a stale hit on a replaced
/// row matches nothing).
fn render_tree(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let name = model
        .session_name
        .as_deref()
        .or(model.session_title.as_deref())
        .unwrap_or("session");
    let rows = crate::app::tree_rows(model);
    let crumb = crate::app::tree_crumb(model).join(" ▸ ");
    let drilled = crate::app::tree_viewed(model).is_some();
    let mut lines = vec![
        Line::styled(
            format!("SESSION TREE — {name} — {crumb}"),
            theme.bright_style(),
        ),
        Line::raw(""),
    ];
    // Selection windows around `tree_sel` when the list outgrows the frame.
    let budget = usize::from(area.height.saturating_sub(4)).max(1);
    let selected = model.tree_sel.min(rows.len().saturating_sub(1));
    let first = selected
        .saturating_sub(budget.saturating_sub(1))
        .min(rows.len().saturating_sub(budget));
    for (index, row) in rows.iter().enumerate().skip(first).take(budget) {
        let hovered = model.hovered.as_ref() == Some(&Hit::TreeRow(row.clone()));
        let style = if index == selected {
            theme.selection_style()
        } else {
            match row {
                crate::app::TreeRow::Branch { .. } => theme.bright_style(),
                crate::app::TreeRow::Fork { .. } => theme.gold_style(),
                crate::app::TreeRow::Node { .. } => theme.text_style(),
            }
        };
        let mut line = Line::styled(row.label().to_owned(), style);
        if hovered && index != selected {
            line = hover_band(line, true, area.width, theme);
        }
        hits.push((
            row_rect(area, area.y, lines.len()),
            Hit::TreeRow(row.clone()),
        ));
        lines.push(line);
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "↑↓ select · ⏎ jump / open fork · f fork at node · esc {}",
            if drilled { "up to parent" } else { "back" }
        ),
        theme.dim_style(),
    ));
    frame.render_widget(Paragraph::new(lines).style(theme.text_style()), area);
}

/// `/tools` live (W8b) — a read-only view of the daemon's canonical
/// registry + remembered session grants (research §W8b-4). Committed
/// snapshot only; while the read is in flight the screen says so —
/// nothing is fabricated.
fn render_tools(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let mut lines = vec![
        Line::styled("TOOLS — daemon inventory", theme.bright_style()),
        Line::raw(""),
    ];
    match &model.tools_inventory {
        None => lines.push(Line::styled(
            "fetching the daemon's tool inventory…",
            theme.dim_style(),
        )),
        Some(snapshot) => {
            for entry in &snapshot.tools {
                let effects = entry
                    .manifest
                    .effects
                    .iter()
                    .map(|effect| format!("{effect:?}").to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .join("+");
                let effects = if effects.is_empty() {
                    "none".to_owned()
                } else {
                    effects
                };
                lines.push(Line::from(vec![
                    Span::styled("  ⚒ ", theme.dim_style()),
                    Span::styled(entry.manifest.name.clone(), theme.maroon_style()),
                    Span::styled(
                        format!(
                            " · {} · default {}",
                            effects,
                            format!("{:?}", entry.default).to_ascii_lowercase()
                        ),
                        theme.dim_style(),
                    ),
                ]));
            }
            lines.push(Line::raw(""));
            if snapshot.remembered_grants.is_empty() {
                lines.push(Line::styled(
                    "no remembered session grants",
                    theme.dim_style(),
                ));
            } else {
                lines.push(Line::styled(
                    "remembered session grants",
                    theme.bright_style(),
                ));
                for grant in &snapshot.remembered_grants {
                    lines.push(Line::styled(format!("  {grant:?}"), theme.dim_style()));
                }
            }
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "read-only — workspace cwd + bounded supervised process, not a sandbox · esc back",
        theme.dim_style(),
    ));
    frame.render_widget(Paragraph::new(lines).style(theme.text_style()), area);
}

/// `/hooks` (H4) — the daemon's hook discovery + digest trust for the
/// active session's workspace, and the session's journaled firings in the
/// lower half (newest first, bounded). Committed daemon truth ONLY: the
/// in-flight read says so, the demo says it has no engine, and the trust
/// column moves on list snapshots, never on clicks.
fn render_hooks(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let hooks = &model.hooks;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "HOOKS",
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            match &hooks.policy {
                Some(policy) => format!(" — workspace + profile · policy {policy}"),
                None => " — workspace + profile".to_owned(),
            },
            theme.dim_style(),
        ),
    ])];
    if let Some(message) = &hooks.message {
        lines.push(Line::styled(message.clone(), theme.gold_style()));
    }
    lines.push(Line::raw(""));
    match &hooks.rows {
        None => {
            if hooks.message.is_none() {
                lines.push(Line::styled(
                    "fetching the daemon's hook discovery…",
                    theme.dim_style(),
                ));
            }
        }
        Some(rows) if rows.is_empty() => {
            lines.push(Line::styled(
                if model.mode.fabricates_locally() {
                    "  no hooks in the demo — live mode lists the daemon's discovery"
                } else {
                    "  no hooks discovered — hooks.json at the workspace root, \
                     or the profile's hooks.json"
                },
                theme.dim_style(),
            ));
        }
        Some(rows) => {
            let selected = hooks.cursor.min(rows.len().saturating_sub(1));
            for (index, row) in rows.iter().enumerate() {
                let glyph = hooks.glyph(row);
                let glyph_style = match glyph {
                    crate::hooks::TrustGlyph::Trusted => theme.gold_style(),
                    crate::hooks::TrustGlyph::Untrusted => theme.dim_style(),
                    crate::hooks::TrustGlyph::RevokedByEdit => theme.warn_style(),
                };
                let is_selected = index == selected;
                let cursor = if is_selected { "❯" } else { " " };
                let decision = if row.decision { " · decision" } else { "" };
                let mut spans = vec![
                    Span::styled(format!(" {cursor} "), theme.gold_style()),
                    Span::styled(
                        format!("{}. ", index + 1),
                        if is_selected {
                            theme.bright_style()
                        } else {
                            theme.dim_style()
                        },
                    ),
                    Span::styled(format!("{} ", glyph.glyph()), glyph_style),
                    Span::styled(
                        row.name.clone(),
                        theme
                            .bright_style()
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            " · {}:{}{decision} · {} · {}",
                            row.kind,
                            row.event,
                            crate::hooks::short_digest(&row.digest),
                            glyph.label(),
                        ),
                        theme.dim_style(),
                    ),
                ];
                if hooks.pending.as_deref() == Some(row.digest.as_str()) {
                    // In-flight receipt feedback WITHOUT moving the trust
                    // column (forbidden optimism — the accounts law).
                    spans.push(Span::styled(
                        " …",
                        theme.pulse_ink(theme.gold, model.anim_phase),
                    ));
                }
                let hovered = model.hovered.as_ref() == Some(&Hit::HookRow(row.digest.clone()));
                let mut line = Line::from(spans);
                if is_selected {
                    let pad = (area.width as usize).saturating_sub(line.width());
                    if pad > 0 {
                        line.push_span(Span::raw(" ".repeat(pad)));
                    }
                    line = line.style(theme.selection_style());
                } else if hovered {
                    line = hover_band(line, true, area.width, theme);
                }
                hits.push((
                    row_rect(area, area.y, lines.len()),
                    Hit::HookRow(row.digest.clone()),
                ));
                lines.push(line);
            }
        }
    }
    // Recent firings — the session's journaled hook facts, newest first,
    // bounded by the RENDER cap (the store keeps a deeper tail).
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "recent firings — newest first",
        theme.bright_style(),
    ));
    if model.hook_facts.is_empty() {
        lines.push(Line::styled("  none this session", theme.dim_style()));
    } else {
        for entry in model
            .hook_facts
            .recent()
            .take(crate::hooks::FIRING_ROWS_MAX)
        {
            lines.push(Line::styled(
                format!("  {}", entry.line()),
                theme.dim_style(),
            ));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "↑↓ select · 1-9 pick · ⏎ trust / revoke · esc back",
        theme.dim_style(),
    ));
    frame.render_widget(Paragraph::new(lines).style(theme.text_style()), area);
    // The trust/revoke confirmation card — an overlay the session-scoped
    // esc law closes (esc cancels the CARD, never the screen).
    if let Some(confirm) = &hooks.confirm {
        let action = if confirm.grant { "trust" } else { "revoke" };
        let card_lines = vec![
            Line::styled(
                format!("{action} hook `{}`?", confirm.name),
                theme.bright_style(),
            ),
            Line::styled(
                format!("digest {}", crate::hooks::short_digest(&confirm.digest)),
                theme.dim_style(),
            ),
            Line::styled("⏎ confirm · esc cancel", theme.faint_style()),
        ];
        let height = u16::try_from(card_lines.len() + 2).unwrap_or(u16::MAX);
        let width = area.width.saturating_sub(4).max(24);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area
                .y
                .saturating_add(area.height.saturating_sub(height + 1))
                .max(area.y),
            width,
            height: height.min(area.height),
        };
        frame.render_widget(ratatui::widgets::Clear, rect);
        frame.render_widget(
            Paragraph::new(card_lines).block(
                Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .border_style(theme.frame_style())
                    .style(theme.text_style()),
            ),
            rect,
        );
    }
}

/// Wrap a row in the shared hover band (sim `:hover { background: selBg }`),
/// padding it to the full region width so the band reads as one strip.
fn hover_band<'a>(mut line: Line<'a>, hovered: bool, width: u16, theme: &Theme) -> Line<'a> {
    if !hovered {
        return line;
    }
    let pad = (width as usize).saturating_sub(line.width());
    if pad > 0 {
        line.push_span(Span::raw(" ".repeat(pad)));
    }
    line.style(theme.hover_style())
}

/// The background-agent waiting line (owner item 8b, styled after Claude
/// Code's): `✳ Waiting for N background agents to finish`, shown only while
/// live chips exist. N is `treeLiveCount` — the same count that derives the
/// `◔ WAITING · N` badge, so the two can never disagree. A chip holding a
/// question is still unfinished, but it is unfinished ON THE USER, so the
/// line says so rather than implying the agent is busy.
fn waiting_for_agents(model: &AppModel) -> Option<String> {
    let live = crate::app::tree_live_count(&model.chips);
    if live == 0 {
        return None;
    }
    let plural = if live > 1 { "s" } else { "" };
    let needs_input = crate::app::flatten_chips(&model.chips)
        .iter()
        .filter(|(_, chip)| {
            !chip.closed && chip.state == crate::script::ChipDisplayState::InputRequired
        })
        .count();
    let mut line = format!("✳ Waiting for {live} background agent{plural} to finish");
    if needs_input > 0 {
        line = format!("✳ Waiting for {live} background agent{plural} — {needs_input} needs input");
    }
    Some(line)
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
    // The ⌂ main row is part of the map on BOTH screens (owner item 3), so
    // it always costs a row while the panel is open.
    let _ = on_subagent;
    let rows = crate::app::flatten_chips(&model.chips).len() + 1;
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
        // Owner item 3: the main row is ALWAYS in the map, not just in the
        // subagent view, and whichever node is being VIEWED is bold — so the
        // panel always answers "where am I?". (The sim only draws this row on
        // the subagent screen, tui.js:2915-2920; the owner's direction wins.)
        let on_main = !on_subagent;
        row_hits.push((lines.len(), Hit::SessionHome));
        let mut home = Line::from(vec![
            Span::styled(" ⌂ ", theme.gold_style()),
            Span::styled(
                format!("{} — back to the main transcript", model.display_name()),
                if on_main {
                    theme.bright_style().add_modifier(Modifier::BOLD)
                } else {
                    theme.dim_style()
                },
            ),
        ]);
        home = hover_band(
            home,
            model.hovered == Some(Hit::SessionHome),
            area.width,
            theme,
        );
        lines.push(home);
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
            // Sim chip glyph pulses (tui.js:4823-4834): running/tool wear
            // maroon, input-required the amber warn — both breathing on
            // the shared clock; every other state keeps the quiet gold.
            let glyph_style = if chip.closed {
                theme.faint_style()
            } else {
                match display {
                    crate::script::ChipDisplayState::Running
                    | crate::script::ChipDisplayState::Tool => {
                        theme.pulse_ink(theme.maroon, model.anim_phase)
                    }
                    crate::script::ChipDisplayState::InputRequired => {
                        theme.pulse_ink(theme.warn, model.anim_phase)
                    }
                    _ => theme.gold_style(),
                }
            };
            let mut spans = vec![
                Span::styled(format!(" {indent}{connector} "), theme.faint_style()),
                Span::styled(format!("{glyph} "), glyph_style),
                Span::styled(
                    format!("{} {}", chip.callsign, chip.hon),
                    if chip.closed {
                        theme.faint_style()
                    } else if viewing {
                        // The viewed node is bold (owner item 3).
                        theme.bright_style().add_modifier(Modifier::BOLD)
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
            let line = hover_band(
                Line::from(spans),
                model.hovered == Some(Hit::ChipRow(chip.agent.clone())),
                area.width,
                theme,
            );
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
    // The login card outranks the chip's question card on the band too
    // (TUI6.2c finding 3, same law as the session menu).
    let menu = if model.login.is_some() {
        None
    } else {
        chip.question_menu()
    };
    let needed_input = menu.map_or_else(
        || composer_height(model, area.width),
        |m| {
            u16::try_from(1 + wrapped_menu_body(m, area.width).len() + m.options.len() + 1)
                .unwrap_or(u16::MAX)
        },
    );
    // TUI6.2 fix 4 (review r2 finding 4, overruling the r1 trade): the
    // card's sacred floor is TITLE + options — options without their
    // question is a dignity regression (the 90×12 four-option card shed
    // its title while a blank optional gap survived). Session parity:
    // the session ledger funds the full card before any panel; the
    // subagent floor now does too.
    let floor_input = menu.map_or(1, |m| {
        u16::try_from(m.options.len().max(1) + 1).unwrap_or(u16::MAX)
    });
    // Compact ledger (the session screen's shed order, condensed).
    // TUI6.1 fix 2: the closing rule joined it AHEAD of the SubTree and
    // the gap (review r1: subagent 90×11 / question 90×14 funded the
    // SubTree while the band stayed open) — shed order is now subtree →
    // gap → closing rule → transcript row → header line 2 → rules →
    // header line 1; the input floor never yields. The subtree and gap
    // rungs carry the rule's provisional row in their extras, so they
    // shed in its favor per `band_rule_reserve`'s law.
    let mut gap: u16 = 1;
    let mut transcript_min: u16 = 1;
    let mut header_h: u16 = 2;
    let mut header_rule_h: u16 = 1;
    let mut input_rule_h: u16 = 1;
    let mut subtree_height = subtree_needed(model, true);
    let over = |header_h: u16, rules: u16, extras: u16, area: Rect| {
        area.height.saturating_sub(header_h + rules + extras) < floor_input
    };
    let rule_demand = u16::from(input_rule_h > 0);
    if over(
        header_h,
        header_rule_h + input_rule_h,
        gap + transcript_min + subtree_height + rule_demand,
        area,
    ) {
        subtree_height = 0;
    }
    if over(
        header_h,
        header_rule_h + input_rule_h,
        gap + transcript_min + rule_demand,
        area,
    ) {
        gap = 0;
    }
    // The rule's own claim, through the shared law: reserved whenever the
    // surviving chrome + the transcript's sacred row + the input floor
    // leave it a row; it yields to the transcript's sacred row (session
    // parity), never to the optional panels above.
    let band_rule_h = band_rule_reserve(
        area.height,
        header_h
            + header_rule_h
            + input_rule_h
            + gap
            + transcript_min
            + subtree_height
            + floor_input,
        input_rule_h,
    );
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
        .saturating_sub(chrome + gap + transcript_min + subtree_height + band_rule_h);
    let input_height = needed_input
        .min(input_avail)
        .max(floor_input.min(area.height.saturating_sub(chrome)))
        .clamp(1, area.height.max(1));
    // TUI6 item 6 (the band-anatomy sweep — the owner's screenshot was
    // THIS surface: `❯ message …` straight into `▼ subagents`): the band
    // closes with the rule reserved above plus an inputBg pad row when a
    // row remains (the pad is the InputBar's bottom padding and stays
    // OPTIONAL — behind the rule, per the law).
    let spare = area.height.saturating_sub(
        chrome + gap + transcript_min + subtree_height + input_height + band_rule_h,
    );
    let band_pad = u16::from(spare > 0 && input_rule_h > 0);
    let [
        header_area,
        header_rule,
        transcript_area,
        rule_area,
        composer_area,
        band_pad_area,
        band_rule_area,
        subtree_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Length(header_rule_h),
        Constraint::Min(transcript_min),
        Constraint::Length(input_rule_h),
        Constraint::Length(input_height),
        Constraint::Length(band_pad),
        Constraint::Length(band_rule_h),
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
    // Sim chip-view state badge (tui.js:5320-5330): input_required wears
    // the warn tone and PULSES (1.2s) until answered; other states keep
    // the port's quiet frame/gold chrome.
    let (badge_chrome, badge_ink) =
        if !chip.closed && display == crate::script::ChipDisplayState::InputRequired {
            let warn = theme.pulse_ink(theme.warn, model.anim_phase);
            (warn, warn)
        } else {
            (theme.frame_style(), theme.gold_style())
        };
    header_bottom.extend(chip_two_tone(badge_label, badge_chrome, badge_ink));
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
        transcript_lines(
            &mut lines,
            entry,
            theme,
            transcript_area.width,
            model.anim_phase,
        );
    }
    if chip.state == crate::script::ChipDisplayState::Thinking {
        lines.push(thinking_line(theme, model.anim_phase));
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
    // The band's closing anatomy (TUI6 item 6): inputBg pad, then the
    // frame rule — rendered on BOTH the composer and question-card forms,
    // exactly as the session band does.
    if band_pad > 0 {
        frame.render_widget(Block::default().style(theme.input_style()), band_pad_area);
    }
    if band_rule_h > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(band_rule_area.width as usize),
                theme.frame_style(),
            ))
            .style(theme.text_style()),
            band_rule_area,
        );
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
    // ---- Sacred-row ledger (review P1-6) ----
    // The composer's CURSOR row is sacred here exactly as it is on the
    // session and subagent screens: the stage may never render a composer
    // the user cannot see while it still accepts typing. Shed order under
    // pressure, first to go → last:
    //
    //   gap → columns → orb → the transcript's sacred row →
    //   bar rule + input rule → the ◉ AURA bar → (never) the cursor row
    //
    // Every shed region also stops emitting hits (the bar chips and the
    // hold-to-talk button are gated on their own area heights), so nothing
    // stays clickable once it stops being painted.
    let left_rows = 1 + aura.roster.len().max(1);
    let right_rows = 1 + aura.log.len().min(7);
    let mut columns_h = u16::try_from(left_rows.max(right_rows)).unwrap_or(8).min(8);
    let mut orb_h: u16 = 4;
    let mut transcript_min: u16 = 1;
    let mut gap: u16 = 1;
    let mut bar_h: u16 = 1;
    let mut bar_rule_h: u16 = 1;
    let mut input_rule_h: u16 = 1;
    let composer_want = composer_height(model, area.width).max(1);
    let over =
        |bar: u16, rules: u16, extras: u16| area.height.saturating_sub(bar + rules + extras) < 1;
    // TUI6.1 fix 2 (review r1: aura 90×10 kept orb/columns while the
    // closing rule shed): the rule row (the gap, TUI5's net-zero trick)
    // outranks the OPTIONAL columns and orb, and yields to the
    // transcript's sacred row (session parity) — shed order is columns →
    // orb → closing rule → transcript row → rules → bar. The
    // columns/orb rungs carry the rule's provisional row (`gap`, still 1
    // here) in their extras so they shed in its favor; TUI6.2 fix 6
    // (review r2 finding 6) then DERIVES the rule row from
    // `band_rule_reserve` over the survivors — the function is the
    // runtime authority, not a debug-only tie.
    if over(
        bar_h,
        bar_rule_h + input_rule_h,
        gap + columns_h + orb_h + transcript_min,
    ) {
        columns_h = 0;
    }
    if over(
        bar_h,
        bar_rule_h + input_rule_h,
        gap + orb_h + transcript_min,
    ) {
        orb_h = 0;
    }
    gap = band_rule_reserve(
        area.height,
        bar_h + bar_rule_h + input_rule_h + columns_h + orb_h + transcript_min + 1,
        input_rule_h,
    );
    if over(bar_h, bar_rule_h + input_rule_h, transcript_min) {
        transcript_min = 0;
    }
    if over(bar_h, bar_rule_h + input_rule_h, 0) {
        bar_rule_h = 0;
        input_rule_h = 0;
    }
    if over(bar_h, 0, 0) {
        bar_h = 0;
    }
    let composer_h = composer_want
        .min(area.height.saturating_sub(
            bar_h + bar_rule_h + input_rule_h + gap + columns_h + orb_h + transcript_min,
        ))
        .max(1)
        .clamp(1, area.height.max(1));
    // TUI6 item 6 (band sweep): the aura band CLOSES — the launcher's
    // net-zero trick verbatim (TUI5 item 1b): the blank gap row under the
    // composer becomes the frame rule the sim draws as the StatusBar's
    // border-top (tui.js:5497); it sheds under pressure exactly as the
    // gap did.
    let band_rule_h = gap;
    let [
        bar_area,
        bar_rule,
        orb_area,
        columns_area,
        transcript_area,
        rule_area,
        composer_area,
        band_rule_area,
    ] = Layout::vertical([
        Constraint::Length(bar_h),
        Constraint::Length(bar_rule_h),
        Constraint::Length(orb_h),
        Constraint::Length(columns_h),
        Constraint::Min(transcript_min),
        Constraint::Length(input_rule_h),
        Constraint::Length(composer_h),
        Constraint::Length(band_rule_h),
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
    if orb_area.height > 0 {
        let talk_label = if aura.state == crate::script::AuraState::Listening {
            "◉ listening…"
        } else {
            "◉ hold to talk"
        };
        // The live aura hold pulses exactly like the session mic (one
        // `.live` law across both talk chips).
        let (talk_chrome, talk_ink) = if aura.state == crate::script::AuraState::Listening {
            let live = theme.pulse_ink(theme.maroon, model.anim_phase);
            (live, live)
        } else if model.hovered == Some(Hit::AuraTalkBtn) {
            (theme.gold_style(), theme.gold_style())
        } else {
            (theme.frame_style(), theme.gold_style())
        };
        let talk_spans = chip_two_tone(talk_label.to_owned(), talk_chrome, talk_ink);
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
    if columns_area.height > 0 {
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
            // Sim `.rrow.running .g` (tui.js:4128-4131): a running
            // controlled session's glyph pulses maroon; others stay gold.
            let glyph_style = if row.state == crate::script::ChipDisplayState::Running {
                theme.pulse_ink(theme.maroon, model.anim_phase)
            } else {
                theme.gold_style()
            };
            left.push(Line::from(vec![
                Span::styled(format!("  {} ", row.state.glyph()), glyph_style),
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
            TranscriptEntry::Error { text } => {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled("✗ ", theme.err_style()),
                    Span::styled(text.as_str(), theme.err_style()),
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
    if band_rule_h > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(band_rule_area.width as usize),
                theme.frame_style(),
            ))
            .style(theme.text_style()),
            band_rule_area,
        );
    }
}

/// The menu's body lines pre-wrapped by display cells into the menu's
/// content width (sim `.iml` white-space: pre-wrap, tui.js:4946).
fn wrapped_menu_body(menu: &haider_protocol::menu::Menu, width: u16) -> Vec<(String, DiffTone)> {
    let budget = (width as usize).saturating_sub(2).max(1);
    menu.body
        .iter()
        .flat_map(|body_line| {
            // Classify per LOGICAL line (pre-wrap), so a wrapped
            // continuation of a long `+` row stays green.
            body_line.split('\n').flat_map(move |logical| {
                let tone = DiffTone::of(logical);
                wrap_body(logical, budget)
                    .into_iter()
                    .map(move |row| (row, tone))
            })
        })
        .collect()
}

/// W4a4: diff-aware body tone for the approval card (and any menu whose
/// body carries a patch preview). The daemon's `approval_preview` emits
/// `-`/`+` prefixed preimage/replacement lines and `---`/`+++` headers
/// (haider-tools filesystem.rs); everything else stays the dim body tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffTone {
    Body,
    Add,
    Del,
    Meta,
}

impl DiffTone {
    fn of(line: &str) -> Self {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            Self::Meta
        } else if line.starts_with('+') {
            Self::Add
        } else if line.starts_with('-') {
            Self::Del
        } else {
            Self::Body
        }
    }

    fn style(self, theme: &Theme) -> ratatui::style::Style {
        match self {
            Self::Body => theme.dim_style(),
            Self::Add => theme.ok_style(),
            Self::Del => theme.err_style(),
            Self::Meta => theme.faint_style(),
        }
    }
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
    for (body_row, tone) in body_rows {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(body_row, tone.style(theme)),
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

/// Rows the `/theme` picker card needs: title + body + the five choice
/// rows + hint (menu_block windows the options under height pressure).
const THEME_PICKER_ROWS: u16 = 8;

/// Whether the `/theme` picker renders THIS frame: open, on a surface
/// that hosts it, with no daemon card holding the input slot (the menu /
/// ask branches outrank it — local chrome never sits on a live ask).
fn theme_picker_showing(model: &AppModel) -> bool {
    model.theme_picker.is_some()
        && matches!(model.screen, Screen::Launcher | Screen::Session)
        && model.projection.open_menu().is_none()
        && model.login.is_none()
}

/// The `/theme` picker (owner spec §3): a numbered arrow-highlight card in
/// the composer's slot, rendered through the SAME `menu_block` anatomy as
/// every other card. The ● marks the COMMITTED choice; the ❯ highlight
/// previews live as it moves. Hits carry [`Hit::ThemeOption`] so hover
/// previews and a click commits.
fn render_theme_picker(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    rule_area: Rect,
    composer_area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let Some(picker) = model.theme_picker else {
        return;
    };
    use crate::theme::ThemeChoice;
    use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
    let committed = |choice: ThemeChoice| {
        if choice == picker.prior { '●' } else { '○' }
    };
    let options = ThemeChoice::MENU
        .iter()
        .map(|choice| {
            let blurb = match choice {
                ThemeChoice::System => "follow the terminal · auto light / dark",
                ThemeChoice::Fixed(key) => match key {
                    crate::theme::ThemeKey::Light => "paper & ink",
                    crate::theme::ThemeKey::Dark => "aged gold on warm black",
                    crate::theme::ThemeKey::Desert => "sand · amber · dusk",
                    crate::theme::ThemeKey::Oasis => "palm night · date gold",
                },
            };
            MenuOption {
                key: choice.name().to_owned(),
                label: format!("{} {} — {blurb}", committed(*choice), choice.name()),
                detail: None,
                decision: None,
            }
        })
        .collect();
    let card = Menu {
        id: haider_protocol::ids::MenuId::new("theme-picker"),
        kind: MenuKind::Choice,
        title: "theme — how haider dresses this terminal".to_owned(),
        body: vec!["previews as you highlight · system re-reads the terminal each boot".to_owned()],
        options,
        blocking: false,
        scope: MenuScope::Session,
        origin: "theme".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(rule_area.width as usize),
            theme.warn_style(),
        ))
        .style(theme.text_style()),
        rule_area,
    );
    let footer = " ↑↓ preview · ⏎ keep · 1-5 quick · esc back";
    let (lines, option_rows) = menu_block(&card, picker.selection, theme, composer_area, footer);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.menu_style()),
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
                Hit::ThemeOption(option_index),
            ));
        }
    }
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
        MenuKind::Choice if menu.origin == "theme" => "◑",
        _ => "?",
    }
}

/// Display cells one composer text row may fill (TUI6): the frame width
/// minus the left pad, the 2-cell `❯ `/`⋮ `/indent gutter, and ONE cell
/// reserved for the line-end caret block. The reserve applies to EVERY
/// row — not just the cursor row — so wrap points depend on the width
/// alone: moving the caret can never move a wrap point (item 5's
/// derive-from-width law).
/// TUI6.1 fix 2 — the closing-rule reservation law (review r1 finding 2),
/// stated ONCE and, since TUI6.2 fix 6, the RUNTIME authority on every
/// surface ledger (r2 finding 6: launcher/aura had duplicated arithmetic
/// tied to this function only by debug_asserts, which compile out —
/// their rule rows are now DERIVED from it): the band's lower rule
/// is RESERVED whenever the rows that outrank it — the surface's
/// surviving chrome, the top input rule, the sacred input floor, and the
/// transcript's sacred row where the surface has one — still leave it a
/// row (`area_h > outranking`). The rule outranks every OPTIONAL row:
/// panels (palette, ⧗ queue, SubTree, todos, the waiting line), the band
/// pad, breathing rows, the launcher's content column and aura's
/// orb/columns. It sheds only when the top-rule + input + lower-rule
/// triple itself cannot fit (and dies with the top rule, `top_rule_h`).
/// The reviewer's five failing frames — launcher 90×4, session+chip
/// 90×11, session menu 90×10, subagent 90×11 / question 90×14, aura
/// 90×10 — are the height-sweep pins in `tui6_softwrap_tests`.
fn band_rule_reserve(area_h: u16, outranking: u16, top_rule_h: u16) -> u16 {
    u16::from(top_rule_h > 0 && area_h > outranking)
}

pub(crate) fn composer_text_budget(width: u16) -> usize {
    (width as usize).saturating_sub(COMPOSER_PAD + 2 + 1).max(1)
}

/// Composer rows currently needed: one VISUAL row per wrapped row of the
/// draft (TUI6 item 1 — one row initially, growth by WRAPPING), capped at
/// [`COMPOSER_MAX_ROWS`] (sim textarea autoGrow, tui.js:2799-2803 — the
/// sim's ONE `<textarea rows={1}>` soft-wraps and autoGrows on every
/// surface, tui.js:3004-3027); beyond the cap the composer windows
/// vertically around the caret.
fn composer_height(model: &AppModel, width: u16) -> u16 {
    // The masked login card REPLACES the composer while it is open
    // (W3c3 M3): title, masked field, hint.
    if model.login.is_some() {
        return LOGIN_CARD_ROWS;
    }
    // The `/theme` picker replaces the composer on its surfaces (owner
    // spec §3) — same input-replacement law as a blocking menu. A daemon
    // card outranks it (render_session keeps the menu branch first).
    if theme_picker_showing(model) {
        return THEME_PICKER_ROWS;
    }
    let rows = crate::composer::wrap_rows(model.composer.text(), composer_text_budget(width))
        .len()
        .clamp(1, COMPOSER_MAX_ROWS);
    // B4b: pending attachment chips claim ONE row above the text rows —
    // height and paint (`render_composer`) share this same predicate, so
    // the band's geometry can never disagree with what lands in it.
    let chips = u16::from(model.composer.has_attachments());
    u16::try_from(rows).unwrap_or(1).saturating_add(chips)
}

/// The gold rule + composer rows on the input ground (sim InputBar,
/// tui.js:5395: `border-top: gold`, `background: inputBg`). Pushes the
/// talk-chip hit region so the click lands exactly on the chip.
///
/// BAND ANATOMY (TUI6 item 6, per Claude Code's own TUI): every surface
/// that draws an input band closes it with a rule BELOW as well as the
/// rule above. The sweep's enumeration of input-band render paths and
/// where each closing rule lives:
///   - `render_launcher`  — `band_rule_area` (TUI5 item 1b, gap→rule);
///   - `render_session`   — `band_pad` + `band_rule_area`, on BOTH the
///     composer and blocking-menu forms (the rule renders outside the
///     menu if/else);
///   - `render_subagent`  — `band_pad` + `band_rule_area` (TUI6 — the
///     owner's screenshot), composer and question-card forms alike;
///   - `render_aura`      — `band_rule_area` (TUI6, gap→rule);
///   - the login card and the arg-slot/palette state REPLACE the
///     composer's CONTENT inside the same band, so they inherit the
///     hosting surface's two rules — no separate path exists.
///
/// Each surface's pair is pinned by a test in `tui6_softwrap_tests`.
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
    // Ground the WHOLE region in inputBg before any text lands, so every row
    // of the band — including rows the composer does not fill — is covered
    // edge to edge (owner item 2).
    frame.render_widget(Block::default().style(theme.input_style()), row_area);
    // B4b: the pending-attachment chip row rides the TOP of the band
    // (same predicate as `composer_height`, so the row it paints is the
    // row the layout granted). The text rows — and their click windows —
    // shift down by exactly the carved row.
    let mut row_area = row_area;
    if model.login.is_none() && model.composer.has_attachments() && row_area.height > 1 {
        frame.render_widget(
            Paragraph::new(attachment_chip_line(model, theme)).style(theme.input_style()),
            Rect {
                height: 1,
                ..row_area
            },
        );
        row_area.y += 1;
        row_area.height -= 1;
    }
    let (lines, chip_at, windows) = composer_lines(model, theme, row_area.width, row_area.height);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.input_style()),
        row_area,
    );
    // The chip's rect goes FIRST: hit_at takes the first match, so the
    // chip keeps its cells over the row-wide text region below.
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
    // TUI5 item 5: each composer text row is a value-carrying click/drag
    // region — from its visible content's first cell to the row edge
    // (clicks right of the text clamp to the line end, native-input law;
    // the sigil/gutter columns are frame chrome and take no caret).
    for window in windows {
        if window.row >= row_area.height || window.content_x >= row_area.width {
            continue;
        }
        hits.push((
            Rect {
                x: row_area.x + window.content_x,
                y: row_area.y + window.row,
                width: row_area.width - window.content_x,
                height: 1,
            },
            Hit::ComposerText {
                start: window.start,
                content: window.content,
                surface: model.surface_key(),
                revision: model.composer.revision(),
                epoch: model.geometry_epoch.get(),
            },
        ));
    }
}

/// The pending-attachment chip row (B4b): one bracket chip per draft
/// attachment, `⋯` while its upload is in flight, a dim removal hint
/// after. Display truth only — every chip here is a block the next
/// submit really carries (or an upload really in flight), nothing more.
fn attachment_chip_line(model: &AppModel, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(COMPOSER_PAD))];
    for chip in model.composer.attachments() {
        let suffix = if chip.artifact.is_none() { " ⋯" } else { "" };
        spans.push(Span::styled(
            format!("[⌁ {}{suffix}]", chip.label),
            theme.gold_style(),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        "· ⌫ at the start removes".to_owned(),
        theme.dim_style(),
    ));
    Line::from(spans)
}

/// One composer text row's clickable window (TUI5 item 5): where its
/// visible content starts on screen and WHAT that content is (byte-exact),
/// so the runtime can map a click column to a caret byte through the same
/// values that rendered. Since TUI6 the rows are WRAP segments of the
/// draft — `start` is the segment's absolute byte offset and `content` its
/// text, so a click on any wrapped row lands on that row's own graphemes.
struct ComposerRowWindow {
    row: u16,
    content_x: u16,
    start: usize,
    content: String,
}

/// Rows the masked login card claims: title · alias · key · hint.
const LOGIN_CARD_ROWS: u16 = 4;

/// The masked `/login … api` card (W3c3 M3 — report R10).
///
/// The renderer is handed a LENGTH, never the key. That is the whole
/// design: no frame can carry the secret, so no snapshot, no scrollback,
/// no drag-selection copy and no `⌃C` can either. The mask is also CAPPED,
/// so a long key does not advertise its length across the terminal.
fn login_lines(card: &crate::app::LoginCard, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    use crate::app::{LoginFocus, LoginStage};
    const MASK_CAP: usize = 32;
    let inner = usize::from(width).saturating_sub(4).max(1);
    let title = format!("  ⚿ {} · API key", card.provider);
    // The caret marks the field the next keystroke lands in (§5.3: two
    // fields, tab switches). A closed card carets neither.
    let editing = card.accepts_input();
    let caret = |focused: bool| if focused { "▏" } else { "" };
    let alias_row = format!(
        "  alias ❯ {}{}",
        card.alias,
        caret(editing && card.focus == LoginFocus::Alias)
    );
    let key_caret = caret(editing && card.focus == LoginFocus::Key);
    let field = match &card.stage {
        // A failed card still shows its mask: it accepts the retype the
        // recovery text asks for (review P2-1).
        LoginStage::Entry | LoginStage::Failed(_) if !card.is_empty() => {
            let shown = card.masked_len().min(MASK_CAP);
            let mask = "•".repeat(shown);
            // JUSTIFIED `…` SURVIVOR (TUI6 item 1 names each): this is
            // the secrecy CAP, not a caret window — the mask stops
            // advertising a long key's length. The composer's
            // no-ellipsis law governs DRAFT text; the mask renders no
            // draft byte at all. (The other band survivor is the
            // `◉ listening…` chip label — sim-verbatim chrome.)
            let more = if card.masked_len() > MASK_CAP {
                "…"
            } else {
                ""
            };
            format!("  key   ❯ {mask}{more}{key_caret}")
        }
        LoginStage::Submitting => "  key   ❯ validating…".to_owned(),
        LoginStage::Failed(text) => format!("  ✗ {text}"),
        LoginStage::Entry => format!("  key   ❯ {key_caret}"),
        LoginStage::Done(identity) => format!("  ✓ signed in · {identity}"),
    };
    let hint = match &card.stage {
        LoginStage::Entry => {
            "    the key is masked and never stored · tab field · ⏎ commit · esc cancel"
        }
        LoginStage::Submitting => "    staging and validating with the provider…",
        LoginStage::Failed(_) => "    ⏎ try again · tab field · esc cancel",
        LoginStage::Done(_) => "    esc closes",
    };
    let clip = |text: String| -> String { text.chars().take(inner + 2).collect() };
    vec![
        Line::styled(clip(title), theme.gold_style()),
        Line::styled(clip(alias_row), theme.text_style()),
        Line::styled(clip(field), theme.text_style()),
        Line::styled(clip(hint.to_owned()), theme.dim_style()),
    ]
}

/// The composer rows (sim InputBar textarea): padded off the frame edge,
/// bold gold ❯ sigil, REAL newlines on their own rows, overlong lines
/// SOFT-WRAPPED at grapheme boundaries into visual rows (TUI6 items 1-5 —
/// the sim's one `<textarea rows={1}>` wraps and autoGrows, tui.js:3004),
/// typed text bright with a gold block cursor (or the dim placeholder +
/// ghost completion), and the right-aligned `[ ◉ talk ]` chip on the first
/// row. NO horizontal windowing and NO `…` in the composer, ever — the
/// TUI5 caret-following window died with the wrap.
///
/// `allocated` is the height the layout actually granted: the composer
/// VERTICALLY tail-windows to it over VISUAL rows (last rows win — the
/// cursor row is sacred at any size, review r3 P2-1a), with a faint ⋮
/// gutter marker when rows are hidden above or below. Returns the rows
/// plus the chip's column offset + width.
///
/// (W3c3.1, review D3-7: the M3 login card was inserted BETWEEN this doc
/// comment and the function it documents, orphaning both it and the
/// `type_complexity` allow onto a `u16` constant. Both are back where they
/// belong.)
#[allow(clippy::type_complexity)]
fn composer_lines<'a>(
    model: &'a AppModel,
    theme: &Theme,
    width: u16,
    allocated: u16,
) -> (Vec<Line<'a>>, Option<(u16, u16)>, Vec<ComposerRowWindow>) {
    // TUI6.2 fix 2 (review r2 finding 2): the frame's wrap budget is
    // published for EVERY branch — the empty-composer return kept a
    // fresh surface at budget 0, so type-then-queued-navigation before
    // the next redraw walked LOGICAL lines (cursor 4 where budget 13's
    // wrapped rows land 17). The budget is a function of the width
    // alone; an empty draft and the login card still occupy a band of
    // this width, so their frames publish it too.
    let budget = composer_text_budget(width);
    model.composer.set_wrap_budget(budget);
    // The masked login card owns the input band while it is open. It emits
    // NO click/drag windows and NO talk chip: a composer text window
    // carries its CONTENT for caret mapping, which is precisely the thing
    // that must not exist for a secret.
    if let Some(card) = model.login.as_ref() {
        let mut lines = login_lines(card, theme, width);
        lines.truncate(usize::from(allocated).max(1));
        return (lines, None, Vec::new());
    }
    let sigil = Span::styled(
        "❯ ",
        theme
            .gold_style()
            .add_modifier(ratatui::style::Modifier::BOLD),
    );
    // Sim `.mic` (tui.js:5467-5489): FRAME chrome, gold label; hover
    // turns the border gold; a live hold shows `◉ listening…`.
    // A LIVE hold wears the sim's `.mic.live` treatment: maroon ink and
    // chrome, pulsing (tui.js:5484-5489, 1.1s); otherwise frame chrome,
    // gold on hover.
    let (talk_chrome, talk_ink) = if model.listening {
        let live = theme.pulse_ink(theme.maroon, model.anim_phase);
        (live, live)
    } else if model.hovered == Some(Hit::TalkChip) {
        (theme.gold_style(), theme.gold_style())
    } else {
        (theme.frame_style(), theme.gold_style())
    };
    let talk_label = if model.listening {
        "◉ listening…"
    } else {
        "◉ talk"
    };
    let chip_spans = chip_two_tone(talk_label.to_owned(), talk_chrome, talk_ink);
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
        // TUI5 item 1: the cursor is a styled CELL — a themed block before
        // the dim placeholder (Claude Code shows exactly this), never an
        // appended glyph. Steady by law (no blink, no animated() term).
        let mut spans = vec![
            Span::raw(" ".repeat(COMPOSER_PAD)),
            sigil,
            Span::styled(" ", theme.cursor_style()),
            Span::styled(format!(" {placeholder}"), theme.dim_style()),
        ];
        let chip_at = chip_fit(&mut spans);
        // The empty composer is still clickable (item 5): the caret can
        // only land at 0, but the press must not fall through to hits
        // beneath the band.
        let windows = vec![ComposerRowWindow {
            row: 0,
            content_x: u16::try_from(COMPOSER_PAD + 2).unwrap_or(4),
            start: 0,
            content: String::new(),
        }];
        return (vec![Line::from(spans)], chip_at, windows);
    }

    let text = model.composer.text();
    let cursor = model.composer.cursor();
    let selection = model.composer.selection_range();
    // Visual rows: the draft wrapped at grapheme boundaries into the
    // frame's budget (TUI6 item 1), published above for every branch so
    // ↑/↓ walk the SAME rows this frame paints (the scroll_max Cell
    // pattern — geometry feedback, never stored wrap points; the
    // reducer stays put).
    let row_bounds = crate::composer::wrap_rows(text, budget);
    let cursor_row_index = crate::composer::visual_row_of(&row_bounds, cursor);
    let total = row_bounds.len();
    let window = (allocated.max(1) as usize).min(COMPOSER_MAX_ROWS);
    // Vertical window over VISUAL rows: prefer the TAIL (the editable
    // end), but the CURSOR row is sacred — moving ↑ into scrolled-out
    // rows scrolls the window up to keep the caret visible (item 2's
    // caret-follows law).
    let mut skip = total.saturating_sub(window);
    if cursor_row_index < skip {
        skip = cursor_row_index;
    }
    let visible = &row_bounds[skip..(skip + window).min(total)];
    let hidden_below = skip + visible.len() < total;
    let last = visible.len().saturating_sub(1);
    let mut rows = Vec::new();
    let mut chip_at = None;
    let mut windows = Vec::new();
    for (index, row) in visible.iter().enumerate() {
        let first_row = index == 0;
        let last_row = index == last;
        let mut spans = vec![Span::raw(" ".repeat(COMPOSER_PAD))];
        if first_row && skip == 0 {
            spans.push(sigil.clone());
        } else if first_row {
            // Earlier rows are scrolled out above (vertical tail window).
            spans.push(Span::styled("⋮ ", theme.faint_style()));
        } else if last_row && hidden_below {
            // Later rows are scrolled out below (the cursor pulled the
            // window up) — same honesty as the ⋮ above.
            spans.push(Span::styled("⋮ ", theme.faint_style()));
        } else {
            spans.push(Span::raw("  "));
        }
        composer_row_spans(&mut spans, text, *row, cursor, selection, theme);
        if skip + index == cursor_row_index {
            // Inline ghost completion (sim `.ghostline`, tui.js:3028-3034)
            // — it rides the CARET'S visual row (an overlong palette query
            // wraps like any draft, so this is not always row 0).
            if let Some(ghost) = model.ghost() {
                spans.push(Span::styled(ghost, theme.dim_style()));
                spans.push(Span::styled(" ⇥ tab", theme.faint_style()));
            }
        }
        windows.push(ComposerRowWindow {
            row: u16::try_from(index).unwrap_or(u16::MAX),
            content_x: u16::try_from(COMPOSER_PAD + 2).unwrap_or(u16::MAX),
            start: row.start,
            content: text[row.start..row.end].to_owned(),
        });
        if first_row {
            chip_at = chip_fit(&mut spans);
        }
        rows.push(Line::from(spans));
    }
    (rows, chip_at, windows)
}

/// One visual row's text spans — since TUI6 the ONE renderer for every
/// composer row (the TUI5 cursor-row/plain-row split died with the
/// horizontal caret window; there is nothing left to ellipsize). Groups
/// the row's graphemes into style runs: the selection band on covered
/// cells, the cursor CELL (which WINS over the band — the active end must
/// stay distinct, TUI5 item 4) in reverse-video, plain draft ink
/// otherwise, and the line-end caret block over a space when the caret
/// owns this row's end (`line_last` — the position ON the `\n` or at the
/// text end; a caret ON a wrap point renders on the FOLLOWING row, the
/// `WrapRow` no-affinity law, so wrap rows never draw the end block).
fn composer_row_spans<'s>(
    spans: &mut Vec<Span<'s>>,
    text: &str,
    row: crate::composer::WrapRow,
    cursor: usize,
    selection: Option<(usize, usize)>,
    theme: &Theme,
) {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let visible = &text[row.start..row.end];
    let mut run = String::new();
    let mut run_style = theme.bright_style();
    for (grapheme_offset, grapheme) in visible.grapheme_indices(true) {
        let abs = row.start + grapheme_offset;
        let style = if abs == cursor {
            theme.cursor_style()
        } else if selection.is_some_and(|(start, end)| abs >= start && abs < end) {
            theme.composer_selection_style()
        } else {
            theme.bright_style()
        };
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        // TUI6.1 fix 3: a ZERO-WIDTH cluster (a combining mark or ZWJ
        // standing alone at a line start, reachable by paste) gets a
        // SPACE BASE — one real cell, the terminal convention for a bare
        // mark. This is the render half of `composer::cluster_cells`'s
        // one-cell price: wrap, click and navigation already charge the
        // cluster one cell, so painting it at zero cells hid the caret
        // and skewed every column right of it by one (review r1
        // finding 3).
        if grapheme.width() == 0 {
            run.push(' ');
        }
        run.push_str(grapheme);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    if row.line_last && cursor == row.end {
        // Line-end caret (also end-of-text): the block over a space, in
        // the cell composer_text_budget reserved on every row.
        spans.push(Span::styled(" ", theme.cursor_style()));
    }
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
                Hit::PaletteRow(item.clone()),
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
    // W7b meter truth: the durable occupancy snapshot beats the
    // cumulative usage sum, and an ESTIMATED snapshot wears `~` so the
    // meter never claims a precision it does not have.
    let (tokens, window, approx) = model.projection.latest_footprint().map_or_else(
        || {
            (
                model.projection.context_tokens(),
                identity.context_window,
                "",
            )
        },
        |footprint| {
            (
                footprint.used_tokens,
                footprint.context_window.unwrap_or(identity.context_window),
                match footprint.truth {
                    haider_protocol::context::ContextFootprintTruth::Exact => "",
                    haider_protocol::context::ContextFootprintTruth::Estimated => "~",
                },
            )
        },
    );
    #[allow(clippy::cast_precision_loss)]
    let pct = if window == 0 {
        0.0
    } else {
        tokens as f64 / window as f64
    };
    let meter = format!(
        "{approx}{} tok · {} {}% of {}",
        fmt_tok(tokens),
        meter_cells(pct, METER_CELLS_DEFAULT),
        (pct.clamp(0.0, 1.0) * 100.0).round(),
        fmt_tok(window)
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
    // Sim Badge pulse (tui.js:5558-5563): WAITING/STARTING/PERMISSION/
    // EFFECT_UNKNOWN breathe (all outlined states, so dimming the fg IS
    // the sim's opacity dip); everything else holds steady.
    let (badge_chrome, badge_ink) = if crate::projection::badge_pulses(&badge) {
        (
            theme.pulse(badge_chrome, model.anim_phase),
            theme.pulse(theme.badge_style(tone), model.anim_phase),
        )
    } else {
        (badge_chrome, theme.badge_style(tone))
    };
    left.extend(chip_two_tone(badge, badge_chrome, badge_ink));
    // Sim `.mid`: model · provider, plus the branch name inside a session,
    // plus ` · q:turn` while queue mode holds (tui.js:2840-2842). B2b: the
    // ACTIVE branch's name — "main" on the main branch, the daemon-named
    // fork otherwise.
    let branch = if model.screen == Screen::Session {
        format!(" · {}", model.active_branch_name())
    } else {
        String::new()
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
    // H4: the decision-hook chip — visible exactly while the CURRENT run's
    // permission was answered by a decision hook (journaled fact → chip;
    // a proposal the menu CAS did not apply never lights it). Session
    // surfaces only: the chip is that session's automation, not the
    // launcher's.
    if matches!(
        model.screen,
        Screen::Session | Screen::Subagent | Screen::Hooks | Screen::Tree | Screen::Tools
    ) && model.hook_facts.decision_chip()
    {
        left.extend(chip_two_tone(
            "⚙ hook·decided".to_owned(),
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
    phase: u8,
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
        TranscriptEntry::Item(block) => item_lines(lines, block, theme, width, phase),
        TranscriptEntry::Note { text } => {
            // Sim NoteRow (tui.js:4572-4577): dim, indented off the margin.
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(text.as_str(), theme.dim_style()),
            ]));
        }
        TranscriptEntry::Error { text } => {
            // The failed run's public reason (W5g-6) — err ink, ✗ sigil.
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("✗ ", theme.err_style()),
                Span::styled(text.as_str(), theme.err_style()),
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
fn todo_row<'a>(item: &'a TodoItem, all: &[TodoItem], theme: &Theme, phase: u8) -> Line<'a> {
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
        // Sim `.processing .tbox`: the working item's box pulses gold
        // (tui.js:4694-4697, 1.2s); its text stays steady bright.
        TodoState::Processing => (
            "■",
            theme.pulse_ink(theme.gold, phase),
            theme.bright_style(),
        ),
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

fn item_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    block: &'a ItemBlock,
    theme: &Theme,
    width: u16,
    phase: u8,
) {
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
                        // Sim ToolRow `.glyph` while running: warn ink,
                        // PULSING (tui.js:4524-4530, 1.1s).
                        haider_protocol::item::ToolStatus::Pending
                        | haider_protocol::item::ToolStatus::InProgress => {
                            theme.pulse_ink(theme.warn, phase)
                        }
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
            // W8b (research risk 10): a process tool's streamed output was
            // durably RETAINED but never rendered — show the bounded tail
            // exactly as a direct command row does, honesty markers
            // included.
            if !block.output_tail.is_empty() {
                for line in block.output_text().lines() {
                    lines.push(Line::styled(format!("    {line}"), theme.faint_style()));
                }
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
