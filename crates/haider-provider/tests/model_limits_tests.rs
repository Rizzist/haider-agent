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
            output_limit_sourced: true,
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
                output_limit_sourced: true,
            }
        );
    }
    assert_eq!(
        static_model_limits("bedrock", "anthropic.claude-opus-4-8"),
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
            output_limit_sourced: true,
        }
    );
    assert_eq!(
        static_model_limits("vertex", "claude-sonnet-4-5@20250929"),
        StaticModelLimits {
            context_window: Some(200_000),
            max_output_tokens: 64_000,
            output_limit_sourced: true,
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
            output_limit_sourced: true,
        }
    );
    assert_eq!(
        static_model_limits("deepseek", "deepseek-flash"),
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 384_000,
            output_limit_sourced: true,
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

/// Gemini 3.x IDs with official model pages (checked 2026-09-25): 1,048,576
/// input / 65,536 output. Antigravity's `-high` spelling shares the row.
#[test]
fn documented_gemini_3_rows_are_sourced() {
    for model in [
        "gemini-3.8-flash",
        "gemini-3.8-flash-high",
        "gemini-3.7-flash",
        "gemini-3.6-flash",
        "gemini-3.5-flash-lite",
        "gemini-3.1-pro-preview",
        "gemini-3.1-flash-lite",
    ] {
        assert_eq!(
            static_model_limits("gemini", model),
            StaticModelLimits {
                context_window: Some(1_048_576),
                max_output_tokens: 65_536,
                output_limit_sourced: true,
            },
            "{model}"
        );
    }
    assert!(!static_model_limits("gemini", "gemini-3.8-flash-tts").output_limit_sourced);
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
        8_000,
        "an output maximum that would reach a known context window is cut to half of it"
    );
    assert_eq!(
        model_output_limit("custom", "future-model", Some(15_999), Some(16_000)),
        15_999,
        "a maximum strictly below the window is kept"
    );
    assert_eq!(
        model_output_limit("gemini", "gemini-3-pro-image-preview", None, Some(65_536)),
        32_768
    );
}

/// S1: an unverified fallback maximum is a guess, so explicit budgets may go
/// up to the adapter maximum (strictly below a known window); a published row
/// or a catalog declaration is exact.
#[test]
fn explicit_ceiling_lifts_only_the_unverified_fallback() {
    use haider_provider::{
        OutputLimitProvenance, UNKNOWN_OUTPUT_LIMIT, explicit_output_ceiling,
        output_limit_provenance,
    };
    for (provider, model) in [
        ("custom973", "custom-unknown"),
        ("anthropic", "claude-sonnet-4-20250514"),
        ("anthropic-oauth", "claude-3-7-sonnet-latest"),
        ("gemini", "gemini-9-unlisted"),
        ("openai", "gpt-unknown"),
        ("deepseek", "deepseek-reasoner"),
    ] {
        assert!(!static_model_limits(provider, model).output_limit_sourced);
        let provenance = output_limit_provenance(provider, model, None);
        assert_eq!(provenance, OutputLimitProvenance::Fallback);
        let projected = model_output_limit(provider, model, None, None);
        assert_eq!(projected, UNKNOWN_OUTPUT_LIMIT, "{provider}/{model}");
        assert_eq!(
            explicit_output_ceiling(provenance, projected, None),
            MAX_OUTPUT_LIMIT,
            "{provider}/{model}"
        );
        let windowed = model_output_limit(provider, model, None, Some(100_000));
        assert_eq!(
            explicit_output_ceiling(provenance, windowed, Some(100_000)),
            50_000,
            "{provider}/{model} stays strictly below a known window"
        );
    }
    // A catalog that declares a different limit for an unsourced row is exact.
    let provenance = output_limit_provenance("custom973", "custom-unknown", Some(16_000));
    assert_eq!(provenance, OutputLimitProvenance::Declared);
    let declared = model_output_limit("custom973", "custom-unknown", Some(16_000), None);
    assert_eq!(explicit_output_ceiling(provenance, declared, None), 16_000);
    // Published rows, including a published 8,192, are exact.
    for (provider, model, context_window) in [
        ("gemini", "gemini-2.0-flash", Some(1_048_576)),
        ("openai", "gpt-4o", Some(128_000)),
        ("anthropic", "claude-sonnet-4-5", Some(200_000)),
        ("fake", "fake-model", None),
    ] {
        assert!(static_model_limits(provider, model).output_limit_sourced);
        let provenance = output_limit_provenance(provider, model, None);
        assert_eq!(provenance, OutputLimitProvenance::Static);
        let projected = model_output_limit(provider, model, None, context_window);
        assert_eq!(
            explicit_output_ceiling(provenance, projected, context_window),
            projected,
            "{provider}/{model}"
        );
    }
}

/// 973-output-cap-fixes B2: numeric equality is not provenance. Two rows for
/// the same unlisted Gemini model project the identical 8,192 maximum; the one
/// whose catalog DECLARES 8,192 keeps it as the explicit ceiling, while the
/// one without a declaration (the local fallback) admits a larger explicit
/// budget.
/// MUTATION CHECK: decide the ceiling by comparing the projected value with
/// the fallback (the pre-fix rule). Expected runtime failure: the declared
/// row's ceiling becomes 384,000.
#[test]
fn a_declared_maximum_equal_to_the_fallback_stays_exact() {
    use haider_provider::{
        OutputLimitProvenance, UNKNOWN_OUTPUT_LIMIT, explicit_output_ceiling,
        output_limit_provenance,
    };
    let (provider, model, window) = ("gemini", "gemini-9-unlisted", Some(1_048_576));
    let declared_max = model_output_limit(provider, model, Some(8_192), window);
    let fallback_max = model_output_limit(provider, model, None, window);
    assert_eq!(declared_max, UNKNOWN_OUTPUT_LIMIT);
    assert_eq!(
        fallback_max, declared_max,
        "the two rows are numerically identical"
    );
    let declared = output_limit_provenance(provider, model, Some(8_192));
    let fallback = output_limit_provenance(provider, model, None);
    assert_eq!(declared, OutputLimitProvenance::Declared);
    assert_eq!(fallback, OutputLimitProvenance::Fallback);
    assert_eq!(
        explicit_output_ceiling(declared, declared_max, window),
        8_192
    );
    assert_eq!(
        explicit_output_ceiling(fallback, fallback_max, window),
        MAX_OUTPUT_LIMIT
    );
}

/// 973-output-cap-fixes B1: Kimi Code and Haider Code publish context windows
/// but no output maximum. The window is kept; the output keeps the
/// conservative 30,000 derived default but is NOT a published limit, so an
/// explicit larger budget is admitted within the window. xAI documents its
/// 128,000 per-response value and stays published.
#[test]
fn first_party_windows_do_not_publish_an_output_maximum() {
    use haider_provider::{
        OutputLimitProvenance, UNKNOWN_OUTPUT_LIMIT, explicit_output_ceiling,
        output_limit_provenance,
    };
    let unpublished_output = [
        (
            "kimi-oauth",
            "kimi-for-coding",
            Some(1_048_576),
            30_000,
            384_000,
        ),
        ("kimi-oauth", "k3", Some(262_144), 30_000, 131_072),
        (
            "kimi-oauth",
            "kimi-for-coding-highspeed",
            Some(262_144),
            30_000,
            131_072,
        ),
        ("haider-code", "kimi-k3", Some(1_000_000), 30_000, 384_000),
        (
            "haider-code",
            "deepseek-v4-pro",
            Some(128_000),
            30_000,
            64_000,
        ),
        ("haider-code", "glm-5.3", Some(200_000), 30_000, 100_000),
    ];
    for (provider, model, context_window, max_output_tokens, explicit) in unpublished_output {
        assert_eq!(
            static_model_limits(provider, model),
            StaticModelLimits {
                context_window,
                max_output_tokens,
                output_limit_sourced: false,
            },
            "{provider}/{model}"
        );
        let provenance = output_limit_provenance(provider, model, None);
        assert_eq!(
            provenance,
            OutputLimitProvenance::Fallback,
            "{provider}/{model}"
        );
        let projected = model_output_limit(provider, model, None, context_window);
        assert_eq!(projected, 30_000, "{provider}/{model} derived default");
        let ceiling = explicit_output_ceiling(provenance, projected, context_window);
        assert_eq!(ceiling, explicit, "{provider}/{model}");
        assert!(ceiling >= 60_000, "{provider}/{model} admits 60,000");
    }
    for (provider, model, context_window) in [
        ("xai", "grok-4.6", Some(500_000)),
        ("xai", "grok-4.3-latest", Some(1_000_000)),
        ("grok-oauth", "grok-build-0.1", Some(256_000)),
        ("xai", "grok-code-fast-1", Some(256_000)),
    ] {
        assert_eq!(
            static_model_limits(provider, model),
            StaticModelLimits {
                context_window,
                max_output_tokens: 128_000,
                output_limit_sourced: true,
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
                output_limit_sourced: false,
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
    assert_eq!(
        provider_stated_output_limit(
            &invalid("This model supports at most 10 images per request."),
            30_000
        ),
        None,
        "`supports at most` counts only when a token unit follows the number"
    );
    assert_eq!(
        provider_stated_output_limit(
            &invalid("This endpoint supports at most 8,192 tokens of output."),
            30_000
        ),
        Some(8_192)
    );
}
