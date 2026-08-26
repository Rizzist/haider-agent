use haider_protocol::cache::ProviderViewBoundaryV1;
use haider_protocol::provider::Block;

use crate::{
    ANTHROPIC_OAUTH_PROVIDER_NAME, ANTHROPIC_PROVIDER_NAME, BEDROCK_PROVIDER_NAME,
    DEEPSEEK_PROVIDER_NAME, GEMINI_PROVIDER_NAME, KIMI_OAUTH_PROVIDER_NAME, Message, MessageRole,
    OPENAI_OAUTH_PROVIDER_NAME, OPENAI_PROVIDER_NAME, VERTEX_PROVIDER_NAME, XAI_PROVIDER_NAME,
    cacheable_prompt_minimum,
};

/// How a provider/model pair accepts cache placement controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMarkerMode {
    /// Prefix matching and writes are wholly provider-managed.
    Automatic,
    /// Inline Anthropic `cache_control` markers.
    AnthropicExplicit,
    /// Public OpenAI Responses explicit breakpoints.
    OpenAiExplicit,
    /// A separately-created cache resource (currently Gemini).
    ExplicitResource,
    /// No verified cache-placement surface.
    Unsupported,
}

/// Published cache-write price relative to ordinary input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheWritePrice {
    pub default_multiplier: f64,
    pub five_minute_multiplier: f64,
    pub one_hour_multiplier: f64,
}

/// Capability row used for placement, never inferred from a lookalike custom
/// model. Missing token minimums stay `None` rather than becoming folklore.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CachePlacementCapabilities {
    pub marker_mode: CacheMarkerMode,
    pub marker_budget: u8,
    pub minimum_tokens_per_breakpoint: Option<u64>,
    pub ttl_options_ms: &'static [u64],
    pub cache_write_price: CacheWritePrice,
    pub excludes_live_zone: bool,
    pub tool_pair_atomicity: bool,
}

const NO_TTLS: &[u64] = &[];
const ANTHROPIC_TTLS: &[u64] = &[5 * 60 * 1_000, 60 * 60 * 1_000];
const OPENAI_EXPLICIT_TTLS: &[u64] = &[30 * 60 * 1_000];
const GEMINI_TTLS: &[u64] = &[60 * 60 * 1_000];

const DEFAULT_WRITE_PRICE: CacheWritePrice = CacheWritePrice {
    default_multiplier: 1.0,
    five_minute_multiplier: 1.0,
    one_hour_multiplier: 1.0,
};

/// Exact provider/model cache-placement capabilities.
///
/// OpenAI OAuth is intentionally automatic: the responses-lite transport
/// rejects the public API's explicit breakpoint field. DeepSeek is also
/// automatic and therefore always has a zero marker budget.
#[must_use]
pub fn cache_placement_capabilities(provider: &str, model: &str) -> CachePlacementCapabilities {
    let cache_write_price = cache_write_price(provider, model);
    if matches!(
        provider,
        ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME
    ) {
        return CachePlacementCapabilities {
            marker_mode: CacheMarkerMode::AnthropicExplicit,
            marker_budget: 4,
            minimum_tokens_per_breakpoint: cacheable_prompt_minimum(provider, model),
            ttl_options_ms: ANTHROPIC_TTLS,
            cache_write_price,
            excludes_live_zone: true,
            tool_pair_atomicity: true,
        };
    }
    if provider == OPENAI_PROVIDER_NAME && is_gpt_56_or_later_verified(model) {
        return CachePlacementCapabilities {
            marker_mode: CacheMarkerMode::OpenAiExplicit,
            marker_budget: 2,
            minimum_tokens_per_breakpoint: cacheable_prompt_minimum(provider, model),
            ttl_options_ms: OPENAI_EXPLICIT_TTLS,
            cache_write_price,
            excludes_live_zone: true,
            tool_pair_atomicity: true,
        };
    }
    if provider == GEMINI_PROVIDER_NAME && cacheable_prompt_minimum(provider, model).is_some() {
        return CachePlacementCapabilities {
            marker_mode: CacheMarkerMode::ExplicitResource,
            marker_budget: 1,
            minimum_tokens_per_breakpoint: cacheable_prompt_minimum(provider, model),
            ttl_options_ms: GEMINI_TTLS,
            cache_write_price,
            excludes_live_zone: true,
            tool_pair_atomicity: true,
        };
    }
    if matches!(
        provider,
        DEEPSEEK_PROVIDER_NAME
            | OPENAI_OAUTH_PROVIDER_NAME
            | KIMI_OAUTH_PROVIDER_NAME
            | XAI_PROVIDER_NAME
    ) {
        return CachePlacementCapabilities {
            marker_mode: CacheMarkerMode::Automatic,
            marker_budget: 0,
            minimum_tokens_per_breakpoint: cacheable_prompt_minimum(provider, model),
            ttl_options_ms: NO_TTLS,
            cache_write_price,
            excludes_live_zone: true,
            tool_pair_atomicity: true,
        };
    }
    if matches!(provider, BEDROCK_PROVIDER_NAME | VERTEX_PROVIDER_NAME) {
        // The documented thresholds remain available to diagnostics, but
        // these adapters do not yet expose a verified marker transport.
        return CachePlacementCapabilities {
            marker_mode: CacheMarkerMode::Unsupported,
            marker_budget: 0,
            minimum_tokens_per_breakpoint: cacheable_prompt_minimum(provider, model),
            ttl_options_ms: NO_TTLS,
            cache_write_price,
            excludes_live_zone: true,
            tool_pair_atomicity: true,
        };
    }
    CachePlacementCapabilities {
        marker_mode: CacheMarkerMode::Unsupported,
        marker_budget: 0,
        minimum_tokens_per_breakpoint: None,
        ttl_options_ms: NO_TTLS,
        cache_write_price,
        excludes_live_zone: true,
        tool_pair_atomicity: true,
    }
}

fn cache_write_price(provider: &str, model: &str) -> CacheWritePrice {
    crate::cache_pricing_policy_for(provider, model).map_or(DEFAULT_WRITE_PRICE, |policy| {
        CacheWritePrice {
            default_multiplier: policy.default_write_multiplier,
            five_minute_multiplier: policy.write_5m_multiplier,
            one_hour_multiplier: policy.write_1h_multiplier,
        }
    })
}

fn is_gpt_56_or_later_verified(model: &str) -> bool {
    model == "gpt-5.6" || model.starts_with("gpt-5.6-")
}

/// Adapter-neutral placement result. History boundaries are exclusive source
/// message indexes and are always outside the volatile live zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineBreakpointPlan {
    pub mark_system: bool,
    pub mark_final_tool: bool,
    pub history_ends: Vec<usize>,
}

impl InlineBreakpointPlan {
    #[must_use]
    pub fn ledger_boundaries(&self) -> Vec<ProviderViewBoundaryV1> {
        let mut boundaries = Vec::new();
        if self.mark_system {
            boundaries.push(ProviderViewBoundaryV1 {
                section: "system".into(),
                message_end: None,
            });
        }
        if self.mark_final_tool {
            boundaries.push(ProviderViewBoundaryV1 {
                section: "tools".into(),
                message_end: None,
            });
        }
        boundaries.extend(
            self.history_ends
                .iter()
                .map(|boundary| ProviderViewBoundaryV1 {
                    section: "history".into(),
                    message_end: Some(u64::try_from(*boundary).unwrap_or(u64::MAX)),
                }),
        );
        boundaries
    }
}

/// Places a provider's verified inline marker budget over immutable content.
///
/// Anthropic reserves stable system + final tool slots, then places the two
/// remaining slots at the oldest eligible and newest reusable history
/// boundaries. OpenAI uses two history blocks: the oldest block closes the
/// stable header prefix and the newest closes immutable history. No provider
/// marks the current-user/live tail. Tool-call/result pairs are indivisible.
#[must_use]
// Placement is a pure decision over nine independent provider and request facts.
#[allow(clippy::too_many_arguments)]
pub fn plan_inline_breakpoints(
    provider: &str,
    model: &str,
    messages: &[Message],
    stable_history_end: usize,
    previous_history_end: Option<usize>,
    latest_compaction_summary_end: Option<usize>,
    has_system: bool,
    has_tools: bool,
    stable_prefix_tokens: u64,
) -> InlineBreakpointPlan {
    let capabilities = cache_placement_capabilities(provider, model);
    let stable_history_end = stable_history_end.min(messages.len());
    if !matches!(
        capabilities.marker_mode,
        CacheMarkerMode::AnthropicExplicit | CacheMarkerMode::OpenAiExplicit
    ) || capabilities
        .minimum_tokens_per_breakpoint
        .is_some_and(|minimum| stable_prefix_tokens < minimum)
    {
        return InlineBreakpointPlan {
            mark_system: false,
            mark_final_tool: false,
            history_ends: Vec::new(),
        };
    }

    let mut budget = usize::from(capabilities.marker_budget);
    let mark_system =
        capabilities.marker_mode == CacheMarkerMode::AnthropicExplicit && has_system && budget > 0;
    budget = budget.saturating_sub(usize::from(mark_system));
    let mark_final_tool =
        capabilities.marker_mode == CacheMarkerMode::AnthropicExplicit && has_tools && budget > 0;
    budget = budget.saturating_sub(usize::from(mark_final_tool));

    let newest = atomic_boundary_at_or_before(messages, stable_history_end);
    let preferred_oldest = previous_history_end
        .filter(|boundary| *boundary <= stable_history_end)
        .or(latest_compaction_summary_end.filter(|boundary| *boundary <= stable_history_end))
        .or_else(|| (stable_history_end > 0).then_some(1));
    let oldest = preferred_oldest
        .and_then(|boundary| atomic_boundary_at_or_after(messages, boundary, stable_history_end));
    let mut history_ends = Vec::new();
    if budget > 1
        && let Some(oldest) = oldest
        && Some(oldest) != newest
    {
        history_ends.push(oldest);
    }
    if budget > 0
        && let Some(newest) = newest
    {
        history_ends.push(newest);
    }
    history_ends.sort_unstable();
    history_ends.dedup();
    history_ends.truncate(budget);
    InlineBreakpointPlan {
        mark_system,
        mark_final_tool,
        history_ends,
    }
}

fn atomic_boundary_at_or_before(messages: &[Message], boundary: usize) -> Option<usize> {
    let mut candidate = boundary.min(messages.len());
    while candidate > 0 {
        if !splits_tool_pair(messages, candidate) && annotatable(messages, candidate) {
            return Some(candidate);
        }
        candidate = candidate.saturating_sub(1);
    }
    None
}

fn atomic_boundary_at_or_after(
    messages: &[Message],
    boundary: usize,
    ceiling: usize,
) -> Option<usize> {
    let mut candidate = boundary.max(1);
    let ceiling = ceiling.min(messages.len());
    while candidate <= ceiling {
        if !splits_tool_pair(messages, candidate) && annotatable(messages, candidate) {
            return Some(candidate);
        }
        candidate = candidate.saturating_add(1);
    }
    None
}

fn annotatable(messages: &[Message], boundary: usize) -> bool {
    messages
        .get(boundary.saturating_sub(1))
        .and_then(|message| message.blocks.last())
        .is_some_and(|block| !matches!(block, Block::ProviderOpaque { .. }))
}

fn splits_tool_pair(messages: &[Message], boundary: usize) -> bool {
    let Some(before) = messages.get(boundary.saturating_sub(1)) else {
        return false;
    };
    let Some(after) = messages.get(boundary) else {
        return false;
    };
    before.role == MessageRole::Assistant
        && after.role == MessageRole::Tool
        && before.blocks.iter().any(|block| {
            let Block::ToolCall { call_id, .. } = block else {
                return false;
            };
            after.blocks.iter().any(|candidate| {
                matches!(candidate, Block::ToolResult { call_id: result, .. } if result == call_id)
            })
        })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{ToolDefinition, canonical_tool_definitions};

    #[test]
    fn deepseek_is_automatic_and_never_receives_markers() {
        let capabilities =
            cache_placement_capabilities(DEEPSEEK_PROVIDER_NAME, "deepseek-v4-flash");
        assert_eq!(capabilities.marker_mode, CacheMarkerMode::Automatic);
        assert_eq!(capabilities.marker_budget, 0);
        let messages = vec![Message::user_text("stable"), Message::user_text("live")];
        assert!(
            plan_inline_breakpoints(
                DEEPSEEK_PROVIDER_NAME,
                "deepseek-v4-flash",
                &messages,
                1,
                None,
                None,
                true,
                true,
                8_192,
            )
            .history_ends
            .is_empty()
        );
    }

    #[test]
    fn anthropic_uses_four_slots_without_entering_live_zone() {
        let messages = vec![
            Message::user_text("summary"),
            Message::user_text("old"),
            Message::assistant(vec![Block::Text {
                text: "answer".into(),
            }]),
            Message::user_text("live"),
        ];
        let plan = plan_inline_breakpoints(
            ANTHROPIC_PROVIDER_NAME,
            "claude-opus-5",
            &messages,
            3,
            None,
            Some(1),
            true,
            true,
            8_192,
        );
        assert!(plan.mark_system);
        assert!(plan.mark_final_tool);
        assert_eq!(plan.history_ends, vec![1, 3]);
        assert_eq!(plan.ledger_boundaries().len(), 4);
    }

    #[test]
    fn tool_call_and_result_are_an_atomic_boundary() {
        let messages = vec![
            Message::assistant(vec![Block::ToolCall {
                call_id: "call-1".into(),
                name: "read".into(),
                args: serde_json::json!({}),
            }]),
            Message::tool_result("call-1", "ok", false),
            Message::user_text("live"),
        ];
        let plan = plan_inline_breakpoints(
            OPENAI_PROVIDER_NAME,
            "gpt-5.6",
            &messages,
            2,
            None,
            None,
            true,
            false,
            8_192,
        );
        assert_eq!(plan.history_ends, vec![2]);
    }

    #[test]
    fn below_minimum_and_unverified_openai_models_receive_no_markers() {
        let messages = vec![Message::user_text("stable")];
        assert!(
            plan_inline_breakpoints(
                OPENAI_PROVIDER_NAME,
                "gpt-5.6",
                &messages,
                1,
                None,
                None,
                true,
                false,
                1_023,
            )
            .history_ends
            .is_empty()
        );
        assert_eq!(
            cache_placement_capabilities(OPENAI_PROVIDER_NAME, "gpt-5.5").marker_mode,
            CacheMarkerMode::Unsupported
        );
    }

    #[test]
    fn tool_schema_cache_abi_has_golden_bytes() {
        let tools = vec![
            ToolDefinition {
                name: "z_tool".into(),
                description: "Z".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "a_tool".into(),
                description: "A".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "z": {"type": "string"},
                        "a": {"type": "number"},
                    }
                }),
            },
        ];
        let frozen = canonical_tool_definitions(&tools);
        let bytes = serde_json::to_vec(&frozen).expect("canonical tools serialize");
        assert_eq!(
            bytes,
            br#"[{"name":"a_tool","description":"A","input_schema":{"properties":{"a":{"type":"number"},"z":{"type":"string"}},"type":"object"}},{"name":"z_tool","description":"Z","input_schema":{"type":"object"}}]"#
        );
    }
}
