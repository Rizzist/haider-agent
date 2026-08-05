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
//! - **Local accounting** — a journal fold per session: cumulative
//!   [`haider_protocol::EventPayload::Usage`] snapshots keyed by
//!   `(run, agent)` (last snapshot wins; summing them would double-count),
//!   `model_selected` facts tracked in sequence order so each usage chunk
//!   prices under the model active when it was reported, lines-of-code from
//!   COMPLETED `fs_write`/`fs_patch`/`fs_edit` tool receipts, and session
//!   span from creation to the last committed event. Sessions, duration,
//!   and LOC attribute to the session's DOMINANT account (most tokens);
//!   token totals attribute exactly per usage events.
//!
//! Secrets discipline: tokens live only inside `SecretHandle` borrows for
//! the duration of one request; reasons, reports, and cache entries carry
//! no token or response bytes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_accounts::SecretHandle;
use haider_protocol::credential::{AuthMethod, CredentialDescriptor};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::session::ModelSelected;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1,
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
        // The broker's own message may name vault/refresh internals; the
        // report carries only the bounded classification.
        self.resolve(descriptor)
            .await
            .map_err(|_| MeterUnavailable::new("credential_unavailable"))
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
    ) -> Result<(u16, Vec<u8>), MeterUnavailable>;
}

/// Production transport: pinned-policy reqwest client (no proxy, no
/// redirects, bounded timeouts). A client that cannot be built degrades to
/// typed unavailability, never a panic.
pub(crate) struct ReqwestUsageMeterHttp {
    client: Option<reqwest::Client>,
}

impl ReqwestUsageMeterHttp {
    pub(crate) fn new() -> Self {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .ok();
        Self { client }
    }
}

#[async_trait::async_trait]
impl UsageMeterHttp for ReqwestUsageMeterHttp {
    async fn get(
        &self,
        url: &str,
        bearer: &SecretHandle,
        extra_headers: &[(&'static str, &'static str)],
    ) -> Result<(u16, Vec<u8>), MeterUnavailable> {
        let Some(client) = &self.client else {
            return Err(MeterUnavailable::new("transport_unavailable"));
        };
        let token = std::str::from_utf8(bearer.expose_secret())
            .map_err(|_| MeterUnavailable::new("credential_not_utf8"))?;
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| MeterUnavailable::new("credential_not_header_safe"))?;
        authorization.set_sensitive(true);
        let mut request = client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, authorization);
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
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

/// Which meter (if any) serves one descriptor. Only the three sanctioned
/// OAuth subscriptions have a server meter; every API-key, custom, or
/// unknown provider is honest local-only accounting.
pub(crate) fn meter_for(descriptor: &CredentialDescriptor) -> Option<UsageMeterEndpoint> {
    if descriptor.auth_method != AuthMethod::OAuth {
        return None;
    }
    match descriptor.provider.as_str() {
        haider_provider::OPENAI_OAUTH_PROVIDER_NAME => Some(UsageMeterEndpoint::OpenAiOauth),
        haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME => Some(UsageMeterEndpoint::AnthropicOauth),
        haider_provider::KIMI_OAUTH_PROVIDER_NAME => Some(UsageMeterEndpoint::KimiOauth),
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
    /// openai-oauth enrichment captured at fetch time.
    token_identity: OpenAiTokenIdentity,
}

/// The installed `usage.report` service (hub seam, like the worker manager).
pub(crate) struct UsageReportService {
    snapshot: AccountsSnapshot,
    tokens: Option<Arc<dyn MeterTokenSource>>,
    http: Arc<dyn UsageMeterHttp>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    cache: tokio::sync::Mutex<HashMap<(String, CredentialAlias), MeterCacheEntry>>,
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
        }
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
        let mut accounts = Vec::new();
        for descriptor in self.descriptors() {
            let mut plan = None;
            let mut identity = Some(descriptor.identity.clone()).filter(|id| !id.is_empty());
            let meter = match meter_for(&descriptor) {
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
                                plan = reading.plan;
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
                token_identity: OpenAiTokenIdentity {
                    email: token_identity.email.clone(),
                    plan: token_identity.plan.clone(),
                },
            },
        );
        MeterCacheEntry {
            fetched_at_ms: now,
            outcome,
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
        let outcome = match self
            .http
            .get(endpoint.url(), &token, endpoint.extra_headers())
            .await
        {
            Ok((status, body)) => endpoint.parse(status, &body),
            Err(unavailable) => Err(unavailable),
        };
        (outcome, token_identity)
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

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cached: u64,
    pub est_cost_usd: Option<f64>,
}

impl TokenTotals {
    fn add(&mut self, input: u64, output: u64, reasoning: u64, cached: u64, cost: Option<f64>) {
        self.input = self.input.saturating_add(input);
        self.output = self.output.saturating_add(output);
        self.reasoning = self.reasoning.saturating_add(reasoning);
        self.cached = self.cached.saturating_add(cached);
        if let Some(cost) = cost {
            *self.est_cost_usd.get_or_insert(0.0) += cost;
        }
    }

    fn magnitude(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
            .saturating_add(self.cached)
    }
}

#[derive(Deserialize)]
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
}

#[derive(Deserialize)]
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
}

const FS_TOOL_NAMES: [&str; 3] = ["fs_write", "fs_patch", "fs_edit"];

fn line_count(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    text.lines().count() as u64
}

/// Lines added/removed for one COMPLETED fs tool receipt:
/// - `fs_write { content }` — content lines added (prior contents unknown);
/// - `fs_patch { preimage, replacement }` — preimage removed, replacement
///   added;
/// - `fs_edit { old_string, new_string }` — old removed, new added (counted
///   once even under `replace_all`; the receipt carries no occurrence
///   count).
fn fs_receipt_lines(name: &str, args: &serde_json::Value) -> (u64, u64) {
    let text = |key: &str| args.get(key).and_then(|value| value.as_str()).unwrap_or("");
    match name {
        "fs_write" => (line_count(text("content")), 0),
        "fs_patch" => (
            line_count(text("replacement")),
            line_count(text("preimage")),
        ),
        "fs_edit" => (
            line_count(text("new_string")),
            line_count(text("old_string")),
        ),
        _ => (0, 0),
    }
}

/// Incremental fold of one session's committed envelopes (in seq order)
/// into local stats. Exact across page boundaries: the folder holds the
/// last cumulative usage snapshot per `(run, agent)` and the model active
/// at each snapshot, and only reduces on [`SessionFolder::finish`].
///
/// - `model_selected` facts switch the pricing model for LATER usage;
/// - usage snapshots are cumulative per `(run, agent)` — the last snapshot
///   wins (summing them would double-count);
/// - unattributed usage (no account, no subtotals) is skipped, never
///   invented onto an account.
#[derive(Default)]
pub(crate) struct SessionFolder {
    stats: SessionLocalStats,
    current_model: String,
    chunks: HashMap<(String, String), (UsagePayload, String)>,
}

impl SessionFolder {
    pub(crate) fn new(initial_model: &str) -> Self {
        Self {
            stats: SessionLocalStats::default(),
            current_model: initial_model.to_owned(),
            chunks: HashMap::new(),
        }
    }

    pub(crate) fn push(&mut self, envelope: &RawEnvelope) {
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
                let Ok(usage) = serde_json::from_value::<UsagePayload>(envelope.payload.clone())
                else {
                    return;
                };
                let key = (
                    envelope
                        .run_id
                        .as_ref()
                        .map(|run| run.as_str().to_owned())
                        .unwrap_or_default(),
                    envelope
                        .agent_id
                        .as_ref()
                        .map(|agent| agent.as_str().to_owned())
                        .unwrap_or_default(),
                );
                self.chunks.insert(key, (usage, self.current_model.clone()));
            }
            "item" => {
                let payload = &envelope.payload;
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
            _ => {}
        }
    }

    pub(crate) fn finish(self) -> SessionLocalStats {
        let mut stats = self.stats;
        for (usage, model) in self.chunks.into_values() {
            let model = model.as_str();
            if !usage.accounts.is_empty() {
                for subtotal in usage.accounts {
                    let cost = haider_provider::estimate_chunk_cost_usd(
                        model,
                        subtotal.input,
                        subtotal.output,
                        subtotal.reasoning,
                        subtotal.cached,
                    );
                    stats.tokens.entry(subtotal.account).or_default().add(
                        subtotal.input,
                        subtotal.output,
                        subtotal.reasoning,
                        subtotal.cached,
                        cost,
                    );
                }
            } else if let Some(account) = usage.account {
                let cost = haider_provider::estimate_chunk_cost_usd(
                    model,
                    usage.input,
                    usage.output,
                    usage.reasoning,
                    usage.cached,
                );
                stats.tokens.entry(account).or_default().add(
                    usage.input,
                    usage.output,
                    usage.reasoning,
                    usage.cached,
                    cost,
                );
            }
        }
        stats
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
        if let Some(cost) = tokens.est_cost_usd {
            *entry.est_cost_usd.get_or_insert(0.0) += cost;
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
