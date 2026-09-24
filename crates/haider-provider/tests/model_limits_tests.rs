use haider_provider::{StaticModelLimits, static_model_limits};

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
