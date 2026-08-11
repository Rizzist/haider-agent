//! `--plain` renderer: the projection as plain UTF-8 lines — no colors, no
//! cursor control, no alternate screen. This is the CI/test/pipe fallback
//! (and the oracle for "did the projection say the right thing"), so shapes
//! stay stable and greppable.

use crate::format::{METER_CELLS_DEFAULT, fmt_tok, meter_cells};
use crate::projection::{ItemBlock, SessionProjection, TranscriptEntry};
use haider_protocol::item::{ToolStatus, TurnItem};

/// Status glyphs shared by tool and command rows (sim ToolRow vocabulary,
/// tui.js:3901-3909: running `◐` · ok `✓` · err `✗`).
#[must_use]
pub const fn status_glyph(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "…",
        ToolStatus::InProgress => "◐",
        ToolStatus::Completed => "✓",
        ToolStatus::Failed => "✗",
        ToolStatus::Cancelled => "⊘",
    }
}

/// Render the whole session view as plain lines: transcript, pinned todos,
/// open menu, throughput row, status line. `window` sizes the context meter
/// (0 = no meter). `throughput` is the live token-rate readout when a turn is
/// streaming (WG6 parity — the same figures the styled row shows), `None`
/// otherwise so idle plain output is unchanged.
#[must_use]
pub fn render_plain(
    projection: &SessionProjection,
    window: u64,
    throughput: Option<&crate::throughput::ThroughputReadout>,
) -> String {
    render_plain_impl(projection, window, throughput, None)
}

/// Live/plain parity with the styled status and `/usage` cache semantics.
#[must_use]
pub fn render_plain_with_cache(
    projection: &SessionProjection,
    window: u64,
    throughput: Option<&crate::throughput::ThroughputReadout>,
    cache_usage: &crate::cache_usage::SessionUsageFold,
) -> String {
    render_plain_impl(projection, window, throughput, Some(cache_usage))
}

fn render_plain_impl(
    projection: &SessionProjection,
    window: u64,
    throughput: Option<&crate::throughput::ThroughputReadout>,
    cache_usage: Option<&crate::cache_usage::SessionUsageFold>,
) -> String {
    let mut out = String::new();
    for entry in projection.entries() {
        match entry {
            TranscriptEntry::User {
                text,
                attachments,
                voice,
                from_main,
            } => {
                // S3: parent-authored chip rows wear → · from main (the
                // rendered view's vocabulary, kept greppable here).
                out.push_str(if *from_main {
                    "→ "
                } else if *voice {
                    "◉ "
                } else {
                    "❯ "
                });
                out.push_str(text);
                if *attachments > 0 {
                    out.push_str(&format!(" [+{attachments} attachment(s)]"));
                }
                if *voice {
                    out.push_str(" · spoken");
                }
                if *from_main {
                    out.push_str(" · from main");
                }
                out.push('\n');
            }
            TranscriptEntry::Item(block) => render_item(&mut out, block),
            TranscriptEntry::Note { text } => {
                out.push_str(text);
                out.push('\n');
            }
            TranscriptEntry::Error { text } => {
                out.push_str("✗ ");
                out.push_str(text);
                out.push('\n');
            }
            TranscriptEntry::Shell {
                cmd,
                out: shell_out,
            } => {
                out.push_str(&format!("$ {cmd}\n"));
                for line in shell_out.split('\n') {
                    out.push_str("  ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    if let Some(todos) = projection.todos().filter(|t| t.pinned) {
        out.push_str(&format!(
            "todos — {}/{} done",
            todos.done_count(),
            todos.items.len()
        ));
        if let Some(current) = todos.current() {
            out.push_str(&format!(" · ■ {}", current.text));
        }
        out.push('\n');
        for item in &todos.items {
            let mark = match item.state {
                haider_protocol::history::TodoState::Completed => "✓",
                haider_protocol::history::TodoState::Processing => "■",
                haider_protocol::history::TodoState::Listed => "☐",
            };
            out.push_str(&format!("  {mark} {}\n", item.text));
        }
    }
    if let Some(menu) = projection.open_menu() {
        out.push_str(&format!("? {}\n", menu.title));
        for (index, option) in menu.options.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", index + 1, option.label));
        }
    }
    // W-G: the live throughput row sits in the band above the status line —
    // present only while the turn streams (the caller passes `None` at rest),
    // printing the SAME figures as the styled render row.
    if let Some(readout) = throughput {
        out.push_str(&readout.plain_text());
        out.push('\n');
    }
    let cache_totals = cache_usage
        .filter(|usage| !usage.is_empty())
        .map(crate::cache_usage::SessionUsageFold::totals);
    if let Some(totals) = &cache_totals {
        out.push_str(&cache_breakdown_plain(totals));
        out.push('\n');
    }
    out.push_str(&status_line(projection, window));
    if let Some(totals) = &cache_totals {
        out.push_str(" · ");
        out.push_str(&crate::cache_usage::wide_status(totals));
    }
    out.push('\n');
    out
}

fn cache_breakdown_plain(stats: &haider_protocol::usage::CacheUsageStatsV1) -> String {
    use crate::cache_usage::CacheUsageStatsExt as _;
    let hit = stats
        .complete_hit_rate()
        .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.2}%", rate * 100.0));
    let coverage = stats
        .telemetry_coverage()
        .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.0}%", rate * 100.0));
    let mut out = format!(
        "cache usage — logical {} · uncached {} · write {} (5m {} · 1h {}) · read {} · hit {hit} · coverage {coverage}",
        fmt_tok(stats.logical_input_tokens),
        fmt_tok(stats.uncached_input_tokens),
        fmt_tok(stats.cache_write_tokens),
        fmt_tok(stats.cache_write_5m_tokens),
        fmt_tok(stats.cache_write_1h_tokens),
        fmt_tok(stats.cache_read_tokens),
    );
    match (
        stats.input_with_cache_usd,
        stats.input_without_cache_usd,
        stats.estimated_savings_usd,
    ) {
        (Some(with), Some(without), Some(savings)) => out.push_str(&format!(
            " · input ${with:.4} cached / ${without:.4} without · savings ${savings:.4}"
        )),
        _ => out.push_str(" · input cost n/a · savings n/a"),
    }
    for breakdown in &stats.breakdowns {
        let epoch = breakdown
            .cache_epoch
            .get(..8)
            .unwrap_or(&breakdown.cache_epoch);
        let cost = match (
            breakdown.input_with_cache_usd,
            breakdown.input_without_cache_usd,
            breakdown.estimated_savings_usd,
        ) {
            (Some(with), Some(without), Some(savings)) => {
                format!(" · input ${with:.4}/${without:.4} · save ${savings:.4}")
            }
            _ => " · input $ n/a".to_owned(),
        };
        let complete = breakdown.logical_input_tokens > 0
            && breakdown.telemetry_covered_input_tokens == breakdown.logical_input_tokens;
        let denominator = breakdown
            .cache_read_tokens
            .saturating_add(breakdown.uncached_input_tokens);
        let part_hit = if complete {
            #[allow(clippy::cast_precision_loss)]
            let rate = if denominator == 0 {
                0.0
            } else {
                breakdown.cache_read_tokens as f64 / denominator as f64
            };
            format!("{:.2}%", rate * 100.0)
        } else {
            "n/a".to_owned()
        };
        #[allow(clippy::cast_precision_loss)]
        let part_coverage = if breakdown.logical_input_tokens == 0 {
            "n/a".to_owned()
        } else {
            format!(
                "{:.0}%",
                breakdown.telemetry_covered_input_tokens as f64
                    / breakdown.logical_input_tokens as f64
                    * 100.0
            )
        };
        out.push_str(&format!(
            "\n  {} / {} / {} / {:?} — uncached {} · write {} · read {} · hit {part_hit} · coverage {part_coverage}{cost}",
            if breakdown.provider.is_empty() {
                "unknown"
            } else {
                &breakdown.provider
            },
            if breakdown.model.is_empty() {
                "unknown"
            } else {
                &breakdown.model
            },
            if epoch.is_empty() { "unknown" } else { epoch },
            breakdown.request_kind,
            fmt_tok(breakdown.uncached_input_tokens),
            fmt_tok(breakdown.cache_write_tokens),
            fmt_tok(breakdown.cache_read_tokens),
        ));
    }
    out
}

/// The plain status line: `IDLE · 33k tok · ▰▰▱▱▱▱▱▱▱▱ 17% of 200k`.
#[must_use]
pub fn status_line(projection: &SessionProjection, window: u64) -> String {
    let badge = projection.badge();
    let tokens = projection.context_tokens();
    if window == 0 {
        return format!("{badge} · {} tok", fmt_tok(tokens));
    }
    #[allow(clippy::cast_precision_loss)]
    let pct = tokens as f64 / window as f64;
    format!(
        "{badge} · {} tok · {} {}% of {}",
        fmt_tok(tokens),
        meter_cells(pct, METER_CELLS_DEFAULT),
        (pct.clamp(0.0, 1.0) * 100.0).round(),
        fmt_tok(window)
    )
}

fn render_item(out: &mut String, block: &ItemBlock) {
    match &block.item {
        TurnItem::AgentMessage { text } => {
            out.push_str(text);
            if block.streaming {
                out.push('▮');
            }
            out.push('\n');
        }
        TurnItem::Reasoning { summary } => {
            out.push_str("· ");
            out.push_str(summary);
            out.push('\n');
        }
        TurnItem::ToolCall { name, status, .. } => {
            out.push_str(&format!("⚒ {name} {}\n", status_glyph(*status)));
        }
        TurnItem::CommandExecution {
            command,
            status,
            exit_code,
            ..
        } => {
            out.push_str(&format!("$ {command} {}", status_glyph(*status)));
            if let Some(code) = exit_code {
                out.push_str(&format!(" · exit {code}"));
            }
            out.push('\n');
            if block.output_truncated {
                out.push_str("  ⋯ earlier output truncated\n");
            }
            if block.output_decode_error {
                out.push_str("  ⚠ some output could not be decoded\n");
            }
            let tail = block.output_text();
            for line in tail.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
        TurnItem::FileChange {
            path,
            added,
            removed,
        } => {
            out.push_str(&format!("± {path} +{added} -{removed}\n"));
        }
        TurnItem::ChildSpawn { agent } => {
            out.push_str(&format!("◉ subagent {} spawned\n", agent.as_str()));
        }
        TurnItem::ChildResult { report } => {
            out.push_str(&format!("└ subagent report — {}\n", report.summary));
        }
        TurnItem::Plan { items } => {
            let done = items
                .iter()
                .filter(|i| i.state == haider_protocol::history::TodoState::Completed)
                .count();
            out.push_str(&format!("✓ plan — {done}/{} done\n", items.len()));
        }
        TurnItem::ContextCompaction {
            tokens_before,
            tokens_after,
            ..
        } => match (tokens_before, tokens_after) {
            (Some(before), Some(after)) => out.push_str(&format!(
                "⊟ compacted {} → {}\n",
                fmt_tok(*before),
                fmt_tok(*after)
            )),
            _ => out.push_str("⊟ context compacted\n"),
        },
        TurnItem::Extension { kind, .. } => out.push_str(&format!("⋯ {kind}\n")),
    }
}
