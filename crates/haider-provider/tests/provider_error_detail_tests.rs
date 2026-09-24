#![allow(clippy::expect_used)]

use haider_provider::{
    ProviderErrorKind, replay_anthropic_http_error, replay_gemini_http_error,
    replay_openai_http_error,
};

type Classifier = fn(u16, Option<&str>, &[u8]) -> haider_provider::ProviderError;
const WITHHELD: &str = "message withheld: may contain account data";

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
        assert_eq!(
            classify(403, None, html.as_bytes()).presentation.detail,
            WITHHELD
        );
        let invalid = classify(403, None, b"\xff\xfeprivate");
        assert!(!invalid.presentation.detail.contains("private"));
    }
}

#[test]
fn suspicious_message_shapes_are_withheld_across_adapters() {
    let messages = [
        "Access denied: alice973@example.test",
        "Access denied: Cookie: session=fixture_973_cookie_secret",
        "Access denied: Set-Cookie: session=fixture_973_cookie_secret",
        "Access denied: acct_973_private_account",
        "Access denied: org_973_private_org",
        "Access denied: user_973_private_user",
        "Access denied: credit_973_private_credit",
        "Access denied: prompt=fixture_973_private_request_payload",
        "Access denied: request_body={\"prompt\":\"private\"}",
        "Access denied: https://example.test/path?token=private",
        "Access denied: Authorization: Bearer fixture-opaque-secret",
        "Access denied: sk-fixture-973-key",
        "Access denied: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.fixture973signature",
        "Invalid shape.\u{1b}[31m",
        "messages.2: tool_use ids were found without tool_result blocks",
    ];
    for classify in classifiers() {
        for message in messages {
            let body = serde_json::json!({"error": {"message": message}}).to_string();
            for input in [body.as_bytes(), message.as_bytes()] {
                assert_eq!(
                    classify(400, None, input).presentation.detail,
                    WITHHELD,
                    "{message}"
                );
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
                        assert_eq!(classify(400, None, input).presentation.detail, WITHHELD);
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
        expected: Option<String>,
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
                if fixture.expected.is_some() {
                    assert_eq!(detail, WITHHELD, "{}", fixture.name);
                }
                assert!(detail.len() <= 512);
            }
        }
    }
    Ok(())
}
