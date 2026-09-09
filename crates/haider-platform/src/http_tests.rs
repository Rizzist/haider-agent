#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;
use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Guard {
    calls: AtomicUsize,
}
impl reqwest::dns::Resolve for Guard {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(Box::new(io::Error::other("origin guard rejected name")) as _) })
    }
}

async fn proxy_fixture() -> (SocketAddr, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                request.push(stream.read_u8().await.unwrap());
                assert!(request.len() < 8192);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nsynthetic",
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        })
        .await
        .expect("proxy request deadline")
    });
    (address, task)
}

#[tokio::test]
async fn android_named_numeric_proxy_exclusions_and_reconfiguration_preserve_origin_guard() {
    let state: ProxyState = Arc::new(Mutex::new(None));
    let origin = Arc::new(Guard {
        calls: AtomicUsize::new(0),
    });
    let client = with_system_proxy(reqwest::Client::builder().no_proxy(), state.clone())
        .dns_resolver(origin.clone())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    // Reconfigure the same client, proving neither its selector nor connector
    // freezes a previous runtime's proxy hostname or port. A separate fixture
    // keeps an idle connection alive to exercise pooling across reconfiguration.
    for hostname in ["localhost", "127.0.0.1", "localhost"] {
        let (address, task) = proxy_fixture().await;
        let mut proxy = SystemProxy::new(
            hostname,
            address.port(),
            vec![
                "excluded.invalid".into(),
                "*.direct.invalid".into(),
                "localhost".into(),
            ],
        )
        .unwrap();
        proxy.resolve_address().await;
        assert_eq!(proxy.url.host_str(), Some("127.0.0.1"));
        *state.lock().unwrap() = Some(proxy);
        let response = client
            .get("http://origin.invalid/synthetic")
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "synthetic");
        let request = task.await.unwrap();
        assert!(request.starts_with("GET http://origin.invalid/synthetic HTTP/1.1\r\n"));
        assert_eq!(
            origin.calls.load(Ordering::SeqCst),
            0,
            "proxy DNS is independent"
        );
    }
    for url in [
        "http://excluded.invalid/",
        "http://nested.direct.invalid/",
        "http://localhost/",
        "http://localhost:8123/",
        "https://localhost/",
        "https://localhost:8123/",
    ] {
        assert!(client.get(url).send().await.is_err());
    }
    assert_eq!(
        origin.calls.load(Ordering::SeqCst),
        6,
        "same-host exclusions with explicit/default ports retain the direct origin guard"
    );
    *state.lock().unwrap() = None;
    assert!(
        client
            .get("http://unexpected.invalid/")
            .send()
            .await
            .is_err()
    );
    assert_eq!(origin.calls.load(Ordering::SeqCst), 7);
}

#[test]
fn android_proxy_exclusions_are_whole_host_patterns() {
    for (pattern, host, expected) in [
        ("example.org", "example.org", true),
        ("example.org", "evil-example.org", false),
        ("*.example.org", "a.b.example.org", true),
        ("*.example.org", "example.org", false),
        ("*", "anything.invalid", true),
        ("a*b*c", "abc", true),
        ("a*b*c", "abxc", true),
        ("a*b*c", "ab", false),
        ("", "example.org", false),
    ] {
        assert_eq!(excluded_host(pattern, host), expected, "{pattern} / {host}");
    }
    assert!(SystemProxy::new("user@host", 80, vec![]).is_err());
    assert!(SystemProxy::new("localhost", 0, vec![]).is_err());
    let ipv6 = SystemProxy::new("::1", 8080, vec![]).unwrap();
    assert_eq!(ipv6.url.as_str(), "http://[::1]:8080/");
}

#[tokio::test]
async fn android_proxy_reconfiguration_does_not_reuse_an_idle_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_address = listener.local_addr().unwrap();
    let first_proxy = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
                assert!(request.len() < 8192);
            }
            assert!(
                String::from_utf8(request)
                    .unwrap()
                    .starts_with("GET http://origin.invalid/first HTTP/1.1\r\n")
            );
            // Keep this server-side connection available for another request.
            // A pooled client could otherwise reuse it after changing proxies.
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: keep-alive\r\n\r\nfirst",
                )
                .await
                .unwrap();
            let mut next = [0; 1];
            assert_eq!(
                stream.read(&mut next).await.unwrap(),
                0,
                "the previous proxy received a request after reconfiguration"
            );
        })
        .await
        .expect("old proxy connection deadline");
    });
    let state: ProxyState = Arc::new(Mutex::new(Some(
        SystemProxy::new("127.0.0.1", first_address.port(), vec![]).unwrap(),
    )));
    let origin = Arc::new(Guard {
        calls: AtomicUsize::new(0),
    });
    let client = with_system_proxy(reqwest::Client::builder().no_proxy(), state.clone())
        .dns_resolver(origin.clone())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert_eq!(
        client
            .get("http://origin.invalid/first")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "first"
    );
    let (second_address, second_proxy) = proxy_fixture().await;
    *state.lock().unwrap() =
        Some(SystemProxy::new("127.0.0.1", second_address.port(), vec![]).unwrap());
    assert_eq!(
        client
            .get("http://origin.invalid/second")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "synthetic"
    );
    assert!(
        second_proxy
            .await
            .unwrap()
            .starts_with("GET http://origin.invalid/second HTTP/1.1\r\n")
    );
    first_proxy.await.unwrap();
    assert_eq!(origin.calls.load(Ordering::SeqCst), 0);
}

struct PinnedOrigin {
    address: SocketAddr,
    calls: AtomicUsize,
}
impl reqwest::dns::Resolve for PinnedOrigin {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(name.as_str(), "localhost");
        let address = self.address;
        Box::pin(async move { Ok(Box::new(std::iter::once(address)) as reqwest::dns::Addrs) })
    }
}

#[tokio::test]
async fn android_proxy_same_host_direct_route_uses_pinned_origin_and_https_uses_connect() {
    let (direct_address, direct_request) = proxy_fixture().await;
    let (proxy_address, proxy_request) = proxy_fixture().await;
    let mut proxy =
        SystemProxy::new("localhost", proxy_address.port(), vec!["localhost".into()]).unwrap();
    proxy.resolve_address().await;
    let state = Arc::new(Mutex::new(Some(proxy)));
    let origin = Arc::new(PinnedOrigin {
        address: direct_address,
        calls: AtomicUsize::new(0),
    });
    let client = with_system_proxy(reqwest::Client::builder().no_proxy(), state)
        .dns_resolver(origin.clone())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert_eq!(
        client
            .get(format!("http://localhost:{}/direct", direct_address.port()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "synthetic"
    );
    assert!(
        direct_request
            .await
            .unwrap()
            .starts_with("GET /direct HTTP/1.1\r\n")
    );
    assert_eq!(origin.calls.load(Ordering::SeqCst), 1);
    // This synthetic proxy records CONNECT and deliberately closes during TLS.
    // TLS trust validation itself is exercised by the Android HTTPS journey.
    assert!(
        client
            .get("https://origin.invalid/https")
            .send()
            .await
            .is_err()
    );
    assert!(
        proxy_request
            .await
            .unwrap()
            .starts_with("CONNECT origin.invalid:443 HTTP/1.1\r\n")
    );
    assert_eq!(
        origin.calls.load(Ordering::SeqCst),
        1,
        "proxy never resolves through origin guard"
    );
}
