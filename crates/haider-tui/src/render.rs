//! Screen renderers: pure functions of ([`AppModel`], frame area) → buffer.
//! Testable headlessly via `TestBackend` (research rec 16/18). Every color
//! comes from the theme's style vocabulary — no literals (rec 12).
//! Visual authority: the `/tui` sim — typography, chips, and row shapes are
//! copied from it deliberately.

use crate::app::{AppModel, Hit, LauncherRow, LoomPane, Screen, update_version_label};
use crate::boot::{boot_subline, check_rows, launcher_subline};
use crate::commands::{HELP_INTRO_TEXT, PALETTE_MAX_ROWS, help_catalog_lines};
use crate::format::{METER_CELLS_DEFAULT, fmt_elapsed, fmt_tok, meter_cells};
use crate::plain::status_glyph;
use crate::projection::{ItemBlock, SessionProjection, TranscriptEntry};
use crate::sanctum::SanctumLine;
use crate::theme::{Theme, ThemeKey};
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::item::TurnItem;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};
use std::collections::BTreeMap;

const TRANSCRIPT_OVERSCAN_ROWS: u64 = 2;
const TRANSCRIPT_CACHE_ENTRIES: usize = 96;
const TRANSCRIPT_EAGER_ENTRIES: usize = 64;
const EXTREME_ENTRY_BYTES: usize = 64 * 1024;
const EXTREME_LOGICAL_LINE_CHARS: usize = 4 * 1024;

/// Sparse, bounded view cache for transcript entry formatting and wrapped
/// geometry. Unmeasured history keeps an estimated height; entries crossing
/// the viewport are measured on demand and correct that estimate. The raw
/// projection remains authoritative and is never cloned into this cache.
#[derive(Debug, Default)]
pub(crate) struct TranscriptLayoutCache {
    initialized: bool,
    width: u16,
    theme: Option<ThemeKey>,
    revision: u64,
    entry_mutation_revision: u64,
    source_ptr: usize,
    source_len: usize,
    phase: u8,
    entries: BTreeMap<usize, CachedTranscriptEntry>,
    corrections: BTreeMap<usize, i64>,
    default_height: u64,
    user_height: u64,
    total_rows: u64,
}

#[derive(Debug, Clone)]
struct CachedTranscriptEntry {
    lines: Vec<Line<'static>>,
    /// Row inside the entry represented by `lines`. Normally zero; extreme
    /// entries may retain only the visible sub-window.
    line_start: u64,
    retained_height: u64,
    height: u64,
    dynamic: bool,
    windowed: bool,
}

impl TranscriptLayoutCache {
    fn reconcile(
        &mut self,
        projection: &SessionProjection,
        theme_key: ThemeKey,
        theme: &Theme,
        width: u16,
        phase: u8,
    ) {
        let layout_changed =
            !self.initialized || self.width != width || self.theme != Some(theme_key);
        let source = projection.entries();
        let projection_changed = self.revision != projection.render_revision()
            || self.source_ptr != source.as_ptr() as usize
            || self.source_len != source.len();
        let entries_mutated = self.entry_mutation_revision != projection.entry_mutation_revision();
        let phase_changed = self.phase != phase;
        let append_only = self.initialized
            && !layout_changed
            && !entries_mutated
            && projection_changed
            && source.len() > self.source_len;
        if !layout_changed && !projection_changed && !phase_changed {
            return;
        }

        if layout_changed || entries_mutated || (projection_changed && !append_only) {
            self.entries.clear();
            self.corrections.clear();
            self.seed_estimates(projection, theme, width, phase);
        } else if append_only {
            // Projection growth cannot invalidate already-committed rows.
            // Keep their measured geometry and extend the estimated suffix;
            // the visible new tail is measured below on demand. Re-seeding
            // from a short appended row would shift the whole old history.
            if phase_changed {
                self.entries.retain(|_, entry| !entry.dynamic);
            }
            let appended = source.len().saturating_sub(self.source_len);
            if appended <= TRANSCRIPT_EAGER_ENTRIES {
                for index in self.source_len..source.len() {
                    self.materialize(projection, theme, width, phase, index, None);
                }
            }
            self.recompute_total(projection);
        } else if phase_changed {
            // Only live tool rows change with the animation clock. Their
            // measured height remains valid; discard just their styled lines.
            self.entries.retain(|_, entry| !entry.dynamic);
        }

        self.initialized = true;
        self.width = width;
        self.theme = Some(theme_key);
        self.revision = projection.render_revision();
        self.entry_mutation_revision = projection.entry_mutation_revision();
        self.source_ptr = source.as_ptr() as usize;
        self.source_len = source.len();
        self.phase = phase;
    }

    fn seed_estimates(
        &mut self,
        projection: &SessionProjection,
        theme: &Theme,
        width: u16,
        phase: u8,
    ) {
        let source = projection.entries();
        if source.is_empty() {
            self.default_height = 1;
            self.user_height = 1;
            self.total_rows = 0;
            return;
        }

        // Sample a constant number of positions. The last entry is the most
        // useful prior for follow mode; evenly spaced fallbacks avoid choosing
        // a rare tail kind without ever walking all N rows.
        let last = source.len() - 1;
        let samples = [last, 0, last / 2, last / 4, last.saturating_mul(3) / 4];
        let representative = samples
            .into_iter()
            .find(|&index| !matches!(source[index], TranscriptEntry::User { .. }))
            .unwrap_or(last);
        let cached = cache_transcript_entry(&source[representative], theme, width, phase);
        self.default_height = cached.height.max(1);
        self.entries.insert(representative, cached);

        self.user_height =
            projection
                .user_entries()
                .first()
                .map_or(self.default_height, |&index| {
                    let cached = cache_transcript_entry(&source[index], theme, width, phase);
                    let height = cached.height.max(1);
                    self.entries.insert(index, cached);
                    height
                });

        self.recompute_total(projection);
        if source.len() <= TRANSCRIPT_EAGER_ENTRIES {
            for index in 0..source.len() {
                self.materialize(projection, theme, width, phase, index, None);
            }
            self.recompute_total(projection);
        }
    }

    fn base_height(&self, projection: &SessionProjection, index: usize) -> u64 {
        if projection.user_entries().binary_search(&index).is_ok() {
            self.user_height
        } else {
            self.default_height
        }
    }

    fn user_entries_before(projection: &SessionProjection, index: usize) -> usize {
        projection
            .user_entries()
            .partition_point(|entry| *entry < index)
    }

    fn row_start(&self, projection: &SessionProjection, index: usize) -> u64 {
        let users = Self::user_entries_before(projection, index);
        let ordinary = index.saturating_sub(users);
        let base = (ordinary as u64)
            .saturating_mul(self.default_height)
            .saturating_add((users as u64).saturating_mul(self.user_height));
        let correction = self
            .corrections
            .range(..index)
            .fold(0i128, |sum, (_, value)| sum + i128::from(*value));
        if correction < 0 {
            base.saturating_sub(u64::try_from(-correction).unwrap_or(u64::MAX))
        } else {
            base.saturating_add(u64::try_from(correction).unwrap_or(u64::MAX))
        }
    }

    fn recompute_total(&mut self, projection: &SessionProjection) {
        self.total_rows = self.row_start(projection, projection.entries().len());
    }

    fn materialize(
        &mut self,
        projection: &SessionProjection,
        theme: &Theme,
        width: u16,
        phase: u8,
        index: usize,
        window: Option<(u64, u64)>,
    ) {
        if index >= projection.entries().len() {
            return;
        }
        if let Some(cached) = self.entries.get(&index) {
            let covered = window.is_none_or(|(start, end)| {
                !cached.windowed
                    || (cached.line_start <= start
                        && cached.line_start.saturating_add(cached.retained_height) >= end)
            });
            if covered {
                return;
            }
        }
        let cached = cache_transcript_entry_window(
            &projection.entries()[index],
            theme,
            width,
            phase,
            window,
        );
        let base = self.base_height(projection, index);
        let correction = i128::from(cached.height) - i128::from(base);
        if correction == 0 {
            self.corrections.remove(&index);
        } else {
            self.corrections.insert(
                index,
                i64::try_from(correction).unwrap_or(if correction < 0 {
                    i64::MIN
                } else {
                    i64::MAX
                }),
            );
        }
        self.entries.insert(index, cached);
        self.recompute_total(projection);
    }

    fn entry_at_row(&self, projection: &SessionProjection, row: u64) -> usize {
        let len = projection.entries().len();
        let mut low = 0usize;
        let mut high = len;
        while low < high {
            let middle = low + (high - low) / 2;
            if self.row_start(projection, middle) <= row {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low.saturating_sub(1).min(len.saturating_sub(1))
    }

    fn prune(&mut self, center: usize) {
        if self.entries.len() > TRANSCRIPT_CACHE_ENTRIES {
            let mut keys = self.entries.keys().copied().collect::<Vec<_>>();
            keys.sort_unstable_by_key(|index| index.abs_diff(center));
            for index in keys.into_iter().skip(TRANSCRIPT_CACHE_ENTRIES) {
                self.entries.remove(&index);
            }
        }
        if self.corrections.len() > TRANSCRIPT_CACHE_ENTRIES {
            let mut keys = self.corrections.keys().copied().collect::<Vec<_>>();
            keys.sort_unstable_by_key(|index| index.abs_diff(center));
            for index in keys.into_iter().skip(TRANSCRIPT_CACHE_ENTRIES) {
                self.corrections.remove(&index);
            }
        }
    }
}

fn cache_transcript_entry(
    entry: &TranscriptEntry,
    theme: &Theme,
    width: u16,
    phase: u8,
) -> CachedTranscriptEntry {
    cache_transcript_entry_window(entry, theme, width, phase, None)
}

fn cache_transcript_entry_window(
    entry: &TranscriptEntry,
    theme: &Theme,
    width: u16,
    phase: u8,
    window: Option<(u64, u64)>,
) -> CachedTranscriptEntry {
    if let TranscriptEntry::Item(block) = entry
        && let TurnItem::AgentMessage { text } = &block.item
        && text.len() > EXTREME_ENTRY_BYTES
    {
        return cache_extreme_agent_entry(block, text, theme, width, window);
    }
    let mut lines = Vec::new();
    transcript_lines(&mut lines, entry, theme, width, phase);
    let lines = lines.into_iter().map(owned_line).collect::<Vec<_>>();
    let height = wrapped_lines_height(&lines, width);
    let dynamic = matches!(
        entry,
        TranscriptEntry::Item(ItemBlock {
            item: TurnItem::ToolCall {
                status: haider_protocol::item::ToolStatus::Pending
                    | haider_protocol::item::ToolStatus::InProgress,
                ..
            },
            ..
        })
    );
    CachedTranscriptEntry {
        lines,
        line_start: 0,
        retained_height: u64::from(height),
        height: u64::from(height),
        dynamic,
        windowed: false,
    }
}

fn indexed_agent_line(
    text: &haider_protocol::reply::ReplyText,
    starts: &[u32],
    index: usize,
) -> String {
    let start = usize::try_from(starts[index]).unwrap_or(0);
    let end = starts
        .get(index + 1)
        .and_then(|offset| usize::try_from(*offset).ok())
        .unwrap_or(text.len());
    bounded_agent_line(text, start, end)
}

/// Materializes at most the renderer's extreme-line budget from an arena
/// range. A single pathological logical line must not recreate a full reply
/// allocation merely to truncate it immediately afterward.
fn bounded_agent_line(
    text: &haider_protocol::reply::ReplyText,
    start: usize,
    end: usize,
) -> String {
    let Some(line) = text.slice(start..end) else {
        return String::new();
    };
    let mut chars = 0_usize;
    let mut bytes = 0_usize;
    let mut truncated = false;
    line.visit_strs(|segment| {
        if truncated {
            return;
        }
        for character in segment.chars() {
            if chars == EXTREME_LOGICAL_LINE_CHARS {
                truncated = true;
                break;
            }
            chars = chars.saturating_add(1);
            bytes = bytes.saturating_add(character.len_utf8());
        }
    });
    let visible = if truncated {
        line.slice(0..bytes).unwrap_or_default()
    } else {
        line
    };
    let mut visible = visible.to_owned_string();
    if !truncated {
        if visible.ends_with('\n') {
            visible.pop();
        }
    } else {
        visible.push_str(" ⋯ extreme line truncated · /export expands raw text");
    }
    visible
}

fn extreme_agent_body_lines(
    source_line: &str,
    is_last: bool,
    streaming: bool,
    theme: &Theme,
    budget: usize,
) -> Vec<Line<'static>> {
    if budget == 0 {
        return vec![owned_line(Line::from(vec![
            Span::raw(" "),
            Span::styled("▏ ", theme.rail_style()),
        ]))];
    }
    let mut markdown = crate::md::render_markdown(source_line)
        .into_iter()
        .next()
        .unwrap_or_default();
    if streaming && is_last {
        markdown.push_cursor();
    }
    crate::md::wrap_spans(&markdown.spans, budget)
        .into_iter()
        .map(|wrapped| {
            let mut spans = vec![Span::raw(" "), Span::styled("▏ ", theme.rail_style())];
            spans.extend(
                wrapped
                    .into_iter()
                    .map(|span| Span::styled(span.text, theme.md_style(span.kind))),
            );
            owned_line(Line::from(spans))
        })
        .collect()
}

/// Lay out an extreme assistant message from its ingest-time byte-range
/// index. A constant sample estimates cumulative height; only logical lines
/// intersecting the requested row window are parsed, wrapped, and retained.
fn cache_extreme_agent_entry(
    block: &ItemBlock,
    text: &haider_protocol::reply::ReplyText,
    theme: &Theme,
    width: u16,
    window: Option<(u64, u64)>,
) -> CachedTranscriptEntry {
    let budget = (width as usize).saturating_sub(3);
    let starts = &block.agent_line_starts;
    if starts.is_empty() {
        let text = bounded_agent_line(text, 0, text.len());
        let lines = extreme_agent_body_lines(&text, true, block.streaming, theme, budget);
        let height = 2u64.saturating_add(u64::try_from(lines.len()).unwrap_or(u64::MAX));
        return CachedTranscriptEntry {
            retained_height: u64::try_from(lines.len()).unwrap_or(u64::MAX),
            lines,
            line_start: 2,
            height,
            dynamic: false,
            windowed: true,
        };
    }

    let logical_count = starts.len();
    let last = logical_count - 1;
    let mut samples = vec![0, last / 4, last / 2, last.saturating_mul(3) / 4, last];
    samples.sort_unstable();
    samples.dedup();
    let sample_heights = samples
        .iter()
        .map(|&index| {
            let height = extreme_agent_body_lines(
                &indexed_agent_line(text, starts, index),
                index == last,
                block.streaming,
                theme,
                budget,
            )
            .len() as u64;
            (index, height)
        })
        .collect::<Vec<_>>();
    let mut ordered_heights = sample_heights
        .iter()
        .map(|(_, height)| *height)
        .collect::<Vec<_>>();
    ordered_heights.sort_unstable();
    let estimated_rows_per_line = ordered_heights[ordered_heights.len() / 2].max(1);
    let sampled_correction = sample_heights.iter().fold(0i64, |correction, (_, height)| {
        correction.saturating_add(
            i64::try_from(*height).unwrap_or(i64::MAX)
                - i64::try_from(estimated_rows_per_line).unwrap_or(i64::MAX),
        )
    });
    let estimated_height = 2u64
        .saturating_add(
            u64::try_from(logical_count)
                .unwrap_or(u64::MAX)
                .saturating_mul(estimated_rows_per_line),
        )
        .saturating_add_signed(sampled_correction);
    let Some((wanted_start, wanted_end)) = window else {
        return CachedTranscriptEntry {
            lines: Vec::new(),
            line_start: estimated_height,
            retained_height: 0,
            height: estimated_height,
            dynamic: false,
            windowed: true,
        };
    };

    let body_start = wanted_start.saturating_sub(2) / estimated_rows_per_line;
    let body_end = wanted_end
        .saturating_sub(2)
        .div_ceil(estimated_rows_per_line)
        .saturating_add(1);
    let first = usize::try_from(body_start).unwrap_or(usize::MAX).min(last);
    let end = usize::try_from(body_end)
        .unwrap_or(usize::MAX)
        .min(logical_count)
        .max(first + 1);
    let include_header = wanted_start < 2 && first == 0;
    let mut lines = Vec::new();
    if include_header {
        lines.push(Line::default());
        let mut head = vec![Span::raw(" "), Span::styled("■ haider", theme.gold_style())];
        if block.spoken {
            head.push(Span::styled(" · ♪ speaking", theme.faint_style()));
        }
        lines.push(owned_line(Line::from(head)));
    }
    for index in first..end {
        lines.extend(extreme_agent_body_lines(
            &indexed_agent_line(text, starts, index),
            index == last,
            block.streaming,
            theme,
            budget,
        ));
    }
    let line_start = if include_header {
        0
    } else {
        let preceding_sample_correction =
            sample_heights
                .iter()
                .fold(0i64, |correction, (index, height)| {
                    if *index < first {
                        correction.saturating_add(
                            i64::try_from(*height).unwrap_or(i64::MAX)
                                - i64::try_from(estimated_rows_per_line).unwrap_or(i64::MAX),
                        )
                    } else {
                        correction
                    }
                });
        2u64.saturating_add(
            u64::try_from(first)
                .unwrap_or(u64::MAX)
                .saturating_mul(estimated_rows_per_line),
        )
        .saturating_add_signed(preceding_sample_correction)
    };
    let retained_height = u64::try_from(lines.len()).unwrap_or(u64::MAX);
    CachedTranscriptEntry {
        lines,
        line_start,
        retained_height,
        height: estimated_height.max(line_start.saturating_add(retained_height)),
        dynamic: false,
        windowed: true,
    }
}

fn owned_line(line: Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .into_iter()
            .map(|span| Span {
                style: span.style,
                content: std::borrow::Cow::Owned(span.content.into_owned()),
            })
            .collect(),
    }
}

fn wrapped_lines_height(lines: &[Line<'_>], width: u16) -> u16 {
    lines.iter().fold(0u16, |total, line| {
        let height = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width);
        total.saturating_add(u16::try_from(height).unwrap_or(1))
    })
}

/// Select only the contiguous transcript slice intersecting the viewport
/// plus a two-row overscan. Returned scroll coordinates remain global.
struct TranscriptViewport<'a> {
    prefix: &'a [Line<'static>],
    suffix: &'a [Line<'static>],
    scroll_back: u64,
    height: u16,
    width: u16,
}

fn virtualized_transcript_lines(
    cache: &mut TranscriptLayoutCache,
    projection: &SessionProjection,
    theme: &Theme,
    phase: u8,
    viewport: TranscriptViewport<'_>,
) -> (Vec<Line<'static>>, u64, u64, u64) {
    let TranscriptViewport {
        prefix,
        suffix,
        scroll_back,
        height: viewport_height,
        width,
    } = viewport;
    let prefix_rows = u64::from(wrapped_lines_height(prefix, width));
    let suffix_rows = u64::from(wrapped_lines_height(suffix, width));
    let entries_base = prefix_rows;
    let suffix_base = entries_base.saturating_add(cache.total_rows);
    let total = suffix_base.saturating_add(suffix_rows);
    let max_scroll = total.saturating_sub(u64::from(viewport_height));
    let scroll = max_scroll.saturating_sub(scroll_back.min(max_scroll));
    let wanted_start = scroll.saturating_sub(TRANSCRIPT_OVERSCAN_ROWS);
    let wanted_end = scroll
        .saturating_add(u64::from(viewport_height))
        .saturating_add(TRANSCRIPT_OVERSCAN_ROWS);
    let prefix_visible = !prefix.is_empty() && wanted_start < prefix_rows;
    let suffix_visible = !suffix.is_empty() && wanted_end > suffix_base && wanted_start < total;

    let source = projection.entries();
    let mut first = if source.is_empty() {
        0
    } else {
        cache.entry_at_row(projection, wanted_start.saturating_sub(entries_base))
    };
    // A measured correction can move the estimate across the target. A
    // constant number of retries converges for the local viewport without a
    // history scan.
    for _ in 0..3 {
        if source.is_empty() {
            break;
        }
        cache.materialize(projection, theme, width, phase, first, None);
        let revised = cache.entry_at_row(projection, wanted_start.saturating_sub(entries_base));
        if revised == first {
            break;
        }
        first = revised;
    }

    let mut lines = Vec::new();
    if prefix_visible {
        lines.extend_from_slice(prefix);
    }
    let mut index = first;
    while index < source.len() {
        let row = entries_base.saturating_add(cache.row_start(projection, index));
        if row >= wanted_end {
            break;
        }
        let local_start = wanted_start.saturating_sub(row);
        let local_end = wanted_end.saturating_sub(row);
        cache.materialize(
            projection,
            theme,
            width,
            phase,
            index,
            Some((local_start, local_end)),
        );
        if let Some(entry) = cache.entries.get(&index) {
            lines.extend_from_slice(&entry.lines);
        }
        index += 1;
    }
    if suffix_visible {
        lines.extend_from_slice(suffix);
    }
    let base = if prefix_visible {
        0
    } else if let Some(entry) = cache.entries.get(&first) {
        entries_base
            .saturating_add(cache.row_start(projection, first))
            .saturating_add(entry.line_start)
    } else {
        suffix_base
    };
    cache.prune(first);
    cache.recompute_total(projection);
    let total = prefix_rows
        .saturating_add(cache.total_rows)
        .saturating_add(suffix_rows);
    let max_scroll = total.saturating_sub(u64::from(viewport_height));
    let scroll = max_scroll.saturating_sub(scroll_back.min(max_scroll));
    (lines, base, total, scroll)
}

/// Register the visible portion of every durable image row. Geometry uses
/// the same wrapped-row coordinates as transcript virtualization, so a click
/// remains aligned after resizing and scroll-back.
fn image_reveal_hits(
    cache: &TranscriptLayoutCache,
    projection: &SessionProjection,
    prefix_rows: u64,
    scroll: u64,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let viewport_end = scroll.saturating_add(u64::from(area.height));
    for (&index, entry) in &cache.entries {
        let Some(TranscriptEntry::Item(block)) = projection.entries().get(index) else {
            continue;
        };
        let TurnItem::Extension { kind, data } = &block.item else {
            continue;
        };
        let Some((image, _)) = crate::projection::image_created_fact(kind, data) else {
            continue;
        };
        let start = prefix_rows.saturating_add(cache.row_start(projection, index));
        let end = start.saturating_add(entry.height);
        let visible_start = start.max(scroll);
        let visible_end = end.min(viewport_end);
        if visible_start >= visible_end {
            continue;
        }
        hits.push((
            Rect {
                x: area.x,
                y: area.y.saturating_add(
                    u16::try_from(visible_start.saturating_sub(scroll)).unwrap_or(u16::MAX),
                ),
                width: area.width,
                height: u16::try_from(visible_end.saturating_sub(visible_start))
                    .unwrap_or(u16::MAX),
            },
            Hit::RevealPath(image.path),
        ));
    }
}

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
    let persistent_diagnostic = persistent_diagnostic(model);
    let (area, diagnostic) = if persistent_diagnostic.is_some() && area.height > 1 {
        let [diagnostic, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        (body, Some(diagnostic))
    } else {
        (area, None)
    };
    if let (Some(rect), Some((presentation, tone))) = (diagnostic, persistent_diagnostic) {
        frame.render_widget(
            Paragraph::new(diagnostic_banner_line(
                presentation,
                tone,
                theme,
                rect.width,
            )),
            rect,
        );
    }
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
        | Screen::Hooks
        | Screen::Usage
        | Screen::Fleet
        | Screen::Graph
        | Screen::Loom
        | Screen::Sessions => 1,
    };
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(status_height)]).areas(area);
    // F2a: the full-screen /model picker COVERS the body while open —
    // it owns the keys, so it owns the pixels and the hit map too.
    if model.ssh_terminal.is_some() {
        render_ssh_terminal(model, theme, frame, body);
        hits.clear();
    } else if model.model_picker.is_some() {
        render_model_picker(model, theme, frame, body, &mut hits);
    } else {
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
            Screen::Usage => render_usage(model, theme, frame, body, &mut hits),
            Screen::Fleet => render_fleet(model, theme, frame, body, &mut hits),
            Screen::Graph => render_graph(model, theme, frame, body, &mut hits),
            Screen::Loom => render_loom(model, theme, frame, body, &mut hits),
            Screen::Sessions => render_sessions(model, theme, frame, body, &mut hits),
        }
    }
    if model.help_open {
        render_help(model, theme, frame, body);
        hits.clear();
    } else if model.shells_open {
        render_shells_overlay(model, theme, frame, body, &mut hits);
    } else if model.ssh_open {
        render_ssh_overlay(model, theme, frame, body, &mut hits);
    } else if model.monitors_open {
        render_monitors_overlay(model, theme, frame, body, &mut hits);
    }
    if model.lockdown_overlay {
        render_lockdown_overlay(model, theme, frame, body);
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
/// T2: the short placeholder while a talk session is engaged — the wave +
/// chip need the first row's right side, and the three-gesture contract
/// IS the relevant hint.
const PLACEHOLDER_TALK: &str = "speak — ⏎ send · esc cancel";
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

/// The voice/dictation chip, moved from the status bar to the header's
/// TOP-RIGHT (owner request): `[ ◉ voice · <route> ]` while voice is on, `None`
/// otherwise. This is the PERSISTENT route indicator — the live `listening…`
/// state stays on the composer's talk chip and its wave, so this chip never
/// collides with the wave-row heuristics. Rendered right-aligned over the
/// header's top row on both the launcher and session.
fn voice_header_chip<'a>(model: &AppModel, theme: &Theme) -> Option<Vec<Span<'a>>> {
    if !model.voice.enabled {
        return None;
    }
    Some(chip_two_tone(
        format!("◉ voice · {}", model.voice.bar_label()),
        theme.frame_style(),
        theme.gold_style(),
    ))
}

/// Paint the voice chip at the RIGHT end of the header's top row — over a
/// rect exactly the chip's width, so the left-aligned product line on the
/// same row is never cleared. `left_used` is the display width the product
/// line already occupies; the chip is dropped (narrow terminal) when it would
/// overlap that content, and hidden entirely when voice is off.
fn render_header_voice_chip(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    header_area: Rect,
    left_used: u16,
) {
    if header_area.height == 0 {
        return;
    }
    if let Some(spans) = voice_header_chip(model, theme) {
        let chip_w = u16::try_from(Line::from(spans.clone()).width()).unwrap_or(0);
        if chip_w > 0 && left_used.saturating_add(2).saturating_add(chip_w) <= header_area.width {
            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect {
                    x: header_area.x + header_area.width - chip_w,
                    y: header_area.y,
                    width: chip_w,
                    height: 1,
                },
            );
        }
    }
}

/// Pad a span row to a fixed display width — the launcher's `.recent`
/// column trick: uniform-width lines center to one shared left edge, and
/// hover bands span the whole column.
/// The `● thinking…` tail (sim `.thinking`, tui.js:4458-4462): gold ink
/// pulsing on the shared clock, the dot breathing ● ↔ ◌ with it (glyph
/// alternation is a port taste-call — a dimmed cell alone can read flat on
/// low-contrast terminals; one law for the session and chip views).
fn thinking_line(theme: &Theme, phase: u8, truecolor: bool) -> Line<'static> {
    // The dot glyph carries its trailing space statically (`● `/`◌ `) so the
    // pulse span costs no per-frame allocation.
    let dot = if phase.is_multiple_of(2) {
        "● "
    } else {
        "◌ "
    };
    // W-E: the STATUS VERB carries a left→right brightness wave (per-glyph
    // shimmer spans on the shared clock); the leading dot keeps its uniform
    // pulse + ● ↔ ◌ breath (the shimmer is ADDITIVE to the verb — the dot
    // stays as is), and the trailing `…` is decoration, base ink, never
    // shimmered (decision 1 / LE3).
    const VERB: &str = "thinking";
    // W-E render allocs: static per-glyph `&str`s replace 8 one-char `String`s
    // per repaint (~240/s at 30 fps). Kept in lockstep with VERB — the shimmer
    // laws pin `VERB.chars()`, and the debug_assert catches any drift.
    const VERB_GLYPHS: [&str; 8] = ["t", "h", "i", "n", "k", "i", "n", "g"];
    debug_assert_eq!(
        VERB_GLYPHS.concat(),
        VERB,
        "the glyph table must spell the verb"
    );
    let len = VERB_GLYPHS.len();
    let mut spans = Vec::with_capacity(len + 3);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(dot, theme.pulse_ink(theme.gold, phase)));
    for (index, glyph) in VERB_GLYPHS.iter().enumerate() {
        spans.push(Span::styled(
            *glyph,
            theme.shimmer_ink(phase, index, len, truecolor),
        ));
    }
    // The `…` wears the shimmer BASE ink (`shimmer_inks()[0]` == the gold
    // accent) statically — the same resting ink the verb falls back to.
    spans.push(Span::styled("…", theme.gold_style()));
    Line::from(spans)
}

/// M4: the transcript-tail retry line, the warn/ember-toned neighbor of
/// `thinking_line`. `✻ API error · Retrying in <N>s · attempt <K>/<max>` while
/// the actor backs off after a retryable provider failure. The countdown shows
/// the committed backoff (the shared clock ticks the render, not a new timer);
/// the whole line wears the warn ink so a transient API error reads distinct
/// from healthy thinking.
fn retrying_line(theme: &Theme, attempt: u32, max: u32, delay_ms: u64) -> Line<'static> {
    let seconds = delay_ms.div_ceil(1_000);
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("✻ API error · Retrying in {seconds}s · attempt {attempt}/{max}"),
            theme.warn_style(),
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
/// One two-tone mark row as styled spans (haidercode.ai port, owner
/// 2026-08-21): strokes in the maroon identity ink, the ya's two dots in
/// GOLD. A mixed cell (baseline stroke over a dot) is `▀` with a gold
/// background so the dot bumps off the rule exactly as the website's
/// blocklogo renders it. Adjacent same-style cells merge into one span.
fn mark_tone_spans(
    cells: &[(char, crate::mark::HalfInk, crate::mark::HalfInk)],
    theme: &Theme,
    ink: Style,
) -> Vec<Span<'static>> {
    use crate::mark::HalfInk;
    let gold = theme.gold_style().add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    for &(glyph, top, bottom) in cells {
        let style = match (top, bottom) {
            (HalfInk::None, HalfInk::None) => Style::default(),
            (HalfInk::Dot | HalfInk::None, HalfInk::Dot) | (HalfInk::Dot, HalfInk::None) => gold,
            (HalfInk::Ink, HalfInk::Dot) => ink.bg(theme.gold.into()),
            (HalfInk::Dot, HalfInk::Ink) => gold.bg(theme.maroon.into()),
            _ => ink,
        };
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(glyph);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    spans
}

fn mark_lines_within(
    model: &AppModel,
    theme: &Theme,
    width: u16,
    banner_budget: u16,
) -> Vec<Line<'static>> {
    let ink = theme.maroon_style().add_modifier(Modifier::BOLD);
    if crate::mark::banner_fits(width) && banner_budget >= crate::mark::BANNER_ROWS {
        return crate::mark::half_block_cells(&crate::mark::BANNER)
            .iter()
            .map(|row| Line::from(mark_tone_spans(row, theme, ink)))
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
    let showing_backtrack = model.backtrack.is_some();
    let palette = if showing_backtrack {
        backtrack_block(model, theme, area.width)
    } else if model.palette_open() {
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
        // letterforms at header scale (24×2 — `mark::HEADER`, the S2
        // half-res-banner rebuild), spanning
        // both band lines exactly as it does beside a session's info block.
        let rows = crate::mark::half_block_cells(&crate::mark::HEADER);
        header_top.extend(mark_tone_spans(&rows[0], theme, mark_ink));
        header_top.push(Span::raw("  "));
        header_bottom.extend(mark_tone_spans(&rows[1], theme, mark_ink));
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
    let header_top_used = u16::try_from(Line::from(header_top.clone()).width()).unwrap_or(0);
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
    // The voice/dictation chip in the TOP-RIGHT of the launcher header (owner:
    // moved off the status bar).
    render_header_voice_chip(model, theme, frame, header_area, header_top_used);
    // Replace the half-block header mark with the crisp حيدر image on a
    // graphics terminal — same 24×2 footprint at the band's lead cell.
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
    // block. HORIZONTALLY the LAUNCHER_COLS-capped block CENTERS in the
    // frame (owner screenshot — the settled launcher left-anchored a
    // ~70-col block in a wide terminal): the pad derives from the CAP, not
    // the built rows, so the block's left edge is stable across frames (a
    // shellout appearing never makes the block jump), and at widths ≤ the
    // cap the pad is zero — exactly the old left-anchored geometry. The
    // header band above and the composer band below stay full-width.
    let center_pad =
        u16::try_from((area.width as usize).saturating_sub(LAUNCHER_COLS) / 2).unwrap_or(0);
    // ONE rect for paint AND hits — shifting only one of them is the
    // W5g-7 hover-offset class of bug, horizontal edition.
    let content_area = Rect {
        x: content_area.x.saturating_add(center_pad),
        width: content_area.width.saturating_sub(center_pad),
        ..content_area
    };
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
    let launcher_session_ids = model.launcher_session_ids();
    let running = model
        .sessions
        .iter()
        .filter(|session| {
            model.session_kinds.get(&session.id) != Some(&haider_rpc::SessionKindWire::Subagent)
        })
        .filter(|session| session.busy())
        .count();
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
    for session_id in launcher_session_ids.iter().take(model.launcher_rows()) {
        let Some(entry) = model
            .sessions
            .iter()
            .find(|session| &session.id == session_id)
        else {
            continue;
        };
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
        if let Some(attention) = model.session_attention.get(&entry.id) {
            if attention.unseen() {
                spans.push(Span::styled(" •", theme.gold_style()));
            }
            if let Some(waiting) = &attention.waiting_why {
                let reason = match waiting.kind {
                    haider_rpc::WaitingWhyKindWire::Permission => "permission",
                    haider_rpc::WaitingWhyKindWire::Question => "question",
                    haider_rpc::WaitingWhyKindWire::Approval => "approval",
                };
                let chip = format!(" needs {reason}");
                if Line::from(spans.clone()).width() + chip.chars().count() + 12 <= area_cap {
                    spans.push(Span::styled(chip, theme.warn_style()));
                }
            }
        }
        // W-flow inline identity: a row whose summary carries a bound agent
        // type wears a small `{glyph} @{id}` chip in the type's registry
        // accent — the SessionSummary.agent_type ↔ loom-snapshot join. The
        // fallback law is exact: no binding, no snapshot entry, or an
        // un-Loom daemon renders today's row untouched; the chip also
        // yields whole when the row is too narrow for it plus some meta.
        if let Some(record) = model.bound_loom_type(entry.agent_type.as_deref()) {
            let chip = if record.glyph.is_empty() {
                format!(" @{}", record.id)
            } else {
                format!(" {} @{}", record.glyph, record.id)
            };
            if Line::from(spans.clone()).width() + chip.chars().count() + 12 <= area_cap {
                let accent = crate::style::loom_accent_style(&record.color)
                    .unwrap_or_else(|| theme.gold_style());
                spans.push(Span::styled(chip, accent.add_modifier(Modifier::BOLD)));
            }
        }
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
        // Launcher fix 2: the row's figures ask the SESSION (turns /
        // row_tokens), which prefers a fresher `session.list` summary over
        // an empty projection — real counts at boot, no attach — and the
        // live/checked-in values whenever the projection has applied at
        // least as far. An Estimated footprint wears the meter's honest
        // `~` prefix.
        let (tokens, tokens_estimated) = entry.row_tokens();
        // Sim renders the blurb segment only when a blurb exists
        // (tui.js:3267 `s.blurb ? … : null`).
        let blurb = entry
            .title
            .as_ref()
            .map(|title| format!(" “{title}” ·"))
            .unwrap_or_default();
        let meta = format!(
            "{blurb} {} · {} {} · {}{} tok · {} · {} · {}",
            // DERIVED (B2b): the seed static plus daemon-installed named
            // branches — the launcher aggregate counts all branches.
            if entry.branches() > 1 {
                format!("{} branches", entry.branches())
            } else {
                "1 branch".to_owned()
            },
            turns,
            if turns == 1 { "turn" } else { "turns" },
            if tokens_estimated { "~" } else { "" },
            fmt_tok(tokens),
            entry.model_short,
            entry.device,
            model.session_display_age(&entry.id, &entry.ago)
        );
        // Sim `.meta`: ellipsized into the column, never clipped.
        let meta_budget = area_cap.saturating_sub(Line::from(spans.clone()).width());
        spans.push(Span::styled(
            ellipsize(&meta, meta_budget),
            theme.dim_style(),
        ));
        recent.push((spans, Some(Hit::AttachSession(entry.id.clone()))));
    }
    // Accounts reflects the local credential registry. The legacy Peers row
    // remains recognizable but advertises no mesh/SSH lane: activating it
    // produces the same typed local-only rejection as durable admission.
    for (row, glyph, name, blurb) in [
        (
            LauncherRow::Aura,
            "◉",
            "Aura",
            "voice session · orchestrator — spawns & steers local sessions, never codes".to_owned(),
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
            "remote placement — not supported · Haider runs local-only".to_owned(),
        ),
        // Owner 2026-08-21: the all-sessions browser — the launcher lists
        // only the most recent handful, this row reaches every one of them
        // with their attention marks.
        (
            LauncherRow::Sessions,
            "≡",
            "All sessions",
            "every session on this machine — unseen + needs-you · /resume".to_owned(),
        ),
        // Sim rows 4+5 (tui.js:6302-6315): the Loom split surfaces.
        (
            LauncherRow::Workflows,
            "⌘",
            "Workflows",
            if model.daemon_serves(haider_rpc::FEATURE_LOOM_PIPE_DAG_V1) {
                "typed pipe DAGs — nodes, gates, conditional edges · /workflows"
            } else {
                "typed sequential workflows · /workflows"
            }
            .to_owned(),
        ),
        (
            LauncherRow::Loom,
            "✦",
            "Loom",
            "Agent Types — capability-scoped specialists · @type to use".to_owned(),
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
    // No VERTICAL centering, no compaction — a hit row IS its painted row
    // (the W5g-7 hover-offset class of bug is unrepresentable here), and
    // the horizontal center pad above moved paint and hits as ONE rect.
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
        if !showing_backtrack {
            palette_row_hits(model, palette_area, hits);
        }
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
fn push_custom_card_lines<'a>(
    model: &'a AppModel,
    theme: &Theme,
    lines_out: &mut Vec<Line<'a>>,
    rects_out: &mut Vec<(usize, u16, u16, Hit)>,
) {
    if let Some(card) = &model.custom_add {
        // G4b: the enterprise kinds retitle the SAME card and relabel its
        // fields; Generic stays byte-for-byte.
        let title = match card.kind {
            crate::app::CustomCardKind::Generic if card.discover_models => {
                "add custom server — local or web"
            }
            crate::app::CustomCardKind::Generic => "add a custom provider — OpenAI-compatible",
            crate::app::CustomCardKind::Azure => "add Azure OpenAI — v1 surface, api-key header",
            crate::app::CustomCardKind::Bedrock => {
                "configure Bedrock — Claude via the mantle bearer surface"
            }
            crate::app::CustomCardKind::Vertex => "configure Vertex — Claude on GCP",
        };
        lines_out.push(Line::from(vec![
            Span::styled("◉ ", theme.gold_style()),
            Span::styled(title, theme.warn_style()),
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
            use crate::app::CustomCardKind;
            // Per-kind field roster: (label, value, field).
            let fields: Vec<(&str, &String, crate::app::CustomField)> = match card.kind {
                CustomCardKind::Generic if card.discover_models => vec![
                    ("alias   ", &card.name, crate::app::CustomField::Name),
                    ("base URL", &card.origin, crate::app::CustomField::Origin),
                ],
                CustomCardKind::Generic => vec![
                    ("name    ", &card.name, crate::app::CustomField::Name),
                    ("origin  ", &card.origin, crate::app::CustomField::Origin),
                    ("model   ", &card.model, crate::app::CustomField::Model),
                ],
                CustomCardKind::Azure => vec![
                    ("name    ", &card.name, crate::app::CustomField::Name),
                    ("endpoint", &card.origin, crate::app::CustomField::Origin),
                    ("deploy  ", &card.model, crate::app::CustomField::Model),
                ],
                CustomCardKind::Bedrock => {
                    vec![("region  ", &card.origin, crate::app::CustomField::Origin)]
                }
                CustomCardKind::Vertex => vec![
                    ("project ", &card.origin, crate::app::CustomField::Origin),
                    ("location", &card.extra, crate::app::CustomField::Extra),
                ],
            };
            for (label, value, field) in fields {
                // In edit mode the NAME line is the locked stable identity —
                // dim it so the fixed id reads apart from the editable
                // origin/model lines.
                let identity_locked = card.edit && field == crate::app::CustomField::Name;
                let field_style = if identity_locked {
                    theme.dim_style()
                } else {
                    theme.text_style()
                };
                let prefix = format!("  {label} ❯ ");
                if card.can_edit_field(field) {
                    rects_out.push((
                        lines_out.len(),
                        u16::try_from(prefix.chars().count()).unwrap_or(u16::MAX),
                        u16::MAX,
                        Hit::CustomProviderField {
                            attempt: card.attempt,
                            field,
                        },
                    ));
                }
                let rendered_value = if editing && card.focus == field && card.can_edit_field(field)
                {
                    let byte = value
                        .char_indices()
                        .nth(card.cursor)
                        .map_or(value.len(), |(byte, _)| byte);
                    format!("{}▏{}", &value[..byte], &value[byte..])
                } else {
                    value.clone()
                };
                lines_out.push(Line::styled(
                    format!("{prefix}{rendered_value}"),
                    field_style,
                ));
            }
            if card.kind == CustomCardKind::Generic && card.discover_models {
                let auth = if card.keyless { "no auth" } else { "API key" };
                let family = if matches!(
                    card.family,
                    haider_rpc::ProviderApiFamilyWire::AnthropicMessages
                ) {
                    "anthropic"
                } else {
                    "openai"
                };
                for (label, value, field) in [
                    ("auth    ", auth, crate::app::CustomField::Auth),
                    ("API     ", family, crate::app::CustomField::ApiFamily),
                ] {
                    let prefix = format!("  {label} ❯ ");
                    if editing {
                        rects_out.push((
                            lines_out.len(),
                            u16::try_from(prefix.chars().count()).unwrap_or(u16::MAX),
                            u16::MAX,
                            Hit::CustomProviderField {
                                attempt: card.attempt,
                                field,
                            },
                        ));
                    }
                    let marker = if editing && card.focus == field {
                        "‹ "
                    } else {
                        ""
                    };
                    let end_marker = if editing && card.focus == field {
                        " ›"
                    } else {
                        ""
                    };
                    lines_out.push(Line::styled(
                        format!("{prefix}{marker}{value}{end_marker}"),
                        theme.text_style(),
                    ));
                }
                if !card.keyless {
                    const MASK_CAP: usize = 32;
                    let prefix = "  key      ❯ ";
                    if editing {
                        rects_out.push((
                            lines_out.len(),
                            u16::try_from(prefix.chars().count()).unwrap_or(u16::MAX),
                            u16::MAX,
                            Hit::CustomProviderField {
                                attempt: card.attempt,
                                field: crate::app::CustomField::Key,
                            },
                        ));
                    }
                    let shown = card.masked_key_len().min(MASK_CAP);
                    let more = if card.masked_key_len() > MASK_CAP {
                        "…"
                    } else {
                        ""
                    };
                    let caret = if editing && card.focus == crate::app::CustomField::Key {
                        "▏"
                    } else {
                        ""
                    };
                    lines_out.push(Line::styled(
                        format!("{prefix}{}{more}{caret}", "•".repeat(shown)),
                        theme.text_style(),
                    ));
                }
            }
            if editing {
                let hint = match card.kind {
                    CustomCardKind::Generic if card.discover_models => {
                        "  probes /v1/models now · key is masked and never printed"
                    }
                    CustomCardKind::Generic => {
                        if card.edit {
                            "  repoint the endpoint or change the model · name is the fixed id"
                        } else {
                            "  the model the server serves (e.g. llama3.1:8b) · the key is asked next"
                        }
                    }
                    CustomCardKind::Azure => {
                        "  https://{resource}.openai.azure.com + your DEPLOYMENT name · api-key asked next"
                    }
                    CustomCardKind::Bedrock => {
                        "  models are seeded (anthropic.claude-…) · the bearer API key is asked next"
                    }
                    CustomCardKind::Vertex => {
                        "  access tokens expire ~1h — paste one next, or import gcloud (auto-refresh)"
                    }
                };
                lines_out.push(Line::styled(hint, theme.dim_style()));
                lines_out.push(Line::styled(
                    if card.discover_models {
                        "  ⏎ add and discover · tab field · ←/→ changes choices · esc cancel"
                    } else if card.edit {
                        "  ⏎ save · tab origin/model · esc cancel"
                    } else {
                        "  ⏎ create · tab field · esc cancel"
                    },
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

/// The account add-button rows (OAuth/API/Kimi/Grok/Gemini/HF/custom) with
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
    let rows: [&[(&str, crate::app::AccountAddKind)]; 8] = [
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
            ("+ Grok (OAuth)", crate::app::AccountAddKind::GrokOAuth),
            ("+ Gemini (API)", crate::app::AccountAddKind::GeminiApi),
        ],
        // 970: agent-owned OAuth on its own row — the label is long enough
        // that pairing it would overflow the 80-column split.
        &[(
            "+ Google Antigravity (OAuth)",
            crate::app::AccountAddKind::GoogleAntigravity,
        )],
        &[
            (
                "+ Haider Code (API)",
                crate::app::AccountAddKind::HaiderCodeApi,
            ),
            ("+ DeepSeek (API)", crate::app::AccountAddKind::DeepSeekApi),
            ("+ xAI (API)", crate::app::AccountAddKind::XaiApi),
        ],
        &[
            ("+ HuggingFace", crate::app::AccountAddKind::HuggingFace),
            ("+ OpenCode Zen", crate::app::AccountAddKind::OpencodeZen),
            ("+ OpenCode Go", crate::app::AccountAddKind::OpencodeGo),
        ],
        // G4a: the local OSS presets — keyless customs at the servers'
        // default loopback origins. OpenAI stays first, Custom stays last
        // (the B6b edge rule).
        &[
            ("+ Ollama (local)", crate::app::AccountAddKind::Ollama),
            ("+ LM Studio (local)", crate::app::AccountAddKind::LmStudio),
        ],
        // G4b: the enterprise surfaces — Custom still stays last.
        &[
            ("+ Azure OpenAI", crate::app::AccountAddKind::AzureOpenAi),
            ("+ Bedrock (Claude)", crate::app::AccountAddKind::Bedrock),
            ("+ Vertex (Claude)", crate::app::AccountAddKind::Vertex),
        ],
        &[("+ Add custom server", crate::app::AccountAddKind::Custom)],
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

/// The Google Antigravity FIRST-LOGIN disclosure (970 owner decision). It
/// states WHO performs the sign-in (Google's own agent, not Haider), WHAT that
/// agent is (proprietary Google software under Google's terms), what it COSTS
/// (the first-hand figures pinned in `crate::app`), and the terms warning
/// verbatim — and only then offers a key. Nothing has been downloaded and no
/// flow has started while this is on screen: `[1]` IS the install consent.
///
/// No OAuth URL, query, code or token can appear here — the card renders
/// before any flow exists, and carries only constants.
fn antigravity_consent_lines(theme: &Theme, width: u16) -> Vec<Line<'static>> {
    // Four cells of indent plus slack: a produced row must never reach the
    // pane edge, or ratatui re-wraps it and the verbatim text breaks apart.
    let budget = usize::from(width).saturating_sub(6).max(1);
    let mut lines = vec![Line::from(vec![
        Span::styled("◉ ", theme.gold_style()),
        Span::styled(
            "Google Antigravity — the sign-in is Google's, not Haider's",
            theme.warn_style(),
        ),
    ])];
    for text in [
        "Google's own official antigravity-acp agent performs the OAuth and keeps the token — no Google credential ever enters Haider's vault.",
        "That agent is proprietary Google software, run under Google's terms (antigravity.google/terms).",
    ] {
        for row in wrap_body(text, budget) {
            lines.push(Line::styled(format!("  {row}"), theme.dim_style()));
        }
    }
    for cost in crate::app::GOOGLE_ANTIGRAVITY_COST_LINES {
        lines.push(Line::styled(format!("  · {cost}"), theme.faint_style()));
    }
    lines.push(Line::styled("  ⚠ terms warning", theme.warn_style()));
    for row in wrap_body(crate::app::GOOGLE_ANTIGRAVITY_TERMS_WARNING, budget) {
        lines.push(Line::styled(format!("    {row}"), theme.warn_style()));
    }
    lines.push(Line::from(vec![Span::styled(
        "  [1] install Google's agent and sign in · [2] cancel",
        theme.gold_style(),
    )]));
    lines
}

/// Display name for one credential-source KIND. Every kind reads as its
/// durable wire name with the underscores opened out — except the ones whose
/// product name is not derivable that way. Google's agent is
/// `google_antigravity` on the wire and `google-antigravity (ACP)` on screen,
/// because the badge has to say which protocol the account is reached over.
fn account_source_kind_label(kind: &str) -> String {
    match kind {
        crate::app::GOOGLE_ANTIGRAVITY_SOURCE_KIND => "google-antigravity (ACP)".to_owned(),
        other => other.replace('_', " "),
    }
}

fn account_source_health(source: &crate::app::AccountSourceRow) -> String {
    match source.health.as_str() {
        "ready" => "ready".to_owned(),
        "source_gone" => "unlinked (source gone)".to_owned(),
        "requires_origin_client" => format!(
            "not readable without {}",
            match source.refresh_owner.as_str() {
                "codex" => "Codex",
                "claude_code" => "Claude Code",
                "grok_cli" => "the Grok CLI",
                "kimi_cli" => "kimi-cli",
                _ => "origin client",
            }
        ),
        "unreadable" => "unreadable".to_owned(),
        "invalid" => "invalid source".to_owned(),
        "expired" => "expired · relogin required".to_owned(),
        "revoked" => "revoked · relogin required".to_owned(),
        other => other.replace('_', " "),
    }
}

fn account_source_style(
    theme: &Theme,
    source: &crate::app::AccountSourceRow,
) -> ratatui::style::Style {
    match source.health.as_str() {
        "ready" => theme.ok_style(),
        "source_gone" | "expired" | "revoked" | "invalid" => theme.err_style(),
        "requires_origin_client" | "unreadable" => theme.warn_style(),
        _ => theme.dim_style(),
    }
}

fn push_account_source_lines<'a>(
    source: &'a crate::app::AccountSourceRow,
    linked: bool,
    theme: &Theme,
    lines: &mut Vec<Line<'a>>,
) {
    let kind = account_source_kind_label(&source.kind);
    let store = source.credential_store.replace('_', " ");
    let owner = source.refresh_owner.replace('_', " ");
    lines.push(Line::from(vec![
        Span::styled(format!("    [{kind}] "), theme.gold_style()),
        Span::styled(source.label.clone(), theme.bright_style()),
        Span::styled(
            format!(" · {store} · refresh: {owner} · "),
            theme.dim_style(),
        ),
        Span::styled(
            account_source_health(source),
            account_source_style(theme, source),
        ),
    ]));
    let identity = source
        .masked_identity
        .as_deref()
        .unwrap_or("identity unknown");
    let plan = source.plan.as_deref().unwrap_or("plan unknown");
    let refreshed = source.last_refreshed_at_ms.map_or_else(
        || "refreshed unknown".to_owned(),
        |timestamp| format!("refreshed {}", calendar_instant(timestamp).1),
    );
    let expires = source.access_expires_at_ms.map_or_else(
        || "expires unknown".to_owned(),
        |timestamp| format!("expires {}", calendar_instant(timestamp).1),
    );
    let seen = source.last_seen_at_ms.map_or_else(
        || "seen unknown".to_owned(),
        |timestamp| format!("seen {}", calendar_instant(timestamp).1),
    );
    lines.push(Line::styled(
        format!(
            "      {identity} · {plan} · {refreshed} · {expires} · {seen}{}",
            if linked { "" } else { " · account not linked" }
        ),
        theme.dim_style(),
    ));
}

/// The source badge for one agent-owned Google account (970). Google's agent
/// holds the credential in its OWN profile, so the daemon enrols no
/// credential source for it and there is nothing to join by alias — the badge
/// is derived from the account row instead, and rides the SAME
/// [`push_account_source_lines`] renderer as every enrolled source. Nothing
/// here is invented: the health is the daemon's own `CredentialStatus`, and
/// every timestamp Haider cannot know (it never sees the token) stays `None`
/// so the row says `unknown` rather than a fabricated instant.
fn derived_antigravity_source(row: &crate::app::AccountRow) -> crate::app::AccountSourceRow {
    crate::app::AccountSourceRow {
        source_id: format!("antigravity:{}", row.alias),
        account_alias: Some(haider_protocol::ids::CredentialAlias::new(
            row.alias.clone(),
        )),
        kind: crate::app::GOOGLE_ANTIGRAVITY_SOURCE_KIND.to_owned(),
        label: "Google's antigravity-acp agent".to_owned(),
        path: None,
        credential_store: "google agent profile".to_owned(),
        refresh_owner: "antigravity_acp".to_owned(),
        health: match row.status {
            haider_protocol::credential::CredentialStatus::Ok => "ready",
            haider_protocol::credential::CredentialStatus::Limited { .. } => "rate limited",
            haider_protocol::credential::CredentialStatus::Expired => "expired",
            haider_protocol::credential::CredentialStatus::Revoked => "revoked",
            haider_protocol::credential::CredentialStatus::NeedsAttention { .. } => {
                "needs attention"
            }
        }
        .to_owned(),
        last_seen_at_ms: None,
        last_refreshed_at_ms: None,
        access_expires_at_ms: None,
        plan: None,
        // P1 MASK LAW — one authority, exactly like every other identity the
        // screen renders.
        masked_identity: Some(crate::format::mask_identity(&row.identity)),
    }
}

fn render_accounts(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    // Declared BEFORE `lines` so the derived rows outlive the frame that
    // borrows them. A Google account that the daemon DID enrol a source for
    // keeps that source and gets no second badge.
    let derived_sources: Vec<crate::app::AccountSourceRow> = model
        .accounts
        .rows
        .iter()
        .filter(|row| row.provider == crate::app::GOOGLE_ANTIGRAVITY_PROVIDER)
        .filter(|row| {
            !model.accounts.sources.iter().any(|source| {
                source
                    .account_alias
                    .as_ref()
                    .is_some_and(|alias| alias.as_str() == row.alias)
            })
        })
        .map(derived_antigravity_source)
        .collect();
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
    // 970 owner decision: `google-antigravity` ships ENABLED BY DEFAULT with
    // no policy gate and carries this STANDING disclosure instead — one
    // warning before the first login, then this line for as long as a Google
    // account exists. The text is pinned verbatim in `crate::app`; this only
    // wraps it.
    if model
        .accounts
        .rows
        .iter()
        .any(|row| row.provider == crate::app::GOOGLE_ANTIGRAVITY_PROVIDER)
    {
        lines.push(Line::from(vec![
            Span::styled("  ⚠ ", theme.warn_style()),
            Span::styled(
                "google-antigravity (ACP)",
                theme
                    .bright_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(" — terms warning", theme.warn_style()),
        ]));
        for row in wrap_body(
            crate::app::GOOGLE_ANTIGRAVITY_TERMS_WARNING,
            usize::from(area.width).saturating_sub(6).max(1),
        ) {
            lines.push(Line::styled(format!("    {row}"), theme.warn_style()));
        }
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
                haider_protocol::credential::CredentialStatus::NeedsAttention { reason } => {
                    use haider_protocol::credential::CredentialAttentionReason;
                    match reason {
                        CredentialAttentionReason::KeychainDenied => {
                            "needs attention — keychain access denied · re-link or re-allow"
                        }
                        CredentialAttentionReason::KeychainLocked => {
                            "needs attention — keychain locked · unlock login keychain (password may have changed)"
                        }
                        CredentialAttentionReason::KeychainMissing => {
                            "needs attention — Claude Code credential missing · re-link"
                        }
                        CredentialAttentionReason::KeychainUnavailable => {
                            "needs attention — keychain unavailable · retry refresh or re-link"
                        }
                        CredentialAttentionReason::SourceGone => {
                            "needs attention — source gone · re-link or remove"
                        }
                        CredentialAttentionReason::SourceUnreadable => {
                            "needs attention — source unreadable · check permissions or re-link"
                        }
                        CredentialAttentionReason::OriginClientRequired => {
                            "needs attention — origin client required · refresh in the origin client"
                        }
                        CredentialAttentionReason::PolicyBlocked => {
                            "needs attention — policy blocked · use a supported credential"
                        }
                    }
                    .to_owned()
                }
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
                    // P1 MASK LAW (the U2 owner addendum extended): the
                    // identity renders MASKED unless this visit revealed
                    // it (`r`) — one authority, `format::mask_identity`.
                    format!(
                        " · {} · {status_text}",
                        if model.accounts.revealed {
                            row.account_identity
                                .as_ref()
                                .map_or_else(|| row.identity.clone(), |identity| identity.summary())
                        } else {
                            crate::format::mask_identity(
                                &row.account_identity.as_ref().map_or_else(
                                    || row.identity.clone(),
                                    |identity| identity.summary(),
                                ),
                            )
                        },
                    ),
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
            for source in model
                .accounts
                .sources
                .iter()
                .chain(derived_sources.iter())
                .filter(|source| {
                    source
                        .account_alias
                        .as_ref()
                        .is_some_and(|alias| alias.as_str() == row.alias)
                })
            {
                push_account_source_lines(source, true, theme, &mut lines);
            }
        }
    }

    let unlinked = model
        .accounts
        .sources
        .iter()
        .filter(|source| {
            source.account_alias.as_ref().is_none_or(|alias| {
                !model
                    .accounts
                    .rows
                    .iter()
                    .any(|row| row.alias == alias.as_str())
            })
        })
        .collect::<Vec<_>>();
    if !unlinked.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "ENROLLED SOURCES — without a linked account",
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));
        for source in unlinked {
            push_account_source_lines(source, false, theme, &mut lines);
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
    // 970 — the Google Antigravity disclosure sits where every other accounts
    // card does, and owns the keyboard the same way (`[1]` / `[2]`).
    if model.antigravity_consent.is_some() {
        footer_lines.extend(antigravity_consent_lines(theme, area.width));
        footer_lines.push(Line::raw(""));
    }
    // The OAuth add card (W5e-1, sim authFlow MenuBox tui.js:3629-3682) —
    // rendered with the bottom chrome, above the add row.
    if let Some(card) = &model.oauth_add {
        // B6b: name the flow honestly — Kimi and Grok are device-code
        // grants, not loopback PKCE exchanges (the daemon owns both; the
        // card only reports).
        let agent_owned = card.provider == crate::app::GOOGLE_ANTIGRAVITY_PROVIDER;
        let flow = match card.provider.as_str() {
            "kimi-oauth" | "grok-oauth" => "OAuth (device code)",
            // 970: Haider drives no OAuth here at all — Google's agent owns
            // the whole exchange, so naming a Haider-driven issuer would be
            // a lie about who holds the token.
            _ if agent_owned => "OAuth (Google's own agent)",
            _ => "OAuth (loopback PKCE)",
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
                    if agent_owned {
                        // The install was consented on the disclosure card;
                        // this is the honest report of what that consent set
                        // in motion, never a silent background download.
                        "  installing Google's agent if it is not present, then starting its sign-in…"
                    } else {
                        "  starting the loopback flow…"
                    },
                    theme.dim_style(),
                ));
            }
            crate::app::OAuthAddPhase::WaitingBrowser { origin, .. } => {
                footer_lines.push(Line::styled(
                    if agent_owned {
                        // SECURITY: the agent's sign-in URL, its query and the
                        // authorization code never reach a rendered line — only
                        // the FACT that a browser was opened.
                        format!(
                            "  {} performs the sign-in — approve in the browser it opens; the agent keeps the token, Haider never sees it",
                            if origin.is_empty() {
                                "Google's own antigravity-acp agent"
                            } else {
                                origin
                            }
                        )
                    } else {
                        format!(
                            "  your browser opened {} — approve there; tokens land in the vault",
                            if origin.is_empty() {
                                "the provider"
                            } else {
                                origin
                            }
                        )
                    },
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
    // The `+ Add custom server` card (W5g-4; sim MenuBox
    // tui.js:3629-3682). Demo = the sim's verbatim fabrication card; live
    // = the editable name/origin fields (the provider.configure front
    // door).
    push_custom_card_lines(model, theme, &mut footer_lines, &mut add_button_rects);
    // 970: the first-login disclosure is the safeguard that REPLACES a policy
    // gate, so it has to stay readable at 80 columns. Its total modality
    // already makes the add row dead while it is open, so the row yields to
    // it rather than pushing the warning off a small terminal.
    if model.antigravity_consent.is_none() {
        push_account_add_buttons(model, theme, &mut footer_lines, &mut add_button_rects);
    }
    footer_lines.push(Line::raw(""));
    footer_lines.push(Line::styled(
        "click an account to make it active · + adds via OAuth / API · x removes · r reveals · esc back",
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
    // F2b: each provider block's header line, for cursor-follow scrolling.
    let mut header_lines: Vec<usize> = Vec::new();

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
    push_custom_card_lines(model, theme, &mut lines, &mut chip_hits);
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
        // G4a: a KEYLESS local provider (chat-completions custom, stored
        // origin, no auth methods) with nothing discovered is almost always
        // a server that is not running — say so, actionably.
        let keyless_local = matches!(
            summary.api_family,
            haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions
                | haider_rpc::ProviderApiFamilyWire::AnthropicMessages
        ) && summary.endpoint.is_some()
            && summary.auth_methods.is_empty();
        let (dot, dot_style, health) = match summary.availability {
            ProviderAvailabilityWire::Available => ("●", theme.ok_style(), "available".to_owned()),
            ProviderAvailabilityWire::Unavailable if keyless_local && summary.models.is_empty() => {
                (
                    "○",
                    theme.dim_style(),
                    "unavailable — start the server, then refresh (f)".to_owned(),
                )
            }
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
        if !matches!(summary.trust, haider_rpc::ProviderTrustWire::Full) {
            header
                .spans
                .push(Span::styled("  🔒 lockdown", theme.gold_style()));
        }
        if model.providers.cursor == index {
            header = hover_band(header, true, area.width, theme);
        }
        header_lines.push(lines.len());
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

    // F2b: the add-login actions are PINNED at the bottom (owner: "the
    // providers page I should be able to scroll it and bottom should have
    // the add login buttons") — a fixed footer under a scrolling roster.
    // Tiny frames keep the flowed layout instead: everything stays in the
    // scroll body, still reachable by scrolling to the end.
    // G4a: the preset roster outgrew one hint line at narrow widths — the
    // key map splits into the action line and the preset line so `esc back`
    // stays visible at 100-118 columns.
    let hint = "model click sets default · t trust · e edit · x remove · f refresh · esc back";
    let preset_hint = "presets: h HuggingFace · z Zen · g Go · o Ollama · l LM Studio";
    let enterprise_hint = "named: d DeepSeek · enterprise: a Azure · b Bedrock · v Vertex";
    let mut footer_lines: Vec<Line<'_>> = Vec::new();
    let mut footer_hits: Vec<(usize, u16, u16, Hit)> = Vec::new();
    let pinned = area.height >= 12;
    if pinned {
        push_account_add_buttons(model, theme, &mut footer_lines, &mut footer_hits);
        footer_lines.push(Line::styled(hint, theme.faint_style()));
        footer_lines.push(Line::styled(preset_hint, theme.faint_style()));
        footer_lines.push(Line::styled(enterprise_hint, theme.faint_style()));
    } else {
        push_account_add_buttons(model, theme, &mut lines, &mut chip_hits);
        lines.push(Line::styled(hint, theme.faint_style()));
        lines.push(Line::styled(preset_hint, theme.faint_style()));
        lines.push(Line::styled(enterprise_hint, theme.faint_style()));
    }
    let footer_height = u16::try_from(footer_lines.len()).unwrap_or(0);
    let [roster_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(area);

    // RENDER is the single scroll authority (the transcript's law): the
    // frame writes the true max, reconciles the offset, and resolves a
    // cursor-follow latch against ITS OWN line layout.
    let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_scroll = total.saturating_sub(roster_area.height);
    model.providers.scroll_max.set(max_scroll);
    let mut scroll = model.providers.scroll.get().min(max_scroll);
    if model.providers.follow_cursor.take() {
        let header = header_lines
            .get(model.providers.cursor)
            .and_then(|&line| u16::try_from(line).ok())
            .unwrap_or(0);
        if header < scroll {
            scroll = header;
        } else if header + 1 >= scroll + roster_area.height {
            scroll = (header + 2)
                .saturating_sub(roster_area.height)
                .min(max_scroll);
        }
    }
    model.providers.scroll.set(scroll);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), roster_area);
    // The house scroll indicator: `⋮` gutter marks on the edge rows while
    // content hides beyond them (menu_block's vocabulary).
    if scroll > 0 && roster_area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled("⋮", theme.faint_style())),
            Rect {
                x: roster_area.x,
                y: roster_area.y,
                width: 1.min(roster_area.width),
                height: 1,
            },
        );
    }
    if scroll < max_scroll && roster_area.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::styled("⋮", theme.faint_style())),
            Rect {
                x: roster_area.x,
                y: roster_area.y + roster_area.height - 1,
                width: 1.min(roster_area.width),
                height: 1,
            },
        );
    }
    if footer_height > 0 {
        frame.render_widget(Paragraph::new(footer_lines), footer_area);
    }

    for (line_index, x, width, hit) in chip_hits {
        let line = u16::try_from(line_index).unwrap_or(u16::MAX);
        if line < scroll || line - scroll >= roster_area.height || x >= roster_area.width {
            continue;
        }
        hits.push((
            Rect {
                x: roster_area.x + x,
                y: roster_area.y + (line - scroll),
                width: width.min(roster_area.width - x),
                height: 1,
            },
            hit,
        ));
    }
    for (line_index, x, width, hit) in footer_hits {
        let y = footer_area.y + line_index as u16;
        if y >= footer_area.y + footer_area.height || x >= footer_area.width {
            continue;
        }
        hits.push((
            Rect {
                x: footer_area.x + x,
                y,
                width: width.min(footer_area.width - x),
                height: 1,
            },
            hit,
        ));
    }
}

/// One `/usage` meter line's bar ink from its utilization (U2): the
/// threshold law lives in [`crate::format::usage_tone`]; this only maps
/// tones onto theme slots.
/// Models scope: attributed daily lane folds grouped by `(model, provider)`
/// for the selected ledger range. Rows are ordered by descending token total;
/// missing attribution and missing prices remain explicit absence.
fn render_usage_models(model: &AppModel, theme: &Theme, lines: &mut Vec<Line<'_>>) {
    use crate::format::fmt_tok;

    if model.mode.fabricates_locally() && model.usage.history.is_none() {
        lines.push(Line::styled(
            "  demo — history is ledger truth, never fabricated; run bare `haider` against a daemon",
            theme.dim_style(),
        ));
        return;
    }
    if model.usage.history_fetching {
        lines.push(Line::styled("  fetching…", theme.gold_style()));
    }
    if let Some(error) = &model.usage.history_error {
        lines.push(Line::from(vec![
            Span::styled("  ✗ model history read failed — ", theme.err_style()),
            Span::styled(error.clone(), theme.err_style()),
        ]));
        if model.usage.history.is_some() {
            lines.push(Line::styled(
                "  showing the previously committed range (older truth, never fabricated)",
                theme.dim_style(),
            ));
        }
    }
    let Some(days) = &model.usage.history else {
        if !model.usage.history_fetching && model.usage.history_error.is_none() {
            lines.push(Line::styled(
                "  no model history yet — f fetches one",
                theme.dim_style(),
            ));
        }
        return;
    };

    lines.push(Line::from(vec![
        Span::styled(
            "  MODELS",
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} — this device's ledger · r range",
                model.usage.model_range.label()
            ),
            theme.dim_style(),
        ),
    ]));
    lines.push(Line::raw(""));

    #[derive(Default)]
    struct ModelFold {
        model: String,
        provider: String,
        requests: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        reasoning_tokens: u64,
        est_cost_microusd: Option<u64>,
        price_missing: bool,
    }
    let selected_start = model
        .usage
        .model_range
        .days()
        .map_or(0, |count| days.len().saturating_sub(count));
    let selected = &days[selected_start..];
    let provider_matches = |provider: &str| {
        model.usage.filter.as_deref().is_none_or(|filter| {
            provider
                .to_ascii_lowercase()
                .starts_with(&filter.to_ascii_lowercase())
        })
    };
    let mut folds = std::collections::BTreeMap::<(String, String), ModelFold>::new();
    for row in selected
        .iter()
        .flat_map(|day| day.models.iter())
        .filter(|row| provider_matches(&row.provider))
    {
        let fold = folds
            .entry((row.model.clone(), row.provider.clone()))
            .or_insert_with(|| ModelFold {
                model: row.model.clone(),
                provider: row.provider.clone(),
                ..ModelFold::default()
            });
        fold.requests = fold.requests.saturating_add(row.requests);
        fold.input_tokens = fold.input_tokens.saturating_add(row.input_tokens);
        fold.output_tokens = fold.output_tokens.saturating_add(row.output_tokens);
        fold.cache_read_tokens = fold.cache_read_tokens.saturating_add(row.cache_read_tokens);
        fold.reasoning_tokens = fold.reasoning_tokens.saturating_add(row.reasoning_tokens);
        match row.est_cost_microusd {
            Some(cost) if !fold.price_missing => {
                fold.est_cost_microusd =
                    Some(fold.est_cost_microusd.unwrap_or(0).saturating_add(cost));
            }
            Some(_) => {}
            None => {
                fold.est_cost_microusd = None;
                fold.price_missing = true;
            }
        }
    }
    let first_sampled = days
        .iter()
        .find(|day| day.total.is_some())
        .map(|day| day.date.as_str());
    let first_attributed = days
        .iter()
        .find(|day| day.models.iter().any(|row| provider_matches(&row.provider)))
        .map(|day| day.date.as_str());
    if let (Some(sampled), Some(attributed)) = (first_sampled, first_attributed)
        && sampled < attributed
    {
        lines.push(Line::styled(
            format!(
                "  earlier ledger totals predate model attribution · first attributed date {attributed}"
            ),
            theme.warn_style(),
        ));
    }
    if folds.is_empty() {
        lines.push(Line::styled(
            first_attributed.map_or_else(
                || {
                    "  no attributed rows in this range · first attributed date unavailable"
                        .to_owned()
                },
                |date| format!("  no attributed rows in this range · first attributed date {date}"),
            ),
            theme.dim_style(),
        ));
        return;
    }
    let mut ordered = folds.into_values().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        let tokens = |fold: &ModelFold| {
            fold.input_tokens
                .saturating_add(fold.output_tokens)
                .saturating_add(fold.reasoning_tokens)
        };
        tokens(right)
            .cmp(&tokens(left))
            .then_with(|| left.model.cmp(&right.model))
            .then_with(|| left.provider.cmp(&right.provider))
    });
    lines.push(Line::styled(
        format!(
            "  {:<24} {:<18} {:>8} {:>8} {:>8} {:>8} {:>10}",
            "model", "provider", "in", "out", "cached", "requests", "est. cost"
        ),
        theme.dim_style(),
    ));
    for fold in &ordered {
        let cost = fold.est_cost_microusd.map_or_else(
            || "—".to_owned(),
            |microusd| {
                let cents = microusd.saturating_add(5_000) / 10_000;
                format!("${}.{:02}", cents / 100, cents % 100)
            },
        );
        lines.push(Line::styled(
            format!(
                "  {:<24} {:<18} {:>8} {:>8} {:>8} {:>8} {:>10}",
                ellipsize(&fold.model, 24),
                ellipsize(&fold.provider, 18),
                fmt_tok(fold.input_tokens),
                fmt_tok(fold.output_tokens),
                fmt_tok(fold.cache_read_tokens),
                fold.requests,
                cost,
            ),
            theme.text_style(),
        ));
    }
}

/// The 954 History scope: a codex-style token-activity heatmap from the
/// device-local ledger window (`usage.history_range`).
///
/// Cell honesty carries the ledger's absence law into pixels: an ABSENT
/// day (`total: None`, no local sample) renders a faint `·`; a PRESENT
/// all-zero day renders a faint `▫` (a measured zero is a fact); active
/// days render `■` on a four-step ramp blended from the theme's accent
/// over its ground (`gold.over(bg, …)`) so every theme derives its own
/// ramp — never a hardcoded palette. Quartile thresholds come from the
/// window's own nonzero days.
fn render_usage_history(model: &AppModel, theme: &Theme, lines: &mut Vec<Line<'_>>) {
    use crate::format::{days_from_iso_date, fmt_tok, weekday_from_days};

    // Demo refuses FETCHES (the reducer never pushes the read), but a
    // held window renders wherever it came from — hiding applied state
    // would fabricate emptiness, the inverse sin.
    if model.mode.fabricates_locally() && model.usage.history.is_none() {
        lines.push(Line::styled(
            "  demo — history is ledger truth, never fabricated; run bare `haider` against a daemon",
            theme.dim_style(),
        ));
        return;
    }
    if model.usage.history_fetching {
        lines.push(Line::styled("  fetching…", theme.gold_style()));
    }
    if let Some(error) = &model.usage.history_error {
        lines.push(Line::from(vec![
            Span::styled("  ✗ history read failed — ", theme.err_style()),
            Span::styled(error.clone(), theme.err_style()),
        ]));
        if model.usage.history.is_some() {
            lines.push(Line::styled(
                "  showing the previously committed window (older truth, never fabricated)",
                theme.dim_style(),
            ));
        }
    }
    let Some(days) = &model.usage.history else {
        if !model.usage.history_fetching && model.usage.history_error.is_none() {
            lines.push(Line::styled(
                "  no history window read yet — the daemon may predate usage_history_v1",
                theme.dim_style(),
            ));
        }
        return;
    };

    // Dated cells keyed by days-since-epoch; ISO dates sort lexically but
    // the grid needs weekday math, so parse once. Malformed dates are
    // dropped (never a panic in a render).
    let mut cells: Vec<(i64, Option<u64>)> = days
        .iter()
        .filter_map(|day| {
            let z = days_from_iso_date(&day.date)?;
            let tokens = day
                .total
                .as_ref()
                .map(|t| t.input_tokens + t.output_tokens + t.reasoning_tokens);
            Some((z, tokens))
        })
        .collect();
    cells.sort_by_key(|(z, _)| *z);
    let Some(&(last_day, _)) = cells.last() else {
        lines.push(Line::styled(
            "  the window contains no days — an empty range, not an error",
            theme.dim_style(),
        ));
        return;
    };

    // Header stats over PRESENT days only (absent days assert nothing).
    let lifetime: u64 = cells.iter().filter_map(|(_, t)| *t).sum();
    let peak: u64 = cells.iter().filter_map(|(_, t)| *t).max().unwrap_or(0);
    // Best streak: the longest run of CONSECUTIVE calendar days with
    // activity (a gap in the ledger breaks a run — absent days assert
    // nothing, and a streak must not bridge them).
    let (mut best, mut run, mut prev) = (0u32, 0u32, None::<i64>);
    for (z, tokens) in &cells {
        let active = tokens.is_some_and(|t| t > 0);
        run = if active && prev == Some(*z - 1) {
            run + 1
        } else {
            u32::from(active)
        };
        best = best.max(run);
        prev = active.then_some(*z);
    }
    // Current streak: consecutive active days counting back from the
    // window's last day.
    let mut streak = 0u32;
    for (z, tokens) in cells.iter().rev() {
        if tokens.is_some_and(|t| t > 0) && *z == last_day - i64::from(streak) {
            streak += 1;
        } else {
            break;
        }
    }
    lines.push(Line::from(vec![
        Span::styled(
            "  Token activity",
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  lifetime {} · peak {} · streak {}d (best {}d)",
                fmt_tok(lifetime),
                fmt_tok(peak),
                streak,
                best
            ),
            theme.dim_style(),
        ),
    ]));
    lines.push(Line::raw(""));

    // Quartile thresholds over nonzero days.
    let mut nonzero: Vec<u64> = cells
        .iter()
        .filter_map(|(_, t)| *t)
        .filter(|t| *t > 0)
        .collect();
    nonzero.sort_unstable();
    let q = |f: usize| {
        nonzero
            .get((nonzero.len().saturating_sub(1)) * f / 4)
            .copied()
            .unwrap_or(0)
    };
    let (q1, q2, q3) = (q(1), q(2), q(3));
    let ramp = [
        theme.gold.over(theme.bg, 280),
        theme.gold.over(theme.bg, 520),
        theme.gold.over(theme.bg, 760),
        theme.gold,
    ];
    let by_day: std::collections::BTreeMap<i64, Option<u64>> = cells.iter().copied().collect();

    // Grid: weeks as columns, Su..Sa rows, ending at the window's last day.
    let first_day = cells.first().map(|(z, _)| *z).unwrap_or(last_day);
    let last_col_start = last_day - i64::from(weekday_from_days(last_day));
    let weeks: i64 =
        (last_col_start - (first_day - i64::from(weekday_from_days(first_day)))) / 7 + 1;
    let day_names = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
    for row in 0..7u32 {
        let mut spans = vec![Span::styled(
            format!("  {} ", day_names[row as usize]),
            theme.dim_style(),
        )];
        for week in 0..weeks {
            let z = last_col_start - (weeks - 1 - week) * 7 + i64::from(row);
            let (glyph, color) = match by_day.get(&z) {
                None => ("· ", theme.faint),
                Some(None) => ("· ", theme.faint),
                Some(Some(0)) => ("▫ ", theme.faint),
                Some(Some(t)) => {
                    let idx = if *t <= q1 {
                        0
                    } else if *t <= q2 {
                        1
                    } else if *t <= q3 {
                        2
                    } else {
                        3
                    };
                    ("■ ", ramp[idx])
                }
            };
            spans.push(Span::styled(
                glyph,
                ratatui::style::Style::default().fg(color.into()),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    let mut legend = vec![Span::styled("     · none  ▫ zero  ", theme.dim_style())];
    for color in ramp {
        legend.push(Span::styled(
            "■ ",
            ratatui::style::Style::default().fg(color.into()),
        ));
    }
    legend.push(Span::styled(
        " more — daily · weekly · cumulative — next",
        theme.dim_style(),
    ));
    lines.push(Line::from(legend));
}

fn usage_bar_style(theme: &Theme, utilization: f64) -> ratatui::style::Style {
    match crate::format::usage_tone(utilization) {
        crate::format::UsageTone::Ok => theme.ok_style(),
        crate::format::UsageTone::Warn => theme.warn_style(),
        crate::format::UsageTone::Err => theme.err_style(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CalendarResetKind {
    FiveHour,
    Weekly,
}

impl CalendarResetKind {
    const fn marker(self) -> char {
        match self {
            Self::FiveHour => '5',
            Self::Weekly => 'W',
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::Weekly => "weekly",
        }
    }
}

/// Pick only provider-published windows whose names have a defined meaning.
/// In particular, this never derives a reset by adding five hours or seven
/// days: a missing canonical window/reset stays `reset unknown`.
fn calendar_reset_window(
    account: &haider_protocol::usage::AccountUsageReportV1,
    kind: CalendarResetKind,
) -> Option<&haider_protocol::usage::UsageWindowV1> {
    let haider_protocol::usage::AccountMeterStateV1::Metered { windows } = &account.meter else {
        return None;
    };
    let provider = account.provider.to_ascii_lowercase();
    let names: &[&str] = match (provider.as_str(), kind) {
        (provider, CalendarResetKind::FiveHour) if provider.starts_with("anthropic") => {
            &["five_hour"]
        }
        (provider, CalendarResetKind::Weekly) if provider.starts_with("anthropic") => {
            &["seven_day"]
        }
        (provider, CalendarResetKind::FiveHour)
            if provider.starts_with("openai") || provider.contains("codex") =>
        {
            &["5h", "five_hour"]
        }
        (provider, CalendarResetKind::Weekly)
            if provider.starts_with("openai") || provider.contains("codex") =>
        {
            &["weekly", "seven_day"]
        }
        _ => return None,
    };
    // OpenAI can publish identically sized named per-model windows. Only an
    // unlabeled provider window is account-wide; even a lone labeled window
    // is not a safe account reset and must stay unknown.
    windows
        .iter()
        .find(|window| window.label.is_none() && names.contains(&window.window.as_str()))
}

fn calendar_account_marker(index: usize) -> String {
    u8::try_from(index)
        .ok()
        .filter(|index| *index < 26)
        .map_or_else(
            || (index + 1).to_string(),
            |index| char::from(b'a' + index).to_string(),
        )
}

fn calendar_month_name(month: u32) -> &'static str {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    month
        .checked_sub(1)
        .and_then(|month| usize::try_from(month).ok())
        .and_then(|month| MONTHS.get(month))
        .copied()
        .unwrap_or("Unknown")
}

fn calendar_days_in_month(year: i64, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = crate::format::days_from_civil(year, month, 1);
    let next = crate::format::days_from_civil(next_year, next_month, 1);
    u32::try_from(next.saturating_sub(first)).unwrap_or(31)
}

fn calendar_instant(timestamp_ms: u64) -> (i64, String) {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let seconds = timestamp_ms / 1_000;
    let day = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_in_day = seconds % 86_400;
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let (year, month, date) = crate::format::civil_from_days(day);
    let weekday = WEEKDAYS[crate::format::weekday_from_days(day) as usize];
    (
        day,
        format!(
            "{weekday} {date:02} {} {year:04} · {hour:02}:{minute:02} UTC",
            calendar_month_name(month)
        ),
    )
}

fn calendar_compact_instant(timestamp_ms: u64, calendar_year: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let seconds = timestamp_ms / 1_000;
    let day = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_in_day = seconds % 86_400;
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let (year, month, date) = crate::format::civil_from_days(day);
    let weekday = WEEKDAYS[crate::format::weekday_from_days(day) as usize];
    let year = if year == calendar_year {
        String::new()
    } else {
        format!(" '{:02}", year.rem_euclid(100))
    };
    format!(
        "{weekday} {date:02} {}{year} {hour:02}:{minute:02}",
        &calendar_month_name(month)[..3]
    )
}

/// Provider reset calendar. The report timestamp is the sole `today`
/// authority, so a frozen RPC fixture produces byte-for-byte stable dates in
/// every timezone. The grid includes the whole report month and at least the
/// next fourteen UTC days even when today is at month end.
fn render_usage_calendar(
    model: &AppModel,
    theme: &Theme,
    area_width: u16,
    lines: &mut Vec<Line<'_>>,
    header_lines: &mut Vec<usize>,
) {
    use haider_protocol::usage::AccountMeterStateV1;

    let Some(report) = &model.usage.report else {
        if !model.usage.fetching && model.usage.error.is_none() {
            lines.push(Line::styled(
                "  no usage snapshot yet — reset calendar unavailable",
                theme.dim_style(),
            ));
        }
        return;
    };

    let groups = model.usage.groups();
    let mut visible_slots = groups
        .iter()
        .flat_map(|group| group.accounts.iter().copied())
        .collect::<Vec<_>>();
    visible_slots.sort_unstable();
    visible_slots.dedup();
    if visible_slots.is_empty() {
        lines.push(Line::styled(
            if report.accounts.is_empty() {
                "  no accounts known — /login adds one".to_owned()
            } else {
                format!(
                    "  no accounts match \"{}\" — bare /usage clears the filter",
                    model.usage.filter.as_deref().unwrap_or_default()
                )
            },
            theme.dim_style(),
        ));
        return;
    }

    let generated_seconds = report.generated_at_ms / 1_000;
    let today = i64::try_from(generated_seconds / 86_400).unwrap_or(i64::MAX);
    let (year, month, today_date) = crate::format::civil_from_days(today);
    let month_start = crate::format::days_from_civil(year, month, 1);
    let grid_start = month_start - i64::from(crate::format::weekday_from_days(month_start));
    let month_end = month_start + i64::from(calendar_days_in_month(year, month)) - 1;
    let required_end = month_end.max(today.saturating_add(14));
    let grid_end = required_end
        + i64::from(6_u32.saturating_sub(crate::format::weekday_from_days(required_end)));

    let selected_slot = groups
        .get(model.usage.cursor.min(groups.len().saturating_sub(1)))
        .and_then(|group| group.accounts.get(model.usage.selected_tab(group)))
        .copied();
    let mut events = Vec::new();
    for &slot in &visible_slots {
        let Some(account) = report.accounts.get(slot) else {
            continue;
        };
        for kind in [CalendarResetKind::FiveHour, CalendarResetKind::Weekly] {
            let Some(reset) =
                calendar_reset_window(account, kind).and_then(|window| window.resets_at_ms)
            else {
                continue;
            };
            let (day, _) = calendar_instant(reset);
            events.push((day, slot, kind));
        }
    }

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} {year:04}", calendar_month_name(month)),
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            " · UTC reset calendar · provider timestamps only",
            theme.dim_style(),
        ),
    ]));
    lines.push(Line::styled(
        format!(
            "  [{today_date:02}] today · a5 five-hour · aW weekly · letters map to every account below"
        ),
        theme.dim_style(),
    ));
    lines.push(Line::raw(""));

    let cell_width = usize::from(area_width.saturating_sub(2) / 7).max(7);
    let mut weekday_line = vec![Span::raw("  ")];
    for name in ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"] {
        weekday_line.push(Span::styled(
            format!("{name:<cell_width$}"),
            theme.dim_style(),
        ));
    }
    lines.push(Line::from(weekday_line));

    let mut week_start = grid_start;
    while week_start <= grid_end {
        let mut week = vec![Span::raw("  ")];
        for offset in 0_i64..7 {
            let day = week_start + offset;
            let (_, cell_month, date) = crate::format::civil_from_days(day);
            let markers = events
                .iter()
                .filter(|(event_day, _, _)| *event_day == day)
                .filter_map(|(_, slot, kind)| {
                    let visible_index = visible_slots
                        .iter()
                        .position(|candidate| candidate == slot)?;
                    Some(format!(
                        "{}{}",
                        calendar_account_marker(visible_index),
                        kind.marker()
                    ))
                })
                .collect::<Vec<_>>()
                .join(",");
            let date = if day == today {
                format!("[{date:02}]")
            } else {
                format!(" {date:02} ")
            };
            let cell = ellipsize(
                &if markers.is_empty() {
                    date
                } else {
                    format!("{date} {markers}")
                },
                cell_width,
            );
            let style = if day == today {
                theme
                    .gold_style()
                    .bg(theme.gold_soft.into())
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else if events.iter().any(|(event_day, _, _)| *event_day == day) {
                theme.gold_style()
            } else if cell_month == month {
                theme.text_style()
            } else {
                theme.faint_style()
            };
            week.push(Span::styled(format!("{cell:<cell_width$}"), style));
        }
        lines.push(Line::from(week));
        week_start += 7;
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  RESET MARKERS",
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            " — exact meter fields · unknown is never inferred",
            theme.dim_style(),
        ),
    ]));
    let mut account_lines = vec![None; report.accounts.len()];
    if area_width < 100 {
        for (visible_index, &slot) in visible_slots.iter().enumerate() {
            let Some(account) = report.accounts.get(slot) else {
                continue;
            };
            account_lines[slot] = Some(lines.len());
            let marker = calendar_account_marker(visible_index);
            let selected = selected_slot == Some(slot);
            let reset = |kind: CalendarResetKind| {
                calendar_reset_window(account, kind)
                    .and_then(|window| window.resets_at_ms)
                    .map_or_else(
                        || "reset unknown".to_owned(),
                        |timestamp| calendar_compact_instant(timestamp, year),
                    )
            };
            let suffix = match &account.meter {
                AccountMeterStateV1::Unavailable { reason } => format!(
                    " · meter unavailable ({})",
                    crate::format::fmt_meter_reason(reason)
                ),
                AccountMeterStateV1::LocalOnly => " · local only".to_owned(),
                AccountMeterStateV1::Metered { .. } => String::new(),
            };
            lines.push(Line::styled(
                format!(
                    "  {} {marker} {} · 5h {} · W {} UTC{suffix}",
                    if selected { ">" } else { " " },
                    account.alias.as_str(),
                    reset(CalendarResetKind::FiveHour),
                    reset(CalendarResetKind::Weekly),
                ),
                if selected {
                    theme
                        .gold_style()
                        .add_modifier(ratatui::style::Modifier::BOLD)
                } else if matches!(account.meter, AccountMeterStateV1::Unavailable { .. }) {
                    theme.warn_style()
                } else {
                    theme.bright_style()
                },
            ));
        }
        for group in &groups {
            let selected = group
                .accounts
                .get(model.usage.selected_tab(group))
                .copied()
                .or_else(|| group.accounts.first().copied());
            header_lines.push(
                selected
                    .and_then(|slot| account_lines.get(slot).copied().flatten())
                    .unwrap_or(0),
            );
        }
        return;
    }
    for (visible_index, &slot) in visible_slots.iter().enumerate() {
        let Some(account) = report.accounts.get(slot) else {
            continue;
        };
        account_lines[slot] = Some(lines.len());
        let marker = calendar_account_marker(visible_index);
        let selected = selected_slot == Some(slot);
        let identity = account
            .identity
            .as_deref()
            .map(crate::format::mask_identity)
            .unwrap_or_else(|| "—".to_owned());
        let plan = account.plan.as_deref().unwrap_or("plan unknown");
        lines.push(Line::styled(
            format!(
                "  {} {marker}  {} · {} · {identity} · {plan}",
                if selected { ">" } else { " " },
                account.alias.as_str(),
                account.provider,
            ),
            if selected {
                theme
                    .gold_style()
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                theme.bright_style()
            },
        ));
        match &account.meter {
            AccountMeterStateV1::Unavailable { reason } => lines.push(Line::styled(
                format!(
                    "      5h reset unknown · weekly reset unknown · meter unavailable · {}",
                    crate::format::fmt_meter_reason(reason)
                ),
                theme.warn_style(),
            )),
            AccountMeterStateV1::LocalOnly => lines.push(Line::styled(
                "      5h reset unknown · weekly reset unknown · local only; no provider meter",
                theme.dim_style(),
            )),
            AccountMeterStateV1::Metered { .. } => {
                for kind in [CalendarResetKind::FiveHour, CalendarResetKind::Weekly] {
                    let row = calendar_reset_window(account, kind).map_or_else(
                        || {
                            format!(
                                "      {} reset unknown · exact window not published",
                                kind.label()
                            )
                        },
                        |window| {
                            window.resets_at_ms.map_or_else(
                                || {
                                    format!(
                                        "      {} reset unknown · {} published no reset",
                                        kind.label(),
                                        window.window
                                    )
                                },
                                |reset| {
                                    let (_, instant) = calendar_instant(reset);
                                    format!("      {} {instant} · {}", kind.label(), window.window)
                                },
                            )
                        },
                    );
                    lines.push(Line::styled(row, theme.dim_style()));
                }
            }
        }
    }
    for group in &groups {
        let selected = group
            .accounts
            .get(model.usage.selected_tab(group))
            .copied()
            .or_else(|| group.accounts.first().copied());
        header_lines.push(
            selected
                .and_then(|slot| account_lines.get(slot).copied().flatten())
                .unwrap_or(0),
        );
    }
}

/// U2 — the `/usage` screen: one block per provider group showing the
/// SELECTED account (←/→ tabs), its meter state rendered per the wire's
/// tag — `metered` limit bars with % + reset times, `unavailable` the
/// typed reason (NEVER a fabricated bar), `local_only` an honest
/// no-server-meter note — plus journal-derived local stats and a
/// device-total footer. F2b scroll discipline throughout; identities wear
/// the streamer mask unless this visit toggled `r`.
fn render_usage(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    use crate::cache_usage::CacheUsageStatsExt as _;
    use haider_protocol::usage::AccountMeterStateV1;

    let mut lines: Vec<Line<'_>> = Vec::new();
    // (line, column, width, hit) — chips resolved to rects after layout.
    let mut chip_hits: Vec<(usize, u16, u16, Hit)> = Vec::new();
    // F2b: each provider group's header line, for cursor-follow scrolling.
    let mut header_lines: Vec<usize> = Vec::new();

    let global = model.usage.scope == crate::app::UsageScope::Global;
    let history = model.usage.scope == crate::app::UsageScope::History;
    let models = model.usage.scope == crate::app::UsageScope::Models;
    let calendar = model.usage.scope == crate::app::UsageScope::Calendar;
    lines.push(Line::from(vec![
        Span::styled(
            "USAGE",
            theme
                .bright_style()
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            match model.usage.scope {
                crate::app::UsageScope::Accounts => " · accounts",
                crate::app::UsageScope::Global => " · global",
                crate::app::UsageScope::History => " · history",
                crate::app::UsageScope::Models => " · models",
                crate::app::UsageScope::Calendar => " · calendar",
            },
            theme.gold_style(),
        ),
        Span::styled(
            " — meters are provider truth · stats are this device's journal · s switches scope",
            theme.dim_style(),
        ),
    ]));
    let scope_line = lines.len();
    let mut scope_spans = vec![Span::raw("  ")];
    let mut scope_column = 2_u16;
    for (index, scope) in [
        crate::app::UsageScope::Accounts,
        crate::app::UsageScope::Calendar,
        crate::app::UsageScope::Global,
        crate::app::UsageScope::History,
        crate::app::UsageScope::Models,
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            scope_spans.push(Span::styled(" · ", theme.dim_style()));
            scope_column = scope_column.saturating_add(3);
        }
        let name = scope.name();
        let width = u16::try_from(name.chars().count()).unwrap_or(u16::MAX);
        chip_hits.push((scope_line, scope_column, width, Hit::UsageScope(scope)));
        scope_spans.push(Span::styled(
            name,
            if model.usage.scope == scope {
                theme
                    .gold_style()
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                theme.dim_style()
            },
        ));
        scope_column = scope_column.saturating_add(width);
    }
    scope_spans.extend([
        Span::styled("    s", theme.gold_style()),
        Span::styled(" next scope · ", theme.dim_style()),
        Span::styled("← →", theme.gold_style()),
        Span::styled(" account", theme.dim_style()),
    ]);
    lines.push(Line::from(scope_spans));
    if let Some(filter) = &model.usage.filter {
        lines.push(Line::styled(
            if history {
                format!(
                    "  filter: {filter}* — heatmap stays cross-provider · account/model rows filter"
                )
            } else if calendar {
                format!("  filter: {filter}* — reset rows filter · bare /usage clears")
            } else {
                format!("  filter: {filter}* — bare /usage clears")
            },
            theme.gold_style(),
        ));
    }
    if model.mode.fabricates_locally() {
        lines.push(Line::styled(
            "  demo — usage is live daemon truth, never fabricated; run bare `haider` against a daemon",
            theme.dim_style(),
        ));
    } else if model.usage.fetching {
        lines.push(Line::styled("  fetching…", theme.gold_style()));
    } else if let Some(error) = &model.usage.error {
        lines.push(Line::from(vec![
            Span::styled("  ✗ usage read failed — ", theme.err_style()),
            Span::styled(error.clone(), theme.err_style()),
        ]));
    }
    lines.push(Line::raw(""));

    // 954 History scope: the heatmap replaces every report-based section
    // below — it reads the ledger window, not the usage report.
    if history {
        render_usage_history(model, theme, &mut lines);
    }
    if models {
        render_usage_models(model, theme, &mut lines);
    }
    if calendar {
        render_usage_calendar(model, theme, area.width, &mut lines, &mut header_lines);
    }

    if !history && !models && !calendar && model.cache_usage.has_classified_usage() {
        let cache = model.cache_usage.totals();
        let all_input_share = cache
            .complete_hit_rate()
            .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.2}%", rate * 100.0));
        let coverage = cache
            .telemetry_coverage()
            .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.0}%", rate * 100.0));
        lines.push(Line::from(vec![
            Span::styled(
                "CURRENT SESSION",
                theme
                    .bright_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(
                " — latest cumulative snapshot per cache lane",
                theme.dim_style(),
            ),
        ]));
        if model.cache_usage.has_unclassified_usage() {
            lines.push(Line::styled(
                "    unclassified request usage present — excluded from totals and rates",
                theme.warn_style(),
            ));
        }
        lines.push(Line::styled(
            format!(
                "    input — logical {} · uncached {} · cache read {} · all-input share {all_input_share} · coverage {coverage}",
                fmt_tok(cache.logical_input_tokens),
                fmt_tok(cache.uncached_input_tokens),
                fmt_tok(cache.cache_read_tokens),
            ),
            theme.dim_style(),
        ));
        lines.push(Line::styled(
            format!(
                "    cache write — total {} · 5m {} · 1h {} · billed output {}",
                fmt_tok(cache.cache_write_tokens),
                fmt_tok(cache.cache_write_5m_tokens),
                fmt_tok(cache.cache_write_1h_tokens),
                fmt_tok(cache.billed_output_tokens),
            ),
            theme.dim_style(),
        ));
        match (
            cache.input_with_cache_usd,
            cache.input_without_cache_usd,
            cache.estimated_savings_usd,
        ) {
            (Some(with), Some(without), Some(savings)) => {
                let qualifier = if cache.metered_input_tokens < cache.logical_input_tokens {
                    " · metered lanes only"
                } else {
                    ""
                };
                let equivalent = if cache.breakdowns.iter().any(|breakdown| {
                    breakdown.auth_method == Some(haider_protocol::credential::AuthMethod::OAuth)
                }) {
                    match (
                        cache.api_equivalent_input_with_cache_usd,
                        cache.api_equivalent_input_without_cache_usd,
                        cache.api_equivalent_estimated_savings_usd,
                    ) {
                        (Some(api_with), Some(api_without), Some(api_savings)) => format!(
                            " · ≈${api_with:.4}/${api_without:.4} API rate (all lanes) · ≈${api_savings:.4} savings"
                        ),
                        _ => " · $— API rate (all lanes)".to_owned(),
                    }
                } else {
                    String::new()
                };
                lines.push(Line::styled(
                    format!(
                        "    input cost — ${with:.4} with caching · ${without:.4} without · ${savings:.4} estimated savings{qualifier}{equivalent}"
                    ),
                    theme.dim_style(),
                ));
            }
            _ if !cache.breakdowns.is_empty()
                && cache.breakdowns.iter().all(|breakdown| {
                    breakdown.auth_method == Some(haider_protocol::credential::AuthMethod::OAuth)
                }) =>
            {
                let equivalent = match (
                    cache.api_equivalent_input_with_cache_usd,
                    cache.api_equivalent_input_without_cache_usd,
                    cache.api_equivalent_estimated_savings_usd,
                ) {
                    (Some(with), Some(without), Some(savings)) => format!(
                        "    input cost — plan · ≈${with:.4}/${without:.4} API rate · ≈${savings:.4} savings"
                    ),
                    _ => "    input cost — plan · $— API rate".to_owned(),
                };
                lines.push(Line::styled(equivalent, theme.dim_style()));
            }
            _ => lines.push(Line::styled(
                "    input cost — $— · without caching $— · savings $—",
                theme.dim_style(),
            )),
        }
        for breakdown in &cache.breakdowns {
            let epoch = breakdown
                .cache_epoch
                .get(..8)
                .unwrap_or(&breakdown.cache_epoch);
            let lane = match breakdown.request_kind {
                haider_protocol::provider::UsageRequestKind::MainTurn => "main",
                haider_protocol::provider::UsageRequestKind::Compaction => "compaction",
                haider_protocol::provider::UsageRequestKind::DelegatedAgent => "delegated",
                _ => "unclassified",
            };
            let provider = if breakdown.provider.is_empty() {
                "unknown"
            } else {
                &breakdown.provider
            };
            let model_name = if breakdown.model.is_empty() {
                "unknown"
            } else {
                &breakdown.model
            };
            let epoch = if epoch.is_empty() { "unknown" } else { epoch };
            let part_hit = if breakdown.telemetry_covered_input_tokens
                == breakdown.logical_input_tokens
                && breakdown.logical_input_tokens > 0
            {
                let denominator = breakdown
                    .cache_read_tokens
                    .saturating_add(breakdown.uncached_input_tokens);
                if denominator == 0 {
                    "0.00%".to_owned()
                } else {
                    #[allow(clippy::cast_precision_loss)]
                    let rate = breakdown.cache_read_tokens as f64 / denominator as f64;
                    format!("{:.2}%", rate * 100.0)
                }
            } else {
                "n/a".to_owned()
            };
            let part_cost = match (
                breakdown.input_with_cache_usd,
                breakdown.input_without_cache_usd,
                breakdown.estimated_savings_usd,
            ) {
                (Some(with), Some(without), Some(savings)) => {
                    format!(" · input ${with:.4}/${without:.4} · save ${savings:.4}")
                }
                _ if breakdown.auth_method
                    == Some(haider_protocol::credential::AuthMethod::OAuth) =>
                {
                    match (
                        breakdown.api_equivalent_input_with_cache_usd,
                        breakdown.api_equivalent_input_without_cache_usd,
                        breakdown.api_equivalent_estimated_savings_usd,
                    ) {
                        (Some(with), Some(without), Some(savings)) => format!(
                            " · plan · ≈${with:.4}/${without:.4} API rate · save ≈${savings:.4}"
                        ),
                        _ => " · plan · input $— API rate".to_owned(),
                    }
                }
                _ => " · input $—".to_owned(),
            };
            lines.push(Line::styled(
                format!(
                    "      {provider} · {model_name} · epoch {epoch} · {lane} — uncached {} · write {} · read {} · all-input share {part_hit}{part_cost}",
                    fmt_tok(breakdown.uncached_input_tokens),
                    fmt_tok(breakdown.cache_write_tokens),
                    fmt_tok(breakdown.cache_read_tokens),
                ),
                if breakdown.request_kind
                    == haider_protocol::provider::UsageRequestKind::Compaction
                {
                    theme.gold_style()
                } else {
                    theme.dim_style()
                },
            ));
        }
        lines.push(Line::raw(""));
    } else if model.cache_usage.has_unclassified_usage() {
        lines.push(Line::styled(
            "CURRENT SESSION — unclassified request usage present; excluded from totals and rates",
            theme.warn_style(),
        ));
        lines.push(Line::raw(""));
    }

    // Direct/exclusive daemon snapshots. This block is independent of cache
    // telemetry and account-report availability: a tool-only agent is still
    // real work, while absent usage remains `n/a` rather than zero.
    let main_metrics = model.main_agent_metrics();
    let child_metrics = model
        .chips
        .iter()
        .map(|chip| (chip, model.chip_metrics(chip)))
        .collect::<Vec<_>>();
    if main_metrics.is_some() || child_metrics.iter().any(|(_, metrics)| metrics.is_some()) {
        lines.push(Line::from(vec![
            Span::styled(
                "AGENTS",
                theme
                    .bright_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(" — CURRENT SESSION", theme.dim_style()),
        ]));
        let all_snapshots = main_metrics.into_iter().chain(
            child_metrics
                .iter()
                .filter_map(|(_, metrics)| metrics.as_ref().copied()),
        );
        if main_metrics.is_some()
            && child_metrics.iter().all(|(_, metrics)| metrics.is_some())
            && let Some(total) = crate::agent_metrics::aggregate(all_snapshots)
        {
            let (tokens, cost) = total.usage.as_ref().map_or_else(
                || ("n/a tokens".to_owned(), "cost n/a".to_owned()),
                |usage| {
                    (
                        format!(
                            "{} tokens",
                            fmt_tok(crate::agent_metrics::normalized_tokens(usage))
                        ),
                        crate::agent_metrics::detailed_cost(usage),
                    )
                },
            );
            lines.push(Line::styled(
                format!(
                    "    session total — {} tools · {tokens} · {cost}",
                    total.tool_attempts
                ),
                theme.dim_style(),
            ));
        }
        let row = |label: &str, metrics: Option<&haider_protocol::agent::AgentMetricsSnapshot>| {
            metrics.map_or_else(
                || format!("    {label} — metrics n/a"),
                |metrics| {
                    let (tokens, cost) = metrics.usage.as_ref().map_or_else(
                        || ("n/a tokens".to_owned(), "cost n/a".to_owned()),
                        |usage| {
                            (
                                format!(
                                    "{} tokens",
                                    fmt_tok(crate::agent_metrics::normalized_tokens(usage))
                                ),
                                crate::agent_metrics::detailed_cost(usage),
                            )
                        },
                    );
                    format!(
                        "    {label} — {} tools · {tokens} · {cost}",
                        metrics.tool_attempts
                    )
                },
            )
        };
        lines.push(Line::styled(row("main", main_metrics), theme.dim_style()));
        for (chip, metrics) in child_metrics {
            let label = if chip.callsign.is_empty() {
                chip.agent.as_str()
            } else {
                chip.callsign.as_str()
            };
            lines.push(Line::styled(row(label, metrics), theme.dim_style()));
        }
        lines.push(Line::raw(""));
    }

    let groups = model.usage.groups();
    if !history
        && !models
        && !calendar
        && let Some(report) = &model.usage.report
    {
        if report.accounts.is_empty() {
            lines.push(Line::styled(
                "  no accounts known — /login adds one",
                theme.dim_style(),
            ));
        } else if groups.is_empty() {
            lines.push(Line::styled(
                format!(
                    "  no accounts match \"{}\" — bare /usage clears the filter",
                    model.usage.filter.as_deref().unwrap_or_default()
                ),
                theme.dim_style(),
            ));
        }
        // 954 global scope: one line per account — every account of every
        // group (not just selected tabs) — then the shared THIS DEVICE
        // totals footer below. The headline window is the one with the
        // LEAST runway (max utilization): the wall you will hit first.
        if global {
            for group in &groups {
                for &slot in &group.accounts {
                    let Some(account) = report.accounts.get(slot) else {
                        continue;
                    };
                    let alias = account.alias.as_str();
                    let mut spans = vec![
                        Span::styled(format!("  {alias:<18}"), theme.bright_style()),
                        Span::styled(
                            format!("{:<14}", ellipsize(&account.provider, 13)),
                            theme.dim_style(),
                        ),
                    ];
                    match &account.meter {
                        haider_protocol::usage::AccountMeterStateV1::Metered { windows } => {
                            if let Some(worst) = windows.iter().max_by(|a, b| {
                                a.utilization
                                    .partial_cmp(&b.utilization)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            }) {
                                spans.push(Span::styled(
                                    crate::format::remaining_bar(
                                        worst.utilization,
                                        crate::format::USAGE_BAR_CELLS,
                                    ),
                                    usage_bar_style(theme, worst.utilization),
                                ));
                                spans.push(Span::styled(
                                    format!(
                                        "  {:>9}",
                                        crate::format::fmt_remaining(worst.utilization)
                                    ),
                                    theme.bright_style(),
                                ));
                                spans.push(Span::styled(
                                    format!(" ({})", ellipsize(&worst.window, 14)),
                                    theme.dim_style(),
                                ));
                            } else {
                                // A successful reading with no published
                                // windows is NOT "no meter" — say what it is.
                                spans.push(Span::styled(
                                    "metered · no windows published",
                                    theme.dim_style(),
                                ));
                            }
                        }
                        haider_protocol::usage::AccountMeterStateV1::Unavailable { reason } => {
                            spans.push(Span::styled(
                                format!(
                                    "meter unavailable · {}",
                                    crate::format::fmt_meter_reason(reason)
                                ),
                                theme.warn_style(),
                            ));
                        }
                        haider_protocol::usage::AccountMeterStateV1::LocalOnly => {
                            spans.push(Span::styled("local only", theme.dim_style()));
                        }
                    }
                    let local = &account.local;
                    spans.push(Span::styled(
                        format!(
                            "  in {} · out {} · cached {}",
                            fmt_tok(local.input_tokens),
                            fmt_tok(local.output_tokens),
                            fmt_tok(local.cached_tokens),
                        ),
                        theme.dim_style(),
                    ));
                    lines.push(Line::from(spans));
                }
            }
            if !groups.is_empty() {
                lines.push(Line::raw(""));
            }
        }
        // Accounts scope renders the full per-provider detail; global
        // renders nothing here (the compact list above already did).
        let detail_groups: &[crate::app::UsageGroup] = if global { &[] } else { &groups };
        for (index, group) in detail_groups.iter().enumerate() {
            let selected_tab = model.usage.selected_tab(group);
            let Some(account) = group
                .accounts
                .get(selected_tab)
                .and_then(|&slot| report.accounts.get(slot))
            else {
                continue;
            };

            // Group header: provider · account tab chips ([alias] each, the
            // selected one gold). Chips are value-carrying hits.
            let mut spans = vec![Span::styled(
                group.provider.clone(),
                theme
                    .bright_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )];
            let mut offset = group.provider.chars().count() as u16;
            if group.accounts.len() > 1 {
                let counter = format!("  {}/{} ←→", selected_tab + 1, group.accounts.len());
                offset += counter.chars().count() as u16;
                spans.push(Span::styled(counter, theme.dim_style()));
            }
            spans.push(Span::raw("  "));
            offset += 2;
            for (position, &slot) in group.accounts.iter().enumerate() {
                let Some(entry) = report.accounts.get(slot) else {
                    continue;
                };
                let label = format!("[{}]", entry.alias.as_str());
                let width = label.chars().count() as u16;
                chip_hits.push((
                    lines.len(),
                    offset,
                    width,
                    Hit::UsageAccountTab {
                        provider: group.provider.clone(),
                        index: position,
                    },
                ));
                spans.push(Span::styled(
                    label,
                    if position == selected_tab {
                        theme.gold_style()
                    } else {
                        theme.dim_style()
                    },
                ));
                spans.push(Span::raw(" "));
                offset += width + 1;
            }
            let mut header = Line::from(spans);
            if model.usage.cursor == index {
                header = hover_band(header, true, area.width, theme);
            }
            header_lines.push(lines.len());
            lines.push(header);

            // Identity line: email/handle (MASKED unless revealed) · plan
            // · auth flavor. The mask is the default on every open.
            let identity = account.identity.as_deref().unwrap_or("—");
            let identity = if model.usage.revealed || identity == "—" {
                identity.to_owned()
            } else {
                crate::format::mask_identity(identity)
            };
            let mut parts = vec![identity];
            if let Some(plan) = &account.plan {
                parts.push(plan.clone());
            }
            parts.push(crate::app::auth_label(account.auth_method).to_owned());
            lines.push(Line::styled(
                format!("    {}", parts.join(" · ")),
                theme.dim_style(),
            ));

            // Meter state — rendered per the wire's tag, never re-judged.
            match &account.meter {
                AccountMeterStateV1::Metered { windows } if windows.is_empty() => {
                    lines.push(Line::styled(
                        "    metered — no windows reported",
                        theme.dim_style(),
                    ));
                }
                AccountMeterStateV1::Metered { windows } => {
                    let name_of = |window: &haider_protocol::usage::UsageWindowV1| {
                        window.label.as_ref().map_or_else(
                            || window.window.clone(),
                            |label| format!("{label} · {}", window.window),
                        )
                    };
                    for window in windows {
                        let mut spans = vec![
                            Span::styled("    ", theme.text_style()),
                            Span::styled(
                                crate::format::remaining_bar(
                                    window.utilization,
                                    crate::format::USAGE_BAR_CELLS,
                                ),
                                // Tone still keys on CONSUMPTION: a nearly
                                // spent window warns even though its bar is
                                // nearly empty in remaining semantics.
                                usage_bar_style(theme, window.utilization),
                            ),
                            Span::styled(
                                format!(
                                    "  {:>9}",
                                    crate::format::fmt_remaining(window.utilization)
                                ),
                                theme.bright_style(),
                            ),
                            Span::styled(
                                format!(" ({})", ellipsize(&name_of(window), 24)),
                                theme.dim_style(),
                            ),
                        ];
                        if let Some(resets_at_ms) = window.resets_at_ms {
                            spans.push(Span::styled(
                                format!(
                                    " · {}",
                                    crate::format::fmt_reset(report.generated_at_ms, resets_at_ms)
                                ),
                                theme.dim_style(),
                            ));
                        }
                        lines.push(Line::from(spans));
                    }
                }
                AccountMeterStateV1::Unavailable { reason } => {
                    // The typed reason, honestly — NEVER a fabricated bar.
                    lines.push(Line::from(vec![
                        Span::styled("    meter unavailable · ", theme.warn_style()),
                        Span::styled(crate::format::fmt_meter_reason(reason), theme.warn_style()),
                    ]));
                }
                AccountMeterStateV1::LocalOnly => {
                    // API-key/custom: no server meter EXISTS — no 0-100
                    // bar; the local counters below are the only truth.
                    lines.push(Line::styled(
                        "    api key — no provider meter · local counters only",
                        theme.dim_style(),
                    ));
                }
            }

            // Local journal stats: duration/cost/LOC, then token splits.
            let local = &account.local;
            let cost = if account.auth_method == haider_protocol::credential::AuthMethod::ApiKey {
                local
                    .est_cost_usd
                    .map_or_else(|| "est $—".to_owned(), |usd| format!("est ${usd:.2}"))
            } else {
                local.api_equivalent_est_cost_usd.map_or_else(
                    || "plan · $— API rate".to_owned(),
                    |usd| format!("plan · ≈${usd:.2} API rate"),
                )
            };
            lines.push(Line::styled(
                format!(
                    "    local — {} session{} · {} · {cost} · +{} −{} lines",
                    local.sessions,
                    if local.sessions == 1 { "" } else { "s" },
                    fmt_elapsed(local.total_duration_ms),
                    local.lines_added,
                    local.lines_removed,
                ),
                theme.dim_style(),
            ));
            lines.push(Line::styled(
                format!(
                    "    tokens — in {} · out {} · reasoning {} · cached {}",
                    fmt_tok(local.input_tokens),
                    fmt_tok(local.output_tokens),
                    fmt_tok(local.reasoning_tokens),
                    fmt_tok(local.cached_tokens),
                ),
                theme.dim_style(),
            ));
            if local.cache.logical_input_tokens > 0 {
                let hit = local
                    .cache
                    .complete_hit_rate()
                    .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.2}%", rate * 100.0));
                let coverage = local
                    .cache
                    .telemetry_coverage()
                    .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.0}%", rate * 100.0));
                lines.push(Line::styled(
                    format!(
                        "    cache — uncached {} · write {} (5m {} · 1h {}) · read {} · {hit} hit · coverage {coverage}",
                        fmt_tok(local.cache.uncached_input_tokens),
                        fmt_tok(local.cache.cache_write_tokens),
                        fmt_tok(local.cache.cache_write_5m_tokens),
                        fmt_tok(local.cache.cache_write_1h_tokens),
                        fmt_tok(local.cache.cache_read_tokens),
                    ),
                    theme.dim_style(),
                ));
                match (
                    local.cache.input_with_cache_usd,
                    local.cache.input_without_cache_usd,
                    local.cache.estimated_savings_usd,
                ) {
                    (Some(with), Some(without), Some(savings))
                        if account.auth_method
                            == haider_protocol::credential::AuthMethod::ApiKey => lines.push(Line::styled(
                        format!(
                            "    input $ — ${with:.4} cached · ${without:.4} without · ${savings:.4} savings"
                        ),
                        theme.dim_style(),
                    )),
                    _ if account.auth_method
                        == haider_protocol::credential::AuthMethod::OAuth => {
                            let equivalent = match (
                                local.cache.api_equivalent_input_with_cache_usd,
                                local.cache.api_equivalent_input_without_cache_usd,
                                local.cache.api_equivalent_estimated_savings_usd,
                            ) {
                                (Some(with), Some(without), Some(savings)) => format!(
                                    "    input cost — plan · ≈${with:.4} cached · ≈${without:.4} without · ≈${savings:.4} API-rate savings"
                                ),
                                _ => "    input cost — plan · $— API rate".to_owned(),
                            };
                            lines.push(Line::styled(equivalent, theme.dim_style()));
                        }
                    _ => lines.push(Line::styled(
                        "    input $ — caching $— · without $— · savings $—",
                        theme.dim_style(),
                    )),
                }
            }
            lines.push(Line::raw(""));
        }

        // Device totals over every SHOWN account (all tabs, not just the
        // selected ones): sessions/duration/LOC attribute uniquely to a
        // dominant account (U1's law), so the sums never double-count.
        if !groups.is_empty() {
            let shown: Vec<&haider_protocol::usage::AccountUsageReportV1> = groups
                .iter()
                .flat_map(|group| group.accounts.iter())
                .filter_map(|&slot| report.accounts.get(slot))
                .collect();
            let sessions: u64 = shown.iter().map(|account| account.local.sessions).sum();
            let duration: u64 = shown
                .iter()
                .map(|account| account.local.total_duration_ms)
                .sum();
            let added: u64 = shown.iter().map(|account| account.local.lines_added).sum();
            let removed: u64 = shown
                .iter()
                .map(|account| account.local.lines_removed)
                .sum();
            // Credentials with no durable token/cost fact are not usage
            // lanes and therefore do not poison a complete total. Once an
            // account has any token truth, an absent price remains `$—`.
            let cost_lanes = shown
                .iter()
                .copied()
                .filter(|account| {
                    let local = &account.local;
                    local.input_tokens > 0
                        || local.output_tokens > 0
                        || local.reasoning_tokens > 0
                        || local.cached_tokens > 0
                        || local.cache.logical_input_tokens > 0
                        || local.est_cost_usd.is_some()
                        || local.api_equivalent_est_cost_usd.is_some()
                })
                .collect::<Vec<_>>();
            let costs: Vec<f64> = cost_lanes
                .iter()
                .filter(|account| {
                    account.auth_method == haider_protocol::credential::AuthMethod::ApiKey
                })
                .filter_map(|account| account.local.est_cost_usd)
                .collect();
            let metered_accounts = cost_lanes
                .iter()
                .filter(|account| {
                    account.auth_method == haider_protocol::credential::AuthMethod::ApiKey
                })
                .count();
            let real_cost = if metered_accounts == 0 {
                "plan".to_owned()
            } else if costs.len() != metered_accounts {
                "est $— (metered lanes)".to_owned()
            } else {
                format!("est ${:.2} (metered lanes)", costs.iter().sum::<f64>())
            };
            let has_oauth = cost_lanes.iter().any(|account| {
                account.auth_method == haider_protocol::credential::AuthMethod::OAuth
            });
            let api_costs = cost_lanes
                .iter()
                .filter_map(|account| account.local.api_equivalent_est_cost_usd)
                .collect::<Vec<_>>();
            let api_equivalent = if !has_oauth {
                String::new()
            } else if api_costs.len() == cost_lanes.len() {
                format!(
                    " · ≈${:.2} API rate (all lanes)",
                    api_costs.iter().sum::<f64>()
                )
            } else {
                " · $— API rate (all lanes)".to_owned()
            };
            let cost = format!("{real_cost}{api_equivalent}");
            lines.push(Line::from(vec![
                Span::styled(
                    "THIS DEVICE",
                    theme
                        .bright_style()
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " — journal totals over {} shown account{}",
                        shown.len(),
                        if shown.len() == 1 { "" } else { "s" }
                    ),
                    theme.dim_style(),
                ),
            ]));
            lines.push(Line::styled(
                format!(
                    "    {sessions} session{} · {} · {cost} · +{added} −{removed} lines",
                    if sessions == 1 { "" } else { "s" },
                    fmt_elapsed(duration),
                ),
                theme.dim_style(),
            ));
        }
    } else if !model.mode.fabricates_locally()
        && !model.usage.fetching
        && model.usage.error.is_none()
    {
        lines.push(Line::styled(
            "  no usage snapshot yet — f fetches one",
            theme.dim_style(),
        ));
    }

    // F2b: a PINNED footer hint under the scrolling report; tiny frames
    // keep the flowed layout (still reachable by scrolling to the end).
    let hint = match model.usage.scope {
        crate::app::UsageScope::Models => {
            "↑↓/PgUp/PgDn scroll · r range · f refresh · s next scope · esc back"
        }
        crate::app::UsageScope::History => "PgUp/PgDn scroll · f refresh · s next scope · esc back",
        crate::app::UsageScope::Calendar => {
            "</> account · ↑↓ provider · PgUp/PgDn scroll · f refresh · s next scope · esc back"
        }
        crate::app::UsageScope::Accounts | crate::app::UsageScope::Global => {
            "←/→ account · ↑↓ provider · r reveal · f refresh · s next scope · esc back"
        }
    };
    let mut footer_lines: Vec<Line<'_>> = Vec::new();
    let pinned = area.height >= 12;
    if pinned {
        footer_lines.push(Line::styled(hint, theme.faint_style()));
    } else {
        lines.push(Line::styled(hint, theme.faint_style()));
    }
    let footer_height = u16::try_from(footer_lines.len()).unwrap_or(0);
    let [report_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(area);

    // RENDER is the single scroll authority (the transcript's law): the
    // frame writes the true max, reconciles the offset, and resolves a
    // cursor-follow latch against ITS OWN line layout.
    let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_scroll = total.saturating_sub(report_area.height);
    model.usage.scroll_max.set(max_scroll);
    let mut scroll = model.usage.scroll.get().min(max_scroll);
    if model.usage.follow_cursor.take() {
        let header = header_lines
            .get(model.usage.cursor)
            .and_then(|&line| u16::try_from(line).ok())
            .unwrap_or(0);
        if header < scroll {
            scroll = header;
        } else if header + 1 >= scroll + report_area.height {
            scroll = (header + 2)
                .saturating_sub(report_area.height)
                .min(max_scroll);
        }
    }
    model.usage.scroll.set(scroll);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), report_area);
    // The house scroll indicator: `⋮` gutter marks on the edge rows while
    // content hides beyond them.
    if scroll > 0 && report_area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled("⋮", theme.faint_style())),
            Rect {
                x: report_area.x,
                y: report_area.y,
                width: 1.min(report_area.width),
                height: 1,
            },
        );
    }
    if scroll < max_scroll && report_area.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::styled("⋮", theme.faint_style())),
            Rect {
                x: report_area.x,
                y: report_area.y + report_area.height - 1,
                width: 1.min(report_area.width),
                height: 1,
            },
        );
    }
    if footer_height > 0 {
        frame.render_widget(Paragraph::new(footer_lines), footer_area);
    }

    for (line_index, x, width, hit) in chip_hits {
        let line = u16::try_from(line_index).unwrap_or(u16::MAX);
        if line < scroll || line - scroll >= report_area.height || x >= report_area.width {
            continue;
        }
        hits.push((
            Rect {
                x: report_area.x + x,
                y: report_area.y + (line - scroll),
                width: width.min(report_area.width - x),
                height: 1,
            },
            hit,
        ));
    }
}

/// F2a — the full-screen `/model` picker: OAuth subscriptions remain exact
/// rows; API models expand to an exact-provider stage when necessary. Both
/// stages keep live search, current/pending identity, aggregate availability,
/// and honest inline errors visible.
fn render_model_picker(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let Some(picker) = &model.model_picker else {
        return;
    };
    if area.height < 4 || area.width < 8 {
        return;
    }
    let top_level = picker.provider_stage.is_none();
    let rows = model.model_picker_filtered(&picker.query);
    let [title_area, search_area, note_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let (title, title_note) = if let Some(stage) = &picker.provider_stage {
        (
            " PROVIDERS",
            format!(" — {} · choose which API serves it", stage.model),
        )
    } else {
        (
            " MODELS",
            " — OAuth subscriptions + one API choice per model".to_owned(),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                title,
                theme
                    .bright_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(title_note, theme.dim_style()),
        ])),
        title_area,
    );
    // The search band: live substring over model + represented providers.
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ❯ ", theme.gold_style()),
            Span::styled(picker.query.clone(), theme.input_style()),
            Span::styled("▮", theme.gold_style()),
        ]))
        .style(theme.input_style()),
        search_area,
    );
    // Inline error (typed refusal / unavailability) outranks the header.
    if let Some(error) = &picker.error {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ✗ ", theme.err_style()),
                Span::styled(error.clone(), theme.err_style()),
            ])),
            note_area,
        );
    } else {
        let count = if top_level {
            format!(
                "   {} choices · ⏎ selects OAuth or opens API providers",
                rows.len()
            )
        } else {
            format!(
                "   {} API providers · ⏎ selects the highlighted provider",
                rows.len()
            )
        };
        frame.render_widget(
            Paragraph::new(Line::styled(count, theme.faint_style())),
            note_area,
        );
    }

    // Column budget: model + provider columns sized to content, capped.
    let is_top_api_group =
        |row: &crate::app::ModelPickerRow| top_level && row.auth == "api" && !row.model.is_empty();
    let provider_label = |row: &crate::app::ModelPickerRow| {
        if is_top_api_group(row) && row.providers.len() > 1 {
            format!("{} providers", row.providers.len())
        } else {
            row.provider.clone()
        }
    };
    let model_w = rows
        .iter()
        .map(|row| row.model.chars().count().max(1))
        .max()
        .unwrap_or(8)
        .clamp(8, 30);
    let provider_w = rows
        .iter()
        .map(|row| provider_label(row).chars().count())
        .max()
        .unwrap_or(8)
        .clamp(8, 22);
    let window_len = list_area.height as usize;
    // The menu viewport law (the `/` palette's follow rule): the selection
    // moves INSIDE a stable window; the window scrolls only when the
    // selection would leave it — never a list scrolling under a pinned
    // highlight. `scroll` is the remembered top; render owns the math since
    // only render knows `window_len`.
    let selection = picker.selection.min(rows.len().saturating_sub(1));
    let start = crate::app::follow_viewport(picker.scroll.get(), selection, rows.len(), window_len);
    picker.scroll.set(start);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "   nothing matches — esc closes, backspace edits the search",
                theme.dim_style(),
            )),
            list_area,
        );
    }
    for (offset, row) in rows.iter().skip(start).take(window_len).enumerate() {
        let index = start + offset;
        let y = list_area.y + u16::try_from(offset).unwrap_or(0);
        let selected = index == selection;
        let pending = picker
            .pending
            .as_ref()
            .is_some_and(|(provider, model_name)| {
                *model_name == row.model
                    && if is_top_api_group(row) {
                        row.providers.contains(provider)
                    } else {
                        *provider == row.provider
                    }
            });
        let hidden_above = offset == 0 && start > 0;
        let hidden_below = offset + 1 == window_len && start + window_len < rows.len();
        let gutter = if hidden_above || hidden_below {
            "⋮"
        } else if row.is_current {
            "●"
        } else {
            " "
        };
        let gutter_style = if row.is_current {
            theme.gold_style()
        } else {
            theme.faint_style()
        };
        let ink = if !row.available || !row.selectable {
            theme.dim_style()
        } else {
            theme.text_style()
        };
        let model_cell = if row.model.is_empty() {
            "—".to_owned()
        } else {
            row.model.clone()
        };
        let default_mark = if row.default_providers > 0 { "*" } else { " " };
        let provider_cell = provider_label(row);
        let provider_cell = if row.lockdown {
            format!("🔒 {provider_cell}")
        } else {
            provider_cell
        };
        let mut spans = vec![
            Span::styled(format!(" {gutter} "), gutter_style),
            Span::styled(
                format!("{model_cell:<model_w$}"),
                if selected {
                    theme
                        .bright_style()
                        .add_modifier(ratatui::style::Modifier::BOLD)
                } else if row.available && row.selectable {
                    theme.text_style()
                } else {
                    theme.dim_style()
                },
            ),
            Span::styled(default_mark.to_owned(), theme.gold_style()),
            Span::raw(" "),
            Span::styled(format!("{provider_cell:<provider_w$}"), ink),
            Span::raw(" "),
            Span::styled(format!("{:<5}", row.auth), theme.dim_style()),
            Span::styled(
                if row.context_window_varies {
                    format!("{:>8}", "varies")
                } else {
                    row.context_window.map_or_else(
                        || format!("{:>8}", "—"),
                        |window| format!("{:>8}", crate::format::fmt_tok(window)),
                    )
                },
                theme.dim_style(),
            ),
        ];
        if row.is_current {
            let current = if is_top_api_group(row) && row.providers.len() > 1 {
                format!(
                    "  current · {}",
                    row.current_provider.as_deref().unwrap_or(&row.provider)
                )
            } else {
                "  current".to_owned()
            };
            spans.push(Span::styled(current, theme.gold_style()));
        }
        if pending {
            spans.push(Span::styled(
                "  …",
                theme.pulse_ink(theme.gold, model.anim_phase),
            ));
        }
        // Refusal truth is mandatory row grammar; aggregate diagnostics are
        // optional detail and must never displace it at bounded widths.
        if let Some(reason) = row.reason.as_ref().filter(|_| !row.available) {
            spans.push(Span::styled(format!("  {reason}"), theme.faint_style()));
        }
        if is_top_api_group(row) {
            spans.push(Span::styled(
                format!(
                    "  {}/{} available",
                    row.available_providers,
                    row.providers.len()
                ),
                theme.faint_style(),
            ));
            if row.lockdown_providers > 0 {
                spans.push(Span::styled(
                    format!(
                        " · {}/{} lockdown",
                        row.lockdown_providers,
                        row.providers.len()
                    ),
                    theme.faint_style(),
                ));
            }
            if row.default_providers > 0 {
                spans.push(Span::styled(
                    format!(" · {} default", row.default_providers),
                    theme.faint_style(),
                ));
            }
        }
        if let Some(age) = row.inventory_age_ms {
            let label = if is_top_api_group(row) {
                "freshest age"
            } else {
                "age"
            };
            spans.push(Span::styled(
                format!("  {label} {}", fmt_inventory_age(age)),
                theme.faint_style(),
            ));
        }
        let mut line = Line::from(spans);
        if selected {
            line = hover_band(line, true, area.width, theme);
        }
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: list_area.x,
                y,
                width: list_area.width,
                height: 1,
            },
        );
        hits.push((
            Rect {
                x: list_area.x,
                y,
                width: list_area.width,
                height: 1,
            },
            Hit::ModelPickerRow {
                provider: row.provider.clone(),
                model: row.model.clone(),
                api_group: top_level && row.auth == "api" && !row.model.is_empty(),
            },
        ));
    }
    let hint = if top_level {
        " ⏎ select OAuth / open API providers · tab providers/trust · esc close · ↑↓ move · type to search"
    } else {
        " ⏎ select provider · tab toggle trust · esc models · ↑↓ move · type to search"
    };
    frame.render_widget(
        Paragraph::new(Line::styled(hint, theme.faint_style())),
        hint_area,
    );
}

fn fmt_inventory_age(age_ms: u64) -> String {
    if age_ms < 60_000 {
        format!("{}s", age_ms / 1_000)
    } else if age_ms < 3_600_000 {
        format!("{}m", age_ms / 60_000)
    } else {
        format!("{}h", age_ms / 3_600_000)
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
    let menu_wrapped_body_rows = menu.map_or(0, |m| {
        wrapped_menu_body(m, area.width, model.clock_ms).len()
    });
    // The computer-permission grant card replaces the plain blocking menu and
    // needs a few extra rows for its labelled prompt + action buttons.
    let card_rows = model.projection.permission_card().map(permission_card_rows);
    let mut needed_input = menu.map_or_else(
        || composer_height(model, area.width).saturating_add(ask_rows),
        |m| u16::try_from(1 + menu_wrapped_body_rows + m.options.len() + 1).unwrap_or(u16::MAX),
    );
    if let Some(rows) = card_rows {
        needed_input = needed_input.max(rows);
    }
    // What the input may claim: everything beyond header(2) + header
    // rule(1) + input rule(1) + gap(1) + one sacred transcript row.
    let mut gap: u16 = 1;
    let mut transcript_min: u16 = 1;
    let mut header_h: u16 = 2;
    let mut header_rule_h: u16 = 1;
    let mut input_rule_h: u16 = 1;
    let mut floor_input = menu.map_or(1, |m| u16::try_from(m.options.len().max(1)).unwrap_or(1));
    if let Some(rows) = card_rows {
        floor_input = floor_input.max(rows);
    }
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
    // W-A: the running background-task band — one ambient line above the
    // composer, shed with the waiting line's priority (a map of live work,
    // like the SubTree, but one row).
    let tasks_line = crate::taskrows::tasks_line(&model.tasks, model.clock_ms);
    let mut tasks_height = u16::from(tasks_line.is_some());
    // W-G: the throughput readout no longer rides the band — it is now the
    // left segment of the composer identity line (always visible, even at
    // rest). See `render_composer`.
    // CG-M1: the always-visible graph strip — one ambient line while a graph
    // is pinned and not abandoned (an abandoned graph clears the strip). It
    // is session state and the lowest-priority ambient row; it sheds before
    // sacred rows on a tiny terminal.
    let graph_strip = model
        .graph
        .as_ref()
        .filter(|status| status.phase != haider_protocol::graph::GraphPhase::Abandoned);
    let mut graph_height = u16::from(graph_strip.is_some());
    // W-G (owner 2026-08-15): the throughput readout's OWN ambient row,
    // directly above the composer band — persistent at rest (the last
    // turn's rate), fixed-width rolling spark. A meter is the lowest-
    // priority ambient row: it sheds before every map of live work.
    // Height floor for the ambient meter rows: on a very short terminal the
    // transcript's visible tail outranks a meter — at 90x10 the persistent
    // throughput row consumed the row the streamed REPLY needed (the live
    // probe caught it). /retry stays reachable as a command below the floor.
    let meters_fit = area.height >= 16;
    let throughput_readout = model.throughput_pill().filter(|_| meters_fit);
    let mut throughput_height = u16::from(throughput_readout.is_some());
    // Owner 2026-08-16/17 (manual retry): an ACTIONABLE recovery row when
    // the last run terminal-failed OR the run is mid-BACKOFF (the daemon's
    // wake seam short-circuits the remaining delay) — click (or /retry)
    // re-runs / fires the next attempt NOW. Outranks the throughput meter.
    let retry_backoff = model.projection.retrying().is_some();
    let retry_row = meters_fit
        && (model.projection.run_errored() || retry_backoff)
        && model.daemon_serves(haider_rpc::FEATURE_RUN_RETRY_V1);
    let mut retry_height = u16::from(retry_row);
    // CU-2: the sacred screen-control banner. While the model is moving the
    // real cursor/keyboard, the owner MUST see it — this row is claimed
    // before every other optional panel (below) and never sheds.
    let screen_control = model.projection.screen_control_active();
    let mut screen_control_height = u16::from(screen_control);
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
    // 954: the live queue panel (daemon-held rows) and the demo's local
    // msg_queue share the slot; live rows win when present.
    let live_queue_rows = model.queue_panel.rows.len();
    let queue_error_line = usize::from(model.queue_panel.error.is_some());
    let mut queue_height = if live_queue_rows + queue_error_line > 0 {
        u16::try_from(live_queue_rows + queue_error_line + 1).unwrap_or(6)
    } else if model.msg_queue.is_empty() {
        0
    } else {
        u16::try_from(model.msg_queue.len() + 1).unwrap_or(4)
    };
    let mut subtree_height = subtree_needed(model, false);
    let showing_backtrack = model.backtrack.is_some();
    let palette = if showing_backtrack {
        backtrack_block(model, theme, area.width)
    } else if model.palette_open() {
        palette_block(model, theme, area.width)
    } else {
        Vec::new()
    };
    let mut palette_height = u16::try_from(palette.len()).unwrap_or(0);
    let mut budget = area.height.saturating_sub(fixed + transcript_min);
    // CU-2: the screen-control banner is claimed before every optional
    // panel — a session driving the real cursor must always be visible.
    if screen_control_height > budget {
        screen_control_height = 0;
    } else {
        budget -= screen_control_height;
    }
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
    if tasks_height > budget {
        tasks_height = 0;
    } else {
        budget -= tasks_height;
    }
    if todos_height > budget {
        todos_height = 0;
    } else {
        budget -= todos_height;
    }
    // CG-M1: the graph strip is the lowest-priority ambient row (checked last
    // = lowest claim on the budget) — the task band and todos outrank it.
    if graph_height > budget {
        graph_height = 0;
    } else {
        budget -= graph_height;
    }
    // The retry row claims before the meter — an actionable recovery
    // affordance outranks telemetry.
    if retry_height > budget {
        retry_height = 0;
    } else {
        budget -= retry_height;
    }
    // The throughput meter claims LAST — beneath even the graph strip.
    if throughput_height > budget {
        throughput_height = 0;
    } else {
        budget -= throughput_height;
    }
    // The closing rule was reserved ABOVE, before the panels (TUI6.1
    // fix 2 — sim anatomy: the border-top of whatever follows the
    // InputBar, SubTree tui.js:4764 / StatusBar tui.js:5497).
    // S2 owner item 4: the TUI6-era PAD row (the InputBar's bottom
    // padding) is RETIRED — the band rests at exactly ONE text row
    // between its rules and grows only with content. The breathing room
    // that separates the transcript's tail from the band lives in the
    // TRANSCRIPT stream now (its one trailing blank line, S2 item 5), so
    // it scrolls with the history instead of padding the chrome.
    // One breathing row above each block that is actually present, taken
    // last and given up first.
    // The waiting line and the task band share one breathing row (they are
    // the same "live background work" block when both are present).
    // 970 owner bug 1: the SubTree gets NO breathing row. The band already
    // closes with its rule (`band_rule_h`), and `render_subagent` has always
    // gone `❯ message …` → rule → `▾ subagents` with nothing between (TUI6
    // item 6, the owner's own screenshot). The session surface kept a
    // `lead_subtree` blank on top of that rule, so the same band read one
    // row taller here than on the subagent screen — the extra empty line
    // under the composer. Session parity: the rule is the separator.
    let want_lead = u16::from(waiting_height + tasks_height + graph_height > 0);
    let want_todos_lead = u16::from(todos_height > 0);
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
    let [
        header_area,
        header_rule,
        transcript_area,
        _lead_waiting,
        waiting_area,
        tasks_area,
        graph_area,
        _lead_todos,
        todos_area,
        queue_area,
        palette_area,
        throughput_area,
        retry_area,
        screen_control_area,
        rule_area,
        composer_area,
        band_rule_area,
        subtree_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Length(header_rule_h),
        Constraint::Min(transcript_min),
        Constraint::Length(lead_waiting),
        Constraint::Length(waiting_height),
        Constraint::Length(tasks_height),
        Constraint::Length(graph_height),
        Constraint::Length(lead_todos),
        Constraint::Length(todos_height),
        Constraint::Length(queue_height),
        Constraint::Length(palette_height),
        Constraint::Length(throughput_height),
        Constraint::Length(retry_height),
        Constraint::Length(screen_control_height),
        Constraint::Length(input_rule_h),
        Constraint::Length(input_height),
        Constraint::Length(band_rule_h),
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
        let rows = crate::mark::half_block_cells(&crate::mark::HEADER);
        header_top.push(Span::raw("  "));
        header_top.extend(mark_tone_spans(&rows[0], theme, mark_ink));
        header_top.push(Span::raw("  "));
        header_bottom.push(Span::raw("  "));
        header_bottom.extend(mark_tone_spans(&rows[1], theme, mark_ink));
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
    // W-flow inline identity: a BOUND agent type recolors the session's own
    // accent — the head callsign trades its gold for the type's registry
    // color and a `{glyph} @{id}` chip rides beside it. The fallback law is
    // exact: no binding / no snapshot entry / un-Loom daemon → today's gold,
    // never a stale accent (`bound_loom_type` re-judges every frame).
    let bound = model.bound_loom_type(model.identity.agent_type.as_deref());
    let head_accent = bound
        .and_then(|record| crate::style::loom_accent_style(&record.color))
        .unwrap_or_else(|| theme.gold_style());
    header_bottom.extend([
        Span::styled(title, theme.dim_style()),
        Span::styled(format!(" ▸ {head} {honorific}"), head_accent),
    ]);
    if let Some(record) = bound {
        let accent =
            crate::style::loom_accent_style(&record.color).unwrap_or_else(|| theme.gold_style());
        header_bottom.push(Span::styled(" · ".to_owned(), theme.dim_style()));
        if !record.glyph.is_empty() {
            header_bottom.push(Span::styled(format!("{} ", record.glyph), accent));
        }
        header_bottom.push(Span::styled(
            format!("@{}", record.id),
            accent.add_modifier(Modifier::BOLD),
        ));
    }
    header_bottom.push(Span::styled(
        format!(" · branch main · {}", identity.device),
        theme.dim_style(),
    ));
    // Shed chrome renders nothing: a 1-row header keeps only the product
    // line (the area clips line 2), a 0-row header/rule disappears whole.
    let header_top_used = u16::try_from(Line::from(header_top.clone()).width()).unwrap_or(0);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(header_top),
            Line::from(header_bottom),
        ]))
        .style(theme.text_style()),
        header_area,
    );
    // The voice/dictation chip in the TOP-RIGHT of the header (owner: moved off
    // the status bar), opposite the left-aligned wordmark/product line.
    render_header_voice_chip(model, theme, frame, header_area, header_top_used);
    // Replace the half-block header mark with the crisp حيدر image on a graphics
    // terminal — same 24×2 footprint, at the fixed slot after the back chip and
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
    // Entry formatting/wrap geometry is cached under the exact projection
    // revision, width, and theme. The frame below receives only the viewport
    // window plus bounded overscan, while every scroll/jump coordinate stays
    // in the same global wrapped-row space as before.
    let mut transcript_cache = model.transcript_layout.borrow_mut();
    transcript_cache.reconcile(
        &model.projection,
        model.theme,
        theme,
        transcript_area.width,
        model.anim_phase,
    );
    let mut tail: Vec<Line<'static>> = Vec::new();
    // Sim `.thinking` (tui.js:4458-4462): a gold tail for the WHOLE running
    // turn, pulsing (1.4s). The port also breathes the dot ● ↔ ◌ on the
    // shared clock — the owner's marquee "alive" element.
    // Gated on the run being ACTIVE, not on the Thinking beat: the beat-only
    // gate blanked the indicator the instant the run reached `Streaming`, so
    // a plainly-working turn looked dead above the composer. `Retrying` stays
    // out of this set — it owns the dedicated tail row pushed just below.
    if model.projection.is_turn_active() {
        // S2 item 5: one breathing row above the badge — it must never
        // sit flush against the last output line.
        tail.push(Line::default());
        tail.push(owned_line(thinking_line(
            theme,
            model.anim_phase,
            model.truecolor,
        )));
    }
    // M4: a retryable provider failure backs off with a visible attempt
    // counter, on the same transcript-tail surface as the thinking line.
    if let Some((attempt, max, delay_ms)) = model.projection.retrying() {
        tail.push(Line::default());
        tail.push(owned_line(retrying_line(theme, attempt, max, delay_ms)));
    }
    // S2 item 5: exactly ONE blank line between the transcript's last
    // output and the composer band. The breathing row rides the STREAM
    // (the pad row died with S2 item 4), so at the bottom-anchored tail
    // it separates output from the band and it scrolls with history.
    if !transcript_cache.entries.is_empty() || !tail.is_empty() {
        tail.push(Line::default());
    }
    let total = transcript_cache
        .total_rows
        .saturating_add(u64::from(wrapped_lines_height(
            &tail,
            transcript_area.width,
        )));
    let max_scroll_rows = total.saturating_sub(u64::from(transcript_area.height));
    let max_scroll = max_scroll_rows;
    // RENDER is the single scroll authority (review r3 P2-2): the frame
    // writes the true max AND reconciles the model's offset against it, so
    // resizes/new content can never leave invisible debt banked anywhere.
    model.scroll_max.set(max_scroll);
    // The drag-autoscroll edge geometry (QoL wave) rides the same
    // frame-feedback discipline as the max above.
    model.transcript_view.set(transcript_area);
    model
        .scroll_back
        .set(model.scroll_back.get().min(max_scroll));
    // B2b-m3: resolve an armed tree jump IN THIS FRAME — node → display
    // entry → wrapped row, every step through the renderer's width-keyed
    // geometry. A resize invalidates the cache before this lookup, so the
    // anchor clears only when it LANDS.
    // (A taken jump whose branch is no longer displayed stays dropped: it
    // is never resolved against another branch's rows.)
    if let Some(jump) = model.pending_jump.take()
        && jump.branch.as_ref() == model.branch_state.active()
    {
        match model.projection.entry_of_node(&jump.node) {
            Some(entry) => {
                let row = transcript_cache.row_start(&model.projection, entry)
                    + u64::from(matches!(
                        model.projection.entries().get(entry),
                        Some(TranscriptEntry::User { .. })
                    ));
                // A near-tail target cannot be top-aligned without fake
                // padding: clamp honestly and let it sit where the real
                // rows put it.
                let target_top = row.min(max_scroll_rows);
                model
                    .scroll_back
                    .set(max_scroll_rows.saturating_sub(target_top));
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
    let (visible_lines, visible_base, visible_total, scroll) = virtualized_transcript_lines(
        &mut transcript_cache,
        &model.projection,
        theme,
        model.anim_phase,
        TranscriptViewport {
            prefix: &[],
            suffix: &tail,
            scroll_back,
            height: transcript_area.height,
            width: transcript_area.width,
        },
    );
    let corrected_max = visible_total.saturating_sub(u64::from(transcript_area.height));
    model.scroll_max.set(corrected_max);
    model
        .scroll_back
        .set(model.scroll_back.get().min(corrected_max));
    // D4: an open `plan` proposal owns the transcript area — the full
    // markdown document renders here (scrolled), while the decision menu
    // keeps the composer band through the ordinary blocking-menu path.
    let plan_menu = model
        .projection
        .open_menu()
        .filter(|open| open.origin == "plan" && !open.options.is_empty() && model.login.is_none());
    if let Some(plan) = plan_menu {
        // Review round 2: RENDER owns the new-proposal reset — plan B paints
        // from the top even before any keypress reaches the key handler.
        // Round 3: keyed by (id, body length) so a re-issued id with new
        // content still resets.
        let plan_key = crate::app::plan_menu_key(plan);
        if model.plan_menu_seen.borrow().as_ref() != Some(&plan_key) {
            *model.plan_menu_seen.borrow_mut() = Some(plan_key);
            model.plan_scroll.set(0);
        }
        let max_scroll =
            render_plan_document(plan, theme, frame, transcript_area, model.plan_scroll.get());
        model.plan_scroll_max.set(max_scroll);
    } else {
        let paragraph = Paragraph::new(Text::from(visible_lines)).wrap(Wrap { trim: false });
        frame.render_widget(
            paragraph.scroll((
                u16::try_from(scroll.saturating_sub(visible_base)).unwrap_or(u16::MAX),
                0,
            )),
            transcript_area,
        );
        image_reveal_hits(
            &transcript_cache,
            &model.projection,
            0,
            scroll,
            transcript_area,
            hits,
        );
    }
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
    // Review round 2: while a plan owns the transcript, the sticky prompt
    // band must neither paint over the document nor keep a clickable hit.
    if plan_menu.is_none()
        && scroll_back > 0
        && scroll > 0
        && transcript_area.height > 0
        && !model.sticky_suppressed.get()
    {
        let user_entries = model.projection.user_entries();
        let sticky_index = user_entries
            .partition_point(|entry| {
                transcript_cache
                    .row_start(&model.projection, *entry)
                    .saturating_add(1)
                    < scroll
            })
            .checked_sub(1)
            .and_then(|position| user_entries.get(position).copied());
        if let Some(entry_index) = sticky_index
            && let Some(TranscriptEntry::User { text, .. }) =
                model.projection.entries().get(entry_index)
        {
            let row = transcript_cache
                .row_start(&model.projection, entry_index)
                .saturating_add(1);
            let jump = visible_total
                .saturating_sub(u64::from(transcript_area.height))
                .saturating_sub(row);
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
            // Verify round 2: hit dispatch is FIRST-match, and the sticky
            // band paints OVER whatever transcript row (an image row's
            // full-width hit included) occupies row zero — so its hit must
            // lead the map, or a click on the visible band would fire the
            // hidden row underneath.
            hits.insert(0, (sticky_rect, Hit::StickyJump(jump)));
        }
    }

    // 954 owner item: the BOTTOM complement of the sticky band. Every
    // FOLLOWING frame stamps the watermark (render is the single scroll
    // authority, so "seen" is defined by what a following frame showed);
    // while scrolled back, a right-aligned chip on the transcript's last
    // row names how much arrived unseen and clicks back to follow. Same
    // plan-menu suppression as the sticky band (surface ownership law).
    if model.scroll_back.get() == 0 {
        model.bottom_watermark.set(model.projection.entries().len());
    } else if plan_menu.is_none() && transcript_area.height > 1 {
        let unseen = model
            .projection
            .entries()
            .len()
            .saturating_sub(model.bottom_watermark.get());
        let label = if unseen > 0 {
            format!(" {unseen} new · Jump to bottom ↓ ")
        } else {
            " Jump to bottom ↓ ".to_owned()
        };
        let width = (label.chars().count() as u16).min(transcript_area.width);
        let band_rect = Rect {
            x: transcript_area.x + transcript_area.width.saturating_sub(width),
            y: transcript_area.y + transcript_area.height - 1,
            width,
            height: 1,
        };
        let hovered = model.hovered == Some(Hit::JumpToBottom);
        let style = if hovered {
            theme.sticky_hover_style()
        } else {
            theme.sticky_style()
        };
        frame.render_widget(
            Paragraph::new(Line::raw(ellipsize(&label, transcript_area.width as usize)))
                .style(style),
            band_rect,
        );
        // First-match dispatch: the chip paints over the transcript row
        // beneath it, so its hit must lead (the sticky-band precedent).
        hits.insert(0, (band_rect, Hit::JumpToBottom));
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

    // W-A: the running background-task band — same ambient voice as the
    // waiting line (gold sigil + dim text), never a hit; elapsed figures
    // tick on the S4 clock while any task runs.
    if let Some(line) = &tasks_line
        && tasks_area.height > 0
    {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled("⚙", theme.gold_style()),
                Span::styled(
                    line.strip_prefix('⚙').unwrap_or(line).to_owned(),
                    theme.dim_style(),
                ),
            ])),
            tasks_area,
        );
    }

    // CG-M1: the graph strip — the ship-loop's live position above the
    // composer while a graph is pinned.
    if let Some(status) = graph_strip
        && graph_area.height > 0
    {
        frame.render_widget(Paragraph::new(graph_strip_line(theme, status)), graph_area);
        // M2c: the strip is clickable — a press opens the `/graph` telemetry
        // screen (the owner's "click the workflow → stats" gesture).
        hits.push((graph_area, Hit::GraphStrip));
    }

    // W-G (owner 2026-08-15): the throughput row — fixed-width rolling spark
    // + rate on its own line above the composer band. Gold spark + bright
    // rate while streaming; the whole line dims at rest, wearing the last
    // turn's measured rate.
    if let Some(readout) = &throughput_readout
        && throughput_area.height > 0
    {
        let streaming = model.projection.is_streaming();
        let (spark_ink, rate_ink) = if streaming {
            (theme.gold_style(), theme.bright_style())
        } else {
            (theme.dim_style(), theme.dim_style())
        };
        // tpsfix (owner 2026-09-03): a SMALL fixed-width strip, left-anchored
        // above the composer — `PILL_WIDTH` cells (6 bar columns + a 4-cell
        // rate field + ` tps`), about a quarter of the old ~40-column row. It
        // does NOT scale with the terminal, so nothing downstream of it moves
        // on a resize, and `· μN` is gone: at turn end it duplicated the
        // headline number (it survives on the verbose `--plain` row).
        let spans = vec![
            Span::raw(" "),
            Span::styled(readout.spark.clone(), spark_ink),
            Span::styled(readout.rate_field(), rate_ink),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), throughput_area);
    }

    // Owner 2026-08-16: the manual-retry row — the recovery affordance for a
    // terminal-failed run. Clickable; /retry is the keyboard path. While the
    // command is in flight the row wears its progress and takes no clicks.
    if retry_row && retry_area.height > 0 {
        if model.retry_inflight {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(" "),
                    Span::styled("↻ retrying…", theme.dim_style()),
                ])),
                retry_area,
            );
        } else {
            let (message, ink) = if retry_backoff {
                (
                    " retrying automatically — click to retry NOW",
                    theme.gold_style(),
                )
            } else {
                (
                    " run failed — click to retry the same turn",
                    theme.err_style(),
                )
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(" "),
                    Span::styled("↻", theme.gold_style()),
                    Span::styled(message, ink),
                    Span::styled(" · /retry", theme.dim_style()),
                ])),
                retry_area,
            );
            hits.push((retry_area, Hit::RetryRun));
        }
    }

    // CU-2: the sacred screen-control banner — a warm, unmissable strip
    // while a session is moving the real cursor/keyboard. Esc is the same
    // interrupt that stops any turn (the daemon's cancel token aborts the
    // in-flight computer action), so the copy points there.
    if screen_control && screen_control_area.height > 0 {
        let warn = theme.warn_style().add_modifier(Modifier::BOLD);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ◉ ", warn),
                Span::styled("controlling your screen", warn),
                Span::styled(" — esc to stop", theme.dim_style()),
            ]))
            .style(theme.warn_style()),
            screen_control_area,
        );
    }
    if queue_height > 0 && (live_queue_rows > 0 || model.queue_panel.error.is_some()) {
        // 954: the LIVE queue panel — daemon-held rows, render-complete
        // (oldest top, latest bottom), each with its delivery-mode toggle
        // and a steer button. Mutations ride the held revision; a stale
        // fence re-reads (never guesses), so these buttons cannot act on
        // a row the list shifted under.
        let mut queue_lines = vec![Line::from(vec![
            Span::styled("⧗ queued", theme.gold_style()),
            Span::styled(
                format!(
                    " — {live_queue_rows} held · daemon-timed · steer sends now · mode cycles turn end ⇄ next tool",
                ),
                theme.dim_style(),
            ),
        ])];
        if let Some(error) = &model.queue_panel.error {
            queue_lines.push(Line::styled(format!("  ✗ {error}"), theme.err_style()));
        }
        let button_w = "  [⇄ next tool]  [steer]".chars().count();
        for (index, row) in model.queue_panel.rows.iter().enumerate() {
            let mode_label = match row.mode {
                haider_protocol::DeliveryMode::Queue => "turn end",
                haider_protocol::DeliveryMode::Subturn => "next tool",
                haider_protocol::DeliveryMode::Steer => "steer soon",
            };
            let toggle_label = match row.mode {
                haider_protocol::DeliveryMode::Queue => "[⇄ next tool]",
                _ => "[⇄ turn end]",
            };
            let text_budget =
                (queue_area.width as usize).saturating_sub(button_w + mode_label.len() + 10);
            let shown = ellipsize(&row.text.replace('\n', " "), text_budget.max(8));
            let left = format!("  {}. {shown}", index + 1);
            let left_width = left.chars().count() + mode_label.len() + 3;
            let pad = (queue_area.width as usize)
                .saturating_sub(left_width + button_w)
                .max(1);
            let row_y = queue_area.y
                + 1
                + u16::from(model.queue_panel.error.is_some())
                + u16::try_from(index).unwrap_or(u16::MAX);
            let toggle_hovered = model.hovered == Some(Hit::QueueRowToggle(row.id.clone()));
            let steer_hovered = model.hovered == Some(Hit::QueueRowSteer(row.id.clone()));
            queue_lines.push(Line::from(vec![
                Span::styled(left, theme.dim_style()),
                Span::styled(format!(" ({mode_label})"), theme.gold_style()),
                Span::raw(" ".repeat(pad)),
                Span::styled(
                    toggle_label.to_owned(),
                    if toggle_hovered {
                        theme.bright_style()
                    } else {
                        theme.dim_style()
                    },
                ),
                Span::raw("  "),
                Span::styled(
                    "[steer]".to_owned(),
                    if steer_hovered {
                        theme.bright_style()
                    } else {
                        theme.gold_style()
                    },
                ),
            ]));
            if row_y < queue_area.y + queue_height {
                let toggle_x = queue_area.x
                    + u16::try_from((queue_area.width as usize).saturating_sub(button_w))
                        .unwrap_or(0);
                let toggle_rect = Rect {
                    x: toggle_x,
                    y: row_y,
                    width: 15,
                    height: 1,
                };
                let steer_rect = Rect {
                    x: toggle_x + 17,
                    y: row_y,
                    width: 7,
                    height: 1,
                };
                hits.push((toggle_rect, Hit::QueueRowToggle(row.id.clone())));
                hits.push((steer_rect, Hit::QueueRowSteer(row.id.clone())));
            }
        }
        frame.render_widget(Paragraph::new(Text::from(queue_lines)), queue_area);
    } else if queue_height > 0 {
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
        if !showing_backtrack {
            palette_row_hits(model, palette_area, hits);
        }
    }

    if model.login.is_none()
        && let Some(card) = model.projection.permission_card()
    {
        // The computer OS-permission grant card takes the blocking-menu slot,
        // enriching the paired `computer-os-permission` menu with Open Settings
        // / Retry and the native-prompt explanation. The login card outranks it
        // for the same reason it outranks a menu (keyboard ownership).
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(rule_area.width as usize),
                theme.warn_style(),
            ))
            .style(theme.text_style()),
            rule_area,
        );
        let (card_lines, button_hits) = permission_card_block(card, theme, composer_area);
        frame.render_widget(
            Paragraph::new(Text::from(card_lines)).style(theme.menu_style()),
            composer_area,
        );
        for (row_offset, hit) in button_hits {
            let y = composer_area.y + row_offset;
            if y < composer_area.y + composer_area.height {
                hits.push((
                    Rect {
                        x: composer_area.x,
                        y,
                        width: composer_area.width,
                        height: 1,
                    },
                    hit,
                ));
            }
        }
    } else if let Some(menu) = menu {
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
        let (menu_lines, option_rows) = menu_block(
            menu,
            model.menu_selection,
            theme,
            composer_area,
            &footer,
            model.clock_ms,
        );
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
    } else if effort_picker_showing(model) {
        render_effort_picker(model, theme, frame, rule_area, composer_area, hits);
    } else if theme_picker_showing(model) {
        render_theme_picker(model, theme, frame, rule_area, composer_area, hits);
    } else {
        render_composer(model, theme, frame, rule_area, composer_area, hits);
    }
    // The inputBg band is one panel edge to edge (owner item 2); S2 item 4
    // retired its padding row — the closing rule sits directly under the
    // last composer row, so the band rests at ONE line and grows only
    // with content.
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
/// `/resume` — every session on the machine, ordered by attention (owner
/// 2026-08-21). Rows render from ROSTER truth alone: the unseen dot and
/// the typed needs-you chip come from the daemon's own attention fields
/// (v0.0.936/937), so this list agrees with the ADE's rail by construction
/// and needs no journal replay to draw.
fn render_sessions(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let rows = model.session_browser_rows();
    let needs = rows.iter().filter(|row| row.needs_input.is_some()).count();
    let unseen = rows.iter().filter(|row| row.unseen).count();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("SESSIONS", theme.bright_style()),
            Span::styled(
                format!("  {} on this machine", rows.len()),
                theme.dim_style(),
            ),
            Span::styled(
                if needs > 0 {
                    format!("  ·  {needs} need you")
                } else {
                    String::new()
                },
                theme.gold_style(),
            ),
            Span::styled(
                if unseen > 0 {
                    format!("  ·  {unseen} unseen")
                } else {
                    String::new()
                },
                theme.maroon_style(),
            ),
        ]),
        Line::styled(
            if model.session_browser_query.is_empty() {
                "  search: type title / dir / model / id".to_owned()
            } else {
                format!("  search: {}▏  ·  esc clears", model.session_browser_query)
            },
            theme.dim_style(),
        ),
    ];
    if rows.is_empty() {
        lines.push(Line::styled(
            if model.session_browser_query.is_empty() {
                "  no sessions yet — start one from the launcher"
            } else {
                "  no sessions match"
            },
            theme.dim_style(),
        ));
    }
    // The list body: one row per session, the selected row banded.
    let list_top = area.y.saturating_add(2);
    let visible = area.height.saturating_sub(3) as usize;
    let first = model
        .session_browser_sel
        .saturating_sub(visible.saturating_sub(1));
    for (offset, row) in rows.iter().skip(first).take(visible).enumerate() {
        let index = first + offset;
        let selected = index == model.session_browser_sel;
        // The attention marks lead the row: gold needs-you chip, then the
        // accent unseen dot (matching the ADE's accent-family dot so the
        // two surfaces read identically).
        let mark = if row.needs_input.is_some() {
            Span::styled("  ◆ ", theme.gold_style())
        } else if row.unseen {
            Span::styled("  ● ", theme.maroon_style())
        } else {
            Span::styled("    ", theme.dim_style())
        };
        let mut spans = vec![
            mark,
            Span::styled(
                ellipsize(&row.title, 44),
                if selected {
                    theme.bright_style()
                } else {
                    theme.text_style()
                },
            ),
        ];
        if let Some(card) = &row.needs_input {
            spans.push(Span::styled(
                format!("  [{}]", needs_input_label(card)),
                theme.gold_style(),
            ));
        }
        if let Some(agent_type) = &row.agent_type {
            spans.push(Span::styled(
                format!("  {agent_type}"),
                theme.maroon_style(),
            ));
        }
        spans.push(Span::styled(
            format!("  {}  ·  {}  ·  {}", row.model_short, row.dir, row.ago),
            theme.dim_style(),
        ));
        let mut line = Line::from(spans);
        if selected {
            line = line.style(theme.hover_style());
        }
        lines.push(line);
        let row_y = list_top.saturating_add(offset as u16);
        if row_y < area.y.saturating_add(area.height) {
            hits.push((
                Rect::new(area.x, row_y, area.width, 1),
                Hit::AttachSession(row.id.clone()),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The needs-you chip's copy for one unified card.
fn needs_input_label(card: &haider_rpc::NeedsInputWire) -> &'static str {
    use haider_rpc::NeedsInputKindWire;
    match card.kind {
        NeedsInputKindWire::Permission => "permission",
        NeedsInputKindWire::Question => "question",
        NeedsInputKindWire::Approval => "approval",
        NeedsInputKindWire::Recovery => "recover",
        NeedsInputKindWire::Secret => "secret",
        NeedsInputKindWire::Update => "update",
        NeedsInputKindWire::TrustHook => "trust",
        NeedsInputKindWire::Choice => "choice",
        NeedsInputKindWire::Conflict => "conflict",
        NeedsInputKindWire::File => "file",
        NeedsInputKindWire::Exhausted => "exhausted",
        NeedsInputKindWire::Unknown => "input",
    }
}

/// W-flow — the declared CLIs this device does NOT have, in declaration
/// order. A name the daemon never probed is OMITTED: unknown is not
/// missing, and offering to install something we never checked would be a
/// guess dressed as a fact.
pub fn missing_clis(
    model: &AppModel,
    record: &haider_protocol::loom::LoomAgentType,
) -> Vec<String> {
    record
        .clis
        .iter()
        .filter(|cli| model.loom_cli_present.get(*cli) == Some(&false))
        .cloned()
        .collect()
}

fn workflow_dependency_layers(
    template: &haider_protocol::graph::GraphTemplateSpec,
) -> std::collections::HashMap<&str, usize> {
    use haider_protocol::graph::GraphNodeName;

    let mut layers = std::collections::HashMap::new();
    let known: std::collections::HashSet<&str> = template
        .nodes
        .iter()
        .map(|node| node.name.as_str())
        .collect();
    // Fixed-point relaxation, capped by node count: every pass can only
    // raise a layer, and a valid DAG settles within `nodes.len()` passes.
    for _ in 0..=template.nodes.len() {
        let mut changed = false;
        for node in &template.nodes {
            let depth = node
                .depends_on
                .iter()
                .map(GraphNodeName::as_str)
                .filter(|dependency| known.contains(dependency))
                .map(|dependency| layers.get(dependency).map_or(0, |value| value + 1))
                .max()
                .unwrap_or(0);
            let entry = layers.entry(node.name.as_str()).or_insert(0);
            if depth > *entry {
                *entry = depth;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    layers
}

/// Lay a workflow's node graph out in dependency LAYERS and draw it, so the
/// shape of a flow is visible at a glance instead of reconstructed from a
/// flat list of `← after` clauses (owner 2026-08-22).
///
/// Layer(n) = 0 when a node has no dependencies, else 1 + max(layer(deps)).
/// Nodes that share a layer run concurrently — which is exactly the fact a
/// flat list hides. The walk is iteration-capped: template validation already
/// rejects cycles, but a renderer must not hang on a malformed one.
pub fn workflow_dag_lines(
    template: &haider_protocol::graph::GraphTemplateSpec,
    theme: &Theme,
) -> Vec<Line<'static>> {
    use haider_protocol::graph::GraphNodeName;

    let layer = workflow_dependency_layers(template);
    let depth = layer.values().copied().max().unwrap_or(0);
    let mut lines = vec![Line::from(vec![
        Span::styled("  DAG", theme.bright_style().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                "  {} node{} · {} layer{}",
                template.nodes.len(),
                if template.nodes.len() == 1 { "" } else { "s" },
                depth + 1,
                if depth == 0 { "" } else { "s" },
            ),
            theme.dim_style(),
        ),
    ])];

    for level in 0..=depth {
        let mut here: Vec<&haider_protocol::graph::GraphNodeSpec> = template
            .nodes
            .iter()
            .filter(|node| layer.get(node.name.as_str()).copied().unwrap_or(0) == level)
            .collect();
        here.sort_by_key(|node| node.name.as_str());
        if here.is_empty() {
            continue;
        }
        if level > 0 {
            lines.push(Line::styled("      │", theme.faint_style()));
        }
        let concurrent = here.len() > 1;
        for (index, node) in here.iter().enumerate() {
            let stem = if level == 0 {
                "   "
            } else if concurrent && index + 1 < here.len() {
                "  ├"
            } else if concurrent {
                "  └"
            } else {
                "  ▼"
            };
            let mut spans = vec![
                Span::styled(format!("{stem} "), theme.faint_style()),
                Span::styled("◆ ", theme.gold_style()),
                Span::styled(
                    node.name.as_str().to_owned(),
                    theme.bright_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {} · {}",
                        crate::graph::gate_kind_label(&node.gate),
                        crate::graph::executor_label(node.executor),
                    ),
                    theme.dim_style(),
                ),
            ];
            if !node.depends_on.is_empty() {
                spans.push(Span::styled(
                    format!(
                        "  ← {}",
                        node.depends_on
                            .iter()
                            .map(GraphNodeName::as_str)
                            .collect::<Vec<_>>()
                            .join(" + ")
                    ),
                    theme.faint_style(),
                ));
            }
            lines.push(Line::from(spans));
        }
        if concurrent {
            lines.push(Line::styled(
                "        concurrent — these run together",
                theme.faint_style(),
            ));
        }
    }
    lines.push(Line::raw(""));
    lines
}

/// Draw the exact frozen runtime topology from L2's activation AST. The
/// mutable workflow catalog may decorate the surrounding row, but never
/// supplies edges or node identity for a graph that is already running.
pub fn workflow_live_dag_lines(
    projection: &haider_client::WorkflowGraphProjection,
    theme: &Theme,
) -> Vec<Line<'static>> {
    use haider_client::{WorkflowGraphEdgeKind, WorkflowNodeState};
    use std::collections::HashMap;

    let mut incoming: HashMap<String, Vec<String>> = HashMap::new();
    let mut outgoing: HashMap<String, Vec<String>> = HashMap::new();
    let mut back: HashMap<String, Vec<String>> = HashMap::new();
    for edge in projection.edges() {
        if edge.kind == WorkflowGraphEdgeKind::Forward
            && let Some(source) = &edge.from
        {
            incoming
                .entry(edge.to.clone())
                .or_default()
                .push(source.clone());
            outgoing
                .entry(source.clone())
                .or_default()
                .push(edge.to.clone());
        } else if edge.kind == WorkflowGraphEdgeKind::Back
            && let Some(source) = &edge.from
        {
            back.entry(source.clone())
                .or_default()
                .push(edge.to.clone());
        }
    }
    let mut layer: HashMap<String, usize> = HashMap::new();
    let node_count = projection.nodes().count();
    for _ in 0..=node_count {
        let mut changed = false;
        for node in projection.nodes() {
            let depth = incoming
                .get(&node.node_id)
                .into_iter()
                .flatten()
                .map(|source| layer.get(source).map_or(0, |value| value + 1))
                .max()
                .unwrap_or(0);
            let entry = layer.entry(node.node_id.clone()).or_insert(0);
            if depth > *entry {
                *entry = depth;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let depth = layer.values().copied().max().unwrap_or(0);
    let mut lines = vec![Line::from(vec![
        Span::styled("  LIVE", theme.gold_style().add_modifier(Modifier::BOLD)),
        Span::styled(
            projection.cursor().map_or_else(
                || "  · connecting…".to_owned(),
                |cursor| format!("  · cursor {cursor}"),
            ),
            theme.dim_style(),
        ),
        Span::styled(
            projection
                .graph_id()
                .map_or_else(String::new, |graph_id| format!("  · {graph_id}")),
            theme.faint_style(),
        ),
    ])];

    for level in 0..=depth {
        let here: Vec<&haider_client::WorkflowNodeProjection> = projection
            .nodes()
            .filter(|node| layer.get(&node.node_id).copied().unwrap_or(0) == level)
            .collect();
        if here.is_empty() {
            continue;
        }
        if level > 0 {
            lines.push(Line::styled("      │", theme.faint_style()));
        }
        for node in here {
            let parents = incoming.get(&node.node_id).cloned().unwrap_or_default();
            if parents.len() > 1 {
                lines.push(Line::styled(
                    format!("      └─ join ← {}", parents.join(" + ")),
                    theme.faint_style(),
                ));
            }
            let branch_parent = parents.iter().find(|source| {
                outgoing
                    .get(*source)
                    .is_some_and(|targets| targets.len() > 1)
            });
            let stem = if level == 0 {
                "   "
            } else if let Some(source) = branch_parent {
                if outgoing.get(source).and_then(|targets| targets.last()) == Some(&node.node_id) {
                    "  └"
                } else {
                    "  ├"
                }
            } else {
                "  ▼"
            };
            let (glyph, label, style) = match node.status {
                WorkflowNodeState::Waiting => {
                    let has_input = node.present_input_count() > 0;
                    if has_input {
                        ("◐", "waiting", theme.dim_style())
                    } else {
                        ("○", "waiting on evidence", theme.faint_style())
                    }
                }
                WorkflowNodeState::Ready => ("◆", "ready", theme.gold_style()),
                WorkflowNodeState::Active => (
                    "◉",
                    "active",
                    theme.selection_style().add_modifier(Modifier::BOLD),
                ),
                WorkflowNodeState::Complete => (
                    "✓",
                    "complete",
                    Style::default()
                        .fg(theme.ok.into())
                        .add_modifier(Modifier::BOLD),
                ),
                WorkflowNodeState::Rejected => (
                    "✗",
                    "rejected",
                    Style::default()
                        .fg(theme.err.into())
                        .add_modifier(Modifier::BOLD),
                ),
                // `WorkflowNodeState` is intentionally non-exhaustive across
                // the client/TUI crate boundary. A newer state stays visible
                // and neutral until this renderer learns its semantics.
                _ => ("?", "unknown state", theme.faint_style()),
            };
            let mut spans = vec![
                Span::styled(format!("{stem} "), theme.faint_style()),
                Span::styled(format!("{glyph} "), style),
                Span::styled(node.node_id.clone(), style),
                Span::styled(format!("  {label}"), style),
            ];
            if !node.inputs_present.is_empty() {
                spans.push(Span::styled("  inputs ", theme.dim_style()));
                for present in &node.inputs_present {
                    spans.push(Span::styled(
                        if *present { "●" } else { "○" },
                        if *present {
                            theme.gold_style()
                        } else {
                            theme.faint_style()
                        },
                    ));
                }
            }
            if !parents.is_empty() {
                spans.push(Span::styled(
                    format!("  ← {}", parents.join(" + ")),
                    theme.faint_style(),
                ));
            }
            lines.push(Line::from(spans));
            if let Some(rejection) = projection.rejection(&node.node_id) {
                lines.push(Line::from(vec![
                    Span::styled("      ↳ reject ", theme.dim_style()),
                    Span::styled(rejection.code_label(), theme.maroon_style()),
                    Span::styled(
                        format!(" · journal cursor {}", rejection.cursor),
                        theme.faint_style(),
                    ),
                ]));
                if let Some(reference) = &rejection.evidence {
                    lines.push(Line::from(vec![
                        Span::styled("        evidence ", theme.dim_style()),
                        Span::styled(reference.as_str().to_owned(), theme.maroon_style()),
                    ]));
                } else {
                    lines.push(Line::styled(
                        "        no evidence artifact published",
                        theme.faint_style(),
                    ));
                }
            }
            if let Some(targets) = outgoing
                .get(&node.node_id)
                .filter(|targets| targets.len() > 1)
            {
                lines.push(Line::styled(
                    format!("      ├─ fork → {}", targets.join(" + ")),
                    theme.faint_style(),
                ));
            }
            if let Some(targets) = back.get(&node.node_id) {
                lines.push(Line::styled(
                    format!("      ↺ reject back → {}", targets.join(" + ")),
                    theme.warn_style(),
                ));
            }
        }
    }
    lines.push(Line::raw(""));
    lines
}

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
            let linked = entry.menu_id().is_some();
            let suffix = if linked { "  [decision menu]" } else { "" };
            let line_index = lines.len();
            lines.push(Line::styled(
                format!("  {}{suffix}", entry.line()),
                if linked {
                    theme.gold_style()
                } else {
                    theme.dim_style()
                },
            ));
            if let Some(menu) = entry.menu_id() {
                hits.push((
                    row_rect(area, area.y, line_index),
                    Hit::HookFiring(menu.clone()),
                ));
            }
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
    if let Some(menu) = &hooks.drilldown {
        let mut card_lines = vec![
            Line::styled("decision menu · read-only", theme.gold_style()),
            Line::styled(menu.title.clone(), theme.bright_style()),
        ];
        card_lines.extend(
            menu.body
                .iter()
                .map(|line| Line::styled(line.clone(), theme.dim_style())),
        );
        for (index, option) in menu.options.iter().enumerate() {
            card_lines.push(Line::styled(
                format!("{}. {}", index + 1, option.label),
                theme.text_style(),
            ));
        }
        card_lines.push(Line::styled("esc back to firings", theme.faint_style()));
        let height = u16::try_from(card_lines.len() + 2)
            .unwrap_or(u16::MAX)
            .min(area.height);
        let width = area.width.saturating_sub(4).max(24);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
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
        // 970 owner item 1: with no subagents the panel still owes ONE row
        // whenever background work is running, because that row is now the
        // only place the shells/monitors counts appear (the status-bar
        // segments are gone). With nothing running at all it collapses
        // entirely, exactly as before.
        return u16::from(!model.band_counts().is_empty());
    }
    if model.subtree_collapsed {
        return 1;
    }
    // The ⌂ main row is part of the map on BOTH screens (owner item 3), so
    // it always costs a row while the panel is open.
    let _ = on_subagent;
    let summary = usize::from(subtree_metrics_summary(model).is_some());
    let chip_rows = crate::app::flatten_chips(&model.chips).len();
    // Fleet entry law: at ≥ ENTRY_COLLAPSE tree nodes the per-chip rows
    // collapse into ONE summary row (`⣿ … · ⌥F fleet`).
    let tree_rows = if chip_rows >= crate::fleet::ENTRY_COLLAPSE {
        1
    } else {
        chip_rows
    };
    let rows = tree_rows + 1 + summary;
    u16::try_from(rows + 1).unwrap_or(u16::MAX)
}

fn direct_metrics(model: &AppModel) -> Option<Vec<&haider_protocol::agent::AgentMetricsSnapshot>> {
    let snapshots = model
        .chips
        .iter()
        .map(|chip| model.chip_metrics(chip))
        .collect::<Option<Vec<_>>>()?;
    (!snapshots.is_empty()).then_some(snapshots)
}

fn subtree_metrics_summary(model: &AppModel) -> Option<String> {
    let aggregate = crate::agent_metrics::aggregate(direct_metrics(model)?)?;
    let (tokens, cost) = aggregate.usage.as_ref().map_or_else(
        || ("n/a tokens".to_owned(), "cost n/a".to_owned()),
        |usage| {
            (
                format!(
                    "{} tokens",
                    fmt_tok(crate::agent_metrics::normalized_tokens(usage))
                ),
                crate::agent_metrics::compact_cost(usage),
            )
        },
    );
    Some(format!(
        "subagents total — {} tools · {tokens} · {cost}",
        aggregate.tool_attempts
    ))
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

/// The legacy S4 chip-row meta — `elapsed · ↓ N tokens` for old daemons.
/// Segment PRESENCE is data truth: a source with nothing to say drops its
/// segment (unknown is never rendered as `0s`/`0 tokens`). Width
/// degradation drops WHOLE segments in law order — tokens first, then
/// elapsed (the F2c pattern); never a mid-segment truncation. Every glyph
/// used (`↓`, `·`, ASCII) is single-width, so char count IS cell width.
fn legacy_chip_row_meta(
    elapsed: Option<&str>,
    tokens: Option<&str>,
    budget: usize,
) -> Option<String> {
    let full = match (elapsed, tokens) {
        (Some(elapsed), Some(tokens)) => format!("{elapsed} · {tokens}"),
        (Some(elapsed), None) => elapsed.to_owned(),
        (None, Some(tokens)) => tokens.to_owned(),
        (None, None) => return None,
    };
    if full.chars().count() <= budget {
        return Some(full);
    }
    // Tokens drop FIRST, elapsed survives alone — or nothing at all.
    let elapsed = elapsed?.to_owned();
    (elapsed.chars().count() <= budget).then_some(elapsed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetricSegment {
    Elapsed,
    Live,
    Tools,
    Tokens,
    Cost,
}

fn metrics_chip_row_meta(
    elapsed: String,
    live: bool,
    tools: u64,
    usage: Option<&haider_protocol::agent::AgentUsageMetrics>,
    budget: usize,
) -> Option<String> {
    let mut segments = vec![(MetricSegment::Elapsed, elapsed)];
    if live {
        segments.push((MetricSegment::Live, "live".to_owned()));
    }
    segments.push((MetricSegment::Tools, format!("{tools} tools")));
    if let Some(usage) = usage {
        segments.push((
            MetricSegment::Tokens,
            format!(
                "{} tokens",
                fmt_tok(crate::agent_metrics::normalized_tokens(usage))
            ),
        ));
        segments.push((
            MetricSegment::Cost,
            crate::agent_metrics::compact_cost(usage),
        ));
    }
    let width = |segments: &[(MetricSegment, String)]| {
        segments
            .iter()
            .map(|(_, segment)| segment.chars().count())
            .sum::<usize>()
            .saturating_add(segments.len().saturating_sub(1) * 3)
    };
    for drop in [
        MetricSegment::Live,
        MetricSegment::Elapsed,
        MetricSegment::Cost,
        MetricSegment::Tools,
        MetricSegment::Tokens,
    ] {
        if width(&segments) <= budget {
            break;
        }
        segments.retain(|(kind, _)| *kind != drop);
    }
    (!segments.is_empty() && width(&segments) <= budget).then(|| {
        segments
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join(" · ")
    })
}

/// The SubTree panel (§2.9): header toggle + depth-first rows with
/// connectors; every row opens its chip's view. Shared by the session and
/// subagent screens (the map is one surface).
/// The act-slot sentence for a chip running a pinned workflow — the DAG
/// position instead of tool chatter (sim tui.js:5410-5428, Image #26 law).
fn workflow_chip_activity(roll: &haider_protocol::agent::AgentGraphRollupV1) -> String {
    match roll.state.as_str() {
        "complete" => format!("✓ {}/{} nodes green", roll.nodes_green, roll.nodes_total),
        "failed" => format!("✗ {}/{} nodes green", roll.nodes_green, roll.nodes_total),
        "gate" => match roll.gate.as_deref() {
            Some("human") | None => "⛩ gate — needs your confirm".to_owned(),
            Some(kind) => format!("⛩ {kind} gate — waiting"),
        },
        _ => format!("node {}/{}", roll.node_index.max(1), roll.nodes_total),
    }
}

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
    let has_chips = !model.chips.is_empty();
    let arrow = if model.subtree_collapsed {
        "▸"
    } else {
        "▾"
    };
    // 970 owner item 1: Claude Code's task line — the band's header row
    // carries the background counts RIGHT-ALIGNED (`· 2 shells · 1 monitor`),
    // each one its own click target. With no subagents the left half is
    // simply absent and the counts stand alone on the row.
    let mut header: Vec<Span<'static>> = if has_chips {
        vec![
            Span::styled(format!("{arrow} subagents"), theme.gold_style()),
            Span::styled(format!(" — {}", subtree_counts(model)), theme.dim_style()),
        ]
    } else {
        Vec::new()
    };
    let counts = model.band_counts();
    // Measured BEFORE the pad so the hit rects land on the glyphs the
    // reader actually sees, not on the gap.
    let counts_width = crate::taskrows::band_counts_text(&counts).chars().count();
    let header_width = Line::from(header.clone()).width();
    // A ≥2-cell gap: the counts must never kiss the subagent summary. When
    // the row cannot hold both, the counts yield — the panel's own state is
    // the more important half of the row.
    let count_spans_fit =
        !counts.is_empty() && header_width + 2 + counts_width <= area.width as usize;
    let mut count_hits: Vec<(u16, u16, Hit)> = Vec::new();
    if count_spans_fit {
        let pad = (area.width as usize)
            .saturating_sub(header_width)
            .saturating_sub(counts_width);
        header.push(Span::raw(" ".repeat(pad)));
        let mut cursor = header_width + pad;
        for (index, count) in counts.iter().enumerate() {
            if index > 0 {
                header.push(Span::raw(" "));
                cursor += 1;
            }
            let width = count.text.chars().count();
            let hit = match count.kind {
                crate::taskrows::BandCountKind::Shells => Hit::ShellStatus,
                crate::taskrows::BandCountKind::Monitors => Hit::MonitorStatus,
            };
            if let (Ok(x), Ok(width_u16)) = (u16::try_from(cursor), u16::try_from(width)) {
                count_hits.push((x, width_u16, hit));
            }
            header.push(Span::styled(count.text.clone(), theme.dim_style()));
            cursor += width;
        }
    }
    let mut lines = vec![Line::from(header)];
    // The counts are pushed FIRST: `hit_rect_at` takes the FIRST rect that
    // contains the pointer, and the toggle's row-wide rect would otherwise
    // swallow every click on them.
    for (x, width, hit) in count_hits {
        let x = area.x.saturating_add(x);
        let end = area.x.saturating_add(area.width);
        if x < end {
            hits.push((
                Rect::new(x, area.y, width.min(end.saturating_sub(x)), 1),
                hit,
            ));
        }
    }
    // With no subagents there is no panel to collapse — the row is counts
    // only, so it carries no toggle.
    let mut row_hits: Vec<(usize, Hit)> = if has_chips {
        vec![(0, Hit::SubTreeToggle)]
    } else {
        Vec::new()
    };
    // No chips means no map to draw — the counts-only row stands alone and
    // owes neither the ⌂ home row nor a tree.
    if has_chips && !model.subtree_collapsed {
        if let Some(summary) = subtree_metrics_summary(model) {
            lines.push(Line::styled(format!("  {summary}"), theme.dim_style()));
        }
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
        // Fleet entry law (slice 1): at ≥ ENTRY_COLLAPSE tree nodes the
        // per-chip rows collapse into ONE summary row — the fleet view's
        // session-born door (⌥F's clickable twin, mockup tui.js:4562-4565).
        if total >= crate::fleet::ENTRY_COLLAPSE {
            let roll = crate::fleet::entry_rollup(&model.chips);
            let mut line = Line::from(vec![
                Span::styled(" ⣿ ", theme.gold_style()),
                Span::styled(crate::fleet::entry_summary(&roll), theme.dim_style()),
            ]);
            line = hover_band(
                line,
                model.hovered == Some(Hit::FleetSummary),
                area.width,
                theme,
            );
            row_hits.push((lines.len(), Hit::FleetSummary));
            lines.push(line);
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
            return;
        }
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
            } else if let Some(roll) = &chip.graph {
                // Sim workflow chips (tui.js:5410-5428): the act slot
                // speaks the DAG — nodes green, gate wait, or position.
                workflow_chip_activity(roll)
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
            ];
            // D1: a typed child's task label leads with `@type ·` (the C2
            // spawn convention) — paint the type segment in its Loom accent
            // and glyph so specialization reads at a glance.
            // Review round 2: the `@type · ` prefix is DAEMON truth only on
            // a Loom-aware daemon (C3 strips cosplay there); the shared
            // [`crate::app::AppModel::loom_task_type`] gate enforces it for
            // this site and the fleet rows alike.
            match model.loom_task_type(&chip.name) {
                Some((record, remainder)) if !chip.closed => {
                    let accent_style = crate::style::loom_accent_style(&record.color)
                        .unwrap_or_else(|| theme.gold_style());
                    spans.push(Span::styled(" · ".to_owned(), ink));
                    if !record.glyph.is_empty() {
                        spans.push(Span::styled(format!("{} ", record.glyph), accent_style));
                    }
                    spans.push(Span::styled(
                        format!("@{}", record.id),
                        accent_style.add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(
                        format!(" · {remainder} · {}", chip.model),
                        ink,
                    ));
                }
                _ => {
                    spans.push(Span::styled(
                        format!(" · {} · {}", chip.name, chip.model),
                        ink,
                    ));
                }
            }
            if chip.lockdown {
                spans.push(Span::styled(" · 🔒", theme.gold_style()));
            }
            if let Some(roll) = chip.graph.as_ref().filter(|_| !chip.closed) {
                // Sim: the NAME stays put while the workflow identity and
                // its current position rotate beside it.
                let workflow = roll.workflow_id.as_deref().unwrap_or("workflow");
                spans.push(Span::styled(" · ".to_owned(), ink));
                spans.push(Span::styled(format!("⛩ {workflow}"), theme.gold_style()));
                if roll.state == "running" {
                    if let Some(label) = roll.node_label.as_deref() {
                        spans.push(Span::styled(format!(" · {label}"), ink));
                    }
                } else {
                    spans.push(Span::styled(format!(" · {}", roll.state), ink));
                }
            }
            if *depth == 0 {
                spans.push(Span::styled(format!(" · {}", chip.device), ink));
            }
            spans.push(Span::styled(format!(" — {activity}"), ink));
            // S4: right-aligned compact metrics in the dim slot. New daemons
            // provide the agent-preserving snapshot; old daemons keep the
            // exact elapsed/token truth chain as a compatibility fallback.
            let left = Line::from(spans.clone()).width();
            // ≥2-cell gap: the meta must never kiss the activity text.
            let budget = (area.width as usize).saturating_sub(left).saturating_sub(2);
            let meta = if let Some(metrics) = model.chip_metrics(chip) {
                metrics_chip_row_meta(
                    fmt_elapsed(crate::agent_metrics::elapsed_ms(metrics, model.clock_ms)),
                    metrics.live,
                    metrics.tool_attempts,
                    metrics.usage.as_ref(),
                    budget,
                )
            } else {
                let elapsed = chip.elapsed_ms(model.clock_ms).map(fmt_elapsed);
                let tokens = crate::app::chip_row_tokens(&model.sessions, chip)
                    .map(|total| format!("↓ {} tokens", fmt_tok(total)));
                legacy_chip_row_meta(elapsed.as_deref(), tokens.as_deref(), budget)
            };
            if let Some(meta) = meta {
                let pad = (area.width as usize)
                    .saturating_sub(left)
                    .saturating_sub(meta.chars().count());
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(Span::styled(meta, theme.dim_style()));
            }
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

/// One fleet list row's glyph ink — the mockup's `.fg` vocabulary: live
/// pulses maroon on the shared clock, done/failed wear the status inks,
/// waiting is calm dim, queued/cancelled barely-there. Theme tokens only.
fn fleet_glyph_style(
    theme: &Theme,
    state: haider_rpc::FleetAgentStateWire,
    anim_phase: u8,
) -> ratatui::style::Style {
    use haider_rpc::FleetAgentStateWire as S;
    match state {
        S::Live => theme.pulse_ink(theme.maroon, anim_phase),
        S::Done => theme.ok_style(),
        S::Failed => theme.err_style(),
        S::Waiting => theme.dim_style(),
        S::Queued | S::Cancelled => theme.faint_style(),
        _ => theme.dim_style(),
    }
}

/// Grid cell geometry: inner width and the gap between cells.
const FLEET_CELL_W: usize = 10;
const FLEET_CELL_GAP: usize = 2;

/// The fleet view (slice 1 — [`crate::fleet`]): rollup header with the
/// drill path, two AUTOMATIC densities (tree list ≤20 nodes, max-density
/// matrix grid above), ⏎ re-roots on a subtree, esc walks up/out. The
/// mockup's `FleetStage` grammar; slice 1 ships no agent-detail frame and
/// no manual density toggle.
#[allow(clippy::too_many_lines)]
/// One node's styled span-group for the graph strip and status rows:
/// `LABEL glyph↺N`, the glyph toned by node state (satisfied/current/blocked).
fn graph_node_spans(
    theme: &Theme,
    status: &haider_protocol::graph::GraphStatus,
    node: &haider_protocol::graph::GraphNodeStatus,
) -> Vec<Span<'static>> {
    use haider_protocol::graph::{GraphBlockReason, GraphPhase};
    let glyph = crate::graph::node_glyph(status, node);
    let glyph_style = if status.phase == GraphPhase::Completed || node.satisfied {
        theme.ok_style()
    } else if status.current_node.as_ref() == Some(&node.node) {
        match (status.phase, status.blocked_reason) {
            (GraphPhase::Blocked, Some(GraphBlockReason::HumanHold)) => theme.warn_style(),
            (GraphPhase::Blocked, _) => theme.err_style(),
            _ => theme.gold_style(),
        }
    } else {
        theme.faint_style()
    };
    let mut spans = vec![
        Span::styled(format!("{} ", node.node.label()), theme.dim_style()),
        Span::styled(glyph.to_owned(), glyph_style),
    ];
    let marker = crate::graph::attempt_marker(node);
    if !marker.is_empty() {
        spans.push(Span::styled(marker, theme.warn_style()));
    }
    spans
}

/// One evidence slot's styled provenance row (M2a): `glyph id  word · digest`,
/// toned by state — verified GREEN, attested GOLD (never green: attested is
/// model testimony, not daemon-verified proof), pending faint, failed red.
fn graph_slot_line(
    theme: &Theme,
    slot: &haider_protocol::graph::GraphEvidenceSlotStatus,
) -> Line<'static> {
    use haider_protocol::graph::{EvidenceAuthority, EvidenceVerdict};
    let (glyph, word) = crate::graph::slot_state(slot);
    let word_style = match slot.verdict {
        Some(EvidenceVerdict::Green) => match slot.authority {
            EvidenceAuthority::DaemonVerified => theme.ok_style(),
            EvidenceAuthority::ModelAttested => theme.warn_style(),
        },
        Some(EvidenceVerdict::Red) => theme.err_style(),
        None => theme.faint_style(),
    };
    let mut spans = vec![
        Span::raw("      "),
        Span::styled(format!("{glyph} "), word_style),
        Span::styled(format!("{:<10}", slot.id), theme.dim_style()),
        Span::styled(word.to_owned(), word_style),
    ];
    let provenance = crate::graph::slot_provenance(slot);
    if !provenance.is_empty() {
        spans.push(Span::styled(provenance, theme.faint_style()));
    }
    Line::from(spans)
}

/// The always-visible graph strip line (a `Line` for the panel stack above
/// the composer). `None` when no graph is held — the caller omits the row.
fn graph_strip_line(theme: &Theme, status: &haider_protocol::graph::GraphStatus) -> Line<'static> {
    use haider_protocol::graph::GraphPhase;
    let mut spans = vec![Span::styled(
        format!("⚑ {} ", status.template),
        theme.gold_style(),
    )];
    for node in &status.nodes {
        spans.push(Span::styled(" ", theme.faint_style()));
        spans.extend(graph_node_spans(theme, status, node));
    }
    let badge = crate::graph::phase_badge(status);
    if !badge.is_empty() {
        let badge_style = match status.phase {
            GraphPhase::Completed => theme.ok_style(),
            GraphPhase::Blocked | GraphPhase::Abandoned => theme.err_style(),
            GraphPhase::Superseded => theme.faint_style(),
            GraphPhase::Active => theme.dim_style(),
        };
        spans.push(Span::styled(format!("  {badge}"), badge_style));
    }
    Line::from(spans)
}

/// Round 4 — plain character wrap for /loom detail lines: the screen
/// scrolls VERTICALLY with exact line math (no Paragraph wrap), so long
/// job/pipe lines fold here instead of clipping.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    // Round 5: fold by TERMINAL CELLS, not scalars — CJK/emoji are two
    // cells wide and combining marks are zero — so a folded line never
    // overruns the pane.
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut cells = 0usize;
    for character in text.chars() {
        let advance = character.width().unwrap_or(0);
        if cells + advance > width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            cells = 0;
        }
        current.push(character);
        cells += advance;
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// D3 — the Loom registry browser: agent types + workflows from the daemon
/// snapshot (hydrated per connection, re-read on every pane entry), each in
/// its registry accent; ⏎ opens a detail pane (type: job + grants +
/// know-how; workflow: typed signature + node chain + pipe source; built-in:
/// nodes + gates from the catalog spec; `none`: the honest default line).
/// W-flow: the workflows pane leads with the synthetic `∅ none` row and the
/// daemon-published built-in catalog — `p` pins the selected row to the bound session,
/// `n` opens the describe-it authoring input on both panes.
/// The Loom/Workflows tab keeps its dedicated Loom editor live at its foot:
/// authoring is a multi-step draft/revise/confirm session, so the operator
/// describes, sees the proposal, and refines WITHOUT leaving the registry
/// they are editing. The list takes navigation keys; every printable
/// character goes to the editor, which is why "new" is a
/// clickable row rather than a bare `n`.
fn render_loom(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    // The composer band owns the foot of the tab; the registry gets the rest.
    let composer_rows = composer_height(model, area.width);
    let band = composer_rows.saturating_add(1);
    let list_height = area.height.saturating_sub(band);
    let list_area = Rect::new(area.x, area.y, area.width, list_height);
    let rule_area = Rect::new(area.x, area.y.saturating_add(list_height), area.width, 1);
    let composer_area = Rect::new(
        area.x,
        area.y.saturating_add(list_height).saturating_add(1),
        area.width,
        composer_rows,
    );
    // Painted BEFORE the registry so every early-return path below still
    // leaves the operator a live composer to type into.
    render_composer(model, theme, frame, rule_area, composer_area, hits);
    let area = list_area;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let on_types = model.loom_pane == LoomPane::Types;
    let catalog_available =
        on_types || model.daemon_serves(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1);
    let (title, own, own_noun, sibling) = if on_types {
        ("loom", model.loom_types.len(), "agent type", "workflows")
    } else {
        ("workflows", model.loom_workflows.len(), "workflow", "loom")
    };
    lines.push(Line::from(vec![
        Span::styled(title, theme.bright_style().add_modifier(Modifier::BOLD)),
        Span::styled(
            if catalog_available {
                format!(
                    " — {own} {own_noun}{} registered · tab ⇄ {sibling}",
                    if own == 1 { "" } else { "s" },
                )
            } else {
                format!(" — catalog unavailable · tab ⇄ {sibling}")
            },
            theme.dim_style(),
        ),
    ]));
    lines.push(Line::raw(""));
    if let Some(authoring) = &model.loom_authoring {
        let noun = match authoring.kind {
            haider_protocol::loom::LoomAuthorKind::AgentType => "AGENT TYPE",
            haider_protocol::loom::LoomAuthorKind::Workflow => "WORKFLOW",
        };
        lines.push(Line::from(vec![
            Span::styled("AUTHORING — ", theme.gold_style()),
            Span::styled(noun, theme.bright_style().add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::raw(""));
        if authoring.authoring_id.is_none() {
            lines.push(Line::styled(
                "Describe the agent type or workflow in prose below.",
                theme.text_style(),
            ));
            lines.push(Line::styled(
                "⏎ asks the daemon for an editable typed draft; nothing registers yet.",
                theme.dim_style(),
            ));
        } else {
            lines.push(Line::styled(
                "The typed JSON below is the draft of record. Edit it directly.",
                theme.text_style(),
            ));
            lines.push(Line::styled(
                "workflow nodes expose types, fork/join dependencies, back_edge, and InstructPipe evidence",
                theme.dim_style(),
            ));
        }
        if authoring.pending {
            lines.push(Line::styled(
                "  validating with daemon…",
                theme.gold_style(),
            ));
        } else if !authoring.errors.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("VALIDATION", theme.err_style()));
            for error in &authoring.errors {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(
                            "  {}:{} {} — ",
                            error.location.line, error.location.column, error.location.field
                        ),
                        theme.warn_style(),
                    ),
                    Span::styled(error.message.clone(), theme.text_style()),
                ]));
            }
        } else if authoring.validated {
            lines.push(Line::styled(
                "  ✓ typed validation passed",
                theme.ok_style(),
            ));
        }
        if let Some(digest) = &authoring.preview_digest {
            lines.push(Line::styled(
                format!("  save preview · {}", crate::graph::digest_short(digest)),
                theme.dim_style(),
            ));
        }
        if let Some(confirmed) = &authoring.confirmed {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("CONFIRMED  ", theme.ok_style()),
                Span::styled(
                    format!(
                        "{} rev {} · {}",
                        confirmed.registration.id,
                        confirmed.registration.rev,
                        crate::graph::digest_short(&confirmed.execution_digest)
                    ),
                    theme.bright_style(),
                ),
            ]));
            lines.push(Line::styled(
                "changed content confirms as a new immutable revision and execution hash",
                theme.dim_style(),
            ));
            if let Some(job_id) = &confirmed.install_job_id {
                match authoring
                    .install_job
                    .as_ref()
                    .filter(|job| &job.job_id == job_id)
                {
                    Some(job) => {
                        let state = if job.cancelled {
                            "cancelled".to_owned()
                        } else {
                            format!("{:?}", job.state).to_ascii_lowercase()
                        };
                        lines.push(Line::styled(
                            format!(
                                "install job {job_id} · {state} · {}/{}{}",
                                job.progress.completed,
                                job.progress.total,
                                job.progress
                                    .current_cli
                                    .as_ref()
                                    .map_or_else(String::new, |cli| format!(" · {cli}"))
                            ),
                            theme.gold_style(),
                        ));
                        if !job.state.is_terminal() {
                            lines.push(Line::styled(
                                "⌃X cancel (registration remains retryable)",
                                theme.dim_style(),
                            ));
                        }
                    }
                    None => {
                        lines.push(Line::styled(
                            format!("install job {job_id} · status not reported"),
                            theme.gold_style(),
                        ));
                    }
                }
            }
        }
        lines.push(Line::raw(""));
        let actions = if model.daemon_serves(haider_rpc::FEATURE_LOOM_AUTHORING_V1) {
            "⇧⏎ newline · ⏎ revise · ⌃V validate-only · ⌃S confirm/register · ⌃A archive confirmed · ⌃X cancel install · esc close"
        } else {
            "authoring unavailable on this connection · esc close draft"
        };
        lines.push(Line::styled(actions, theme.dim_style()));
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }
    // Owner 2026-08-22: a visible, CLICKABLE create affordance. Bare `n`
    // stopped being available the moment printable keys started reaching the
    // composer, and a button is what the operator was looking for anyway.
    {
        let new_row = Line::from(vec![
            Span::styled("  ＋ ", theme.gold_style()),
            Span::styled(
                if on_types {
                    "New agent type"
                } else {
                    "New workflow"
                },
                theme.gold_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   describe it below, or click here to start",
                theme.dim_style(),
            ),
        ]);
        let row_y = area.y.saturating_add(lines.len() as u16);
        if row_y < area.y.saturating_add(area.height) {
            hits.push((Rect::new(area.x, row_y, area.width, 1), Hit::LoomNew));
        }
        lines.push(new_row);
        lines.push(Line::raw(""));
    }
    // A fresh Welcome is already authoritative about feature absence even
    // while the registry snapshot remains unhydrated. Do not turn that typed
    // absence into an endless loading state on the Workflows pane.
    if !catalog_available {
        lines.push(Line::styled(
            "workflow catalog needs workflow_catalog_v1; no local list is substituted",
            theme.dim_style(),
        ));
        lines.push(Line::raw(""));
        lines.push(Line::styled("tab ⇄ loom · esc back", theme.dim_style()));
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }
    // Round 3: an unhydrated live connection is LOADING, not empty — the
    // once-per-connection loom.list may still be in flight (or the socket
    // just died and the next connection re-hydrates).
    if !model.loom_loaded && !model.mode.fabricates_locally() {
        lines.push(Line::styled(
            "loading registry from the daemon…",
            theme.dim_style(),
        ));
        lines.push(Line::raw(""));
        lines.push(Line::styled("esc back", theme.dim_style()));
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }
    // Once its feature is available, neither pane can be empty: both row
    // spaces lead with the synthetic `∅ none` default, so registry emptiness
    // renders inside the REGISTERED section instead of a whole-pane state.
    let total_rows = if on_types {
        model.type_row_count()
    } else {
        model.workflow_row_count()
    };
    let selection = model.loom_selection.min(total_rows.saturating_sub(1));
    let mut selected_line: usize = 0;

    if model.loom_detail {
        if on_types && model.type_row(selection) == Some(crate::app::TypeRow::None) {
            // W-flow inline identity: the synthetic default's detail.
            lines.push(Line::from(vec![
                Span::styled("∅ ", theme.dim_style()),
                Span::styled("none", theme.bright_style().add_modifier(Modifier::BOLD)),
                Span::styled(" — plain session · default", theme.dim_style()),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "every session starts plain — no job injected, default accent",
                theme.text_style(),
            ));
        } else if on_types {
            let Some(crate::app::TypeRow::Registered(index)) = model.type_row(selection) else {
                unreachable!("clamped selection resolves a row");
            };
            let record = &model.loom_types[index];
            let accent = crate::style::loom_accent_style(&record.color)
                .unwrap_or_else(|| theme.gold_style());
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", record.glyph), accent),
                Span::styled(
                    record.name.clone(),
                    theme.bright_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  @{}", record.id), accent),
                Span::styled(
                    format!(
                        "  {} -> {}  · rev {}",
                        record.in_type, record.out_type, record.rev
                    ),
                    theme.dim_style(),
                ),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::styled("JOB", theme.gold_style()));
            let width = (area.width as usize).saturating_sub(4).max(8);
            for line in record.job.lines() {
                for chunk in wrap_plain(line, width) {
                    lines.push(Line::styled(format!("  {chunk}"), theme.text_style()));
                }
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "GRANTS — what it may touch, nothing else",
                theme.gold_style(),
            ));
            for (label, list) in [("cli", &record.clis), ("api", &record.apis)] {
                for item in list {
                    let mut spans = vec![
                        Span::styled(format!("  {label} "), theme.dim_style()),
                        Span::styled(item.clone(), accent),
                    ];
                    // W-flow: a declared CLI is a capability grant, not a
                    // promise the program EXISTS. Three states, and the
                    // third is not the second: present, missing, and NOT
                    // PROBED (an older daemon sent no map) — an unprobed
                    // name must never be drawn as missing.
                    if label == "cli" {
                        match model.loom_cli_present.get(item) {
                            Some(true) => {
                                spans.push(Span::styled("  ✓ installed", theme.faint_style()))
                            }
                            Some(false) => spans
                                .push(Span::styled("  ✗ not on this device", theme.warn_style())),
                            None => {}
                        }
                    }
                    lines.push(Line::from(spans));
                }
            }
            let missing = missing_clis(model, record);
            if !missing.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  ⌃I ", theme.gold_style()),
                    Span::styled(
                        format!(
                            "install the {} missing program{}",
                            missing.len(),
                            if missing.len() == 1 { "" } else { "s" }
                        ),
                        theme.dim_style(),
                    ),
                ]));
            }
            if record.clis.is_empty() && record.apis.is_empty() {
                lines.push(Line::styled(
                    "  none — narrower privilege",
                    theme.faint_style(),
                ));
            }
            if !record.denials.is_empty() {
                lines.push(Line::styled(
                    "DENIALS — explicitly withheld",
                    theme.gold_style(),
                ));
                for denial in &record.denials {
                    lines.push(Line::from(vec![
                        Span::styled("  deny ", theme.dim_style()),
                        Span::styled(denial.clone(), theme.warn_style()),
                    ]));
                }
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled("KNOW-HOW", theme.gold_style()));
            for (label, list) in [("skill", &record.skills), ("script", &record.scripts)] {
                for item in list {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {label} "), theme.dim_style()),
                        Span::styled(item.clone(), theme.text_style()),
                    ]));
                }
            }
            if record.skills.is_empty() && record.scripts.is_empty() {
                lines.push(Line::styled("  none yet", theme.faint_style()));
            }
        } else if let Some(crate::app::WorkflowRow::None) = model.workflow_row(selection) {
            // W-flow: the synthetic default's detail — one honest line.
            lines.push(Line::from(vec![
                Span::styled("∅ ", theme.dim_style()),
                Span::styled("none", theme.bright_style().add_modifier(Modifier::BOLD)),
                Span::styled("  — no flow · default", theme.dim_style()),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "every session starts here — no graph, no gates",
                theme.text_style(),
            ));
        } else if let Some(crate::app::WorkflowRow::BuiltIn(template)) =
            model.workflow_row(selection)
        {
            // W-flow: a built-in's detail derives from its GraphTemplateSpec
            // — node names + gates, never a fabricated pipe source.
            lines.push(Line::from(vec![
                Span::styled("⛩ ", theme.gold_style()),
                Span::styled(
                    template.name.clone(),
                    theme.bright_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  built-in · v{}", template.version),
                    theme.dim_style(),
                ),
            ]));
            lines.push(Line::raw(""));
            let showing_live = model
                .daemon_features
                .contains(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
                && model.workflow_graph.workflow_id() == Some(template.name.as_str());
            if showing_live {
                lines.extend(workflow_live_dag_lines(&model.workflow_graph, theme));
            } else {
                lines.extend(workflow_dag_lines(&template, theme));
            }
            if showing_live && let Some(error) = &model.workflow_graph_error {
                lines.push(Line::from(vec![
                    Span::styled("  live view paused · ", theme.warn_style()),
                    Span::styled(error.clone(), theme.dim_style()),
                ]));
            }
            lines.push(Line::styled("NODES", theme.gold_style()));
            for node in &template.nodes {
                let mut spans = vec![
                    Span::styled(format!("  {}", node.name.as_str()), theme.bright_style()),
                    Span::styled(
                        format!(
                            "  {} · {}",
                            crate::graph::gate_kind_label(&node.gate),
                            crate::graph::executor_label(node.executor),
                        ),
                        theme.dim_style(),
                    ),
                ];
                if !node.verify_slots.is_empty() {
                    spans.push(Span::styled(
                        format!(
                            " · slots {}",
                            node.verify_slots
                                .iter()
                                .map(|slot| slot.id.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                        theme.dim_style(),
                    ));
                }
                if !node.depends_on.is_empty() {
                    spans.push(Span::styled(
                        format!(
                            " ← after {}",
                            node.depends_on
                                .iter()
                                .map(haider_protocol::graph::GraphNodeName::as_str)
                                .collect::<Vec<_>>()
                                .join("+")
                        ),
                        theme.faint_style(),
                    ));
                }
                lines.push(Line::from(spans));
            }
        } else if let Some(workflow) = match model.workflow_row(selection) {
            Some(crate::app::WorkflowRow::Registered(index)) => model.loom_workflows.get(index),
            _ => None,
        } {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("@{}", workflow.id),
                    theme.bright_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {} -> {}  · rev {} · {} · {}",
                        workflow.in_type,
                        workflow.out_type,
                        workflow.rev,
                        workflow.pipe_version,
                        crate::graph::digest_short(&workflow.digest),
                    ),
                    theme.dim_style(),
                ),
            ]));
            lines.push(Line::raw(""));
            let showing_live = model
                .daemon_features
                .contains(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
                && model.workflow_graph.workflow_id() == Some(workflow.id.as_str());
            if showing_live {
                lines.extend(workflow_live_dag_lines(&model.workflow_graph, theme));
            } else {
                lines.extend(workflow_dag_lines(&workflow.template, theme));
            }
            if showing_live && let Some(error) = &model.workflow_graph_error {
                lines.push(Line::from(vec![
                    Span::styled("  live view paused · ", theme.warn_style()),
                    Span::styled(error.clone(), theme.dim_style()),
                ]));
            }
            lines.push(Line::styled("NODES", theme.gold_style()));
            for meta in &workflow.meta {
                let mut spans = vec![Span::styled(
                    format!("  {}", meta.source_name),
                    theme.bright_style(),
                )];
                if let Some(type_id) = &meta.agent_type {
                    let accent = model
                        .loom_type(type_id)
                        .and_then(|record| crate::style::loom_accent_style(&record.color))
                        .unwrap_or_else(|| theme.gold_style());
                    spans.push(Span::styled(format!(" @{type_id}"), accent));
                }
                if !meta.task.is_empty() {
                    spans.push(Span::styled(
                        format!(" \"{}\"", meta.task),
                        theme.dim_style(),
                    ));
                }
                if let Some(back) = &meta.back {
                    spans.push(Span::styled(
                        format!(" ↺{}", back.as_str().to_ascii_lowercase()),
                        theme.warn_style(),
                    ));
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled("PIPE SOURCE", theme.gold_style()));
            let width = (area.width as usize).saturating_sub(4).max(8);
            for line in workflow.source.lines() {
                for chunk in wrap_plain(line, width) {
                    lines.push(Line::styled(format!("  {chunk}"), theme.text_style()));
                }
            }
        }
        let selected_workflow_id = match model.workflow_row(selection) {
            Some(crate::app::WorkflowRow::BuiltIn(template)) => Some(template.name),
            Some(crate::app::WorkflowRow::Registered(index)) => model
                .loom_workflows
                .get(index)
                .map(|workflow| workflow.id.clone()),
            Some(crate::app::WorkflowRow::None) | None => None,
        };
        let showing_selected_live = !on_types
            && model
                .daemon_features
                .contains(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
            && model.workflow_graph.workflow_id() == selected_workflow_id.as_deref();
        let current_inspection = model
            .workflow_evidence_inspection
            .as_ref()
            .filter(|inspection| {
                model
                    .workflow_graph
                    .rejection(&inspection.node_id)
                    .is_some_and(|rejection| rejection.cursor == inspection.cursor)
            });
        if showing_selected_live && let Some(inspection) = current_inspection {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "REJECT EVIDENCE — OPEN",
                    theme.maroon_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  · {}", inspection.node_id), theme.dim_style()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  reason ", theme.dim_style()),
                Span::styled(inspection.code.clone(), theme.maroon_style()),
                Span::styled(
                    format!(" · journal cursor {}", inspection.cursor),
                    theme.faint_style(),
                ),
            ]));
            let width = (area.width as usize).saturating_sub(4).max(8);
            for chunk in wrap_plain(&inspection.message, width) {
                lines.push(Line::styled(format!("  {chunk}"), theme.text_style()));
            }
            if let Some(reference) = &inspection.reference {
                lines.push(Line::styled("  evidence artifact", theme.dim_style()));
                for chunk in wrap_plain(reference, width) {
                    lines.push(Line::styled(format!("  {chunk}"), theme.text_style()));
                }
                lines.push(Line::styled(
                    "  evidence bytes are not exposed by workflow.graph RPC",
                    theme.faint_style(),
                ));
            } else {
                lines.push(Line::styled(
                    "  no evidence artifact published · reason remains inspectable",
                    theme.faint_style(),
                ));
            }
        }
        let rejected_evidence_available = showing_selected_live
            && model
                .workflow_graph
                .nodes()
                .any(|node| model.workflow_graph.rejection(&node.node_id).is_some());
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if showing_selected_live && current_inspection.is_some() {
                "esc close reject evidence"
            } else if rejected_evidence_available {
                "⏎ inspect / next reject · esc back to the list"
            } else {
                "esc back to the list"
            },
            theme.dim_style(),
        ));
    } else if on_types {
        lines.push(Line::styled("AGENT TYPES", theme.gold_style()));
        // W-flow inline identity: the fixed head mirrors the workflows pane
        // — the synthetic `∅ none` default is ALWAYS first (not a registry
        // record, which is exactly what makes it undeletable), then the
        // registered types.
        if selection == 0 {
            selected_line = lines.len();
        }
        lines.push(Line::from(vec![
            Span::styled(if selection == 0 { "❯ " } else { "  " }, theme.gold_style()),
            Span::styled("∅ ", theme.dim_style()),
            Span::styled("none", theme.bright_style().add_modifier(Modifier::BOLD)),
            Span::styled(" — plain session · default", theme.dim_style()),
        ]));
        if model.loom_types.is_empty() {
            lines.push(Line::styled(
                "  none registered — press n: the model proposes one; a plan you accept registers",
                theme.dim_style(),
            ));
        }
        for (offset, record) in model.loom_types.iter().enumerate() {
            let index = 1 + offset;
            let accent = crate::style::loom_accent_style(&record.color)
                .unwrap_or_else(|| theme.gold_style());
            let cursor = if index == selection { "❯ " } else { "  " };
            if index == selection {
                selected_line = lines.len();
            }
            let mut spans = vec![
                Span::styled(cursor.to_owned(), theme.gold_style()),
                Span::styled(format!("{} ", record.glyph), accent),
                Span::styled(
                    format!("@{}", record.id),
                    accent.add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {} -> {}  · {} block{}",
                        record.in_type,
                        record.out_type,
                        record.clis.len()
                            + record.apis.len()
                            + record.skills.len()
                            + record.scripts.len(),
                        if record.clis.len()
                            + record.apis.len()
                            + record.skills.len()
                            + record.scripts.len()
                            == 1
                        {
                            ""
                        } else {
                            "s"
                        },
                    ),
                    theme.dim_style(),
                ),
            ];
            // W-flow: a type whose declared programs are absent will fail at
            // its first turn. Say so on the ROW — the detail is one keypress
            // away, but a gap you have to open a pane to discover is a gap
            // you bind over.
            let missing = missing_clis(model, record);
            if !missing.is_empty() {
                spans.push(Span::styled(
                    format!("  ✗ {} missing", missing.len()),
                    theme.maroon_style(),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "↑↓ select · ⏎ detail · ⌃P bind · ⌃N new · ⌃A archive · ⌃I install · tab ⇄ workflows · esc back",
            theme.dim_style(),
        ));
    } else {
        lines.push(Line::styled(
            "WORKFLOWS — run with @name <brief>",
            theme.gold_style(),
        ));
        // W-flow fixed head: the synthetic `∅ none` row is ALWAYS first —
        // not a registry record, which is exactly what makes it undeletable
        // — then every main-eligible built-in published by the daemon, then
        // the REGISTERED section.
        if selection == 0 {
            selected_line = lines.len();
        }
        lines.push(Line::from(vec![
            Span::styled(if selection == 0 { "❯ " } else { "  " }, theme.gold_style()),
            Span::styled("∅ ", theme.dim_style()),
            Span::styled("none", theme.bright_style().add_modifier(Modifier::BOLD)),
            Span::styled(" — no flow · default", theme.dim_style()),
        ]));
        let builtins = model.builtin_workflow_templates();
        for (offset, template) in builtins.iter().enumerate() {
            let index = 1 + offset;
            if index == selection {
                selected_line = lines.len();
            }
            let mut spans = vec![
                Span::styled(
                    if index == selection { "❯ " } else { "  " },
                    theme.gold_style(),
                ),
                Span::styled("⛩ ", theme.gold_style()),
                Span::styled(
                    template.name.clone(),
                    theme.bright_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  built-in · {} node{}",
                        template.nodes.len(),
                        if template.nodes.len() == 1 { "" } else { "s" },
                    ),
                    theme.dim_style(),
                ),
            ];
            if model
                .daemon_features
                .contains(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
                && model.workflow_graph.workflow_id() == Some(template.name.as_str())
            {
                spans.push(Span::styled("  ● LIVE", theme.gold_style()));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("REGISTERED", theme.gold_style()));
        if model.loom_workflows.is_empty() {
            lines.push(Line::styled(
                if model.daemon_serves(haider_rpc::FEATURE_LOOM_PIPE_DAG_V1) {
                    "  none registered — press n: the model proposes a pipe DAG; a plan you accept registers"
                } else {
                    "  none registered — press n: the model proposes a sequential workflow; a plan you accept registers"
                },
                theme.dim_style(),
            ));
        }
        for (offset, workflow) in model.loom_workflows.iter().enumerate() {
            let index = 1 + builtins.len() + offset;
            if index == selection {
                selected_line = lines.len();
            }
            let cursor = if index == selection { "❯ " } else { "  " };
            let mut spans = vec![
                Span::styled(cursor.to_owned(), theme.gold_style()),
                Span::styled(
                    format!("@{}", workflow.id),
                    theme.bright_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {} -> {} · ", workflow.in_type, workflow.out_type),
                    theme.dim_style(),
                ),
            ];
            for meta in &workflow.meta {
                if let Some(type_id) = &meta.agent_type
                    && let Some(record) = model.loom_type(type_id)
                {
                    let accent = crate::style::loom_accent_style(&record.color)
                        .unwrap_or_else(|| theme.gold_style());
                    spans.push(Span::styled(
                        if record.glyph.is_empty() {
                            "•".to_owned()
                        } else {
                            record.glyph.clone()
                        },
                        accent,
                    ));
                }
            }
            spans.push(Span::styled(
                format!(" · rev {}", workflow.rev),
                theme.faint_style(),
            ));
            if model
                .daemon_features
                .contains(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
                && model.workflow_graph.workflow_id() == Some(workflow.id.as_str())
            {
                spans.push(Span::styled("  ● LIVE", theme.gold_style()));
                if model.workflow_graph.workflow_digest() != Some(workflow.digest.as_str()) {
                    spans.push(Span::styled(" · frozen revision", theme.warn_style()));
                }
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "↑↓ select · ⏎ detail · ⌃P pin · ⌃N new · ⌃A archive · tab ⇄ loom · esc back",
            theme.dim_style(),
        ));
    }
    // Round 3: both views SCROLL — a large registry or a long detail pane
    // must stay reachable. Detail rides loom_scroll against a published
    // ceiling (the plan-surface pattern); the list follows its selection.
    let total_lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_scroll = total_lines.saturating_sub(area.height);
    let scroll = if model.loom_detail {
        model.loom_scroll_max.set(max_scroll);
        model.loom_scroll.min(max_scroll)
    } else {
        let selected = u16::try_from(selected_line).unwrap_or(u16::MAX);
        selected
            .saturating_sub(area.height.saturating_sub(3))
            .min(max_scroll)
    };
    frame.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), area);
}

fn render_graph(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    _hits: &mut [(Rect, Hit)],
) {
    use haider_protocol::graph::GraphPhase;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(model.display_name().to_owned(), theme.dim_style()),
        Span::styled(" › ", theme.faint_style()),
        Span::styled("graph", theme.bright_style().add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::raw(""));

    let Some(status) = model.graph.as_ref() else {
        if model.graph_unsupported {
            lines.push(Line::styled(
                "graph needs a newer daemon (convergence_graph_v1)",
                theme.err_style(),
            ));
        } else {
            lines.push(Line::styled(
                "no graph — /graph pin to start the ship loop",
                theme.dim_style(),
            ));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("esc back to session", theme.dim_style()));
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    };

    // Header: template · digest · epoch (+ the Loom typed signature).
    let loom = model.loom_workflow_meta(&status.template, &status.digest);
    let mut header = vec![
        Span::styled(status.template.clone(), theme.bright_style()),
        Span::styled(
            format!(" · {}", crate::graph::digest_short(&status.digest)),
            theme.faint_style(),
        ),
        Span::styled(format!(" · epoch {}", status.attempt), theme.dim_style()),
    ];
    if let Some(workflow) = loom {
        header.push(Span::styled(
            format!(
                " · {} -> {} · {}",
                workflow.in_type, workflow.out_type, workflow.pipe_version
            ),
            theme.gold_style(),
        ));
    }
    lines.push(Line::from(header));
    lines.push(Line::raw(""));

    // One row per node: glyph · gate · attempt · evidence tally.
    for node in &status.nodes {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(graph_node_spans(theme, status, node));
        spans.push(Span::styled(
            format!(" · {}", crate::graph::gate_label(node)),
            theme.faint_style(),
        ));
        spans.push(Span::styled(
            format!(" · attempt {}/8", node.current_attempt.unwrap_or(0)),
            theme.dim_style(),
        ));
        // D2: a Loom node names its specialist and task, in the type accent.
        if let Some(meta) =
            loom.and_then(|workflow| workflow.meta.iter().find(|meta| meta.node == node.node))
        {
            if let Some(type_id) = &meta.agent_type {
                let accent = model
                    .loom_type(type_id)
                    .and_then(|record| crate::style::loom_accent_style(&record.color))
                    .unwrap_or_else(|| theme.gold_style());
                let glyph_text = model
                    .loom_type(type_id)
                    .map(|record| record.glyph.clone())
                    .filter(|glyph| !glyph.is_empty())
                    .map(|glyph| format!("{glyph} "))
                    .unwrap_or_default();
                spans.push(Span::styled(
                    format!(" · {glyph_text}@{type_id}"),
                    accent.add_modifier(Modifier::BOLD),
                ));
            }
            if !meta.task.is_empty() {
                spans.push(Span::styled(
                    format!(" \"{}\"", meta.task),
                    theme.dim_style(),
                ));
            }
        }
        if crate::graph::is_human_gate(node) {
            // Human gate — no evidence tally.
        } else if node.evidence_slots.is_empty() {
            spans.push(Span::styled(
                format!(" · {}g", node.evidence.green),
                theme.ok_style(),
            ));
            spans.push(Span::styled(
                format!("/{}r", node.evidence.red),
                if node.evidence.red > 0 {
                    theme.err_style()
                } else {
                    theme.faint_style()
                },
            ));
            spans.push(Span::styled(
                format!(" ({} eff)", node.evidence.effective_green),
                theme.faint_style(),
            ));
        } else {
            // M2a: slotted gate — the distinct green frontier over declared slots.
            spans.push(Span::styled(
                format!(
                    " · {}/{} slots",
                    node.evidence.effective_green,
                    node.evidence_slots.len()
                ),
                theme.faint_style(),
            ));
        }
        lines.push(Line::from(spans));
        // M2a: one styled provenance row per declared slot.
        for slot in &node.evidence_slots {
            lines.push(graph_slot_line(theme, slot));
        }
    }
    lines.push(Line::raw(""));

    // M2d: the per-todo run-set (K child graphs) rides the fetched GraphStatus.
    if let Some(run_set) = &status.run_set {
        lines.push(Line::from(vec![Span::styled(
            format!(
                "run-set {}/{} todos",
                run_set.terminal_children, run_set.required_children
            ),
            if run_set.is_complete() {
                theme.ok_style()
            } else {
                theme.gold_style()
            },
        )]));
        for child in &run_set.children {
            use haider_protocol::graph::GraphPhase as ChildPhase;
            let (glyph, stage) = crate::graph::child_glyph_stage(child);
            let glyph_style = match child.phase {
                ChildPhase::Completed => theme.ok_style(),
                ChildPhase::Active => theme.gold_style(),
                ChildPhase::Blocked => theme.err_style(),
                ChildPhase::Abandoned | ChildPhase::Superseded => theme.faint_style(),
            };
            let dep = child
                .depends_on_todo_id
                .map_or_else(String::new, |id| format!(" → after todo {id}"));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), glyph_style),
                Span::styled(format!("todo {}", child.todo_id), theme.dim_style()),
                Span::styled(format!(" · {stage}{dep}"), theme.faint_style()),
            ]));
        }
        lines.push(Line::raw(""));
    }

    // Footer: the current expectation, or the terminal/blocked line.
    match status.phase {
        GraphPhase::Completed => lines.push(Line::styled(
            "✓ complete — every gate satisfied",
            theme.ok_style(),
        )),
        GraphPhase::Abandoned => {
            lines.push(Line::styled("✗ abandoned", theme.err_style()));
        }
        GraphPhase::Blocked => {
            let reason = status
                .blocked_reason
                .map_or("held", crate::graph::block_reason_label);
            lines.push(Line::styled(
                format!("✗ blocked — {reason}"),
                theme.err_style(),
            ));
            lines.push(Line::styled(
                "/graph abandon then re-pin to retry",
                theme.dim_style(),
            ));
        }
        GraphPhase::Superseded => {
            lines.push(Line::styled(
                "⊘ superseded — replaced by a newer workflow",
                theme.faint_style(),
            ));
        }
        GraphPhase::Active => {
            if let Some(current) = status.current_node.as_ref() {
                let expectation = status
                    .nodes
                    .iter()
                    .find(|status_node| &status_node.node == current)
                    .map_or_else(|| format!("advance {current}"), crate::graph::expectation);
                lines.push(Line::from(vec![
                    Span::styled("→ current: ", theme.faint_style()),
                    Span::styled(current.label().to_owned(), theme.gold_style()),
                    Span::styled(format!(" · {expectation}"), theme.dim_style()),
                ]));
            }
        }
    }
    // M2c(#4) inspect telemetry — template rollups, tool-selection stats
    // (#5), and evidence provenance with real workspace revisions (#1). Rides
    // the one-shot `graph.inspect` read fetched when this screen opened.
    if let Some(snapshot) = &model.graph_inspect {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("── telemetry · through seq {} ──", snapshot.through_seq),
            theme.bright_style(),
        ));
        if snapshot.template_rollups.is_empty() {
            lines.push(Line::styled("  no completed runs yet", theme.faint_style()));
        }
        for rollup in snapshot.template_rollups.iter().take(5) {
            let comp = f64::from(rollup.completion_rate_basis_points) / 100.0;
            let aband = f64::from(rollup.abandon_rate_basis_points) / 100.0;
            let per_node = if rollup.declared_nodes > 0 {
                rollup.node_attempts as f64 / rollup.declared_nodes as f64
            } else {
                0.0
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(rollup.template.clone(), theme.gold_style()),
                Span::styled(
                    format!(
                        " · {}/{} done · {comp:.0}%✓ {aband:.0}%✗ · {per_node:.1} att/node · {}ms crit",
                        rollup.completed, rollup.runs, rollup.critical_path_elapsed_ms
                    ),
                    theme.faint_style(),
                ),
            ]));
        }
        if !snapshot.tool_selection.is_empty() {
            lines.push(Line::styled("  tools:", theme.dim_style()));
            for tool in snapshot.tool_selection.iter().take(8) {
                let err = f64::from(tool.error_rate_basis_points) / 100.0;
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("{:<16}", tool.tool_name), theme.dim_style()),
                    Span::styled(format!("{} calls", tool.total_calls), theme.faint_style()),
                    Span::styled(
                        format!(" · {err:.0}% err"),
                        if tool.error_rate_basis_points > 0 {
                            theme.warn_style()
                        } else {
                            theme.faint_style()
                        },
                    ),
                    Span::styled(
                        format!(" · {} redundant", tool.redundant_call_count),
                        if tool.redundant_call_count > 0 {
                            theme.warn_style()
                        } else {
                            theme.faint_style()
                        },
                    ),
                ]));
            }
        }
        let recent: Vec<_> = snapshot.evidence.iter().rev().take(3).collect();
        if !recent.is_empty() {
            lines.push(Line::styled("  recent evidence:", theme.dim_style()));
            for row in recent.into_iter().rev() {
                use haider_protocol::graph::EvidenceVerdict;
                let (glyph, gstyle) = match row.verdict {
                    EvidenceVerdict::Green => ("✓", theme.ok_style()),
                    EvidenceVerdict::Red => ("✗", theme.err_style()),
                };
                let slot = row
                    .slot
                    .as_deref()
                    .map_or_else(String::new, |s| format!(" {s}"));
                let rev = row
                    .workspace_mutation
                    .as_ref()
                    .map_or_else(String::new, |m| {
                        format!(
                            " · rev {}",
                            crate::graph::provenance_short(m.workspace_revision.as_str())
                        )
                    });
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("{glyph} "), gstyle),
                    Span::styled(format!("{}{slot}", row.node.label()), theme.dim_style()),
                    Span::styled(rev, theme.faint_style()),
                ]));
            }
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "/graph pin · /graph abandon · esc back to session",
        theme.dim_style(),
    ));
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_fleet(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    use crate::fleet;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let view = &model.fleet;
    let width = area.width as usize;
    let mut lines: Vec<Line<'_>> = Vec::new();
    // (line offset, x offset, width, hit) — resolved to rects after layout.
    let mut cell_hits: Vec<(usize, u16, u16, Hit)> = Vec::new();

    // -- crumb path (mockup .fcrumbs): session › fleet [› callsign…] ----
    let mut crumbs = vec![
        Span::styled(model.display_name().to_owned(), theme.dim_style()),
        Span::styled(" › ", theme.faint_style()),
    ];
    let snapshot = view.snapshot.as_ref();
    let resolved = snapshot.map(|snapshot| fleet::resolve(snapshot, &view.stack));
    let path_len = resolved.as_ref().map_or(0, |(_, path)| path.len());
    if path_len == 0 {
        crumbs.push(Span::styled(
            "fleet",
            theme.bright_style().add_modifier(Modifier::BOLD),
        ));
    } else {
        crumbs.push(Span::styled("fleet", theme.dim_style()));
    }
    if let Some((_, path)) = &resolved {
        for (index, node) in path.iter().enumerate() {
            crumbs.push(Span::styled(" › ", theme.faint_style()));
            crumbs.push(Span::styled(
                fleet::callsign(node).to_owned(),
                if index + 1 == path.len() {
                    theme.bright_style().add_modifier(Modifier::BOLD)
                } else {
                    theme.dim_style()
                },
            ));
        }
    }

    let Some(snapshot) = snapshot else {
        // No snapshot yet: the honest fetching / failed / empty line —
        // never a fabricated tree.
        lines.push(Line::from(crumbs));
        lines.push(Line::raw(""));
        if let Some(error) = &view.error {
            lines.push(Line::styled(
                format!("✗ fleet read failed — {error}"),
                theme.err_style(),
            ));
        } else if view.fetching {
            lines.push(Line::styled("fetching fleet…", theme.dim_style()));
        } else {
            lines.push(Line::styled(
                "no fleet — this session has no subagents",
                theme.dim_style(),
            ));
        }
        lines.push(Line::styled("esc back to session", theme.dim_style()));
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    };
    let (level, _path) = resolved.unwrap_or((&[], Vec::new()));

    // Owner 2026-08-16: the MEMBER DETAIL frame — a leaf's own page:
    // identity + metrics, the member's OWN workflow (its dynamically-made
    // child graph, honestly empty when it ran ad-hoc), and the transcript
    // door (⏎ opens the chip view for the active session's own chips).
    if let Some(detail) = &view.detail {
        // (line offset, hit) — resolved to rects just before this frame's
        // early return, which is why they cannot ride `cell_hits`.
        let mut detail_hits: Vec<(usize, Hit)> = Vec::new();
        let node = fleet::flatten(level)
            .into_iter()
            .find(|row| &row.node.agent_id == detail)
            .map(|row| row.node);
        crumbs.push(Span::styled(" › ", theme.faint_style()));
        crumbs.push(Span::styled(
            node.map(fleet::callsign).unwrap_or("member").to_owned(),
            theme.bright_style().add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(crumbs));
        lines.push(Line::raw(""));
        match node {
            None => lines.push(Line::styled(
                "member left the fleet — esc back",
                theme.dim_style(),
            )),
            Some(node) => {
                let glyph = fleet::state_glyph(node.state);
                let mut header = vec![
                    Span::styled(
                        format!("{glyph} "),
                        fleet_glyph_style(theme, node.state, model.anim_phase),
                    ),
                    Span::styled(
                        fleet::callsign(node).to_owned(),
                        theme.bright_style().add_modifier(Modifier::BOLD),
                    ),
                ];
                // W-flow: the detail header speaks the same typed accent as
                // the row it was drilled from.
                match model.loom_task_type(&node.task) {
                    Some((record, remainder)) => {
                        let accent = crate::style::loom_accent_style(&record.color)
                            .unwrap_or_else(|| theme.gold_style());
                        header.push(Span::styled(" · ".to_owned(), theme.text_style()));
                        if !record.glyph.is_empty() {
                            header.push(Span::styled(format!("{} ", record.glyph), accent));
                        }
                        header.push(Span::styled(
                            format!("@{}", record.id),
                            accent.add_modifier(Modifier::BOLD),
                        ));
                        header.push(Span::styled(format!(" · {remainder}"), theme.text_style()));
                    }
                    None => {
                        header.push(Span::styled(
                            format!(" · {}", node.task),
                            theme.text_style(),
                        ));
                    }
                }
                lines.push(Line::from(header));
                // Identity under the name, same tail grammar as the row it
                // was drilled from. Absent facts render nothing.
                if let Some(identity) = fleet::node_identity(node, width.saturating_sub(2)) {
                    lines.push(Line::styled(format!("  {identity}"), theme.dim_style()));
                }
                let metric = fleet::node_metric(node);
                if !metric.is_empty() {
                    lines.push(Line::styled(format!("  {metric}"), theme.dim_style()));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("workflow", theme.gold_style()));
                match &view.detail_graph {
                    None => lines.push(Line::styled(
                        "  reading the member's graph…",
                        theme.dim_style(),
                    )),
                    Some((_, None)) => lines.push(Line::styled(
                        "  no personal workflow — this member ran ad-hoc",
                        theme.dim_style(),
                    )),
                    Some((_, Some(status))) => lines.push(Line::from(vec![
                        Span::styled(format!("  {} ", status.template), theme.bright_style()),
                        Span::styled(format!("· {:?}", status.phase), theme.dim_style()),
                        Span::styled(
                            status
                                .current_node
                                .as_ref()
                                .map(|at| format!(" · at {}", at.as_str()))
                                .unwrap_or_default(),
                            theme.gold_style(),
                        ),
                    ])),
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("transcript", theme.gold_style()));
                if crate::app::find_chip(&model.chips, node.agent_id.as_str()).is_some() {
                    // The detail frame used to emit NO hit rects at all
                    // (it returns before the shared resolver below), so
                    // this door was keyboard-only. It is now the mouse's
                    // too — the same door, not a second one.
                    detail_hits.push((
                        lines.len(),
                        Hit::FleetTranscript(node.agent_id.as_str().to_owned()),
                    ));
                    lines.push(Line::styled(
                        "  ⏎ open the full transcript (chip view)",
                        theme.dim_style(),
                    ));
                } else {
                    lines.push(Line::styled(
                        "  lives on the member's own session — attach to view",
                        theme.dim_style(),
                    ));
                }
                // The DESTROY affordance. Deliberate by construction: it is
                // a distinct row that must be ARMED before it acts, and the
                // arm is value-carrying (it names the agent), so a refreshed
                // snapshot under the arm can never retarget the kill.
                lines.push(Line::raw(""));
                if model.fleet.kill_armed.as_ref() == Some(&node.agent_id) {
                    lines.push(Line::styled(
                        format!(
                            "  destroy {}? press d again to confirm · esc cancels",
                            fleet::callsign(node)
                        ),
                        theme.err_style(),
                    ));
                } else {
                    detail_hits.push((
                        lines.len(),
                        Hit::FleetKill(node.agent_id.as_str().to_owned()),
                    ));
                    lines.push(Line::styled(
                        "  ✕ destroy this subagent (d)",
                        theme.dim_style(),
                    ));
                }
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("esc back", theme.dim_style()));
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        for (offset, hit) in detail_hits {
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
        return;
    }
    let roll = fleet::rollup(level);
    let density = fleet::density(roll.total);

    // -- rollup header (frozen grammar): colored counts, right-aligned
    //    beside the crumbs when they fit on one row, else on its own row.
    let header_text = fleet::header_line(&roll);
    let rollup_spans = |theme: &Theme| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("fleet of {}", roll.total), theme.bright_style()),
            Span::styled(" · ", theme.faint_style()),
            Span::styled(format!("✓{}", roll.done), theme.ok_style()),
            Span::styled(format!(" ◉{}", roll.live), theme.gold_style()),
            Span::styled(format!(" ✗{}", roll.failed), theme.err_style()),
            Span::styled(format!(" ◌{}", roll.queued), theme.faint_style()),
            Span::styled(
                {
                    let mut extras = String::new();
                    if roll.waiting > 0 {
                        extras.push_str(&format!(" · ◔{}", roll.waiting));
                    }
                    if roll.cancelled > 0 {
                        extras.push_str(&format!(" · ⊘{}", roll.cancelled));
                    }
                    if roll.unknown > 0 {
                        extras.push_str(&format!(" · ?{}", roll.unknown));
                    }
                    extras
                },
                theme.dim_style(),
            ),
            Span::styled(format!(" · depth {}", roll.max_depth), theme.dim_style()),
        ]
    };
    let crumb_width = Line::from(crumbs.clone()).width();
    if crumb_width + header_text.chars().count() + 3 <= width {
        let pad = width
            .saturating_sub(crumb_width)
            .saturating_sub(header_text.chars().count());
        let mut spans = crumbs;
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(rollup_spans(theme));
        lines.push(Line::from(spans));
    } else {
        lines.push(Line::from(crumbs));
        lines.push(Line::from(rollup_spans(theme)));
    }
    lines.push(Line::raw(""));
    let header_rows = lines.len();

    // -- footers: an error over a stale snapshot, the truncation witness,
    //    and the key hints — all honest, all budgeted before the body.
    let mut footer: Vec<Line<'_>> = Vec::new();
    if let Some(error) = &view.error {
        footer.push(Line::styled(
            format!("✗ fleet refresh failed — showing the last snapshot · {error}"),
            theme.err_style(),
        ));
    }
    if let Some(witness) = fleet::truncation_footer(snapshot) {
        footer.push(Line::styled(witness, theme.warn_style()));
    }
    let esc_label = if view.stack.is_empty() {
        "esc back to session"
    } else {
        "esc up one level"
    };
    footer.push(Line::styled(
        match density {
            fleet::Density::List => format!("↑↓ move · ⏎ drill into a subtree · {esc_label}"),
            fleet::Density::Grid => format!("arrows move · ⏎ drill in · {esc_label}"),
        },
        theme.dim_style(),
    ));

    let body_rows = (area.height as usize)
        .saturating_sub(header_rows)
        .saturating_sub(footer.len());

    match density {
        fleet::Density::List => {
            let rows = fleet::flatten(level);
            view.page_rows.set(body_rows.max(1));
            view.grid_cols.set(1);
            let sel = view.sel.min(rows.len().saturating_sub(1));
            // Selection-follow scroll (top-follow: the selected row stays
            // visible; slice 1 has no free scroll offset).
            let scroll = if body_rows == 0 {
                0
            } else {
                sel.saturating_sub(body_rows.saturating_sub(1))
            };
            for (index, row) in rows.iter().enumerate().skip(scroll).take(body_rows) {
                let node = row.node;
                let selected = index == sel;
                let queued = node.state == haider_rpc::FleetAgentStateWire::Queued;
                let glyph = fleet::state_glyph(node.state);
                let indent = " │ ".repeat(row.rel_depth);
                let callsign = fleet::callsign(node);
                let name_style = if queued {
                    theme.faint_style()
                } else if selected {
                    theme.bright_style().add_modifier(Modifier::BOLD)
                } else {
                    theme.bright_style()
                };
                let mut spans = vec![
                    Span::styled(format!(" {indent}"), theme.faint_style()),
                    Span::styled(
                        glyph.to_owned(),
                        fleet_glyph_style(theme, node.state, model.anim_phase),
                    ),
                    Span::styled(format!(" {callsign}"), name_style),
                ];
                if let Some(marker) = fleet::child_marker(node) {
                    spans.push(Span::styled(format!(" {marker}"), theme.faint_style()));
                }
                let metric = fleet::node_metric(node);
                // The row's IDENTITY tail (`· model · provider`) sits
                // between the NAME and the task: it ADDS identity, it never
                // restates the task the ` — ` fragment already carries.
                // Budgeted after the metric and before the task, and it
                // yields WHOLE to the task's own floor (` — ` plus the
                // 4-char fragment the branch below requires) — a long model
                // name can never starve the task off the row. A node with
                // neither fact renders nothing at all, so a fleet that
                // knows no identity draws exactly today's row.
                let task_reserve = if node.task.is_empty() { 0 } else { 3 + 4 };
                let identity_budget = width
                    .saturating_sub(Line::from(spans.clone()).width())
                    .saturating_sub(metric.chars().count())
                    .saturating_sub(5)
                    .saturating_sub(task_reserve)
                    .saturating_sub(3);
                if let Some(identity) = fleet::node_identity(node, identity_budget) {
                    spans.push(Span::styled(format!(" · {identity}"), theme.dim_style()));
                }
                let left = Line::from(spans.clone()).width();
                // The task fragment fills what the right-aligned metric
                // leaves; it truncates before the metric ever drops.
                let task_budget = width
                    .saturating_sub(left)
                    .saturating_sub(metric.chars().count())
                    .saturating_sub(5);
                // W-flow: a typed member's daemon-stamped `@type · ` task
                // prefix paints glyph+@type in its Loom accent — the SAME
                // trust gate as the subtree chips (`loom_task_type`). The
                // state glyph keeps meaning state; only this segment wears
                // the accent, and an unparseable (or budget-starved) task
                // falls back to today's plain dim fragment.
                let typed = model.loom_task_type(&node.task).filter(|(record, _)| {
                    let prefix = if record.glyph.is_empty() {
                        0
                    } else {
                        record.glyph.chars().count() + 1
                    } + 1
                        + record.id.chars().count();
                    task_budget >= prefix
                });
                if let Some((record, remainder)) = typed {
                    let accent = crate::style::loom_accent_style(&record.color)
                        .unwrap_or_else(|| theme.gold_style());
                    spans.push(Span::styled(" — ".to_owned(), theme.dim_style()));
                    let mut prefix = 1 + record.id.chars().count();
                    if !record.glyph.is_empty() {
                        prefix += record.glyph.chars().count() + 1;
                        spans.push(Span::styled(format!("{} ", record.glyph), accent));
                    }
                    spans.push(Span::styled(
                        format!("@{}", record.id),
                        accent.add_modifier(Modifier::BOLD),
                    ));
                    let rest_budget = task_budget.saturating_sub(prefix + 3);
                    if !remainder.is_empty() && rest_budget >= 4 {
                        let rest: String = if remainder.chars().count() > rest_budget {
                            let mut cut: String = remainder
                                .chars()
                                .take(rest_budget.saturating_sub(1))
                                .collect();
                            cut.push('…');
                            cut
                        } else {
                            remainder.to_owned()
                        };
                        spans.push(Span::styled(format!(" · {rest}"), theme.dim_style()));
                    }
                } else if !node.task.is_empty() && task_budget >= 4 {
                    let task: String = if node.task.chars().count() > task_budget {
                        let mut cut: String = node
                            .task
                            .chars()
                            .take(task_budget.saturating_sub(1))
                            .collect();
                        cut.push('…');
                        cut
                    } else {
                        node.task.clone()
                    };
                    spans.push(Span::styled(format!(" — {task}"), theme.dim_style()));
                }
                let left = Line::from(spans.clone()).width();
                if !metric.is_empty() {
                    let budget = width.saturating_sub(left).saturating_sub(2);
                    if metric.chars().count() <= budget {
                        let pad = width
                            .saturating_sub(left)
                            .saturating_sub(metric.chars().count());
                        spans.push(Span::raw(" ".repeat(pad)));
                        spans.push(Span::styled(metric, theme.dim_style()));
                    }
                }
                let hovered = model.hovered.as_ref()
                    == Some(&Hit::FleetNode(node.agent_id.as_str().to_owned()));
                let line = hover_band(Line::from(spans), hovered, area.width, theme);
                cell_hits.push((
                    lines.len(),
                    0,
                    area.width,
                    Hit::FleetNode(node.agent_id.as_str().to_owned()),
                ));
                lines.push(line);
            }
        }
        fleet::Density::Grid => {
            let cells = level;
            let step = FLEET_CELL_W + FLEET_CELL_GAP;
            let cols = ((width.saturating_sub(1)) / step).max(1);
            view.grid_cols.set(cols);
            // One band = matrix (2 rows) + callsign + a spacer row.
            let band_rows = 4;
            let visible_bands = (body_rows / band_rows).max(1);
            view.page_rows.set(visible_bands);
            let sel = view.sel.min(cells.len().saturating_sub(1));
            let sel_band = sel / cols;
            let first_band = sel_band.saturating_sub(visible_bands.saturating_sub(1));
            let bands = cells
                .chunks(cols)
                .enumerate()
                .skip(first_band)
                .take(visible_bands);
            for (band_index, band) in bands {
                let mut matrix_a: Vec<Span<'_>> = vec![Span::raw(" ")];
                let mut matrix_b: Vec<Span<'_>> = vec![Span::raw(" ")];
                let mut names: Vec<Span<'_>> = vec![Span::raw(" ")];
                // The band's 4th row. It was a blank spacer; it now carries
                // each cell's identity. The band height is UNCHANGED, so
                // the max-density view still shows exactly as many cells as
                // before — the identity costs no cells. When no cell in the
                // band knows a model or a provider every entry falls back to
                // the blank it replaced, and the band draws as it always did.
                let mut idents: Vec<Span<'_>> = vec![Span::raw(" ")];
                for (col, node) in band.iter().enumerate() {
                    let index = band_index * cols + col;
                    let selected = index == sel;
                    let bits = fleet::matrix_bits(node.agent_id.as_str());
                    let [row_a, row_b] = fleet::matrix_rows(bits);
                    let tint = fleet_glyph_style(theme, node.state, model.anim_phase);
                    let dot_span = |row: &str| -> Vec<Span<'static>> {
                        row.chars()
                            .map(|dot| {
                                Span::styled(
                                    dot.to_string(),
                                    if dot == '●' {
                                        tint
                                    } else {
                                        theme.faint_style()
                                    },
                                )
                            })
                            .collect()
                    };
                    let pad_l = (FLEET_CELL_W - 4) / 2;
                    let pad_r = FLEET_CELL_W - 4 - pad_l;
                    for (target, row) in [(&mut matrix_a, &row_a), (&mut matrix_b, &row_b)] {
                        target.push(Span::raw(" ".repeat(pad_l)));
                        target.extend(dot_span(row));
                        target.push(Span::raw(" ".repeat(pad_r)));
                        target.push(Span::raw(" ".repeat(FLEET_CELL_GAP)));
                    }
                    // Callsign under the cell — the selected cell wears the
                    // selection band (ground shifts, ink stays legible).
                    let marker = fleet::child_marker(node);
                    let marker_width = marker
                        .as_ref()
                        .map_or(0, |marker| marker.chars().count() + 1);
                    let callsign_budget = FLEET_CELL_W.saturating_sub(marker_width);
                    let mut name: String = fleet::callsign(node)
                        .chars()
                        .take(callsign_budget)
                        .collect();
                    let name_width = name.chars().count();
                    name.push_str(&" ".repeat(callsign_budget - name_width));
                    // W-flow: a typed member's grid callsign wears its Loom
                    // accent (the matrix dots keep speaking STATE); the
                    // selection band and the queued fade both outrank it.
                    let typed_accent = model
                        .loom_task_type(&node.task)
                        .and_then(|(record, _)| crate::style::loom_accent_style(&record.color));
                    names.push(Span::styled(
                        name,
                        if selected {
                            theme.selection_style().add_modifier(Modifier::BOLD)
                        } else if node.state == haider_rpc::FleetAgentStateWire::Queued {
                            theme.faint_style()
                        } else if let Some(accent) = typed_accent {
                            accent
                        } else {
                            theme.dim_style()
                        },
                    ));
                    if let Some(marker) = marker {
                        names.push(Span::styled(format!(" {marker}"), theme.faint_style()));
                    }
                    names.push(Span::raw(" ".repeat(FLEET_CELL_GAP)));
                    // Ten columns cannot hold two facts, so the cell carries
                    // ONE — the model, or the provider when there is no
                    // model. It follows the CELL's law, not the list's: the
                    // callsign directly above it is already hard-cut to this
                    // same width, so the identity truncates too, with `…`
                    // marking the elision. Absent both facts the cell keeps
                    // the plain blank, byte-identical to the old spacer.
                    match fleet::node_identity_cell(node, FLEET_CELL_W) {
                        Some(ident) => {
                            let mut cell = ident;
                            let cell_width = cell.chars().count();
                            cell.push_str(&" ".repeat(FLEET_CELL_W.saturating_sub(cell_width)));
                            idents.push(Span::styled(
                                cell,
                                if selected {
                                    theme.selection_style()
                                } else {
                                    theme.faint_style()
                                },
                            ));
                        }
                        None => idents.push(Span::raw(" ".repeat(FLEET_CELL_W))),
                    }
                    idents.push(Span::raw(" ".repeat(FLEET_CELL_GAP)));
                    cell_hits.push((
                        lines.len(),
                        u16::try_from(1 + col * step).unwrap_or(u16::MAX),
                        u16::try_from(FLEET_CELL_W).unwrap_or(u16::MAX),
                        Hit::FleetNode(node.agent_id.as_str().to_owned()),
                    ));
                }
                lines.push(Line::from(matrix_a));
                lines.push(Line::from(matrix_b));
                lines.push(Line::from(names));
                lines.push(Line::from(idents));
            }
        }
    }

    // Pin the footer to the bottom of the area.
    let used = lines.len();
    let footer_start = (area.height as usize).saturating_sub(footer.len());
    for _ in used..footer_start {
        lines.push(Line::raw(""));
    }
    lines.extend(footer);
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
    for (offset, x, hit_width, hit) in cell_hits {
        let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
        // Grid hits cover the matrix rows, the callsign row AND the
        // identity row under it — the whole cell is the click target.
        let height = match hit {
            Hit::FleetNode(_) if matches!(density, fleet::Density::Grid) => 4,
            _ => 1,
        };
        if y < area.y + area.height {
            hits.push((
                Rect {
                    x: area.x + x,
                    y,
                    width: hit_width.min(area.width.saturating_sub(x)),
                    height,
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
            u16::try_from(
                1 + wrapped_menu_body(m, area.width, model.clock_ms).len() + m.options.len() + 1,
            )
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
    // closes with the rule reserved above. S2 item 4 retired the pad row
    // that used to ride under it (session parity): the band rests at ONE
    // text row between its rules; the chip transcript's own trailing
    // blank line carries the breathing room instead.
    let [
        header_area,
        header_rule,
        transcript_area,
        rule_area,
        composer_area,
        band_rule_area,
        subtree_area,
        _gap,
    ] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Length(header_rule_h),
        Constraint::Min(transcript_min),
        Constraint::Length(input_rule_h),
        Constraint::Length(input_height),
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
    if chip.lockdown {
        header_bottom.push(Span::styled("🔒 lockdown  ", theme.gold_style()));
    }
    if let Some(handoff) = chip
        .handoff_dir
        .as_deref()
        .and_then(|path| std::path::Path::new(path).file_name())
        .and_then(std::ffi::OsStr::to_str)
    {
        header_bottom.push(Span::styled(
            format!("· handoff {handoff}  "),
            theme.dim_style(),
        ));
    }
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
    let mut prefix: Vec<Line<'static>> = Vec::new();
    if let Some(metrics) = model.chip_metrics(chip) {
        for line in crate::agent_metrics::detail_lines(metrics) {
            prefix.push(Line::styled(format!(" {line}"), theme.dim_style()));
        }
        prefix.push(Line::default());
    }
    let mut transcript_cache = chip.transcript_layout.borrow_mut();
    transcript_cache.reconcile(
        &chip.transcript,
        model.theme,
        theme,
        transcript_area.width,
        model.anim_phase,
    );
    let mut tail: Vec<Line<'static>> = Vec::new();
    // Session parity: the tail is up for the WHOLE running turn, not just the
    // THINKING beat. Judged on `display` (the badge's truth), NOT the raw
    // `chip.state`: a chip whose live children promoted it to `Waiting` prints
    // the dedicated "waiting on N child" row just below, and reading the raw
    // state here would show both rows at once.
    if display.is_turn_active() {
        // S2 item 5: the chip view keeps the session's rhythm — one
        // breathing row above the thinking badge.
        tail.push(Line::default());
        tail.push(owned_line(thinking_line(
            theme,
            model.anim_phase,
            model.truecolor,
        )));
    }
    if display == crate::script::ChipDisplayState::Waiting && live_children > 0 {
        tail.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("◔ waiting on {live_children} child subagent — this session waits too"),
                theme.dim_style(),
            ),
        ]));
    }
    // S2 item 5, session parity: one blank line between the chip
    // transcript's tail and the band.
    if !prefix.is_empty() || !transcript_cache.entries.is_empty() || !tail.is_empty() {
        tail.push(Line::default());
    }
    let total = u64::from(wrapped_lines_height(&prefix, transcript_area.width))
        .saturating_add(transcript_cache.total_rows)
        .saturating_add(u64::from(wrapped_lines_height(
            &tail,
            transcript_area.width,
        )));
    let max_scroll = total.saturating_sub(u64::from(transcript_area.height));
    model.scroll_max.set(max_scroll);
    // Drag-autoscroll edges (QoL wave), as on the session transcript.
    model.transcript_view.set(transcript_area);
    model
        .scroll_back
        .set(model.scroll_back.get().min(max_scroll));
    let (visible_lines, visible_base, visible_total, scroll) = virtualized_transcript_lines(
        &mut transcript_cache,
        &chip.transcript,
        theme,
        model.anim_phase,
        TranscriptViewport {
            prefix: &prefix,
            suffix: &tail,
            scroll_back: model.scroll_back.get(),
            height: transcript_area.height,
            width: transcript_area.width,
        },
    );
    let corrected_max = visible_total.saturating_sub(u64::from(transcript_area.height));
    model.scroll_max.set(corrected_max);
    model
        .scroll_back
        .set(model.scroll_back.get().min(corrected_max));
    frame.render_widget(
        Paragraph::new(Text::from(visible_lines))
            .wrap(Wrap { trim: false })
            .scroll((
                u16::try_from(scroll.saturating_sub(visible_base)).unwrap_or(u16::MAX),
                0,
            )),
        transcript_area,
    );
    image_reveal_hits(
        &transcript_cache,
        &chip.transcript,
        u64::from(wrapped_lines_height(&prefix, transcript_area.width)),
        scroll,
        transcript_area,
        hits,
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
        let (menu_lines, option_rows) = menu_block(
            menu,
            model.menu_selection,
            theme,
            composer_area,
            &footer,
            model.clock_ms,
        );
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
    } else if effort_picker_showing(model) {
        render_effort_picker(model, theme, frame, rule_area, composer_area, hits);
    } else if theme_picker_showing(model) {
        render_theme_picker(model, theme, frame, rule_area, composer_area, hits);
    } else {
        render_composer(model, theme, frame, rule_area, composer_area, hits);
    }
    // The band's closing anatomy (TUI6 item 6, S2 item 4): the frame rule
    // directly under the band — rendered on BOTH the composer and
    // question-card forms, exactly as the session band does.
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
                    "{} · never writes code — it spawns and steers local sessions",
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
            TranscriptEntry::Peer {
                sender,
                sender_kind,
                text,
                receipt,
                ..
            } => peer_entry_lines(
                &mut lines,
                sender,
                sender_kind,
                text,
                *receipt,
                theme,
                transcript_area.width,
            ),
            TranscriptEntry::Note { text } => {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(text.as_str(), theme.dim_style()),
                ]));
            }
            TranscriptEntry::Refusal {
                provider,
                tool,
                reason,
            } => refusal_entry_line(&mut lines, provider, tool, reason, theme),
            TranscriptEntry::Error { text, presentation } => {
                // The same card-shaped treatment as the session view
                // (title / detail / fact line via one shared helper).
                error_entry_lines(
                    &mut lines,
                    text,
                    presentation.as_ref(),
                    theme,
                    transcript_area.width,
                );
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
                    let (mut text, truncated) = text.to_owned_prefix(EXTREME_LOGICAL_LINE_CHARS);
                    if truncated {
                        text.push_str(" ⋯ /export expands raw text");
                    }
                    let body = if block.streaming {
                        wrap_body(&format!("{text}▮"), budget.max(1))
                    } else {
                        wrap_body(&text, budget.max(1))
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

    if effort_picker_showing(model) {
        render_effort_picker(model, theme, frame, rule_area, composer_area, hits);
    } else if theme_picker_showing(model) {
        render_theme_picker(model, theme, frame, rule_area, composer_area, hits);
    } else {
        render_composer(model, theme, frame, rule_area, composer_area, hits);
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
}

/// The persistent banner's severity register (E5-E8 visual pass).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticTone {
    /// The err rail: work is lost or uncertain, or a daemon lane is dead.
    Failure,
    /// The calm warn rail: a lane degraded but the session still works —
    /// action needed, never alarm.
    ActionNeeded,
}

/// The one diagnostic the frame's top row shows, worst slot first, with
/// its severity tone. Store faults, a client/daemon incompatibility, an
/// exhausted busy-retry bound, and the link supervisor's bounded-restart
/// exhaustion are FAILURES (err). A degraded voice lane — mic capture
/// death, the talk supervisor — leaves the session fully usable and wears
/// the calm warn tone instead.
fn persistent_diagnostic(
    model: &AppModel,
) -> Option<(&haider_protocol::error::ErrorPresentation, DiagnosticTone)> {
    if let Some(presentation) = &model.profile_diagnostic {
        return Some((presentation, DiagnosticTone::Failure));
    }
    if let Some(presentation) = &model.compatibility_diagnostic {
        return Some((presentation, DiagnosticTone::Failure));
    }
    if let Some(presentation) = &model.voice_diagnostic {
        return Some((presentation, DiagnosticTone::ActionNeeded));
    }
    if let Some(presentation) = &model.supervisor_diagnostic {
        // The talk supervisor ("talk-supervisor-unavailable") only takes
        // the voice lane down; the link supervisor takes the daemon with it.
        let tone = if presentation.subcode.as_str().starts_with("talk") {
            DiagnosticTone::ActionNeeded
        } else {
            DiagnosticTone::Failure
        };
        return Some((presentation, tone));
    }
    if let Some(presentation) = &model.command_diagnostic {
        return Some((presentation, DiagnosticTone::Failure));
    }
    None
}

/// The persistent diagnostic banner's single row (E5 visual pass), in the
/// error-card grammar: severity rail `▏` + glyph + BOLD tone-ink title,
/// dim detail, then the dim fact segments (subcode + actions — the one
/// error-fact vocabulary). Severity travels in TEXT via the glyph (✗
/// failure / ⚠ action-needed), never in ink alone. Under width pressure
/// the facts shed whole segments first (the subcode never sheds), then
/// the detail ellipsizes; the title never yields. A detail that opens by
/// echoing its own title ("Store unwritable — …") drops the echo — the
/// bold title already said it.
fn diagnostic_banner_line(
    presentation: &haider_protocol::error::ErrorPresentation,
    tone: DiagnosticTone,
    theme: &Theme,
    width: u16,
) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    let (ink, glyph) = match tone {
        DiagnosticTone::Failure => (theme.err, "✗"),
        DiagnosticTone::ActionNeeded => (theme.warn, "⚠"),
    };
    let title = presentation.title.as_str();
    let detail = presentation
        .detail
        .strip_prefix(title)
        .and_then(|rest| rest.strip_prefix(" — "))
        .unwrap_or(presentation.detail.as_str());
    let head = format!("{glyph} {title}");
    let after_head = (width as usize).saturating_sub(1 + head.width());
    let facts = crate::projection::error_fact_segments_with_actions(presentation, None);
    let subcode_width = facts
        .first()
        .map_or(0, |(segment, _)| segment.as_str().width());
    // The facts claim what a full detail would leave them — but at least
    // their identity subcode; the detail ellipsizes into the rest.
    let facts_budget = after_head
        .saturating_sub(" — ".width() + detail.width() + " · ".width())
        .max(subcode_width);
    let fact_line = shed_fact_line(&facts, facts_budget);
    let detail_budget =
        after_head.saturating_sub(" — ".width() + " · ".width() + fact_line.width());
    let detail_shown = ellipsize(detail, detail_budget);
    let mut spans = vec![
        Span::styled("▏", ratatui::style::Style::default().fg(ink.into())),
        Span::styled(
            head,
            ratatui::style::Style::default()
                .fg(ink.into())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !detail_shown.is_empty() {
        spans.push(Span::styled(
            format!(" — {detail_shown}"),
            theme.dim_style(),
        ));
    }
    if !fact_line.is_empty() {
        spans.push(Span::styled(format!(" · {fact_line}"), theme.dim_style()));
    }
    Line::from(spans)
}

/// The menu's body lines pre-wrapped by display cells into the menu's
/// content width (sim `.iml` white-space: pre-wrap, tui.js:4946).
///
/// Recovery bodies come from their typed presentation, not the duplicate
/// menu prose. Detail always uses body tone, even when it starts with a diff
/// marker. The final fact row sheds only whole segments and uses `now_ms` for
/// its live reset countdown.
fn wrapped_menu_body(
    menu: &haider_protocol::menu::Menu,
    width: u16,
    now_ms: u64,
) -> Vec<(String, DiffTone)> {
    // D4: a `plan` proposal's body is the full document — it renders in the
    // transcript area, never in the composer band; the band keeps a one-line
    // pointer so the sizing ladder stays sane.
    if menu.origin == "plan" {
        return vec![(
            "proposal above — ↑↓/PgUp/PgDn scroll · Tab cycles the decision".into(),
            DiffTone::Body,
        )];
    }
    let budget = (width as usize).saturating_sub(2).max(1);
    // Both recovery families speak through their typed presentation when
    // they carry one: the provider/account card (E2) and the E6
    // effect-reconciliation card. A presentation-less Recovery card (demo
    // scripts, older daemons) keeps the baseline prose body below.
    let typed_presentation = match &menu.kind {
        haider_protocol::menu::MenuKind::ErrorRecovery { presentation, .. } => Some(presentation),
        haider_protocol::menu::MenuKind::Recovery { presentation, .. } => presentation.as_ref(),
        _ => None,
    };
    if let Some(presentation) = typed_presentation {
        let mut rows: Vec<(String, DiffTone)> = presentation
            .detail
            .split('\n')
            .flat_map(|logical| wrap_body(logical, budget))
            .map(|row| (row, DiffTone::Body))
            .collect();
        let facts = crate::projection::error_fact_segments(presentation, Some(now_ms));
        rows.push((shed_fact_line(&facts, budget), DiffTone::Body));
        return rows;
    }
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

/// Sheds whole error-fact segments until the line fits `budget` display
/// cells. The highest rank drops first (rightmost on ties); rank zero and
/// display order are stable.
#[must_use]
pub fn shed_fact_line(segments: &[(String, u8)], budget: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let separator_width = " · ".width();
    let mut kept = segments
        .iter()
        .map(|(segment, rank)| (segment.as_str(), *rank, segment.as_str().width()))
        .collect::<Vec<_>>();
    let mut width = kept.iter().map(|(_, _, width)| width).sum::<usize>()
        + kept.len().saturating_sub(1) * separator_width;
    while kept.len() > 1 && width > budget {
        let Some((drop_index, _)) = kept
            .iter()
            .enumerate()
            .filter(|(_, (_, rank, _))| *rank > 0)
            .max_by_key(|(index, (_, rank, _))| (*rank, *index))
        else {
            break;
        };
        let (_, _, dropped_width) = kept.remove(drop_index);
        width = width.saturating_sub(dropped_width + separator_width);
    }
    let capacity = kept
        .iter()
        .map(|(segment, _, _)| segment.len())
        .sum::<usize>()
        + kept.len().saturating_sub(1) * " · ".len();
    let mut joined = String::with_capacity(capacity);
    for (index, (segment, _, _)) in kept.iter().enumerate() {
        if index > 0 {
            joined.push_str(" · ");
        }
        joined.push_str(segment);
    }
    joined
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
fn permission_label(permission: haider_protocol::permission::SystemPermission) -> &'static str {
    use haider_protocol::permission::SystemPermission;
    match permission {
        SystemPermission::ScreenRecording => "Screen Recording",
        SystemPermission::Accessibility => "Accessibility",
    }
}

/// Rows the computer-permission grant card claims in the input band: header +
/// note + Open Settings, plus Retry while the grant is still pending (a
/// restart-pending card drops Retry — a recheck cannot help until restart).
fn permission_card_rows(card: &haider_protocol::permission::PermissionGrantNeeded) -> u16 {
    if card.auto_restart_pending { 3 } else { 4 }
}

/// One full-width card button: bold label left, dim key hint right.
fn permission_button_line(label: &str, key_hint: &str, theme: &Theme, width: u16) -> Line<'static> {
    let left = format!(" ›  {label}");
    let right = format!("{key_hint} ");
    let pad = (width as usize)
        .saturating_sub(left.chars().count() + right.chars().count())
        .max(1);
    Line::from(vec![
        Span::styled(left, theme.text_style().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, ratatui::style::Style::default().fg(theme.dim.into())),
    ])
}

/// The computer OS-permission grant card: replaces the plain
/// `computer-os-permission` blocking menu with a labelled prompt plus the
/// clickable Open Settings / Retry actions (and a granted-restart note). The
/// returned `(row_offset, Hit)` pairs come from what actually rendered.
fn permission_card_block(
    card: &haider_protocol::permission::PermissionGrantNeeded,
    theme: &Theme,
    area: Rect,
) -> (Vec<Line<'static>>, Vec<(u16, Hit)>) {
    let allocated = area.height as usize;
    if allocated == 0 {
        return (Vec::new(), Vec::new());
    }
    let label = permission_label(card.permission);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(u16, Hit)> = Vec::new();
    if card.auto_restart_pending {
        lines.push(Line::from(vec![
            Span::styled("  ✓  ", theme.ok_style()),
            Span::styled(
                format!("{label} granted"),
                theme.ok_style().add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::styled(
            "     Restart Haider to use it — quit and reopen; the parked action resumes automatically."
                .to_string(),
            theme.text_style(),
        ));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  ⚠  ", theme.warn_style()),
            Span::styled(
                format!("macOS needs {label} for computer use"),
                theme.warn_style().add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::styled(
            "     Haider opened the system prompt — grant it, then Retry (it resumes automatically)."
                .to_string(),
            theme.text_style(),
        ));
    }
    let open_offset = u16::try_from(lines.len()).unwrap_or(0);
    lines.push(permission_button_line(
        "Open Settings",
        "o",
        theme,
        area.width,
    ));
    hits.push((open_offset, Hit::PermissionOpenSettings));
    if !card.auto_restart_pending {
        let retry_offset = u16::try_from(lines.len()).unwrap_or(0);
        lines.push(permission_button_line(
            "Retry now",
            "r ⏎",
            theme,
            area.width,
        ));
        hits.push((retry_offset, Hit::PermissionRetry));
    }
    lines.truncate(allocated);
    hits.retain(|(offset, _)| (*offset as usize) < allocated);
    (lines, hits)
}

/// D4: the full-screen plan proposal — header, markdown document with the
/// agent-message rail treatment, and a scroll indicator. Scroll is clamped to
/// the document, so a stale `plan_scroll` can never blank the surface.
fn render_plan_document(
    menu: &haider_protocol::menu::Menu,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    scroll: u16,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("◇ PLAN ", theme.gold_style()),
        Span::styled("· ", theme.dim_style()),
        Span::styled(menu.title.clone(), theme.bright_style()),
    ]));
    lines.push(Line::styled(
        "─".repeat(area.width as usize),
        theme.dim_style(),
    ));
    let budget = (area.width as usize).saturating_sub(3);
    if budget > 0 {
        let document = menu.body.join("\n");
        let md_lines = crate::md::render_markdown(&document);
        let mut idx = 0usize;
        let push_row = |lines: &mut Vec<Line<'static>>, row: Vec<crate::md::MdSpan>| {
            let mut spans = vec![Span::raw(" "), Span::styled("▏ ", theme.rail_style())];
            spans.extend(
                row.into_iter()
                    .map(|span| Span::styled(span.text, theme.md_style(span.kind))),
            );
            lines.push(Line::from(spans));
        };
        while idx < md_lines.len() {
            if md_lines[idx].table.is_some() {
                let start = idx;
                while idx < md_lines.len() && md_lines[idx].table.is_some() {
                    idx += 1;
                }
                let rows: Vec<&crate::md::MdTableRow> = md_lines[start..idx]
                    .iter()
                    .filter_map(|line| line.table.as_ref())
                    .collect();
                for row in crate::md::layout_table(&rows, budget) {
                    push_row(&mut lines, row);
                }
                continue;
            }
            for row in crate::md::wrap_spans(&md_lines[idx].spans, budget) {
                push_row(&mut lines, row);
            }
            idx += 1;
        }
    }
    let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_scroll = total.saturating_sub(area.height);
    let clamped = scroll.min(max_scroll);
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((clamped, 0)), area);
    // Scroll indicator in the top-right corner while the document overflows.
    if max_scroll > 0 && area.width > 12 {
        let label = format!(" {clamped}/{max_scroll} ▾ ");
        let width = u16::try_from(label.len()).unwrap_or(0).min(area.width);
        let corner = Rect {
            x: area.x + area.width - width,
            y: area.y,
            width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::styled(label, theme.dim_style())),
            corner,
        );
    }
    max_scroll
}

fn menu_block(
    menu: &haider_protocol::menu::Menu,
    selection: usize,
    theme: &Theme,
    area: Rect,
    footer: &str,
    now_ms: u64,
) -> (Vec<Line<'static>>, Vec<(u16, usize)>) {
    let allocated = area.height as usize;
    if allocated == 0 {
        return (Vec::new(), Vec::new());
    }
    // Recovery cards use warning ink for remediable conditions and error
    // ink only for hard account failures or an unclassified generic failure.
    let recovery_ink = match &menu.kind {
        haider_protocol::menu::MenuKind::ErrorRecovery { card, .. } => {
            use haider_protocol::menu::ErrorRecoveryCardKind;
            Some(match card {
                ErrorRecoveryCardKind::AccountRevoked
                | ErrorRecoveryCardKind::AccountDeleted
                | ErrorRecoveryCardKind::Generic => theme.err,
                ErrorRecoveryCardKind::OauthExpired
                | ErrorRecoveryCardKind::InvalidApiKey
                | ErrorRecoveryCardKind::KeychainRelink
                | ErrorRecoveryCardKind::RateLimit
                | ErrorRecoveryCardKind::QuotaExhausted
                | ErrorRecoveryCardKind::PartialStream
                | ErrorRecoveryCardKind::StoreUnwritable => theme.warn,
            })
        }
        // E6: the effect-reconciliation card is UNCERTAINTY, not failure —
        // the write may or may not have committed. Calm amber, never err;
        // the ⌁ glyph and the options carry the rest.
        haider_protocol::menu::MenuKind::Recovery { .. } => Some(theme.warn),
        _ => None,
    };
    let selection = selection.min(menu.options.len().saturating_sub(1));
    let mut body_rows = wrapped_menu_body(menu, area.width, now_ms);
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
        match recovery_ink {
            // The accent cell replaces the pad; title in the tone ink,
            // BOLD — TITLE prominent, severity readable from the rail.
            Some(ink) => lines.push(Line::from(vec![
                Span::styled("▏", ratatui::style::Style::default().fg(ink.into())),
                Span::styled(
                    format!("{glyph} {}", menu.title),
                    ratatui::style::Style::default()
                        .fg(ink.into())
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            None => lines.push(Line::from(vec![Span::styled(
                format!(" {glyph} {}", menu.title),
                theme.warn_style(),
            )])),
        }
    }
    for (body_row, tone) in body_rows {
        let gutter = match recovery_ink {
            Some(ink) => Span::styled("▏", ratatui::style::Style::default().fg(ink.into())),
            None => Span::raw(" "),
        };
        lines.push(Line::from(vec![
            gutter,
            Span::styled(body_row, tone.style(theme)),
        ]));
    }
    for (offset, option) in menu.options.iter().skip(start).take(window_len).enumerate() {
        let index = start + offset;
        let selected = index == selection;
        let cursor = if selected { "❯" } else { " " };
        // The server's first recovery option is primary; selection styling
        // takes precedence over its idle gold accent.
        let primary = index == 0 && recovery_ink.is_some();
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
                } else if primary {
                    theme.gold_style()
                } else {
                    theme.menu_style()
                },
            ),
        ];
        // Option detail is all-or-nothing; it never truncates mid-word.
        if selected
            && recovery_ink.is_some()
            && let Some(detail) = &option.detail
        {
            let suffix = format!(" — {detail}");
            let used = span_row_width(&spans);
            if used + unicode_width::UnicodeWidthStr::width(suffix.as_str()) <= area.width as usize
            {
                spans.push(Span::styled(suffix, theme.dim_style()));
            }
        }
        option_rows.push((u16::try_from(lines.len()).unwrap_or(u16::MAX), index));
        lines.push(if selected {
            // Selection ground spans the full row (sim `.imo.sel`).
            let pad = (area.width as usize).saturating_sub(span_row_width(&spans));
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

fn span_row_width(spans: &[Span<'_>]) -> usize {
    use unicode_width::UnicodeWidthStr;
    spans.iter().map(|span| span.content.as_ref().width()).sum()
}

/// Rows the `/theme` picker card needs: title + body + the five choice
/// rows + hint (menu_block windows the options under height pressure).
const THEME_PICKER_ROWS: u16 = 8;

/// Whether the `/theme` picker renders THIS frame: open, on a surface
/// that hosts it, with no daemon card holding the input slot (the menu /
/// ask branches outrank it — local chrome never sits on a live ask).
fn theme_picker_showing(model: &AppModel) -> bool {
    model.theme_picker.is_some()
        && matches!(
            model.screen,
            Screen::Launcher | Screen::Session | Screen::Aura | Screen::Subagent
        )
        && model.projection.open_menu().is_none()
        && model.login.is_none()
        && !(model.screen == Screen::Subagent
            && model
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some()))
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
                    crate::theme::ThemeKey::Water => "sea glass · tide teal · coral",
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
    // The clock feeds only recovery fact lines; a Choice card ignores it.
    let (lines, option_rows) = menu_block(&card, picker.selection, theme, composer_area, footer, 0);
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

/// Whether the `/effort` picker renders THIS frame (G3): open, on a
/// session surface, with no daemon card holding the input slot — the same
/// outranking law as `/theme`.
fn effort_picker_showing(model: &AppModel) -> bool {
    model.effort_picker.is_some()
        && matches!(model.screen, Screen::Session | Screen::Subagent)
        && model.projection.open_menu().is_none()
        && model.login.is_none()
        && !(model.screen == Screen::Subagent
            && model
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some()))
}

/// Rows the `/effort` picker card needs: title + body + the option rows +
/// hint (menu_block windows the options under height pressure).
fn effort_picker_rows_height(model: &AppModel) -> u16 {
    let options = model.effort_picker_rows().len().max(1);
    u16::try_from(options).unwrap_or(u16::MAX).saturating_add(3)
}

/// The `/effort` picker (G3): the CURRENT pair's declared ladder in the
/// composer's slot, through the SAME `menu_block` anatomy as `/theme`. The
/// ● marks the session's current selection; ◇ tags the provider default.
/// Hits carry [`Hit::EffortOption`] so hover highlights and a click
/// commits.
fn render_effort_picker(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    rule_area: Rect,
    composer_area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let Some(picker) = model.effort_picker.as_ref() else {
        return;
    };
    use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
    let rows = model.effort_picker_rows();
    let pending = picker.pending.clone();
    let options = rows
        .iter()
        .map(|row| {
            let marker = if row.is_current { '●' } else { '○' };
            let name = row.effort.as_deref().unwrap_or("default");
            let mut label = format!("{marker} {name}");
            if row.is_provider_default {
                label.push_str(" — provider default");
            }
            if row.effort.is_none() {
                label.push_str(" — revert to the provider's own level");
            }
            if pending.as_ref() == Some(&row.effort) {
                label.push_str(" · committing…");
            }
            MenuOption {
                key: name.to_owned(),
                label,
                detail: None,
                decision: None,
            }
        })
        .collect();
    let mut body = vec![format!(
        "{} · {} — validated by the daemon against this pair's ladder",
        model.identity.model_short, model.identity.provider
    )];
    if let Some(error) = &picker.error {
        body.push(format!("✗ {error}"));
    }
    let card = Menu {
        id: haider_protocol::ids::MenuId::new("effort-picker"),
        kind: MenuKind::Choice,
        title: "effort — reasoning depth for this pair".to_owned(),
        body,
        options,
        blocking: false,
        scope: MenuScope::Session,
        origin: "effort".to_owned(),
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
    let footer = " ↑↓ pick · ⏎ select · 1-9 quick · esc back";
    // The clock feeds only recovery fact lines; a Choice card ignores it.
    let (lines, option_rows) = menu_block(&card, picker.selection, theme, composer_area, footer, 0);
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
                Hit::EffortOption(option_index),
            ));
        }
    }
}

/// Sim `MENU_GLYPH` (tui.js:3057) mapped onto the protocol's menu kinds.
/// The command cards (`voice` ◉ / `tools` ⚒) are `Choice` menus — their
/// free-form `origin` tag carries the sim kind (MenuKind is frozen).
fn menu_glyph(menu: &haider_protocol::menu::Menu) -> &'static str {
    use haider_protocol::menu::{ErrorRecoveryCardKind, MenuKind};
    match &menu.kind {
        MenuKind::Recovery { .. } => "⌁",
        // Renewable limits use the reset glyph. Other recovery classes use
        // warning, matching the partial-stream transcript marker.
        MenuKind::ErrorRecovery {
            card: ErrorRecoveryCardKind::RateLimit | ErrorRecoveryCardKind::QuotaExhausted,
            ..
        } => "⟳",
        MenuKind::ErrorRecovery { .. } => "⚠",
        MenuKind::Exhausted => "⟳",
        // CG-M1 SHIP gate: the graph flag, matching the strip and note rows.
        MenuKind::GraphHumanConfirm { .. } => "⚑",
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
    // T2: the `/talk` setup card replaces the composer the same way (its
    // row count is stage-dependent).
    if let Some(card) = model.talk_setup.as_ref() {
        return card.height();
    }
    // The `/effort` picker replaces the composer (G3) — same
    // input-replacement law as `/theme`; a daemon card outranks it.
    if effort_picker_showing(model) {
        return effort_picker_rows_height(model);
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
    let chips = u16::from(
        model.composer.has_attachments() || !model.mirrored_input_attachments().is_empty(),
    );
    // T2: the partial-transcript ghost row claims ONE row above the text
    // rows too — same shared-predicate discipline
    // (`AppModel::talk_ghost_visible`), and CHROME by law: nothing here
    // touches the transcript projection.
    let ghost = u16::from(model.talk_ghost_visible());
    // 970 owner bug 2: a refused/failed image claims ONE row at the top of
    // the band — the same shared-predicate discipline as the chip row, so
    // the geometry and the paint can never disagree.
    let notice = u16::from(model.composer_notice.is_some());
    u16::try_from(rows)
        .unwrap_or(1)
        .saturating_add(chips)
        .saturating_add(ghost)
        .saturating_add(notice)
}

/// The gold rule + composer rows on the input ground (sim InputBar,
/// tui.js:5395: `border-top: gold`, `background: inputBg`). Pushes the
/// talk-chip hit region so the click lands exactly on the chip.
///
/// BAND ANATOMY (TUI6 item 6, per Claude Code's own TUI): every surface
/// that draws an input band closes it with a rule BELOW as well as the
/// rule above — and since S2 item 4 the rule sits DIRECTLY under the
/// last composer row on every surface (the session/subagent pad row is
/// retired: the band rests at one line and grows only with content). The
/// sweep's enumeration of input-band render paths:
///   - `render_launcher`  — `band_rule_area` (TUI5 item 1b, gap→rule);
///   - `render_session`   — `band_rule_area`, on BOTH the composer and
///     blocking-menu forms (the rule renders outside the menu if/else);
///   - `render_subagent`  — `band_rule_area` (TUI6 — the owner's
///     screenshot), composer and question-card forms alike;
///   - `render_aura`      — `band_rule_area` (TUI6, gap→rule);
///   - the login card and the arg-slot/palette state REPLACE the
///     composer's CONTENT inside the same band, so they inherit the
///     hosting surface's two rules — no separate path exists.
///
/// Each surface's pair is pinned by a test in `tui6_softwrap_tests`; the
/// one-line rest height by `s2_ui_refinement_tests`.
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
    // F2c: the band's TOP BORDER carries the session identity at its
    // right end — `model · oauth|api · reasoning [· fast]`, NO alias —
    // right above the talk chip; the status bar keeps state + tokens.
    // Width degradation drops whole segments (reasoning first, then
    // auth, then the line) — never mid-word garbage.
    let rule_width = rule_area.width as usize;
    // SURFACE-scoped, not session-scoped: while a child is being steered
    // this rule speaks the CHILD's identity. It read the parent's before —
    // the owner's screenshot had `glm-5.2 · api` under a child header that
    // said `deepseek-v4-flash`.
    let identity_text = model
        .surface_composer_identity(rule_width.saturating_sub(6))
        .map(|identity| format!(" {identity} "));
    // W-G (owner 2026-08-15): the throughput readout moved OFF this rule to
    // its own ambient row above the composer band — the rule carries the
    // session identity alone again.
    // W-flow inline identity: a BOUND agent type leads the identity block
    // with its `{glyph} @{id}` chip in the registry accent (the same
    // fallback law as the header — absent binding/snapshot renders today's
    // rule byte-identically). The chip yields WHOLE with the identity text
    // when the rule is too narrow.
    // The bound agent-type chip is the SESSION's binding, so it is
    // parent-scoped for exactly the same reason the model was: it does not
    // ride the rule while a child is being steered.
    let bound_chip = identity_text
        .as_ref()
        .filter(|_| model.screen != crate::app::Screen::Subagent)
        .and(model.bound_loom_type(model.identity.agent_type.as_deref()))
        .map(|record| {
            let chip = if record.glyph.is_empty() {
                format!(" @{}", record.id)
            } else {
                format!(" {} @{}", record.glyph, record.id)
            };
            let accent = crate::style::loom_accent_style(&record.color)
                .unwrap_or_else(|| theme.gold_style());
            (chip, accent)
        });
    let chip_cells = bound_chip
        .as_ref()
        .map_or(0, |(chip, _)| chip.chars().count());
    let rule_line = match &identity_text {
        Some(text) if rule_width > text.chars().count() + chip_cells + 2 => {
            let fill = rule_width - text.chars().count() - chip_cells - 2;
            let mut spans = vec![Span::styled("─".repeat(fill), theme.gold_style())];
            if let Some((chip, accent)) = &bound_chip {
                spans.push(Span::styled(
                    chip.clone(),
                    accent.add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(text.clone(), theme.dim_style()));
            spans.push(Span::styled("──", theme.gold_style()));
            Line::from(spans)
        }
        // Chip too wide beside the text: drop the chip whole, keep the
        // identity segment exactly as before the binding existed.
        Some(text) if rule_width > text.chars().count() + 2 => {
            let fill = rule_width - text.chars().count() - 2;
            Line::from(vec![
                Span::styled("─".repeat(fill), theme.gold_style()),
                Span::styled(text.clone(), theme.dim_style()),
                Span::styled("──", theme.gold_style()),
            ])
        }
        _ => Line::styled("─".repeat(rule_width), theme.gold_style()),
    };
    frame.render_widget(
        Paragraph::new(rule_line).style(theme.text_style()),
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
    // 970 owner bug 2: the image notice is the TOPMOST row in the band —
    // it answers a gesture the user just made, and the draft it preserves
    // sits directly under it. Warn ink, one line, no hits.
    if let Some(notice) = &model.composer_notice
        && row_area.height > 1
    {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" ".repeat(COMPOSER_PAD)),
                Span::styled("⚠ ", theme.warn_style()),
                Span::styled(notice.text(), theme.warn_style()),
            ]))
            .style(theme.input_style()),
            Rect {
                height: 1,
                ..row_area
            },
        );
        row_area.y += 1;
        row_area.height -= 1;
    }
    // T2: the partial-transcript ghost row carves the VERY top of the
    // band (dim, replaced per partial, realized into the composer on
    // commit). Same shared predicate as `composer_height`; chrome only —
    // it emits no hits and never enters the transcript.
    if model.talk_ghost_visible() && row_area.height > 1 {
        frame.render_widget(
            Paragraph::new(talk_ghost_line(model, theme, row_area.width))
                .style(theme.input_style()),
            Rect {
                height: 1,
                ..row_area
            },
        );
        row_area.y += 1;
        row_area.height -= 1;
    }
    if model.login.is_none()
        && (model.composer.has_attachments() || !model.mirrored_input_attachments().is_empty())
        && row_area.height > 1
    {
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
            format!("[{} {}{suffix}]", chip.kind.glyph(), chip.label),
            theme.gold_style(),
        ));
        spans.push(Span::raw(" "));
    }
    let mirrored = model.mirrored_input_attachments();
    if !mirrored.is_empty() {
        let suffix = if mirrored.len() == 1 { "" } else { "s" };
        spans.push(Span::styled(
            format!("[+{} attachment{suffix}]", mirrored.len()),
            theme.gold_style(),
        ));
        spans.push(Span::raw(" "));
    }
    if model.composer.has_attachments() {
        spans.push(Span::styled(
            "· ⌫ at the start removes".to_owned(),
            theme.dim_style(),
        ));
    }
    Line::from(spans)
}

/// T2 — the partial-transcript ghost row: `◉ <text>` in the dim slot,
/// replaced per partial. Overlong text keeps its TAIL (the newest words
/// are what the speaker is watching land) behind a leading `…`.
fn talk_ghost_line(model: &AppModel, theme: &Theme, width: u16) -> Line<'static> {
    let budget = usize::from(width).saturating_sub(COMPOSER_PAD + 4).max(4);
    let ghost = model.talk.ghost.trim();
    let chars: Vec<char> = ghost.chars().collect();
    let text = if chars.len() > budget {
        let tail: String = chars[chars.len() - (budget.saturating_sub(1))..]
            .iter()
            .collect();
        format!("…{tail}")
    } else {
        ghost.to_owned()
    };
    Line::from(vec![
        Span::raw(" ".repeat(COMPOSER_PAD)),
        Span::styled("◉ ".to_owned(), theme.gold_style()),
        Span::styled(text, theme.dim_style()),
    ])
}

/// T2 — the right-to-left wave's spans: one glyph per ring slot, newest at
/// the right edge. Hot columns wear the gold slot, quiet history the
/// faint slot — theme tokens only (the mechanical no-raw-color law covers
/// this seam like every other).
fn talk_wave_spans(model: &AppModel, theme: &Theme) -> Vec<Span<'static>> {
    // 970 owner requirement 2 — REAL amplitude wins whenever a mic is
    // feeding us. The old test compared the ring's peak against
    // `LISTENING_SIGNAL_MIN` and fell back to the synthesized sweep
    // whenever it dipped, so every pause between words snapped the bars
    // from live audio to a canned animation that only advanced on the
    // 600 ms phase tick — which is exactly the "frozen/late" the owner
    // saw. Once fed, the ring IS the display: a quiet passage draws
    // quiet, and the bars track the voice at the capture cadence.
    //
    // The synthesized sweep now means one honest thing: no capture path
    // has fed a level at all, so the row animates rather than sitting as
    // a dead flat line while the engine opens the mic.
    let plain = model.talk.wave_plain;
    let style_for = |hot: bool| {
        if hot {
            theme.gold_style()
        } else {
            theme.faint_style()
        }
    };
    // Allocation: ONE `Vec<Span>` of `WAVE_WIDTH` borrowed `&'static str`
    // symbols. No per-cell `String`, no intermediate cell buffer — this
    // runs on every listening frame (up to 30/s).
    let mut spans = Vec::with_capacity(crate::talk::WAVE_WIDTH);
    if model.talk.wave.fed() {
        spans.extend(model.talk.wave.cells_iter().map(|cell| {
            Span::styled(
                crate::talk::wave_glyph_str(cell, plain),
                style_for(cell.hot),
            )
        }));
    } else {
        spans.extend(
            crate::talk::listening_pulse_cells(model.clock_ms)
                .into_iter()
                .map(|cell| {
                    Span::styled(
                        crate::talk::wave_glyph_str(cell, plain),
                        style_for(cell.hot),
                    )
                }),
        );
    }
    spans
}

/// T2 — the `/talk` setup card's band lines. Handed STATES, never the
/// key: the key field renders a capped mask length (the login-card law).
fn talk_setup_lines(card: &crate::talk::TalkSetupCard, theme: &Theme) -> Vec<Line<'static>> {
    use crate::talk::{KeyStage, RuntimeRowState, SetupStage, WhisperRowState};
    const MASK_CAP: usize = 32;
    let mut lines = Vec::new();
    let stage_label = match card.stage {
        SetupStage::Engine => "engine",
        SetupStage::Local => "local whisper",
        SetupStage::DeepgramKey => "deepgram · API key",
        SetupStage::DeepgramModels => "deepgram · model",
        SetupStage::Language => "deepgram · language",
    };
    lines.push(Line::from(vec![Span::styled(
        format!("  ◉ talk setup — {stage_label}"),
        theme.gold_style(),
    )]));
    let marker = |selected: bool| {
        if selected {
            Span::styled("  ❯ ".to_owned(), theme.gold_style())
        } else {
            Span::raw("    ")
        }
    };
    let row_ink = |selected: bool| {
        if selected {
            theme.bright_style()
        } else {
            theme.text_style()
        }
    };
    match card.stage {
        SetupStage::Engine => {
            let rows = [
                "[1] local whisper — on-device, shares the Diff Forge model dir",
                "[2] deepgram — cloud streaming with your API key",
            ];
            for (index, row) in rows.iter().enumerate() {
                let selected = card.selection == index;
                lines.push(Line::from(vec![
                    marker(selected),
                    Span::styled((*row).to_owned(), row_ink(selected)),
                ]));
            }
        }
        SetupStage::Local => {
            if card.loaded {
                for (index, row) in card.whisper.iter().enumerate() {
                    let selected = card.selection == index;
                    let (state_text, state_style) = match &row.state {
                        WhisperRowState::Installed => ("✓ installed".to_owned(), theme.ok_style()),
                        WhisperRowState::Absent => ("⏎ download".to_owned(), theme.dim_style()),
                        WhisperRowState::Downloading { percent } => (
                            percent.map_or_else(
                                || "↓ downloading…".to_owned(),
                                |value| format!("↓ {value}%"),
                            ),
                            theme.gold_style(),
                        ),
                        WhisperRowState::Failed(message) => {
                            (format!("✗ {message}"), theme.err_style())
                        }
                    };
                    lines.push(Line::from(vec![
                        marker(selected),
                        Span::styled(format!("{} · {}  ", row.id, row.detail), row_ink(selected)),
                        Span::styled(state_text, state_style),
                    ]));
                }
                let runtime_selected = card.selection == card.whisper.len();
                let (runtime_text, runtime_style) = match &card.runtime {
                    RuntimeRowState::Unknown => ("checking…".to_owned(), theme.dim_style()),
                    RuntimeRowState::Found(path) => (format!("✓ {path}"), theme.ok_style()),
                    RuntimeRowState::Missing(hint) => (
                        format!("✗ missing — ⏎ install · {hint}"),
                        theme.warn_style(),
                    ),
                    RuntimeRowState::Installing => ("↓ installing…".to_owned(), theme.gold_style()),
                    RuntimeRowState::Failed(message) => (format!("✗ {message}"), theme.err_style()),
                };
                lines.push(Line::from(vec![
                    marker(runtime_selected),
                    Span::styled("whisper-cli  ".to_owned(), row_ink(runtime_selected)),
                    Span::styled(runtime_text, runtime_style),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    "    loading the shared model dir…".to_owned(),
                    theme.dim_style(),
                )));
            }
        }
        SetupStage::DeepgramKey => {
            let (field, status): (Line<'static>, Line<'static>) = match card.key_stage {
                KeyStage::Reuse => (
                    Line::from(vec![
                        Span::styled("    key ❯ ".to_owned(), theme.gold_style()),
                        Span::styled("●●●●●●●● (vaulted)".to_owned(), theme.dim_style()),
                    ]),
                    Line::from(Span::styled(
                        "    ⏎ reuse the vaulted key · r retype".to_owned(),
                        theme.dim_style(),
                    )),
                ),
                KeyStage::Entry => (
                    Line::from(vec![
                        Span::styled("    key ❯ ".to_owned(), theme.gold_style()),
                        Span::styled(
                            "●".repeat(card.masked_len().min(MASK_CAP)),
                            theme.bright_style(),
                        ),
                        Span::styled("▏".to_owned(), theme.gold_style()),
                    ]),
                    Line::from(Span::styled(
                        "    paste your Deepgram API key · ⏎ validates against /v1/auth/token"
                            .to_owned(),
                        theme.dim_style(),
                    )),
                ),
                KeyStage::Validating => (
                    Line::from(Span::styled(
                        "    key ❯ ●●●●●●●●".to_owned(),
                        theme.dim_style(),
                    )),
                    Line::from(Span::styled(
                        "    validating the key + fetching streaming models…".to_owned(),
                        theme.gold_style(),
                    )),
                ),
                KeyStage::Storing => (
                    Line::from(Span::styled(
                        "    key ❯ ●●●●●●●●".to_owned(),
                        theme.dim_style(),
                    )),
                    Line::from(Span::styled(
                        "    key accepted — vaulting it in the daemon…".to_owned(),
                        theme.gold_style(),
                    )),
                ),
            };
            lines.push(field);
            lines.push(status);
        }
        SetupStage::DeepgramModels => {
            if card.models.is_empty() {
                lines.push(Line::from(Span::styled(
                    "    no streaming models fetched".to_owned(),
                    theme.dim_style(),
                )));
            } else {
                // Window of 5 rows following the selection.
                let window = 5usize;
                let skip = card.selection.saturating_sub(window - 1);
                for (index, model_row) in card.models.iter().enumerate().skip(skip).take(window) {
                    let selected = card.selection == index;
                    lines.push(Line::from(vec![
                        marker(selected),
                        Span::styled(model_row.name.clone(), row_ink(selected)),
                        Span::styled(format!("  {}", model_row.languages), theme.dim_style()),
                    ]));
                }
            }
        }
        SetupStage::Language => {
            lines.push(Line::from(vec![
                Span::styled("    language ❯ ".to_owned(), theme.gold_style()),
                Span::styled(card.language.clone(), theme.bright_style()),
                Span::styled("▏".to_owned(), theme.gold_style()),
            ]));
        }
    }
    if let Some(error) = &card.error {
        lines.push(Line::from(Span::styled(
            format!("    ⚠ {error}"),
            theme.err_style(),
        )));
    }
    lines.push(Line::from(Span::styled(
        match card.stage {
            SetupStage::Engine => "  esc close · ↑↓ or 1-2 pick · ⏎ select",
            SetupStage::Local => "  esc close · ↑↓ pick · ⏎ use / download",
            SetupStage::DeepgramKey => "  esc close · paste + ⏎",
            SetupStage::DeepgramModels => "  esc close · ↑↓ pick · ⏎ select",
            SetupStage::Language => "  esc close · e.g. en, en-US, multi · ⏎ save",
        }
        .to_owned(),
        theme.faint_style(),
    )));
    lines
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
        // P1 MASK LAW: the committed identity rides the card MASKED-
        // ALWAYS (one authority — `mask_identity`). The card is transient
        // confirmation chrome whose keys belong to the alias/key fields,
        // so it carries no reveal of its own; the durable, revealable
        // surface is the `/accounts` row the refresh lands.
        LoginStage::Done(identity) => {
            format!("  ✓ signed in · {}", crate::format::mask_identity(identity))
        }
    };
    let hint = match &card.stage {
        LoginStage::Entry => {
            "    the key is masked · stored only in the daemon vault · tab field · ⏎ commit · esc cancel"
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
    // T2: the `/talk` setup card owns the band the same way — no click
    // windows, no talk chip (its key field is a secret surface, exactly
    // the login card's reasoning).
    if let Some(card) = model.talk_setup.as_ref() {
        let mut lines = talk_setup_lines(card, theme);
        lines.truncate(usize::from(allocated).max(1));
        return (lines, None, Vec::new());
    }
    // W8b command mode: a draft opening with `!` IS the shell escape on a
    // live session — the sigil flips to the `$` the transcript will render
    // this row with, so the promise and the record share one glyph. Demo
    // mode keeps `❯` (the escape only flashes a notice there).
    let bang_mode = model.screen == Screen::Session
        && !model.mode.fabricates_locally()
        && model.composer.text().starts_with('!');
    let sigil = Span::styled(
        if bang_mode { "$ " } else { "❯ " },
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
        // 970 owner requirement 2: the `◉ listening…` indicator blinks at
        // its OWN steady ~1 Hz, decoupled from the audio frames. The
        // shared `anim_phase` counter is a 600 ms COUNTER (a 1.2 s cycle
        // that drifts with whatever else armed the tick), so the blink
        // reads the wall clock directly instead — a burst of envelopes or
        // a silent mic leaves the cadence identical.
        let live = if crate::talk::listening_blink_on(model.clock_ms) {
            theme.pulse_ink(theme.maroon, 0)
        } else {
            theme.pulse_ink(theme.maroon, 1)
        };
        (live, live)
    } else if model.hovered == Some(Hit::TalkChip) {
        (theme.gold_style(), theme.gold_style())
    } else {
        (theme.frame_style(), theme.gold_style())
    };
    // T2: the label follows the live phase (`◉ transcribing…` while the
    // engine assembles); demo listening keeps the sim wording through the
    // same method.
    let talk_label = model.talk_chip_label();
    let chip_spans = chip_two_tone(talk_label.to_owned(), talk_chrome, talk_ink);
    let chip_width = Line::from(chip_spans.clone()).width();
    // Right-aligned talk chip when the row leaves room (2-col right pad).
    // Hidden on the subagent and aura screens (sim §4.1 — aura has its own
    // hold-to-talk button).
    let talk_here = matches!(model.screen, Screen::Session | Screen::Launcher);
    // T2 — the right-to-left wave rides the SAME first row, directly left
    // of the chip, while audio flows. Fixed WAVE_WIDTH cells + one
    // separating space; on a band too narrow for both, the wave yields
    // and the chip keeps its original fit.
    let wave_spans: Option<Vec<Span<'static>>> = (model.screen == Screen::Session
        && model.talk.wave_active())
    .then(|| talk_wave_spans(model, theme));
    let wave_cols = wave_spans
        .as_ref()
        .map_or(0, |_| crate::talk::WAVE_WIDTH + 1);
    let chip_fit = |spans: &mut Vec<Span<'a>>| -> Option<(u16, u16)> {
        if !talk_here {
            return None;
        }
        let used = Line::from(spans.clone()).width();
        let with_wave = used + chip_width + COMPOSER_PAD + wave_cols;
        if wave_cols > 0 && (width as usize) > with_wave {
            let filler = width as usize - with_wave;
            spans.push(Span::raw(" ".repeat(filler)));
            if let Some(wave) = wave_spans.clone() {
                spans.extend(wave);
            }
            spans.push(Span::raw(" "));
            spans.extend(chip_spans.clone());
            return Some((
                u16::try_from(used + filler + wave_cols).unwrap_or(0),
                u16::try_from(chip_width).unwrap_or(0),
            ));
        }
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
            // T2: while talk is engaged the gesture contract replaces the
            // long copy (and leaves the first row room for wave + chip).
            Screen::Session if model.talk.engaged() => PLACEHOLDER_TALK.to_owned(),
            Screen::Launcher => PLACEHOLDER_LAUNCHER.to_owned(),
            // Sim SubComposer placeholder (tui.js:3430-3483).
            Screen::Subagent => model.viewed_chip().map_or_else(
                || PLACEHOLDER_SESSION.to_owned(),
                |chip| format!("message {} — steer this subagent · ⏎ send", chip.callsign),
            ),
            // Sim aura composer placeholder (tui.js:3508-3586), verbatim.
            Screen::Aura => {
                "speak or type — e.g. “spin up billing-service locally and run its tests”"
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
    // QoL pill: the paste placeholders wear the sim's `.ptoken` gold
    // ground in the draft, so the atomic chip READS as a chip.
    let pills = model.composer.pill_ranges();
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
        composer_row_spans(&mut spans, text, *row, cursor, selection, &pills, theme);
        if skip + index == cursor_row_index {
            // Inline ghost completion (sim `.ghostline`, tui.js:3028-3034)
            // — it rides the CARET'S visual row (an overlong palette query
            // wraps like any draft, so this is not always row 0).
            if let Some(ghost) = model.ghost() {
                spans.push(Span::styled(ghost, theme.dim_style()));
                spans.push(Span::styled(" ⇥ tab", theme.faint_style()));
            } else if bang_mode && text[1..].trim().is_empty() {
                // Command mode, command still empty: say where it runs
                // before the first keystroke commits the intent.
                spans.push(Span::styled(" workspace shell · ⏎ run", theme.dim_style()));
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
    pills: &[(usize, usize)],
    theme: &Theme,
) {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let visible = &text[row.start..row.end];
    // QoL pill ground — the transcript token treatment (`.ptoken`,
    // tui.js:4480-4486) on the draft's atomic placeholders. Cursor and
    // selection still win: the caret cell and the band stay distinct on
    // a pill's edges.
    let pill_style = theme.gold_style().bg(theme.gold_soft.into());
    let mut run = String::new();
    let mut run_style = theme.bright_style();
    for (grapheme_offset, grapheme) in visible.grapheme_indices(true) {
        let abs = row.start + grapheme_offset;
        let style = if abs == cursor {
            theme.cursor_style()
        } else if selection.is_some_and(|(start, end)| abs >= start && abs < end) {
            theme.composer_selection_style()
        } else if pills.iter().any(|&(start, end)| abs >= start && abs < end) {
            pill_style
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
        // W-C M1: custom commands wear a `✎` marker in the gutter so they read
        // as the user's, not core — the built-ins keep the plain two-space
        // gutter (identical column alignment either way).
        let gutter = if item.is_custom() { "✎ " } else { "  " };
        let mut spans = vec![
            Span::styled(gutter, name_style),
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

/// Compact durable-prompt chooser above the composer. Each prompt is one
/// physical line: journal bytes are preserved in the model and flattened
/// only for display, so Enter can load the exact original text.
fn backtrack_block(model: &AppModel, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    const MAX_ROWS: usize = 8;
    let Some(chooser) = model.backtrack else {
        return Vec::new();
    };
    let width = usize::from(width);
    let mut lines = vec![Line::styled("─".repeat(width), theme.frame_style())];
    let start = chooser.selection.saturating_add(1).saturating_sub(MAX_ROWS);
    for (index, prompt) in model
        .prompt_history
        .iter()
        .enumerate()
        .skip(start)
        .take(MAX_ROWS)
    {
        let one_line = prompt.text.replace(['\r', '\n'], " ");
        let number = format!(" {:>2}. ", index + 1);
        let shown = ellipsize(&one_line, width.saturating_sub(number.chars().count()));
        let selected = index == chooser.selection;
        let style = if selected {
            theme.selection_style()
        } else {
            theme.text_style()
        };
        let mut line = Line::from(vec![
            Span::styled(number, theme.gold_style()),
            Span::styled(shown, style),
        ])
        .style(style);
        let pad = width.saturating_sub(line.width());
        if pad > 0 {
            line.push_span(Span::raw(" ".repeat(pad)));
        }
        lines.push(line);
    }
    // `f fork` appears only where a fork could actually be issued — the
    // demo twin's recalled prompts carry no durable cut and the hint must
    // not offer a verb that will only refuse.
    let hint = if model.prompt_fork_offered() {
        "↑↓ / digits choose · ⏎ load · f fork into a new session · esc older / close"
    } else {
        "↑↓ / digits choose · ⏎ load · esc older / close"
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(hint, theme.faint_style()),
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

/// The `/help` overlay: prose plus command rows from the shared catalog.
fn render_help(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let mut lines = vec![Line::from(vec![
        Span::styled("help", theme.gold_style()),
        Span::styled("  esc closes", theme.faint_style()),
    ])];
    for entry in HELP_INTRO_TEXT {
        lines.push(Line::styled(*entry, theme.dim_style()));
    }
    for entry in help_catalog_lines(&model.dynamic_slots()) {
        lines.push(Line::styled(entry, theme.dim_style()));
    }
    // W-C M1: user-loaded custom commands are listed under their OWN heading,
    // visually distinct from the built-ins (the maroon `✎ /name` marker), so
    // a dropped-in `.haider/commands` file never reads as core.
    if !model.custom_commands.is_empty() {
        lines.push(Line::styled(
            "custom — from .haider/commands:",
            theme.gold_style(),
        ));
        for command in &model.custom_commands {
            let hint = command
                .argument_hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(format!("  ✎ /{}{hint}", command.name), theme.maroon_style()),
                Span::styled(format!("  {}", command.palette_desc()), theme.dim_style()),
            ]));
        }
    }
    let height = u16::try_from(lines.len() + 1).unwrap_or(area.height);
    let [_, panel] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height.min(area.height)),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.text_style().bg(theme.bar_bg.into())),
        panel,
    );
}

/// Floating terminal-registry details. This reuses the existing body overlay
/// layer and status strip; it never allocates new top-level chrome.
fn render_shells_overlay(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    hits.clear();
    let mut close_offsets = Vec::new();
    let mut lines = vec![Line::from(vec![
        Span::styled("shells", theme.gold_style()),
        Span::styled("  ↑↓ select · enter/close · esc", theme.faint_style()),
    ])];
    if model.shells.is_empty() {
        lines.push(Line::styled("  no terminal sessions", theme.dim_style()));
    } else {
        for (index, shell) in model.shells.iter().enumerate() {
            let marker = if index == model.shells_cursor {
                "›"
            } else {
                " "
            };
            let kind = match &shell.kind {
                haider_rpc::ShellKindWire::Local => "local".to_owned(),
                haider_rpc::ShellKindWire::Ssh { profile } => format!("ssh:{profile}"),
            };
            let status = match &shell.status {
                haider_rpc::ShellStatusWire::Starting => "starting".to_owned(),
                haider_rpc::ShellStatusWire::Running => "running".to_owned(),
                haider_rpc::ShellStatusWire::Exited { code } => {
                    code.map_or_else(|| "exited".to_owned(), |code| format!("exit {code}"))
                }
                haider_rpc::ShellStatusWire::Closed => "closed".to_owned(),
            };
            let line = Line::from(vec![
                Span::styled(format!("{marker} {kind} · {status} · "), theme.dim_style()),
                Span::styled(shell.title.clone(), theme.text_style()),
                Span::styled(format!(" · {}  ", shell.cwd_or_host), theme.dim_style()),
                Span::styled("[close]", theme.maroon_style()),
            ]);
            close_offsets.push((
                line.width().saturating_sub("[close]".len()),
                shell.id.clone(),
            ));
            lines.push(line);
        }
    }
    let height = u16::try_from(lines.len() + 1).unwrap_or(area.height);
    let [_, panel] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height.min(area.height)),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.text_style().bg(theme.bar_bg.into())),
        panel,
    );
    for (index, (offset, shell_id)) in close_offsets.into_iter().enumerate() {
        let y = panel
            .y
            .saturating_add(1 + u16::try_from(index).unwrap_or(u16::MAX));
        let Ok(offset) = u16::try_from(offset) else {
            continue;
        };
        let x = panel.x.saturating_add(offset);
        let panel_end = panel.x.saturating_add(panel.width);
        if y < panel.y.saturating_add(panel.height) && x < panel_end {
            hits.push((
                Rect::new(x, y, 7_u16.min(panel_end.saturating_sub(x)), 1),
                Hit::ShellClose(shell_id),
            ));
        }
    }
}

/// Saved profile list in the existing body-overlay layer. Only public target
/// metadata is available here; authentication details cannot reach the TUI.
fn render_ssh_overlay(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    hits.clear();
    if let Some(form) = model.ssh_form.as_ref() {
        render_ssh_form(form, theme, frame, area);
        return;
    }
    let mut lines = vec![Line::from(vec![
        Span::styled("ssh profiles", theme.gold_style()),
        Span::styled(
            "  ↑↓ select · enter shell · a add · e edit · t test · d,d remove · esc",
            theme.faint_style(),
        ),
    ])];
    if model.ssh_profiles.is_empty() {
        lines.push(Line::styled("  no SSH profiles", theme.dim_style()));
    } else {
        for (index, profile) in model.ssh_profiles.iter().enumerate() {
            let marker = if index == model.ssh_cursor {
                "›"
            } else {
                " "
            };
            let description = profile.description.as_deref().unwrap_or("no description");
            let last_used = profile
                .last_used_ms
                .map_or_else(|| "never".to_owned(), |used| used.to_string());
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} {} · ", profile.name), theme.dim_style()),
                Span::styled(
                    format!("{}@{}:{}", profile.user, profile.host, profile.port),
                    theme.text_style(),
                ),
                Span::styled(
                    format!(
                        " · {} · {description} · last {last_used} · multiplexing",
                        if profile.in_scope {
                            "in scope"
                        } else {
                            "out of scope"
                        }
                    ),
                    theme.faint_style(),
                ),
            ]));
        }
    }
    if let Some(profile) = model.ssh_remove_armed.as_deref() {
        lines.push(Line::styled(
            format!("  remove {profile}? press d again to confirm"),
            Style::default().fg(theme.err.into()),
        ));
    }
    let height = u16::try_from(lines.len() + 1).unwrap_or(area.height);
    let [_, panel] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height.min(area.height)),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.text_style().bg(theme.bar_bg.into())),
        panel,
    );
}

fn render_ssh_form(
    form: &crate::app::SshProfileForm,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let marker = |index| if form.focus == index { "›" } else { " " };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            if form.original.is_some() {
                "edit SSH profile"
            } else {
                "add SSH profile"
            },
            theme.gold_style(),
        ),
        Span::styled(
            "  tab fields · ←→ auth · ⌃S save · esc",
            theme.faint_style(),
        ),
    ])];
    let fields = [
        ("name", form.name.clone()),
        ("description", form.description.clone()),
        ("host", form.host.clone()),
        ("user", form.user.clone()),
        ("port", form.port.clone()),
        ("auth", form.auth_label().to_owned()),
        ("key path / secret", form.credential_display()),
        ("cwd", form.cwd.clone()),
    ];
    for (index, (label, value)) in fields.into_iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("{} {label:17} ", marker(index)), theme.dim_style()),
            Span::styled(value, theme.text_style()),
        ]));
    }
    lines.push(Line::styled(
        "  key files stay outside the vault; pasted keys/passwords match API-key FileVault protection (Windows inherits the profile ACL); no Haider encryption",
        theme.faint_style(),
    ));
    if let Some(error) = &form.error {
        lines.push(Line::styled(
            format!("  {error}"),
            Style::default().fg(theme.err.into()),
        ));
    }
    let height = u16::try_from(lines.len() + 1).unwrap_or(area.height);
    let [_, panel] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height.min(area.height)),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.text_style().bg(theme.bar_bg.into())),
        panel,
    );
}

/// Compact, non-modal explanation for the persistent lockdown status chip.
fn render_lockdown_overlay(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let status = model.lockdown_status.as_ref();
    let provider = status
        .and_then(|status| status.provider.as_deref())
        .unwrap_or(model.identity.provider.as_str());
    let (used, limit) = status.map_or((0, 0), |status| (status.quota_used, status.quota_limit));
    let allowed = "workspace/sandbox read · redacted search · web · text/plan · sandbox write";
    let lines = vec![
        Line::from(vec![
            Span::styled("🔒 provider lockdown", theme.gold_style()),
            Span::styled("  any key closes", theme.faint_style()),
        ]),
        Line::styled(format!("provider  {provider}"), theme.bright_style()),
        Line::styled(
            "denied  shell/SSH · peer send · hooks/MCP · monitors · checkpoints · external writes",
            theme.dim_style(),
        ),
        Line::styled(format!("available  {allowed}"), theme.dim_style()),
        Line::styled(
            format!("global quota  {} / {}", fmt_bytes(used), fmt_bytes(limit)),
            theme.text_style(),
        ),
    ];
    let panel_width = area.width.min(92);
    let panel_height = area
        .height
        .min(u16::try_from(lines.len() + 2).unwrap_or(area.height));
    let [_, centered, _] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(panel_height),
        Constraint::Min(0),
    ])
    .areas(area);
    let [_, panel, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(panel_width),
        Constraint::Min(0),
    ])
    .areas(centered);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().style(theme.frame_style()))
            .wrap(Wrap { trim: true })
            .style(theme.text_style().bg(theme.bar_bg.into())),
        panel,
    );
}

fn render_ssh_terminal(model: &AppModel, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let Some(terminal) = model.ssh_terminal.as_ref() else {
        return;
    };
    let title = Line::from(vec![
        Span::styled(format!("ssh {}", terminal.profile), theme.gold_style()),
        Span::styled("  interactive PTY · ⌃] close", theme.faint_style()),
    ]);
    let body = sanitize_terminal_text(&terminal.display_text());
    let [title_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(title).style(theme.text_style().bg(theme.bg.into())),
        title_area,
    );
    let lines = body.lines().map(Line::raw).collect::<Vec<_>>();
    let scroll = u16::try_from(lines.len().saturating_sub(usize::from(body_area.height)))
        .unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(Text::from(lines))
        .style(theme.text_style().bg(theme.bg.into()))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, body_area);
}

fn sanitize_terminal_text(input: &str) -> String {
    enum Escape {
        Ground,
        Esc,
        Csi,
        Osc,
    }
    let mut state = Escape::Ground;
    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        match state {
            Escape::Ground => match character {
                '\u{1b}' => state = Escape::Esc,
                '\r' => {}
                '\n' | '\t' => output.push(character),
                value if !value.is_control() => output.push(value),
                _ => {}
            },
            Escape::Esc => match character {
                '[' => state = Escape::Csi,
                ']' => state = Escape::Osc,
                _ => state = Escape::Ground,
            },
            Escape::Csi => {
                if ('@'..='~').contains(&character) {
                    state = Escape::Ground;
                }
            }
            Escape::Osc => {
                if character == '\u{7}' {
                    state = Escape::Ground;
                } else if character == '\u{1b}' {
                    state = Escape::Esc;
                }
            }
        }
    }
    output
}

/// The ink one monitor state chip wears. `firing` pulses on the shared
/// clock — it is the one transient state, and the row should read as live.
fn monitor_state_style(
    theme: &Theme,
    state: haider_rpc::MonitorStateWire,
    anim_phase: u8,
) -> ratatui::style::Style {
    match state {
        haider_rpc::MonitorStateWire::Armed => theme.ok_style(),
        haider_rpc::MonitorStateWire::Paused => theme.dim_style(),
        haider_rpc::MonitorStateWire::Firing => theme.pulse_ink(theme.maroon, anim_phase),
        haider_rpc::MonitorStateWire::Exited => theme.faint_style(),
    }
}

/// `2m 5s ago` / `in 45s` against the model clock. A timestamp the clock has
/// not reached yet reads as `now` rather than underflowing into nonsense.
fn monitor_when(clock_ms: u64, at_ms: u64, future: bool) -> String {
    if future {
        if at_ms <= clock_ms {
            "now".to_owned()
        } else {
            format!("in {}", fmt_elapsed(at_ms - clock_ms))
        }
    } else if at_ms >= clock_ms {
        "now".to_owned()
    } else {
        format!("{} ago", fmt_elapsed(clock_ms - at_ms))
    }
}

/// `/monitors` — the existing monitor registry as ACTIONABLE rows (970
/// owner item 2). Each row states what it watches, what state it is in,
/// what it last saw and when it fires next; the SELECTED row also carries
/// its action strip. Reuses the shared body-overlay layer.
fn render_monitors_overlay(
    model: &AppModel,
    theme: &Theme,
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    hits.clear();
    // Row-relative click targets, resolved to absolute coordinates once the
    // panel's own rect is known (the `render_shells_overlay` discipline).
    let mut row_targets: Vec<(usize, usize, usize, Hit)> = Vec::new();
    let mut lines = vec![Line::from(vec![
        Span::styled("monitors", theme.gold_style()),
        Span::styled(
            "  ↑↓/jk select · x stop · p pause · t trigger · e edit · y copy id · esc",
            theme.faint_style(),
        ),
    ])];
    if model.monitors.is_empty() {
        lines.push(Line::styled("  no active monitors", theme.dim_style()));
    } else {
        for (index, monitor) in model.monitors.iter().enumerate() {
            let selected = index == model.monitors_cursor;
            let marker = if selected { "›" } else { " " };
            let state = model.monitor_row_state(monitor);
            let chip = crate::app::monitor_state_chip(state);
            let head = format!(" {marker} {} · ", monitor.monitor_id);
            let source = crate::app::monitor_source_summary(monitor);
            let mut spans = vec![
                Span::styled(head.clone(), theme.dim_style()),
                Span::styled(source.clone(), theme.text_style()),
                Span::styled("  ", theme.dim_style()),
                Span::styled(
                    format!("[{chip}]"),
                    monitor_state_style(theme, state, model.anim_phase),
                ),
            ];
            // The whole row is the select target — the id travels in the
            // hit, never the ordinal.
            let row_width = Line::from(spans.clone()).width();
            row_targets.push((
                lines.len(),
                0,
                row_width.max(1),
                Hit::MonitorRow(monitor.monitor_id.clone()),
            ));
            if selected {
                spans = spans
                    .into_iter()
                    .map(|span| span.patch_style(theme.hover_style()))
                    .collect();
            }
            lines.push(Line::from(spans));

            // Detail row: fire count, last event, next fire — the facts the
            // daemon actually sent, never invented.
            let mut detail = vec![format!("fired {}×", monitor.fire_count)];
            if let Some(last) = monitor.last_event.as_ref() {
                let when = monitor_when(model.clock_ms, last.at_ms, false);
                if last.summary.trim().is_empty() {
                    detail.push(format!("last {when}"));
                } else {
                    detail.push(format!("last {when} — {}", last.summary));
                }
            }
            if let Some(next) = monitor.next_fire_at_ms {
                detail.push(format!("next {}", monitor_when(model.clock_ms, next, true)));
            }
            lines.push(Line::styled(
                format!("     {}", detail.join(" · ")),
                theme.faint_style(),
            ));

            // The action strip belongs to the SELECTED row only — five
            // targets, each a separate rect so a click can never mean two
            // things at once.
            if selected {
                let pause_label = if matches!(state, haider_rpc::MonitorStateWire::Paused) {
                    "[resume]"
                } else {
                    "[pause]"
                };
                let armed =
                    model.monitors_stop_armed.as_deref() == Some(monitor.monitor_id.as_str());
                let stop_label = if armed { "[stop again]" } else { "[stop]" };
                let actions: [(&str, Hit); 5] = [
                    (stop_label, Hit::MonitorStop(monitor.monitor_id.clone())),
                    (pause_label, Hit::MonitorPause(monitor.monitor_id.clone())),
                    (
                        "[trigger now]",
                        Hit::MonitorTrigger(monitor.monitor_id.clone()),
                    ),
                    (
                        "[edit with agent]",
                        Hit::MonitorEdit(monitor.monitor_id.clone()),
                    ),
                    ("[copy id]", Hit::MonitorCopyId(monitor.monitor_id.clone())),
                ];
                let mut spans = vec![Span::styled("     ", theme.dim_style())];
                let mut cursor = 5_usize;
                for (label, hit) in actions {
                    let width = label.chars().count();
                    row_targets.push((lines.len(), cursor, width, hit.clone()));
                    let style = if matches!(hit, Hit::MonitorStop(_)) {
                        theme.maroon_style()
                    } else {
                        theme.gold_style()
                    };
                    spans.push(Span::styled(label.to_owned(), style));
                    spans.push(Span::raw(" "));
                    cursor += width + 1;
                }
                lines.push(Line::from(spans));
            }
        }
    }
    let height = u16::try_from(lines.len() + 1).unwrap_or(area.height);
    let [_, panel] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height.min(area.height)),
    ])
    .areas(area);
    // The overlay OWNS its rows. A `Paragraph`'s style only recolours the
    // cells underneath, so without this the composer band beneath showed
    // THROUGH every row shorter than the panel — the taller a monitor list
    // grew, the more of the band it wore.
    frame.render_widget(ratatui::widgets::Clear, panel);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(theme.text_style().bg(theme.bar_bg.into())),
        panel,
    );
    // Row-relative targets become absolute now that the panel is placed.
    // Action targets are pushed BEFORE the row-select rect that contains
    // them: `hit_rect_at` takes the FIRST containing rect, so a click on
    // `[stop]` must meet the stop rect before the row's own.
    let panel_end_x = panel.x.saturating_add(panel.width);
    let panel_end_y = panel.y.saturating_add(panel.height);
    let mut ordered = row_targets;
    ordered.sort_by_key(|(_, _, _, hit)| u8::from(matches!(hit, Hit::MonitorRow(_))));
    for (row, offset, width, hit) in ordered {
        let (Ok(row), Ok(offset), Ok(width)) = (
            u16::try_from(row),
            u16::try_from(offset),
            u16::try_from(width),
        ) else {
            continue;
        };
        let y = panel.y.saturating_add(row);
        let x = panel.x.saturating_add(offset);
        if y < panel_end_y && x < panel_end_x {
            hits.push((
                Rect::new(x, y, width.min(panel_end_x.saturating_sub(x)), 1),
                hit,
            ));
        }
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;
    if bytes >= GIB {
        format!("{}.{:02} GiB", bytes / GIB, (bytes % GIB) * 100 / GIB)
    } else if bytes >= MIB {
        format!("{}.{:01} MiB", bytes / MIB, (bytes % MIB) * 10 / MIB)
    } else if bytes >= KIB {
        format!("{}.{:01} KiB", bytes / KIB, (bytes % KIB) * 10 / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// One text run of the status bar's bottom-left strip: the content and
/// the tone the renderer styles it with. The TEXT is the shared truth
/// (`status_segment_v1`); the tone is display-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSegment {
    pub text: String,
    pub tone: StatusSegmentTone,
    /// Structured semantics carried by the state badge, when this run is
    /// the one clients should render instead of parsing display text.
    pub state: Option<String>,
    pub detail: Option<String>,
}

/// The strip's style vocabulary — resolved to real styles only inside
/// [`render_status_bar`] (pulse phase included), so the segment composer
/// stays pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSegmentTone {
    Text,
    /// The state chip's `[ ` / ` ]` chrome (pulses with the badge).
    BadgeChrome,
    /// The state word itself.
    Badge,
    Dim,
    /// The H4 decision-hook chip's chrome / label.
    HookChrome,
    Hook,
}

/// The status bar's bottom-LEFT strip — ONE pure composition over
/// (model, width) that BOTH the frame and the W-INP status mirror
/// consume (`status_segment_v1`): the renderer styles these segments,
/// the mirror publishes [`status_left_string`], so screen and mirror can
/// never diverge. Width matters: the meter and cache summaries yield
/// exactly as they do on screen.
#[must_use]
pub fn status_left_segments(model: &AppModel, width: u16) -> Vec<StatusSegment> {
    // The derived WAITING-on-subagents badge overlays plain IDLE (§2.6).
    let (badge, _) = model.status_badge();
    let (state, detail) = model.status_badge_state_detail();
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

    // F2c: token usage sits DIRECTLY right of the state — the identity
    // block (model / auth / reasoning) moved to the composer's top rule.
    // Narrow dignity: the meter YIELDS whole when the bar cannot hold it
    // beside the badge (the badge always survives, never clipped chrome).
    let mut badge_cells = 1 + badge.chars().count() + 4;
    let mut segments = vec![
        StatusSegment {
            text: " ".to_owned(),
            tone: StatusSegmentTone::Text,
            state: None,
            detail: None,
        },
        StatusSegment {
            text: "[ ".to_owned(),
            tone: StatusSegmentTone::BadgeChrome,
            state: None,
            detail: None,
        },
        StatusSegment {
            text: badge.clone(),
            tone: StatusSegmentTone::Badge,
            state: Some(state),
            detail,
        },
        StatusSegment {
            text: " ]".to_owned(),
            tone: StatusSegmentTone::BadgeChrome,
            state: None,
            detail: None,
        },
    ];
    if let Some(provider) = model.active_lockdown_provider() {
        let text = format!("  🔒 lockdown · {provider}");
        badge_cells += text.chars().count();
        segments.push(StatusSegment {
            text,
            tone: StatusSegmentTone::Hook,
            state: None,
            detail: Some("read/search/web/text/plan plus quota-limited sandbox writes".to_owned()),
        });
    }
    if let Some(progress) = model.provider_wait_progress()
        && badge_cells + progress.chars().count() + 2 <= width as usize
    {
        badge_cells += progress.chars().count() + 2;
        segments.push(StatusSegment {
            text: format!("  {progress}"),
            tone: StatusSegmentTone::Dim,
            state: None,
            detail: None,
        });
    }
    let meter_shown = badge_cells + 2 + meter.chars().count() <= width as usize;
    if meter_shown {
        segments.push(StatusSegment {
            text: format!("  {meter}  "),
            tone: StatusSegmentTone::Dim,
            state: None,
            detail: None,
        });
    }
    if meter_shown && !model.cache_usage.is_empty() {
        let totals = model.cache_usage.totals();
        let reread_basis_points = model.main_cache_reread_hit_basis_points();
        let wide = crate::cache_usage::wide_status(&totals, reread_basis_points);
        let medium = crate::cache_usage::medium_status(&totals, reread_basis_points);
        let branch_reserve = if model.screen == Screen::Session {
            model.active_branch_name().chars().count() + 4
        } else {
            0
        };
        let used = badge_cells + 2 + meter.chars().count();
        let available = (width as usize).saturating_sub(used + branch_reserve);
        if wide.chars().count() + 2 <= available {
            segments.push(StatusSegment {
                text: format!("{wide}  "),
                tone: StatusSegmentTone::Dim,
                state: None,
                detail: None,
            });
        } else if medium.chars().count() + 2 <= available {
            segments.push(StatusSegment {
                text: format!("{medium}  "),
                tone: StatusSegmentTone::Dim,
                state: None,
                detail: None,
            });
        }
    }
    // The branch name inside a session, plus ` · q:turn` while queue mode
    // holds (tui.js:2840-2842). B2b: the ACTIVE branch's name — "main" on
    // the main branch, the daemon-named fork otherwise.
    if model.screen == Screen::Session {
        let queue_tag = if model.queue_mode {
            " · q:turn"
        } else if model.subturn_mode {
            " · q:subturn"
        } else {
            ""
        };
        segments.push(StatusSegment {
            text: format!("· {}{queue_tag}  ", model.active_branch_name()),
            tone: StatusSegmentTone::Text,
            state: None,
            detail: None,
        });
    }
    // 970 owner item 1: the shell and monitor counts LEFT this strip for
    // the band's task line (`▾ subagents — … · 2 shells · 1 monitor`, see
    // `render_subtree`). Counting them in both places WAS the owner's
    // double count, so nothing may reintroduce a segment here.
    // The voice/dictation chip moved to the TOP-RIGHT header (see
    // `voice_header_pill`), so the status bar no longer carries it.
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
        segments.push(StatusSegment {
            text: "[ ".to_owned(),
            tone: StatusSegmentTone::HookChrome,
            state: None,
            detail: None,
        });
        segments.push(StatusSegment {
            text: "⚙ hook·decided".to_owned(),
            tone: StatusSegmentTone::Hook,
            state: None,
            detail: None,
        });
        segments.push(StatusSegment {
            text: " ]".to_owned(),
            tone: StatusSegmentTone::HookChrome,
            state: None,
            detail: None,
        });
    }
    segments
}

/// The strip's display STRING, byte-exact with what the frame paints —
/// the value the W-INP status mirror publishes (`status_segment_v1`).
#[must_use]
pub fn status_left_string(model: &AppModel, width: u16) -> String {
    status_left_surface(model, width).line
}

/// The status mirror's render-compatible display line plus the state badge's
/// structured semantics. `state` and `detail` are read directly from the
/// typed badge segment — never recovered by parsing `line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLeftSurface {
    pub line: String,
    pub state: Option<String>,
    pub detail: Option<String>,
}

/// Compose the bottom-left strip once for both its byte-exact display line
/// and its additive structured state fields.
#[must_use]
pub fn status_left_surface(model: &AppModel, width: u16) -> StatusLeftSurface {
    let segments = status_left_segments(model, width);
    let (state, detail) = segments
        .iter()
        .find_map(|segment| {
            segment
                .state
                .as_ref()
                .map(|state| (Some(state.clone()), segment.detail.clone()))
        })
        .unwrap_or((None, None));
    let line = segments.into_iter().map(|segment| segment.text).collect();
    StatusLeftSurface {
        line,
        state,
        detail,
    }
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
    // The W-INP status mirror composes at the frame's exact width (the
    // scroll_max frame-feedback discipline).
    model.status_width.set(area.width);
    let (badge, tone) = model.status_badge();
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
    let left_segments = status_left_segments(model, area.width);
    let left: Vec<Span<'_>> = left_segments
        .iter()
        .map(|segment| {
            let style = match segment.tone {
                StatusSegmentTone::Text => theme.text_style(),
                StatusSegmentTone::BadgeChrome => badge_chrome,
                StatusSegmentTone::Badge => badge_ink,
                StatusSegmentTone::Dim => theme.dim_style(),
                StatusSegmentTone::HookChrome => theme.frame_style(),
                StatusSegmentTone::Hook => theme.gold_style(),
            };
            Span::styled(segment.text.clone(), style)
        })
        .collect();

    // OTA: a discovered release is durable model data, not a modal. It
    // quietly occupies the status-bar hint slot on every surface, yielding
    // only while a transient flash is speaking. The launcher help hint
    // returns automatically after a later current-version fact clears it.
    let update_hint = model
        .update_available
        .as_deref()
        .map(|version| format!("⬆ {} — /update ", update_version_label(version)));
    let hint_shown = model.flash.is_none()
        && update_hint.is_none()
        && model.screen == Screen::Launcher
        && !model.help_open;
    let right = if let Some(flash) = &model.flash {
        flash.clone()
    } else if let Some(update_hint) = update_hint {
        update_hint
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
    // 970 owner item 1: this strip carries NO clickable segment any more —
    // the shells/monitors counts (its only two) moved to the band task row.
    // Sim StatusBar (tui.js:5492-5499): TRANSPARENT ground (its frame
    // border-top has no row budget here; the dim ink carries the bar) —
    // the owner's "tan band" was our former bar_bg tint. No tinted rows
    // may bracket the composer.
    frame.render_widget(
        Paragraph::new(Line::from(left)).style(theme.text_style()),
        left_area,
    );
    if let Some(provider) = model.active_lockdown_provider() {
        let prefix = status_left_string(model, area.width);
        let marker = format!("🔒 lockdown · {provider}");
        if let Some(byte_offset) = prefix.find(&marker) {
            let x_offset = Line::from(prefix[..byte_offset].to_owned()).width();
            let marker_width = Line::from(marker).width();
            if let (Ok(x_offset), Ok(marker_width)) =
                (u16::try_from(x_offset), u16::try_from(marker_width))
            {
                hits.push((
                    Rect {
                        x: area.x.saturating_add(x_offset),
                        y: area.y,
                        width: marker_width.min(left_area.width.saturating_sub(x_offset)),
                        height: 1,
                    },
                    Hit::LockdownStatus,
                ));
            }
        }
    }
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
            from_main,
        } => {
            lines.push(Line::default());
            // Sim UserRow (tui.js:4465-4492): MAROON bold sigil (gold ❯
            // belongs to the composer/sticky only), bright pre-wrap text
            // (multi-line submits keep their newlines), gold pill paste
            // tokens. Voice rows swap the sigil for ◉ and tag ` · spoken`
            // (tui.js:3884-3890). Parent-authored rows in a chip
            // transcript (S3) swap it for → and tag ` · from main` — the
            // same boundary-crossing glyph the parent's `→ messaged`
            // marker wears.
            let sigil = if *from_main {
                "→ "
            } else if *voice {
                "◉ "
            } else {
                "❯ "
            };
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
                    if *from_main {
                        spans.push(Span::styled(" · from main", theme.faint_style()));
                    }
                }
                lines.push(Line::from(spans));
            }
        }
        TranscriptEntry::Item(block) => item_lines(lines, block, theme, width, phase),
        TranscriptEntry::Peer {
            sender,
            sender_kind,
            text,
            receipt,
            ..
        } => peer_entry_lines(lines, sender, sender_kind, text, *receipt, theme, width),
        TranscriptEntry::Note { text } => {
            // Sim NoteRow (tui.js:4572-4577): dim, indented off the margin.
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(text.as_str(), theme.dim_style()),
            ]));
        }
        TranscriptEntry::Refusal {
            provider,
            tool,
            reason,
        } => refusal_entry_line(lines, provider, tool, reason, theme),
        TranscriptEntry::Error { text, presentation } => {
            error_entry_lines(lines, text, presentation.as_ref(), theme, width);
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

fn refusal_entry_line(
    lines: &mut Vec<Line<'_>>,
    provider: &str,
    tool: &str,
    reason: &str,
    theme: &Theme,
) {
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "🔒 refused",
            theme.gold_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {provider} · {tool} — {reason}"),
            theme.dim_style(),
        ),
    ]));
}

fn peer_entry_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    sender: &'a str,
    sender_kind: &'a str,
    text: &'a str,
    receipt: Option<haider_protocol::peer::PeerDelivery>,
    theme: &Theme,
    width: u16,
) {
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("@ ", theme.gold_style().add_modifier(Modifier::BOLD)),
        Span::styled(sender, theme.bright_style().add_modifier(Modifier::BOLD)),
        Span::styled("›", theme.gold_style().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {sender_kind}"), theme.dim_style()),
        Span::styled(" · UNTRUSTED PEER INPUT", theme.maroon_style()),
    ]));
    let body_width = usize::from(width).saturating_sub(3).max(1);
    for row in wrap_body(text, body_width) {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("▏ ", theme.rail_style()),
            Span::styled(row, theme.text_style()),
        ]));
    }
    if let Some(receipt) = receipt {
        let label = match receipt {
            haider_protocol::peer::PeerDelivery::Queued => "queued",
            haider_protocol::peer::PeerDelivery::Delivered => "delivered",
            haider_protocol::peer::PeerDelivery::Expired => "expired",
            haider_protocol::peer::PeerDelivery::Refused => "refused",
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("receipt · {label}"), theme.dim_style()),
        ]));
    }
}

/// A typed failed run renders as a card-shaped block behind a severity rail:
///
/// ```text
///  ✗ Provider rate limit reached            ← err ink, BOLD (title)
///  ▏ Wait for the provider limit to reset,  ← err rail · dim detail,
///  ▏ then retry.                              wrapped by display cells
///  ▏ rate-limited · HTTP 429 · req 8f3a2c1… ← dim fact line, whole-
///                                             segment shed to width
/// ```
///
/// The fact line carries the trailing `actions: …` hint — the transcript
/// row may be the only guidance when no recovery card follows. A
/// text-only error (client-observed failures, pre-E2 wire errors) keeps
/// the baseline one-line `✗` render. The reset figure is the static
/// provider delay recorded at failure time (a transcript row is a
/// record, not a countdown).
fn error_entry_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    text: &'a str,
    presentation: Option<&'a haider_protocol::error::ErrorPresentation>,
    theme: &Theme,
    width: u16,
) {
    lines.push(Line::default());
    let Some(presentation) = presentation else {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("✗ ", theme.err_style()),
            Span::styled(text, theme.err_style()),
        ]));
        return;
    };
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("✗ ", theme.err_style()),
        Span::styled(
            presentation.title.as_str(),
            theme.err_style().add_modifier(Modifier::BOLD),
        ),
    ]));
    let budget = (width as usize).saturating_sub(3);
    if budget == 0 {
        return;
    }
    for logical in presentation.detail.split('\n') {
        for row in wrap_body(logical, budget) {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("▏ ", theme.err_style()),
                Span::styled(row, theme.dim_style()),
            ]));
        }
    }
    let facts = crate::projection::error_fact_segments_with_actions(presentation, None);
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("▏ ", theme.err_style()),
        Span::styled(shed_fact_line(&facts, budget), theme.dim_style()),
    ]));
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

/// Length of a paste token at the start of `text`, if present: the QoL
/// pill placeholder (`[Pasted text #N +K lines]`), the sim's historical
/// `[Pasted N lines]`, or `[Image #N]`.
fn paste_token_len(text: &str) -> Option<usize> {
    if let Some(body) = text.strip_prefix("[Pasted text #") {
        let n_digits = body.chars().take_while(char::is_ascii_digit).count();
        if n_digits > 0
            && let Some(rest) = body[n_digits..].strip_prefix(" +")
        {
            let k_digits = rest.chars().take_while(char::is_ascii_digit).count();
            if k_digits > 0 && rest[k_digits..].starts_with(" lines]") {
                return Some("[Pasted text #".len() + n_digits + 2 + k_digits + " lines]".len());
            }
        }
    }
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
    use unicode_segmentation::UnicodeSegmentation;
    // Grapheme clusters, not chars: an emoji (VS16/ZWJ/flag/skin-tone) stays
    // one unbreakable unit whose width matches ratatui's renderer, so it
    // never splits mid-cluster or mis-sums in the pre/code wrap path.
    let cluster_w = |c: &str| unicode_width::UnicodeWidthStr::width(c).max(1);
    let mut row = String::new();
    let mut row_width = 0usize;
    let mut clusters = line.graphemes(true).peekable();
    while let Some(&first) = clusters.peek() {
        let is_space = first == " ";
        // Collect one run (all-spaces or no-spaces).
        let mut run = String::new();
        let mut run_width = 0usize;
        while let Some(&cluster) = clusters.peek() {
            if (cluster == " ") != is_space {
                break;
            }
            clusters.next();
            run.push_str(cluster);
            run_width += cluster_w(cluster);
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
        for cluster in run.graphemes(true) {
            let cluster_width = cluster_w(cluster);
            if cluster_width > budget {
                // Unrepresentable at this width (e.g. CJK beside a rail in
                // a 3-col frame) — dropping is the only honest option that
                // keeps the no-implicit-wrap invariant.
                continue;
            }
            if row_width + cluster_width > budget {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push_str(cluster);
            row_width += cluster_width;
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
    // CU-2 computer tool: the action is a top-level `"action"` tag with
    // coordinate/text siblings. Render exactly what the model is doing to
    // the screen — the transcript is the owner's window into a session that
    // can move their real cursor.
    if let Some(action) = args.get("action").and_then(|v| v.as_str()) {
        return computer_action_desc(action, args);
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

/// Human-readable summary of one CU-2 computer action for the tool row.
fn computer_action_desc(action: &str, args: &serde_json::Value) -> String {
    let u = |key| args.get(key).and_then(serde_json::Value::as_u64);
    let xy = || match (u("x"), u("y")) {
        (Some(x), Some(y)) => format!(" ({x}, {y})"),
        _ => String::new(),
    };
    let point = |key| {
        args.get(key).map_or_else(String::new, |p| {
            match (
                p.get("x").and_then(serde_json::Value::as_u64),
                p.get("y").and_then(serde_json::Value::as_u64),
            ) {
                (Some(x), Some(y)) => format!("({x}, {y})"),
                _ => String::new(),
            }
        })
    };
    match action {
        "left_click" | "right_click" | "middle_click" | "double_click" | "mouse_move" => {
            format!("{action}{}", xy())
        }
        "left_click_drag" => format!("drag {} → {}", point("from"), point("to")),
        "type" => match args.get("text").and_then(|v| v.as_str()) {
            Some(text) => format!("type \"{text}\""),
            None => "type".to_owned(),
        },
        "key" => match args.get("keys").and_then(|v| v.as_str()) {
            Some(keys) => format!("key {keys}"),
            None => "key".to_owned(),
        },
        "scroll" => {
            let dir = args.get("direction").and_then(|v| v.as_str()).unwrap_or("");
            let amount = u("amount").unwrap_or(0);
            format!("scroll {dir} ×{amount}{}", xy())
        }
        "wait" => match u("ms") {
            Some(ms) => format!("wait {ms}ms"),
            None => "wait".to_owned(),
        },
        other => other.replace('_', " "),
    }
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
            // P2-2): it rides the last markdown line as a Cursor-kind
            // span, so its cell is accounted for by the styled walker and
            // a last row that exactly fills the budget pushes the ▮ onto
            // its own RAILED row instead of overflowing rail-less.
            //
            // F2d: the body is MARKDOWN — parsed to kind-tagged spans
            // (line-stable: one MdLine per source line; unterminated
            // spans in a streaming prefix render literally and restyle
            // in place once the closing marker arrives), wrapped by the
            // same pre-wrap walk as plain text so styling never moves a
            // break, and inked via `Theme::md_style` theme slots.
            let budget = (width as usize).saturating_sub(3);
            if budget == 0 {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled("▏ ", theme.rail_style()),
                ]));
            } else {
                let text = text.to_owned_string();
                let mut md_lines = crate::md::render_markdown(&text);
                if block.streaming
                    && let Some(tail) = md_lines.last_mut()
                {
                    tail.push_cursor();
                }
                // G5: consecutive table-tagged lines are ONE table — the
                // parser emits header/delimiter/body as a contiguous run
                // — laid out for the CURRENT budget (natural grid /
                // wrapped grid / stacked records; the mode is a pure
                // function of the budget, so a resize flips it with no
                // state). Layout rows already fit the budget: push them
                // railed, never re-wrapped.
                let push_row = |lines: &mut Vec<Line<'a>>, row: Vec<crate::md::MdSpan>| {
                    let mut spans = vec![Span::raw(" "), Span::styled("▏ ", theme.rail_style())];
                    spans.extend(
                        row.into_iter()
                            .map(|span| Span::styled(span.text, theme.md_style(span.kind))),
                    );
                    lines.push(Line::from(spans));
                };
                let mut idx = 0usize;
                while idx < md_lines.len() {
                    if md_lines[idx].table.is_some() {
                        let start = idx;
                        while idx < md_lines.len() && md_lines[idx].table.is_some() {
                            idx += 1;
                        }
                        let rows: Vec<&crate::md::MdTableRow> = md_lines[start..idx]
                            .iter()
                            .filter_map(|line| line.table.as_ref())
                            .collect();
                        for row in crate::md::layout_table(&rows, budget) {
                            push_row(lines, row);
                        }
                        continue;
                    }
                    for row in crate::md::wrap_spans(&md_lines[idx].spans, budget) {
                        push_row(lines, row);
                    }
                    idx += 1;
                }
            }
        }
        TurnItem::IncompleteAgentMessage { text, interruption } => {
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("■ haider", theme.gold_style()),
            ]));
            let budget = (width as usize).saturating_sub(3);
            if budget > 0 {
                let (mut text, truncated) = text.to_owned_prefix(EXTREME_LOGICAL_LINE_CHARS);
                if truncated {
                    text.push_str(" ⋯ /export expands raw text");
                }
                for markdown_line in crate::md::render_markdown(&text) {
                    for row in crate::md::wrap_spans(&markdown_line.spans, budget) {
                        let mut spans =
                            vec![Span::raw(" "), Span::styled("▏ ", theme.rail_style())];
                        spans.extend(
                            row.into_iter()
                                .map(|span| Span::styled(span.text, theme.md_style(span.kind))),
                        );
                        lines.push(Line::from(spans));
                    }
                }
            }
            // An interruption is warning metadata, not a failed response.
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("⚠ ", theme.warn_style()),
                Span::styled("incomplete — stream interrupted (", theme.dim_style()),
                Span::styled(interruption.subcode.as_str(), theme.dim_style()),
                Span::styled(")", theme.dim_style()),
            ]));
        }
        TurnItem::Reasoning { summary } => {
            lines.push(Line::from(vec![
                Span::styled(" · ", theme.faint_style()),
                Span::styled(
                    {
                        let (mut text, truncated) =
                            summary.to_owned_prefix(EXTREME_LOGICAL_LINE_CHARS);
                        if truncated {
                            text.push_str(" ⋯ /export expands raw text");
                        }
                        text
                    },
                    theme.dim_style(),
                ),
            ]));
        }
        TurnItem::ToolCall {
            name, status, args, ..
        } => {
            // Sim ToolRow (tui.js:3901-3908): glyph (ok / warn-running /
            // err) · MAROON name · dim ellipsized desc from the args.
            let remote_profile = if name == "ssh_shell" || name == "process_exec" {
                args.get("profile").and_then(serde_json::Value::as_str)
            } else {
                None
            };
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(
                    format!("{} ", status_glyph(*status)),
                    match status {
                        haider_protocol::item::ToolStatus::Rejected
                        | haider_protocol::item::ToolStatus::Conflict
                        | haider_protocol::item::ToolStatus::Failed
                        | haider_protocol::item::ToolStatus::Unknown => theme.err_style(),
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
                Span::styled(
                    remote_profile.map_or_else(
                        || name.clone(),
                        |profile| format!("↗ remote · {profile} · {name}"),
                    ),
                    theme.maroon_style(),
                ),
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
            if let Some(reason) = &block.tool_reason {
                let used = Line::from(spans.clone()).width();
                let budget = (width as usize).saturating_sub(used + 3);
                if budget > 0 {
                    // E8 visual pass: a reason on a COMPLETED row is a
                    // recovered in-flight retry ("transient web_fetch
                    // failure — retry 2/2 succeeded") — quiet dim
                    // metadata, never an alarming tone; only failure
                    // reasons wear the err ink.
                    spans.push(Span::styled(
                        format!(" · {}", ellipsize(reason, budget)),
                        if status == &haider_protocol::item::ToolStatus::Completed {
                            theme.dim_style()
                        } else {
                            theme.err_style()
                        },
                    ));
                }
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
            let sigil = if block.user_command { "  ! " } else { "  $ " };
            let mut spans = vec![
                Span::styled(sigil, theme.gold_style()),
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
            // Sim shape (seed rows, tui.js:480): a completed fs_edit tool
            // row — ✓ glyph · maroon name · dim path · dim +a −r meta.
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("✓ ", theme.ok_style()),
                Span::styled("fs_edit", theme.maroon_style()),
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
            tokens_estimated,
            ..
        } => {
            // Sim CompactRow (tui.js:3919-3924), gold: the additive
            // optional token counts render the sim string exactly; items
            // without numbers keep the honest count-free row.
            let text = match (tokens_before, tokens_after) {
                (Some(before), Some(after)) => format!(
                    "⊟ compacted {}{} → {}{} · summary retained · originals stay in /tree",
                    if *tokens_estimated { "~" } else { "" },
                    fmt_tok(*before),
                    if *tokens_estimated { "~" } else { "" },
                    fmt_tok(*after)
                ),
                _ => "⊟ context compacted — summary retained · originals stay in /tree".to_owned(),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(text, theme.gold_style()),
            ]));
        }
        TurnItem::Refusal { reason } => {
            lines.push(Line::from(vec![
                Span::styled("  ✗ model refused — ", theme.err_style()),
                Span::styled(reason.as_str(), theme.text_style()),
            ]));
        }
        TurnItem::Extension { kind, data } => {
            if let Some(budget) =
                haider_protocol::request_budget::RequestBudgetStatusV1::from_extension_item(
                    &block.item,
                )
            {
                lines.push(Line::styled(
                    format!("  {}", budget.summary()),
                    theme.gold_style(),
                ));
            } else if let Some((_, label)) = crate::projection::image_created_fact(kind, data) {
                let suffix = label.strip_prefix("🖼 image").unwrap_or(&label);
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("🖼 image", theme.gold_style()),
                    Span::styled(suffix.to_owned(), theme.dim_style()),
                ]));
            } else if let Some(transition) =
                haider_protocol::cache::CacheEpochTransitionV1::from_extension_item(&block.item)
            {
                lines.push(Line::styled(
                    format!("  {}", transition.display_label()),
                    theme.gold_style(),
                ));
            } else if let Some(label) = crate::projection::retry_marker_label(kind, data) {
                // E8 visual pass: a bounded in-flight retry is a QUIET
                // fact — the ⟳ renewal glyph and dim ink (readable, the
                // ≥ 3.4:1 floor — recovery in progress deserves better
                // than the barely-there faint band, and no alarm tone).
                lines.push(Line::styled(format!("  ⟳ {label}"), theme.dim_style()));
            } else {
                let label = data
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(kind);
                lines.push(Line::styled(format!("  ⋯ {label}"), theme.faint_style()));
            }
        }
    }
}
