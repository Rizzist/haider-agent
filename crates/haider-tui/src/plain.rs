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
        ToolStatus::Rejected => "✗",
        ToolStatus::Conflict => "✗",
        ToolStatus::Failed => "✗",
        ToolStatus::Cancelled => "⊘",
        ToolStatus::Unknown => "?",
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
            TranscriptEntry::Peer {
                sender,
                sender_kind,
                text,
                receipt,
                ..
            } => {
                out.push_str(&format!(
                    "@ {sender}› · {sender_kind} · UNTRUSTED PEER INPUT\n"
                ));
                for line in text.split('\n') {
                    out.push_str("  ▏ ");
                    out.push_str(line);
                    out.push('\n');
                }
                if let Some(receipt) = receipt {
                    out.push_str("    receipt · ");
                    out.push_str(match receipt {
                        haider_protocol::peer::PeerDelivery::Queued => "queued",
                        haider_protocol::peer::PeerDelivery::Delivered => "delivered",
                        haider_protocol::peer::PeerDelivery::Expired => "expired",
                        haider_protocol::peer::PeerDelivery::Refused => "refused",
                    });
                    out.push('\n');
                }
            }
            TranscriptEntry::Item(block) => render_item(&mut out, block),
            TranscriptEntry::Note { text } => {
                out.push_str(text);
                out.push('\n');
            }
            TranscriptEntry::Refusal {
                provider,
                tool,
                reason,
            } => {
                out.push_str(&format!("🔒 REFUSED · {provider} · {tool} — {reason}\n"));
            }
            TranscriptEntry::Error { text, .. } => {
                // `text` is the flattened presentation (title — detail
                // [subcode] · facts · actions) — plain carries the same
                // information as the styled block, in honest plain text.
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
        // Plain output uses the static provider delay because it has no
        // render clock for a live countdown. Both recovery families speak
        // through their typed presentation when they carry one — the E2
        // provider/account card and the E6 effect-reconciliation card show
        // the same detail + fact line the styled card renders.
        let typed_presentation = match &menu.kind {
            haider_protocol::menu::MenuKind::ErrorRecovery { presentation, .. } => {
                Some(presentation)
            }
            haider_protocol::menu::MenuKind::Recovery { presentation, .. } => presentation.as_ref(),
            _ => None,
        };
        if let Some(presentation) = typed_presentation {
            for line in presentation.detail.split('\n') {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
            let facts = crate::projection::error_fact_segments(presentation, None);
            out.push_str("  ");
            out.push_str(&crate::projection::join_error_fact_segments(&facts));
            out.push('\n');
        }
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
    if cache_usage.is_some_and(crate::cache_usage::SessionUsageFold::has_unclassified_usage) {
        out.push_str(
            "cache usage — unclassified request usage present; excluded from totals and rates\n",
        );
    }
    let cache_totals = cache_usage
        .filter(|usage| usage.has_classified_usage())
        .map(crate::cache_usage::SessionUsageFold::totals);
    if let Some(totals) = &cache_totals {
        out.push_str(&cache_breakdown_plain(totals));
        out.push('\n');
    }
    out.push_str(&status_line(projection, window));
    if let Some(totals) = &cache_totals {
        out.push_str(" · ");
        out.push_str(&crate::cache_usage::wide_status(totals, None));
    }
    out.push('\n');
    out
}

fn cache_breakdown_plain(stats: &haider_protocol::usage::CacheUsageStatsV1) -> String {
    use crate::cache_usage::CacheUsageStatsExt as _;
    let all_input_share = stats
        .complete_hit_rate()
        .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.2}%", rate * 100.0));
    let coverage = stats
        .telemetry_coverage()
        .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.0}%", rate * 100.0));
    let mut out = format!(
        "cache usage — logical {} · uncached {} · write {} (5m {} · 1h {}) · read {} · all-input share {all_input_share} · coverage {coverage}",
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
        (Some(with), Some(without), Some(savings)) => {
            let qualifier = if stats.metered_input_tokens < stats.logical_input_tokens {
                " (metered lanes)"
            } else {
                ""
            };
            out.push_str(&format!(
                " · input ${with:.4} cached / ${without:.4} without · savings ${savings:.4}{qualifier}"
            ));
            if stats.breakdowns.iter().any(|breakdown| {
                breakdown.auth_method == Some(haider_protocol::credential::AuthMethod::OAuth)
            }) {
                match (
                    stats.api_equivalent_input_with_cache_usd,
                    stats.api_equivalent_input_without_cache_usd,
                    stats.api_equivalent_estimated_savings_usd,
                ) {
                    (Some(api_with), Some(api_without), Some(api_savings)) => {
                        out.push_str(&format!(
                            " · ≈${api_with:.4}/${api_without:.4} API rate (all lanes) · savings ≈${api_savings:.4}"
                        ));
                    }
                    _ => out.push_str(" · $— API rate (all lanes)"),
                }
            }
        }
        _ if !stats.breakdowns.is_empty()
            && stats.breakdowns.iter().all(|breakdown| {
                breakdown.auth_method == Some(haider_protocol::credential::AuthMethod::OAuth)
            }) =>
        {
            match (
                stats.api_equivalent_input_with_cache_usd,
                stats.api_equivalent_input_without_cache_usd,
                stats.api_equivalent_estimated_savings_usd,
            ) {
                (Some(with), Some(without), Some(savings)) => out.push_str(&format!(
                    " · plan · ≈${with:.4}/${without:.4} API rate · savings ≈${savings:.4}"
                )),
                _ => out.push_str(" · plan · $— API rate"),
            }
        }
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
            _ if breakdown.auth_method == Some(haider_protocol::credential::AuthMethod::OAuth) => {
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
            "\n  {} / {} / {} / {:?} — uncached {} · write {} · read {} · all-input share {part_hit} · coverage {part_coverage}{cost}",
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

/// Plain counterpart of the rich direct-agent block and selected-agent
/// detail. Existing plain entry points stay stable; callers with an
/// `AppModel` opt into this additive section.
#[must_use]
pub fn agent_metrics_plain(model: &crate::app::AppModel) -> String {
    let main = model.main_agent_metrics();
    let children = model
        .chips
        .iter()
        .map(|chip| (chip, model.chip_metrics(chip)))
        .collect::<Vec<_>>();
    if main.is_none() && children.iter().all(|(_, metrics)| metrics.is_none()) {
        return String::new();
    }
    let row = |label: &str, metrics: Option<&haider_protocol::agent::AgentMetricsSnapshot>| {
        metrics.map_or_else(
            || format!("{label} — metrics n/a"),
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
                    "{label} — {} tools · {tokens} · {cost}",
                    metrics.tool_attempts
                )
            },
        )
    };
    let mut out = "AGENTS — CURRENT SESSION\n".to_owned();
    if main.is_some()
        && children.iter().all(|(_, metrics)| metrics.is_some())
        && let Some(total) = crate::agent_metrics::aggregate(
            main.into_iter()
                .chain(children.iter().filter_map(|(_, metrics)| *metrics)),
        )
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
        out.push_str(&format!(
            "session total — {} tools · {tokens} · {cost}\n",
            total.tool_attempts
        ));
    }
    out.push_str(&row("main", main));
    out.push('\n');
    for (chip, metrics) in children {
        let label = if chip.callsign.is_empty() {
            chip.agent.as_str()
        } else {
            chip.callsign.as_str()
        };
        out.push_str(&row(label, metrics));
        out.push('\n');
    }
    if model.screen == crate::app::Screen::Subagent
        && let Some(chip) = model.viewed_chip()
        && let Some(metrics) = model.chip_metrics(chip)
    {
        for line in crate::agent_metrics::detail_lines(metrics) {
            out.push_str(&line);
            out.push('\n');
        }
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
            text.visit_strs(|part| out.push_str(part));
            if block.streaming {
                out.push('▮');
            }
            out.push('\n');
        }
        TurnItem::IncompleteAgentMessage { text, interruption } => {
            text.visit_strs(|part| out.push_str(part));
            out.push('\n');
            out.push_str("⚠ incomplete — stream interrupted (");
            out.push_str(interruption.subcode.as_str());
            out.push_str(")\n");
        }
        TurnItem::Reasoning { summary } => {
            out.push_str("· ");
            summary.visit_strs(|part| out.push_str(part));
            out.push('\n');
        }
        TurnItem::ToolCall { name, status, .. } => {
            out.push_str(&format!("⚒ {name} {}", status_glyph(*status)));
            if let Some(reason) = &block.tool_reason {
                out.push_str(&format!(" · {reason}"));
            }
            out.push('\n');
        }
        TurnItem::CommandExecution {
            command,
            status,
            exit_code,
            ..
        } => {
            let sigil = if block.user_command { '!' } else { '$' };
            out.push_str(&format!("{sigil} {command} {}", status_glyph(*status)));
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
            tokens_estimated,
            ..
        } => match (tokens_before, tokens_after) {
            (Some(before), Some(after)) => out.push_str(&format!(
                "⊟ compacted {}{} → {}{}\n",
                if *tokens_estimated { "~" } else { "" },
                fmt_tok(*before),
                if *tokens_estimated { "~" } else { "" },
                fmt_tok(*after)
            )),
            _ => out.push_str("⊟ context compacted\n"),
        },
        TurnItem::Refusal { reason } => out.push_str(&format!("✗ model refused — {reason}\n")),
        TurnItem::Extension { kind, data } => {
            if let Some((_, label)) = crate::projection::image_created_fact(kind, data) {
                out.push_str(&label);
                out.push('\n');
            } else if let Some(transition) =
                haider_protocol::cache::CacheEpochTransitionV1::from_extension_item(&block.item)
            {
                out.push_str(&transition.display_label());
                out.push('\n');
            } else if let Some(label) = crate::projection::retry_marker_label(kind, data) {
                // E8: a bounded in-flight retry marker — the ⟳ renewal
                // glyph and the same sentence the styled row shows.
                out.push_str(&format!("⟳ {label}\n"));
            } else {
                let label = data
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(kind);
                out.push_str(&format!("⋯ {label}\n"));
            }
        }
    }
}
