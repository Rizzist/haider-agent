use haider_provider::{
    MAX_OUTPUT_LIMIT, StaticModelLimits, model_output_limit, static_model_limits,
};

#[test]
fn known_subscription_models_have_large_static_limits() {
    assert_eq!(
        static_model_limits("anthropic-oauth", "claude-fable-5-1"),
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
        }
    );
    assert_eq!(
        static_model_limits("openai-oauth", "gpt-5.6-sol").max_output_tokens,
        128_000
    );
    assert_eq!(
        static_model_limits("bedrock", "anthropic.claude-opus-4-8"),
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
        }
    );
    assert_eq!(
        static_model_limits("vertex", "claude-sonnet-4-5@20250929"),
        StaticModelLimits {
            context_window: Some(200_000),
            max_output_tokens: 64_000,
        }
    );
}

#[test]
fn unknown_models_still_clear_the_legacy_ceiling() {
    for provider in [
        "anthropic-oauth",
        "openai-oauth",
        "gemini",
        "deepseek",
        "kimi-oauth",
        "xai",
        "custom",
    ] {
        assert!(
            static_model_limits(provider, "future-model").max_output_tokens > 4_096,
            "{provider} must not regress to the legacy ceiling"
        );
    }
}

#[test]
fn conservative_context_fallbacks_match_adapter_capabilities() {
    assert_eq!(
        static_model_limits("anthropic", "future-model").context_window,
        Some(100_000)
    );
    assert_eq!(
        static_model_limits("openai", "gpt-5.3").context_window,
        Some(400_000)
    );
    assert_eq!(
        static_model_limits("gemini", "gemini-2.0-flash").context_window,
        Some(1_048_576)
    );
}

#[test]
fn projected_output_limit_prefers_the_catalog_and_respects_bounds() {
    assert_eq!(
        model_output_limit("anthropic-oauth", "claude-fable-5-1", None, Some(1_000_000)),
        128_000,
        "an undeclared row uses the pinned table"
    );
    assert_eq!(
        model_output_limit("anthropic-oauth", "claude-fable-5-1", Some(20_000), None),
        20_000,
        "a catalog declaration wins over the pinned table"
    );
    assert_eq!(
        model_output_limit("custom", "future-model", Some(10_000_000), None),
        MAX_OUTPUT_LIMIT
    );
    assert_eq!(
        model_output_limit("custom", "future-model", Some(64_000), Some(16_000)),
        16_000,
        "the output maximum never exceeds a known context window"
    );
}
