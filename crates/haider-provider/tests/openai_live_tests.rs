//! Explicitly gated native OpenAI smoke. Default test lanes stay network-free.

#![allow(clippy::expect_used)]

use haider_accounts::{CredentialAlias, MemoryVault, Vault, import_env};
use haider_protocol::provider::StreamEvent;
use haider_provider::{Message, OpenAiProvider, Provider, TurnRequest};

const LIVE_GATE: &str = "HAIDER_LIVE_PROVIDER_TESTS";
const KEY_ENV: &str = "HAIDER_OPENAI_API_KEY";
const MODEL_ENV: &str = "HAIDER_OPENAI_MODEL";

#[tokio::test]
#[ignore = "live OpenAI smoke; requires HAIDER_LIVE_PROVIDER_TESTS=1"]
async fn live_openai_responses_text_smoke_is_explicitly_gated() {
    if std::env::var(LIVE_GATE).as_deref() != Ok("1") {
        return;
    }
    let model = std::env::var(MODEL_ENV).expect("live OpenAI smoke requires HAIDER_OPENAI_MODEL");
    let vault = MemoryVault::new();
    let alias =
        import_env(&vault, "openai", KEY_ENV).expect("imports live key through accounts bridge");
    assert_eq!(alias, CredentialAlias::new("openai-env"));
    let credential = vault.resolve(&alias).expect("resolves imported live key");
    let provider = OpenAiProvider::new(credential, &model)
        .expect("OpenAI client")
        .with_account(alias);
    let request = TurnRequest {
        messages: vec![Message::user_text("Reply with exactly: haider-live-ok")],
        model,
        max_tokens: 64,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
    };
    let mut stream = provider
        .stream_turn(request)
        .await
        .expect("live stream starts");
    let mut saw_text = false;
    let mut saw_usage = false;
    let mut saw_finish = false;
    while let Some(item) = stream.recv().await {
        match item.expect("live stream item") {
            StreamEvent::TextDelta { text } => saw_text |= !text.is_empty(),
            StreamEvent::UsageUpdate(_) => saw_usage = true,
            StreamEvent::Finish { .. } => {
                saw_finish = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_text);
    assert!(saw_usage);
    assert!(saw_finish);
}
