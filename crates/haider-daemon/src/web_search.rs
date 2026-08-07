//! W-B (decision 3): the daemon-side executor behind the CLIENT `web_search`
//! tool advertised on responses-lite pairs.
//!
//! Responses-lite REJECTS hosted tools, so the subscription pair cannot ask
//! the model's own backend to search. Codex solves this by executing the
//! search ITSELF against `{subscription_base}/alpha/search` under the same
//! OAuth Bearer as turns; this module is that execution, minus codex's
//! namespaced `web.run` surface.
//!
//! Shape mirrors [`crate::usage_report`]: the credential comes from the SAME
//! [`CredentialBroker`] provider construction uses (so a search rides the
//! refresh single-flight instead of racing it) and the transport is an
//! injected seam, so every law drives a loopback server or a stub and no
//! test ever dials chatgpt.com.
//!
//! Honesty rules:
//! - The endpoint is UNOFFICIAL (codex-source-verified only). A non-2xx
//!   status surfaces the provider's own bounded body VERBATIM.
//! - 404/410 means the endpoint is gone: the failure is marked `degraded`
//!   so the hub latches the session capability off and the tool stops being
//!   advertised — one probe per session, never a retry storm.
//!
//! Secrets discipline: the token lives only inside a `SecretHandle` borrow
//! for the duration of one request; failures carry no token bytes.

use std::sync::Arc;
use std::time::Duration;

use haider_accounts::SecretHandle;
use haider_provider::{
    OPENAI_OAUTH_PROVIDER_NAME, codex_alpha_search_request_body, codex_alpha_search_response_text,
    codex_alpha_search_url,
};

use crate::oauth::CredentialBroker;
use crate::worker::{WebSearchExecutor, WebSearchFailure};

/// How much of a failing response body is quoted back verbatim.
const ERROR_BODY_QUOTE_BYTES: usize = 2 * 1024;

/// Injected HTTP seam: ONE authenticated POST, no retries (the tool result
/// is the model's retry signal). Every failure is a bounded reason string.
#[async_trait::async_trait]
pub(crate) trait WebSearchHttp: Send + Sync {
    async fn post_json(
        &self,
        url: &str,
        bearer: &SecretHandle,
        body: &serde_json::Value,
    ) -> Result<(u16, Vec<u8>), String>;
}

/// Production transport: proxy-free, redirect-free, timeout-bounded.
pub(crate) struct ReqwestWebSearchHttp {
    client: Option<reqwest::Client>,
}

impl ReqwestWebSearchHttp {
    pub(crate) fn new() -> Self {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(45))
            .build()
            .ok();
        Self { client }
    }
}

#[async_trait::async_trait]
impl WebSearchHttp for ReqwestWebSearchHttp {
    async fn post_json(
        &self,
        url: &str,
        bearer: &SecretHandle,
        body: &serde_json::Value,
    ) -> Result<(u16, Vec<u8>), String> {
        let Some(client) = &self.client else {
            return Err("search transport is unavailable".into());
        };
        let token = std::str::from_utf8(bearer.expose_secret())
            .map_err(|_| "subscription credential is not valid UTF-8".to_owned())?;
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "subscription credential is not header-safe".to_owned())?;
        authorization.set_sensitive(true);
        let response = client
            .post(url)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .json(body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    "search request timed out".to_owned()
                } else {
                    "search request could not reach the endpoint".to_owned()
                }
            })?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|_| "search response body could not be read".to_owned())?;
        Ok((status, bytes.to_vec()))
    }
}

/// The client `web_search` executor for subscription pairs.
pub(crate) struct SubscriptionWebSearch {
    broker: CredentialBroker,
    http: Arc<dyn WebSearchHttp>,
}

impl SubscriptionWebSearch {
    pub(crate) fn new(broker: CredentialBroker, http: Arc<dyn WebSearchHttp>) -> Self {
        Self { broker, http }
    }
}

/// Quotes a failing body verbatim, bounded and on a char boundary.
fn quote_error_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "(empty response body)".to_owned();
    }
    if trimmed.len() <= ERROR_BODY_QUOTE_BYTES {
        return trimmed.to_owned();
    }
    let mut cut = ERROR_BODY_QUOTE_BYTES;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &trimmed[..cut])
}

#[async_trait::async_trait]
impl WebSearchExecutor for SubscriptionWebSearch {
    async fn search(
        &self,
        model: &str,
        session_id: &str,
        query: &str,
    ) -> Result<String, WebSearchFailure> {
        let account = self
            .broker
            .resolve_account(OPENAI_OAUTH_PROVIDER_NAME, None)
            .await
            .map_err(|error| WebSearchFailure {
                message: error.message,
                degraded: false,
            })?;
        let bearer = self
            .broker
            .resolve(&account.descriptor)
            .await
            .map_err(|error| WebSearchFailure {
                message: error.message,
                degraded: false,
            })?;
        let url = codex_alpha_search_url(account.descriptor.base_url.as_deref());
        let body = codex_alpha_search_request_body(session_id, model, query);
        let (status, bytes) =
            self.http
                .post_json(&url, &bearer, &body)
                .await
                .map_err(|message| WebSearchFailure {
                    message,
                    degraded: false,
                })?;
        match status {
            // Gone for good: latch the session capability off.
            404 | 410 => Err(WebSearchFailure {
                message: format!(
                    "the subscription search endpoint answered HTTP {status}; web_search is unavailable for the rest of this session"
                ),
                degraded: true,
            }),
            200..=299 => Ok(codex_alpha_search_response_text(&bytes)),
            _ => Err(WebSearchFailure {
                message: format!("HTTP {status}: {}", quote_error_body(&bytes)),
                degraded: false,
            }),
        }
    }
}
