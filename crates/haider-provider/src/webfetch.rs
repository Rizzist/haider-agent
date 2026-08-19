//! W-B local `web_fetch` engine.
//!
//! Model-supplied URLs are HOSTILE INPUT, so every hop passes a strict-public
//! origin fence built on the G4a pinned-resolver pattern: HTTPS may target
//! PUBLIC addresses only, plain HTTP may target LOOPBACK only, and hostname
//! answers are resolved first, classified in full, and PINNED onto the
//! connection so a DNS rebind between check and connect cannot re-aim the
//! request. Redirects are followed manually (cap
//! [`WEB_FETCH_MAX_REDIRECTS`]) with the SAME validation re-run per hop.
//! Bodies are content-type gated (`text/*` + `application/json`), HTML is
//! reduced to readable text by a small in-crate reducer, and output is
//! capped at [`WEB_FETCH_OUTPUT_CAP_BYTES`] with an honest truncation marker.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::origin::{FixedDnsResolver, SystemFixedDnsResolver, blocked_public_web_target};
use crate::{ProviderError, ProviderErrorKind};

/// Maximum redirect hops followed by one fetch.
pub const WEB_FETCH_MAX_REDIRECTS: usize = 5;
/// Hard cap on the text handed back to the tool result.
pub const WEB_FETCH_OUTPUT_CAP_BYTES: usize = 96 * 1024;
/// Hard cap on raw bytes read off the wire before reduction.
const WEB_FETCH_SOURCE_CAP_BYTES: usize = 4 * 1024 * 1024;
/// Max nested DROP_CONTENT elements tracked at once (H2). A closing tag scans
/// this stack (`rposition`), so an UNBOUNDED stack makes `<script>`×N then
/// `</style>`×N (each a full scan that matches nothing) O(N²) within the 4 MiB
/// source cap → CPU exhaustion. Bounding the depth keeps every close O(1);
/// opens past the cap are ignored (their content is still dropped while the
/// stack is non-empty) — this is a reduction of hostile input, not a fidelity
/// contract, and legitimate documents never nest drop-chrome this deep.
const MAX_DROP_STACK_DEPTH: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// M6 (W-F): whole-fetch WALL-CLOCK ceiling across ALL hops and chunks. The
/// per-chunk idle timeout resets every chunk, so a 1-byte-per-29-seconds drip
/// (slowloris) never trips it; this absolute deadline aborts the entire fetch
/// no matter how the bytes are paced.
const WEB_FETCH_TOTAL_DEADLINE: Duration = Duration::from_secs(120);

/// The bounded, reduced result of one guarded fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFetchOutcome {
    /// The URL that finally answered (after redirects).
    pub final_url: String,
    /// The declared media type (parameters stripped).
    pub content_type: String,
    /// Reduced, bounded text.
    pub text: String,
    /// Whether the source or the reduced output was cut at a cap.
    pub truncated: bool,
}

/// Result of the production GET retry seam. `attempts` is always one or two
/// and is carried to the UI so recovery is visible rather than implicit.
#[derive(Debug)]
pub struct WebFetchExecution {
    pub outcome: Result<WebFetchOutcome, ProviderError>,
    pub attempts: u8,
}

/// Fetches one model-supplied URL through the strict-public origin policy.
pub async fn fetch_public_url(
    url: &str,
    max_bytes: Option<u64>,
) -> Result<WebFetchOutcome, ProviderError> {
    fetch_public_url_inner(
        url,
        max_bytes,
        Arc::new(SystemFixedDnsResolver),
        WEB_FETCH_TOTAL_DEADLINE,
        None,
    )
    .await
}

/// Fetches an idempotent public GET with exactly one transient retry.
/// Origin, MIME, and non-retryable HTTP refusals are never retried.
pub async fn fetch_public_url_with_one_retry(
    url: &str,
    max_bytes: Option<u64>,
) -> WebFetchExecution {
    bounded_get_retry(|| fetch_public_url(url, max_bytes)).await
}

/// Host-scoped variant (Loom B2): EVERY hop of the redirect chain must stay
/// on one of the granted hosts — a redirect that leaves the scope is a
/// refusal, never a silent escape past a use-site check on hop 0 alone.
pub async fn fetch_public_url_scoped_with_one_retry(
    url: &str,
    max_bytes: Option<u64>,
    allowed_hosts: &[String],
) -> WebFetchExecution {
    bounded_get_retry(|| {
        fetch_public_url_inner(
            url,
            max_bytes,
            Arc::new(SystemFixedDnsResolver),
            WEB_FETCH_TOTAL_DEADLINE,
            Some(allowed_hosts),
        )
    })
    .await
}

async fn bounded_get_retry<F, Fut>(mut fetch: F) -> WebFetchExecution
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<WebFetchOutcome, ProviderError>>,
{
    let first = fetch().await;
    if !first.as_ref().is_err_and(transient_get_failure) {
        return WebFetchExecution {
            outcome: first,
            attempts: 1,
        };
    }
    WebFetchExecution {
        outcome: fetch().await,
        attempts: 2,
    }
}

fn transient_get_failure(error: &ProviderError) -> bool {
    error.retryable
        && matches!(
            error.kind,
            ProviderErrorKind::Transport | ProviderErrorKind::StreamInterrupted
        )
}

/// Resolver-injectable variant of [`fetch_public_url`] (tests use stub
/// resolvers plus loopback listeners — never the real network).
pub async fn fetch_public_url_with_resolver(
    url: &str,
    max_bytes: Option<u64>,
    resolver: Arc<dyn FixedDnsResolver>,
) -> Result<WebFetchOutcome, ProviderError> {
    fetch_public_url_inner(url, max_bytes, resolver, WEB_FETCH_TOTAL_DEADLINE, None).await
}

/// Deadline-injectable variant (M6): laws drive a slow-drip loopback server
/// against a SHORT overall deadline to prove the whole-fetch wall-clock abort;
/// the public entry points use [`WEB_FETCH_TOTAL_DEADLINE`].
pub async fn fetch_public_url_with_deadline(
    url: &str,
    max_bytes: Option<u64>,
    total_deadline: Duration,
) -> Result<WebFetchOutcome, ProviderError> {
    fetch_public_url_inner(
        url,
        max_bytes,
        Arc::new(SystemFixedDnsResolver),
        total_deadline,
        None,
    )
    .await
}

async fn fetch_public_url_inner(
    url: &str,
    max_bytes: Option<u64>,
    resolver: Arc<dyn FixedDnsResolver>,
    total_deadline: Duration,
    allowed_hosts: Option<&[String]>,
) -> Result<WebFetchOutcome, ProviderError> {
    // M6: one ABSOLUTE deadline anchors every hop and chunk read below.
    let deadline = tokio::time::Instant::now() + total_deadline;
    let mut current = reqwest::Url::parse(url)
        .map_err(|error| refused(format!("web_fetch URL does not parse: {error}")))?;
    let output_cap = max_bytes
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|bytes| *bytes > 0)
        .map_or(WEB_FETCH_OUTPUT_CAP_BYTES, |bytes| {
            bytes.min(WEB_FETCH_OUTPUT_CAP_BYTES)
        });
    // H1 downgrade fence: once the chain's FIRST hop is a public host, no
    // later hop may be redirected onto loopback/private/link-local. `None`
    // until the origin is classified on hop 0.
    let mut chain_started_public: Option<bool> = None;
    for _hop in 0..=WEB_FETCH_MAX_REDIRECTS {
        // Loom B2: the granted-host scope holds on EVERY hop — the target of
        // each redirect re-enters this check before any connection opens.
        if let Some(hosts) = allowed_hosts {
            let host = current.host_str().unwrap_or_default();
            if !hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host))
            {
                return Err(refused(format!(
                    "web_fetch host `{host}` is outside this agent's granted APIs"
                )));
            }
        }
        let forbid_downgrade = chain_started_public == Some(true);
        let target = validate_fetch_target(&current, resolver.as_ref(), forbid_downgrade).await?;
        chain_started_public.get_or_insert(target.is_public);
        let response = open_response(&current, &target, deadline).await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    refused(format!(
                        "web_fetch redirect from {current} carries no usable Location"
                    ))
                })?;
            // The next hop re-enters the SAME guard on the next loop pass —
            // per-hop re-validation is the law (LW6), never a one-time check.
            current = current.join(location).map_err(|error| {
                refused(format!("web_fetch redirect target does not parse: {error}"))
            })?;
            continue;
        }
        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            let kind = if matches!(code, 408 | 425 | 429) || code >= 500 {
                ProviderErrorKind::Transport
            } else {
                ProviderErrorKind::InvalidRequest
            };
            return Err(ProviderError::new(
                kind,
                format!("web_fetch {current} returned HTTP {code}"),
            )
            .with_http_metadata(code, None));
        }
        let content_type = declared_media_type(&response)?;
        let (bytes, source_truncated) = read_body_bounded(response, deadline).await?;
        let text = if content_type == "text/html" {
            reduce_html_to_text(&String::from_utf8_lossy(&bytes))
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let (text, output_truncated) = cap_output(text, output_cap);
        let truncated = source_truncated || output_truncated;
        let text = if truncated {
            format!("{text}\n[web_fetch: output truncated at {output_cap} bytes]")
        } else {
            text
        };
        return Ok(WebFetchOutcome {
            final_url: current.to_string(),
            content_type,
            text,
            truncated,
        });
    }
    Err(refused(format!(
        "web_fetch exceeded {WEB_FETCH_MAX_REDIRECTS} redirects"
    )))
}

/// One validated hop: the effective host/port plus the pinned addresses a
/// hostname resolved to (`None` for IP-literal hosts — nothing to pin).
pub(crate) struct ValidatedFetchTarget {
    host: String,
    pinned: Option<Vec<SocketAddr>>,
    /// TRUE iff EVERY validated address for this hop is a public target.
    /// The caller records the first hop's value as the chain origin and
    /// forbids a later public→non-public downgrade (H1).
    is_public: bool,
}

/// The strict-public origin fence, applied to EVERY hop (LW6):
/// - `https` may target PUBLIC addresses only;
/// - `http` may target LOOPBACK only;
/// - RFC1918, link-local (cloud metadata), CGNAT, ULA, multicast,
///   unspecified, broadcast, and 0/8 are refused under BOTH schemes;
/// - userinfo never rides a fetch;
/// - hostnames resolve first, EVERY answer must pass, and the answers are
///   pinned onto the connection (DNS-rebinding defense);
/// - H1 downgrade fence: when `forbid_public_downgrade` is set (the chain
///   STARTED from a public host), a target that resolves to a non-public
///   address (loopback/private/link-local) is REFUSED — an approved public
///   fetch can never be bounced by a 302 onto `127.0.0.1:<port>`.
pub(crate) async fn validate_fetch_target(
    url: &reqwest::Url,
    resolver: &dyn FixedDnsResolver,
    forbid_public_downgrade: bool,
) -> Result<ValidatedFetchTarget, ProviderError> {
    let scheme = url.scheme();
    if scheme != "https" && scheme != "http" {
        return Err(refused(format!(
            "web_fetch supports only http(s) URLs, not `{scheme}`"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refused("web_fetch URLs must not carry userinfo"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| refused("web_fetch URL has no host"))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| refused("web_fetch URL has no usable port"))?;
    let literal = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(&host)
        .parse::<IpAddr>()
        .ok();
    let (addresses, pinned) = match literal {
        Some(address) => (vec![SocketAddr::new(address, port)], None),
        None => {
            let answers = resolver.resolve(&host, port).await.map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::Transport,
                    format!("web_fetch could not resolve host `{host}`: {error}"),
                )
            })?;
            if answers.is_empty() {
                return Err(refused(format!(
                    "web_fetch host `{host}` resolved to no addresses"
                )));
            }
            (answers.clone(), Some(answers))
        }
    };
    let mut all_public = true;
    for address in &addresses {
        let ip = address.ip();
        let public = !blocked_public_web_target(ip);
        if scheme == "https" {
            if blocked_public_web_target(ip) {
                return Err(refused(format!(
                    "web_fetch refuses non-public target {ip} for {url}"
                )));
            }
        } else if !ip.is_loopback() {
            return Err(refused(format!(
                "web_fetch allows plain HTTP only to loopback, not {ip} for {url}"
            )));
        }
        // H1 downgrade fence: a chain that started PUBLIC may not be redirected
        // onto a loopback/private/link-local target (SSRF via 302).
        if forbid_public_downgrade && !public {
            return Err(refused(format!(
                "web_fetch refuses redirect onto non-public target {ip} for {url}: a chain that started from a public host cannot be downgraded to loopback/private/link-local"
            )));
        }
        all_public &= public;
    }
    Ok(ValidatedFetchTarget {
        host,
        pinned,
        is_public: all_public,
    })
}

async fn open_response(
    url: &reqwest::Url,
    target: &ValidatedFetchTarget,
    deadline: tokio::time::Instant,
) -> Result<reqwest::Response, ProviderError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONNECT_TIMEOUT);
    if let Some(pinned) = &target.pinned {
        // The validated answers are the ONLY addresses this client may dial
        // for the host — a rebind between validation and connect dead-ends.
        builder = builder.resolve_to_addrs(&target.host, pinned);
    }
    let client = builder.build().map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            format!("web_fetch could not construct its HTTP client: {error}"),
        )
    })?;
    let opening = client
        .get(url.clone())
        .header(
            reqwest::header::ACCEPT,
            "text/html, application/json;q=0.9, text/*;q=0.8",
        )
        .send();
    // M6: bound the open by whichever comes first — the per-open timeout or
    // the whole-fetch deadline.
    within_fetch_deadline(opening, deadline, RESPONSE_OPEN_TIMEOUT, || {
        ProviderError::new(
            ProviderErrorKind::Transport,
            format!(
                "web_fetch response did not open within {} seconds",
                RESPONSE_OPEN_TIMEOUT.as_secs()
            ),
        )
    })
    .await?
    .map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Transport,
            format!("web_fetch transport failed: {error}"),
        )
    })
}

/// The typed abort for the whole-fetch wall-clock deadline (M6). Phrased
/// without the second count because the deadline is injectable for laws.
fn overall_deadline_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        "web_fetch exceeded its overall fetch deadline".to_owned(),
    )
}

/// Content-type gate (decision 5): `text/*` and `application/json` pass;
/// everything else — PDF included (deferred to W-D) — is a typed refusal.
fn declared_media_type(response: &reqwest::Response) -> Result<String, ProviderError> {
    let declared = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| refused("web_fetch response declares no Content-Type"))?;
    let media_type = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if media_type.starts_with("text/") || media_type == "application/json" {
        Ok(media_type)
    } else {
        Err(refused(format!(
            "web_fetch refuses content type `{media_type}` (text/* and application/json only)"
        )))
    }
}

async fn read_body_bounded(
    mut response: reqwest::Response,
    deadline: tokio::time::Instant,
) -> Result<(Vec<u8>, bool), ProviderError> {
    let mut body = Vec::new();
    loop {
        let Some(chunk) = next_chunk(&mut response, deadline).await? else {
            return Ok((body, false)); // Clean EOF: nothing was truncated.
        };
        // body.len() < cap here — the `== cap` branch always returns below.
        let remaining = WEB_FETCH_SOURCE_CAP_BYTES - body.len();
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true)); // More bytes than the cap can hold.
        }
        body.extend_from_slice(&chunk);
        if body.len() == WEB_FETCH_SOURCE_CAP_BYTES {
            // M9 off-by-one: a body that lands EXACTLY on the cap is only
            // truncated if MORE bytes follow — one honest extra read decides,
            // instead of blindly flagging truncation at the boundary.
            let more = next_chunk(&mut response, deadline).await?.is_some();
            return Ok((body, more));
        }
    }
}

/// One chunk read bounded by whichever fires first: the per-chunk idle timeout
/// or the whole-fetch wall-clock `deadline` (M6). A stall past the deadline is
/// the overall-deadline abort; a stall inside it is the per-chunk idle abort.
/// Returns the inferred chunk type unnamed — reqwest does not re-export it.
async fn next_chunk(
    response: &mut reqwest::Response,
    deadline: tokio::time::Instant,
    // `use<>`: the owned chunk captures no borrow of `response`, so the caller
    // may read the next chunk while a prior one is still in scope.
) -> Result<Option<impl std::ops::Deref<Target = [u8]> + use<>>, ProviderError> {
    within_fetch_deadline(response.chunk(), deadline, CHUNK_IDLE_TIMEOUT, || {
        ProviderError::new(
            ProviderErrorKind::Transport,
            format!(
                "web_fetch body stalled for {} seconds",
                CHUNK_IDLE_TIMEOUT.as_secs()
            ),
        )
    })
    .await?
    .map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Transport,
            format!("web_fetch body read failed: {error}"),
        )
    })
}

/// Runs `future` bounded by whichever fires first — `per_op` from now, or the
/// whole-fetch wall-clock `deadline` (M6). A timeout at/after the deadline is
/// the overall-deadline abort; a timeout inside it is `stalled` (the per-op
/// abort). Generic so the chunk type never needs naming.
async fn within_fetch_deadline<F: std::future::Future>(
    future: F,
    deadline: tokio::time::Instant,
    per_op: Duration,
    stalled: impl FnOnce() -> ProviderError,
) -> Result<F::Output, ProviderError> {
    let op_deadline = deadline.min(tokio::time::Instant::now() + per_op);
    tokio::time::timeout_at(op_deadline, future)
        .await
        .map_err(|_| {
            if tokio::time::Instant::now() >= deadline {
                overall_deadline_error()
            } else {
                stalled()
            }
        })
}

/// Cuts `text` at the byte cap on a character boundary.
fn cap_output(text: String, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = text;
    text.truncate(cut);
    (text, true)
}

/// Reduces an HTML document to readable text (decision 5): script/style/nav
/// and other chrome are DROPPED with their content, headings keep a `#`
/// prefix, list items a `-` marker, links keep their href, pre/code content
/// keeps its whitespace, and entities decode minimally. Deliberately small
/// and dependency-free — a reduction, not a rendering.
#[must_use]
pub fn reduce_html_to_text(html: &str) -> String {
    /// Elements whose entire CONTENT is dropped.
    const DROP_CONTENT: &[&str] = &[
        "script", "style", "noscript", "template", "svg", "head", "nav", "iframe", "object",
    ];
    /// Elements that force a line break around themselves.
    const BLOCK: &[&str] = &[
        "p",
        "div",
        "section",
        "article",
        "header",
        "footer",
        "main",
        "aside",
        "table",
        "tr",
        "ul",
        "ol",
        "blockquote",
        "form",
        "figure",
        "figcaption",
        "details",
        "summary",
    ];

    let mut output = String::new();
    let mut chars = html.char_indices().peekable();
    let mut drop_stack: Vec<String> = Vec::new();
    let mut pre_depth = 0usize;
    let mut pending_href: Option<String> = None;

    while let Some((index, character)) = chars.next() {
        if character != '<' {
            if drop_stack.is_empty() {
                push_text(&mut output, character, pre_depth > 0);
            }
            continue;
        }
        // Comments (and CDATA/doctype noise) skip to their terminator.
        let rest = &html[index..];
        if rest.starts_with("<!--") {
            let end = html[index..].find("-->").map(|offset| index + offset + 3);
            skip_until(&mut chars, end);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            let end = html[index..].find('>').map(|offset| index + offset + 1);
            skip_until(&mut chars, end);
            continue;
        }
        let Some(end) = tag_end(html, index) else {
            break; // Unterminated tag: everything after is markup noise.
        };
        let tag = &html[index + 1..end];
        skip_until(&mut chars, Some(end + 1));
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|character: char| character.is_ascii_whitespace() || character == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        if DROP_CONTENT.contains(&name.as_str()) {
            if closing {
                if let Some(position) = drop_stack.iter().rposition(|open| *open == name) {
                    drop_stack.truncate(position);
                }
            } else if !tag.ends_with('/') && drop_stack.len() < MAX_DROP_STACK_DEPTH {
                // H2: opens past the depth cap are IGNORED — the stack is still
                // non-empty so content stays dropped, but the O(1) close bound
                // holds. Never let a hostile page grow this scan unboundedly.
                drop_stack.push(name);
            }
            continue;
        }
        if !drop_stack.is_empty() {
            continue;
        }
        match name.as_str() {
            "br" => output.push('\n'),
            "pre" => {
                if closing {
                    pre_depth = pre_depth.saturating_sub(1);
                } else {
                    pre_depth += 1;
                }
                ensure_break(&mut output);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                ensure_break(&mut output);
                if !closing {
                    let level = name.as_bytes()[1] - b'0';
                    for _ in 0..level {
                        output.push('#');
                    }
                    output.push(' ');
                }
            }
            "li" => {
                if !closing {
                    ensure_break(&mut output);
                    output.push_str("- ");
                } else {
                    ensure_break(&mut output);
                }
            }
            "td" | "th" if closing && !output.ends_with(char::is_whitespace) => {
                output.push(' ');
            }
            "a" => {
                if closing {
                    if let Some(href) = pending_href.take()
                        && (href.starts_with("http://") || href.starts_with("https://"))
                    {
                        output.push_str(" (");
                        output.push_str(&href);
                        output.push(')');
                    }
                } else {
                    pending_href = attribute_value(tag, "href").map(decode_entities);
                }
            }
            _ if BLOCK.contains(&name.as_str()) => ensure_break(&mut output),
            _ => {}
        }
    }

    collapse_blank_runs(&output)
}

fn tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, character) in html[start..].char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            (None, '>') => return Some(start + offset),
            _ => {}
        }
    }
    None
}

fn skip_until(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, end: Option<usize>) {
    match end {
        Some(end) => {
            while chars.peek().is_some_and(|(index, _)| *index < end) {
                chars.next();
            }
        }
        None => while chars.next().is_some() {},
    }
}

fn attribute_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let position = lower.find(name)?;
    let rest = tag[position + name.len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let mut characters = rest.chars();
    match characters.next()? {
        quote @ ('"' | '\'') => {
            let rest = &rest[1..];
            let end = rest.find(quote)?;
            Some(rest[..end].to_owned())
        }
        _ => Some(
            rest.split(|character: char| character.is_ascii_whitespace() || character == '>')
                .next()
                .unwrap_or_default()
                .to_owned(),
        ),
    }
}

fn push_text(output: &mut String, character: char, preformatted: bool) {
    if preformatted {
        output.push(character);
        return;
    }
    if character.is_whitespace() {
        if !output.is_empty() && !output.ends_with(char::is_whitespace) {
            output.push(' ');
        }
        return;
    }
    if character == '&' {
        // Entities are decoded in a later pass over collected text; push the
        // raw character here — `collapse_blank_runs` runs `decode_entities`.
        output.push('&');
        return;
    }
    output.push(character);
}

fn ensure_break(output: &mut String) {
    while output.ends_with(' ') {
        output.pop();
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn decode_entities(text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find('&') {
        output.push_str(&rest[..position]);
        rest = &rest[position..];
        // H3: scan for the terminating ';' within the first 12 bytes WITHOUT
        // a raw byte slice — `rest[..12]` can fall mid-codepoint (`&aaaa…é;`)
        // and panic. `char_indices` never yields a non-boundary index, and
        // ';' is ASCII so its index equals its byte offset.
        let Some(end) = rest
            .char_indices()
            .take_while(|(index, _)| *index < 12)
            .find(|(_, character)| *character == ';')
            .map(|(index, _)| index)
        else {
            output.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|number| {
                    number.strip_prefix('x').map_or_else(
                        || number.parse::<u32>().ok(),
                        |hex| u32::from_str_radix(hex, 16).ok(),
                    )
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(decoded) => {
                output.push(decoded);
                rest = &rest[end + 1..];
            }
            None => {
                output.push('&');
                rest = &rest[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

fn collapse_blank_runs(text: &str) -> String {
    let decoded = decode_entities(text);
    let mut output = String::with_capacity(decoded.len());
    let mut blank_lines = 0usize;
    for line in decoded.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_lines += 1;
            if blank_lines > 1 {
                continue;
            }
        } else {
            blank_lines = 0;
        }
        output.push_str(trimmed);
        output.push('\n');
    }
    let trimmed = output.trim_matches('\n');
    trimmed.to_owned()
}

fn refused(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Loom B2 MUTATION CHECK: move the host-scope check out of the hop loop
    /// (back to hop 0 only) or drop it. Expected RUNTIME failure: a scoped
    /// fetch to a foreign host proceeds past the fence. The refusal fires
    /// BEFORE any resolution or connection — no network in this test.
    #[tokio::test]
    async fn scoped_fetch_refuses_hosts_outside_the_grant() {
        let hosts = vec!["api.example".to_owned()];
        let execution =
            fetch_public_url_scoped_with_one_retry("https://other.example/v1/thing", None, &hosts)
                .await;
        let error = execution.outcome.expect_err("foreign host must refuse");
        assert!(
            error
                .to_string()
                .contains("outside this agent's granted APIs"),
            "{error}"
        );
        assert!(!error.retryable, "a scope refusal is never retried");
        // Host comparison is case-insensitive; the scheme/port are not the
        // scope, the HOST is.
        let execution =
            fetch_public_url_scoped_with_one_retry("https://API.example:443/x", None, &hosts).await;
        assert!(
            !matches!(
                &execution.outcome,
                Err(error) if error.to_string().contains("granted APIs")
            ),
            "case-variant granted host must pass the fence"
        );
    }

    fn success() -> WebFetchOutcome {
        WebFetchOutcome {
            final_url: "https://example.test/".into(),
            content_type: "text/plain".into(),
            text: "ok".into(),
            truncated: false,
        }
    }

    /// E8 GET law: deleting the retry yields one call; looping on the second
    /// transient error yields more than two. Both mutations break this test.
    #[tokio::test]
    async fn idempotent_web_fetch_transient_failure_retries_exactly_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let execution = bounded_get_retry(|| {
            let calls = Arc::clone(&calls);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(ProviderError::new(
                        ProviderErrorKind::Transport,
                        "injected transient failure",
                    ))
                } else {
                    Ok(success())
                }
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(execution.attempts, 2);
        assert!(execution.outcome.is_ok());

        let calls = Arc::new(AtomicUsize::new(0));
        let execution = bounded_get_retry(|| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "injected non-retryable refusal",
                ))
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(execution.attempts, 1);
    }
}
