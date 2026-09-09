use haider_provider::{
    ProviderErrorKind, replay_anthropic_http_error, replay_gemini_http_error,
    replay_openai_http_error,
};

#[test]
fn rejected_requests_keep_bounded_provider_diagnostics() {
    for classify in [
        replay_anthropic_http_error,
        replay_openai_http_error,
        replay_gemini_http_error,
    ] {
        let detail = "messages.2: tool_use ids were found without tool_result blocks immediately after: registration";
        let body = serde_json::json!({"error": {
            "type": "invalid_request_error", "status": "INVALID_ARGUMENT", "message": detail
        }})
        .to_string();
        let error =
            classify(400, None, body.as_bytes()).with_http_metadata(400, Some("req-fixture-400"));
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(error.presentation.detail, detail);
        assert_eq!(error.presentation.provider_http_status, Some(400));
        assert_eq!(
            error.presentation.provider_request_id.as_deref(),
            Some("req-fixture-400")
        );
        assert!(!error.retryable);

        let without_type =
            serde_json::json!({"error": {"message": detail}, "api_key": "fixture-extra-field"})
                .to_string();
        assert_eq!(
            classify(400, None, without_type.as_bytes())
                .presentation
                .detail,
            detail
        );
        let quoted_secret = serde_json::json!({"error": {"message": "Invalid field: \"api_key\":\"fixture-quoted-secret\""}}).to_string();
        assert!(
            !classify(400, None, quoted_secret.as_bytes())
                .presentation
                .detail
                .contains("fixture-quoted-secret")
        );
        let raw = classify(400, None, b"Bad request: messages[2].content[0] is empty");
        assert_eq!(
            raw.presentation.detail,
            "Bad request: messages[2].content[0] is empty"
        );

        let body = serde_json::json!({"error": {"type": "invalid_request_error", "message": "x".repeat(2048)}}).to_string();
        assert!(
            classify(400, None, body.as_bytes())
                .presentation
                .detail
                .len()
                <= 515
        );
    }
}

#[test]
fn echoed_fixture_credentials_and_terminal_controls_are_not_public() {
    for classify in [
        replay_anthropic_http_error,
        replay_gemini_http_error,
        replay_openai_http_error,
    ] {
        let body = serde_json::json!({"error": {"type": "invalid_request_error", "message":
            "Invalid shape. Authorization: Bearer fixture-opaque-value sk-fixture-secret-value\u{1b}[31m"
        }}).to_string();
        let detail = classify(400, None, body.as_bytes()).presentation.detail;
        assert!(detail.starts_with("Invalid shape."));
        assert!(!detail.contains("fixture-opaque-value"));
        assert!(!detail.contains("sk-fixture-secret-value"));
        assert!(!detail.contains('\u{1b}'));
    }
}

#[test]
fn inline_authorization_schemes_redact_the_following_credential() {
    for classify in [
        replay_anthropic_http_error,
        replay_gemini_http_error,
        replay_openai_http_error,
    ] {
        let body = serde_json::json!({"error": {"message":
            "Invalid request; Authorization:Bearer fixture-opaque-secret"
        }})
        .to_string();
        assert_eq!(
            classify(400, None, body.as_bytes()).presentation.detail,
            "Invalid request; [REDACTED] [REDACTED]"
        );
        for scheme in ["Bearer", "Basic", "Token", "bEaReR", "bAsIc", "tOkEn"] {
            for header in [
                format!("Authorization:{scheme}"),
                format!("authorization={scheme}"),
                format!("\"Authorization\":\"{scheme}"),
                format!("'Authorization':'{scheme}"),
                format!("Authorization:\"{scheme}\""),
                format!("\"Authorization:{scheme}"),
                format!("Authorization='{scheme}"),
            ] {
                for credential in [
                    "fixture-opaque-secret",
                    "\"fixture-opaque-secret\"",
                    "'fixture-opaque-secret'",
                ] {
                    for whitespace in [" ", "  ", "\n\t", "\u{2003}"] {
                        let message = format!(
                            "Invalid request; {header}{whitespace}{credential} retained detail"
                        );
                        let body = serde_json::json!({"error": {
                            "type": "invalid_request_error", "message": message
                        }})
                        .to_string();
                        let detail = classify(400, None, body.as_bytes()).presentation.detail;
                        let public_whitespace = whitespace.replace('\t', " ");
                        assert_eq!(
                            detail,
                            format!(
                                "Invalid request; [REDACTED]{public_whitespace}[REDACTED] retained detail"
                            ),
                            "input: {message}"
                        );
                    }
                }
            }
            let message = format!("Authorization:  {scheme}\n\t'fixture-opaque-secret' retained");
            let detail = classify(400, None, message.as_bytes()).presentation.detail;
            assert_eq!(detail, "Authorization:  [REDACTED]\n [REDACTED] retained");
        }
    }
}

#[test]
fn redacted_diagnostics_preserve_whitespace_control_sanitization_and_utf8_bound() {
    for classify in [
        replay_anthropic_http_error,
        replay_gemini_http_error,
        replay_openai_http_error,
    ] {
        let message = "messages.2:\n  invalid  tool pairing";
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        assert_eq!(
            classify(400, None, body.as_bytes()).presentation.detail,
            message
        );
        let message = format!(
            "Invalid request; Authorization:Bearer\n  \"fixture-opaque-secret\"\u{1b}[31m {}",
            "界".repeat(300)
        );
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        let detail = classify(400, None, body.as_bytes()).presentation.detail;
        let prefix = "Invalid request; [REDACTED]\n  [REDACTED] ";
        assert_eq!(
            detail,
            format!("{prefix}{}", "界".repeat((512 - prefix.len()) / 3))
        );
        let body = serde_json::json!({"error": {
            "message": "Invalid\u{1b}[31m request; Authorization:Token fixture-opaque-secret"
        }})
        .to_string();
        let detail = classify(400, None, body.as_bytes()).presentation.detail;
        assert!(!detail.contains('\u{1b}'));
        assert!(!detail.contains("fixture-opaque-secret"));
    }
}

#[test]
fn every_credential_in_compact_diagnostics_is_redacted() -> Result<(), serde_json::Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fixture {
        name: String,
        detail: String,
        secrets: Vec<String>,
        expected: Option<String>,
    }

    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("fixtures/provider_error_details.json"))?;
    assert!(!fixtures.is_empty(), "the credential corpus must run");
    for classify in [
        replay_anthropic_http_error,
        replay_gemini_http_error,
        replay_openai_http_error,
    ] {
        for fixture in &fixtures {
            assert!(!fixture.secrets.is_empty() || fixture.expected.is_some());
            let message = &fixture.detail;
            let body = serde_json::json!({"error": {
                "type": "invalid_request_error", "message": message
            }})
            .to_string();
            // The same sanitizer serves structured and plain HTTP error bodies.
            for input in [body.as_bytes(), message.as_bytes()] {
                let error = classify(400, None, input)
                    .with_http_metadata(400, Some("req-multi-header-fixture"));
                let detail = error.presentation.detail;
                for secret in &fixture.secrets {
                    assert!(
                        !detail.contains(secret),
                        "{}: credential survived in {detail:?}",
                        fixture.name
                    );
                }
                if let Some(expected) = &fixture.expected {
                    assert_eq!(&detail, expected, "{}", fixture.name);
                }
                if message.ends_with("retained detail") {
                    assert!(
                        detail.ends_with("retained detail"),
                        "{}: {detail:?}",
                        fixture.name
                    );
                }
                assert!(detail.len() <= 512);
                assert!(!detail.chars().any(|c| c.is_control() && c != '\n'));
                assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
                assert_eq!(error.presentation.provider_http_status, Some(400));
                assert_eq!(
                    error.presentation.provider_request_id.as_deref(),
                    Some("req-multi-header-fixture")
                );
            }
        }
    }
    Ok(())
}
