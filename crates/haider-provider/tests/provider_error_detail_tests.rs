#![allow(clippy::expect_used)]
//! Provider error detail policy (ruling 2): known templates publish on every
//! surface; unknown provider prose is withheld on shareable surfaces and kept
//! only in the owner-local `provider_raw_detail`.

use haider_provider::acp::client::{ACP_STDERR_TAIL_BYTES, AcpError, StderrRing};
use haider_provider::acp::wire::JsonRpcError;
use haider_provider::{
    ProviderError, ProviderErrorKind, ProviderSlotEvidence, deadline_exhausted_error,
    replay_anthropic_http_error, replay_gemini_http_error, replay_gemini_sse,
    replay_openai_http_error, replay_openai_responses_sse,
};
use std::time::Duration;

type Classifier = fn(u16, Option<&str>, &[u8]) -> ProviderError;
const WITHHELD: &str = "details withheld";

fn classifiers() -> [Classifier; 3] {
    [
        replay_anthropic_http_error,
        replay_gemini_http_error,
        replay_openai_http_error,
    ]
}

fn anthropic_openai() -> [(&'static str, Classifier); 2] {
    [
        ("anthropic", replay_anthropic_http_error),
        ("openai", replay_openai_http_error),
    ]
}

fn body(error_type: &str, message: &str) -> String {
    serde_json::json!({"error": {"type": error_type, "message": message}}).to_string()
}

fn gemini_body(message: &str) -> String {
    serde_json::json!({"error": {"code": 400, "message": message, "status": "INVALID_ARGUMENT"}})
        .to_string()
}

/// Everything a shareable surface can see of a provider error: the durable
/// presentation minus the local-only field, the public message, and the
/// serialized error (which never carries the local-only raw text).
fn shareable_text(error: &ProviderError) -> String {
    let mut presentation = error.presentation.clone();
    presentation.strip_local_only();
    format!(
        "{} || {} || {} || {}",
        presentation.detail,
        error.public_message(),
        serde_json::to_string(&presentation).expect("presentation json"),
        serde_json::to_string(&error.shareable()).expect("error json"),
    )
}

// ---------------------------------------------------------------------------
// Templates: must-show list (positive) and near misses (negative)
// ---------------------------------------------------------------------------

/// Every must-show message from ruling 2 and the earlier lane rulings/tests.
const MUST_SHOW: &[&str] = &[
    "Overloaded",
    "Your organization does not have access to this model.",
    "prompt is too long: 120001 tokens > 100000 maximum",
    "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.",
    "Unsupported parameter: 'temperature' is not supported with this model.",
    "Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.",
    "Unknown parameter: 'service_tier'.",
    "Your authentication token has been invalidated. Please try signing in again.",
    "This organization has been disabled.",
    "Your account is not active, please check your billing details on our website.",
    "Your organization must be verified to stream this model.",
    "The caller does not have permission",
    "Please reduce the length of the messages or completion.",
    "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.",
    "API key not valid. Please pass a valid API key.",
    "User location is not supported for the API use.",
    "Request contains an invalid argument.",
    "Resource has been exhausted (e.g. check quota).",
    "prompt is too long: 40,000 tokens > 1,000,000 maximum",
    "Invalid JSON payload received. Unknown name \"thinking_budget\" at 'generation_config': Cannot find field.",
];

#[test]
fn must_show_messages_render_verbatim_on_every_http_family() {
    let mut failures = Vec::new();
    for prose in MUST_SHOW {
        let json = body("invalid_request_error", prose);
        for (family, classify) in anthropic_openai() {
            let error = classify(400, None, json.as_bytes());
            if error.presentation.detail != *prose || error.provider_raw_detail.is_some() {
                failures.push(format!(
                    "{family}: {prose:?} -> {:?}",
                    error.presentation.detail
                ));
            }
        }
        let gemini = replay_gemini_http_error(400, None, gemini_body(prose).as_bytes());
        if gemini.presentation.detail != *prose {
            failures.push(format!(
                "gemini: {prose:?} -> {:?}",
                gemini.presentation.detail
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "must-show failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn template_slots_render_only_typed_safe_values() {
    for (prose, expected) in [
        (
            "Rate limit reached for gpt-4o in organization org-fixture123 on tokens per min (TPM): Limit 30000, Used 29000, Requested 2000. Please try again in 2s.",
            "Rate limit reached for gpt-4o in organization [REDACTED] on tokens per min (TPM): Limit 30000, Used 29000, Requested 2000. Please try again in 2s.",
        ),
        (
            "You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com/docs/guides/error-codes/api-errors.",
            "You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com.",
        ),
        (
            "Incorrect API key provided: sk-proj-****wxyz. You can find your API key at https://platform.openai.com/account/api-keys.",
            "Incorrect API key provided: [REDACTED]. You can find your API key at https://platform.openai.com.",
        ),
        (
            "Project `proj_fixture42` does not have access to model `gpt-4o`",
            "Project `[REDACTED]` does not have access to model `gpt-4o`",
        ),
    ] {
        let json = body("rate_limit_error", prose);
        let mut error = replay_openai_http_error(429, None, json.as_bytes());
        error.corroborate_slots(&requested("gpt-4o"));
        assert_eq!(error.presentation.detail, expected);
    }
    let gemini = "models/gemini-9.9-fixture is not found for API version v1beta, or is not supported for generateContent. Call ListModels to see the list of available models and their supported methods.";
    let mut error = replay_gemini_http_error(400, None, gemini_body(gemini).as_bytes());
    error.corroborate_slots(&requested("gemini-9.9-fixture"));
    assert_eq!(error.presentation.detail, gemini);
}

fn requested(model: &str) -> ProviderSlotEvidence {
    ProviderSlotEvidence {
        requested_model: Some(model.to_owned()),
        tool_call_ids: Vec::new(),
    }
}

/// Ruling D: `{model}`, `{tool_call_id}` and `{request_id}` publish only
/// values Haider can corroborate; otherwise the message stays withheld.
#[test]
fn identity_slots_publish_only_corroborated_values() {
    let model_missing = "The model `gpt-9-fixture` does not exist or you do not have access to it.";
    let json = body("invalid_request_error", model_missing);
    let mut error = replay_openai_http_error(404, None, json.as_bytes());
    assert!(error.presentation.detail.ends_with(WITHHELD), "no evidence");
    assert_eq!(error.provider_raw_detail.as_deref(), Some(model_missing));
    error.corroborate_slots(&requested("some-other-model"));
    assert!(error.presentation.detail.ends_with(WITHHELD), "wrong model");
    error.corroborate_slots(&requested("gpt-9-fixture"));
    assert_eq!(error.presentation.detail, model_missing);
    assert!(error.provider_raw_detail.is_none());

    let orphan = "messages.2: `tool_use` ids were found without `tool_result` blocks immediately after: toolu_fixture01. Each `tool_use` block must have a corresponding `tool_result` block in the next message.";
    let json = body("invalid_request_error", orphan);
    let mut error = replay_anthropic_http_error(400, None, json.as_bytes());
    error.corroborate_slots(&ProviderSlotEvidence {
        requested_model: Some("claude-fixture".into()),
        tool_call_ids: vec!["toolu_other".into()],
    });
    assert!(
        error.presentation.detail.ends_with(WITHHELD),
        "unknown tool id"
    );
    error.corroborate_slots(&ProviderSlotEvidence {
        requested_model: Some("claude-fixture".into()),
        tool_call_ids: vec!["toolu_fixture01".into()],
    });
    assert_eq!(error.presentation.detail, orphan);

    let overloaded = "That model is currently overloaded with other requests. You can retry your request, or contact us through our help center at help.openai.com if the error persists. (Please include the request ID req_captured01 in your message.)";
    let json = body("server_error", overloaded);
    let same = replay_openai_http_error(503, None, json.as_bytes())
        .with_http_metadata(503, Some("req_captured01"));
    assert_eq!(same.presentation.detail, overloaded);
    let other = replay_openai_http_error(503, None, json.as_bytes())
        .with_http_metadata(503, Some("req_different02"));
    assert!(
        other.presentation.detail.ends_with(WITHHELD),
        "request id mismatch"
    );
    // F1 (verify6): a captured header that the structured request-id field
    // rejects (account marker) must not be published through the template.
    let account = overloaded.replace("req_captured01", "req_account_quillv6x");
    let captured_account =
        replay_openai_http_error(503, None, body("server_error", &account).as_bytes())
            .with_http_metadata(503, Some("req_account_quillv6x"));
    assert!(captured_account.presentation.provider_request_id.is_none());
    assert!(
        captured_account.presentation.detail.ends_with(WITHHELD),
        "{}",
        captured_account.presentation.detail
    );
    assert!(!shareable_text(&captured_account).contains("quillv6x"));
    let long = overloaded.replace("req_captured01", &format!("req_{}", "a".repeat(70)));
    let uncaptured = replay_openai_http_error(503, None, body("server_error", &long).as_bytes());
    assert!(
        uncaptured.presentation.detail.ends_with(WITHHELD),
        "uncaptured > 64 bytes"
    );
}

#[test]
fn near_miss_messages_do_not_match_a_template() {
    for prose in [
        // Extra unrecognised text before/after a known message.
        "Overloaded quillmere",
        "quillmere: Overloaded",
        "Your organization quillmere does not have access to this model.",
        "Your organization does not have access to this model. Contact quillmere.",
        "User location is not supported for the API use. Org: quillmere",
        // Slot values of the wrong type.
        "The model `quillmere` does not exist or you do not have access to it.",
        "The model `gpt-4o quillmere` does not exist or you do not have access to it.",
        "Unsupported parameter: 'quillmere' is not supported with this model.",
        "Unknown parameter: 'tools.quillmere'.",
        "You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://quillmere.openai.com/docs.",
        "prompt is too long: many tokens > 100000 maximum",
        "Rate limit reached for gpt-4o in organization quillmere on tokens per min (TPM): Limit 1, Used 1, Requested 1. Please try again in 2s.",
        // Case and wording changes.
        "overloaded",
        "OVERLOADED",
        "Your organisation does not have access to this model.",
    ] {
        let json = body("permission_error", prose);
        for (family, classify) in anthropic_openai() {
            let error = classify(403, None, json.as_bytes());
            assert!(
                error.presentation.detail.ends_with(WITHHELD),
                "{family}: {prose:?} -> {:?}",
                error.presentation.detail
            );
            // The raw text is kept for owner-local surfaces (the shared
            // output redactor may still mask opaque-looking spans in it).
            assert!(error.provider_raw_detail.is_some(), "{family}: raw local");
            if prose.contains("quillmere") {
                assert!(
                    !shareable_text(&error).contains("quillmere"),
                    "{family}: {prose:?}"
                );
            }
        }
    }
}

#[test]
fn gemini_stream_error_frames_use_the_template_boundary() {
    let frame = |message: &str| {
        format!(
            "data: {}\n\n",
            serde_json::json!({"error": {"code": 400, "message": message, "status": "INVALID_ARGUMENT"}})
        )
    };
    let known = "User location is not supported for the API use.";
    let error = replay_gemini_sse(frame(known).as_bytes())
        .into_iter()
        .find_map(Result::err)
        .expect("error frame");
    assert_eq!(error.presentation.detail, known);
    assert_eq!(error.presentation.provider_http_status, None);
    let unknown = replay_gemini_sse(frame("Denied for org quillgemini.").as_bytes())
        .into_iter()
        .find_map(Result::err)
        .expect("error frame");
    assert!(unknown.presentation.detail.ends_with(WITHHELD));
    assert!(!shareable_text(&unknown).contains("quillgemini"));
    assert!(
        unknown
            .provider_raw_detail
            .is_some_and(|raw| raw.contains("quillgemini"))
    );
}

/// Final-review should-fix templates: real current wording renders (with
/// corroborated slots) instead of "details withheld". Sources are cited on
/// each template entry.
#[test]
fn current_provider_wordings_render_with_corroborated_slots() {
    // Anthropic current 429 (continuedev/continue#10425), both apostrophes.
    for apostrophe in ["'", "\u{2019}"] {
        let prose = format!(
            "This request would exceed your organization{apostrophe}s rate limit of 50,000 input tokens per minute (org: f427910e-3b38-4559-b0a8-1ab041e6cbca, model: claude-haiku-4-5-20251001). For details, refer to: https://docs.claude.com/en/api/rate-limits. You can see the response headers for current usage. Please reduce the prompt length or the maximum tokens requested, or try again later. You may also contact sales at https://www.anthropic.com/contact-sales to discuss your options for a rate limit increase."
        );
        let mut error =
            replay_anthropic_http_error(429, None, body("rate_limit_error", &prose).as_bytes());
        error.corroborate_slots(&requested("claude-haiku-4-5-20251001"));
        let detail = &error.presentation.detail;
        assert!(
            detail.starts_with("This request would exceed your organization"),
            "{detail}"
        );
        assert!(
            detail.contains("(org: [REDACTED], model: claude-haiku-4-5-20251001)"),
            "{detail}"
        );
        assert!(
            detail.contains("refer to: https://docs.claude.com."),
            "{detail}"
        );
        assert!(!detail.contains("f427910e"), "{detail}");
        let mut other =
            replay_anthropic_http_error(429, None, body("rate_limit_error", &prose).as_bytes());
        other.corroborate_slots(&requested("claude-sonnet-4-5"));
        assert!(
            other.presentation.detail.ends_with(WITHHELD),
            "uncorroborated model"
        );
    }
    // Gemini free-tier quota 429 (google-gemini/gemini-cli#13112).
    let quota = "You exceeded your current quota, please check your plan and billing details. For more information on this error, head to: https://ai.google.dev/gemini-api/docs/rate-limits. To monitor your current usage, head to: https://ai.dev/usage?tab=rate-limit. \n* Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_input_token_count, limit: 250000, model: gemini-2.5-flash\nPlease retry in 39.844676573s.";
    let gemini_429 = |message: &str| {
        serde_json::json!({"error": {"code": 429, "message": message, "status": "RESOURCE_EXHAUSTED"}})
            .to_string()
    };
    let mut error = replay_gemini_http_error(429, None, gemini_429(quota).as_bytes());
    error.corroborate_slots(&requested("gemini-2.5-flash"));
    assert_eq!(
        error.presentation.detail,
        "You exceeded your current quota, please check your plan and billing details. For more information on this error, head to: https://ai.google.dev. To monitor your current usage, head to: https://ai.dev. \n* Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_input_token_count, limit: 250000, model: gemini-2.5-flash\nPlease retry in 39.844676573s."
    );
    let unknown_metric = quota.replace(
        "generate_content_free_tier_input_token_count",
        "quillmetric",
    );
    let mut error = replay_gemini_http_error(429, None, gemini_429(&unknown_metric).as_bytes());
    error.corroborate_slots(&requested("gemini-2.5-flash"));
    assert!(
        error.presentation.detail.ends_with(WITHHELD),
        "unlisted metric"
    );
    // Gemini 503 high demand (discuss.ai.google.dev).
    let demand = "This model is currently experiencing high demand. Spikes in demand are usually temporary. Please try again later.";
    let error = replay_gemini_http_error(
        503,
        None,
        serde_json::json!({"error": {"code": 503, "message": demand, "status": "UNAVAILABLE"}})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(error.presentation.detail, demand);
    // Anthropic image too large (anthropics/claude-code 5 MB issues).
    let image = "messages.2.content.0.image.source.base64: image exceeds 5 MB maximum: 8259148 bytes > 5242880 bytes";
    let error =
        replay_anthropic_http_error(400, None, body("invalid_request_error", image).as_bytes());
    assert_eq!(error.presentation.detail, image);
    // Anthropic thinking budget, docs.claude.com wording.
    let thinking = "`max_tokens` must be greater than `thinking.budget_tokens`. Please consult our documentation at https://docs.claude.com/en/docs/build-with-claude/extended-thinking#max-tokens-and-context-window-size";
    let error = replay_anthropic_http_error(
        400,
        None,
        body("invalid_request_error", thinking).as_bytes(),
    );
    assert_eq!(error.presentation.detail, thinking);
    // Error-type allowlist additions.
    for (family, error_type) in [
        ("anthropic", "not_found_error"),
        ("anthropic", "request_too_large"),
        ("openai", "model_not_found"),
    ] {
        let json = serde_json::json!({"error": {"type": error_type, "message": "x"}}).to_string();
        let error = if family == "anthropic" {
            replay_anthropic_http_error(404, None, json.as_bytes())
        } else {
            replay_openai_http_error(404, None, json.as_bytes())
        };
        assert_eq!(
            error.presentation.provider_error_type.as_deref(),
            Some(error_type)
        );
    }
}

// ---------------------------------------------------------------------------
// Unknown prose: withheld on shareable surfaces, raw only owner-local
// ---------------------------------------------------------------------------

#[test]
fn unknown_prose_keeps_default_explanation_and_raw_only_locally() {
    let prose = "The workspace quillmere has been archived.";
    for classify in classifiers() {
        let error = classify(400, None, gemini_body(prose).as_bytes());
        assert!(error.presentation.detail.ends_with(WITHHELD));
        assert!(error.presentation.detail.len() > WITHHELD.len() + 3);
        assert!(error.presentation.provider_raw_detail.is_none());
        assert_eq!(error.provider_raw_detail.as_deref(), Some(prose));
        assert!(!shareable_text(&error).contains("quillmere"));
        // The raw text is not a serialized ProviderError field.
        assert!(
            !serde_json::to_string(&error)
                .expect("json")
                .contains("quillmere")
        );
    }
}

#[test]
fn raw_local_text_still_passes_the_credential_redactor() {
    for secret in [
        "Authorization: Bearer fixture_973_bearer_secret",
        "x-api-key: sk-fixture-973-key-value",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.fixture973signature",
    ] {
        let prose = format!("Access denied: {secret}");
        let error =
            replay_openai_http_error(403, None, body("permission_error", &prose).as_bytes());
        let raw = error.provider_raw_detail.expect("raw local");
        for fragment in [
            "fixture_973_bearer_secret",
            "sk-fixture-973-key-value",
            "fixture973signature",
        ] {
            assert!(!raw.contains(fragment), "{raw}");
        }
        assert!(error.presentation.detail.ends_with(WITHHELD));
    }
}

#[test]
fn credential_boundary_matrix_never_reaches_a_shareable_surface() {
    for classify in classifiers() {
        for introducer in [
            "authorization",
            "x-api-key",
            "api_key",
            "access_token",
            "Bearer",
            "echoed=",
        ] {
            for separator in [" ", "\n\t", ":", "=", " : = \n"] {
                let message = format!(
                    "Invalid request; {introducer}{separator}\"opaque-openhead, \\\"escaped\n api_key=inner, opentail"
                );
                let json = serde_json::json!({"error": {"message": message}}).to_string();
                for input in [json.as_bytes(), message.as_bytes()] {
                    let error = classify(400, None, input);
                    assert!(error.presentation.detail.ends_with(WITHHELD));
                    assert!(!shareable_text(&error).contains("opaque-openhead"));
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
            let json = serde_json::json!({"error": {"message": fixture.detail}}).to_string();
            for input in [json.as_bytes(), fixture.detail.as_bytes()] {
                let error = classify(400, None, input);
                let shareable = shareable_text(&error);
                for secret in &fixture.secrets {
                    assert!(
                        !shareable.contains(secret),
                        "{} leaked {secret}",
                        fixture.name
                    );
                }
                assert!(!error.presentation.detail.is_empty(), "{}", fixture.name);
                assert!(error.presentation.detail.len() <= 512);
            }
        }
    }
    Ok(())
}

#[test]
fn oversized_markup_and_invalid_utf8_bodies_never_publish_fragments() {
    for classify in [replay_anthropic_http_error, replay_openai_http_error] {
        let html = format!("<html><body>{}</body></html>", "x".repeat(16_000));
        let error = classify(403, None, html.as_bytes());
        assert!(error.presentation.detail.ends_with(WITHHELD));
        let invalid = classify(403, None, b"\xff\xfeprivate");
        assert!(!shareable_text(&invalid).contains("private"));
        assert!(invalid.presentation.detail.ends_with(WITHHELD));
        let long =
            serde_json::json!({"error": {"message": format!("{}quillmere", "ok ".repeat(900))}})
                .to_string();
        let error = classify(400, None, long.as_bytes());
        assert!(error.presentation.detail.ends_with(WITHHELD));
        assert!(!shareable_text(&error).contains("quillmere"));
    }
}

// ---------------------------------------------------------------------------
// Structured identity (unchanged by ruling 2)
// ---------------------------------------------------------------------------

#[test]
fn plain_denial_keeps_structured_fields_and_replays() {
    for classify in [replay_anthropic_http_error, replay_openai_http_error] {
        let json = body(
            "permission_error",
            "Your organization does not have access to this model.",
        );
        let error =
            classify(403, None, json.as_bytes()).with_http_metadata(403, Some("req_fixture-403"));
        assert_eq!(error.kind, ProviderErrorKind::PermissionDenied);
        assert_eq!(
            error.presentation.detail,
            "Your organization does not have access to this model."
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
        let replayed: ProviderError =
            serde_json::from_slice(&journal).expect("journal shape replays");
        assert_eq!(replayed, error);
    }
}

#[test]
fn provider_route_status_matrix_retains_safe_identity() {
    let families: [(&str, Classifier); 2] = [
        ("anthropic", replay_anthropic_http_error),
        ("openai", replay_openai_http_error),
    ];
    for (family, classify) in families {
        for (status, error_type, kind, message) in [
            (
                403,
                "permission_error",
                ProviderErrorKind::PermissionDenied,
                "This organization has been disabled.",
            ),
            (
                401,
                "authentication_error",
                ProviderErrorKind::Authentication,
                "invalid x-api-key",
            ),
            (
                429,
                "rate_limit_error",
                ProviderErrorKind::RateLimited,
                "Resource has been exhausted (e.g. check quota).",
            ),
            (
                500,
                "api_error",
                ProviderErrorKind::Transport,
                "Internal server error",
            ),
        ] {
            let json =
                serde_json::json!({"type":"error","error":{"type":error_type,"message":message}})
                    .to_string();
            let id = format!("req_{family}_{status}");
            let error =
                classify(status, None, json.as_bytes()).with_http_metadata(status, Some(&id));
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
        let json = body("acct_973_private_account", "Access denied.");
        let error = classify(403, None, json.as_bytes())
            .with_http_metadata(403, Some("req_fixture Cookie: session=private"));
        assert!(error.presentation.provider_error_type.is_none());
        assert!(error.presentation.provider_request_id.is_none());
        assert_eq!(error.presentation.provider_http_status, Some(403));
    }
    let json = br#"{"error":{"type":"acct_private","code":"permission_denied","message":"Access denied."}}"#;
    let error = replay_openai_http_error(403, None, json);
    assert_eq!(
        error.presentation.provider_error_type.as_deref(),
        Some("permission_denied")
    );
}

#[test]
fn request_ids_require_the_exact_bounded_header_shape() {
    let json = br#"{"error":{"type":"permission_error","message":"Access denied."}}"#;
    for request_id in [
        "req_fixture@example.test",
        "req_fixture?account=private",
        "req_fixture Cookie: private",
        "req_acct_973_private_account",
        "acct_973_private_account",
        "req_",
    ] {
        let error =
            replay_openai_http_error(403, None, json).with_http_metadata(403, Some(request_id));
        assert!(
            error.presentation.provider_request_id.is_none(),
            "{request_id}"
        );
    }
    let long = format!("req_{}", "x".repeat(129));
    assert!(
        replay_openai_http_error(403, None, json)
            .with_http_metadata(403, Some(&long))
            .presentation
            .provider_request_id
            .is_none()
    );
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

// ---------------------------------------------------------------------------
// ACP and the message boundary
// ---------------------------------------------------------------------------

#[test]
fn acp_rpc_messages_render_from_templates_or_stay_local() {
    let known = AcpError::Rpc(JsonRpcError {
        code: -32000,
        message: "Authentication required".into(),
        data: None,
    })
    .into_provider_error("");
    assert!(
        known.message.ends_with("Authentication required"),
        "{}",
        known.message
    );
    assert!(known.provider_raw_detail.is_none());

    let private = "robin.verify2@synthetic.example";
    let unknown = AcpError::Rpc(JsonRpcError {
        code: -32000,
        message: format!("Permission denied for {private}"),
        data: None,
    })
    .into_provider_error(&format!("agent stderr: account {private}"));
    assert!(!unknown.message.contains(private));
    assert!(!shareable_text(&unknown).contains(private));
    assert!(unknown.presentation.detail.ends_with(WITHHELD));
    let raw = unknown.provider_raw_detail.clone().expect("raw local");
    assert!(raw.contains("Agent stderr tail"), "{raw}");

    // A known RPC message with a stderr tail: the detail renders from the
    // template while the tail stays local.
    let tailed = AcpError::Rpc(JsonRpcError {
        code: -32000,
        message: "Internal error".into(),
        data: None,
    })
    .into_provider_error("org quillmere throttled");
    assert_eq!(tailed.presentation.detail, "Internal error");
    assert!(!shareable_text(&tailed).contains("quillmere"));
    assert!(
        tailed
            .provider_raw_detail
            .expect("tail local")
            .contains("quillmere")
    );
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
    assert!(!shareable_text(&error).contains("robin.verify2"));
}

#[test]
fn untrusted_messages_publish_only_through_templates() {
    let error = ProviderError::new(
        ProviderErrorKind::MalformedFrame,
        "Anthropic SSE `quillmere` data is not valid JSON",
    )
    .with_untrusted_message();
    assert!(
        !error.public_message().contains("quillmere"),
        "{}",
        error.public_message()
    );
    assert!(
        error
            .local_message_detail()
            .expect("local")
            .contains("quillmere")
    );
    assert!(!error.shareable().message.contains("quillmere"));
    let known =
        ProviderError::new(ProviderErrorKind::Overloaded, "Overloaded").with_untrusted_message();
    assert_eq!(known.public_message(), "Overloaded: Overloaded");
    assert!(known.local_message_detail().is_none());
    // Haider-authored messages are published as written.
    let trusted = ProviderError::new(
        ProviderErrorKind::Transport,
        "OpenAI HTTP 503 returned an overloaded error",
    );
    assert_eq!(trusted.public_message(), trusted.to_string());
}

#[test]
fn local_timeout_telemetry_survives_the_public_message_policy() {
    let error =
        deadline_exhausted_error(Duration::from_millis(60_000), Duration::from_millis(1_234));
    let message = error.public_message();
    assert_eq!(message, error.to_string());
    assert!(
        message.contains("reason=deadline_exhausted opened_within_ms=1234 budget_ms=60000"),
        "{message}"
    );
}

// ---------------------------------------------------------------------------
// Permanent regression: 9-verify3-opus adversarial inputs (adv-verify3).
// Every input must leave no marker on any shareable surface.
// ---------------------------------------------------------------------------

fn verify3_http_surfaces(message: &str) -> Vec<(String, ProviderError)> {
    let json = body("permission_error", message);
    let mut out = vec![
        (
            "anthropic403".to_owned(),
            replay_anthropic_http_error(403, None, json.as_bytes()),
        ),
        (
            "openai403".to_owned(),
            replay_openai_http_error(403, None, json.as_bytes()),
        ),
        (
            "gemini400".to_owned(),
            replay_gemini_http_error(400, None, gemini_body(message).as_bytes()),
        ),
    ];
    let rpc = AcpError::Rpc(JsonRpcError {
        code: -32000,
        message: message.to_owned(),
        data: None,
    });
    out.push(("acp_rpc_no_tail".to_owned(), rpc.into_provider_error("")));
    let rpc = AcpError::Rpc(JsonRpcError {
        code: -32000,
        message: "agent failed".into(),
        data: None,
    });
    out.push((
        "acp_stderr_tail".to_owned(),
        rpc.into_provider_error(message),
    ));
    out
}

type Verify3Case = (&'static str, &'static str, &'static [&'static str]);

const VERIFY3_CASES: &[Verify3Case] = &[
    (
        "digits",
        "Your organization quill9mere has no access to this model.",
        &["quill9mere"],
    ),
    (
        "hyphen_digits",
        "Your organization quill-mere-7 has no access to this model.",
        &["quill-mere-7", "mere-7"],
    ),
    (
        "underscore",
        "Your workspace quill_mere lacks access to this model.",
        &["quill_mere"],
    ),
    (
        "unicode_name",
        "Your organization quillmère has no access.",
        &["quillm"],
    ),
    (
        "unicode_label",
        "Organización quillmere sin acceso.",
        &["quillmere"],
    ),
    (
        "multiword_to_comma",
        "Your organization Quill Mere Holdings Ltd, has no access.",
        &["Quill", "Mere", "Holdings"],
    ),
    (
        "multiword_to_semicolon",
        "Denied for team quill mere holdings; contact support.",
        &["quill", "holdings"],
    ),
    (
        "name_with_of",
        "Your organization Bank of Quillmere does not have access to this model.",
        &["Quillmere"],
    ),
    (
        "name_with_for",
        "Your organization Friends for Quillmere does not have access.",
        &["Quillmere"],
    ),
    (
        "name_with_and",
        "Your team Quillmere and Wexford has exceeded its quota.",
        &["Quillmere", "Wexford"],
    ),
    (
        "name_with_the_lower",
        "Denied for organization the quillmere group.",
        &["quillmere"],
    ),
    (
        "copula_is",
        "Your organization is quillmere.",
        &["quillmere"],
    ),
    (
        "copula_was",
        "The workspace was quillmere-labs and is now disabled.",
        &["quillmere"],
    ),
    (
        "copula_are",
        "Your projects are quillmere and wexford.",
        &["quillmere", "wexford"],
    ),
    (
        "copula_were",
        "The accounts were quillmere.",
        &["quillmere"],
    ),
    (
        "copula_state_then_name",
        "Your organization is restricted: quillmere.",
        &["quillmere"],
    ),
    (
        "copula_not",
        "Your organization is not quillmere.",
        &["quillmere"],
    ),
    (
        "state_like_name",
        "Your organization is verified-quillmere.",
        &["quillmere"],
    ),
    (
        "stopword_cap",
        "Access denied for team Access Quillmere.",
        &["Quillmere"],
    ),
    (
        "stopword_lower_name",
        "Access denied for team access quillmere.",
        &["quillmere"],
    ),
    (
        "stopword_cap_does",
        "Your organization Does Quillmere has no access.",
        &["Quillmere"],
    ),
    (
        "org_colon_nospace",
        "Denied: org:quillmere lacks access.",
        &["quillmere"],
    ),
    (
        "org_colon_space",
        "Denied: Org: Quillmere lacks access.",
        &["Quillmere"],
    ),
    (
        "tenant_equals",
        "Denied for tenant=quillmere; retry.",
        &["quillmere"],
    ),
    (
        "tenant_equals_spaced",
        "Denied for tenant = quillmere; retry.",
        &["quillmere"],
    ),
    ("team_plain", "Denied for team quillmere.", &["quillmere"]),
    (
        "team_colon_cap",
        "Team: Quillmere has no access.",
        &["Quillmere"],
    ),
    ("team_hyphen", "Denied for team-quillmere.", &["quillmere"]),
    ("slash", "Denied for workspace/quillmere.", &["quillmere"]),
    ("hash", "Denied for org#quillmere.", &["quillmere"]),
    (
        "paren_nospace",
        "Denied for project(quillmere).",
        &["quillmere"],
    ),
    (
        "paren_space",
        "Denied for project (quillmere).",
        &["quillmere"],
    ),
    (
        "emdash_nospace",
        "Denied for organization\u{2014}quillmere.",
        &["quillmere"],
    ),
    (
        "emdash_space_after",
        "Denied for organization\u{2014} quillmere has no access.",
        &["quillmere"],
    ),
    (
        "endash_nospace",
        "Denied for organization\u{2013}quillmere.",
        &["quillmere"],
    ),
    (
        "comma_nospace",
        "Denied for organization,quillmere.",
        &["quillmere"],
    ),
    (
        "appositive",
        "Your organization, quillmere, has no access.",
        &["quillmere"],
    ),
    (
        "nbsp",
        "Your organization\u{a0}quillmere has no access.",
        &["quillmere"],
    ),
    (
        "tab",
        "Your organization\tquillmere has no access.",
        &["quillmere"],
    ),
    (
        "possessive_label_name",
        "Your organization's name is quillmere.",
        &["quillmere"],
    ),
    (
        "possessive_label_slug",
        "The workspace's slug is quillmere.",
        &["quillmere"],
    ),
    (
        "name_before_label",
        "Quillmere's organization does not have access to this model.",
        &["Quillmere"],
    ),
    (
        "name_before_label_lower",
        "The quillmere organization does not have access.",
        &["quillmere"],
    ),
    (
        "plural_label",
        "Denied for organizations quillmere and wexford.",
        &["quillmere", "wexford"],
    ),
    (
        "british_label",
        "Your organisation quillmere has no access.",
        &["quillmere"],
    ),
    (
        "german_label",
        "Organisation quillmere hat keinen Zugriff.",
        &["quillmere"],
    ),
    ("konto", "Konto quillmere gesperrt.", &["quillmere"]),
    (
        "compte",
        "Le compte quillmere est suspendu.",
        &["quillmere"],
    ),
    (
        "company_label",
        "Denied for company quillmere.",
        &["quillmere"],
    ),
    (
        "repeated_label",
        "Denied for organization organization quillmere.",
        &["quillmere"],
    ),
    (
        "repeated_label_colon",
        "Denied for org org: quillmere.",
        &["quillmere"],
    ),
    (
        "two_labels",
        "Denied for organization quillmere, organization wexford.",
        &["quillmere", "wexford"],
    ),
    (
        "user_label",
        "The user quillmere is not allowed to use this model.",
        &["quillmere"],
    ),
    (
        "customer_label",
        "Customer Quillmere has an unpaid invoice.",
        &["Quillmere"],
    ),
    (
        "quoted_escape",
        "Your organization \"quill\\\"mere\" has no access.",
        &["mere"],
    ),
    (
        "guillemets",
        "Your organization \u{ab}quillmere\u{bb} has no access.",
        &["quillmere"],
    ),
    (
        "backticks",
        "Your organization `quillmere` has no access.",
        &["quillmere"],
    ),
    (
        "bare_tenant_host",
        "See quillmere.synthetic.example/help for details.",
        &["quillmere"],
    ),
    (
        "named_label",
        "Organization named quillmere has no access.",
        &["quillmere"],
    ),
    (
        "label_newline",
        "Your organization\nquillmere has no access.",
        &["quillmere"],
    ),
    (
        "email_label",
        "Contact admin quill.mere@synthetic.example for access.",
        &["quill.mere"],
    ),
    (
        "id_in_prose",
        "Denied for acct_quillmere7.",
        &["quillmere7"],
    ),
    (
        "suffix_trick",
        "See https://api.openai.com.quillmere.test/x for help.",
        &["quillmere"],
    ),
    (
        "prefix_trick",
        "See https://quillmere-api.openai.com/ for help.",
        &["quillmere"],
    ),
    (
        "subdomain_of_allowed",
        "See https://quillmere.api.openai.com/ for help.",
        &["quillmere"],
    ),
    (
        "userinfo_allowed_host",
        "See https://quillmere@api.openai.com/v1 for help.",
        &["quillmere"],
    ),
    (
        "userinfo_pass_allowed_host",
        "See https://quillmere:fixturepass@api.openai.com/v1 for help.",
        &["quillmere", "fixturepass"],
    ),
    (
        "at_confusion",
        "See https://api.openai.com@quillmere.test/ for help.",
        &["quillmere"],
    ),
    (
        "punycode",
        "See https://xn--pi-7ld.openai.com/ for help.",
        &["xn--"],
    ),
    (
        "cyrillic_lookalike",
        "See https://\u{430}pi.openai.com/quillmere for help.",
        &["quillmere"],
    ),
    (
        "uppercase_query",
        "See HTTPS://API.OPENAI.COM/x?org=quillmere for help.",
        &["quillmere"],
    ),
    (
        "port_path",
        "See https://api.openai.com:8443/quillmere for help.",
        &["quillmere"],
    ),
    (
        "trailing_dot_host",
        "See https://api.openai.com./quillmere for help.",
        &["quillmere"],
    ),
    (
        "percent_host",
        "See https://api.openai.com%2equillmere.test/ for help.",
        &["quillmere"],
    ),
    (
        "backslash",
        "See https://api.openai.com\\@quillmere.test/ for help.",
        &["quillmere"],
    ),
    (
        "ftp_scheme",
        "See ftp://api.openai.com/quillmere for help.",
        &["quillmere"],
    ),
    (
        "azure_tenant",
        "See https://quillmere.openai.azure.com/openai/deployments for help.",
        &["quillmere"],
    ),
    (
        "allowed_path_org",
        "See https://platform.openai.com/account/org-quillmere/settings.",
        &["quillmere"],
    ),
    (
        "allowed_fragment",
        "See https://platform.openai.com#quillmere for help.",
        &["quillmere"],
    ),
    (
        "paren_url",
        "See (https://quillmere.test/help) for help.",
        &["quillmere"],
    ),
    (
        "ipv6",
        "See https://[::1]/quillmere for help.",
        &["quillmere"],
    ),
    (
        "scheme_relative",
        "See //quillmere.test/help for help.",
        &["quillmere"],
    ),
    (
        "javascript_scheme",
        "See javascript:alert('quillmere') for help.",
        &["quillmere"],
    ),
];

fn verify3_cut_cases() -> Vec<(&'static str, String)> {
    let pad = "ok ".repeat(165);
    vec![
        (
            "cut_label_before_512",
            format!("{pad}organization quillmere"),
        ),
        (
            "cut_name_straddles_512",
            format!("{pad}orgs org quillmerequillmere"),
        ),
        (
            "cut_over_512_after_scrub",
            format!("{}organization quillmere", "ok ".repeat(200)),
        ),
    ]
}

/// The ACP stderr ring front-drains to 8 KiB so the retained tail can start
/// with the bare name (the label was cut away).
fn verify3_front_cut_stderr_tail() -> String {
    let ring = StderrRing::new(ACP_STDERR_TAIL_BYTES);
    let first = "denied: organization quillmere has no access";
    let cut_at = first.find("quillmere").expect("marker");
    let filler_len = ACP_STDERR_TAIL_BYTES + cut_at - first.len() - 2;
    ring.push(format!("{first}\n").as_bytes());
    ring.push(format!("{}\n", "A".repeat(filler_len)).as_bytes());
    ring.tail()
}

#[test]
fn verify3_adversarial_inputs_leave_no_marker_on_shareable_surfaces() {
    let mut cases: Vec<(String, String, Vec<&str>)> = VERIFY3_CASES
        .iter()
        .map(|(name, message, markers)| {
            ((*name).to_owned(), (*message).to_owned(), markers.to_vec())
        })
        .collect();
    cases.extend(
        verify3_cut_cases()
            .into_iter()
            .map(|(name, message)| (name.to_owned(), message, vec!["quillmere"])),
    );
    let mut leaks = Vec::new();
    for (name, message, markers) in &cases {
        for (surface, error) in verify3_http_surfaces(message) {
            let shareable = shareable_text(&error);
            for marker in markers {
                if shareable.contains(marker) {
                    leaks.push(format!("{name}/{surface} [{marker}]: {shareable}"));
                }
            }
            // HTTP adapter messages stay Haider-authored templates.
            if !surface.starts_with("acp")
                && markers.iter().any(|marker| error.message.contains(marker))
            {
                leaks.push(format!("{name}/{surface} message: {}", error.message));
            }
        }
    }
    let tail = verify3_front_cut_stderr_tail();
    let error = AcpError::Rpc(JsonRpcError {
        code: -32000,
        message: "agent failed".into(),
        data: None,
    })
    .into_provider_error(&tail);
    if shareable_text(&error).contains("quillmere") {
        leaks.push(format!("acp_front_cut_tail: {}", error.presentation.detail));
    }
    assert!(
        leaks.is_empty(),
        "{} synthetic leaks:\n{}",
        leaks.len(),
        leaks.join("\n")
    );
}

#[test]
fn verify3_unknown_inputs_remain_visible_owner_locally() {
    // The owner still sees what the provider said (TUI / CLI print).
    for (name, message, _) in VERIFY3_CASES.iter().take(12) {
        let error =
            replay_openai_http_error(403, None, body("permission_error", message).as_bytes());
        let raw = error.provider_raw_detail.expect("raw local");
        assert!(!raw.is_empty(), "{name}");
        assert!(error.presentation.detail.ends_with(WITHHELD), "{name}");
    }
}
