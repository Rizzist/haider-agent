//! Runtime laws for the U1 `usage.report` service: meter routing, the
//! cache/poll floor, typed unavailability, JWT identity enrichment, and the
//! journal fold's attribution arithmetic.
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_accounts::{MemoryVault, SecretHandle, Vault};
use haider_protocol::EventPayload;
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, CredentialAlias, DeviceId, EventId, RunId, SessionId};
use haider_protocol::provider::{AccountUsage, Usage, UsageSource};
use haider_protocol::session::ModelSelected;
use haider_protocol::usage::AccountMeterStateV1;
use haider_provider::MeterUnavailable;

use super::{
    MeterTokenSource, OpenAiTokenIdentity, SessionFolder, UsageMeterHttp, UsageReportService,
    attribute_session, meter_for, openai_token_identity,
};

/// The exact anthropic-oauth body captured LIVE on 2026-08-05 (also frozen
/// as `haider-provider/tests/fixtures/usage/anthropic_oauth_usage_live.json`).
const ANTHROPIC_LIVE_BODY: &str = r#"{"five_hour":{"utilization":60.0,"resets_at":"2026-08-05T09:50:00.833669+00:00","limit_dollars":null,"used_dollars":null,"remaining_dollars":null},"seven_day":{"utilization":12.0,"resets_at":"2026-08-05T21:00:00.833689+00:00","limit_dollars":null,"used_dollars":null,"remaining_dollars":null},"seven_day_opus":null,"seven_day_sonnet":null,"extra_usage":{"is_enabled":false},"limits":[]}"#;

fn descriptor(provider: &str, alias: &str, auth_method: AuthMethod) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: provider.into(),
        base_url: None,
        auth_method,
        identity: format!("{alias}@example.invalid"),
        status: CredentialStatus::Ok,
        active: true,
    }
}

fn snapshot(descriptors: Vec<CredentialDescriptor>) -> crate::accounts::AccountsSnapshot {
    Arc::new(std::sync::Mutex::new(descriptors))
}

/// Mints a real `SecretHandle` through the vault seam (the only mint).
fn secret(bytes: &[u8]) -> SecretHandle {
    let vault = MemoryVault::default();
    let alias = CredentialAlias::new("mint");
    vault.put(&alias, bytes).expect("put");
    vault.resolve(&alias).expect("resolve")
}

struct StubTokens {
    bytes: Vec<u8>,
}

#[async_trait::async_trait]
impl MeterTokenSource for StubTokens {
    async fn bearer(
        &self,
        _descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, MeterUnavailable> {
        Ok(secret(&self.bytes))
    }
}

struct FailingTokens;

#[async_trait::async_trait]
impl MeterTokenSource for FailingTokens {
    async fn bearer(
        &self,
        _descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, MeterUnavailable> {
        Err(MeterUnavailable::new("credential_unavailable"))
    }
}

/// Counts calls and serves a fixed (status, body) per URL.
struct StubHttp {
    calls: AtomicU64,
    responses: HashMap<String, (u16, Vec<u8>)>,
}

impl StubHttp {
    fn new(responses: impl IntoIterator<Item = (String, (u16, Vec<u8>))>) -> Self {
        Self {
            calls: AtomicU64::new(0),
            responses: responses.into_iter().collect(),
        }
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl UsageMeterHttp for StubHttp {
    async fn get(
        &self,
        url: &str,
        _bearer: &SecretHandle,
        _extra_headers: &[(&'static str, &'static str)],
    ) -> Result<(u16, Vec<u8>), MeterUnavailable> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(url)
            .cloned()
            .ok_or_else(|| MeterUnavailable::new("transport_error"))
    }
}

fn service_with_clock(
    descriptors: Vec<CredentialDescriptor>,
    tokens: Option<Arc<dyn MeterTokenSource>>,
    http: Arc<dyn UsageMeterHttp>,
    clock: Arc<AtomicU64>,
) -> UsageReportService {
    UsageReportService::with_clock(
        snapshot(descriptors),
        tokens,
        http,
        Box::new(move || clock.load(Ordering::SeqCst)),
    )
}

async fn empty_store() -> (tempfile::TempDir, haider_core::SqliteStoreHandle) {
    let root = tempfile::tempdir().expect("profile dir");
    let store = haider_core::SqliteStoreHandle::open(root.path())
        .await
        .expect("store");
    (root, store)
}

/// LAW (api_key_and_custom_accounts_are_local_only_and_never_probe_http):
/// API-key and custom-provider accounts have NO server meter — the report
/// answers `local_only` for each and the HTTP seam is never touched; an
/// OAuth account with no token source is typed `unavailable`, never a crash
/// and never a fabricated meter.
#[tokio::test]
async fn api_key_and_custom_accounts_are_local_only_and_never_probe_http() {
    let http = Arc::new(StubHttp::new([]));
    let service = service_with_clock(
        vec![
            descriptor("openai", "billing-key", AuthMethod::ApiKey),
            descriptor("opencode-zen", "zen-key", AuthMethod::ApiKey),
            descriptor("gemini", "gem-key", AuthMethod::ApiKey),
            descriptor("anthropic-oauth", "max-sub", AuthMethod::OAuth),
        ],
        None,
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        Arc::new(AtomicU64::new(1_000_000)),
    );
    let (_root, store) = empty_store().await;
    let report = service.report(&store).await.expect("report");
    assert_eq!(report.generated_at_ms, 1_000_000);
    assert_eq!(report.accounts.len(), 4);
    for entry in &report.accounts[..3] {
        assert_eq!(entry.meter, AccountMeterStateV1::LocalOnly, "{entry:?}");
        assert_eq!(entry.auth_method, AuthMethod::ApiKey);
    }
    assert_eq!(
        report.accounts[3].meter,
        AccountMeterStateV1::Unavailable {
            reason: "credential_broker_unavailable".into()
        }
    );
    assert_eq!(http.calls(), 0, "no meter endpoint may be probed");
}

/// LAW (oauth_meter_reading_normalizes_and_respects_the_poll_floor): a live
/// anthropic reading lands normalized (60.0 percent → 0.6 fraction, RFC 3339
/// resets → Unix ms), and a second report inside the 180 s floor serves the
/// CACHE — exactly one HTTP call — while a report at the floor refetches.
#[tokio::test]
async fn oauth_meter_reading_normalizes_and_respects_the_poll_floor() {
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let http = Arc::new(StubHttp::new([(
        haider_provider::ANTHROPIC_OAUTH_USAGE_URL.to_owned(),
        (200, ANTHROPIC_LIVE_BODY.as_bytes().to_vec()),
    )]));
    let service = service_with_clock(
        vec![descriptor("anthropic-oauth", "max-sub", AuthMethod::OAuth)],
        Some(Arc::new(StubTokens {
            bytes: b"redacted-test-token".to_vec(),
        })),
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        Arc::clone(&clock),
    );
    let (_root, store) = empty_store().await;
    let report = service.report(&store).await.expect("report");
    let entry = &report.accounts[0];
    assert_eq!(entry.identity.as_deref(), Some("max-sub@example.invalid"));
    assert_eq!(entry.plan, None);
    let AccountMeterStateV1::Metered { windows } = &entry.meter else {
        panic!("expected a metered state, got {:?}", entry.meter);
    };
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].window, "five_hour");
    assert!((windows[0].utilization - 0.6).abs() < 1e-9);
    assert_eq!(windows[0].resets_at_ms, Some(1_785_923_400_833));
    assert_eq!(windows[1].window, "seven_day");
    assert!((windows[1].utilization - 0.12).abs() < 1e-9);
    assert_eq!(http.calls(), 1);

    // Inside the floor: served from cache, no second call.
    clock.store(1_000_000 + 179_999, Ordering::SeqCst);
    let cached = service.report(&store).await.expect("cached report");
    assert!(matches!(
        cached.accounts[0].meter,
        AccountMeterStateV1::Metered { .. }
    ));
    assert_eq!(http.calls(), 1, "the poll floor forbids a refetch");

    // At the floor: refetched.
    clock.store(1_000_000 + 180_000, Ordering::SeqCst);
    let _ = service.report(&store).await.expect("refetched report");
    assert_eq!(http.calls(), 2);
}

/// LAW (meter_failures_are_typed_cached_and_never_hammered): an HTTP 429
/// reading is a typed `unavailable` (`http_status_429`) that is CACHED like
/// a success — an immediate second report does not hammer the endpoint —
/// and a token-source failure surfaces its own typed reason without any
/// HTTP call.
#[tokio::test]
async fn meter_failures_are_typed_cached_and_never_hammered() {
    let clock = Arc::new(AtomicU64::new(5_000_000));
    let http = Arc::new(StubHttp::new([(
        haider_provider::ANTHROPIC_OAUTH_USAGE_URL.to_owned(),
        (429, b"{}".to_vec()),
    )]));
    let service = service_with_clock(
        vec![descriptor("anthropic-oauth", "max-sub", AuthMethod::OAuth)],
        Some(Arc::new(StubTokens {
            bytes: b"redacted-test-token".to_vec(),
        })),
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        Arc::clone(&clock),
    );
    let (_root, store) = empty_store().await;
    let report = service.report(&store).await.expect("report");
    assert_eq!(
        report.accounts[0].meter,
        AccountMeterStateV1::Unavailable {
            reason: "http_status_429".into()
        }
    );
    let again = service.report(&store).await.expect("second report");
    assert_eq!(
        again.accounts[0].meter,
        AccountMeterStateV1::Unavailable {
            reason: "http_status_429".into()
        }
    );
    assert_eq!(http.calls(), 1, "failures are cached, not hammered");

    let failing = service_with_clock(
        vec![descriptor("kimi-oauth", "kimi-main", AuthMethod::OAuth)],
        Some(Arc::new(FailingTokens)),
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        clock,
    );
    let report = failing.report(&store).await.expect("report");
    assert_eq!(
        report.accounts[0].meter,
        AccountMeterStateV1::Unavailable {
            reason: "credential_unavailable".into()
        }
    );
    assert_eq!(http.calls(), 1, "no HTTP without a token");
}

fn jwt_with_claims(claims: &serde_json::Value) -> Vec<u8> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    format!("{header}.{payload}.unsigned-test-signature").into_bytes()
}

/// LAW (openai_token_claims_supply_email_and_plan_with_meter_precedence):
/// the openai-oauth access token's JWT payload decodes (unverified, display
/// only) into email (top-level, with the `https://api.openai.com/profile`
/// fallback) and `chatgpt_plan_type`; in the report the JWT email replaces
/// the descriptor identity, the meter's own `plan_type` beats the JWT plan,
/// and a malformed token degrades to descriptor identity — never an error.
#[tokio::test]
async fn openai_token_claims_supply_email_and_plan_with_meter_precedence() {
    // Pure decode: top-level email + auth plan.
    let identity = openai_token_identity(&jwt_with_claims(&serde_json::json!({
        "email": "person@example.invalid",
        "https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}
    })));
    assert_eq!(
        identity,
        OpenAiTokenIdentity {
            email: Some("person@example.invalid".into()),
            plan: Some("pro".into()),
        }
    );
    // Profile fallback when the top-level email is absent.
    let identity = openai_token_identity(&jwt_with_claims(&serde_json::json!({
        "https://api.openai.com/profile": {"email": "fallback@example.invalid"}
    })));
    assert_eq!(identity.email.as_deref(), Some("fallback@example.invalid"));
    // Malformed input is None-shaped, never a panic.
    assert_eq!(
        openai_token_identity(b"not-a-jwt"),
        OpenAiTokenIdentity::default()
    );

    // Service precedence: wham plan_type ("plus") beats the JWT plan
    // ("pro"); the JWT email replaces the descriptor identity.
    let wham = br#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":6,"reset_at":1738300000,"limit_window_seconds":18000}}}"#;
    let http = Arc::new(StubHttp::new([(
        haider_provider::OPENAI_OAUTH_USAGE_URL.to_owned(),
        (200, wham.to_vec()),
    )]));
    let service = service_with_clock(
        vec![descriptor(
            "openai-oauth",
            "work-chatgpt",
            AuthMethod::OAuth,
        )],
        Some(Arc::new(StubTokens {
            bytes: jwt_with_claims(&serde_json::json!({
                "email": "person@example.invalid",
                "https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}
            })),
        })),
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        Arc::new(AtomicU64::new(9_000_000)),
    );
    let (_root, store) = empty_store().await;
    let report = service.report(&store).await.expect("report");
    let entry = &report.accounts[0];
    assert_eq!(entry.identity.as_deref(), Some("person@example.invalid"));
    assert_eq!(entry.plan.as_deref(), Some("plus"), "meter plan wins");
    assert!(matches!(entry.meter, AccountMeterStateV1::Metered { .. }));
}

fn envelope(
    seq: u64,
    run: Option<&str>,
    agent: Option<&str>,
    committed_at_ms: u64,
    payload: serde_json::Value,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: haider_protocol::envelope::SCHEMA_VERSION,
        event_id: EventId::new(format!("ev-{seq}")),
        seq,
        session_id: SessionId::new("s-usage"),
        branch_id: None,
        run_id: run.map(RunId::new),
        agent_id: agent.map(AgentId::new),
        device_id: DeviceId::new("d-test"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload,
    }
}

fn usage_payload(usage: Usage) -> serde_json::Value {
    serde_json::to_value(EventPayload::Usage(usage)).expect("usage payload")
}

fn plain_usage(input: u64, output: u64, account: &str) -> Usage {
    Usage {
        input,
        output,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: Some(CredentialAlias::new(account)),
        accounts: Vec::new(),
    }
}

fn completed_tool(name: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::to_value(EventPayload::Item(
        haider_protocol::item::ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("it-1"),
            item: haider_protocol::item::TurnItem::ToolCall {
                call_id: "c-1".into(),
                name: name.into(),
                args,
                status: haider_protocol::item::ToolStatus::Completed,
            },
        },
    ))
    .expect("item payload")
}

/// LAW (session_folder_attributes_tokens_cost_duration_and_loc): the
/// journal fold takes the LAST cumulative usage snapshot per (run, agent)
/// (never a sum of snapshots), prices each chunk under the model active
/// when it committed (`model_selected` switches later chunks), splits
/// rotation subtotals per account, counts lines only from COMPLETED fs
/// receipts, skips unattributed usage — and session count, span, and LOC
/// attribute to the dominant account.
#[test]
fn session_folder_attributes_tokens_cost_duration_and_loc() {
    let mut folder = SessionFolder::new("claude-sonnet-4-5");
    // Run 1, head agent: cumulative snapshots — the last one wins.
    folder.push(&envelope(
        1,
        Some("r-1"),
        None,
        1_000,
        usage_payload(plain_usage(400_000, 3_000, "personal-max")),
    ));
    folder.push(&envelope(
        2,
        Some("r-1"),
        None,
        2_000,
        usage_payload(plain_usage(1_000_000, 100_000, "personal-max")),
    ));
    // Same run, a SUBAGENT: its own cumulative lane, additive to the head's.
    folder.push(&envelope(
        3,
        Some("r-1"),
        Some("a-child"),
        3_000,
        usage_payload(plain_usage(200_000, 10_000, "personal-max")),
    ));
    // Completed fs receipts count lines; a failed one must not.
    folder.push(&envelope(
        4,
        Some("r-1"),
        None,
        4_000,
        completed_tool(
            "fs_patch",
            serde_json::json!({"path":"src/lib.rs","preimage":"a\n","replacement":"b\nc\nd\n"}),
        ),
    ));
    folder.push(&envelope(
        5,
        Some("r-1"),
        None,
        5_000,
        completed_tool(
            "fs_write",
            serde_json::json!({"path":"new.rs","content":"x\ny\n"}),
        ),
    ));
    folder.push(&envelope(
        6,
        Some("r-1"),
        None,
        6_000,
        serde_json::to_value(EventPayload::Item(
            haider_protocol::item::ItemEvent::Completed {
                item_id: haider_protocol::ids::ItemId::new("it-2"),
                item: haider_protocol::item::TurnItem::ToolCall {
                    call_id: "c-2".into(),
                    name: "fs_patch".into(),
                    args: serde_json::json!({"preimage":"zzz\n","replacement":"qqq\n"}),
                    status: haider_protocol::item::ToolStatus::Failed,
                },
            },
        ))
        .expect("failed item"),
    ));
    // Model switch: LATER usage prices under gpt-5.2.
    folder.push(&envelope(
        7,
        None,
        None,
        7_000,
        ModelSelected {
            provider: "openai".into(),
            model: "gpt-5.2".into(),
        }
        .to_payload_value()
        .expect("model selected"),
    ));
    // Run 2 under the new model, with rotation subtotals across accounts.
    folder.push(&envelope(
        8,
        Some("r-2"),
        None,
        8_000,
        usage_payload(Usage {
            input: 3_000_000,
            output: 300_000,
            reasoning: 0,
            cached: 0,
            source: UsageSource::ProviderReported,
            account: None,
            accounts: vec![
                AccountUsage {
                    account: CredentialAlias::new("personal-max"),
                    input: 1_000_000,
                    output: 100_000,
                    reasoning: 0,
                    cached: 0,
                    source: UsageSource::ProviderReported,
                },
                AccountUsage {
                    account: CredentialAlias::new("billing-key"),
                    input: 2_000_000,
                    output: 200_000,
                    reasoning: 0,
                    cached: 0,
                    source: UsageSource::ProviderReported,
                },
            ],
        }),
    ));
    // Unattributed usage: no account, no subtotals — skipped, not invented.
    folder.push(&envelope(
        9,
        Some("r-3"),
        None,
        9_000,
        usage_payload(Usage {
            input: 999,
            output: 999,
            reasoning: 0,
            cached: 0,
            source: UsageSource::Estimated,
            account: None,
            accounts: Vec::new(),
        }),
    ));

    let stats = folder.finish();
    assert_eq!(stats.last_committed_at_ms, 9_000);
    assert_eq!(stats.lines_added, 5, "3 patched + 2 written");
    assert_eq!(stats.lines_removed, 1, "failed receipts never count");

    let max = stats.tokens[&CredentialAlias::new("personal-max")];
    // Head lane last snapshot (1M/100k) + subagent lane (200k/10k) +
    // rotation subtotal (1M/100k).
    assert_eq!(max.input, 2_200_000);
    assert_eq!(max.output, 210_000);
    // Cost: sonnet chunks (1M in @ $3 + 100k out @ $15 = 4.5) + subagent
    // (200k @ 3 = 0.6 + 10k @ 15 = 0.15) + gpt-5.2 subtotal
    // (1M @ 1.25 + 100k @ 10 = 2.25).
    let expected_max_cost = 3.0 + 1.5 + 0.6 + 0.15 + 1.25 + 1.0;
    let max_cost = max.est_cost_usd.expect("priced");
    assert!(
        (max_cost - expected_max_cost).abs() < 1e-9,
        "cost {max_cost} != {expected_max_cost}"
    );
    let billing = stats.tokens[&CredentialAlias::new("billing-key")];
    assert_eq!(billing.input, 2_000_000);
    let billing_cost = billing.est_cost_usd.expect("priced");
    assert!(((2.5 + 2.0) - billing_cost).abs() < 1e-9);
    assert_eq!(stats.tokens.len(), 2, "unattributed usage joins no account");

    // Attribution: billing-key dominates (2.2M vs 2.41M magnitudes —
    // personal-max: 2.2M + 210k = 2.41M; billing: 2.2M) — recompute:
    // personal-max magnitude 2_410_000, billing 2_200_000 → personal-max
    // is dominant and takes the session, span, and LOC.
    let mut totals = HashMap::new();
    attribute_session(&mut totals, 500, stats);
    let max_totals = &totals[&CredentialAlias::new("personal-max")];
    assert_eq!(max_totals.sessions, 1);
    assert_eq!(max_totals.total_duration_ms, 8_500, "9_000 - created 500");
    assert_eq!(max_totals.lines_added, 5);
    assert_eq!(max_totals.lines_removed, 1);
    let billing_totals = &totals[&CredentialAlias::new("billing-key")];
    assert_eq!(billing_totals.sessions, 0);
    assert_eq!(billing_totals.total_duration_ms, 0);
    assert_eq!(billing_totals.lines_added, 0);
}

/// LAW (meter_routing_is_flavor_and_provider_strict): only the three
/// sanctioned OAuth subscriptions route to a meter; the SAME provider names
/// under an API key, and any other provider under OAuth, are meterless.
#[test]
fn meter_routing_is_flavor_and_provider_strict() {
    use haider_provider::UsageMeterEndpoint;
    let cases = [
        ("openai-oauth", Some(UsageMeterEndpoint::OpenAiOauth)),
        ("anthropic-oauth", Some(UsageMeterEndpoint::AnthropicOauth)),
        ("kimi-oauth", Some(UsageMeterEndpoint::KimiOauth)),
        ("openai", None),
        ("anthropic", None),
        ("gemini", None),
        ("opencode-zen", None),
    ];
    for (provider, expected) in cases {
        assert_eq!(
            meter_for(&descriptor(provider, "a", AuthMethod::OAuth)),
            expected,
            "oauth {provider}"
        );
        assert_eq!(
            meter_for(&descriptor(provider, "a", AuthMethod::ApiKey)),
            None,
            "api-key {provider} must never meter"
        );
    }
}
