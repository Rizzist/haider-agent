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
        static_model_limits("openai-oauth", "gpt-5.6-sol").context_window,
        Some(1_050_000)
    );
    assert_eq!(
        static_model_limits("openai", "gpt-4.1").context_window,
        Some(1_047_576)
    );
    assert_eq!(
        static_model_limits("openai", "gpt-5.4-mini").context_window,
        Some(400_000)
    );
    assert_eq!(
        static_model_limits("openai", "gpt-5.3-chat-latest").max_output_tokens,
        16_384
    );
    for model in ["o3", "o4-mini"] {
        assert_eq!(
            static_model_limits("openai", model),
            StaticModelLimits {
                context_window: Some(200_000),
                max_output_tokens: 100_000,
            }
        );
    }
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
    assert_eq!(
        static_model_limits("fake", "fake-model").max_output_tokens,
        haider_protocol::output_budget::DEFAULT_OUTPUT_LIMIT,
        "the local fake adapter preserves the shared default fixture budget"
    );
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
        None
    );
    assert_eq!(
        static_model_limits("openai", "gpt-5.3").context_window,
        None
    );
    assert_eq!(
        static_model_limits("gemini", "gemini-2.0-flash").context_window,
        Some(1_048_576)
    );
    assert_eq!(
        static_model_limits("gemini", "gemini-1.5-flash").max_output_tokens,
        8_192
    );
    assert_eq!(
        static_model_limits("deepseek", "deepseek-v4-pro"),
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 384_000,
        }
    );
    assert_eq!(
        static_model_limits("deepseek", "deepseek-flash"),
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 384_000,
        },
        "deepseek-flash (V4.1-Flash) is a current documented ID"
    );
    assert_eq!(
        static_model_limits("deepseek", "deepseek-reasoner").max_output_tokens,
        8_192
    );
    assert_eq!(
        static_model_limits("deepseek", "deepseek-reasoner").context_window,
        None
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

#[test]
fn first_party_subscription_rows_are_sourced_and_leave_the_unknown_fallback() {
    use haider_provider::UNKNOWN_OUTPUT_LIMIT;
    let cases = [
        ("kimi-oauth", "kimi-for-coding", Some(1_048_576), 30_000),
        ("kimi-oauth", "k3", Some(262_144), 30_000),
        (
            "kimi-oauth",
            "kimi-for-coding-highspeed",
            Some(262_144),
            30_000,
        ),
        ("xai", "grok-4.6", Some(500_000), 128_000),
        ("xai", "grok-4.3-latest", Some(1_000_000), 128_000),
        ("grok-oauth", "grok-build-0.1", Some(256_000), 128_000),
        ("xai", "grok-code-fast-1", Some(256_000), 128_000),
        ("haider-code", "kimi-k3", Some(1_000_000), 30_000),
        ("haider-code", "deepseek-v4-pro", Some(128_000), 30_000),
        ("haider-code", "glm-5.3", Some(200_000), 30_000),
    ];
    for (provider, model, context_window, max_output_tokens) in cases {
        assert_eq!(
            static_model_limits(provider, model),
            StaticModelLimits {
                context_window,
                max_output_tokens,
            },
            "{provider}/{model}"
        );
    }
    for (provider, model) in [
        ("kimi-oauth", "kimi-future"),
        ("xai", "grok-9"),
        ("haider-code", "qwen3-coder-next"),
        ("custom973", "custom-unknown"),
    ] {
        assert_eq!(
            static_model_limits(provider, model),
            StaticModelLimits {
                context_window: None,
                max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
            },
            "{provider}/{model} is truly unknown"
        );
    }
}

#[test]
fn provider_stated_output_limits_parse_only_lowering_invalid_requests() {
    use haider_provider::{ProviderError, ProviderErrorKind, provider_stated_output_limit};
    let invalid = |message: &str| ProviderError::new(ProviderErrorKind::InvalidRequest, message);
    assert_eq!(
        provider_stated_output_limit(
            &invalid(
                "max_tokens: 30000 > 16000, which is the maximum allowed number of output tokens for claude-x"
            ),
            30_000
        ),
        Some(16_000)
    );
    assert_eq!(
        provider_stated_output_limit(
            &invalid(
                "max_tokens is too large: 30000. This model supports at most 16384 completion tokens, whereas you provided 30000."
            ),
            30_000
        ),
        Some(16_384)
    );
    assert_eq!(
        provider_stated_output_limit(
            &invalid("Invalid max_tokens value, the valid range of max_tokens is [1, 8192]"),
            30_000
        ),
        Some(8_192)
    );
    let mut presented = invalid("Anthropic HTTP 400 returned an invalid request");
    presented.presentation.detail =
        "max_tokens: 30000 > 8192, which is the maximum allowed number of output tokens".into();
    assert_eq!(
        provider_stated_output_limit(&presented, 30_000),
        Some(8_192),
        "the adapter keeps provider wording in the presentation detail"
    );
    assert_eq!(
        provider_stated_output_limit(
            &invalid("the valid range of max_tokens is [1, 393216]"),
            30_000
        ),
        None,
        "a statement that would not lower the budget never retries"
    );
    assert_eq!(
        provider_stated_output_limit(
            &ProviderError::new(
                ProviderErrorKind::ContextExceeded,
                "supports at most 16384 completion tokens"
            ),
            30_000
        ),
        None,
        "only invalid-request errors carry an output maximum"
    );
    assert_eq!(
        provider_stated_output_limit(&invalid("tool schema invalid"), 30_000),
        None
    );
}
