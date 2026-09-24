#![allow(clippy::expect_used)]

use haider_provider::{
    ProviderErrorKind, replay_anthropic_http_error, replay_gemini_http_error,
    replay_openai_http_error, replay_openai_responses_sse,
};

type Classifier = fn(u16, Option<&str>, &[u8]) -> haider_provider::ProviderError;
const WITHHELD: &str = "details withheld";

fn classifiers() -> [Classifier; 3] {
    [
        replay_anthropic_http_error,
        replay_gemini_http_error,
        replay_openai_http_error,
    ]
}

#[test]
fn plain_denial_keeps_diagnostic_value_and_structured_fields() {
    for classify in [replay_anthropic_http_error, replay_openai_http_error] {
        let body = serde_json::json!({"error": {
            "type": "permission_error", "message": "You do not have permission to use this model."
        }})
        .to_string();
        let error =
            classify(403, None, body.as_bytes()).with_http_metadata(403, Some("req_fixture-403"));
        assert_eq!(error.kind, ProviderErrorKind::PermissionDenied);
        assert_eq!(
            error.presentation.detail,
            "You do not have permission to use this model."
        );
        assert_eq!(error.presentation.provider_http_status, Some(403));
        assert_eq!(
            error.presentation.provider_error_type.as_deref(),
            Some("permission_error")
        );
        assert_eq!(
            error.presentation.provider_request_id.as_deref(),
            Some("req_fixture-403")
        );
        let journal = serde_json::to_vec(&error).expect("journal shape serializes");
        let replayed: haider_provider::ProviderError =
            serde_json::from_slice(&journal).expect("journal shape replays");
        assert_eq!(replayed, error);
    }
}

#[test]
fn provider_route_status_matrix_retains_safe_identity() {
    let families: [(&str, Classifier); 4] = [
        ("anthropic", replay_anthropic_http_error),
        ("anthropic-oauth", replay_anthropic_http_error),
        ("openai", replay_openai_http_error),
        ("openai-oauth", replay_openai_http_error),
    ];
    for (family, classify) in families {
        for (status, error_type, kind) in [
            (403, "permission_error", ProviderErrorKind::PermissionDenied),
            (
                401,
                "authentication_error",
                ProviderErrorKind::Authentication,
            ),
            (429, "rate_limit_error", ProviderErrorKind::RateLimited),
            (500, "api_error", ProviderErrorKind::Transport),
        ] {
            let message = "The provider declined this operation.";
            let body =
                serde_json::json!({"type":"error","error":{"type":error_type,"message":message}})
                    .to_string();
            let id = format!("req_{family}_{status}");
            let error =
                classify(status, None, body.as_bytes()).with_http_metadata(status, Some(&id));
            assert_eq!(error.kind, kind, "{family} {status}");
            assert_eq!(error.presentation.detail, message, "{family} {status}");
            assert_eq!(
                error.presentation.provider_error_type.as_deref(),
                Some(error_type)
            );
            assert_eq!(
                error.presentation.provider_request_id.as_deref(),
                Some(id.as_str())
            );
            assert_eq!(error.presentation.provider_http_status, Some(status));
        }
    }
}

#[test]
fn arbitrary_provider_type_and_request_id_are_not_structured_diagnostics() {
    for classify in [replay_anthropic_http_error, replay_openai_http_error] {
        let body = serde_json::json!({"error": {
            "type": "acct_973_private_account", "message": "Access denied."
        }})
        .to_string();
        let error = classify(403, None, body.as_bytes())
            .with_http_metadata(403, Some("req_fixture Cookie: session=private"));
        assert!(error.presentation.provider_error_type.is_none());
        assert!(error.presentation.provider_request_id.is_none());
        assert_eq!(error.presentation.provider_http_status, Some(403));
    }
    let body = br#"{"error":{"type":"acct_private","code":"permission_denied","message":"Access denied."}}"#;
    let error = replay_openai_http_error(403, None, body);
    assert_eq!(
        error.presentation.provider_error_type.as_deref(),
        Some("permission_denied")
    );
}

#[test]
fn request_ids_require_the_exact_bounded_header_shape() {
    let body = br#"{"error":{"type":"permission_error","message":"Access denied."}}"#;
    for request_id in [
        "req_fixture@example.test",
        "req_fixture?account=private",
        "req_fixture Cookie: private",
        "req_acct_973_private_account",
        "acct_973_private_account",
        "req_",
    ] {
        let error =
            replay_openai_http_error(403, None, body).with_http_metadata(403, Some(request_id));
        assert!(
            error.presentation.provider_request_id.is_none(),
            "{request_id}"
        );
        assert_eq!(
            error.presentation.provider_error_type.as_deref(),
            Some("permission_error")
        );
    }
    let long = format!("req_{}", "x".repeat(129));
    assert!(
        replay_openai_http_error(403, None, body)
            .with_http_metadata(403, Some(&long))
            .presentation
            .provider_request_id
            .is_none()
    );
}

#[test]
fn oversized_and_invalid_utf8_provider_bodies_never_publish_fragments() {
    for classify in [replay_anthropic_http_error, replay_openai_http_error] {
        let html = format!("<html><body>{}</body></html>", "x".repeat(16_000));
        assert!(
            classify(403, None, html.as_bytes())
                .presentation
                .detail
                .ends_with(WITHHELD)
        );
        let invalid = classify(403, None, b"\xff\xfeprivate");
        assert!(!invalid.presentation.detail.contains("private"));
        assert!(invalid.presentation.detail.ends_with(WITHHELD));
        let markup = serde_json::json!({"error": {"message": "Invalid request <body>fixture973private</body>"}}).to_string();
        let detail = classify(403, None, markup.as_bytes()).presentation.detail;
        assert!(detail.ends_with(WITHHELD));
        assert!(!detail.contains("fixture973private"));
    }
}

#[test]
fn suspicious_message_shapes_are_scrubbed_across_adapters() {
    let messages = [
        (
            "Access denied: alice973@example.test",
            "alice973@example.test",
        ),
        (
            "Access denied: Cookie: session=fixture_973_cookie_secret",
            "fixture_973_cookie_secret",
        ),
        (
            "Access denied: Set-Cookie: session=fixture_973_cookie_secret",
            "fixture_973_cookie_secret",
        ),
        (
            "Access denied: acct_973_private_account",
            "acct_973_private_account",
        ),
        ("Access denied: org_973_private_org", "org_973_private_org"),
        (
            "Access denied: user_973_private_user",
            "user_973_private_user",
        ),
        (
            "Access denied: credit_973_private_credit",
            "credit_973_private_credit",
        ),
        (
            "Access denied: prompt=fixture_973_private_request_payload",
            "fixture_973_private_request_payload",
        ),
        (
            "Access denied: request_body={\"prompt\":\"private\"}",
            "private",
        ),
        (
            "Access denied: https://example.test/path?token=private",
            "token=private",
        ),
        (
            "Access denied: Authorization: Bearer fixture-opaque-secret",
            "fixture-opaque-secret",
        ),
        (
            "Invalid request; \"fixturehead Authorization: Bearer fixturetail\"; retry.",
            "fixturehead",
        ),
        (
            "Invalid request; \"fixturehead Authorization: Bearer fixturetail\"; retry.",
            "fixturetail",
        ),
        ("Access denied: sk-fixture-973-key", "sk-fixture-973-key"),
        (
            "Access denied: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.fixture973signature",
            "eyJhbGciOiJIUzI1NiJ9",
        ),
        ("Invalid shape.\u{1b}[31m", "\u{1b}"),
    ];
    for classify in classifiers() {
        for (message, secret) in messages {
            let body = serde_json::json!({"error": {"message": message}}).to_string();
            for input in [body.as_bytes(), message.as_bytes()] {
                let detail = classify(400, None, input).presentation.detail;
                assert!(!detail.contains(secret), "{message}: {detail}");
            }
        }
    }
}

#[test]
fn credential_boundary_matrix_remains_fail_closed() {
    for classify in classifiers() {
        for introducer in [
            "proxy-authorization",
            "authorization",
            "x-api-key",
            "x_api_key",
            "api-key",
            "api_key",
            "apikey",
            "access_token",
            "refresh_token",
            "Bearer",
            "Authorization:Bearer",
            "echoed=",
        ] {
            for separator in [" ", "\n\t", ":", ":\n", "=", " = ", " : = \n"] {
                for quote in ['"', '\''] {
                    let message = format!(
                        "Invalid request; {introducer}{separator}{quote}opaque-openhead, \\{quote}escaped\n api_key=inner, opentail"
                    );
                    let body = serde_json::json!({"error": {"message": message}}).to_string();
                    for input in [body.as_bytes(), message.as_bytes()] {
                        assert!(
                            classify(400, None, input)
                                .presentation
                                .detail
                                .ends_with(WITHHELD)
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn previous_credential_corpus_remains_private() -> Result<(), serde_json::Error> {
    #[derive(serde::Deserialize)]
    struct Fixture {
        name: String,
        detail: String,
        secrets: Vec<String>,
    }
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("fixtures/provider_error_details.json"))?;
    assert!(!fixtures.is_empty());
    for classify in classifiers() {
        for fixture in &fixtures {
            let body = serde_json::json!({"error": {"message": fixture.detail}}).to_string();
            for input in [body.as_bytes(), fixture.detail.as_bytes()] {
                let detail = classify(400, None, input).presentation.detail;
                for secret in &fixture.secrets {
                    assert!(!detail.contains(secret), "{} leaked {secret}", fixture.name);
                }
                assert!(!detail.is_empty(), "{}", fixture.name);
                assert!(detail.len() <= 512);
            }
        }
    }
    Ok(())
}

#[test]
fn useful_provider_messages_survive_scrubbing() {
    for (classify, message, expected) in [
        (
            replay_anthropic_http_error as Classifier,
            "Overloaded",
            "Overloaded",
        ),
        (
            replay_anthropic_http_error,
            "Your organization does not have access to this model.",
            "Your organization does not have access to this model.",
        ),
        (
            replay_anthropic_http_error,
            "prompt is too long: 120001 tokens > 100000 maximum",
            "prompt is too long: 120001 tokens > 100000 maximum",
        ),
        (
            replay_anthropic_http_error,
            "Your credit balance is too low to access the Anthropic API…",
            "Your credit balance is too low to access the Anthropic API…",
        ),
        (
            replay_openai_http_error,
            "Unsupported parameter: service_tier",
            "Unsupported parameter: service_tier",
        ),
        (
            replay_openai_http_error,
            "Unknown field: metadata",
            "Unknown field: metadata",
        ),
        (
            replay_openai_http_error,
            "The model `x` does not exist or you do not have access to it.",
            "The model `x` does not exist or you do not have access to it.",
        ),
        (
            replay_openai_http_error,
            "You exceeded your current quota. Read the docs: https://platform.openai.com/docs/guides/error-codes?token=fixture",
            "You exceeded your current quota. Read the docs: https://platform.openai.com",
        ),
        (
            replay_openai_http_error,
            "Your authentication token has been invalidated. Please sign in again.",
            "Your authentication token has been invalidated. Please sign in again.",
        ),
        (
            replay_gemini_http_error,
            "Invalid JSON payload received. Unknown name \"toolConfig\"",
            "Invalid JSON payload received. Unknown name \"toolConfig\"",
        ),
        (
            replay_gemini_http_error,
            "INVALID_ARGUMENT: Request contains an invalid argument.",
            "INVALID_ARGUMENT: Request contains an invalid argument.",
        ),
    ] {
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        let detail = classify(400, None, body.as_bytes()).presentation.detail;
        assert_eq!(detail, expected, "{message}");
    }
    for word in [
        "forgot", "users", "nobody", "promptly", "tokens", "sessions",
    ] {
        let message = format!("The {word} field is valid.");
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        assert_eq!(
            replay_openai_http_error(400, None, body.as_bytes())
                .presentation
                .detail,
            message
        );
    }
    let gemini = replay_gemini_http_error(
        400,
        None,
        br#"{"error":{"status":"INVALID_ARGUMENT","message":"Invalid JSON payload received. Unknown name \"toolConfig\""}}"#,
    );
    assert_eq!(
        gemini.presentation.detail,
        "Invalid JSON payload received. Unknown name \"toolConfig\""
    );
    assert!(gemini.presentation.provider_error_type.is_none());
}

#[test]
fn redaction_pairs_never_expose_synthetic_account_values() {
    let cases = [
        (
            "Access denied for alice973@example.test; retry later.",
            "alice973@example.test",
        ),
        (
            "Your organization named Willow Fixture Labs lacks access.",
            "Willow Fixture Labs",
        ),
        ("Your organization Acme, Inc lacks access.", "Acme, Inc"),
        (
            "The account (acct_973_private_account) has expired.",
            "acct_973_private_account",
        ),
        (
            "See https://example.test/private?token=fixture973query for quota details.",
            "fixture973query",
        ),
        (
            "See https://fixtureuser:fixturepass@example.test/private?token=fixture973query for quota details.",
            "fixturepass",
        ),
        (
            "See https://org_973_private_org.example.test/private for quota details.",
            "org_973_private_org",
        ),
        (
            "Invalid payload {\"prompt\":\"fixture973body\"}; retry.",
            "fixture973body",
        ),
        (
            "Invalid token QWxhZGRpbjpvcGVuIHNlc2FtZV9maXh0dXJlOTcz; retry.",
            "QWxhZGRpbjpvcGVuIHNlc2FtZV9maXh0dXJlOTcz",
        ),
        (
            "Invalid digest 0123456789abcdef0123456789abcdef; retry.",
            "0123456789abcdef0123456789abcdef",
        ),
        (
            "The org (оrg_973_private) is unavailable.",
            "оrg_973_private",
        ),
        (
            "Access denied for alice973@exam\u{200b}ple.test.",
            "alice973@exam\u{200b}ple.test",
        ),
    ];
    for classify in classifiers() {
        for (message, secret) in cases {
            let body = serde_json::json!({"error": {"message": message}}).to_string();
            let detail = classify(400, None, body.as_bytes()).presentation.detail;
            assert!(!detail.contains(secret), "{message}: {detail}");
            assert!(detail.len() <= 512);
        }
        let email = serde_json::json!({"error": {"message": "Access denied for alice973@example.test; retry later."}}).to_string();
        assert_eq!(
            classify(400, None, email.as_bytes()).presentation.detail,
            "Access denied for [REDACTED]; retry later."
        );
        let json = serde_json::json!({"error": {"message": "Invalid payload {\"prompt\":\"fixture973body\"}; retry."}}).to_string();
        assert_eq!(
            classify(400, None, json.as_bytes()).presentation.detail,
            "Invalid payload [REDACTED]; retry."
        );
        let two = serde_json::json!({"error": {"message": "Invalid {\"prompt\":\"fixtureA\"} and {\"token\":\"fixtureB\"}; retry."}}).to_string();
        assert_eq!(
            classify(400, None, two.as_bytes()).presentation.detail,
            "Invalid [REDACTED] and [REDACTED]; retry."
        );
        let url = serde_json::json!({"error": {"message": "See https://example.test/private?token=fixture973query for quota details."}}).to_string();
        assert_eq!(
            classify(400, None, url.as_bytes()).presentation.detail,
            "See https://example.test for quota details."
        );
    }
    let near_cut = format!("{} alice973@example.test", "safe ".repeat(101));
    let body = serde_json::json!({"error": {"message": near_cut}}).to_string();
    let detail = replay_openai_http_error(400, None, body.as_bytes())
        .presentation
        .detail;
    assert!(detail.ends_with(WITHHELD));
    assert!(!detail.contains("alice973@example.test"));

    let malformed =
        serde_json::json!({"error": {"message": "Invalid payload {\"prompt\":\"fixture973body\""}})
            .to_string();
    let detail = replay_openai_http_error(400, None, malformed.as_bytes())
        .presentation
        .detail;
    assert!(detail.starts_with("The provider could not accept this request shape."));
    assert!(detail.ends_with(WITHHELD));
    assert!(!detail.contains("fixture973body"));
}

#[test]
fn openai_safe_code_then_safe_type_classification_precedence() {
    let unknown_code =
        br#"{"error":{"code":"private_code","type":"rate_limit_error","message":"Please wait."}}"#;
    let error = replay_openai_http_error(400, None, unknown_code);
    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert_eq!(
        error.presentation.provider_error_type.as_deref(),
        Some("rate_limit_error")
    );

    let conflicting = br#"{"error":{"code":"insufficient_quota","type":"invalid_request_error","message":"Quota exhausted."}}"#;
    let error = replay_openai_http_error(400, None, conflicting);
    assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    assert_eq!(
        error.presentation.provider_error_type.as_deref(),
        Some("invalid_request_error")
    );

    let stream = replay_openai_responses_sse(
        b"event: error\ndata: {\"error\":{\"code\":\"private_code\",\"type\":\"rate_limit_error\",\"message\":\"Please wait.\"}}\n\n",
    );
    let error = stream
        .into_iter()
        .next()
        .expect("stream item")
        .expect_err("error frame");
    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert_eq!(
        error.presentation.provider_error_type.as_deref(),
        Some("rate_limit_error")
    );
}

#[test]
fn post_scrub_byte_limit_never_cuts_a_unicode_scalar_or_secret() {
    let exact = format!("{}go", "ok ".repeat(170));
    assert_eq!(exact.len(), 512);
    let body = serde_json::json!({"error": {"message": exact}}).to_string();
    assert_eq!(
        replay_openai_http_error(400, None, body.as_bytes())
            .presentation
            .detail,
        exact
    );
    let one_byte_over = format!("{}goo", "ok ".repeat(170));
    assert_eq!(one_byte_over.len(), 513);
    let body = serde_json::json!({"error": {"message": one_byte_over}}).to_string();
    assert!(
        replay_openai_http_error(400, None, body.as_bytes())
            .presentation
            .detail
            .ends_with(WITHHELD)
    );
    let over = format!("{}🦀", "ok ".repeat(170));
    let body = serde_json::json!({"error": {"message": over}}).to_string();
    let detail = replay_openai_http_error(400, None, body.as_bytes())
        .presentation
        .detail;
    assert!(detail.ends_with(WITHHELD));
    assert!(!detail.contains('🦀'));
}
