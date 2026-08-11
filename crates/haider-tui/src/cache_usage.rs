//! Session-wide cache usage fold and display math.
//!
//! Provider streams emit cumulative snapshots within one logical run. The
//! latest snapshot for a full cache-domain/request-lane key replaces the
//! earlier one; totals sum only those latest snapshots.

use haider_protocol::credential::AuthMethod;
use haider_protocol::provider::{CacheStatAvailability, Usage, UsageRequestKind};
use haider_protocol::usage::{CacheUsageBreakdownV1, CacheUsageStatsV1};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UsageFoldKey {
    run: String,
    agent: String,
    provider: String,
    model: String,
    epoch: String,
    request_kind: UsageRequestKind,
}

fn scope_auth_method(auth_scope: &str) -> Option<AuthMethod> {
    match auth_scope {
        "api_key" => Some(AuthMethod::ApiKey),
        "oauth" | "oauth_subscription" => Some(AuthMethod::OAuth),
        _ => None,
    }
}

/// Latest-snapshot session fold used by the status bar, `/usage`, and plain
/// renderer. It is session-wide rather than branch-wide, so delegated-agent,
/// parked-branch, and compaction usage cannot disappear on a view switch.
#[derive(Debug, Clone, Default)]
pub struct SessionUsageFold {
    chunks: HashMap<UsageFoldKey, Usage>,
}

impl SessionUsageFold {
    pub fn note(&mut self, usage: &Usage) {
        let scope = usage.scope.as_ref();
        let key = UsageFoldKey {
            run: scope
                .and_then(|scope| scope.run.as_ref())
                .map_or_else(|| "legacy".to_owned(), |run| run.as_str().to_owned()),
            agent: scope
                .and_then(|scope| scope.agent.as_ref())
                .map_or_else(String::new, |agent| agent.as_str().to_owned()),
            provider: scope.map_or_else(String::new, |scope| scope.provider.clone()),
            model: scope.map_or_else(String::new, |scope| scope.model.clone()),
            epoch: scope.map_or_else(String::new, |scope| scope.cache_epoch.clone()),
            request_kind: scope.map_or(UsageRequestKind::MainTurn, |scope| scope.request_kind),
        };
        self.chunks.insert(key, usage.clone());
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    #[must_use]
    pub fn totals(&self) -> CacheUsageStatsV1 {
        let mut totals = CacheUsageStatsV1::default();
        let mut breakdowns: HashMap<
            (String, String, String, UsageRequestKind, Option<AuthMethod>),
            CacheUsageBreakdownV1,
        > = HashMap::new();
        let mut input_with = 0.0;
        let mut input_without = 0.0;
        let mut savings = 0.0;
        let mut metered_priced = true;
        let mut has_metered = false;
        let mut api_input_with = 0.0;
        let mut api_input_without = 0.0;
        let mut api_savings = 0.0;
        let mut api_priced = true;
        let mut has_lane = false;

        for (key, usage) in &self.chunks {
            has_lane = true;
            let normalized = usage.normalized.as_ref();
            let logical = normalized.map_or(usage.input, |usage| usage.logical_input);
            let uncached = normalized.map_or(usage.input, |usage| usage.uncached_input);
            let read = normalized.map_or(0, |usage| usage.cache_read_input);
            let write = normalized.map_or(0, |usage| usage.cache_write_input);
            let write_5m = normalized.map_or(0, |usage| usage.cache_write_5m_input);
            let write_1h = normalized.map_or(0, |usage| usage.cache_write_1h_input);
            let output = normalized.map_or(usage.output, |usage| usage.billed_output);
            let covered = normalized.map_or(0, |usage| usage.cache_telemetry_input);

            totals.logical_input_tokens = totals.logical_input_tokens.saturating_add(logical);
            totals.uncached_input_tokens = totals.uncached_input_tokens.saturating_add(uncached);
            totals.cache_read_tokens = totals.cache_read_tokens.saturating_add(read);
            totals.cache_write_tokens = totals.cache_write_tokens.saturating_add(write);
            totals.cache_write_5m_tokens = totals.cache_write_5m_tokens.saturating_add(write_5m);
            totals.cache_write_1h_tokens = totals.cache_write_1h_tokens.saturating_add(write_1h);
            totals.billed_output_tokens = totals.billed_output_tokens.saturating_add(output);
            totals.telemetry_covered_input_tokens = totals
                .telemetry_covered_input_tokens
                .saturating_add(covered);
            let auth_method = usage
                .scope
                .as_ref()
                .and_then(|scope| scope_auth_method(&scope.auth_scope));
            let metered = auth_method == Some(AuthMethod::ApiKey);
            if metered {
                has_metered = true;
                totals.metered_input_tokens = totals.metered_input_tokens.saturating_add(logical);
            }

            let breakdown = breakdowns
                .entry((
                    key.provider.clone(),
                    key.model.clone(),
                    key.epoch.clone(),
                    key.request_kind,
                    auth_method,
                ))
                .or_insert_with(|| CacheUsageBreakdownV1 {
                    provider: key.provider.clone(),
                    model: key.model.clone(),
                    cache_epoch: key.epoch.clone(),
                    request_kind: key.request_kind,
                    auth_method,
                    cache_status: CacheStatAvailability::Present,
                    ..CacheUsageBreakdownV1::default()
                });
            let breakdown_had_input = breakdown.logical_input_tokens > 0;
            breakdown.logical_input_tokens = breakdown.logical_input_tokens.saturating_add(logical);
            breakdown.uncached_input_tokens =
                breakdown.uncached_input_tokens.saturating_add(uncached);
            breakdown.cache_read_tokens = breakdown.cache_read_tokens.saturating_add(read);
            breakdown.cache_write_tokens = breakdown.cache_write_tokens.saturating_add(write);
            breakdown.cache_write_5m_tokens =
                breakdown.cache_write_5m_tokens.saturating_add(write_5m);
            breakdown.cache_write_1h_tokens =
                breakdown.cache_write_1h_tokens.saturating_add(write_1h);
            breakdown.billed_output_tokens = breakdown.billed_output_tokens.saturating_add(output);
            breakdown.telemetry_covered_input_tokens = breakdown
                .telemetry_covered_input_tokens
                .saturating_add(covered);
            if covered != logical {
                breakdown.cache_status = CacheStatAvailability::Unavailable;
            }

            if metered {
                if let Some(cost) = usage.cache_cost {
                    input_with += cost.input_with_cache_usd;
                    input_without += cost.input_without_cache_usd;
                    savings += cost.estimated_savings_usd;
                    merge_cost(
                        &mut breakdown.input_with_cache_usd,
                        Some(cost.input_with_cache_usd),
                        breakdown_had_input,
                    );
                    merge_cost(
                        &mut breakdown.input_without_cache_usd,
                        Some(cost.input_without_cache_usd),
                        breakdown_had_input,
                    );
                    merge_cost(
                        &mut breakdown.estimated_savings_usd,
                        Some(cost.estimated_savings_usd),
                        breakdown_had_input,
                    );
                } else {
                    metered_priced = false;
                    merge_cost(
                        &mut breakdown.input_with_cache_usd,
                        None,
                        breakdown_had_input,
                    );
                    merge_cost(
                        &mut breakdown.input_without_cache_usd,
                        None,
                        breakdown_had_input,
                    );
                    merge_cost(
                        &mut breakdown.estimated_savings_usd,
                        None,
                        breakdown_had_input,
                    );
                }
            }
            if auth_method.is_some() {
                if let Some(cost) = usage.cache_cost {
                    api_input_with += cost.input_with_cache_usd;
                    api_input_without += cost.input_without_cache_usd;
                    api_savings += cost.estimated_savings_usd;
                    merge_cost(
                        &mut breakdown.api_equivalent_input_with_cache_usd,
                        Some(cost.input_with_cache_usd),
                        breakdown_had_input,
                    );
                    merge_cost(
                        &mut breakdown.api_equivalent_input_without_cache_usd,
                        Some(cost.input_without_cache_usd),
                        breakdown_had_input,
                    );
                    merge_cost(
                        &mut breakdown.api_equivalent_estimated_savings_usd,
                        Some(cost.estimated_savings_usd),
                        breakdown_had_input,
                    );
                } else {
                    api_priced = false;
                    merge_cost(
                        &mut breakdown.api_equivalent_input_with_cache_usd,
                        None,
                        breakdown_had_input,
                    );
                    merge_cost(
                        &mut breakdown.api_equivalent_input_without_cache_usd,
                        None,
                        breakdown_had_input,
                    );
                    merge_cost(
                        &mut breakdown.api_equivalent_estimated_savings_usd,
                        None,
                        breakdown_had_input,
                    );
                }
            } else {
                api_priced = false;
            }
        }

        let mut breakdowns = breakdowns.into_values().collect::<Vec<_>>();
        breakdowns.sort_by(|left, right| {
            (
                left.provider.as_str(),
                left.model.as_str(),
                left.cache_epoch.as_str(),
                request_kind_rank(left.request_kind),
            )
                .cmp(&(
                    right.provider.as_str(),
                    right.model.as_str(),
                    right.cache_epoch.as_str(),
                    request_kind_rank(right.request_kind),
                ))
        });
        if has_metered && metered_priced {
            totals.input_with_cache_usd = Some(input_with);
            totals.input_without_cache_usd = Some(input_without);
            totals.estimated_savings_usd = Some(savings);
        }
        if has_lane && api_priced {
            totals.api_equivalent_input_with_cache_usd = Some(api_input_with);
            totals.api_equivalent_input_without_cache_usd = Some(api_input_without);
            totals.api_equivalent_estimated_savings_usd = Some(api_savings);
        }
        totals.breakdowns = breakdowns;
        totals
    }
}

fn merge_cost(target: &mut Option<f64>, source: Option<f64>, target_had_input: bool) {
    *target = match (*target, source, target_had_input) {
        (_, None, _) => None,
        (Some(left), Some(right), true) => Some(left + right),
        (_, Some(right), false) => Some(right),
        (None, Some(_), true) => None,
    };
}

const fn request_kind_rank(kind: UsageRequestKind) -> u8 {
    match kind {
        UsageRequestKind::MainTurn => 0,
        UsageRequestKind::Compaction => 1,
        UsageRequestKind::DelegatedAgent => 2,
    }
}

pub trait CacheUsageStatsExt {
    /// Token-weighted hit rate only when every logical input token has an
    /// authoritative cache split.
    fn complete_hit_rate(&self) -> Option<f64>;

    fn telemetry_coverage(&self) -> Option<f64>;
}

impl CacheUsageStatsExt for CacheUsageStatsV1 {
    fn complete_hit_rate(&self) -> Option<f64> {
        if self.logical_input_tokens == 0
            || self.telemetry_covered_input_tokens != self.logical_input_tokens
        {
            return None;
        }
        let denominator = self
            .cache_read_tokens
            .saturating_add(self.uncached_input_tokens);
        if denominator == 0 {
            return Some(0.0);
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.cache_read_tokens as f64 / denominator as f64)
    }

    fn telemetry_coverage(&self) -> Option<f64> {
        if self.logical_input_tokens == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.telemetry_covered_input_tokens as f64 / self.logical_input_tokens as f64)
    }
}

#[must_use]
pub fn wide_status(stats: &CacheUsageStatsV1) -> String {
    match stats.complete_hit_rate() {
        Some(hit) => format!(
            "↑{} ↓{} ⚡{} {:.2}% hit",
            crate::format::fmt_tok(stats.uncached_input_tokens),
            crate::format::fmt_tok(stats.billed_output_tokens),
            crate::format::fmt_tok(stats.cache_read_tokens),
            hit * 100.0,
        ),
        None => "⚡n/a · hit n/a".to_owned(),
    }
}

#[must_use]
pub fn medium_status(stats: &CacheUsageStatsV1) -> String {
    match stats.complete_hit_rate() {
        Some(hit) => format!(
            "⚡{} {:.1}% hit",
            crate::format::fmt_tok(stats.cache_read_tokens),
            hit * 100.0,
        ),
        None => "⚡n/a · hit n/a".to_owned(),
    }
}
