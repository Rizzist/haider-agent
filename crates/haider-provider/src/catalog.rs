//! Model discovery from the PROVIDERS' OWN sources (W5e-2).
//!
//! The owner requirement is that model choice never comes from a hardcoded
//! slug table: it comes from the same place the vendors' own CLIs get it,
//! authorized with the subscription credentials the vault already holds.
//!
//! - **OpenAI subscription**: `GET {OPENAI_SUBSCRIPTION_BASE_URL}/models`,
//!   the endpoint the installed codex CLI uses
//!   (`codex-api/src/endpoint/models.rs`, base
//!   `https://chatgpt.com/backend-api/codex`). Confirmed against a live
//!   `~/.codex/models_cache.json` on 2026-07-30: ETag-conditional, and each
//!   entry carries display name, description, reasoning-effort ladder,
//!   visibility and picker priority.
//! - **Anthropic subscription**: `GET {ANTHROPIC_OAUTH_BASE_URL}/v1/models`
//!   with the OAuth bearer and the same beta headers W5b.2 already proves
//!   work for inference.
//!
//! DISCOVERY IS NEVER SYNTHESIZED. When a provider will not serve a list,
//! [`CatalogError::Unavailable`] is returned so the caller can fall back to
//! its last-known cache or say "unavailable" — this module never invents a
//! model that the provider did not name.
//!
//! Every request here is CREDENTIAL-BEARING to a fixed origin, so it takes
//! the same W5a discipline as the token endpoints: resolve-validate-pin
//! through [`FixedOriginGuard`], proxies off, redirects refused, bounded
//! body.

use std::sync::Arc;
use std::time::Duration;

use crate::origin::{FixedDnsResolver, FixedOriginGuard, SystemFixedDnsResolver};
use reqwest::header::HeaderValue;

/// Hard cap on a discovery response body. The codex payload embeds
/// per-model base instructions, so this is generous but still bounded.
const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const CATALOG_TIMEOUT: Duration = Duration::from_secs(15);
/// Value for the codex model endpoint's required `client_version` query
/// param. The backend only checks PRESENCE and well-formedness — it does
/// not gate the returned catalog on the value — so a stable recent codex
/// version keeps us a good citizen without pinning haider to codex's
/// release cadence.
const OPENAI_CODEX_MODELS_CLIENT_VERSION: &str = "0.145.0";

/// One model as the PROVIDER described it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredModel {
    /// The wire id used in a turn request.
    pub slug: String,
    /// Human label the provider supplies; falls back to the slug.
    pub display_name: String,
    /// The provider's declared context window in tokens; `None` when the
    /// provider does not declare one — never a guess.
    pub context_window: Option<u64>,
    pub description: Option<String>,
    /// The provider's default reasoning effort, when it declares one.
    pub default_effort: Option<String>,
    /// The effort ladder the provider declares (codex: low → ultra).
    /// EMPTY means "the provider declares none" — never a guessed ladder.
    pub supported_efforts: Vec<String>,
    /// False when the provider marks the model hidden from pickers.
    pub visible: bool,
    /// Picker ordering hint; lower sorts first.
    pub priority: Option<i64>,
    /// Provider-declared capability metadata that is richer than the common
    /// catalog contract. Absent for existing sources so their serialized
    /// cache rows stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<DiscoveredModelExtensions>,
}

/// Kimi's declared model flags, kept as provider-owned facts rather than
/// inferred from a model name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredModelExtensions {
    pub protocol: String,
    pub supports_reasoning: bool,
    pub supports_vision: bool,
    pub supports_tool_use: bool,
    pub supports_thinking_type: bool,
}

/// The result of one discovery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCatalog {
    pub models: Vec<DiscoveredModel>,
    /// Validator for a conditional refetch, when the provider supplies one.
    pub etag: Option<String>,
}

#[derive(Debug)]
pub enum CatalogError {
    /// The provider answered "not modified": the caller's cache stands.
    NotModified,
    /// The provider will not serve a list to this credential (or at all).
    /// The caller falls back; it must NOT fabricate a list.
    Unavailable { reason: String },
    /// Transport/guard failure — retryable.
    Transport { reason: String },
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotModified => write!(formatter, "model catalog is unchanged"),
            Self::Unavailable { reason } | Self::Transport { reason } => {
                write!(formatter, "{reason}")
            }
        }
    }
}

/// Which provider family's discovery shape to speak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogSource {
    /// codex's ChatGPT-backend catalog.
    OpenAiSubscription,
    /// Anthropic's `/v1/models` under an OAuth bearer.
    AnthropicSubscription,
    /// Kimi Code's nonstandard OAuth catalog.
    KimiOAuth,
    /// Gemini's fixed model catalog authenticated by `x-goog-api-key`.
    GeminiApiKey,
    /// An OpenAI-compatible custom profile's stored API origin.
    OpenAiCompatible { origin: String },
}

/// The final request URL for a source's model catalog.
///
/// The codex backend REQUIRES a `client_version` query param — a bearer-only
/// GET 400s with `missing field 'client_version'` (confirmed against the live
/// endpoint 2026-07-30). Any well-formed value returns the full catalog; the
/// value does not gate the model list. The origin guard pins the base
/// host/path, so the query is not part of the SSRF surface.
#[must_use]
pub fn catalog_request_url(source: CatalogSource, endpoint: &str) -> String {
    match source {
        CatalogSource::OpenAiSubscription => {
            format!("{endpoint}?client_version={OPENAI_CODEX_MODELS_CLIENT_VERSION}")
        }
        CatalogSource::AnthropicSubscription
        | CatalogSource::KimiOAuth
        | CatalogSource::GeminiApiKey
        | CatalogSource::OpenAiCompatible { .. } => endpoint.to_owned(),
    }
}

impl CatalogSource {
    /// The exact endpoint discovery will request (and pin).
    #[must_use]
    pub fn endpoint(&self) -> String {
        match self {
            Self::OpenAiSubscription => {
                format!("{}/models", crate::OPENAI_SUBSCRIPTION_BASE_URL)
            }
            Self::AnthropicSubscription => {
                format!("{}/v1/models", crate::ANTHROPIC_OAUTH_BASE_URL)
            }
            Self::KimiOAuth => format!("{}/models", crate::KIMI_OAUTH_BASE_URL),
            Self::GeminiApiKey => crate::GEMINI_MODELS_URL.to_owned(),
            Self::OpenAiCompatible { origin } => {
                format!("{}/models", origin.trim().trim_end_matches('/'))
            }
        }
    }

    fn trusted_host(&self) -> Option<&'static str> {
        match self {
            Self::OpenAiSubscription => Some("chatgpt.com"),
            Self::AnthropicSubscription => Some("api.anthropic.com"),
            Self::KimiOAuth => Some("api.kimi.com"),
            Self::GeminiApiKey => Some("generativelanguage.googleapis.com"),
            Self::OpenAiCompatible { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalogAuthMode {
    Bearer,
    XGoogApiKey,
    /// Azure OpenAI v1 (G4b): API keys authenticate with the bare
    /// `api-key` header — Bearer is documented only for Entra tokens.
    AzureApiKey,
}

impl CatalogSource {
    fn auth_mode(&self) -> CatalogAuthMode {
        match self {
            Self::GeminiApiKey => CatalogAuthMode::XGoogApiKey,
            // G4b: a custom profile at an Azure OpenAI origin discovers
            // under the same `api-key` header its turn requests use (ONE
            // predicate — `azure_openai_origin`).
            Self::OpenAiCompatible { origin } if crate::openai::azure_openai_origin(origin) => {
                CatalogAuthMode::AzureApiKey
            }
            Self::OpenAiSubscription
            | Self::AnthropicSubscription
            | Self::KimiOAuth
            | Self::OpenAiCompatible { .. } => CatalogAuthMode::Bearer,
        }
    }
}

/// Validates a custom profile origin and returns its exact model-list URL.
///
/// This discovery-time backstop runs before a request is built and obeys the
/// SAME origin matrix as the custom adapter's
/// [`crate::CompatibleOriginPolicy::TrustedLan`] fence (G4a — the
/// `OpenAiCompatible` source exists only for Custom-provenance profiles):
/// loopback and RFC1918 LAN literals are accepted over http AND https;
/// link-local `169.254.0.0/16`, multicast, and other special-use literals
/// are refused entirely; every other remote origin must use HTTPS.
pub fn openai_compatible_catalog_endpoint(origin: &str) -> Result<String, CatalogError> {
    let origin = origin.trim().trim_end_matches('/');
    let parsed = reqwest::Url::parse(origin).map_err(|_| CatalogError::Unavailable {
        reason: "custom model catalog origin is not a valid URL".to_owned(),
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(CatalogError::Unavailable {
            reason: "custom model catalog origin must be an http(s) URL without credentials, query, or fragment"
                .to_owned(),
        });
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| CatalogError::Unavailable {
            reason: "custom model catalog origin must include a host".to_owned(),
        })?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let ip = host.parse::<std::net::IpAddr>().ok();
    if ip.is_some_and(|address| {
        crate::openai::blocked_credential_target_with_policy(
            address,
            crate::CompatibleOriginPolicy::TrustedLan,
        )
    }) {
        return Err(CatalogError::Unavailable {
            reason:
                "custom model catalog origin must not target a link-local, multicast, or special-use IP address"
                    .to_owned(),
        });
    }
    let plain_http_host = host.eq_ignore_ascii_case("localhost")
        || ip.is_some_and(|address| {
            address.is_loopback() || crate::openai::rfc1918_private(address)
        });
    if parsed.scheme() == "http" && !plain_http_host {
        return Err(CatalogError::Unavailable {
            reason:
                "custom remote model catalog origins must use HTTPS; HTTP is allowed only for loopback or RFC1918 LAN hosts"
                    .to_owned(),
        });
    }
    Ok(format!("{origin}/models"))
}

/// Discovers a provider's model list with an optional bearer credential.
///
/// `etag` is the caller's cached validator; when the provider answers 304
/// this returns [`CatalogError::NotModified`] and the cache stands.
pub async fn discover_models(
    source: CatalogSource,
    access_token: Option<&str>,
    etag: Option<&str>,
) -> Result<DiscoveredCatalog, CatalogError> {
    discover_models_with_resolver(source, access_token, etag, Arc::new(SystemFixedDnsResolver))
        .await
}

/// Testable core: the resolver is injectable so a fake server (and the SSRF
/// pins) can drive the exact production path.
pub async fn discover_models_with_resolver(
    source: CatalogSource,
    access_token: Option<&str>,
    etag: Option<&str>,
    resolver: Arc<dyn FixedDnsResolver>,
) -> Result<DiscoveredCatalog, CatalogError> {
    let (endpoint, guard) = match &source {
        CatalogSource::OpenAiCompatible { origin } => {
            (openai_compatible_catalog_endpoint(origin)?, None)
        }
        CatalogSource::OpenAiSubscription
        | CatalogSource::AnthropicSubscription
        | CatalogSource::KimiOAuth
        | CatalogSource::GeminiApiKey => {
            let endpoint = source.endpoint();
            let Some(trusted_host) = source.trusted_host() else {
                return Err(CatalogError::Unavailable {
                    reason: "model catalog origin is not usable".to_owned(),
                });
            };
            let guard = Arc::new(
                FixedOriginGuard::new(&endpoint, trusted_host, resolver).map_err(|error| {
                    CatalogError::Unavailable {
                        reason: format!("model catalog origin is not usable: {}", error.message),
                    }
                })?,
            );
            // The SSRF gate runs BEFORE the token can leave: resolve, validate
            // the exact endpoint, pin the addresses.
            guard.validate_endpoint(&endpoint).await.map_err(|error| {
                CatalogError::Unavailable {
                    reason: format!("model catalog endpoint refused: {}", error.message),
                }
            })?;
            (endpoint, Some(guard))
        }
    };
    let mut client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(CATALOG_TIMEOUT);
    if let Some(guard) = &guard {
        client = client.dns_resolver(Arc::clone(guard));
    }
    let client = client.build().map_err(|_| CatalogError::Transport {
        reason: "model catalog transport is unavailable".to_owned(),
    })?;

    let request_url = catalog_request_url(source.clone(), &endpoint);
    let mut request = client
        .get(&request_url)
        .header(reqwest::header::CONNECTION, "close")
        .header(reqwest::header::ACCEPT, "application/json");
    request = apply_catalog_credential(request, &source, access_token)?;
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if matches!(source, CatalogSource::AnthropicSubscription) {
        // The same beta surface the OAuth inference path already uses.
        request = request
            .header("anthropic-beta", crate::ANTHROPIC_OAUTH_BETA_VALUE)
            .header("anthropic-version", "2023-06-01");
    }

    let response = request
        .send()
        .await
        .map_err(|error| CatalogError::Transport {
            reason: format!("model catalog request failed: {error}"),
        })?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Err(CatalogError::NotModified);
    }
    if response.status().is_redirection() {
        return Err(CatalogError::Unavailable {
            reason: "model catalog endpoint redirects; refusing to follow".to_owned(),
        });
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        // 401/403/404 under a subscription token is the "not served to this
        // credential" case the brief anticipated: honest unavailability, and
        // the caller keeps its last-known list.
        return Err(CatalogError::Unavailable {
            reason: format!("provider does not serve a model list to this credential ({status})"),
        });
    }
    let response_etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // Bounded read: never trust a length header.
    let mut body: Vec<u8> = Vec::new();
    let mut stream = response;
    while let Some(chunk) = stream
        .chunk()
        .await
        .map_err(|error| CatalogError::Transport {
            reason: format!("model catalog body failed: {error}"),
        })?
    {
        if body.len() + chunk.len() > MAX_CATALOG_BYTES {
            return Err(CatalogError::Unavailable {
                reason: "model catalog response exceeded its bound".to_owned(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|error| CatalogError::Unavailable {
            reason: format!("model catalog response is not JSON: {error}"),
        })?;
    let models = parse_catalog(source, &value)?;
    if models.is_empty() {
        return Err(CatalogError::Unavailable {
            reason: "provider returned an empty model list".to_owned(),
        });
    }
    Ok(DiscoveredCatalog {
        models,
        etag: response_etag,
    })
}

/// Parses either provider's shape. Public so the fake-server tests drive the
/// same parser production uses.
pub fn parse_catalog(
    source: CatalogSource,
    value: &serde_json::Value,
) -> Result<Vec<DiscoveredModel>, CatalogError> {
    let entries = match source {
        // codex: `{ "models": [ … ] }`
        CatalogSource::OpenAiSubscription | CatalogSource::GeminiApiKey => value.get("models"),
        // Anthropic and OpenAI-compatible: `{ "data": [ … ] }`
        CatalogSource::AnthropicSubscription
        | CatalogSource::KimiOAuth
        | CatalogSource::OpenAiCompatible { .. } => value.get("data"),
    }
    .and_then(serde_json::Value::as_array)
    .ok_or_else(|| CatalogError::Unavailable {
        reason: "model catalog response has no model array".to_owned(),
    })?;

    let mut models = Vec::new();
    for entry in entries {
        if matches!(source, CatalogSource::GeminiApiKey) {
            let Some(slug) = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .and_then(|name| name.strip_prefix("models/").or(Some(name)))
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            models.push(DiscoveredModel {
                slug: slug.to_owned(),
                display_name: slug.to_owned(),
                context_window: entry
                    .get("inputTokenLimit")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|window| *window > 0),
                description: None,
                default_effort: None,
                supported_efforts: Vec::new(),
                visible: true,
                priority: None,
                extensions: None,
            });
            continue;
        }
        if matches!(source, CatalogSource::OpenAiCompatible { .. }) {
            let Some(slug) = entry.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            models.push(DiscoveredModel {
                slug: slug.to_owned(),
                display_name: slug.to_owned(),
                context_window: None,
                description: None,
                default_effort: None,
                supported_efforts: Vec::new(),
                visible: true,
                priority: None,
                extensions: None,
            });
            continue;
        }
        if matches!(source, CatalogSource::KimiOAuth)
            && entry
                .get("protocol")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|protocol| protocol.eq_ignore_ascii_case("anthropic"))
        {
            // Residual documented by B6k: the Anthropic Messages adapter is
            // intentionally not wired for Kimi until a later catalog wave.
            continue;
        }
        // codex names it `slug`; Anthropic names it `id`.
        let Some(slug) = entry
            .get("slug")
            .or_else(|| entry.get("id"))
            .and_then(serde_json::Value::as_str)
        else {
            continue; // an unnamed entry is not a model we can request
        };
        let display_name = entry
            .get("display_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(slug)
            .to_owned();
        let supported_efforts = entry
            .get("supported_reasoning_levels")
            .and_then(serde_json::Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|level| {
                        level
                            .get("effort")
                            .or(Some(level))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let context_window = match source {
            CatalogSource::OpenAiSubscription => entry
                .get("context_window")
                .and_then(serde_json::Value::as_u64)
                .filter(|window| *window > 0),
            CatalogSource::KimiOAuth => entry
                .get("context_length")
                .and_then(serde_json::Value::as_u64)
                .filter(|window| *window > 0),
            CatalogSource::AnthropicSubscription
            | CatalogSource::GeminiApiKey
            | CatalogSource::OpenAiCompatible { .. } => None,
        };
        let kimi_extensions =
            matches!(source, CatalogSource::KimiOAuth).then(|| DiscoveredModelExtensions {
                protocol: entry
                    .get("protocol")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("openai")
                    .to_owned(),
                supports_reasoning: entry
                    .get("supports_reasoning")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                supports_vision: entry
                    .get("supports_image_in")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                supports_tool_use: entry
                    .get("supports_tool_use")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                supports_thinking_type: entry
                    .get("supports_thinking_type")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            });
        let supported_efforts = if matches!(source, CatalogSource::KimiOAuth) {
            entry
                .get("think_efforts")
                .and_then(serde_json::Value::as_array)
                .map(|efforts| {
                    efforts
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            supported_efforts
        };
        models.push(DiscoveredModel {
            slug: slug.to_owned(),
            display_name,
            context_window,
            description: entry
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            default_effort: entry
                .get("default_reasoning_level")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            supported_efforts,
            // codex marks pickable models `visibility: "list"`. An entry with
            // no visibility field is visible (Anthropic's shape).
            visible: entry
                .get("visibility")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|visibility| visibility == "list"),
            priority: entry.get("priority").and_then(serde_json::Value::as_i64),
            extensions: kimi_extensions,
        });
    }
    Ok(models)
}

pub(crate) fn apply_catalog_credential(
    request: reqwest::RequestBuilder,
    source: &CatalogSource,
    credential: Option<&str>,
) -> Result<reqwest::RequestBuilder, CatalogError> {
    let Some(credential) = credential else {
        return Ok(request);
    };
    match source.auth_mode() {
        CatalogAuthMode::Bearer => Ok(request.bearer_auth(credential)),
        CatalogAuthMode::XGoogApiKey => {
            sensitive_credential_header(request, "x-goog-api-key", credential)
        }
        CatalogAuthMode::AzureApiKey => sensitive_credential_header(request, "api-key", credential),
    }
}

fn sensitive_credential_header(
    request: reqwest::RequestBuilder,
    name: &'static str,
    credential: &str,
) -> Result<reqwest::RequestBuilder, CatalogError> {
    let mut value =
        HeaderValue::from_bytes(credential.as_bytes()).map_err(|_| CatalogError::Unavailable {
            reason: "model catalog credential is not a valid HTTP header value".to_owned(),
        })?;
    value.set_sensitive(true);
    Ok(request.header(name, value))
}

/// Picker order: provider priority first, then display name. Hidden models
/// are dropped — the provider said not to list them.
#[must_use]
pub fn pickable(models: &[DiscoveredModel]) -> Vec<DiscoveredModel> {
    let mut visible: Vec<DiscoveredModel> = models
        .iter()
        .filter(|model| model.visible)
        .cloned()
        .collect();
    visible.sort_by(|left, right| {
        left.priority
            .unwrap_or(i64::MAX)
            .cmp(&right.priority.unwrap_or(i64::MAX))
            .then_with(|| left.display_name.cmp(&right.display_name))
    });
    visible
}
