#![allow(clippy::expect_used)]

//! W-B local web_fetch end-to-end laws over LOOPBACK mock servers only —
//! no test in this file dials the real network (loopback plain HTTP is the
//! policy's one http allowance, which is exactly what makes these fetches
//! testable end to end).

use haider_provider::{WEB_FETCH_OUTPUT_CAP_BYTES, fetch_public_url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves each accepted connection one canned HTTP/1.1 response chosen by
/// the request path. Returns the bound loopback base URL.
async fn spawn_loopback_server(
    routes: Vec<(&'static str, String)>,
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
                    || "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned(),
                    |(_, response)| response.clone(),
                );
                let _ = socket.write_all(response.as_bytes()).await;
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
            "/to-public-http",
            redirect_response("http://93.184.216.34/doc"),
        ),
        ("/to-private", redirect_response("https://10.0.0.8/doc")),
    ])
    .await;

    for path in ["/to-metadata", "/to-public-http", "/to-private"] {
        let error = fetch_public_url(&format!("{base}{path}"), None)
            .await
            .expect_err("hostile redirect target must be refused");
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
/// caller's smaller `max_bytes`), cuts on a char boundary, and appends the
/// honest truncation marker; a body under the cap is untouched.
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
    assert!(
        outcome.text.contains(&format!(
            "[web_fetch: output truncated at {WEB_FETCH_OUTPUT_CAP_BYTES} bytes]"
        )),
        "honest marker present"
    );
    assert!(outcome.text.len() <= WEB_FETCH_OUTPUT_CAP_BYTES + 128);

    let outcome = fetch_public_url(&format!("{base}/big"), Some(1024))
        .await
        .expect("capped fetch succeeds");
    assert!(outcome.truncated);
    assert!(outcome.text.starts_with(&"x".repeat(1024)));
    assert!(
        outcome
            .text
            .contains("[web_fetch: output truncated at 1024 bytes]")
    );

    let outcome = fetch_public_url(&format!("{base}/small"), None)
        .await
        .expect("small fetch succeeds");
    assert_eq!(outcome.text, "tiny body");
    assert!(!outcome.truncated);
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
