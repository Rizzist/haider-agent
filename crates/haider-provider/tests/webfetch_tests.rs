#![allow(clippy::expect_used)]

//! W-B local web_fetch end-to-end laws over LOOPBACK mock servers only —
//! no test in this file dials the real network (loopback plain HTTP is the
//! policy's one http allowance, which is exactly what makes these fetches
//! testable end to end).

use haider_provider::{
    ProviderErrorKind, WEB_FETCH_OUTPUT_CAP_BYTES, fetch_public_url, fetch_public_url_with_deadline,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves each accepted connection one canned HTTP/1.1 response chosen by
/// the request path. Returns the bound loopback base URL.
async fn spawn_loopback_server(
    routes: Vec<(&'static str, String)>,
) -> (String, tokio::task::JoinHandle<()>) {
    spawn_loopback_byte_server(
        routes
            .into_iter()
            .map(|(path, response)| (path, response.into_bytes()))
            .collect(),
    )
    .await
}

/// Keep response bodies as raw bytes so source-digest tests can distinguish
/// the actual body from the lossy UTF-8 projection used for readable output.
async fn spawn_loopback_byte_server(
    routes: Vec<(&'static str, Vec<u8>)>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds loopback listener");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let mut read = 0usize;
                loop {
                    let Ok(n) = socket.read(&mut buffer[read..]).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    read += n;
                    if buffer[..read]
                        .windows(4)
                        .any(|window| window == b"\r\n\r\n")
                    {
                        break;
                    }
                    if read == buffer.len() {
                        return;
                    }
                }
                let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let response = routes.iter().find(|(route, _)| *route == path).map_or_else(
                    || b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_vec(),
                    |(_, response)| response.clone(),
                );
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (base, handle)
}

fn text_response(content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn redirect_response(location: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

/// LAW (LW6, end to end): a loopback HTML page fetches through the guard,
/// follows a same-origin redirect, reduces to readable text (script content
/// dropped), and reports the final URL and media type honestly.
#[tokio::test]
async fn loopback_fetch_follows_redirects_and_reduces_html() {
    let html = "<html><head><script>evil()</script></head><body>\
                <h1>Docs</h1><p>hello <a href=\"https://example.com/x\">world</a></p>\
                </body></html>";
    let (base, _server) = spawn_loopback_server(vec![
        ("/start", String::new()),
        ("/doc", text_response("text/html; charset=utf-8", html)),
    ])
    .await;
    let (base2, _server2) =
        spawn_loopback_server(vec![("/hop", redirect_response(&format!("{base}/doc")))]).await;

    let outcome = fetch_public_url(&format!("{base2}/hop"), None)
        .await
        .expect("loopback fetch succeeds");
    assert_eq!(outcome.final_url, format!("{base}/doc"));
    assert_eq!(outcome.content_type, "text/html");
    assert!(!outcome.truncated);
    assert!(outcome.text.contains("# Docs"), "{}", outcome.text);
    assert!(
        outcome.text.contains("world (https://example.com/x)"),
        "{}",
        outcome.text
    );
    assert!(!outcome.text.contains("evil"), "{}", outcome.text);
}

/// LAW (LW6, redirect re-validation — the mutation anchor): EVERY hop passes
/// the origin fence again. A loopback response redirecting to the cloud
/// metadata address is refused at the hop, and a redirect to a PUBLIC
/// plain-HTTP origin is refused by the same per-hop rule.
#[tokio::test]
async fn redirect_hops_are_revalidated_through_the_origin_fence() {
    let (base, _server) = spawn_loopback_server(vec![
        (
            "/to-metadata",
            redirect_response("http://169.254.169.254/latest/meta-data/"),
        ),
        (
            // TEST-NET-2 (RFC 5737): public by the fence's classification and
            // guaranteed never routed, so even a BROKEN fence cannot turn
            // this fixture into a real network call.
            "/to-public-http",
            redirect_response("http://198.51.100.7/doc"),
        ),
        ("/to-private", redirect_response("https://10.0.0.8/doc")),
    ])
    .await;

    for path in ["/to-metadata", "/to-public-http", "/to-private"] {
        let error = fetch_public_url(&format!("{base}{path}"), None)
            .await
            .expect_err("hostile redirect target must be refused");
        // The REFUSAL kind is the discriminator: a fence that only guards
        // the first hop would still fail — as a TRANSPORT error from dialing
        // the hostile target — and that is exactly what must not happen.
        assert_eq!(
            error.kind,
            ProviderErrorKind::InvalidRequest,
            "the hop is refused by the fence, not by the network: {error}"
        );
        assert!(
            error.message.contains("web_fetch"),
            "typed refusal for {path}: {error}"
        );
    }
}

/// LAW (LW6, redirect cap): the sixth redirect is refused — bounded, never
/// a loop.
#[tokio::test]
async fn redirect_chains_cap_at_five_hops() {
    let (base, server) = {
        // Self-redirecting route: /loop -> /loop forever.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds loopback listener");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let self_base = base.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let response = redirect_response(&format!("{self_base}/loop"));
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 2048];
                    let _ = socket.read(&mut buffer).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (base, handle)
    };
    let error = fetch_public_url(&format!("{base}/loop"), None)
        .await
        .expect_err("unbounded redirects must be refused");
    assert!(
        error.message.contains("exceeded 5 redirects"),
        "typed cap refusal: {error}"
    );
    server.abort();
}

/// LAW (LW6, size cap + honest truncation): output caps at 96 KiB (or the
/// caller's smaller `max_bytes`), keeps a tail-weighted UTF-8-safe view, and
/// inserts a machine-readable marker; a body under the cap is untouched.
#[tokio::test]
async fn output_caps_with_an_honest_truncation_marker() {
    let big = "x".repeat(WEB_FETCH_OUTPUT_CAP_BYTES + 10_000);
    let (base, _server) = spawn_loopback_server(vec![
        ("/big", text_response("text/plain", &big)),
        ("/small", text_response("text/plain", "tiny body")),
    ])
    .await;

    let outcome = fetch_public_url(&format!("{base}/big"), None)
        .await
        .expect("big fetch succeeds");
    assert!(outcome.truncated);
    assert!(outcome.text.contains("\"haider_elision_v1\""));
    assert!(outcome.text.contains("\"scope\":\"web_fetch_output_cap\""));
    assert!(outcome.text.ends_with(&"x".repeat(256)));
    assert!(outcome.text.len() <= WEB_FETCH_OUTPUT_CAP_BYTES);

    let outcome = fetch_public_url(&format!("{base}/big"), Some(1024))
        .await
        .expect("capped fetch succeeds");
    assert!(outcome.truncated);
    assert!(outcome.text.starts_with(&"x".repeat(64)));
    assert!(outcome.text.contains("\"haider_elision_v1\""));
    assert!(outcome.text.ends_with(&"x".repeat(64)));
    assert!(outcome.text.len() <= 1024);

    let outcome = fetch_public_url(&format!("{base}/small"), None)
        .await
        .expect("small fetch succeeds");
    assert_eq!(outcome.text, "tiny body");
    assert!(!outcome.truncated);

    let error = fetch_public_url(&format!("{base}/big"), Some(1))
        .await
        .expect_err("an impossible marker budget is refused");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("at least 512"), "{error}");
}

/// LAW (W-F M6, whole-fetch wall-clock deadline): the per-chunk idle timeout
/// resets every chunk, so a slow drip (headers, then silence) never trips it.
/// An ABSOLUTE deadline across all hops+chunks aborts the fetch anyway — a
/// server that sends headers then holds the body open is cut off as a typed
/// Transport error naming the overall deadline. Driven with a SHORT injected
/// deadline over loopback (never the real network).
#[tokio::test]
async fn slow_drip_body_is_aborted_by_the_overall_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds loopback listener");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    let server = tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut scratch = vec![0u8; 1024];
            let _ = socket.read(&mut scratch).await;
            // Headers promise a 1 MB body; the server then goes silent, sending
            // only a sliver and holding the connection open past the deadline.
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 1000000\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(b"partial").await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let _ = socket.shutdown().await;
        }
    });

    let started = std::time::Instant::now();
    let error = fetch_public_url_with_deadline(
        &format!("{base}/slow"),
        None,
        std::time::Duration::from_millis(300),
    )
    .await
    .expect_err("a slow-drip body must be aborted by the overall deadline");
    assert_eq!(error.kind, ProviderErrorKind::Transport, "{error}");
    assert!(
        error.message.contains("overall"),
        "names the overall deadline: {error}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "aborted promptly at the deadline, not after the 30s per-chunk idle"
    );
    server.abort();
}

/// LAW (W-F M9, honest truncation at the source-cap boundary): a source body
/// of EXACTLY the 4 MiB cap with a clean EOF is NOT truncated; one byte past
/// it IS. The reducer drops the huge `<script>` body so the OUTPUT never
/// nears its own 96 KiB cap — the `truncated` flag reflects the SOURCE
/// boundary alone.
#[tokio::test]
async fn source_cap_boundary_truncation_is_off_by_one_honest() {
    const SOURCE_CAP: usize = 4 * 1024 * 1024;
    let prefix = "<html><body><p>BOUNDARY_MARKER</p><script>";
    let suffix = "</script></body></html>";
    let make = |total: usize| {
        let filler = "x".repeat(total - prefix.len() - suffix.len());
        format!("{prefix}{filler}{suffix}")
    };
    let exact = make(SOURCE_CAP);
    assert_eq!(
        exact.len(),
        SOURCE_CAP,
        "the exact body is the cap to the byte"
    );
    let over = make(SOURCE_CAP + 1);
    let mut changed_discarded_byte = over.clone();
    changed_discarded_byte.replace_range(SOURCE_CAP.., "!");
    assert_eq!(&over[..SOURCE_CAP], &changed_discarded_byte[..SOURCE_CAP]);
    let (base, server) = spawn_loopback_server(vec![
        ("/exact", text_response("text/html", &exact)),
        ("/over", text_response("text/html", &over)),
        (
            "/changed-discarded-byte",
            text_response("text/html", &changed_discarded_byte),
        ),
    ])
    .await;

    let at = fetch_public_url(&format!("{base}/exact"), None)
        .await
        .expect("exact-cap fetch succeeds");
    assert!(
        at.text.contains("BOUNDARY_MARKER"),
        "marker survives: {}",
        &at.text[..at.text.len().min(80)]
    );
    assert!(
        !at.truncated,
        "a body EXACTLY at the source cap with a clean EOF is NOT truncated"
    );
    assert!(at.truncation.is_none(), "exact-cap EOF has no typed marker");

    let past = fetch_public_url(&format!("{base}/over"), None)
        .await
        .expect("over-cap fetch succeeds");
    assert!(past.truncated, "one byte past the source cap IS truncated");
    assert!(past.text.contains("\"haider_elision_v1\""));
    assert!(past.text.contains("\"omitted_bytes_exact\":false"));
    let provenance = past.truncation.as_ref().expect("source-cap provenance");
    assert!(provenance.truncated);
    assert_eq!(provenance.original_bytes, (SOURCE_CAP + 1) as u64);
    assert_eq!(
        provenance.sha256, "dc0935c82d0358225cda8e000411be8a277b429dc53e37b880146e6c8de6d136",
        "digest includes the one observed overflow/lookahead byte"
    );

    let changed = fetch_public_url(&format!("{base}/changed-discarded-byte"), None)
        .await
        .expect("changed discarded suffix fetch succeeds");
    assert_eq!(
        changed.text, past.text,
        "retained response bytes are identical"
    );
    let changed_provenance = changed.truncation.expect("changed source-cap provenance");
    assert_eq!(changed_provenance.original_bytes, provenance.original_bytes);
    assert_eq!(
        changed_provenance.sha256,
        "0c8bfe1f4e3259caf3695d89715bc08a10cef038281a1f55784f51a71d937f3a"
    );
    assert_ne!(changed_provenance.sha256, provenance.sha256);
    server.abort();
}

/// The production response reader hashes raw body bytes before UTF-8
/// replacement and output capping, including invalid sequences discarded by
/// the readable projection. The digest was independently pinned with SHA-256.
#[tokio::test]
async fn truncated_invalid_utf8_response_hashes_original_body_bytes() {
    let mut body = vec![b'x'; 4096];
    body[..4].copy_from_slice(&[0xff, 0xc3, b'(', 0x80]);
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    let (base, server) = spawn_loopback_byte_server(vec![("/invalid", response)]).await;
    let outcome = fetch_public_url(&format!("{base}/invalid"), Some(512))
        .await
        .expect("invalid UTF-8 still produces bounded readable output");
    assert!(outcome.truncated);
    assert!(outcome.text.starts_with("��(�"));
    assert!(outcome.text.len() <= 512);
    let provenance = outcome.truncation.expect("output-cap source provenance");
    assert_eq!(provenance.original_bytes, body.len() as u64);
    assert_eq!(
        provenance.sha256,
        "50988e81d87e6a68088f37f1d6137fd67cb5beb6499e260214587d3bb1745cd2"
    );
    assert_ne!(
        provenance.sha256,
        haider_protocol::tool::ToolTruncation::from_bytes(
            String::from_utf8_lossy(&body).as_bytes(),
            0,
        )
        .sha256,
        "lossy UTF-8 bytes must not become the original-byte hash"
    );
    server.abort();
}

/// LAW (LW6, content-type gate): `text/*` and `application/json` pass;
/// everything else — and a missing declaration — is a typed refusal.
#[tokio::test]
async fn content_types_gate_to_text_and_json_only() {
    let (base, _server) = spawn_loopback_server(vec![
        ("/json", text_response("application/json", "{\"ok\":true}")),
        ("/markdown", text_response("text/markdown", "# md")),
        ("/pdf", text_response("application/pdf", "%PDF-1.4")),
        (
            "/naked",
            "HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbody".to_owned(),
        ),
    ])
    .await;

    let json = fetch_public_url(&format!("{base}/json"), None)
        .await
        .expect("json passes");
    assert_eq!(json.content_type, "application/json");
    assert_eq!(json.text, "{\"ok\":true}");

    let markdown = fetch_public_url(&format!("{base}/markdown"), None)
        .await
        .expect("text/* passes");
    assert_eq!(markdown.text, "# md");

    let error = fetch_public_url(&format!("{base}/pdf"), None)
        .await
        .expect_err("pdf is refused (deferred to W-D)");
    assert!(error.message.contains("application/pdf"), "{error}");

    let error = fetch_public_url(&format!("{base}/naked"), None)
        .await
        .expect_err("missing content-type is refused");
    assert!(error.message.contains("Content-Type"), "{error}");
}

/// Review pin (coordinator, W-B review of record): the 4 MiB SOURCE cap
/// bounds the raw body BEFORE html reduction, independently of the 96 KiB
/// OUTPUT cap. The observation is non-degenerate: a body that is huge at the
/// source but reduces to almost nothing (5 MiB of dropped <script> content
/// around a tiny marker) hits the SOURCE cap but never the output cap — so
/// `truncated` reflects the source cap ALONE here.
/// MUTATION CHECK (executed): neuter the source-cap clamp in
/// `read_body_bounded` (read the whole body). Expected RUNTIME failure: the
/// full 5 MiB reduces to the tiny marker, nothing hits the 96 KiB output cap,
/// and `truncated` flips to false — the memory-safety valve is gone unseen.
#[tokio::test]
async fn oversized_source_is_capped_before_reduction_even_when_it_reduces_small() {
    // Marker first (survives both the capped and uncapped read), then ~5 MiB
    // of script content the reducer drops — 1 MiB past the 4 MiB source cap.
    let filler = "x".repeat(5 * 1024 * 1024);
    let html = format!("<html><body><p>SRCCAP_MARKER</p><script>{filler}</script></body></html>");
    let (base, _server) = spawn_loopback_server(vec![(
        "/big",
        text_response("text/html; charset=utf-8", &html),
    )])
    .await;

    let outcome = fetch_public_url(&format!("{base}/big"), None)
        .await
        .expect("loopback fetch succeeds");
    assert!(
        outcome.text.contains("SRCCAP_MARKER"),
        "the marker survives reduction: {}",
        &outcome.text[..outcome.text.len().min(120)]
    );
    assert!(
        outcome.text.len() <= WEB_FETCH_OUTPUT_CAP_BYTES,
        "reduced output is tiny, far under the output cap"
    );
    assert!(
        outcome.truncated,
        "the raw body exceeded the 4 MiB SOURCE cap, so the fetch is truncated \
         even though the reduced output never approached the 96 KiB output cap"
    );
    assert!(outcome.text.contains("\"haider_elision_v1\""));
    assert!(outcome.text.contains("\"omitted_bytes_exact\":false"));
}
