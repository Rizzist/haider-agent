//! Debug/test-only fake usage and reset server hook (same safety model as
//! `oauth::debug_fake_oauth`; see `docs/testing/fake-usage.md`).
//!
//! It lets the REAL debug `haiderd` exercise usage meters, the reset-credit
//! consume and the Haider Code plan poller against a local fake server, so
//! end-to-end proofs never contact a live provider. Rules, enforced here:
//! - compiled only under `cfg(debug_assertions)`: absent from release/ci builds;
//! - explicit opt-in: nothing changes unless [`ORIGIN_ENV`] is set, and it is
//!   honoured only while `HAIDER_TEST_FAKE_PROVIDER` is also set;
//! - numeric IPv4 loopback `http://127.x.y.z:<port>` only (no path, query,
//!   fragment or credentials). Any refused configuration disables usage
//!   networking in that process; it never falls back to the live endpoints;
//! - real credentials cannot be redirected: a bearer is sent to the fake only
//!   when it is a fake-minted credential (a JWT whose `iss` is the fake
//!   origin, or an opaque key with [`FAKE_KEY_PREFIX`]). Any other credential
//!   is refused locally and is sent nowhere;
//! - it announces `TEST MODE` on stderr once per process.

use std::ffi::OsStr;
use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use url::Url;
use zeroize::Zeroizing;

pub(crate) const ORIGIN_ENV: &str = "HAIDER_TEST_FAKE_USAGE_ORIGIN";
const REQUIRED_ENV: &str = "HAIDER_TEST_FAKE_PROVIDER";
/// Opaque (non-JWT) fake keys, e.g. a Haider Code API key, must carry this.
pub(crate) const FAKE_KEY_PREFIX: &str = "hk-haider-test-fake-";

/// Live control-plane hosts whose usage/reset/plan paths the fake serves.
const FAKEABLE_HOSTS: [&str; 5] = [
    "chatgpt.com",
    "haidercode.ai",
    "api.anthropic.com",
    "api.kimi.com",
    "cli-chat-proxy.grok.com",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Refused(&'static str),
    Fake(Url),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route<'a> {
    Live(&'a str),
    Fake(String),
    Refused(&'static str),
}

/// Pure resolution used by the process-wide [`mode`] and by tests.
pub(crate) fn resolve(fake_provider_set: bool, origin: Option<&OsStr>) -> Mode {
    let Some(origin) = origin else {
        return Mode::Off;
    };
    if !fake_provider_set {
        return Mode::Refused("test_mode_requires_fake_provider");
    }
    match origin.to_str().and_then(parse_origin) {
        Some(origin) => Mode::Fake(origin),
        None => Mode::Refused("test_mode_origin_invalid"),
    }
}

fn mode() -> &'static Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    MODE.get_or_init(|| {
        let mode = resolve(
            std::env::var_os(REQUIRED_ENV).is_some(),
            std::env::var_os(ORIGIN_ENV).as_deref(),
        );
        match &mode {
            Mode::Off => {}
            Mode::Refused(reason) => eprintln!(
                "haiderd: {ORIGIN_ENV} refused ({reason}); usage, reset and plan requests are disabled \
                 (requires {REQUIRED_ENV} and an http://127.x.y.z:<port> origin)"
            ),
            Mode::Fake(origin) => eprintln!(
                "haiderd: TEST MODE — usage meters, reset consume and the Haider Code plan use the fake server at {}",
                origin.as_str().trim_end_matches('/')
            ),
        }
        mode
    })
}

/// Resolves (and announces) the mode at daemon startup.
pub(crate) fn announce() {
    let _ = mode();
}

/// `Some(origin)` only for an `http://<ipv4 loopback>:<port>` value; the
/// explicit port is read from the raw input (`Url` drops a default port).
pub(crate) fn parse_origin(value: &str) -> Option<Url> {
    let rest = value.strip_prefix("http://")?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    let (host, port) = authority.split_once(':')?;
    let ip = host.parse::<std::net::Ipv4Addr>().ok()?;
    let port_is_explicit = !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0);
    if !ip.is_loopback() || !port_is_explicit {
        return None;
    }
    let url = Url::parse(value).ok()?;
    let parsed_loopback = matches!(url.host(), Some(url::Host::Ipv4(parsed)) if parsed == ip);
    (url.scheme() == "http"
        && parsed_loopback
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

/// The fake-origin URL for one live usage/plan URL (path and query kept).
pub(crate) fn rewrite(live: &str, origin: &Url) -> Option<String> {
    let live = Url::parse(live).ok()?;
    if live.scheme() != "https"
        || !live
            .host_str()
            .is_some_and(|host| FAKEABLE_HOSTS.contains(&host))
    {
        return None;
    }
    let mut fake = origin.join(live.path()).ok()?;
    fake.set_query(live.query());
    Some(fake.into())
}

/// True only for fake-minted credentials bound to this fake origin.
pub(crate) fn bearer_is_fake(token: &[u8], origin: &Url) -> bool {
    let Ok(token) = std::str::from_utf8(token) else {
        return false;
    };
    if token.starts_with(FAKE_KEY_PREFIX) {
        return true;
    }
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let mut decoded = Zeroizing::new(Vec::new());
    if URL_SAFE_NO_PAD
        .decode_vec(payload.as_bytes(), &mut decoded)
        .is_err()
    {
        return false;
    }
    #[derive(serde::Deserialize)]
    struct Issuer {
        iss: Option<String>,
    }
    serde_json::from_slice::<Issuer>(&decoded)
        .ok()
        .and_then(|claims| claims.iss)
        .is_some_and(|iss| iss.trim_end_matches('/') == origin.as_str().trim_end_matches('/'))
}

/// Pure routing decision for one request.
pub(crate) fn route_with<'a>(mode: &Mode, live: &'a str, bearer: &[u8]) -> Route<'a> {
    match mode {
        Mode::Off => Route::Live(live),
        Mode::Refused(reason) => Route::Refused(reason),
        Mode::Fake(origin) => {
            if !bearer_is_fake(bearer, origin) {
                return Route::Refused("test_mode_real_credential_refused");
            }
            match rewrite(live, origin) {
                Some(fake) => Route::Fake(fake),
                None => Route::Refused("test_mode_endpoint_not_fakeable"),
            }
        }
    }
}

/// Routing for the process-wide mode.
pub(crate) fn route<'a>(live: &'a str, bearer: &[u8]) -> Route<'a> {
    route_with(mode(), live, bearer)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn jwt(iss: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"iss":"{iss}","exp":4102444800}}"#));
        format!("{header}.{payload}.sig")
    }

    /// MUTATION CHECK: accept `localhost`, a non-loopback IPv4, https, or a
    /// path; one of these rows then parses.
    #[test]
    fn origin_is_numeric_loopback_only() {
        assert!(parse_origin("http://127.0.0.1:43123").is_some());
        assert!(parse_origin("http://127.0.0.1:43123/").is_some());
        for bad in [
            "http://localhost:43123",
            "http://10.0.0.1:43123",
            "https://127.0.0.1:43123",
            "http://127.0.0.1",
            "http://127.0.0.1:80/v1",
            "http://user:pw@127.0.0.1:43123",
            "http://127.0.0.1:43123/?q=1",
            "http://127.0.0.1:0",
            "http://chatgpt.com:443",
        ] {
            assert!(parse_origin(bad).is_none(), "{bad} must be refused");
        }
    }

    /// MUTATION CHECK: honour the origin without `HAIDER_TEST_FAKE_PROVIDER`
    /// or fall back to live on an invalid origin.
    #[test]
    fn opt_in_requires_fake_provider_and_never_falls_back_to_live() {
        assert_eq!(resolve(false, None), Mode::Off);
        assert_eq!(resolve(true, None), Mode::Off);
        assert_eq!(
            resolve(false, Some(OsStr::new("http://127.0.0.1:43123"))),
            Mode::Refused("test_mode_requires_fake_provider")
        );
        assert_eq!(
            resolve(true, Some(OsStr::new("https://chatgpt.com"))),
            Mode::Refused("test_mode_origin_invalid")
        );
        let refused = Mode::Refused("test_mode_origin_invalid");
        assert_eq!(
            route_with(
                &refused,
                haider_provider::OPENAI_OAUTH_RESET_CONSUME_URL,
                jwt("x").as_bytes()
            ),
            Route::Refused("test_mode_origin_invalid")
        );
    }

    /// MUTATION CHECK: drop the issuer/prefix guard; a live-issued JWT or a
    /// real-looking Haider Code key is then redirected to the fake server.
    #[test]
    fn real_credentials_are_never_redirected() {
        let origin = parse_origin("http://127.0.0.1:43123").expect("origin");
        let mode = Mode::Fake(origin.clone());
        let live_jwt = jwt("https://auth.openai.com");
        for real in [
            live_jwt.as_str(),
            "hk-live-0123456789abcdef",
            "sk-ant-oat01-real",
            "not.a.jwt.token",
            "",
        ] {
            assert_eq!(
                route_with(
                    &mode,
                    haider_provider::OPENAI_OAUTH_USAGE_URL,
                    real.as_bytes()
                ),
                Route::Refused("test_mode_real_credential_refused"),
                "{real:?} must not be sent"
            );
        }
        let fake_jwt = jwt("http://127.0.0.1:43123");
        assert!(bearer_is_fake(fake_jwt.as_bytes(), &origin));
        assert!(!bearer_is_fake(
            jwt("http://127.0.0.1:43124").as_bytes(),
            &origin
        ));
        assert!(bearer_is_fake(
            format!("{FAKE_KEY_PREFIX}plan").as_bytes(),
            &origin
        ));
    }

    /// MUTATION CHECK: keep the live host, drop the query, or rewrite an
    /// arbitrary (non-usage) host.
    #[test]
    fn live_usage_urls_rewrite_to_the_fake_origin_only() {
        let origin = parse_origin("http://127.0.0.1:43123").expect("origin");
        let mode = Mode::Fake(origin.clone());
        let fake_jwt = jwt("http://127.0.0.1:43123");
        assert_eq!(
            route_with(
                &mode,
                haider_provider::OPENAI_OAUTH_RESET_CONSUME_URL,
                fake_jwt.as_bytes()
            ),
            Route::Fake(
                "http://127.0.0.1:43123/backend-api/wham/rate-limit-reset-credits/consume".into()
            )
        );
        assert_eq!(
            rewrite(haider_provider::HAIDER_CODE_ACCOUNT_URL, &origin).as_deref(),
            Some("http://127.0.0.1:43123/v1/account")
        );
        assert_eq!(
            rewrite(
                "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
                &origin
            )
            .as_deref(),
            Some("http://127.0.0.1:43123/v1/billing?format=credits")
        );
        assert_eq!(rewrite("https://example.com/v1/account", &origin), None);
        assert_eq!(
            rewrite("http://chatgpt.com/backend-api/wham/usage", &origin),
            None
        );
        assert_eq!(
            route_with(&Mode::Off, "https://chatgpt.com/x", b"anything"),
            Route::Live("https://chatgpt.com/x")
        );
    }
}
