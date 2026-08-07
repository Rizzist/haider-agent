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
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Fetches one model-supplied URL through the strict-public origin policy.
pub async fn fetch_public_url(
    url: &str,
    max_bytes: Option<u64>,
) -> Result<WebFetchOutcome, ProviderError> {
    fetch_public_url_with_resolver(url, max_bytes, Arc::new(SystemFixedDnsResolver)).await
}

/// Resolver-injectable variant of [`fetch_public_url`] (tests use stub
/// resolvers plus loopback listeners — never the real network).
pub async fn fetch_public_url_with_resolver(
    url: &str,
    max_bytes: Option<u64>,
    resolver: Arc<dyn FixedDnsResolver>,
) -> Result<WebFetchOutcome, ProviderError> {
    let mut current = reqwest::Url::parse(url)
        .map_err(|error| refused(format!("web_fetch URL does not parse: {error}")))?;
    let output_cap = max_bytes
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|bytes| *bytes > 0)
        .map_or(WEB_FETCH_OUTPUT_CAP_BYTES, |bytes| {
            bytes.min(WEB_FETCH_OUTPUT_CAP_BYTES)
        });
    for _hop in 0..=WEB_FETCH_MAX_REDIRECTS {
        let target = validate_fetch_target(&current, resolver.as_ref()).await?;
        let response = open_response(&current, &target).await?;
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
            return Err(ProviderError::new(
                ProviderErrorKind::Transport,
                format!("web_fetch {current} returned HTTP {}", status.as_u16()),
            ));
        }
        let content_type = declared_media_type(&response)?;
        let (bytes, source_truncated) = read_body_bounded(response).await?;
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
}

/// The strict-public origin fence, applied to EVERY hop (LW6):
/// - `https` may target PUBLIC addresses only;
/// - `http` may target LOOPBACK only;
/// - RFC1918, link-local (cloud metadata), CGNAT, ULA, multicast,
///   unspecified, broadcast, and 0/8 are refused under BOTH schemes;
/// - userinfo never rides a fetch;
/// - hostnames resolve first, EVERY answer must pass, and the answers are
///   pinned onto the connection (DNS-rebinding defense).
pub(crate) async fn validate_fetch_target(
    url: &reqwest::Url,
    resolver: &dyn FixedDnsResolver,
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
    for address in &addresses {
        let ip = address.ip();
        if scheme == "https" {
            if blocked_public_web_target(ip) {
                return Err(refused(format!(
                    "web_fetch refuses non-public target {ip} for {url}"
                )));
            }
        } else if !ip.is_loopback() {
            return Err(refused(format!(
                "web_fetch allows plain HTTP only to loopback, not {ip}"
            )));
        }
    }
    Ok(ValidatedFetchTarget { host, pinned })
}

async fn open_response(
    url: &reqwest::Url,
    target: &ValidatedFetchTarget,
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
    tokio::time::timeout(RESPONSE_OPEN_TIMEOUT, opening)
        .await
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Transport,
                format!(
                    "web_fetch response did not open within {} seconds",
                    RESPONSE_OPEN_TIMEOUT.as_secs()
                ),
            )
        })?
        .map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Transport,
                format!("web_fetch transport failed: {error}"),
            )
        })
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
) -> Result<(Vec<u8>, bool), ProviderError> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::time::timeout(CHUNK_IDLE_TIMEOUT, response.chunk())
            .await
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Transport,
                    format!(
                        "web_fetch body stalled for {} seconds",
                        CHUNK_IDLE_TIMEOUT.as_secs()
                    ),
                )
            })?
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::Transport,
                    format!("web_fetch body read failed: {error}"),
                )
            })?;
        let Some(chunk) = chunk else {
            return Ok((body, false));
        };
        let remaining = WEB_FETCH_SOURCE_CAP_BYTES.saturating_sub(body.len());
        if chunk.len() >= remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
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
            } else if !tag.ends_with('/') {
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
            "td" | "th" => {
                if closing && !output.ends_with(char::is_whitespace) {
                    output.push(' ');
                }
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
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
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
