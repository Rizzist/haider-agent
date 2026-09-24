#![allow(clippy::expect_used)]

use haider_provider::acp::client::AcpError;
use haider_provider::acp::wire::JsonRpcError;
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
fn marked_account_names_and_tenant_hosts_do_not_publish() {
    for (message, private) in [
        (
            "Your organization cedarbranch has no access.",
            "cedarbranch",
        ),
        (
            "Your organization 'cedarbranch labs' has no access.",
            "cedarbranch",
        ),
        (
            "Your organization “cedarbranch labs” has no access.",
            "cedarbranch",
        ),
        (
            "Your organization ©cedarbranch has no access.",
            "cedarbranch",
        ),
        ("Your org Cedarbranch Labs has no access.", "Cedarbranch"),
        ("Your organization Does Labs has no access.", "Does Labs"),
        ("The workspace northquay lacks permission.", "northquay"),
        ("The project beaconridge is disabled.", "beaconridge"),
        ("Team silver meadow is disabled.", "silver meadow"),
        ("TEAM: silver meadow is disabled.", "silver meadow"),
        ("The team name silver meadow is disabled.", "silver meadow"),
        ("tenant 'cedarbranch labs' is disabled.", "cedarbranch"),
        ("account cedarbranch is disabled.", "cedarbranch"),
        (
            "Your organization cеdarbranch has no access.",
            "cеdarbranch",
        ),
        (
            "See https://cedarbranch.synthetic.example/private?x=1",
            "cedarbranch",
        ),
    ] {
        for classify in [replay_anthropic_http_error, replay_openai_http_error] {
            let body = serde_json::json!({"error": {"message": message}}).to_string();
            let detail = classify(403, None, body.as_bytes()).presentation.detail;
            assert!(!detail.contains(private), "{message}: {detail}");
        }
    }
}

#[test]
fn account_names_are_scrubbed_in_place_across_label_shapes() {
    for (message, private) in [
        ("Your organization is cedarbranch.", "cedarbranch"),
        (
            "Your account, cedarbranch labs, has no access.",
            "cedarbranch",
        ),
        ("ORGANIZATION cedarbranch has no access.", "cedarbranch"),
        (
            "Your organization \u{201c}cedarbranch labs has no access.",
            "cedarbranch",
        ),
        (
            "Your organization `cedarbranch` has no access.",
            "cedarbranch",
        ),
        ("The project (beaconridge) is disabled.", "beaconridge"),
        ("Organisation northquay lacks permission.", "northquay"),
        ("Customer cedarbranch was suspended.", "cedarbranch"),
        ("The user cedarbranch cannot use this model.", "cedarbranch"),
        ("Denied: tenant - cedarbranch - is disabled.", "cedarbranch"),
        ("The team Is Cedarbranch is disabled.", "Cedarbranch"),
    ] {
        for classify in classifiers() {
            let body = serde_json::json!({"error": {"message": message}}).to_string();
            let detail = classify(403, None, body.as_bytes()).presentation.detail;
            assert!(!detail.contains(private), "{message}: {detail}");
        }
    }
    for (message, expected) in [
        (
            "Your organization cedarbranch has no access to this model.",
            "Your organization [REDACTED] has no access to this model.",
        ),
        (
            "Your organization 'cedarbranch labs' has no access.",
            "Your organization [REDACTED] has no access.",
        ),
        (
            "Rate limit reached for gpt-4o in organization cedarbranch on tokens per min.",
            "Rate limit reached for gpt-4o in organization [REDACTED] on tokens per min.",
        ),
        (
            "Your organization is cedarbranch.",
            "Your organization is [REDACTED].",
        ),
        (
            "The workspace northquay lacks permission.",
            "The workspace [REDACTED] lacks permission.",
        ),
    ] {
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        assert_eq!(
            replay_openai_http_error(403, None, body.as_bytes())
                .presentation
                .detail,
            expected
        );
    }
}

#[test]
fn ordinary_account_prose_is_not_mistaken_for_a_name() {
    for message in [
        "Your organization does not have access to this model.",
        "Your organization is not allowed to use this model.",
        "Your account is not active, please check your billing details.",
        "Your organization has been disabled.",
        "This organization is currently suspended.",
        "You must be a member of an organization to use the API.",
        "Please contact your organization administrator.",
        "Check billing or switch account.",
        "The first message must use the user role.",
        "Roles must alternate between \"user\" and \"assistant\".",
        "Your project quota was exceeded.",
        "The account associated with this API key has been deactivated.",
    ] {
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        for classify in [replay_anthropic_http_error, replay_openai_http_error] {
            assert_eq!(
                classify(403, None, body.as_bytes()).presentation.detail,
                message
            );
        }
    }
}

#[test]
fn grammatical_account_prose_and_public_documentation_hosts_remain_useful() {
    for predicate in [
        "does",
        "doesn't",
        "is",
        "isn't",
        "has",
        "hasn't",
        "have",
        "lacks",
        "cannot",
        "can't",
        "may",
        "must",
        "should",
        "was",
        "will",
        "not",
        "currently",
    ] {
        let message = format!("Your organization {predicate} access to this model.");
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        assert_eq!(
            replay_openai_http_error(403, None, body.as_bytes())
                .presentation
                .detail,
            message
        );
    }
    for host in [
        "platform.openai.com",
        "status.anthropic.com",
        "ai.google.dev",
    ] {
        let message = format!("See https://{host}/private?token=fixture for help.");
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        let detail = replay_openai_http_error(403, None, body.as_bytes())
            .presentation
            .detail;
        assert!(detail.contains(&format!("https://{host}")), "{detail}");
        assert!(!detail.contains("fixture"), "{detail}");
    }
    for host in [
        "cedarbranch.synthetic.example",
        "tenant.status.openai.com",
        "api.openai.com.synthetic.example",
    ] {
        let message = format!("See https://{host}/private?token=fixture for help.");
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        let detail = replay_openai_http_error(403, None, body.as_bytes())
            .presentation
            .detail;
        assert!(detail.contains("[link removed]"), "{detail}");
        assert!(!detail.contains(host), "{detail}");
    }
    let message = "See cedarbranch://api.openai.com/private for help.";
    let body = serde_json::json!({"error": {"message": message}}).to_string();
    let detail = replay_openai_http_error(403, None, body.as_bytes())
        .presentation
        .detail;
    assert!(!detail.contains("cedarbranch"), "{detail}");
}

#[test]
fn acp_conversion_never_retains_child_prose_in_the_message_or_journal() {
    let private = "robin.verify2@synthetic.example";
    let error = AcpError::Rpc(JsonRpcError {
        code: -32000,
        message: format!("Permission denied for {private}"),
        data: None,
    })
    .into_provider_error(&format!("agent stderr: account {private}"));
    assert!(!error.message.contains(private));
    assert!(!error.presentation.detail.contains(private));
    assert!(!error.public_message().contains(private));
    let journal = serde_json::to_string(&error).expect("serialize ACP provider error");
    assert!(!journal.contains(private));
    let injected = haider_provider::ProviderError::new(
        ProviderErrorKind::PermissionDenied,
        format!("Permission denied for {private}"),
    );
    assert!(!injected.public_message().contains(private));
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
            "See [link removed] for quota details."
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

#[test]
fn acp_auth_method_ids_keep_protocol_names_but_not_account_values() {
    let error = AcpError::AuthMethodUnavailable {
        advertised: vec![
            "gemini-api-key".to_owned(),
            "robin.verify2@synthetic.example".to_owned(),
            "0123456789abcdef0123456789abcdef".to_owned(),
        ],
    }
    .into_provider_error("");
    assert!(
        error.message.contains("gemini-api-key"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("robin.verify2"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("0123456789abcdef"),
        "{}",
        error.message
    );
    assert!(!error.public_message().contains("robin.verify2"));
}

#[test]
fn local_timeout_telemetry_survives_the_public_message_policy() {
    let error = haider_provider::deadline_exhausted_error(
        std::time::Duration::from_millis(2_000),
        std::time::Duration::from_millis(1_500),
    );
    let message = error.public_message();
    assert!(message.contains("reason=deadline_exhausted"), "{message}");
    assert!(message.contains("opened_within_ms=1500"), "{message}");
}
