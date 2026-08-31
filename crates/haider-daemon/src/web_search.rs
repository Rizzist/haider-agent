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

/// Hard ceiling on the raw search response body streamed off the endpoint
/// (H4). `response.bytes()` buffers the WHOLE body first; a compromised or
/// runaway `chatgpt.com` could hand back an unbounded response and exhaust
/// memory before the 32 KiB downstream result cap ever applies. The extracted
/// text is re-capped to 32 KiB downstream, so this bounds the raw JSON
/// generously — legitimate results still parse, a hostile body cannot.
const SEARCH_RESPONSE_CAP_BYTES: usize = 1024 * 1024;

/// Streaming, bounded body read: never buffers more than `cap` bytes off the
/// unofficial endpoint (H4). Mirrors the webfetch `read_body_bounded` shape —
/// a chunk that would cross the cap is clamped and the read stops.
async fn read_body_capped(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "search response body could not be read".to_owned())?
    {
        let remaining = cap.saturating_sub(body.len());
        if chunk.len() >= remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Injected credential seam: which subscription credential one search rides,
/// and at which origin. Production is the SAME [`CredentialBroker`] provider
/// construction uses, so a search shares its refresh single-flight; laws
/// stub it with a vault-minted handle.
#[async_trait::async_trait]
pub(crate) trait WebSearchCredentials: Send + Sync {
    /// `(base_url override, bearer)` for the active subscription account.
    async fn subscription_bearer(&self) -> Result<(Option<String>, SecretHandle), String>;
}

#[async_trait::async_trait]
impl WebSearchCredentials for CredentialBroker {
    async fn subscription_bearer(&self) -> Result<(Option<String>, SecretHandle), String> {
        let account = self
            .resolve_account(OPENAI_OAUTH_PROVIDER_NAME, None)
            .await
            .map_err(|error| error.message)?;
        let base_url = account.descriptor.base_url.clone();
        let bearer = self
            .resolve(&account.descriptor)
            .await
            .map_err(|error| error.message)?;
        Ok((base_url, bearer))
    }
}

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
    transport: crate::http_transport::SharedHttpTransport,
}

impl ReqwestWebSearchHttp {
    pub(crate) fn new() -> Self {
        Self {
            transport: crate::http_transport::SharedHttpTransport,
        }
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
        let Some(client) = self.transport.client() else {
            return Err("search transport is unavailable".into());
        };
        let token = std::str::from_utf8(bearer.expose_secret())
            .map_err(|_| "subscription credential is not valid UTF-8".to_owned())?;
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "subscription credential is not header-safe".to_owned())?;
        authorization.set_sensitive(true);
        let response = client
            .post(url)
            .timeout(Duration::from_secs(45))
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
        // H4: stream the body under a hard cap instead of buffering it whole.
        let bytes = read_body_capped(response, SEARCH_RESPONSE_CAP_BYTES).await?;
        Ok((status, bytes))
    }
}

/// The client `web_search` executor for subscription pairs.
pub(crate) struct SubscriptionWebSearch {
    credentials: Arc<dyn WebSearchCredentials>,
    http: Arc<dyn WebSearchHttp>,
}

impl SubscriptionWebSearch {
    pub(crate) fn new(
        credentials: Arc<dyn WebSearchCredentials>,
        http: Arc<dyn WebSearchHttp>,
    ) -> Self {
        Self { credentials, http }
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
        let (base_url, bearer) =
            self.credentials
                .subscription_bearer()
                .await
                .map_err(|message| WebSearchFailure {
                    message,
                    degraded: false,
                })?;
        let url = codex_alpha_search_url(base_url.as_deref());
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

#[cfg(test)]
mod web_search_tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use haider_accounts::{MemoryVault, Vault};
    use haider_protocol::ids::CredentialAlias;
    use std::sync::Mutex;

    /// Mints a real `SecretHandle` through the vault seam (the only mint).
    fn secret(bytes: &[u8]) -> SecretHandle {
        let vault = MemoryVault::default();
        let alias = CredentialAlias::new("mint");
        vault.put(&alias, bytes).expect("put");
        vault.resolve(&alias).expect("resolve")
    }

    struct StubCredentials {
        base_url: Option<String>,
    }

    #[async_trait::async_trait]
    impl WebSearchCredentials for StubCredentials {
        async fn subscription_bearer(&self) -> Result<(Option<String>, SecretHandle), String> {
            Ok((self.base_url.clone(), secret(b"SUBSCRIPTION_ACCESS_TOKEN")))
        }
    }

    struct MissingCredentials;

    #[async_trait::async_trait]
    impl WebSearchCredentials for MissingCredentials {
        async fn subscription_bearer(&self) -> Result<(Option<String>, SecretHandle), String> {
            Err("provider `openai-oauth` has no active credential".into())
        }
    }

    #[derive(Default)]
    struct StubHttp {
        posts: Mutex<Vec<(String, Vec<u8>, serde_json::Value)>>,
        reply: Mutex<Option<(u16, Vec<u8>)>>,
    }

    impl StubHttp {
        fn replying(status: u16, body: &str) -> Arc<Self> {
            Arc::new(Self {
                posts: Mutex::new(Vec::new()),
                reply: Mutex::new(Some((status, body.as_bytes().to_vec()))),
            })
        }

        fn posts(&self) -> Vec<(String, Vec<u8>, serde_json::Value)> {
            self.posts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl WebSearchHttp for StubHttp {
        async fn post_json(
            &self,
            url: &str,
            bearer: &SecretHandle,
            body: &serde_json::Value,
        ) -> Result<(u16, Vec<u8>), String> {
            self.posts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((
                    url.to_owned(),
                    bearer.expose_secret().to_vec(),
                    body.clone(),
                ));
            Ok(self
                .reply
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or((200, Vec::new())))
        }
    }

    fn executor(
        credentials: Arc<dyn WebSearchCredentials>,
        http: Arc<dyn WebSearchHttp>,
    ) -> SubscriptionWebSearch {
        SubscriptionWebSearch::new(credentials, http)
    }

    /// LAW (LW4 client half, request): one search is exactly ONE POST to
    /// `{subscription_base}/alpha/search`, under the SAME Bearer as turns,
    /// carrying the golden SearchRequest body; a credential's own base_url
    /// wins over the sanctioned default.
    #[tokio::test]
    async fn one_search_is_one_authenticated_post_of_the_golden_body() {
        let http = StubHttp::replying(200, r#"{"output_text":"the answer"}"#);
        let search = executor(
            Arc::new(StubCredentials { base_url: None }),
            Arc::clone(&http) as Arc<dyn WebSearchHttp>,
        );
        let text = search
            .search("gpt-5.6-sol", "session-9", "rust sse decoding")
            .await
            .expect("200 yields text");
        assert_eq!(text, "the answer");

        let posts = http.posts();
        assert_eq!(posts.len(), 1, "no retries: the tool result is the signal");
        assert_eq!(posts[0].0, haider_provider::OPENAI_ALPHA_SEARCH_URL);
        assert_eq!(posts[0].1, b"SUBSCRIPTION_ACCESS_TOKEN");
        assert_eq!(
            posts[0].2,
            haider_provider::codex_alpha_search_request_body(
                "session-9",
                "gpt-5.6-sol",
                "rust sse decoding"
            )
        );

        // A credential that pins its own origin is honored.
        let proxied = StubHttp::replying(200, "{}");
        let search = executor(
            Arc::new(StubCredentials {
                base_url: Some("https://proxy.internal/backend-api/codex/".into()),
            }),
            Arc::clone(&proxied) as Arc<dyn WebSearchHttp>,
        );
        search
            .search("m", "s", "q")
            .await
            .expect("proxied 200 yields text");
        assert_eq!(
            proxied.posts()[0].0,
            "https://proxy.internal/backend-api/codex/alpha/search"
        );
    }

    /// LAW (LW4 client half, honesty): the endpoint is UNOFFICIAL. 404/410
    /// mean GONE — the failure is `degraded`, which latches the session
    /// capability off. Every other non-2xx surfaces the provider's own body
    /// VERBATIM and leaves the capability alone, and a missing credential is
    /// a plain failure rather than a capability verdict.
    #[tokio::test]
    async fn gone_statuses_degrade_and_every_other_failure_surfaces_verbatim() {
        for status in [404, 410] {
            let search = executor(
                Arc::new(StubCredentials { base_url: None }),
                StubHttp::replying(status, "not found"),
            );
            let failure = search
                .search("m", "s", "q")
                .await
                .expect_err("a gone endpoint fails");
            assert!(failure.degraded, "HTTP {status} is the GONE verdict");
            assert!(
                failure.message.contains(&status.to_string()),
                "the status is named: {}",
                failure.message
            );
        }
        for status in [400, 401, 429, 500] {
            let search = executor(
                Arc::new(StubCredentials { base_url: None }),
                StubHttp::replying(status, r#"{"error":{"message":"upstream said no"}}"#),
            );
            let failure = search
                .search("m", "s", "q")
                .await
                .expect_err("a non-2xx fails");
            assert!(
                !failure.degraded,
                "HTTP {status} must not spend the capability"
            );
            assert!(
                failure.message.contains("upstream said no"),
                "the provider's body is verbatim: {}",
                failure.message
            );
        }
        let search = executor(Arc::new(MissingCredentials), StubHttp::replying(200, "{}"));
        let failure = search
            .search("m", "s", "q")
            .await
            .expect_err("no credential, no search");
        assert!(!failure.degraded);
        assert!(failure.message.contains("no active credential"));
    }

    /// LAW (W-F H4, bounded body read): the production transport STREAMS the
    /// search response under `SEARCH_RESPONSE_CAP_BYTES` rather than buffering
    /// the whole body (`response.bytes()`) — a runaway/compromised endpoint
    /// cannot exhaust memory before the 32 KiB downstream cap applies. Driven
    /// over a LOOPBACK server (never chatgpt.com): an oversized body is
    /// clamped to exactly the cap.
    #[tokio::test]
    async fn production_transport_caps_the_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let over = SEARCH_RESPONSE_CAP_BYTES + 64 * 1024;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                // Drain the (small) request head so the client's write unblocks.
                let mut scratch = vec![0u8; 4096];
                let _ = socket.read(&mut scratch).await;
                let body = "x".repeat(over);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let http = ReqwestWebSearchHttp::new();
        let (status, bytes) = http
            .post_json(
                &format!("http://{addr}/alpha/search"),
                &secret(b"TOKEN"),
                &serde_json::json!({"q": "hi"}),
            )
            .await
            .expect("loopback POST succeeds");
        assert_eq!(status, 200);
        assert_eq!(
            bytes.len(),
            SEARCH_RESPONSE_CAP_BYTES,
            "an oversized body is clamped to exactly the cap, never buffered whole"
        );
        server.abort();
    }

    /// The verbatim quote is BOUNDED and stays on a char boundary — an
    /// unofficial endpoint's error body is untrusted input, not a log sink.
    #[test]
    fn error_body_quotes_are_bounded_and_char_safe() {
        assert_eq!(quote_error_body(b"   "), "(empty response body)");
        assert_eq!(quote_error_body(b"  boom  "), "boom");
        let huge = "é".repeat(4 * 1024);
        let quoted = quote_error_body(huge.as_bytes());
        assert!(quoted.ends_with('…'));
        assert!(quoted.len() <= ERROR_BODY_QUOTE_BYTES + 8);
    }
}
