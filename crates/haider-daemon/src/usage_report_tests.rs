#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

//! Runtime laws for the U1 `usage.report` service: meter routing, the
//! cache/poll floor, typed unavailability, JWT identity enrichment, and the
//! journal fold's attribution arithmetic.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_accounts::{MemoryVault, SecretHandle, Vault};
use haider_protocol::EventPayload;
use haider_protocol::agent::AgentUsageMetrics;
use haider_protocol::credential::{
    AccountIdentity, AuthMethod, CredentialAttentionReason, CredentialDescriptor, CredentialStatus,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, CredentialAlias, DeviceId, EventId, RunId, SessionId};
use haider_protocol::provider::{
    AccountUsage, CacheCostEstimate, CacheStatAvailability, NormalizedUsage, RequestUsage, Usage,
    UsageRequestKind, UsageScope, UsageSource,
};
use haider_protocol::session::ModelSelected;
use haider_protocol::usage::{
    AccountMeterStateV1, HaiderCodeAllowanceStateV1, HaiderCodePlanSnapshotV1,
    HaiderCodeWeeklyAllowanceV1, UsageHistoryModelTotalV1, UsageHistoryRangeDayV1,
};
use haider_provider::MeterUnavailable;

use super::{
    MeterTokenSource, OpenAiTokenIdentity, SessionFolder, UsageMeterHttp, UsageReportService,
    attribute_session, credential_meter_unavailable_reason, enrich_usage_history_costs, meter_for,
    openai_token_identity,
};
use crate::haider_code_plan::{PlanFetchOutcome, PlanMeterValues, classify_account_response};

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
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

fn openai_descriptor(alias: &str, account_id: &str) -> CredentialDescriptor {
    let mut descriptor = descriptor("openai-oauth", alias, AuthMethod::OAuth);
    descriptor.account_identity = Some(AccountIdentity {
        email: Some(format!("{alias}@example.invalid")),
        display_name: None,
        account_id: Some(account_id.to_owned()),
        plan: Some("pro".into()),
        issuer: Some("https://auth.openai.com".into()),
        captured_at: 1,
        verified: false,
    });
    descriptor
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

#[test]
fn expired_revoked_and_linked_source_failures_have_distinct_meter_reasons() {
    assert_eq!(
        credential_meter_unavailable_reason(&CredentialStatus::Expired),
        Some("credential_expired")
    );
    assert_eq!(
        credential_meter_unavailable_reason(&CredentialStatus::Revoked),
        Some("credential_revoked")
    );
    for (reason, expected) in [
        (
            CredentialAttentionReason::SourceGone,
            "credential_source_gone",
        ),
        (
            CredentialAttentionReason::SourceUnreadable,
            "credential_source_unreadable",
        ),
        (
            CredentialAttentionReason::OriginClientRequired,
            "credential_origin_client_required",
        ),
        (
            CredentialAttentionReason::PolicyBlocked,
            "credential_policy_blocked",
        ),
    ] {
        assert_eq!(
            credential_meter_unavailable_reason(&CredentialStatus::NeedsAttention { reason }),
            Some(expected)
        );
    }
}

/// Counts calls and serves a fixed (status, body) per URL.
type CapturedMeterRequest = (String, Vec<(&'static str, &'static str)>, Option<String>);

struct StubHttp {
    calls: AtomicU64,
    responses: HashMap<String, (u16, Vec<u8>)>,
    requests: std::sync::Mutex<Vec<CapturedMeterRequest>>,
}

impl StubHttp {
    fn new(responses: impl IntoIterator<Item = (String, (u16, Vec<u8>))>) -> Self {
        Self {
            calls: AtomicU64::new(0),
            responses: responses.into_iter().collect(),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<CapturedMeterRequest> {
        self.requests.lock().expect("request capture").clone()
    }
}

#[async_trait::async_trait]
impl UsageMeterHttp for StubHttp {
    async fn get(
        &self,
        url: &str,
        _bearer: &SecretHandle,
        extra_headers: &[(&'static str, &'static str)],
        chatgpt_account_id: Option<&str>,
    ) -> Result<(u16, Vec<u8>), MeterUnavailable> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().expect("request capture").push((
            url.to_owned(),
            extra_headers.to_vec(),
            chatgpt_account_id.map(str::to_owned),
        ));
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

/// MUTATION CHECK: route Haider Code through the OAuth-only `meter_for`, use
/// the public float to derive history basis points, or default missing credit
/// balances to zero. Expected runtime failure: LocalOnly/Metered transitions,
/// exact 3900 bp, sample count, or `credits=None` changes.
#[tokio::test]
async fn haider_code_held_plan_is_metered_and_arrival_appends_one_exact_sample() {
    let clock = Arc::new(AtomicU64::new(1_777_075_200_000));
    let service = service_with_clock(
        vec![descriptor("haider-code", "haider-main", AuthMethod::ApiKey)],
        None,
        Arc::new(StubHttp::new([])),
        Arc::clone(&clock),
    );
    let (_root, store) = empty_store().await;

    let before = service.report(&store).await.expect("report before status");
    assert_eq!(before.accounts[0].meter, AccountMeterStateV1::LocalOnly);

    let snapshot = HaiderCodePlanSnapshotV1 {
        plan: Some("go-max".into()),
        plan_label: Some("Go Max".into()),
        weekly_allowance: Some(HaiderCodeWeeklyAllowanceV1 {
            percent_remaining: Some(61.0),
            state: Some(HaiderCodeAllowanceStateV1::Ok),
            resets_at_ms: Some(1_900_000_000_000),
            grace_until_ms: Some(1_900_000_060_000),
        }),
        usage_credits_usd: None,
        auto_topup_enabled: None,
        hold: None,
        models_live: None,
        refresh_after_s: Some(37),
        cached: Some(false),
    };
    service
        .capture_haider_code_plan_status(
            &store,
            CredentialAlias::new("haider-main"),
            snapshot,
            PlanMeterValues {
                weekly_percent_remaining: Some(61),
                credits: None,
                hold: None,
            },
        )
        .await
        .expect("capture plan status");

    let after = service.report(&store).await.expect("report after status");
    assert_eq!(after.accounts[0].plan.as_deref(), Some("go-max"));
    let AccountMeterStateV1::Metered { windows } = &after.accounts[0].meter else {
        panic!("held Haider Code plan must be metered");
    };
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].window, "weekly");
    assert!((windows[0].utilization - 0.39).abs() < 1e-12);

    let day = store
        .usage_history_day("2026-04-25".into())
        .await
        .expect("history read")
        .expect("history day");
    assert_eq!(day.meter_samples.len(), 1);
    let sample = &day.meter_samples[0];
    assert_eq!(sample.account, "haider-main");
    assert_eq!(sample.window, "weekly");
    assert_eq!(sample.basis_points, 3_900);
    assert_eq!(sample.resets_at_ms, Some(1_900_000_000_000));
    assert_eq!(sample.grace_until_ms, Some(1_900_000_060_000));
    assert_eq!(sample.plan.as_deref(), Some("go-max"));
    assert_eq!(sample.credits, None, "absence is not a zero balance");
    assert_eq!(sample.hold, None, "structured hold state is not a balance");
    assert_eq!(sample.stale, Some(false));

    let _ = service.report(&store).await.expect("second report");
    let day = store
        .usage_history_day("2026-04-25".into())
        .await
        .expect("second history read")
        .expect("history day");
    assert_eq!(
        day.meter_samples.len(),
        1,
        "report rendering must not duplicate an arrival sample"
    );
}

/// MUTATION CHECK: drop `meter.credits` at the capture-to-ledger seam.
/// Expected runtime failure: the raw provider integer no longer reaches the
/// frozen sample verbatim even though classification retained its provenance.
#[tokio::test]
async fn haider_code_integer_credit_arrival_reaches_ledger_verbatim() {
    let clock = Arc::new(AtomicU64::new(1_777_075_200_000));
    let service = service_with_clock(
        vec![descriptor("haider-code", "haider-main", AuthMethod::ApiKey)],
        None,
        Arc::new(StubHttp::new([])),
        clock,
    );
    let (_root, store) = empty_store().await;
    let reading = classify_account_response(
        200,
        br#"{
          "plan":"go",
          "weekly_allowance":{"percent_remaining":61},
          "usage_credits_usd":-17
        }"#,
    );
    let PlanFetchOutcome::Snapshot(reading) = reading else {
        panic!("provider plan snapshot");
    };
    service
        .capture_haider_code_plan_status(
            &store,
            CredentialAlias::new("haider-main"),
            reading.snapshot,
            reading.meter,
        )
        .await
        .expect("capture integer balance");

    let day = store
        .usage_history_day("2026-04-25".into())
        .await
        .expect("history read")
        .expect("history day");
    assert_eq!(day.meter_samples.len(), 1);
    assert_eq!(day.meter_samples[0].basis_points, 3_900);
    assert_eq!(day.meter_samples[0].credits, Some(-17));
}

#[tokio::test]
async fn haider_code_float_percent_is_held_but_never_written_to_history() {
    let clock = Arc::new(AtomicU64::new(1_777_075_200_000));
    let service = service_with_clock(
        vec![descriptor("haider-code", "haider-main", AuthMethod::ApiKey)],
        None,
        Arc::new(StubHttp::new([])),
        clock,
    );
    let (_root, store) = empty_store().await;
    let snapshot = HaiderCodePlanSnapshotV1 {
        plan: Some("go".into()),
        plan_label: None,
        weekly_allowance: Some(HaiderCodeWeeklyAllowanceV1 {
            percent_remaining: Some(61.25),
            state: None,
            resets_at_ms: None,
            grace_until_ms: None,
        }),
        usage_credits_usd: None,
        auto_topup_enabled: None,
        hold: None,
        models_live: None,
        refresh_after_s: None,
        cached: None,
    };
    service
        .capture_haider_code_plan_status(
            &store,
            CredentialAlias::new("haider-main"),
            snapshot,
            PlanMeterValues::default(),
        )
        .await
        .expect("hold float status");

    let report = service.report(&store).await.expect("float report");
    assert!(matches!(
        report.accounts[0].meter,
        AccountMeterStateV1::Metered { .. }
    ));
    assert!(
        store
            .usage_history_day("2026-04-25".into())
            .await
            .expect("history read")
            .is_none(),
        "a float percentage cannot be reconstructed into a ledger sample"
    );
}

/// Review finding (954c integration): an OUT-OF-RANGE provider integer
/// (percent_remaining > 100) must skip the sample, not saturate into a
/// fabricated basis-points value — checked_sub's None arm is load-bearing.
///
/// MUTATION CHECK (executed at review): replace the checked_sub-else-skip
/// with `saturating_sub(..).max(1)` — this pin fails with a fabricated
/// sample where none may exist.
#[tokio::test]
async fn haider_code_out_of_range_percent_never_fabricates_a_sample() {
    let clock = Arc::new(AtomicU64::new(1_777_075_200_000));
    let service = service_with_clock(
        vec![descriptor("haider-code", "haider-main", AuthMethod::ApiKey)],
        None,
        Arc::new(StubHttp::new([])),
        clock,
    );
    let (_root, store) = empty_store().await;
    let snapshot = HaiderCodePlanSnapshotV1 {
        plan: Some("go".into()),
        plan_label: None,
        weekly_allowance: Some(HaiderCodeWeeklyAllowanceV1 {
            percent_remaining: Some(150.0),
            state: None,
            resets_at_ms: None,
            grace_until_ms: None,
        }),
        usage_credits_usd: None,
        auto_topup_enabled: None,
        hold: None,
        models_live: None,
        refresh_after_s: None,
        cached: None,
    };
    service
        .capture_haider_code_plan_status(
            &store,
            CredentialAlias::new("haider-main"),
            snapshot,
            PlanMeterValues {
                weekly_percent_remaining: Some(150),
                credits: None,
                hold: None,
            },
        )
        .await
        .expect("hold out-of-range status");
    assert!(
        store
            .usage_history_day("2026-04-25".into())
            .await
            .expect("history read")
            .is_none(),
        "an out-of-range provider integer must skip, never saturate into bp"
    );
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
    let history = store
        .usage_history_day("1970-01-01".into())
        .await
        .expect("meter history read")
        .expect("meter history day");
    assert_eq!(
        history
            .meter_samples
            .iter()
            .map(|sample| sample.basis_points)
            .collect::<Vec<_>>(),
        [6_000, 1_200]
    );

    // Inside the floor: served from cache, no second call.
    clock.store(1_000_000 + 179_999, Ordering::SeqCst);
    let cached = service.report(&store).await.expect("cached report");
    assert!(matches!(
        cached.accounts[0].meter,
        AccountMeterStateV1::Metered { .. }
    ));
    assert_eq!(http.calls(), 1, "the poll floor forbids a refetch");
    assert_eq!(
        store
            .usage_history_day("1970-01-01".into())
            .await
            .expect("cached meter history read")
            .expect("cached meter history day")
            .meter_samples
            .len(),
        2,
        "cached report assembly must not duplicate meter samples"
    );

    // At the floor: refetched.
    clock.store(1_000_000 + 180_000, Ordering::SeqCst);
    let _ = service.report(&store).await.expect("refetched report");
    assert_eq!(http.calls(), 2);
    assert_eq!(
        store
            .usage_history_day("1970-01-01".into())
            .await
            .expect("refetched meter history read")
            .expect("refetched meter history day")
            .meter_samples
            .len(),
        4
    );

    // MUTATION CHECK: deriving basis points from a rounded display percent
    // breaks the exact vector; appending cached readings makes the length-2
    // assertion fail before the poll floor expires.
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
        vec![openai_descriptor("work-chatgpt", "chatgpt-account-123")],
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
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].2.as_deref(), Some("chatgpt-account-123"));
    assert!(
        requests[0]
            .1
            .contains(&("originator", haider_provider::OPENAI_OAUTH_USAGE_ORIGINATOR))
    );
}

/// The WHAM request must never guess an OpenAI organization or omit the
/// ID-token-derived ChatGPT account coordinate. Missing identity fails before
/// HTTP and remains branchable in the report row.
#[tokio::test]
async fn openai_meter_requires_the_chatgpt_account_id_claim_before_http() {
    let http = Arc::new(StubHttp::new([(
        haider_provider::OPENAI_OAUTH_USAGE_URL.to_owned(),
        (200, b"{}".to_vec()),
    )]));
    let service = service_with_clock(
        vec![descriptor(
            "openai-oauth",
            "missing-account",
            AuthMethod::OAuth,
        )],
        Some(Arc::new(StubTokens {
            bytes: b"redacted-test-token".to_vec(),
        })),
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        Arc::new(AtomicU64::new(9_000_000)),
    );
    let (_root, store) = empty_store().await;
    let report = service.report(&store).await.expect("typed report");
    assert_eq!(
        report.accounts[0].meter,
        AccountMeterStateV1::Unavailable {
            reason: "credential_identity_incomplete".into()
        }
    );
    assert_eq!(
        http.calls(),
        0,
        "missing account identity fails before HTTP"
    );
}

#[test]
fn attributed_history_costs_require_a_known_provider_model_pair() {
    let row = |provider: &str| UsageHistoryModelTotalV1 {
        model: "gpt-5.2".into(),
        provider: provider.into(),
        requests: 1,
        input_tokens: 1_000_000,
        output_tokens: 100_000,
        cache_read_tokens: 100_000,
        reasoning_tokens: 10_000,
        est_cost_microusd: None,
    };
    let mut days = vec![UsageHistoryRangeDayV1 {
        date: "2026-08-28".into(),
        total: None,
        models: vec![row("openai-oauth"), row("custom-openai-compatible")],
    }];
    enrich_usage_history_costs(&mut days);
    assert!(
        days[0].models[0]
            .est_cost_microusd
            .is_some_and(|cost| cost > 0),
        "a registered provider/model pair is priced"
    );
    assert_eq!(
        days[0].models[1].est_cost_microusd, None,
        "a custom provider never inherits economics from the model slug"
    );
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
        payload: payload.into(),
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
        normalized: None,
        scope: Some(account_scope(account)),
        cache_cost: None,
        request: None,
    }
}

fn account_scope(account: &str) -> UsageScope {
    let metered = account == "billing-key";
    UsageScope {
        provider: if metered { "openai" } else { "anthropic-oauth" }.into(),
        model: if metered {
            "gpt-5.2"
        } else {
            "claude-sonnet-4-5"
        }
        .into(),
        account_scope: Some(CredentialAlias::new(account)),
        auth_scope: if metered {
            "api_key"
        } else {
            "oauth_subscription"
        }
        .into(),
        api_family: None,
        effort: None,
        speed: None,
        cache_epoch: "usage-report-fixture".into(),
        stable_prefix_tokens: 0,
        cache_boundaries: None,
        request_kind: UsageRequestKind::MainTurn,
        run: None,
        agent: None,
        prefix_digests: None,
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
            "fs_edit",
            serde_json::json!({"path":"src/lib.rs","edits":[{"old":"a\n","new":"b\nc\nd\n"}]}),
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
                    name: "fs_edit".into(),
                    args: serde_json::json!({"edits":[{"old":"zzz\n","new":"qqq\n"}]}),
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
                    normalized: None,
                    scope: Some(account_scope("personal-max")),
                    cache_cost: None,
                },
                AccountUsage {
                    account: CredentialAlias::new("billing-key"),
                    input: 2_000_000,
                    output: 200_000,
                    reasoning: 0,
                    cached: 0,
                    source: UsageSource::ProviderReported,
                    normalized: None,
                    scope: Some(account_scope("billing-key")),
                    cache_cost: None,
                },
            ],
            normalized: None,
            scope: None,
            cache_cost: None,
            request: None,
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
            normalized: None,
            scope: None,
            cache_cost: None,
            request: None,
        }),
    ));

    let stats = folder.finish();
    assert_eq!(stats.last_committed_at_ms, 9_000);
    assert_eq!(stats.lines_added, 5, "3 patched + 2 written");
    assert_eq!(stats.lines_removed, 1, "failed receipts never count");

    let max = &stats.tokens[&CredentialAlias::new("personal-max")];
    // Head lane last snapshot (1M/100k) + subagent lane (200k/10k) +
    // rotation subtotal (1M/100k).
    assert_eq!(max.input, 2_200_000);
    assert_eq!(max.output, 210_000);
    // Subscription usage is intentionally token-only: API-list pricing is
    // not presented as a charge against a flat plan.
    assert_eq!(max.est_cost_usd, None);
    let billing = &stats.tokens[&CredentialAlias::new("billing-key")];
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

/// CM1 session fold law: changing account/auth attribution during rotation
/// does not fork a cumulative request lane, while compaction is a distinct
/// additive lane under the exact cache-domain key.
#[test]
fn cm1_session_folder_uses_latest_full_cache_lane_snapshot() {
    let account = CredentialAlias::new("billing-key");
    let usage = |logical: u64,
                 uncached: u64,
                 read: u64,
                 request_kind: UsageRequestKind,
                 scope_account: &str,
                 auth: &str| Usage {
        input: logical,
        output: 10,
        reasoning: 0,
        cached: read,
        source: UsageSource::ProviderReported,
        account: Some(account.clone()),
        accounts: Vec::new(),
        normalized: Some(NormalizedUsage {
            logical_input: logical,
            uncached_input: uncached,
            cache_read_input: read,
            billed_output: 10,
            cache_status: CacheStatAvailability::Present,
            cache_telemetry_input: logical,
            ..NormalizedUsage::default()
        }),
        scope: Some(UsageScope {
            provider: "openai".into(),
            model: "gpt-5".into(),
            account_scope: Some(CredentialAlias::new(scope_account)),
            auth_scope: auth.into(),
            api_family: None,
            effort: None,
            speed: None,
            cache_epoch: "epoch-a".into(),
            stable_prefix_tokens: 0,
            cache_boundaries: None,
            request_kind,
            run: Some(RunId::new("run-cache")),
            agent: None,
            prefix_digests: None,
        }),
        cache_cost: Some(CacheCostEstimate {
            input_with_cache_usd: logical as f64 / 1_000_000.0,
            input_without_cache_usd: logical as f64 / 500_000.0,
            estimated_savings_usd: logical as f64 / 1_000_000.0,
            explicit_storage_usd: 0.0,
        }),
        request: None,
    };

    let mut folder = SessionFolder::new("gpt-5");
    folder.push(&envelope(
        1,
        Some("run-cache"),
        None,
        1,
        usage_payload(usage(
            500,
            100,
            400,
            UsageRequestKind::MainTurn,
            "first",
            "api_key",
        )),
    ));
    folder.push(&envelope(
        2,
        Some("run-cache"),
        None,
        2,
        usage_payload(usage(
            1_000,
            200,
            800,
            UsageRequestKind::MainTurn,
            "rotated",
            "oauth",
        )),
    ));
    folder.push(&envelope(
        3,
        Some("run-cache"),
        None,
        3,
        usage_payload(usage(
            100,
            100,
            0,
            UsageRequestKind::Compaction,
            "rotated",
            "oauth",
        )),
    ));

    let stats = folder.finish();
    let totals = &stats.tokens[&account];
    assert_eq!(totals.input, 1_100, "first main snapshot was replaced");
    assert_eq!(totals.cache.logical_input_tokens, 1_100);
    assert_eq!(totals.cache.uncached_input_tokens, 300);
    assert_eq!(totals.cache.cache_read_tokens, 800);
    assert_eq!(totals.cache.breakdowns.len(), 2);
    assert!(totals.cache.breakdowns.iter().any(|breakdown| {
        breakdown.request_kind == UsageRequestKind::Compaction
            && breakdown.logical_input_tokens == 100
    }));
}

#[test]
fn cache_requests_fold_by_ordinal_with_response_local_counters() {
    let account = CredentialAlias::new("billing-key");
    let request_usage = |ordinal: u64, logical: u64, output: u64| RequestUsage {
        ordinal,
        input: logical,
        output,
        reasoning: None,
        cached: Some(0),
        source: UsageSource::ProviderReported,
        account: Some(account.clone()),
        normalized: Some(NormalizedUsage {
            logical_input: logical,
            uncached_input: logical,
            cache_read_input: 0,
            billed_output: output,
            cache_status: CacheStatAvailability::Present,
            cache_telemetry_input: logical,
            ..NormalizedUsage::default()
        }),
        cache_cost: None,
        cache: None,
    };
    let cumulative = |logical: u64, output: u64, request: RequestUsage| Usage {
        input: logical,
        output,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: Some(account.clone()),
        accounts: Vec::new(),
        normalized: Some(NormalizedUsage {
            logical_input: logical,
            uncached_input: logical,
            cache_read_input: 0,
            billed_output: output,
            cache_status: CacheStatAvailability::Present,
            cache_telemetry_input: logical,
            ..NormalizedUsage::default()
        }),
        scope: Some(UsageScope {
            provider: "openai".into(),
            model: "gpt-5.6-terra".into(),
            account_scope: Some(account.clone()),
            auth_scope: "api_key".into(),
            api_family: None,
            effort: None,
            speed: None,
            cache_epoch: "epoch-request-local".into(),
            stable_prefix_tokens: 4_096,
            cache_boundaries: None,
            request_kind: UsageRequestKind::MainTurn,
            run: Some(RunId::new("run-request-local")),
            agent: None,
            prefix_digests: None,
        }),
        cache_cost: None,
        request: Some(request),
    };

    let mut folder = SessionFolder::new("gpt-5.6-terra");
    folder.push(&envelope(
        1,
        Some("run-request-local"),
        None,
        1,
        usage_payload(cumulative(100, 10, request_usage(1, 100, 10))),
    ));
    folder.push(&envelope(
        2,
        Some("run-request-local"),
        None,
        2,
        usage_payload(cumulative(300, 30, request_usage(2, 200, 20))),
    ));
    let mut unavailable_request = request_usage(3, 50, 5);
    unavailable_request.normalized = None;
    let mut unavailable_cumulative = cumulative(350, 35, unavailable_request);
    unavailable_cumulative.normalized = None;
    folder.push(&envelope(
        3,
        Some("run-request-local"),
        None,
        3,
        usage_payload(unavailable_cumulative),
    ));

    let stats = folder.finish();
    let totals = &stats.tokens[&account];
    assert_eq!(totals.input, 350, "request-local input is summed once");
    assert_eq!(totals.output, 35, "request-local output is summed once");
    assert_eq!(totals.cache.logical_input_tokens, 300);
    let requests = &totals.cache.requests;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].request.ordinal, 1);
    assert_eq!(requests[0].request.input, 100);
    assert_eq!(requests[1].request.ordinal, 2);
    assert_eq!(requests[1].request.input, 200);
    assert_eq!(requests[2].request.ordinal, 3);
    assert_eq!(requests[2].request.input, 50);
    assert_eq!(requests[2].request.normalized, None);
}

fn metrics_usage(logical: u64, output: u64, model: &str, request_kind: UsageRequestKind) -> Usage {
    Usage {
        input: logical,
        output,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: Some(CredentialAlias::new("billing-key")),
        accounts: Vec::new(),
        normalized: Some(NormalizedUsage {
            logical_input: logical,
            uncached_input: logical,
            billed_output: output,
            cache_status: CacheStatAvailability::Present,
            cache_telemetry_input: logical,
            ..NormalizedUsage::default()
        }),
        scope: Some(UsageScope {
            provider: "openai".into(),
            model: model.into(),
            account_scope: Some(CredentialAlias::new("billing-key")),
            auth_scope: "api_key".into(),
            api_family: None,
            effort: None,
            speed: None,
            cache_epoch: "metrics-epoch".into(),
            stable_prefix_tokens: 0,
            cache_boundaries: None,
            request_kind,
            run: Some(RunId::new("metrics-run")),
            agent: None,
            prefix_digests: None,
        }),
        cache_cost: None,
        request: None,
    }
}

fn cache_reread_metrics_usage(run: &str, logical: u64, output: u64, cache_read: u64) -> Usage {
    let mut usage = metrics_usage(logical, output, "gpt-5.2", UsageRequestKind::MainTurn);
    usage.cached = cache_read;
    let normalized = usage.normalized.as_mut().expect("normalized usage");
    normalized.uncached_input = logical.saturating_sub(cache_read);
    normalized.cache_read_input = cache_read;
    usage.scope.as_mut().expect("usage scope").run = Some(RunId::new(run));
    usage
}

fn cache_reread_metric_snapshot(calls: &[(&str, u64, u64, u64)]) -> AgentUsageMetrics {
    let mut folder = SessionFolder::new("gpt-5.2");
    for (index, &(run, logical, output, cache_read)) in calls.iter().enumerate() {
        let seq = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        folder.push(&envelope(
            seq,
            Some(run),
            Some("agent-cache"),
            seq,
            usage_payload(cache_reread_metrics_usage(run, logical, output, cache_read)),
        ));
    }
    folder
        .agent_snapshot(
            &SessionId::new("s-usage"),
            Some(&AgentId::new("agent-cache")),
            u64::try_from(calls.len()).unwrap_or(u64::MAX),
        )
        .and_then(|snapshot| snapshot.usage)
        .expect("cache re-read usage")
}

fn cancelled_cache_reread_metric_snapshot(requests: &[(u64, u64, u64)]) -> AgentUsageMetrics {
    let mut folder = SessionFolder::new("gpt-5.2");
    let mut cumulative_logical = 0_u64;
    let mut cumulative_output = 0_u64;
    let mut cumulative_cache_read = 0_u64;
    for (index, &(logical, output, cache_read)) in requests.iter().enumerate() {
        let ordinal = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        let mut usage =
            cache_reread_metrics_usage("run-cancelled-cache", logical, output, cache_read);
        usage.request = Some(RequestUsage {
            ordinal,
            input: usage.input,
            output: usage.output,
            reasoning: None,
            cached: Some(usage.cached),
            source: usage.source,
            account: usage.account.clone(),
            normalized: usage.normalized.clone(),
            cache_cost: usage.cache_cost,
            cache: None,
        });

        // The actor journals a cumulative turn snapshot while `request`
        // retains this response's local counters. Reproduce that exact shape:
        // the folder must split completed requests before a later cancellation.
        cumulative_logical = cumulative_logical.saturating_add(logical);
        cumulative_output = cumulative_output.saturating_add(output);
        cumulative_cache_read = cumulative_cache_read.saturating_add(cache_read);
        usage.input = cumulative_logical;
        usage.output = cumulative_output;
        usage.cached = cumulative_cache_read;
        usage.normalized = Some(NormalizedUsage {
            logical_input: cumulative_logical,
            uncached_input: cumulative_logical.saturating_sub(cumulative_cache_read),
            billed_output: cumulative_output,
            cache_read_input: cumulative_cache_read,
            cache_status: CacheStatAvailability::Present,
            cache_telemetry_input: cumulative_logical,
            ..NormalizedUsage::default()
        });
        folder.push(&envelope(
            ordinal,
            Some("run-cancelled-cache"),
            Some("agent-cache"),
            ordinal,
            usage_payload(usage),
        ));
    }
    let terminal_seq = u64::try_from(requests.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    folder.push(&envelope(
        terminal_seq,
        Some("run-cancelled-cache"),
        None,
        terminal_seq,
        serde_json::to_value(EventPayload::RunState(
            haider_protocol::state::RunState::Cancelled,
        ))
        .expect("cancelled state"),
    ));
    let snapshot = folder
        .agent_snapshot(
            &SessionId::new("s-usage"),
            Some(&AgentId::new("agent-cache")),
            terminal_seq,
        )
        .expect("cancelled cache snapshot");
    assert!(!snapshot.live);
    snapshot.usage.expect("cancelled cache usage")
}

/// MUTATION CHECK: replace `logical.min(accumulator.prev_prefix_tokens)` with
/// `logical`; the genuinely single completed request reports `Some(0)` instead
/// of the required absence.
#[test]
fn cache_reread_metric_single_completed_request_then_cancelled_is_none() {
    let usage = cancelled_cache_reread_metric_snapshot(&[(1_000, 100, 0)]);
    assert_eq!(usage.cache_reread_hit_basis_points, None);
}

/// MUTATION CHECK: remove `request_ordinal` from `UsageChunkKey`; the two
/// request-local rows collapse back to one lane snapshot and the metric becomes
/// `None` after cancellation.
#[test]
fn cache_reread_metric_two_completed_requests_then_cancelled_is_present() {
    let usage = cancelled_cache_reread_metric_snapshot(&[(1_000, 100, 0), (1_100, 100, 900)]);
    assert_eq!(usage.cache_reread_hit_basis_points, Some(8_181));
}

/// MUTATION CHECK: remove `chronological_chunks.sort_by_key`; key-order folding drops the expected 10_000 to 5_000.
#[test]
fn cache_reread_metric_steady_state_excludes_unavoidable_first_input() {
    let usage = cache_reread_metric_snapshot(&[
        ("run-z-first", 1_000, 0, 0),
        ("run-a-second", 1_000, 0, 1_000),
        ("run-m-third", 1_000, 0, 1_000),
    ]);
    assert_eq!(usage.cache_reread_hit_basis_points, Some(10_000));
    assert_eq!(usage.cache_hit_basis_points, Some(6_666));
}

/// MUTATION CHECK: replace `logical.min(accumulator.prev_prefix_tokens)` with `accumulator.prev_prefix_tokens`; compaction returns 4_654 instead of 10_000.
#[test]
fn cache_reread_metric_compaction_clamps_cacheable_window() {
    let usage = cache_reread_metric_snapshot(&[
        ("run-first", 1_000, 100, 0),
        ("run-compacted", 400, 50, 512),
    ]);
    assert_eq!(usage.cache_reread_hit_basis_points, Some(10_000));
}

/// MUTATION CHECK: replace `read.min(cacheable)` with `cacheable`; real eviction reports 10_000 instead of 6_278.
#[test]
fn cache_reread_metric_real_eviction_drives_ratio_down() {
    let usage = cache_reread_metric_snapshot(&[
        ("run-first", 10_000, 1_000, 0),
        ("run-steady", 11_000, 1_000, 11_000),
        ("run-evicted", 11_819, 1_000, 3_328),
    ]);
    assert_eq!(usage.cache_reread_hit_basis_points, Some(6_278));
    assert!(usage.cache_reread_hit_basis_points < Some(7_000));
}

/// LAW mv1 — Started+Completed for one durable ToolCall is one attempt;
/// Delta and CommandExecution never count, while a Completed-only replay is
/// the one allowed fallback.
///
/// MUTATION CHECK (executed): salt the uniqueness key with envelope sequence;
/// Started+Completed for one item became two attempts (expected 2, got 3).
#[test]
fn mv1_tool_count_uniqueness_started_completed_delta_and_command() {
    use haider_protocol::ids::ItemId;
    use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};

    let tool = |status| TurnItem::ToolCall {
        call_id: "call-1".into(),
        name: "fs_read".into(),
        args: serde_json::json!({}),
        status,
    };
    let command = TurnItem::CommandExecution {
        call_id: "cmd-1".into(),
        command: "pwd".into(),
        status: ToolStatus::InProgress,
        exit_code: None,
    };
    let events = [
        ItemEvent::Started {
            item_id: ItemId::new("tool-1"),
            item: tool(ToolStatus::InProgress),
        },
        ItemEvent::Delta {
            item_id: ItemId::new("tool-1"),
            delta: ItemDelta::ToolArgs {
                fragment: "{}".into(),
            },
        },
        ItemEvent::Completed {
            item_id: ItemId::new("tool-1"),
            item: tool(ToolStatus::Completed),
        },
        ItemEvent::Started {
            item_id: ItemId::new("cmd-1"),
            item: command.clone(),
        },
        ItemEvent::Completed {
            item_id: ItemId::new("cmd-1"),
            item: command,
        },
        ItemEvent::Completed {
            item_id: ItemId::new("tool-2"),
            item: tool(ToolStatus::Failed),
        },
    ];
    let mut folder = SessionFolder::new("gpt-5.2");
    for (index, event) in events.into_iter().enumerate() {
        folder.push(&envelope(
            index as u64 + 1,
            Some("metrics-run"),
            Some("agent-a"),
            index as u64 + 1,
            serde_json::to_value(EventPayload::Item(event)).expect("item"),
        ));
    }
    let snapshot = folder
        .agent_snapshot(
            &SessionId::new("s-usage"),
            Some(&AgentId::new("agent-a")),
            6,
        )
        .expect("snapshot");
    assert_eq!(snapshot.tool_attempts, 2);
}

/// LAW mv2 — later cumulative usage replaces the same full lane key, while a
/// distinct request lane remains additive.
///
/// MUTATION CHECK (executed): sum both same-key snapshots; 250 became 350.
#[test]
fn mv2_latest_snapshot_replaces_same_key_and_distinct_keys_sum() {
    let mut folder = SessionFolder::new("gpt-5.2");
    for (seq, logical, lane) in [
        (1, 100, UsageRequestKind::MainTurn),
        (2, 200, UsageRequestKind::MainTurn),
        (3, 50, UsageRequestKind::Compaction),
    ] {
        folder.push(&envelope(
            seq,
            Some("metrics-run"),
            Some("agent-a"),
            seq,
            usage_payload(metrics_usage(logical, 10, "gpt-5.2", lane)),
        ));
    }
    let usage = folder
        .agent_snapshot(
            &SessionId::new("s-usage"),
            Some(&AgentId::new("agent-a")),
            3,
        )
        .and_then(|snapshot| snapshot.usage)
        .expect("usage");
    assert_eq!(usage.logical_input_tokens, 250);
    assert_eq!(usage.billed_output_tokens, 20);
    assert_eq!(usage.breakdowns.len(), 2);
}

/// LAW mv3 — agent identity remains queryable after the latest-snapshot fold,
/// while the pre-existing account report still receives the exact combined
/// totals.
///
/// MUTATION CHECK (executed): erase the agent key; one agent snapshot absorbed
/// the other and this separate-query assertion failed.
#[test]
fn mv3_agent_dimension_preserved_and_account_report_unchanged() {
    let mut folder = SessionFolder::new("gpt-5.2");
    for (seq, agent, logical) in [(1, "agent-a", 100), (2, "agent-b", 200)] {
        let mut usage = metrics_usage(logical, 10, "gpt-5.2", UsageRequestKind::MainTurn);
        usage.cached = logical / 2;
        let normalized = usage.normalized.as_mut().expect("normalized");
        normalized.uncached_input = logical - usage.cached;
        normalized.cache_read_input = usage.cached;
        folder.push(&envelope(
            seq,
            Some("metrics-run"),
            Some(agent),
            seq,
            usage_payload(usage),
        ));
    }
    for (agent, expected) in [("agent-a", 100), ("agent-b", 200)] {
        let usage = folder
            .agent_snapshot(&SessionId::new("s-usage"), Some(&AgentId::new(agent)), 2)
            .and_then(|snapshot| snapshot.usage)
            .expect("agent usage");
        assert_eq!(usage.logical_input_tokens, expected);
    }
    let stats = folder.finish();
    assert_eq!(
        stats.tokens[&CredentialAlias::new("billing-key")].input,
        300,
        "account aggregation remains the unchanged sum of both agents"
    );
    let account = &stats.tokens[&CredentialAlias::new("billing-key")];
    assert_eq!(account.output, 20);
    assert_eq!(account.cached, 150);
    assert_eq!(account.cache.logical_input_tokens, 300);
    assert_eq!(account.cache.uncached_input_tokens, 150);
    assert_eq!(account.cache.cache_read_tokens, 150);
    assert!(account.est_cost_usd.is_some_and(|cost| cost > 0.0));
    assert_eq!(account.api_equivalent_est_cost_usd, account.est_cost_usd);

    // A root session remains the `None` bucket after a child transcript
    // projection arrives carrying the child's agent id on the parent run.
    let mut root = SessionFolder::new("gpt-5.2");
    root.push(&envelope(
        1,
        Some("root-run"),
        None,
        1,
        usage_payload(metrics_usage(75, 5, "gpt-5.2", UsageRequestKind::MainTurn)),
    ));
    root.push(&envelope(
        2,
        Some("root-run"),
        Some("agent-child"),
        2,
        serde_json::to_value(EventPayload::UserMessage {
            text: "mirrored child prompt".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Steer,
        })
        .expect("prompt"),
    ));
    let root_snapshot = root
        .primary_agent_snapshot(&SessionId::new("root-session"), 2)
        .expect("root metrics");
    assert_eq!(root_snapshot.agent, None);
    assert_eq!(
        root_snapshot
            .usage
            .expect("root usage")
            .logical_input_tokens,
        75
    );

    // Child sessions begin with an unscoped SessionCreated row; roster
    // snapshots must still select the first actual delegated agent.
    let mut child = SessionFolder::new("gpt-5.2");
    child.push(&envelope(
        1,
        None,
        None,
        1,
        serde_json::to_value(EventPayload::RunState(
            haider_protocol::state::RunState::Thinking,
        ))
        .expect("state"),
    ));
    child.push(&envelope(
        2,
        Some("metrics-run"),
        Some("agent-a"),
        2,
        usage_payload(metrics_usage(
            100,
            10,
            "gpt-5.2",
            UsageRequestKind::DelegatedAgent,
        )),
    ));
    assert_eq!(
        child
            .primary_agent_snapshot(&SessionId::new("child-session"), 2)
            .and_then(|snapshot| snapshot.agent),
        Some(AgentId::new("agent-a"))
    );
}

/// LAW mv4 — no usage fact remains `None`; one unknown-priced lane poisons
/// the complete aggregate instead of becoming a partial or `$0` estimate.
///
/// MUTATION CHECK (executed): omit the unpriced lane's
/// `all_lanes_priced = false`; the aggregate exposed a known partial.
#[test]
fn mv4_no_usage_is_none_and_unknown_or_mixed_price_is_unpriced() {
    let mut empty = SessionFolder::new("gpt-5.2");
    empty.push(&envelope(
        1,
        Some("metrics-run"),
        Some("agent-a"),
        1,
        serde_json::to_value(EventPayload::RunState(
            haider_protocol::state::RunState::Thinking,
        ))
        .expect("state"),
    ));
    assert!(
        empty
            .agent_snapshot(
                &SessionId::new("s-usage"),
                Some(&AgentId::new("agent-a")),
                1,
            )
            .expect("snapshot")
            .usage
            .is_none()
    );

    let mut mixed = SessionFolder::new("gpt-5.2");
    for (seq, model, lane) in [
        (1, "gpt-5.2", UsageRequestKind::MainTurn),
        (2, "unknown-model", UsageRequestKind::Compaction),
    ] {
        mixed.push(&envelope(
            seq,
            Some("metrics-run"),
            Some("agent-a"),
            seq,
            usage_payload(metrics_usage(100, 10, model, lane)),
        ));
    }
    let usage = mixed
        .agent_snapshot(
            &SessionId::new("s-usage"),
            Some(&AgentId::new("agent-a")),
            2,
        )
        .and_then(|snapshot| snapshot.usage)
        .expect("usage");
    assert!(!usage.all_lanes_priced);
    assert_eq!(usage.metered_cost_microusd, None);
    assert_eq!(usage.api_equivalent_cost_microusd, None);
}

/// Q4 law: a scoped custom provider cannot inherit built-in pricing from a
/// colliding model slug, including a stale model-only cache estimate already
/// persisted by an older writer.
///
/// MUTATION CHECK: use the compatibility model-only estimators for scoped
/// rows, or trust `Usage.cache_cost` before qualifying its provider; one of
/// the cost assertions becomes present.
#[test]
fn custom_provider_model_collision_remains_unpriced_in_live_and_account_folds() {
    let mut usage = metrics_usage(1_000_000, 100_000, "gpt-5", UsageRequestKind::MainTurn);
    usage.cached = 800_000;
    let normalized = usage.normalized.as_mut().expect("normalized usage");
    normalized.uncached_input = 200_000;
    normalized.cache_read_input = 800_000;
    usage.scope.as_mut().expect("usage scope").provider = "router-lab".into();
    usage.cache_cost = Some(CacheCostEstimate {
        input_with_cache_usd: 0.1,
        input_without_cache_usd: 1.0,
        estimated_savings_usd: 0.9,
        explicit_storage_usd: 0.0,
    });

    let mut folder = SessionFolder::new("gpt-5");
    folder.push(&envelope(
        1,
        Some("metrics-run"),
        Some("agent-a"),
        1,
        usage_payload(usage),
    ));
    let live = folder
        .agent_snapshot(
            &SessionId::new("s-custom-price"),
            Some(&AgentId::new("agent-a")),
            1,
        )
        .and_then(|snapshot| snapshot.usage)
        .expect("live custom usage");
    assert!(!live.all_lanes_priced);
    assert_eq!(live.metered_cost_microusd, None);
    assert_eq!(live.api_equivalent_cost_microusd, None);

    let stats = folder.finish();
    let account = &stats.tokens[&CredentialAlias::new("billing-key")];
    assert_eq!(account.est_cost_usd, None);
    assert_eq!(account.api_equivalent_est_cost_usd, None);
    assert_eq!(account.cache.input_with_cache_usd, None);
    assert_eq!(account.cache.api_equivalent_input_with_cache_usd, None);
}

/// LAW mv6 — a terminal committed state forces a settled snapshot through
/// that exact sequence, while Error/Cancelled retain partial work.
///
/// MUTATION CHECK (executed): clear usage chunks on terminal; both terminal
/// cases lost their 110 normalized tokens and the assertions failed.
#[test]
fn mv6_terminal_snapshot_drops_live_and_retains_error_cancel_partials() {
    for terminal in [
        haider_protocol::state::RunState::Errored,
        haider_protocol::state::RunState::Cancelled,
    ] {
        let mut folder = SessionFolder::new("gpt-5.2");
        folder.push(&envelope(
            1,
            Some("metrics-run"),
            Some("agent-a"),
            100,
            usage_payload(metrics_usage(
                100,
                10,
                "gpt-5.2",
                UsageRequestKind::MainTurn,
            )),
        ));
        folder.push(&envelope(
            2,
            Some("metrics-run"),
            None,
            700,
            serde_json::to_value(EventPayload::RunState(terminal)).expect("terminal"),
        ));
        let snapshot = folder
            .agent_snapshot(
                &SessionId::new("s-usage"),
                Some(&AgentId::new("agent-a")),
                2,
            )
            .expect("snapshot");
        assert!(!snapshot.live);
        assert_eq!(snapshot.terminal_at_ms, Some(700));
        let usage = snapshot.usage.expect("partials retained");
        assert_eq!(
            usage
                .logical_input_tokens
                .saturating_add(usage.billed_output_tokens),
            110
        );
    }
}

/// LAW (meter_routing_is_flavor_and_provider_strict): only the three OAuth
/// subscriptions with supported server meters route to one; Grok's quota
/// lane, the SAME provider names under an API key, and unknown providers are
/// meterless.
#[test]
fn meter_routing_is_flavor_and_provider_strict() {
    use haider_provider::UsageMeterEndpoint;
    let cases = [
        ("openai-oauth", Some(UsageMeterEndpoint::OpenAiOauth)),
        ("anthropic-oauth", Some(UsageMeterEndpoint::AnthropicOauth)),
        ("kimi-oauth", Some(UsageMeterEndpoint::KimiOauth)),
        // Owner ask (plan-usage UI): the Grok proxy's billing surface IS the
        // subscription meter — weekly credit percent, on-demand pool, plan
        // tier — so grok-oauth routes to the fourth meter like the other
        // CLI lanes. Local usage stays zero-cost under subscription pricing.
        ("grok-oauth", Some(UsageMeterEndpoint::GrokOauth)),
        ("openai", None),
        ("anthropic", None),
        ("gemini", None),
        ("deepseek", None),
        ("xai", None),
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

/// LAW (970 layer B): a LINKED Grok CLI row carries no stored tier — the
/// Grok store has no such field and the access token is never decoded for
/// one. The tier reaches the report through the existing live `grok-oauth`
/// meter, with no code path specific to linked rows.
///
/// MUTATION CHECK: stop overwriting `plan` from the meter reading, or route
/// a linked `grok-oauth` descriptor away from `UsageMeterEndpoint::GrokOauth`.
/// Expected runtime failure: `SuperGrok` never arrives and the weekly window
/// disappears.
#[tokio::test]
async fn linked_grok_source_row_reports_its_tier_from_the_live_meter() {
    use haider_provider::UsageMeterEndpoint;
    let body = br#"{
        "subscriptionTier": "SuperGrok",
        "config": {
            "creditUsagePercent": 42.5,
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-07-03T04:01:09.238389+00:00",
                "end": "2026-07-10T04:01:09.238389+00:00"
            }
        }
    }"#
    .to_vec();
    let http = Arc::new(StubHttp::new([(
        UsageMeterEndpoint::GrokOauth.url().to_owned(),
        (200, body),
    )]));

    // Exactly the descriptor source reconciliation writes for a linked
    // `~/.grok` root: OAuth, `grok-oauth`, an email identity, and NO plan.
    let mut linked = descriptor("grok-oauth", "grok-2f8c1d0a4b6e", AuthMethod::OAuth);
    linked.identity = "pilot@example.test".into();
    linked.account_identity = Some(AccountIdentity {
        email: Some("pilot@example.test".into()),
        display_name: None,
        account_id: Some("xai:user-linked".into()),
        plan: None,
        issuer: Some("https://auth.x.ai".into()),
        captured_at: 1,
        verified: false,
    });
    assert_eq!(meter_for(&linked), Some(UsageMeterEndpoint::GrokOauth));

    let service = service_with_clock(
        vec![linked],
        Some(Arc::new(StubTokens {
            bytes: b"linked-access-token".to_vec(),
        })),
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        Arc::new(AtomicU64::new(1_777_075_200_000)),
    );
    let (_root, store) = empty_store().await;
    let report = service.report(&store).await.expect("report");

    assert_eq!(report.accounts.len(), 1);
    assert_eq!(
        report.accounts[0].plan.as_deref(),
        Some("SuperGrok"),
        "the tier renders from the meter reading, never from the store"
    );
    let AccountMeterStateV1::Metered { windows } = &report.accounts[0].meter else {
        panic!("a linked Grok row is metered like any other grok-oauth account");
    };
    assert_eq!(windows[0].window, "weekly");
    assert_eq!(http.calls(), 1);
    assert_eq!(
        http.requests()[0].0,
        UsageMeterEndpoint::GrokOauth.url(),
        "no linked-row-specific endpoint exists"
    );
}

// ---------------------------------------------------------------------------
// v0.0.970: honest absence for the supervised Antigravity agent
// ---------------------------------------------------------------------------

/// Antigravity exposes NO structured quota over ACP (verified twice against
/// the published v1 schema and the live 1.1.1 binary), so the account reports
/// typed unavailability — never zero usage, never a synthesized reset, and
/// never a probe to an endpoint that does not exist.
///
/// MUTATION CHECK: answer `AccountMeterStateV1::LocalOnly` for this provider.
/// Expected runtime failure: the state assertion below, because `LocalOnly`
/// asserts Haider's own counters are the truth for an agent that reports no
/// tokens at all — which renders as zero usage.
#[tokio::test]
async fn a_supervised_agent_account_reports_unavailable_not_zero_usage() {
    let http = Arc::new(StubHttp::new([]));
    let service = service_with_clock(
        vec![descriptor(
            "google-antigravity",
            "google-work",
            AuthMethod::OAuth,
        )],
        None,
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        Arc::new(AtomicU64::new(1_000_000)),
    );
    let (_root, store) = empty_store().await;
    let report = service.report(&store).await.expect("report");

    let account = &report.accounts[0];
    assert_eq!(
        account.meter,
        AccountMeterStateV1::Unavailable {
            reason: "provider_exposes_no_quota".into()
        }
    );
    assert!(
        !matches!(account.meter, AccountMeterStateV1::Metered { .. }),
        "absent data is never rendered as a metered window"
    );
    assert_eq!(
        account.plan, None,
        "no plan is claimed: the agent never reports one, and a model in the \
         catalog is not evidence of Pro or Ultra"
    );
    assert_eq!(
        http.calls(),
        0,
        "there is no quota endpoint to probe, so none is probed"
    );
    // The local fold contributes nothing, which is exactly why `LocalOnly`
    // would have been a lie here.
    assert_eq!(account.local.input_tokens, 0);
    assert_eq!(account.local.output_tokens, 0);

    // `meter_for` still declines the account: no HTTP meter exists for it.
    assert!(
        meter_for(&descriptor(
            "google-antigravity",
            "google-work",
            AuthMethod::OAuth
        ))
        .is_none()
    );
}

/// A provider-supplied retry instant IS published, exactly as given, and
/// nothing else ever becomes a timestamp.
///
/// MUTATION CHECK: derive `resets_at_ms` from Google's documented five-hour
/// cadence (`now + 5h`) instead of the descriptor's instant. Expected runtime
/// failure: the exact-instant assertion, since the clock is 1_000_000 and the
/// published window would move.
#[tokio::test]
async fn only_a_provider_supplied_retry_instant_becomes_a_published_window() {
    let mut limited = descriptor("google-antigravity", "google-limited", AuthMethod::OAuth);
    // A provider-supplied instant that is NOT any multiple of five hours or a
    // week from the clock, so no cadence arithmetic could have produced it.
    limited.status = CredentialStatus::Limited {
        until_ms: 1_763_500_123_456,
    };
    let http = Arc::new(StubHttp::new([]));
    let service = service_with_clock(
        vec![limited],
        None,
        Arc::clone(&http) as Arc<dyn UsageMeterHttp>,
        Arc::new(AtomicU64::new(1_000_000)),
    );
    let (_root, store) = empty_store().await;
    let report = service.report(&store).await.expect("report");

    let AccountMeterStateV1::Metered { windows } = &report.accounts[0].meter else {
        panic!("a provider-supplied retry instant is publishable");
    };
    assert_eq!(
        windows.len(),
        1,
        "exactly the one window the provider named"
    );
    assert_eq!(windows[0].window, "retry_after");
    assert!(
        (windows[0].utilization - 1.0).abs() < f64::EPSILON,
        "a rate-limited account is fully consumed, which is what the upstream \
         error said"
    );
    assert_eq!(windows[0].resets_at_ms, Some(1_763_500_123_456));
    assert_eq!(windows[0].label, None);
    assert_eq!(report.accounts[0].plan, None);
    assert_eq!(http.calls(), 0);
}

/// Every durable credential failure keeps its OWN typed reason rather than
/// collapsing into the no-quota reason: a revoked Google login is a different,
/// more actionable state than "this protocol publishes no quota".
///
/// MUTATION CHECK: return the no-quota reason unconditionally from
/// `agent_owned_meter_state`. Expected runtime failure: each of the four
/// typed-reason assertions below.
#[test]
fn a_supervised_agent_keeps_the_typed_credential_reasons() {
    use super::{ACP_AGENT_NO_QUOTA_REASON, agent_owned_meter, agent_owned_meter_state};

    for (status, expected) in [
        (CredentialStatus::Ok, ACP_AGENT_NO_QUOTA_REASON),
        (CredentialStatus::Expired, "credential_expired"),
        (CredentialStatus::Revoked, "credential_revoked"),
        (
            CredentialStatus::NeedsAttention {
                reason: CredentialAttentionReason::SourceGone,
            },
            "credential_source_gone",
        ),
    ] {
        assert_eq!(
            agent_owned_meter_state(&status),
            AccountMeterStateV1::Unavailable {
                reason: expected.into()
            },
            "{status:?}"
        );
    }

    // The lane is scoped to the supervised agent's OAuth accounts only.
    assert!(agent_owned_meter(&descriptor(
        "google-antigravity",
        "google-work",
        AuthMethod::OAuth
    )));
    assert!(!agent_owned_meter(&descriptor(
        "gemini",
        "gemini-key",
        AuthMethod::ApiKey
    )));
    assert!(!agent_owned_meter(&descriptor(
        "google-antigravity",
        "impossible",
        AuthMethod::ApiKey
    )));
}
