use std::io;
use std::net::SocketAddr;

use async_trait::async_trait;

use crate::ProviderErrorKind;
use crate::origin::FixedDnsResolver;
use crate::webfetch::{reduce_html_to_text, validate_fetch_target};

struct StubResolver {
    answers: Vec<SocketAddr>,
}

#[async_trait]
impl FixedDnsResolver for StubResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(self.answers.clone())
    }
}

async fn validate(url: &str, answers: Vec<SocketAddr>) -> Result<(), crate::ProviderError> {
    let url = reqwest::Url::parse(url).expect("test URL parses");
    let resolver = StubResolver { answers };
    validate_fetch_target(&url, &resolver).await.map(|_| ())
}

/// LAW (LW6, origin matrix — BOTH directions): public HTTPS passes (IP
/// literal and resolved-hostname forms alike); public plain-HTTP, RFC1918,
/// link-local/metadata, CGNAT, ULA, loopback-over-HTTPS-only rules, mixed
/// DNS answers, foreign schemes, and userinfo are refused. Loopback is the
/// ONE plain-HTTP allowance.
#[tokio::test]
async fn origin_matrix_allows_public_and_refuses_hostile_targets_both_directions() {
    let public = SocketAddr::from(([93, 184, 216, 34], 443));

    // ALLOWED direction.
    validate("https://example.com/doc", vec![public])
        .await
        .expect("public https hostname passes");
    validate("https://93.184.216.34/doc", Vec::new())
        .await
        .expect("public https IP literal passes");
    validate("http://127.0.0.1:8080/doc", Vec::new())
        .await
        .expect("loopback plain http passes");
    validate("http://[::1]:8080/doc", Vec::new())
        .await
        .expect("IPv6 loopback plain http passes");

    // REFUSED direction: every hostile class, under both address forms.
    let refused_literals = [
        "http://93.184.216.34/",       // public plain http
        "http://example.com/",         // public plain http via hostname (below)
        "https://10.23.45.67/",        // RFC1918
        "https://172.16.45.67/",       // RFC1918
        "https://192.168.1.10/",       // RFC1918
        "https://169.254.169.254/",    // link-local / cloud metadata
        "https://100.100.100.200/",    // CGNAT shared space (metadata twin)
        "https://127.0.0.1/",          // loopback is NOT public
        "https://[::1]/",              // IPv6 loopback
        "https://[fe80::1]/",          // IPv6 link-local
        "https://[fc00::1]/",          // IPv6 ULA
        "https://0.0.0.0/",            // unspecified
        "ftp://example.com/file",      // foreign scheme
        "https://user:pw@example.com", // userinfo
    ];
    for url in refused_literals {
        let answers = vec![public];
        let error = validate(url, answers)
            .await
            .expect_err(&format!("{url} must be refused"));
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{url}");
    }

    // Hostname answers are classified in FULL: one hostile answer poisons
    // the set even when a public one rides beside it (rebind defense).
    let error = validate(
        "https://rebind.example.com/",
        vec![public, SocketAddr::from(([10, 0, 0, 8], 443))],
    )
    .await
    .expect_err("mixed public+private answers must be refused");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    for hostile in [
        vec![SocketAddr::from(([192, 168, 0, 2], 443))],
        vec![SocketAddr::from(([169, 254, 169, 254], 443))],
        Vec::new(),
    ] {
        validate("https://evil.example.com/", hostile)
            .await
            .expect_err("hostile or empty answers must be refused");
    }
    // A hostname resolving to loopback is refused over https (loopback is
    // the plain-http allowance, not a public target)…
    validate(
        "https://localhost.example.com/",
        vec![SocketAddr::from(([127, 0, 0, 1], 443))],
    )
    .await
    .expect_err("loopback answers are not public");
    // …and allowed over plain http.
    validate(
        "http://local.example.com:8080/",
        vec![SocketAddr::from(([127, 0, 0, 1], 8080))],
    )
    .await
    .expect("loopback-resolving hostname passes over plain http");
}

/// LAW (LW6, html reduction): script/style/nav CONTENT is dropped, headings
/// keep a `#` prefix, list items a `-` marker, links keep their href,
/// pre/code keeps its shape, entities decode, and blank runs collapse.
#[test]
fn html_reduction_drops_script_style_nav_and_keeps_readable_structure() {
    let html = r#"<!DOCTYPE html><html><head><title>ignored</title>
<style>body { color: red; }</style></head><body>
<nav><a href="/home">chrome link</a></nav>
<h1>Main &amp; Title</h1>
<p>First paragraph with a <a href="https://example.com/ref">reference</a>.</p>
<script>alert("evil payload");</script>
<ul><li>alpha</li><li>beta</li></ul>
<pre><code>let x = 1;
let y = 2;</code></pre>
<p>Tail&nbsp;text</p>
<!-- a comment -->
</body></html>"#;
    let reduced = reduce_html_to_text(html);

    assert!(!reduced.contains("color: red"), "style content dropped");
    assert!(!reduced.contains("evil payload"), "script content dropped");
    assert!(
        !reduced.contains("alert"),
        "script content dropped entirely"
    );
    assert!(!reduced.contains("chrome link"), "nav content dropped");
    assert!(!reduced.contains("ignored"), "head content dropped");
    assert!(!reduced.contains("<p>"), "no markup survives");
    assert!(
        reduced.contains("# Main & Title"),
        "h1 keeps a heading marker and entities decode: {reduced}"
    );
    assert!(
        reduced.contains("reference (https://example.com/ref)"),
        "links keep their href: {reduced}"
    );
    assert!(reduced.contains("- alpha"), "list items marked: {reduced}");
    assert!(reduced.contains("- beta"), "list items marked: {reduced}");
    assert!(
        reduced.contains("let x = 1;\nlet y = 2;"),
        "pre/code keeps its line shape: {reduced}"
    );
    assert!(
        reduced.contains("Tail text"),
        "nbsp decodes to a space: {reduced}"
    );
    assert!(
        !reduced.contains("\n\n\n"),
        "blank runs collapse: {reduced:?}"
    );
}
