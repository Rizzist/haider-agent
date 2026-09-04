//! Cross-provider `usage.report` assembly (U1).
//!
//! One service, two truths, honestly separated:
//! - **OAuth meters** — the three researched endpoints
//!   ([`haider_provider::UsageMeterEndpoint`]) fetched with broker-resolved
//!   access tokens through an injected [`UsageMeterHttp`], cached per
//!   account, and never polled more often than each endpoint's floor
//!   (codex ≥ 60 s, anthropic ≥ 180 s, kimi ≥ 300 s). A failed reading is a
//!   typed `unavailable` with a bounded reason — cached like a success so
//!   failures cannot turn into hammering, and never replaced by stale
//!   "good" numbers.
//! - **Haider Code plan meter** — no independent probe exists. The
//!   active-session `/v1/account` plan-status push deposits the held snapshot
//!   here and appends its exact-integer weekly allowance opportunistically.
//!   With no held status, the API-key account remains honest `local_only`.
//! - **Local accounting** — a journal fold per session: cumulative
//!   [`haider_protocol::EventPayload::Usage`] snapshots keyed by
//!   `(run, agent)` (last snapshot wins; summing them would double-count),
//!   `model_selected` facts tracked in sequence order so each usage chunk
//!   prices under the model active when it was reported, lines-of-code from
//!   COMPLETED `fs_write`/`fs_edit`/`fs_path` tool receipts, and session
//!   span from creation to the last committed event. Sessions, duration,
//!   and LOC attribute to the session's DOMINANT account (most tokens);
//!   token totals attribute exactly per usage events.
//!
//! Secrets discipline: tokens live only inside `SecretHandle` borrows for
//! the duration of one request; reasons, reports, and cache entries carry
//! no token or response bytes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_accounts::SecretHandle;
use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentMetricsSnapshot, AgentUsageBreakdown, AgentUsageMetrics};
use haider_protocol::credential::{
    AuthMethod, CredentialAttentionReason, CredentialDescriptor, CredentialStatus,
};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{AgentId, CredentialAlias, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{
    CacheCostEstimate, CacheStatAvailability, NormalizedUsage, RequestUsage, UsageRequestKind,
    UsageScope,
};
use haider_protocol::session::ModelSelected;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, CacheUsageBreakdownV1, CacheUsageRequestScopeV1,
    CacheUsageRequestV1, CacheUsageStatsV1, HaiderCodePlanSnapshotV1, LocalUsageStatsV1,
    UsageHistoryMeterSampleV1, UsageHistoryRangeDayV1, UsageReportV1, UsageWindowV1,
};
use haider_provider::{MeterReading, MeterUnavailable, UsageMeterEndpoint};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::accounts::AccountsSnapshot;
use crate::oauth::CredentialBroker;

/// Injected token seam: how the service obtains a bearer token for one
/// OAuth descriptor. Production is the SAME [`CredentialBroker`] that
/// provider construction uses (its refresh single-flight paces token work);
/// laws stub it with vault-minted handles.
#[async_trait::async_trait]
pub(crate) trait MeterTokenSource: Send + Sync {
    async fn bearer(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, MeterUnavailable>;
}

#[async_trait::async_trait]
impl MeterTokenSource for CredentialBroker {
    async fn bearer(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, MeterUnavailable> {
        if let Some(reason) = credential_meter_unavailable_reason(&descriptor.status) {
            return Err(MeterUnavailable::new(reason));
        }
        // The broker's own message may name vault/refresh internals; the
        // report carries only the bounded classification.
        self.resolve(descriptor).await.map_err(|error| {
            let reason = if error.code == ErrorCode::CredentialMissing {
                "credential_source_gone"
            } else {
                "credential_unavailable"
            };
            MeterUnavailable::new(reason)
        })
    }
}

/// Converts durable credential health into the stable, bounded meter reason
/// vocabulary. This prevents a revoked login or vanished linked source from
/// collapsing back to the screenshot's ambiguous `credential unavailable`.
fn credential_meter_unavailable_reason(status: &CredentialStatus) -> Option<&'static str> {
    match status {
        CredentialStatus::Ok | CredentialStatus::Limited { .. } => None,
        CredentialStatus::Expired => Some("credential_expired"),
        CredentialStatus::Revoked => Some("credential_revoked"),
        CredentialStatus::NeedsAttention { reason } => Some(match reason {
            CredentialAttentionReason::KeychainDenied => "credential_keychain_denied",
            CredentialAttentionReason::KeychainLocked => "credential_keychain_locked",
            CredentialAttentionReason::KeychainMissing | CredentialAttentionReason::SourceGone => {
                "credential_source_gone"
            }
            CredentialAttentionReason::KeychainUnavailable
            | CredentialAttentionReason::SourceUnreadable => "credential_source_unreadable",
            CredentialAttentionReason::OriginClientRequired => "credential_origin_client_required",
            CredentialAttentionReason::PolicyBlocked => "credential_policy_blocked",
        }),
    }
}

/// Injected HTTP seam for meter fetches: one authenticated GET, no retries
/// (the cache floor owns pacing). Every failure is a typed reason.
#[async_trait::async_trait]
pub(crate) trait UsageMeterHttp: Send + Sync {
    async fn get(
        &self,
        url: &str,
        bearer: &SecretHandle,
        extra_headers: &[(&'static str, &'static str)],
        chatgpt_account_id: Option<&str>,
    ) -> Result<(u16, Vec<u8>), MeterUnavailable>;
}

/// Production transport: pinned-policy reqwest client (no proxy, no
/// redirects, bounded timeouts). A client that cannot be built degrades to
/// typed unavailability, never a panic.
pub(crate) struct ReqwestUsageMeterHttp {
    transport: crate::http_transport::SharedHttpTransport,
}

impl ReqwestUsageMeterHttp {
    pub(crate) fn new() -> Self {
        Self {
            transport: crate::http_transport::SharedHttpTransport,
        }
    }
}

#[async_trait::async_trait]
impl UsageMeterHttp for ReqwestUsageMeterHttp {
    async fn get(
        &self,
        url: &str,
        bearer: &SecretHandle,
        extra_headers: &[(&'static str, &'static str)],
        chatgpt_account_id: Option<&str>,
    ) -> Result<(u16, Vec<u8>), MeterUnavailable> {
        let Some(client) = self.transport.client() else {
            return Err(MeterUnavailable::new("transport_unavailable"));
        };
        let token = std::str::from_utf8(bearer.expose_secret())
            .map_err(|_| MeterUnavailable::new("credential_not_utf8"))?;
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| MeterUnavailable::new("credential_not_header_safe"))?;
        authorization.set_sensitive(true);
        let mut request = client
            .get(url)
            .timeout(Duration::from_secs(15))
            .header(reqwest::header::AUTHORIZATION, authorization);
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }
        if let Some(account_id) = chatgpt_account_id {
            let mut account_id = reqwest::header::HeaderValue::from_str(account_id)
                .map_err(|_| MeterUnavailable::new("credential_account_id_not_header_safe"))?;
            account_id.set_sensitive(true);
            request = request.header(haider_provider::OPENAI_OAUTH_ACCOUNT_ID_HEADER, account_id);
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                MeterUnavailable::new("transport_timeout")
            } else {
                MeterUnavailable::new("transport_error")
            }
        })?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|_| MeterUnavailable::new("transport_error"))?;
        Ok((status, body.to_vec()))
    }
}

/// Stable meter reason for a supervised-agent provider whose protocol exposes
/// no quota at all.
///
/// Verified twice against the live 1.1.1 Google binary and against the
/// published ACP v1 schema (`docs/testing/v0.0.970/googleoauth_acp-wire-facts.md`): the
/// only usage-shaped update ACP defines is `usage_update`, which carries
/// CONTEXT-WINDOW OCCUPANCY (`used` tokens in a window of `size`), not
/// subscription quota — and Antigravity never sends even that. Quota failures
/// arrive as unstructured prose.
///
/// This is deliberately `Unavailable` rather than `LocalOnly`: `LocalOnly`
/// asserts that Haider's own counters are the truth, and for a supervised
/// agent that reports no token counts at all they would render as zero usage.
pub(crate) const ACP_AGENT_NO_QUOTA_REASON: &str = "provider_exposes_no_quota";

/// Window name published when — and only when — an upstream error supplied an
/// EXACT retry instant. It is the provider's own timestamp being reported, not
/// a cadence being turned into a guess.
pub(crate) const ACP_AGENT_RETRY_WINDOW: &str = "retry_after";

/// The meter state for an account whose provider publishes no structured
/// quota over its protocol.
///
/// - A durable credential problem keeps its own typed reason: a revoked login
///   is more actionable than "no quota surface", and reusing the existing
///   vocabulary keeps one classification per cause.
/// - `Limited { until_ms }` publishes that exact instant as a fully consumed
///   window. The timestamp is PROVIDER-SUPPLIED — it reached the descriptor
///   from an upstream error that named a retry instant — so publishing it is
///   reporting, not synthesis. Google's documented five-hour/weekly cadence is
///   never turned into a timestamp here.
/// - Everything else is honest absence. Nothing infers a plan (`Pro`/`Ultra`)
///   from which models happen to appear.
pub(crate) fn agent_owned_meter_state(status: &CredentialStatus) -> AccountMeterStateV1 {
    if let Some(reason) = credential_meter_unavailable_reason(status) {
        return AccountMeterStateV1::Unavailable {
            reason: reason.to_owned(),
        };
    }
    match status {
        CredentialStatus::Limited { until_ms } => AccountMeterStateV1::Metered {
            windows: vec![UsageWindowV1 {
                window: ACP_AGENT_RETRY_WINDOW.to_owned(),
                // The account is rate-limited right now: that is what the
                // upstream error said, not an extrapolation from usage.
                utilization: 1.0,
                resets_at_ms: Some(*until_ms),
                label: None,
            }],
        },
        CredentialStatus::Ok
        | CredentialStatus::Expired
        | CredentialStatus::Revoked
        | CredentialStatus::NeedsAttention { .. } => AccountMeterStateV1::Unavailable {
            reason: ACP_AGENT_NO_QUOTA_REASON.to_owned(),
        },
    }
}

/// True when the provider's credential and quota both live inside an external
/// agent process rather than in an HTTP meter Haider could poll.
pub(crate) fn agent_owned_meter(descriptor: &CredentialDescriptor) -> bool {
    descriptor.auth_method == AuthMethod::OAuth
        && descriptor.provider == haider_provider::GOOGLE_ANTIGRAVITY_PROVIDER_NAME
}

/// Which meter (if any) serves one descriptor. Only subscriptions with a
/// documented server meter are probed; Grok OAuth, every API-key/custom, and
/// unknown providers remain honest local-only accounting.
pub(crate) fn meter_for(descriptor: &CredentialDescriptor) -> Option<UsageMeterEndpoint> {
    if descriptor.auth_method != AuthMethod::OAuth {
        return None;
    }
    match descriptor.provider.as_str() {
        haider_provider::OPENAI_OAUTH_PROVIDER_NAME => Some(UsageMeterEndpoint::OpenAiOauth),
        haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME => Some(UsageMeterEndpoint::AnthropicOauth),
        haider_provider::KIMI_OAUTH_PROVIDER_NAME => Some(UsageMeterEndpoint::KimiOauth),
        haider_provider::GROK_OAUTH_PROVIDER_NAME => Some(UsageMeterEndpoint::GrokOauth),
        _ => None,
    }
}

/// Unverified display-only claims from an openai-oauth ACCESS token: the
/// account email and ChatGPT plan ride the JWT payload. Display data only —
/// nothing here authorizes anything, so no signature check is required
/// (mirrors the codex CLI's own `IdTokenInfo` flat decode).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenAiTokenIdentity {
    pub email: Option<String>,
    pub plan: Option<String>,
}

#[derive(Default, Deserialize)]
struct OpenAiTokenClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "https://api.openai.com/profile")]
    profile: Option<OpenAiProfileClaims>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    auth: Option<OpenAiAuthClaims>,
}

#[derive(Default, Deserialize)]
struct OpenAiProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Default, Deserialize)]
struct OpenAiAuthClaims {
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
}

/// Decodes the JWT payload segment without verification (display only):
/// top-level `email` (falling back to `https://api.openai.com/profile`) and
/// `https://api.openai.com/auth`.`chatgpt_plan_type`. Malformed input is
/// `None`-shaped, never an error.
pub(crate) fn openai_token_identity(token: &[u8]) -> OpenAiTokenIdentity {
    fn claims(token: &[u8]) -> Option<OpenAiTokenClaims> {
        let token = std::str::from_utf8(token).ok()?;
        let payload = token.split('.').nth(1)?;
        let mut decoded = Zeroizing::new(Vec::new());
        URL_SAFE_NO_PAD
            .decode_vec(payload.as_bytes(), &mut decoded)
            .ok()?;
        serde_json::from_slice(&decoded).ok()
    }
    let Some(claims) = claims(token) else {
        return OpenAiTokenIdentity::default();
    };
    OpenAiTokenIdentity {
        email: claims
            .email
            .or(claims.profile.and_then(|profile| profile.email)),
        plan: claims.auth.and_then(|auth| auth.chatgpt_plan_type),
    }
}

struct MeterCacheEntry {
    fetched_at_ms: u64,
    outcome: Result<MeterReading, MeterUnavailable>,
    /// True only for the call that performed the provider read. Cached report
    /// assembly must not append the same meter sample again.
    fresh: bool,
    /// openai-oauth enrichment captured at fetch time.
    token_identity: OpenAiTokenIdentity,
}

#[derive(Debug, Clone)]
struct HeldHaiderCodePlan {
    snapshot: HaiderCodePlanSnapshotV1,
}

/// The installed `usage.report` service (hub seam, like the worker manager).
pub(crate) struct UsageReportService {
    snapshot: AccountsSnapshot,
    tokens: Option<Arc<dyn MeterTokenSource>>,
    http: Arc<dyn UsageMeterHttp>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    cache: tokio::sync::Mutex<HashMap<(String, CredentialAlias), MeterCacheEntry>>,
    haider_code_plans: tokio::sync::Mutex<HashMap<CredentialAlias, HeldHaiderCodePlan>>,
}

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

impl UsageReportService {
    pub(crate) fn new(
        snapshot: AccountsSnapshot,
        tokens: Option<Arc<dyn MeterTokenSource>>,
        http: Arc<dyn UsageMeterHttp>,
    ) -> Self {
        Self::with_clock(snapshot, tokens, http, Box::new(system_now_ms))
    }

    pub(crate) fn with_clock(
        snapshot: AccountsSnapshot,
        tokens: Option<Arc<dyn MeterTokenSource>>,
        http: Arc<dyn UsageMeterHttp>,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            snapshot,
            tokens,
            http,
            clock,
            cache: tokio::sync::Mutex::new(HashMap::new()),
            haider_code_plans: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Holds one successfully received Haider Code plan status and freezes
    /// its weekly allowance into history when (and only when) the source JSON
    /// published an integer percent. This is fed by the existing plan poller;
    /// it never starts a second probe or changes that poller's cadence.
    pub(crate) async fn capture_haider_code_plan_status(
        &self,
        store: &haider_core::SqliteStoreHandle,
        account_alias: CredentialAlias,
        snapshot: HaiderCodePlanSnapshotV1,
        meter: crate::haider_code_plan::PlanMeterValues,
    ) -> Result<(), haider_protocol::error::HaiderError> {
        self.haider_code_plans.lock().await.insert(
            account_alias.clone(),
            HeldHaiderCodePlan {
                snapshot: snapshot.clone(),
            },
        );

        let Some(allowance) = snapshot.weekly_allowance.as_ref() else {
            return Ok(());
        };
        let Some(percent_remaining) = meter.weekly_percent_remaining else {
            // A float remains usable by the current report surface, but the
            // history reader must never reconstruct an integer from it.
            return Ok(());
        };
        let Some(used_percent) = 100_u64.checked_sub(percent_remaining) else {
            return Ok(());
        };
        // Exact provider-integer arithmetic: used basis points are
        // (100 - published whole percent remaining) * 100. No float enters
        // this derivation and no reader is permitted to reconstruct it.
        let Some(basis_points) = used_percent
            .checked_mul(100)
            .and_then(|value| u32::try_from(value).ok())
        else {
            return Ok(());
        };

        store
            .append_usage_meter_sample(UsageHistoryMeterSampleV1 {
                account: account_alias.as_str().to_owned(),
                window: "weekly".into(),
                basis_points,
                resets_at_ms: allowance.resets_at_ms,
                grace_until_ms: allowance.grace_until_ms,
                sampled_at_ms: (self.clock)(),
                plan: snapshot.plan.clone(),
                credits: meter.credits,
                hold: meter.hold,
                stale: snapshot.cached,
            })
            .await
    }

    pub(crate) async fn clear_haider_code_plan_status(&self, account_alias: &CredentialAlias) {
        self.haider_code_plans.lock().await.remove(account_alias);
    }

    fn descriptors(&self) -> Vec<CredentialDescriptor> {
        self.snapshot
            .lock()
            .map(|descriptors| descriptors.clone())
            .unwrap_or_default()
    }

    /// Assembles the full report. Store failures propagate (the caller owns
    /// the wire error); meter failures NEVER propagate — they are typed
    /// per-account unavailability.
    pub(crate) async fn report(
        &self,
        store: &haider_core::SqliteStoreHandle,
    ) -> Result<UsageReportV1, haider_protocol::error::HaiderError> {
        let local = collect_local_stats(store).await?;
        let haider_code_plans = self.haider_code_plans.lock().await.clone();
        let mut accounts = Vec::new();
        for descriptor in self.descriptors() {
            let mut plan = None;
            let mut identity = Some(descriptor.identity.clone()).filter(|id| !id.is_empty());
            let meter = if descriptor.provider == haider_provider::HAIDER_CODE_PROVIDER_NAME {
                match haider_code_plans.get(&descriptor.alias) {
                    Some(held) => {
                        plan.clone_from(&held.snapshot.plan);
                        AccountMeterStateV1::Metered {
                            windows: haider_code_windows(&held.snapshot),
                        }
                    }
                    None => AccountMeterStateV1::LocalOnly,
                }
            } else if agent_owned_meter(&descriptor) {
                // The plan stays ABSENT: the supervised agent never reports
                // one, and a model appearing in its catalog is not evidence of
                // a Pro or Ultra subscription.
                agent_owned_meter_state(&descriptor.status)
            } else {
                match meter_for(&descriptor) {
                    None => AccountMeterStateV1::LocalOnly,
                    Some(endpoint) => {
                        let entry = self.metered(endpoint, &descriptor).await;
                        if let Some(email) = entry.token_identity.email {
                            identity = Some(email);
                        }
                        plan = entry.token_identity.plan;
                        match entry.outcome {
                            Ok(reading) => {
                                if reading.plan.is_some() {
                                    plan.clone_from(&reading.plan);
                                }
                                if entry.fresh {
                                    for (window, basis_points) in
                                        reading.windows.iter().zip(&reading.basis_points)
                                    {
                                        store
                                            .append_usage_meter_sample(
                                                haider_protocol::usage::UsageHistoryMeterSampleV1 {
                                                    account: descriptor.alias.as_str().to_owned(),
                                                    window: window.window.clone(),
                                                    basis_points: *basis_points,
                                                    resets_at_ms: window.resets_at_ms,
                                                    grace_until_ms: None,
                                                    sampled_at_ms: entry.fetched_at_ms,
                                                    plan: plan.clone(),
                                                    credits: None,
                                                    hold: None,
                                                    stale: None,
                                                },
                                            )
                                            .await?;
                                    }
                                }
                                AccountMeterStateV1::Metered {
                                    windows: reading.windows,
                                }
                            }
                            Err(unavailable) => AccountMeterStateV1::Unavailable {
                                reason: unavailable.reason,
                            },
                        }
                    }
                }
            };
            accounts.push(AccountUsageReportV1 {
                provider: descriptor.provider.clone(),
                alias: descriptor.alias.clone(),
                identity,
                plan,
                auth_method: descriptor.auth_method,
                meter,
                local: local.get(&descriptor.alias).cloned().unwrap_or_default(),
            });
        }
        Ok(UsageReportV1 {
            generated_at_ms: (self.clock)(),
            accounts,
        })
    }

    /// One cached meter reading. The cache mutex is held across the fetch on
    /// purpose: it is the single-flight guard, and the poll floor bounds how
    /// often anything blocks here.
    async fn metered(
        &self,
        endpoint: UsageMeterEndpoint,
        descriptor: &CredentialDescriptor,
    ) -> MeterCacheEntry {
        let key = (descriptor.provider.clone(), descriptor.alias.clone());
        let now = (self.clock)();
        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.get(&key)
            && now.saturating_sub(entry.fetched_at_ms) < endpoint.min_poll_interval_ms()
        {
            return MeterCacheEntry {
                fetched_at_ms: entry.fetched_at_ms,
                outcome: entry.outcome.clone(),
                fresh: false,
                token_identity: OpenAiTokenIdentity {
                    email: entry.token_identity.email.clone(),
                    plan: entry.token_identity.plan.clone(),
                },
            };
        }
        let (outcome, token_identity) = self.fetch(endpoint, descriptor).await;
        cache.insert(
            key,
            MeterCacheEntry {
                fetched_at_ms: now,
                outcome: outcome.clone(),
                fresh: false,
                token_identity: OpenAiTokenIdentity {
                    email: token_identity.email.clone(),
                    plan: token_identity.plan.clone(),
                },
            },
        );
        MeterCacheEntry {
            fetched_at_ms: now,
            outcome,
            fresh: true,
            token_identity,
        }
    }

    async fn fetch(
        &self,
        endpoint: UsageMeterEndpoint,
        descriptor: &CredentialDescriptor,
    ) -> (Result<MeterReading, MeterUnavailable>, OpenAiTokenIdentity) {
        let Some(tokens) = &self.tokens else {
            return (
                Err(MeterUnavailable::new("credential_broker_unavailable")),
                OpenAiTokenIdentity::default(),
            );
        };
        let token = match tokens.bearer(descriptor).await {
            Ok(token) => token,
            Err(unavailable) => {
                return (Err(unavailable), OpenAiTokenIdentity::default());
            }
        };
        let token_identity = if endpoint == UsageMeterEndpoint::OpenAiOauth {
            openai_token_identity(token.expose_secret())
        } else {
            OpenAiTokenIdentity::default()
        };
        // OpenAI's WHAM meter is account-scoped. This value is the
        // `chatgpt_account_id` captured from the OAuth ID token by
        // `OpenAiOAuthIdentitySource`; an OpenAI organization id is never a
        // substitute. Refuse early instead of sending an ambiguous request
        // that fails later as a bare 401/403.
        let chatgpt_account_id = if endpoint == UsageMeterEndpoint::OpenAiOauth {
            match descriptor
                .account_identity
                .as_ref()
                .and_then(|identity| identity.account_id.as_deref())
                .filter(|account_id| !account_id.is_empty())
            {
                Some(account_id) => Some(account_id),
                None => {
                    return (
                        Err(MeterUnavailable::new("credential_identity_incomplete")),
                        token_identity,
                    );
                }
            }
        } else {
            None
        };
        let outcome = match self
            .http
            .get(
                endpoint.url(),
                &token,
                endpoint.extra_headers(),
                chatgpt_account_id,
            )
            .await
        {
            Ok((status, body)) => endpoint.parse_at(status, &body, (self.clock)()),
            Err(unavailable) => Err(unavailable),
        };
        (outcome, token_identity)
    }
}

fn haider_code_windows(snapshot: &HaiderCodePlanSnapshotV1) -> Vec<UsageWindowV1> {
    let Some(allowance) = snapshot.weekly_allowance.as_ref() else {
        return Vec::new();
    };
    let Some(percent_remaining) = allowance.percent_remaining else {
        return Vec::new();
    };
    vec![UsageWindowV1 {
        window: "weekly".into(),
        utilization: ((100.0 - percent_remaining) / 100.0).clamp(0.0, 1.0),
        resets_at_ms: allowance.resets_at_ms,
        label: None,
    }]
}

/// Adds read-time price estimates to attributed ledger rows. Counters remain
/// the raw day-fold truth; unknown provider/model pairs keep `None` and are
/// never priced from a model slug alone.
pub(crate) fn enrich_usage_history_costs(days: &mut [UsageHistoryRangeDayV1]) {
    for row in days.iter_mut().flat_map(|day| day.models.iter_mut()) {
        row.est_cost_microusd = haider_provider::estimate_chunk_cost_usd_for(
            &row.provider,
            &row.model,
            row.input_tokens,
            row.output_tokens,
            row.reasoning_tokens,
            row.cache_read_tokens,
        )
        .map(usd_to_microusd);
    }
}

// --- local accounting -----------------------------------------------------

/// Pure per-session fold state (separated from storage so the attribution
/// laws run over in-memory envelopes).
#[derive(Debug, Default)]
pub(crate) struct SessionLocalStats {
    /// Exact per-account token totals from usage events.
    pub tokens: HashMap<CredentialAlias, TokenTotals>,
    /// Session span end (max committed_at_ms seen).
    pub last_committed_at_ms: u64,
    /// Lines summed from completed fs tool receipts (session-level; the
    /// receipts carry no account, so attribution follows the dominant
    /// account).
    pub lines_added: u64,
    pub lines_removed: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cached: u64,
    pub est_cost_usd: Option<f64>,
    pub api_equivalent_est_cost_usd: Option<f64>,
    pub cache: CacheUsageStatsV1,
    metered_cache_cost_missing: bool,
    api_equivalent_cache_cost_missing: bool,
    metered_cost_missing: bool,
    api_equivalent_cost_missing: bool,
}

impl TokenTotals {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        input: u64,
        output: u64,
        reasoning: u64,
        cached: u64,
        cost: Option<f64>,
        normalized: Option<&NormalizedUsage>,
        scope: Option<&UsageScope>,
        cache_cost: Option<CacheCostEstimate>,
        request: Option<&RequestUsage>,
    ) {
        self.input = self.input.saturating_add(input);
        self.output = self.output.saturating_add(output);
        self.reasoning = self.reasoning.saturating_add(reasoning);
        self.cached = self.cached.saturating_add(cached);
        let auth_method = scope.and_then(scope_auth_method);
        let metered = auth_method == Some(AuthMethod::ApiKey);
        let known_auth = auth_method.is_some();
        if metered && let Some(cost) = cost {
            *self.est_cost_usd.get_or_insert(0.0) += cost;
        }
        if known_auth && let Some(cost) = cost {
            *self.api_equivalent_est_cost_usd.get_or_insert(0.0) += cost;
        }
        if metered && cost.is_none() {
            self.metered_cost_missing = true;
        }
        if !known_auth || cost.is_none() {
            self.api_equivalent_cost_missing = true;
        }
        add_cache_stats(&mut self.cache, normalized, scope, cache_cost, request);
        if metered && normalized.is_some() && cache_cost.is_none() {
            self.metered_cache_cost_missing = true;
        }
        if (!known_auth || cache_cost.is_none()) && normalized.is_some() {
            self.api_equivalent_cache_cost_missing = true;
        }
    }

    fn magnitude(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
            .saturating_add(self.cached)
    }
}

#[derive(serde::Serialize, Deserialize)]
struct UsagePayload {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    reasoning: u64,
    #[serde(default)]
    cached: u64,
    #[serde(default)]
    account: Option<CredentialAlias>,
    #[serde(default)]
    accounts: Vec<UsageAccountPayload>,
    #[serde(default)]
    normalized: Option<NormalizedUsage>,
    #[serde(default)]
    scope: Option<UsageScope>,
    #[serde(default)]
    cache_cost: Option<CacheCostEstimate>,
    #[serde(default)]
    request: Option<RequestUsage>,
}

#[derive(serde::Serialize, Deserialize)]
struct UsageAccountPayload {
    account: CredentialAlias,
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    reasoning: u64,
    #[serde(default)]
    cached: u64,
    #[serde(default)]
    normalized: Option<NormalizedUsage>,
    #[serde(default)]
    scope: Option<UsageScope>,
    #[serde(default)]
    cache_cost: Option<CacheCostEstimate>,
}

fn add_cache_stats(
    totals: &mut CacheUsageStatsV1,
    normalized: Option<&NormalizedUsage>,
    scope: Option<&UsageScope>,
    cost: Option<CacheCostEstimate>,
    request: Option<&RequestUsage>,
) {
    if let Some(request) = request {
        totals.requests.push(CacheUsageRequestV1 {
            scope: scope.map(|scope| CacheUsageRequestScopeV1 {
                provider: scope.provider.clone(),
                model: scope.model.clone(),
                cache_epoch: scope.cache_epoch.clone(),
                request_kind: scope.request_kind,
                auth_method: scope_auth_method(scope),
                run: scope.run.clone(),
            }),
            request: request.clone(),
        });
    }
    let Some(usage) = normalized else {
        return;
    };
    let totals_had_input = totals.logical_input_tokens > 0;
    let totals_had_metered_input = totals.metered_input_tokens > 0;
    totals.logical_input_tokens = totals
        .logical_input_tokens
        .saturating_add(usage.logical_input);
    totals.uncached_input_tokens = totals
        .uncached_input_tokens
        .saturating_add(usage.uncached_input);
    totals.cache_read_tokens = totals
        .cache_read_tokens
        .saturating_add(usage.cache_read_input);
    totals.cache_write_tokens = totals
        .cache_write_tokens
        .saturating_add(usage.cache_write_input);
    totals.cache_write_5m_tokens = totals
        .cache_write_5m_tokens
        .saturating_add(usage.cache_write_5m_input);
    totals.cache_write_1h_tokens = totals
        .cache_write_1h_tokens
        .saturating_add(usage.cache_write_1h_input);
    totals.billed_output_tokens = totals
        .billed_output_tokens
        .saturating_add(usage.billed_output);
    totals.telemetry_covered_input_tokens = totals
        .telemetry_covered_input_tokens
        .saturating_add(usage.cache_telemetry_input);
    let auth_method = scope.and_then(scope_auth_method);
    if auth_method == Some(haider_protocol::credential::AuthMethod::ApiKey) {
        totals.metered_input_tokens = totals
            .metered_input_tokens
            .saturating_add(usage.logical_input);
    }
    if auth_method == Some(AuthMethod::ApiKey) {
        merge_optional_cost(
            &mut totals.input_with_cache_usd,
            cost.map(|cost| cost.input_with_cache_usd),
            totals_had_metered_input,
        );
        merge_optional_cost(
            &mut totals.input_without_cache_usd,
            cost.map(|cost| cost.input_without_cache_usd),
            totals_had_metered_input,
        );
        merge_optional_cost(
            &mut totals.estimated_savings_usd,
            cost.map(|cost| cost.estimated_savings_usd),
            totals_had_metered_input,
        );
    }
    let api_cost = auth_method.is_some().then_some(cost).flatten();
    merge_optional_cost(
        &mut totals.api_equivalent_input_with_cache_usd,
        api_cost.map(|cost| cost.input_with_cache_usd),
        totals_had_input,
    );
    merge_optional_cost(
        &mut totals.api_equivalent_input_without_cache_usd,
        api_cost.map(|cost| cost.input_without_cache_usd),
        totals_had_input,
    );
    merge_optional_cost(
        &mut totals.api_equivalent_estimated_savings_usd,
        api_cost.map(|cost| cost.estimated_savings_usd),
        totals_had_input,
    );

    let (provider, model, epoch, request_kind) = scope.map_or_else(
        || {
            (
                String::new(),
                String::new(),
                String::new(),
                UsageRequestKind::MainTurn,
            )
        },
        |scope| {
            (
                scope.provider.clone(),
                scope.model.clone(),
                scope.cache_epoch.clone(),
                scope.request_kind,
            )
        },
    );
    let position = totals.breakdowns.iter().position(|entry| {
        entry.provider == provider
            && entry.model == model
            && entry.cache_epoch == epoch
            && entry.request_kind == request_kind
            && entry.auth_method == auth_method
    });
    let position = position.unwrap_or_else(|| {
        totals.breakdowns.push(CacheUsageBreakdownV1 {
            provider: provider.clone(),
            model: model.clone(),
            cache_epoch: epoch.clone(),
            request_kind,
            auth_method,
            cache_status: CacheStatAvailability::Present,
            ..CacheUsageBreakdownV1::default()
        });
        totals.breakdowns.len() - 1
    });
    let breakdown = &mut totals.breakdowns[position];
    let breakdown_had_input = breakdown.logical_input_tokens > 0;
    breakdown.logical_input_tokens = breakdown
        .logical_input_tokens
        .saturating_add(usage.logical_input);
    breakdown.uncached_input_tokens = breakdown
        .uncached_input_tokens
        .saturating_add(usage.uncached_input);
    breakdown.cache_read_tokens = breakdown
        .cache_read_tokens
        .saturating_add(usage.cache_read_input);
    breakdown.cache_write_tokens = breakdown
        .cache_write_tokens
        .saturating_add(usage.cache_write_input);
    breakdown.cache_write_5m_tokens = breakdown
        .cache_write_5m_tokens
        .saturating_add(usage.cache_write_5m_input);
    breakdown.cache_write_1h_tokens = breakdown
        .cache_write_1h_tokens
        .saturating_add(usage.cache_write_1h_input);
    breakdown.billed_output_tokens = breakdown
        .billed_output_tokens
        .saturating_add(usage.billed_output);
    breakdown.telemetry_covered_input_tokens = breakdown
        .telemetry_covered_input_tokens
        .saturating_add(usage.cache_telemetry_input);
    if usage.cache_status != CacheStatAvailability::Present {
        breakdown.cache_status = CacheStatAvailability::Unavailable;
    }
    if auth_method == Some(AuthMethod::ApiKey) {
        merge_optional_cost(
            &mut breakdown.input_with_cache_usd,
            cost.map(|cost| cost.input_with_cache_usd),
            breakdown_had_input,
        );
        merge_optional_cost(
            &mut breakdown.input_without_cache_usd,
            cost.map(|cost| cost.input_without_cache_usd),
            breakdown_had_input,
        );
        merge_optional_cost(
            &mut breakdown.estimated_savings_usd,
            cost.map(|cost| cost.estimated_savings_usd),
            breakdown_had_input,
        );
    }
    merge_optional_cost(
        &mut breakdown.api_equivalent_input_with_cache_usd,
        api_cost.map(|cost| cost.input_with_cache_usd),
        breakdown_had_input,
    );
    merge_optional_cost(
        &mut breakdown.api_equivalent_input_without_cache_usd,
        api_cost.map(|cost| cost.input_without_cache_usd),
        breakdown_had_input,
    );
    merge_optional_cost(
        &mut breakdown.api_equivalent_estimated_savings_usd,
        api_cost.map(|cost| cost.estimated_savings_usd),
        breakdown_had_input,
    );
}

fn scope_auth_method(scope: &UsageScope) -> Option<AuthMethod> {
    match scope.auth_scope.as_str() {
        "api_key" => Some(AuthMethod::ApiKey),
        "oauth" | "oauth_subscription" => Some(AuthMethod::OAuth),
        _ => None,
    }
}

const FS_TOOL_NAMES: [&str; 3] = ["fs_write", "fs_edit", "fs_path"];

fn line_count(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    text.lines().count() as u64
}

/// Lines added/removed for one COMPLETED fs tool receipt:
/// - `fs_write { content }` — content lines added (prior contents unknown);
/// - `fs_edit { edits }` — every old anchor removed and new text added once
///   (the receipt does not carry `replace_all` occurrence counts);
/// - `fs_path` — structural mutation with no inferred line delta.
fn fs_receipt_lines(name: &str, args: &serde_json::Value) -> (u64, u64) {
    let text = |key: &str| args.get(key).and_then(|value| value.as_str()).unwrap_or("");
    match name {
        "fs_write" => (line_count(text("content")), 0),
        "fs_edit" => args
            .get("edits")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .fold((0u64, 0u64), |(added, removed), edit| {
                let old = edit
                    .get("old")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let new = edit
                    .get("new")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                (
                    added.saturating_add(line_count(new)),
                    removed.saturating_add(line_count(old)),
                )
            }),
        _ => (0, 0),
    }
}

/// Incremental fold of one session's committed envelopes (in seq order)
/// into local stats. Exact across page boundaries: the folder holds the
/// last usage snapshot per `(run, agent, provider, model, cache epoch,
/// request kind, request ordinal)` and only reduces on
/// [`SessionFolder::finish`]. Modern records use response-local counters and
/// one key per physical request; legacy cumulative records retain a missing
/// ordinal and therefore preserve their last-snapshot-wins behavior.
///
/// - `model_selected` facts switch the pricing model for LATER usage;
/// - legacy usage snapshots are cumulative per cache/request lane — the
///   last snapshot wins (summing them would double-count);
/// - unattributed usage (no account, no subtotals) is skipped, never
///   invented onto an account.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct UsageChunkKey {
    run: String,
    agent: String,
    provider: String,
    model: String,
    cache_epoch: String,
    request_kind: UsageRequestKind,
    request_ordinal: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct AgentTiming {
    started_at_ms: u64,
    terminal_at_ms: Option<u64>,
    live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AgentBreakdownKey {
    provider: String,
    model: String,
    cache_epoch: String,
    request_kind: UsageRequestKind,
    auth_method: Option<AuthMethod>,
}

#[derive(Default)]
struct AgentBreakdownAccumulator {
    logical_input_tokens: u64,
    billed_output_tokens: u64,
    additional_reasoning_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    metered_cost_usd: f64,
    api_equivalent_cost_usd: f64,
    priced: bool,
    saw_component: bool,
}

#[derive(Default)]
struct AgentUsageAccumulator {
    logical_input_tokens: u64,
    uncached_input_tokens: u64,
    billed_output_tokens: u64,
    additional_reasoning_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    telemetry_covered_input_tokens: u64,
    prev_prefix_tokens: u64,
    cacheable_input_tokens: u64,
    cache_read_on_cacheable_tokens: u64,
    metered_cost_usd: f64,
    api_equivalent_cost_usd: f64,
    metered_lanes_priced: bool,
    all_lanes_priced: bool,
    has_metered_lanes: bool,
    has_oauth_lanes: bool,
    saw_component: bool,
    breakdowns: HashMap<AgentBreakdownKey, AgentBreakdownAccumulator>,
}

fn add_agent_cache_reread_component(
    accumulator: &mut AgentUsageAccumulator,
    input: u64,
    output: u64,
    cached: u64,
    normalized: Option<&NormalizedUsage>,
) {
    let logical = normalized.map_or(input, |usage| usage.logical_input);
    let billed_output = normalized.map_or(output, |usage| usage.billed_output);
    let read = normalized.map_or(cached, |usage| usage.cache_read_input);
    let cacheable = logical.min(accumulator.prev_prefix_tokens);
    let hit = read.min(cacheable);
    accumulator.cacheable_input_tokens =
        accumulator.cacheable_input_tokens.saturating_add(cacheable);
    accumulator.cache_read_on_cacheable_tokens = accumulator
        .cache_read_on_cacheable_tokens
        .saturating_add(hit);
    accumulator.prev_prefix_tokens = logical.saturating_add(billed_output);
}

fn scoped_chunk_cost_usd(
    scope: Option<&UsageScope>,
    model: &str,
    input: u64,
    output: u64,
    reasoning: u64,
    cached: u64,
) -> Option<f64> {
    scope
        .map(|scope| scope.provider.as_str())
        .filter(|provider| !provider.is_empty())
        .map_or_else(
            || haider_provider::estimate_chunk_cost_usd(model, input, output, reasoning, cached),
            |provider| {
                haider_provider::estimate_chunk_cost_usd_for(
                    provider, model, input, output, reasoning, cached,
                )
            },
        )
}

fn scoped_normalized_cost_usd(
    scope: Option<&UsageScope>,
    model: &str,
    usage: &NormalizedUsage,
) -> Option<f64> {
    scope
        .map(|scope| scope.provider.as_str())
        .filter(|provider| !provider.is_empty())
        .map_or_else(
            || haider_provider::estimate_normalized_usage_cost_usd(model, usage),
            |provider| {
                haider_provider::estimate_normalized_usage_cost_usd_for(provider, model, usage)
            },
        )
}

fn scoped_cache_cost(
    scope: Option<&UsageScope>,
    model: &str,
    usage: &NormalizedUsage,
    persisted: Option<CacheCostEstimate>,
) -> Option<CacheCostEstimate> {
    match scope
        .map(|scope| scope.provider.as_str())
        .filter(|provider| !provider.is_empty())
    {
        Some(provider) => haider_provider::estimate_cache_input_costs_for(provider, model, usage),
        None => persisted.or_else(|| haider_provider::estimate_cache_input_costs(model, usage)),
    }
}

#[allow(clippy::too_many_arguments)]
fn add_agent_usage_component(
    accumulator: &mut AgentUsageAccumulator,
    input: u64,
    output: u64,
    reasoning: u64,
    cached: u64,
    normalized: Option<&NormalizedUsage>,
    scope: Option<&UsageScope>,
    fallback_model: &str,
) {
    use haider_protocol::provider::ReasoningAccounting;

    let logical = normalized.map_or(input, |usage| usage.logical_input);
    let uncached = normalized.map_or(input, |usage| usage.uncached_input);
    let read = normalized.map_or(cached, |usage| usage.cache_read_input);
    let write = normalized.map_or(0, |usage| usage.cache_write_input);
    let billed_output = normalized.map_or(output, |usage| usage.billed_output);
    let additional_reasoning = normalized.map_or(0, |usage| {
        if usage.reasoning_accounting == ReasoningAccounting::AdditionalToOutput {
            usage.reasoning_detail
        } else {
            0
        }
    });
    let covered = normalized.map_or(0, |usage| usage.cache_telemetry_input);
    let model = scope
        .map(|scope| scope.model.as_str())
        .filter(|model| !model.is_empty())
        .unwrap_or(fallback_model);
    let cost = normalized.map_or_else(
        || scoped_chunk_cost_usd(scope, model, input, output, reasoning, cached),
        |usage| scoped_normalized_cost_usd(scope, model, usage),
    );
    let auth_method = scope.and_then(scope_auth_method);
    let metered = auth_method == Some(AuthMethod::ApiKey);

    accumulator.saw_component = true;
    if !accumulator.metered_lanes_priced && !accumulator.has_metered_lanes {
        accumulator.metered_lanes_priced = true;
    }
    if !accumulator.all_lanes_priced
        && !accumulator.has_metered_lanes
        && !accumulator.has_oauth_lanes
        && accumulator.breakdowns.is_empty()
    {
        accumulator.all_lanes_priced = true;
    }
    accumulator.logical_input_tokens = accumulator.logical_input_tokens.saturating_add(logical);
    accumulator.uncached_input_tokens = accumulator.uncached_input_tokens.saturating_add(uncached);
    accumulator.billed_output_tokens = accumulator
        .billed_output_tokens
        .saturating_add(billed_output);
    accumulator.additional_reasoning_tokens = accumulator
        .additional_reasoning_tokens
        .saturating_add(additional_reasoning);
    accumulator.cache_read_tokens = accumulator.cache_read_tokens.saturating_add(read);
    accumulator.cache_write_tokens = accumulator.cache_write_tokens.saturating_add(write);
    accumulator.telemetry_covered_input_tokens = accumulator
        .telemetry_covered_input_tokens
        .saturating_add(covered);
    match auth_method {
        Some(AuthMethod::ApiKey) => {
            accumulator.has_metered_lanes = true;
            if let Some(cost) = cost {
                accumulator.metered_cost_usd += cost;
                accumulator.api_equivalent_cost_usd += cost;
            } else {
                accumulator.metered_lanes_priced = false;
                accumulator.all_lanes_priced = false;
            }
        }
        Some(AuthMethod::OAuth) => {
            accumulator.has_oauth_lanes = true;
            if let Some(cost) = cost {
                accumulator.api_equivalent_cost_usd += cost;
            } else {
                accumulator.all_lanes_priced = false;
            }
        }
        None => accumulator.all_lanes_priced = false,
    }

    let key = AgentBreakdownKey {
        provider: scope.map_or_else(String::new, |scope| scope.provider.clone()),
        model: model.to_owned(),
        cache_epoch: scope.map_or_else(String::new, |scope| scope.cache_epoch.clone()),
        request_kind: scope.map_or(UsageRequestKind::MainTurn, |scope| scope.request_kind),
        auth_method,
    };
    let breakdown = accumulator.breakdowns.entry(key).or_default();
    if !breakdown.saw_component {
        breakdown.priced = true;
        breakdown.saw_component = true;
    }
    breakdown.logical_input_tokens = breakdown.logical_input_tokens.saturating_add(logical);
    breakdown.billed_output_tokens = breakdown.billed_output_tokens.saturating_add(billed_output);
    breakdown.additional_reasoning_tokens = breakdown
        .additional_reasoning_tokens
        .saturating_add(additional_reasoning);
    breakdown.cache_read_tokens = breakdown.cache_read_tokens.saturating_add(read);
    breakdown.cache_write_tokens = breakdown.cache_write_tokens.saturating_add(write);
    if auth_method.is_none() || cost.is_none() {
        breakdown.priced = false;
    } else if let Some(cost) = cost {
        breakdown.api_equivalent_cost_usd += cost;
        if metered {
            breakdown.metered_cost_usd += cost;
        }
    }
}

fn usd_to_microusd(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    ((value * 1_000_000.0).round() as u64).max(1)
}

#[derive(Default)]
pub(crate) struct SessionFolder {
    stats: SessionLocalStats,
    current_model: String,
    /// BTreeMap (ship-gate round): the folds below accumulate FLOAT costs,
    /// and float addition is non-associative — a HashMap's randomized
    /// iteration order could shift the final microusd rounding run-to-run.
    /// Ordered iteration makes every fold deterministic.
    /// The tuple's sequence stamp preserves journal chronology for the
    /// order-dependent cache re-read fold without changing key deduplication.
    chunks: BTreeMap<UsageChunkKey, (UsagePayload, String, u64)>,
    tool_attempts: HashMap<String, HashSet<String>>,
    timings: HashMap<String, AgentTiming>,
    run_agents: HashMap<String, Option<String>>,
    root_run_seen: bool,
    primary_agent: Option<AgentId>,
}

impl SessionFolder {
    pub(crate) fn new(initial_model: &str) -> Self {
        Self {
            stats: SessionLocalStats::default(),
            current_model: initial_model.to_owned(),
            chunks: BTreeMap::new(),
            tool_attempts: HashMap::new(),
            timings: HashMap::new(),
            run_agents: HashMap::new(),
            root_run_seen: false,
            primary_agent: None,
        }
    }

    /// Model active at the latest envelope folded into this session.
    /// Empty means the session supplied neither typed metadata nor a
    /// `model_selected` fact.
    pub(crate) fn active_model(&self) -> Option<&str> {
        (!self.current_model.is_empty()).then_some(self.current_model.as_str())
    }

    /// Heap owned by the incremental observe/usage fold. Saturating charges
    /// deliberately include collection slabs plus every retained coordinate;
    /// these maps are precisely where long-lived sessions accumulate run,
    /// agent, usage-chunk, and tool IDs.
    pub(crate) fn deep_owned_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>().saturating_add(self.current_model.capacity());
        total = total.saturating_add(self.stats.tokens.capacity().saturating_mul(
            std::mem::size_of::<(CredentialAlias, TokenTotals)>().saturating_add(1),
        ));
        for (alias, totals) in &self.stats.tokens {
            total = total.saturating_add(alias.0.capacity()).saturating_add(
                serde_json::to_vec(&totals.cache).map_or(usize::MAX, |bytes| bytes.len()),
            );
        }
        for (key, (payload, model, _)) in &self.chunks {
            let conservative_btree_node = 11_usize
                .saturating_mul(
                    std::mem::size_of::<UsageChunkKey>().saturating_add(std::mem::size_of::<(
                        UsagePayload,
                        String,
                        u64,
                    )>()),
                )
                .saturating_add(16_usize.saturating_mul(std::mem::size_of::<usize>()));
            total = total
                // One complete eleven-slot node plus generous edge/metadata
                // space per occupied entry covers sparse-root and split-node
                // slack without depending on std's private BTree layout.
                .saturating_add(conservative_btree_node)
                .saturating_add(key.run.capacity())
                .saturating_add(key.agent.capacity())
                .saturating_add(key.provider.capacity())
                .saturating_add(key.model.capacity())
                .saturating_add(key.cache_epoch.capacity())
                .saturating_add(model.capacity())
                .saturating_add(
                    serde_json::to_vec(payload).map_or(usize::MAX, |bytes| bytes.len()),
                );
        }
        total =
            total.saturating_add(self.tool_attempts.capacity().saturating_mul(
                std::mem::size_of::<(String, HashSet<String>)>().saturating_add(1),
            ));
        for (tool, attempts) in &self.tool_attempts {
            total = total.saturating_add(tool.capacity()).saturating_add(
                attempts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<String>().saturating_add(1)),
            );
            for attempt in attempts {
                total = total.saturating_add(attempt.capacity());
            }
        }
        total = total.saturating_add(
            self.timings
                .capacity()
                .saturating_mul(std::mem::size_of::<(String, AgentTiming)>().saturating_add(1)),
        );
        for agent in self.timings.keys() {
            total = total.saturating_add(agent.capacity());
        }
        total = total.saturating_add(
            self.run_agents
                .capacity()
                .saturating_mul(std::mem::size_of::<(String, Option<String>)>().saturating_add(1)),
        );
        for (run, agent) in &self.run_agents {
            total = total.saturating_add(run.capacity());
            if let Some(agent) = agent {
                total = total.saturating_add(agent.capacity());
            }
        }
        if let Some(agent) = &self.primary_agent {
            total = total.saturating_add(agent.0.capacity());
        }
        total
    }

    pub(crate) fn push(&mut self, envelope: &RawEnvelope) {
        // Delegated sessions begin with an unscoped SessionCreated fact, then
        // name their durable agent on the accepted turn. Remember that first
        // actual agent, while `root_run_seen` below keeps root sessions in the
        // `None` bucket even after child-scoped mirror events arrive.
        if self.primary_agent.is_none()
            && let Some(agent) = &envelope.agent_id
        {
            self.primary_agent = Some(agent.clone());
        }
        if let Some(run) = &envelope.run_id {
            let owner = self
                .run_agents
                .entry(run.as_str().to_owned())
                .or_insert_with(|| {
                    envelope
                        .agent_id
                        .as_ref()
                        .map(|agent| agent.as_str().to_owned())
                });
            self.root_run_seen |= owner.is_none();
        }
        // Cancellation terminalization can emit an unscoped run-state
        // envelope. Recover its already-established agent from the durable
        // run instead of accidentally settling the root/empty bucket.
        let envelope_agent = envelope.agent_id.as_ref().map_or_else(
            || {
                envelope
                    .run_id
                    .as_ref()
                    .and_then(|run| self.run_agents.get(run.as_str()))
                    .cloned()
                    .flatten()
                    .unwrap_or_default()
            },
            |agent| agent.as_str().to_owned(),
        );
        // SessionCreated/config facts are not agent work. The first
        // run-scoped durable fact establishes the elapsed basis.
        if envelope.run_id.is_some() {
            let timing = self.timings.entry(envelope_agent.clone()).or_default();
            if timing.started_at_ms == 0 {
                timing.started_at_ms = envelope.committed_at_ms;
                timing.live = true;
            }
        }
        self.stats.last_committed_at_ms = self
            .stats
            .last_committed_at_ms
            .max(envelope.committed_at_ms);
        let Some(kind) = envelope.payload.get("type").and_then(|kind| kind.as_str()) else {
            return;
        };
        match kind {
            "model_selected" => {
                if let Some(selected) = ModelSelected::from_payload_value(&envelope.payload) {
                    self.current_model = selected.model;
                }
            }
            "usage" => {
                let Ok(mut usage) = envelope.payload.decode::<UsagePayload>() else {
                    return;
                };
                let request_ordinal = usage.request.as_ref().map(|request| request.ordinal);
                if let Some(request) = usage.request.as_ref() {
                    usage.input = request.input;
                    usage.output = request.output;
                    usage.reasoning = request.reasoning.unwrap_or(0);
                    usage.cached = request.cached.unwrap_or(0);
                    usage.account.clone_from(&request.account);
                    usage.accounts.clear();
                    usage.normalized.clone_from(&request.normalized);
                    usage.cache_cost = request.cache_cost;
                }
                let scope = usage.scope.as_ref();
                let model = scope
                    .map(|scope| scope.model.as_str())
                    .filter(|model| !model.is_empty())
                    .unwrap_or(&self.current_model)
                    .to_owned();
                let key = UsageChunkKey {
                    run: scope
                        .and_then(|scope| scope.run.as_ref())
                        .or(envelope.run_id.as_ref())
                        .map_or_else(String::new, |run| run.as_str().to_owned()),
                    agent: scope
                        .and_then(|scope| scope.agent.as_ref())
                        .or(envelope.agent_id.as_ref())
                        .map_or_else(String::new, |agent| agent.as_str().to_owned()),
                    provider: scope.map_or_else(String::new, |scope| scope.provider.clone()),
                    model: model.clone(),
                    cache_epoch: scope.map_or_else(String::new, |scope| scope.cache_epoch.clone()),
                    request_kind: scope
                        .map_or(UsageRequestKind::MainTurn, |scope| scope.request_kind),
                    request_ordinal,
                };
                self.chunks.insert(key, (usage, model, envelope.seq));
            }
            "item" => {
                if envelope
                    .payload
                    .get("item")
                    .and_then(|item| item.get("item"))
                    .and_then(serde_json::Value::as_str)
                    != Some("tool_call")
                {
                    return;
                }
                let payload = &envelope.payload;
                if let Ok(EventPayload::Item(event)) = payload.decode_event() {
                    match event {
                        ItemEvent::Started { item_id, item }
                        | ItemEvent::Completed { item_id, item }
                            if matches!(item, TurnItem::ToolCall { .. }) =>
                        {
                            self.tool_attempts
                                .entry(envelope_agent.clone())
                                .or_default()
                                .insert(item_id.as_str().to_owned());
                        }
                        ItemEvent::Started { .. }
                        | ItemEvent::Delta { .. }
                        | ItemEvent::Completed { .. } => {}
                    }
                }
                if payload.get("event").and_then(|event| event.as_str()) != Some("completed") {
                    return;
                }
                let Some(item) = payload.get("item") else {
                    return;
                };
                if item.get("item").and_then(|kind| kind.as_str()) != Some("tool_call")
                    || item.get("status").and_then(|status| status.as_str()) != Some("completed")
                {
                    return;
                }
                let Some(name) = item.get("name").and_then(|name| name.as_str()) else {
                    return;
                };
                if !FS_TOOL_NAMES.contains(&name) {
                    return;
                }
                let empty = serde_json::Value::Null;
                let args = item.get("args").unwrap_or(&empty);
                let (added, removed) = fs_receipt_lines(name, args);
                self.stats.lines_added = self.stats.lines_added.saturating_add(added);
                self.stats.lines_removed = self.stats.lines_removed.saturating_add(removed);
            }
            "run_state" => {
                if let Ok(EventPayload::RunState(state)) = envelope.payload.decode_event() {
                    let timing = self.timings.entry(envelope_agent).or_default();
                    if state.is_terminal() {
                        timing.live = false;
                        timing.terminal_at_ms = Some(envelope.committed_at_ms);
                    } else {
                        timing.live = true;
                        timing.terminal_at_ms = None;
                    }
                }
            }
            _ => {}
        }
    }

    /// Direct metrics for the session's owning agent at `head_seq`. The
    /// borrowing form lets the delegation mirror publish as each child page
    /// advances without consuming the account-report fold.
    pub(crate) fn primary_agent_snapshot(
        &self,
        session_id: &SessionId,
        head_seq: u64,
    ) -> Option<AgentMetricsSnapshot> {
        let agent = (!self.root_run_seen)
            .then_some(self.primary_agent.as_ref())
            .flatten();
        self.agent_snapshot(session_id, agent, head_seq)
    }

    pub(crate) fn agent_snapshot(
        &self,
        session_id: &SessionId,
        agent: Option<&AgentId>,
        head_seq: u64,
    ) -> Option<AgentMetricsSnapshot> {
        let agent_key = agent.map_or("", AgentId::as_str);
        let timing = self.timings.get(agent_key)?;
        let mut accumulator = AgentUsageAccumulator::default();
        for (key, (usage, fallback_model, _)) in &self.chunks {
            if key.agent != agent_key {
                continue;
            }
            if usage.accounts.is_empty() {
                add_agent_usage_component(
                    &mut accumulator,
                    usage.input,
                    usage.output,
                    usage.reasoning,
                    usage.cached,
                    usage.normalized.as_ref(),
                    usage.scope.as_ref(),
                    fallback_model,
                );
            } else {
                for subtotal in &usage.accounts {
                    add_agent_usage_component(
                        &mut accumulator,
                        subtotal.input,
                        subtotal.output,
                        subtotal.reasoning,
                        subtotal.cached,
                        subtotal.normalized.as_ref(),
                        subtotal.scope.as_ref().or(usage.scope.as_ref()),
                        fallback_model,
                    );
                }
            }
        }
        let mut chronological_chunks = self
            .chunks
            .iter()
            .filter(|(key, _)| key.agent == agent_key)
            .collect::<Vec<_>>();
        chronological_chunks.sort_by_key(|(_, (_, _, seq))| *seq);
        for (_, (usage, _, _)) in chronological_chunks {
            if usage.accounts.is_empty() {
                add_agent_cache_reread_component(
                    &mut accumulator,
                    usage.input,
                    usage.output,
                    usage.cached,
                    usage.normalized.as_ref(),
                );
            } else {
                for subtotal in &usage.accounts {
                    add_agent_cache_reread_component(
                        &mut accumulator,
                        subtotal.input,
                        subtotal.output,
                        subtotal.cached,
                        subtotal.normalized.as_ref(),
                    );
                }
            }
        }
        let usage = accumulator.saw_component.then(|| {
            let cache_hit_basis_points = (accumulator.logical_input_tokens > 0
                && accumulator.telemetry_covered_input_tokens == accumulator.logical_input_tokens)
                .then(|| {
                    let denominator = accumulator
                        .cache_read_tokens
                        .saturating_add(accumulator.uncached_input_tokens);
                    let points = accumulator
                        .cache_read_tokens
                        .saturating_mul(10_000)
                        .checked_div(denominator)
                        .unwrap_or_default();
                    u32::try_from(points).unwrap_or(10_000).min(10_000)
                });
            let cache_reread_hit_basis_points =
                (accumulator.cacheable_input_tokens > 0).then(|| {
                    let points = accumulator
                        .cache_read_on_cacheable_tokens
                        .saturating_mul(10_000)
                        .checked_div(accumulator.cacheable_input_tokens)
                        .unwrap_or_default();
                    u32::try_from(points).unwrap_or(10_000).min(10_000)
                });
            let metered_cost_microusd = accumulator
                .has_metered_lanes
                .then_some(accumulator.metered_lanes_priced)
                .filter(|priced| *priced)
                .map(|_| usd_to_microusd(accumulator.metered_cost_usd));
            let api_equivalent_cost_microusd = accumulator
                .all_lanes_priced
                .then(|| usd_to_microusd(accumulator.api_equivalent_cost_usd));
            let mut breakdowns = accumulator
                .breakdowns
                .into_iter()
                .map(|(key, value)| AgentUsageBreakdown {
                    provider: key.provider,
                    model: key.model,
                    cache_epoch: key.cache_epoch,
                    request_kind: key.request_kind,
                    auth_method: key.auth_method,
                    logical_input_tokens: value.logical_input_tokens,
                    billed_output_tokens: value.billed_output_tokens,
                    additional_reasoning_tokens: value.additional_reasoning_tokens,
                    cache_read_tokens: value.cache_read_tokens,
                    cache_write_tokens: value.cache_write_tokens,
                    metered_cost_microusd: (key.auth_method == Some(AuthMethod::ApiKey)
                        && value.priced)
                        .then(|| usd_to_microusd(value.metered_cost_usd)),
                    api_equivalent_cost_microusd: value
                        .priced
                        .then(|| usd_to_microusd(value.api_equivalent_cost_usd)),
                    priced: value.priced,
                })
                .collect::<Vec<_>>();
            // Verify round 2 (roster digests): the fold's HashMap key includes
            // the auth method, so the sort must too — rows sharing every
            // other coordinate would otherwise reorder run-to-run and turn
            // serialized-summary digests into false deltas.
            breakdowns.sort_by(|left, right| {
                (
                    left.provider.as_str(),
                    left.model.as_str(),
                    left.cache_epoch.as_str(),
                    request_kind_rank(left.request_kind),
                    format!("{:?}", left.auth_method),
                )
                    .cmp(&(
                        right.provider.as_str(),
                        right.model.as_str(),
                        right.cache_epoch.as_str(),
                        request_kind_rank(right.request_kind),
                        format!("{:?}", right.auth_method),
                    ))
            });
            AgentUsageMetrics {
                logical_input_tokens: accumulator.logical_input_tokens,
                billed_output_tokens: accumulator.billed_output_tokens,
                additional_reasoning_tokens: accumulator.additional_reasoning_tokens,
                cache_read_tokens: accumulator.cache_read_tokens,
                cache_write_tokens: accumulator.cache_write_tokens,
                cache_hit_basis_points,
                cache_reread_hit_basis_points,
                metered_cost_microusd,
                api_equivalent_cost_microusd,
                all_lanes_priced: accumulator.all_lanes_priced,
                has_metered_lanes: accumulator.has_metered_lanes,
                has_oauth_lanes: accumulator.has_oauth_lanes,
                breakdowns,
            }
        });
        Some(AgentMetricsSnapshot {
            agent: agent.cloned(),
            session_id: session_id.clone(),
            head_seq,
            started_at_ms: timing.started_at_ms,
            terminal_at_ms: timing.terminal_at_ms,
            live: timing.live,
            tool_attempts: self
                .tool_attempts
                .get(agent_key)
                .map_or(0, HashSet::len)
                .try_into()
                .unwrap_or(u64::MAX),
            usage,
        })
    }

    pub(crate) fn finish(self) -> SessionLocalStats {
        let mut stats = self.stats;
        for (usage, model, _) in self.chunks.into_values() {
            let model = model.as_str();
            if !usage.accounts.is_empty() {
                let parent_scope = usage.scope.as_ref();
                for subtotal in usage.accounts {
                    let subtotal_scope = subtotal.scope.as_ref().or(parent_scope);
                    let subtotal_model = subtotal_scope
                        .map(|scope| scope.model.as_str())
                        .filter(|model| !model.is_empty())
                        .unwrap_or(model);
                    let cost = subtotal.normalized.as_ref().map_or_else(
                        || {
                            scoped_chunk_cost_usd(
                                subtotal_scope,
                                subtotal_model,
                                subtotal.input,
                                subtotal.output,
                                subtotal.reasoning,
                                subtotal.cached,
                            )
                        },
                        |normalized| {
                            scoped_normalized_cost_usd(subtotal_scope, subtotal_model, normalized)
                        },
                    );
                    let cache_cost = subtotal.normalized.as_ref().and_then(|normalized| {
                        scoped_cache_cost(
                            subtotal_scope,
                            subtotal_model,
                            normalized,
                            subtotal.cache_cost,
                        )
                    });
                    stats.tokens.entry(subtotal.account).or_default().add(
                        subtotal.input,
                        subtotal.output,
                        subtotal.reasoning,
                        subtotal.cached,
                        cost,
                        subtotal.normalized.as_ref(),
                        subtotal_scope,
                        cache_cost,
                        None,
                    );
                }
            } else if let Some(account) = usage.account {
                let scope = usage.scope.as_ref();
                let scoped_model = scope
                    .map(|scope| scope.model.as_str())
                    .filter(|model| !model.is_empty())
                    .unwrap_or(model);
                let cache_cost = usage.normalized.as_ref().and_then(|normalized| {
                    scoped_cache_cost(scope, scoped_model, normalized, usage.cache_cost)
                });
                let cost = usage.normalized.as_ref().map_or_else(
                    || {
                        scoped_chunk_cost_usd(
                            scope,
                            scoped_model,
                            usage.input,
                            usage.output,
                            usage.reasoning,
                            usage.cached,
                        )
                    },
                    |normalized| scoped_normalized_cost_usd(scope, scoped_model, normalized),
                );
                stats.tokens.entry(account).or_default().add(
                    usage.input,
                    usage.output,
                    usage.reasoning,
                    usage.cached,
                    cost,
                    usage.normalized.as_ref(),
                    scope,
                    cache_cost,
                    usage.request.as_ref(),
                );
            }
        }
        for totals in stats.tokens.values_mut() {
            if totals.metered_cost_missing {
                totals.est_cost_usd = None;
            }
            if totals.api_equivalent_cost_missing {
                totals.api_equivalent_est_cost_usd = None;
            }
            if totals.metered_cache_cost_missing {
                totals.cache.input_with_cache_usd = None;
                totals.cache.input_without_cache_usd = None;
                totals.cache.estimated_savings_usd = None;
            }
            if totals.api_equivalent_cache_cost_missing {
                totals.cache.api_equivalent_input_with_cache_usd = None;
                totals.cache.api_equivalent_input_without_cache_usd = None;
                totals.cache.api_equivalent_estimated_savings_usd = None;
            }
        }
        stats
    }
}

const fn request_kind_rank(kind: UsageRequestKind) -> u8 {
    match kind {
        UsageRequestKind::MainTurn => 0,
        UsageRequestKind::Compaction => 1,
        UsageRequestKind::DelegatedAgent => 2,
        _ => 3,
    }
}

/// Merges one folded session into per-account totals: tokens exactly per
/// account; the session count, span, and LOC to the dominant account.
pub(crate) fn attribute_session(
    totals: &mut HashMap<CredentialAlias, LocalUsageStatsV1>,
    created_at_ms: u64,
    stats: SessionLocalStats,
) {
    let mut dominant: Option<(CredentialAlias, u64)> = None;
    for (alias, tokens) in &stats.tokens {
        let magnitude = tokens.magnitude();
        let entry = totals.entry(alias.clone()).or_default();
        entry.input_tokens = entry.input_tokens.saturating_add(tokens.input);
        entry.output_tokens = entry.output_tokens.saturating_add(tokens.output);
        entry.reasoning_tokens = entry.reasoning_tokens.saturating_add(tokens.reasoning);
        entry.cached_tokens = entry.cached_tokens.saturating_add(tokens.cached);
        merge_cache_stats(&mut entry.cache, &tokens.cache);
        if let Some(cost) = tokens.est_cost_usd {
            *entry.est_cost_usd.get_or_insert(0.0) += cost;
        }
        if let Some(cost) = tokens.api_equivalent_est_cost_usd {
            *entry.api_equivalent_est_cost_usd.get_or_insert(0.0) += cost;
        }
        let beats = dominant.as_ref().is_none_or(|(_, best)| magnitude > *best);
        if beats {
            dominant = Some((alias.clone(), magnitude));
        }
    }
    if let Some((alias, _)) = dominant {
        let entry = totals.entry(alias).or_default();
        entry.sessions = entry.sessions.saturating_add(1);
        entry.total_duration_ms = entry
            .total_duration_ms
            .saturating_add(stats.last_committed_at_ms.saturating_sub(created_at_ms));
        entry.lines_added = entry.lines_added.saturating_add(stats.lines_added);
        entry.lines_removed = entry.lines_removed.saturating_add(stats.lines_removed);
    }
}

fn merge_optional_cost(target: &mut Option<f64>, source: Option<f64>, target_had_input: bool) {
    *target = match (*target, source, target_had_input) {
        (_, None, _) => None,
        (Some(left), Some(right), true) => Some(left + right),
        (_, Some(right), false) => Some(right),
        (None, Some(_), true) => None,
    };
}

fn merge_cache_stats(target: &mut CacheUsageStatsV1, source: &CacheUsageStatsV1) {
    let target_had_input = target.logical_input_tokens > 0;
    let target_had_metered_input = target.metered_input_tokens > 0;
    target.logical_input_tokens = target
        .logical_input_tokens
        .saturating_add(source.logical_input_tokens);
    target.uncached_input_tokens = target
        .uncached_input_tokens
        .saturating_add(source.uncached_input_tokens);
    target.cache_read_tokens = target
        .cache_read_tokens
        .saturating_add(source.cache_read_tokens);
    target.cache_write_tokens = target
        .cache_write_tokens
        .saturating_add(source.cache_write_tokens);
    target.cache_write_5m_tokens = target
        .cache_write_5m_tokens
        .saturating_add(source.cache_write_5m_tokens);
    target.cache_write_1h_tokens = target
        .cache_write_1h_tokens
        .saturating_add(source.cache_write_1h_tokens);
    target.billed_output_tokens = target
        .billed_output_tokens
        .saturating_add(source.billed_output_tokens);
    target.telemetry_covered_input_tokens = target
        .telemetry_covered_input_tokens
        .saturating_add(source.telemetry_covered_input_tokens);
    target.metered_input_tokens = target
        .metered_input_tokens
        .saturating_add(source.metered_input_tokens);
    target.requests.extend(source.requests.iter().cloned());
    if source.metered_input_tokens > 0 {
        merge_optional_cost(
            &mut target.input_with_cache_usd,
            source.input_with_cache_usd,
            target_had_metered_input,
        );
        merge_optional_cost(
            &mut target.input_without_cache_usd,
            source.input_without_cache_usd,
            target_had_metered_input,
        );
        merge_optional_cost(
            &mut target.estimated_savings_usd,
            source.estimated_savings_usd,
            target_had_metered_input,
        );
    }
    merge_optional_cost(
        &mut target.api_equivalent_input_with_cache_usd,
        source.api_equivalent_input_with_cache_usd,
        target_had_input,
    );
    merge_optional_cost(
        &mut target.api_equivalent_input_without_cache_usd,
        source.api_equivalent_input_without_cache_usd,
        target_had_input,
    );
    merge_optional_cost(
        &mut target.api_equivalent_estimated_savings_usd,
        source.api_equivalent_estimated_savings_usd,
        target_had_input,
    );
    for source_breakdown in &source.breakdowns {
        let position = target.breakdowns.iter().position(|entry| {
            entry.provider == source_breakdown.provider
                && entry.model == source_breakdown.model
                && entry.cache_epoch == source_breakdown.cache_epoch
                && entry.request_kind == source_breakdown.request_kind
                && entry.auth_method == source_breakdown.auth_method
        });
        let Some(position) = position else {
            target.breakdowns.push(source_breakdown.clone());
            continue;
        };
        let entry = &mut target.breakdowns[position];
        let entry_had_input = entry.logical_input_tokens > 0;
        entry.logical_input_tokens = entry
            .logical_input_tokens
            .saturating_add(source_breakdown.logical_input_tokens);
        entry.uncached_input_tokens = entry
            .uncached_input_tokens
            .saturating_add(source_breakdown.uncached_input_tokens);
        entry.cache_read_tokens = entry
            .cache_read_tokens
            .saturating_add(source_breakdown.cache_read_tokens);
        entry.cache_write_tokens = entry
            .cache_write_tokens
            .saturating_add(source_breakdown.cache_write_tokens);
        entry.cache_write_5m_tokens = entry
            .cache_write_5m_tokens
            .saturating_add(source_breakdown.cache_write_5m_tokens);
        entry.cache_write_1h_tokens = entry
            .cache_write_1h_tokens
            .saturating_add(source_breakdown.cache_write_1h_tokens);
        entry.billed_output_tokens = entry
            .billed_output_tokens
            .saturating_add(source_breakdown.billed_output_tokens);
        entry.telemetry_covered_input_tokens = entry
            .telemetry_covered_input_tokens
            .saturating_add(source_breakdown.telemetry_covered_input_tokens);
        if source_breakdown.cache_status != CacheStatAvailability::Present {
            entry.cache_status = CacheStatAvailability::Unavailable;
        }
        merge_optional_cost(
            &mut entry.input_with_cache_usd,
            source_breakdown.input_with_cache_usd,
            entry_had_input,
        );
        merge_optional_cost(
            &mut entry.input_without_cache_usd,
            source_breakdown.input_without_cache_usd,
            entry_had_input,
        );
        merge_optional_cost(
            &mut entry.estimated_savings_usd,
            source_breakdown.estimated_savings_usd,
            entry_had_input,
        );
        merge_optional_cost(
            &mut entry.api_equivalent_input_with_cache_usd,
            source_breakdown.api_equivalent_input_with_cache_usd,
            entry_had_input,
        );
        merge_optional_cost(
            &mut entry.api_equivalent_input_without_cache_usd,
            source_breakdown.api_equivalent_input_without_cache_usd,
            entry_had_input,
        );
        merge_optional_cost(
            &mut entry.api_equivalent_estimated_savings_usd,
            source_breakdown.api_equivalent_estimated_savings_usd,
            entry_had_input,
        );
    }
}

const SCAN_PAGE_ENVELOPES: usize = 256;
const SCAN_PAGE_BYTES: usize = 512 * 1024;

/// Scans every durable session and folds per-account local stats.
async fn collect_local_stats(
    store: &haider_core::SqliteStoreHandle,
) -> Result<HashMap<CredentialAlias, LocalUsageStatsV1>, haider_protocol::error::HaiderError> {
    let mut totals = HashMap::new();
    for session_id in store.session_ids().await? {
        let metadata = store.session_metadata(&session_id).await?;
        let initial_model = metadata
            .as_ref()
            .map(|metadata| metadata.model.clone())
            .unwrap_or_default();
        let mut created_at_ms = metadata
            .as_ref()
            .map(|metadata| metadata.created_at_ms)
            .unwrap_or(0);
        let mut folder = SessionFolder::new(&initial_model);
        let mut since_seq = 0;
        loop {
            let page = store
                .read_page(&session_id, since_seq, SCAN_PAGE_ENVELOPES, SCAN_PAGE_BYTES)
                .await?;
            let Some(last) = page.last() else {
                break;
            };
            since_seq = last.seq;
            if created_at_ms == 0 {
                created_at_ms = page
                    .first()
                    .map(|envelope| envelope.committed_at_ms)
                    .unwrap_or(0);
            }
            for envelope in &page {
                folder.push(envelope);
            }
        }
        let stats = folder.finish();
        if !stats.tokens.is_empty() || stats.lines_added > 0 || stats.lines_removed > 0 {
            attribute_session(&mut totals, created_at_ms, stats);
        }
    }
    Ok(totals)
}

#[cfg(test)]
#[path = "usage_report_tests.rs"]
mod usage_report_tests;
