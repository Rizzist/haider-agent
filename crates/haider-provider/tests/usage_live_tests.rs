//! Explicitly gated LIVE poll of the anthropic-oauth usage meter (U1).
//!
//! Ignored by default; lanes must not run it. Gate:
//! `HAIDER_LIVE_USAGE_TESTS=1` plus a real Claude subscription access token
//! in `HAIDER_ANTHROPIC_OAUTH_TOKEN`. The token is read from the
//! environment, sent only as the Authorization header, and never printed;
//! the asserted body carries no secret material.
#![allow(clippy::expect_used)]

use haider_provider::UsageMeterEndpoint;

const LIVE_GATE: &str = "HAIDER_LIVE_USAGE_TESTS";
const TOKEN_ENV: &str = "HAIDER_ANTHROPIC_OAUTH_TOKEN";

/// LAW (live_anthropic_oauth_meter_parses_and_normalizes): one real GET to
/// the researched endpoint, through the REAL header set and parser, yields
/// at least one named window whose utilization is already the normalized
/// 0–1 fraction — proving the live shape still matches the frozen fixture
/// contract.
#[tokio::test]
#[ignore = "live anthropic-oauth usage poll; requires HAIDER_LIVE_USAGE_TESTS=1 + HAIDER_ANTHROPIC_OAUTH_TOKEN"]
async fn live_anthropic_oauth_meter_parses_and_normalizes() {
    if std::env::var(LIVE_GATE).as_deref() != Ok("1") {
        eprintln!("skipped: set {LIVE_GATE}=1 to run");
        return;
    }
    let token = std::env::var(TOKEN_ENV).expect("live token env");
    let endpoint = UsageMeterEndpoint::AnthropicOauth;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("client");
    let mut request = client.get(endpoint.url()).bearer_auth(&token);
    for (name, value) in endpoint.extra_headers() {
        request = request.header(*name, *value);
    }
    let response = request.send().await.expect("live GET");
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("live body");
    let reading = endpoint
        .parse(status, &body)
        .expect("live body parses through the real parser");
    assert!(
        !reading.windows.is_empty(),
        "a live subscription reports at least one window"
    );
    for window in &reading.windows {
        assert!(
            (0.0..=1.0).contains(&window.utilization),
            "normalized on arrival: {} = {}",
            window.window,
            window.utilization
        );
        eprintln!(
            "live window {} utilization={:.3} resets_at_ms={:?}",
            window.window, window.utilization, window.resets_at_ms
        );
    }
}
