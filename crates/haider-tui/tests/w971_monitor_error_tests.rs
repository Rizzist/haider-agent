#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::error::ErrorCode;
use haider_provider::replay_anthropic_http_error;
use haider_tui::plain::render_plain;
use haider_tui::projection::SessionProjection;

#[test]
fn provider_400_detail_survives_journal_roundtrip_and_error_card_projection() {
    let detail = "messages.2: tool_use ids were found without tool_result blocks immediately after: fixture-call";
    let body = serde_json::json!({"error": {"type": "invalid_request_error", "message": detail}})
        .to_string();
    let error = replay_anthropic_http_error(400, None, body.as_bytes())
        .with_http_metadata(400, Some("req-fixture-400"));
    let event = EventPayload::RunFailed {
        code: ErrorCode::ProviderError,
        message: error.message,
        retryable: error.retryable,
        presentation: Some(error.presentation),
    };
    let journal = serde_json::to_string(&event).expect("journal JSON");
    let replayed: EventPayload = serde_json::from_str(&journal).expect("journal replay");
    let mut projection = SessionProjection::default();
    projection.apply(&replayed);
    let card = render_plain(&projection, 0, None);
    assert!(card.contains("Provider rejected the request"));
    assert!(card.contains(detail));
    assert!(card.contains("invalid-provider-request"));
    assert!(card.contains("HTTP 400"));
    assert!(card.contains("req-fixture-400"));
    assert!(!card.contains("The provider could not accept this request shape."));
    if let Some(directory) = std::env::var_os("HAIDER_MONITOR_TEST_EVIDENCE") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("evidence directory");
        std::fs::write(directory.join("error-card.txt"), card).expect("card evidence");
        std::fs::write(directory.join("journal-error.json"), journal).expect("journal evidence");
    }
}
