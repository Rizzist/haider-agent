use crate::{
    has_subscription_static_catalog, model_list_forbidden, static_model_limits,
    subscription_static_model_ids, subscription_static_models,
};

#[test]
fn subscription_fallbacks_have_distinct_ids_and_limit_metadata() {
    for provider in [
        "anthropic-oauth",
        "openai-oauth",
        "kimi-oauth",
        "grok-oauth",
        "haider-code",
    ] {
        let rows = subscription_static_models(provider);
        assert!(!rows.is_empty(), "{provider}");
        let mut ids = std::collections::HashSet::new();
        for row in rows {
            assert!(ids.insert(row.slug.clone()), "duplicate {}", row.slug);
            let limit = static_model_limits(provider, &row.slug);
            assert!(limit.max_output_tokens > 0);
            assert!(
                limit.context_window.is_some(),
                "missing context for {}",
                row.slug
            );
            assert_eq!(row.context_window, limit.context_window);
            // Every maintained row has a sourced limit, never the 8,192 guess.
            assert!(
                limit.output_limit_sourced,
                "unsourced limit for {}",
                row.slug
            );
        }
    }
    assert!(subscription_static_models("custom").is_empty());
    for (provider, model, context, output) in [
        (
            "anthropic-oauth",
            "claude-fable-5-1",
            Some(1_000_000),
            128_000,
        ),
        (
            "anthropic-oauth",
            "claude-haiku-4-5-20251001",
            Some(200_000),
            64_000,
        ),
        ("openai-oauth", "gpt-5.6-sol", Some(1_050_000), 128_000),
        ("openai-oauth", "gpt-6-sol", Some(1_050_000), 128_000),
        // Kimi and Haider Code publish no output ceiling: the shared default
        // budget is their sourced maximum. xAI documents 128,000.
        (
            "kimi-oauth",
            "kimi-for-coding",
            Some(1_048_576),
            crate::DEFAULT_OUTPUT_LIMIT,
        ),
        ("grok-oauth", "grok-4.6", Some(500_000), 128_000),
    ] {
        let limits = static_model_limits(provider, model);
        assert_eq!(
            (limits.context_window, limits.max_output_tokens),
            (context, output)
        );
    }
}

#[test]
fn static_rows_follow_the_id_list_order_and_presence() {
    for provider in [
        "anthropic-oauth",
        "openai-oauth",
        "kimi-oauth",
        "grok-oauth",
        "haider-code",
        "anthropic",
        "custom",
    ] {
        let ids = subscription_static_model_ids(provider);
        let rows = subscription_static_models(provider);
        assert_eq!(has_subscription_static_catalog(provider), !ids.is_empty());
        assert_eq!(
            rows.iter().map(|row| row.slug.as_str()).collect::<Vec<_>>(),
            ids
        );
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.priority, Some(index as i64));
            assert_eq!(row.display_name, row.slug);
            assert!(row.visible);
        }
    }
}

#[test]
fn only_a_403_model_list_refusal_is_informational() {
    assert!(model_list_forbidden(
        "provider does not serve a model list to this credential (403)"
    ));
    assert!(!model_list_forbidden(
        "provider does not serve a model list to this credential (404)"
    ));
    assert!(!model_list_forbidden("server returned 403 for /models"));
}

#[test]
fn openai_oauth_static_rows_are_exactly_the_lite_servable_six() {
    let rows = subscription_static_models("openai-oauth");
    assert_eq!(
        rows.iter().map(|row| row.slug.as_str()).collect::<Vec<_>>(),
        [
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ]
    );
    assert!(rows.iter().all(|row| row.use_responses_lite == Some(true)));
    assert!(
        rows.iter()
            .all(|row| crate::model_servable_by_endpoint("openai-oauth", row))
    );
}
