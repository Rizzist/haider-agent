//! Cache eligibility machinery shared by provider adapters and the harness.
//!
//! Provider caching remains the provider's job. This module protects the
//! byte prefix offered to that cache, centralizes capability-gated marker
//! placement, and exposes economic measurement over raw request usage.

mod planner;
mod provider_view;
mod telemetry;

pub use planner::{
    CacheMarkerMode, CachePlacementCapabilities, CacheWritePrice, InlineBreakpointPlan,
    cache_placement_capabilities, plan_inline_breakpoints,
};
pub use provider_view::{
    PreparedProviderView, ProviderViewContinuity, ProviderViewInvariantError,
    validate_provider_view_prefix,
};
pub use telemetry::{CacheEconomicSample, CacheScenario, economic_cache_hit_rate};

pub(crate) use provider_view::prepared_serialized_provider_view;
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) use provider_view::{
    prepared_array_provider_view, prepared_array_provider_view_with_system,
};
