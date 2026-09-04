//! CM3 cache-sensitive configuration preflight.

use haider_core::StoreHandle;
use haider_protocol::EventPayload;
use haider_protocol::cache::CachePolicyMode;
use haider_protocol::ids::SessionId;
use haider_protocol::provider::{UsageRequestKind, UsageScope};
use haider_protocol::session::SessionMetadataV1;
use haider_provider::{CacheWriteTtl, estimate_cache_rewarm_cost_usd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheChangeWarning {
    pub changed_fields: Vec<String>,
    pub invalidated_stable_tokens: u64,
    pub rewarm_cost_microusd: Option<u64>,
    pub rewarm_api_equivalent_cost_microusd: Option<u64>,
    pub rewarm_base_input_equivalent_tokens: Option<u64>,
    pub policy: CachePolicyMode,
    pub confirmation_required: bool,
}

impl CacheChangeWarning {
    pub(crate) fn message(&self) -> String {
        let fields = self.changed_fields.join(", ");
        let cost = if let Some(microusd) = self.rewarm_cost_microusd {
            format!("${:.4}", microusd as f64 / 1_000_000.0)
        } else if let Some(microusd) = self.rewarm_api_equivalent_cost_microusd {
            format!("≈${:.4} API rate (plan)", microusd as f64 / 1_000_000.0)
        } else {
            "$—".to_owned()
        };
        format!(
            "cache epoch change: {fields}; {} stable-prefix tokens invalidated; estimated next-turn re-warm {cost}; repeat the same selection to confirm a new epoch",
            self.invalidated_stable_tokens
        )
    }
}

/// Single enforcement predicate shared by every config mutation RPC.
#[must_use]
pub(crate) const fn blocks_change(warning: &CacheChangeWarning, confirm_new_epoch: bool) -> bool {
    warning.confirmation_required && !confirm_new_epoch
}

/// Combines the impact of one profile-global credential switch across every
/// warmed session that uses that provider. A single confirmation covers the
/// complete reversible transition, so tokens and known costs are summed.
pub(crate) fn combine_cache_change_warnings(
    warnings: Vec<CacheChangeWarning>,
) -> Option<CacheChangeWarning> {
    let mut warnings = warnings.into_iter();
    let mut combined = warnings.next()?;
    for warning in warnings {
        for field in warning.changed_fields {
            if !combined.changed_fields.contains(&field) {
                combined.changed_fields.push(field);
            }
        }
        combined.invalidated_stable_tokens = combined
            .invalidated_stable_tokens
            .saturating_add(warning.invalidated_stable_tokens);
        combined.rewarm_cost_microusd = combined
            .rewarm_cost_microusd
            .zip(warning.rewarm_cost_microusd)
            .map(|(left, right)| left.saturating_add(right));
        combined.rewarm_api_equivalent_cost_microusd = combined
            .rewarm_api_equivalent_cost_microusd
            .zip(warning.rewarm_api_equivalent_cost_microusd)
            .map(|(left, right)| left.saturating_add(right));
        combined.rewarm_base_input_equivalent_tokens = combined
            .rewarm_base_input_equivalent_tokens
            .zip(warning.rewarm_base_input_equivalent_tokens)
            .map(|(left, right)| left.saturating_add(right));
        combined.confirmation_required |= warning.confirmation_required;
        combined.policy = match (combined.policy, warning.policy) {
            (CachePolicyMode::Economy, _) | (_, CachePolicyMode::Economy) => {
                CachePolicyMode::Economy
            }
            (CachePolicyMode::Balanced, _) | (_, CachePolicyMode::Balanced) => {
                CachePolicyMode::Balanced
            }
            _ => CachePolicyMode::Mobility,
        };
    }
    Some(combined)
}

fn microusd(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    (value * 1_000_000.0).round().min(u64::MAX as f64) as u64
}

/// Assesses one rendered-request change against the current warmed epoch.
/// Tuning fields are always pinned; pair switches use the session policy.
pub(crate) fn assess_cache_change(
    metadata: &SessionMetadataV1,
    current_scope: Option<&UsageScope>,
    target_provider: &str,
    target_model: &str,
    target_auth_scope: Option<&str>,
    changed_fields: Vec<String>,
    tuning_change: bool,
) -> Option<CacheChangeWarning> {
    if changed_fields.is_empty() {
        return None;
    }
    let scope = current_scope?;
    let stable = scope.stable_prefix_tokens;
    let estimate = estimate_cache_rewarm_cost_usd(
        target_provider,
        target_model,
        stable,
        CacheWriteTtl::Default,
    );
    let estimated_microusd = estimate.map(|estimate| microusd(estimate.extra_input_cost_usd));
    let equivalent_tokens = estimate.map(|estimate| {
        estimate
            .base_input_equivalent_tokens
            .round()
            .min(u64::MAX as f64) as u64
    });
    let target_auth_scope = target_auth_scope.unwrap_or(&scope.auth_scope);
    let api_key = target_auth_scope == "api_key";
    let known_auth = matches!(
        target_auth_scope,
        "api_key" | "oauth" | "oauth_subscription"
    );
    let display_microusd = api_key.then_some(estimated_microusd).flatten();
    let api_equivalent_microusd = known_auth.then_some(estimated_microusd).flatten();
    let settings = metadata.cache_policy;
    let confirmation_required = tuning_change
        || match settings.mode {
            CachePolicyMode::Economy => true,
            CachePolicyMode::Balanced => {
                estimated_microusd.is_none_or(|cost| cost >= settings.cold_cost_threshold_microusd)
            }
            CachePolicyMode::Mobility => false,
        };
    Some(CacheChangeWarning {
        changed_fields,
        invalidated_stable_tokens: stable,
        rewarm_cost_microusd: display_microusd,
        rewarm_api_equivalent_cost_microusd: api_equivalent_microusd,
        rewarm_base_input_equivalent_tokens: equivalent_tokens,
        policy: settings.mode,
        confirmation_required,
    })
}

/// Latest durable head-agent/main-turn cache scope. Usage events are
/// cumulative within a lane; only the newest scope is needed for preflight.
pub(crate) async fn latest_main_cache_scope(
    store: &haider_core::SqliteStoreHandle,
    session_id: &SessionId,
) -> Result<Option<UsageScope>, haider_protocol::error::HaiderError> {
    let mut cursor = 0;
    let mut latest = None;
    loop {
        let page = StoreHandle::read(store, session_id, cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            let Ok(EventPayload::Usage(usage)) = envelope.payload.decode_event() else {
                continue;
            };
            let Some(scope) = usage.scope else {
                continue;
            };
            if scope.request_kind == UsageRequestKind::MainTurn && scope.agent.is_none() {
                latest = Some(scope);
            }
        }
    }
    Ok(latest)
}
