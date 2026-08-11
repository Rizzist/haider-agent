//! Shared formatting and direct-agent aggregation for rich/plain surfaces.

use haider_protocol::agent::{AgentMetricsSnapshot, AgentUsageBreakdown, AgentUsageMetrics};
use haider_protocol::credential::AuthMethod;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetricsAggregate {
    pub tool_attempts: u64,
    /// Absent when any included agent has no durable usage truth.
    pub usage: Option<AgentUsageMetrics>,
}

#[must_use]
pub fn normalized_tokens(usage: &AgentUsageMetrics) -> u64 {
    usage
        .logical_input_tokens
        .saturating_add(usage.billed_output_tokens)
}

#[must_use]
pub fn elapsed_ms(snapshot: &AgentMetricsSnapshot, now_ms: u64) -> u64 {
    snapshot
        .terminal_at_ms
        .unwrap_or(now_ms)
        .saturating_sub(snapshot.started_at_ms)
}

fn usd(microusd: u64) -> String {
    let value = microusd as f64 / 1_000_000.0;
    if microusd == 0 || microusd >= 10_000 {
        format!("{value:.2}")
    } else if microusd >= 1_000 {
        format!("{value:.4}")
    } else {
        format!("{value:.6}")
    }
}

#[must_use]
pub fn compact_cost(usage: &AgentUsageMetrics) -> String {
    if !usage.all_lanes_priced {
        return "$—".to_owned();
    }
    match (usage.has_metered_lanes, usage.has_oauth_lanes) {
        (true, true) => match (
            usage.metered_cost_microusd,
            usage.api_equivalent_cost_microusd,
        ) {
            (Some(real), Some(equivalent)) => {
                format!("${} + ≈${}", usd(real), usd(equivalent))
            }
            _ => "$—".to_owned(),
        },
        (true, false) => usage
            .metered_cost_microusd
            .map_or_else(|| "$—".to_owned(), |cost| format!("${}", usd(cost))),
        (false, true) => usage
            .api_equivalent_cost_microusd
            .map_or_else(|| "$—".to_owned(), |cost| format!("≈${}", usd(cost))),
        (false, false) => "$—".to_owned(),
    }
}

#[must_use]
pub fn detailed_cost(usage: &AgentUsageMetrics) -> String {
    if !usage.all_lanes_priced {
        return "$—".to_owned();
    }
    match (usage.has_metered_lanes, usage.has_oauth_lanes) {
        (true, true) => match (
            usage.metered_cost_microusd,
            usage.api_equivalent_cost_microusd,
        ) {
            (Some(real), Some(equivalent)) => format!(
                "${} metered · ≈${} API rate (all lanes)",
                usd(real),
                usd(equivalent)
            ),
            _ => "$—".to_owned(),
        },
        (true, false) => usage
            .metered_cost_microusd
            .map_or_else(|| "$—".to_owned(), |cost| format!("${}", usd(cost))),
        (false, true) => usage.api_equivalent_cost_microusd.map_or_else(
            || "$—".to_owned(),
            |cost| format!("≈${} API rate · plan", usd(cost)),
        ),
        (false, false) => "$—".to_owned(),
    }
}

fn breakdown_cost(breakdown: &AgentUsageBreakdown) -> String {
    if !breakdown.priced {
        return "$—".to_owned();
    }
    match breakdown.auth_method {
        Some(AuthMethod::ApiKey) => breakdown
            .metered_cost_microusd
            .map_or_else(|| "$—".to_owned(), |cost| format!("${}", usd(cost))),
        Some(AuthMethod::OAuth) => breakdown.api_equivalent_cost_microusd.map_or_else(
            || "$—".to_owned(),
            |cost| format!("≈${} API rate · plan", usd(cost)),
        ),
        None => "$—".to_owned(),
    }
}

#[must_use]
pub fn detail_lines(snapshot: &AgentMetricsSnapshot) -> Vec<String> {
    let Some(usage) = &snapshot.usage else {
        return vec![
            format!(
                "own — {} tools · tokens n/a · cost n/a",
                snapshot.tool_attempts
            ),
            "tokens — in n/a · out n/a · cached n/a · cache write n/a".to_owned(),
            "cache — hit n/a".to_owned(),
        ];
    };
    let mut lines = vec![
        format!(
            "own — {} tools · {} tokens · {}",
            snapshot.tool_attempts,
            crate::format::fmt_tok(normalized_tokens(usage)),
            detailed_cost(usage)
        ),
        format!(
            "tokens — in {} · out {} · cached {} · cache write {}{}",
            crate::format::fmt_tok(usage.logical_input_tokens),
            crate::format::fmt_tok(usage.billed_output_tokens),
            crate::format::fmt_tok(usage.cache_read_tokens),
            crate::format::fmt_tok(usage.cache_write_tokens),
            if usage.additional_reasoning_tokens == 0 {
                String::new()
            } else {
                format!(
                    " · reasoning +{}",
                    crate::format::fmt_tok(usage.additional_reasoning_tokens)
                )
            }
        ),
        usage.cache_hit_basis_points.map_or_else(
            || "cache — hit n/a".to_owned(),
            |points| format!("cache — {:.2}% hit", points as f64 / 100.0),
        ),
    ];
    for breakdown in &usage.breakdowns {
        let lane = match breakdown.request_kind {
            haider_protocol::provider::UsageRequestKind::MainTurn => "main",
            haider_protocol::provider::UsageRequestKind::Compaction => "compaction",
            haider_protocol::provider::UsageRequestKind::DelegatedAgent => "delegated",
        };
        let provider = if breakdown.provider.is_empty() {
            "unknown"
        } else {
            &breakdown.provider
        };
        let model = if breakdown.model.is_empty() {
            "unknown"
        } else {
            &breakdown.model
        };
        lines.push(format!(
            "{provider}/{model} · {lane} — {} tokens · {}",
            crate::format::fmt_tok(
                breakdown
                    .logical_input_tokens
                    .saturating_add(breakdown.billed_output_tokens)
            ),
            breakdown_cost(breakdown)
        ));
    }
    lines
}

#[must_use]
pub fn aggregate<'a>(
    snapshots: impl IntoIterator<Item = &'a AgentMetricsSnapshot>,
) -> Option<MetricsAggregate> {
    let snapshots = snapshots.into_iter().collect::<Vec<_>>();
    if snapshots.is_empty() {
        return None;
    }
    let tool_attempts = snapshots.iter().fold(0_u64, |sum, snapshot| {
        sum.saturating_add(snapshot.tool_attempts)
    });
    if snapshots.iter().any(|snapshot| snapshot.usage.is_none()) {
        return Some(MetricsAggregate {
            tool_attempts,
            usage: None,
        });
    }
    let mut usage = AgentUsageMetrics {
        all_lanes_priced: true,
        ..AgentUsageMetrics::default()
    };
    let mut metered_cost = 0_u64;
    let mut api_cost = 0_u64;
    let mut metered_priced = true;
    let mut api_priced = true;
    for snapshot in snapshots {
        let Some(item) = snapshot.usage.as_ref() else {
            unreachable!("absence returned above");
        };
        usage.logical_input_tokens = usage
            .logical_input_tokens
            .saturating_add(item.logical_input_tokens);
        usage.billed_output_tokens = usage
            .billed_output_tokens
            .saturating_add(item.billed_output_tokens);
        usage.additional_reasoning_tokens = usage
            .additional_reasoning_tokens
            .saturating_add(item.additional_reasoning_tokens);
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(item.cache_read_tokens);
        usage.cache_write_tokens = usage
            .cache_write_tokens
            .saturating_add(item.cache_write_tokens);
        usage.has_metered_lanes |= item.has_metered_lanes;
        usage.has_oauth_lanes |= item.has_oauth_lanes;
        usage.all_lanes_priced &= item.all_lanes_priced;
        if item.has_metered_lanes {
            if let Some(cost) = item.metered_cost_microusd {
                metered_cost = metered_cost.saturating_add(cost);
            } else {
                metered_priced = false;
            }
        }
        if let Some(cost) = item.api_equivalent_cost_microusd {
            api_cost = api_cost.saturating_add(cost);
        } else {
            api_priced = false;
        }
    }
    usage.metered_cost_microusd =
        (usage.has_metered_lanes && metered_priced).then_some(metered_cost);
    usage.api_equivalent_cost_microusd = (usage.all_lanes_priced && api_priced).then_some(api_cost);
    Some(MetricsAggregate {
        tool_attempts,
        usage: Some(usage),
    })
}
