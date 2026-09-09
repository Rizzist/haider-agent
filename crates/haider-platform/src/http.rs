//! HTTP bootstrap shared by native clients. Desktop behavior is unchanged.

/// A standard client builder with Android's bundled Mozilla trust anchors.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    #[cfg(target_os = "android")]
    {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        builder.tls_backend_preconfigured(tls)
    }
    #[cfg(not(target_os = "android"))]
    builder
}

/// Control/provider traffic uses the explicit Android system proxy, if any.
/// Desktop retains its existing no-proxy behavior. Pinned web-fetch clients
/// use `http_client_builder().no_proxy()` to preserve their DNS boundary.
pub fn native_http_client_builder() -> reqwest::ClientBuilder {
    let builder = http_client_builder().no_proxy();
    #[cfg(target_os = "android")]
    {
        with_system_proxy(builder, android_proxy())
    }
    #[cfg(not(target_os = "android"))]
    builder
}

#[cfg(any(test, target_os = "android"))]
type ProxyState = std::sync::Arc<std::sync::Mutex<Option<SystemProxy>>>;

#[cfg(any(test, target_os = "android"))]
#[derive(Clone)]
struct SystemProxy {
    url: reqwest::Url,
    exclusions: Vec<String>,
}

#[cfg(any(test, target_os = "android"))]
impl SystemProxy {
    fn new(host: &str, port: u16, exclusions: Vec<String>) -> Result<Self, String> {
        if host.trim().is_empty() || host.contains(['/', '@', '?', '#']) || port == 0 {
            return Err("invalid Android system proxy".into());
        }
        let mut url =
            reqwest::Url::parse("http://localhost").map_err(|_| "invalid Android system proxy")?;
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        url.set_host(Some(&host))
            .map_err(|_| "invalid Android system proxy")?;
        url.set_port(Some(port))
            .map_err(|()| "invalid Android system proxy")?;
        Ok(Self {
            url,
            exclusions: exclusions
                .into_iter()
                .map(|name| name.to_ascii_lowercase())
                .collect(),
        })
    }

    async fn resolve_address(&mut self) {
        let Some(host) = self.url.host_str() else {
            return;
        };
        if host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok()
        {
            return;
        }
        let port = self.url.port_or_known_default().unwrap_or(80);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::net::lookup_host((host.to_owned(), port)),
        )
        .await;
        if let Ok(Ok(addresses)) = result {
            let addresses: Vec<_> = addresses.collect();
            // Prefer the emulator/system proxy's IPv4 route when both address
            // families are published; an IPv6-only proxy keeps its IPv6 address.
            if let Some(address) = addresses
                .iter()
                .find(|address| address.is_ipv4())
                .or(addresses.first())
            {
                let _ = self.url.set_ip_host(address.ip());
            }
        }
        // Failed proxy DNS must not prevent local daemon readiness. Keeping the
        // configured hostname fails through the ordinary HTTP resolver/guard;
        // it never installs an exemption or silently switches to direct access.
    }

    fn selected_for(&self, request: &reqwest::Url) -> Option<reqwest::Url> {
        let host = request.host_str()?.to_ascii_lowercase();
        (!self
            .exclusions
            .iter()
            .any(|pattern| excluded_host(pattern, &host)))
        .then(|| self.url.clone())
    }
}

#[cfg(any(test, target_os = "android"))]
fn excluded_host(pattern: &str, host: &str) -> bool {
    // Android ProxyInfo exclusions use whole-host matching with '*' wildcards.
    let mut rest = host;
    let mut parts = pattern.split('*').peekable();
    let Some(first) = parts.next() else {
        return false;
    };
    if parts.peek().is_none() {
        return host == first;
    }
    let Some(after_prefix) = rest.strip_prefix(first) else {
        return false;
    };
    rest = after_prefix;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    true
}

#[cfg(any(test, target_os = "android"))]
fn with_system_proxy(builder: reqwest::ClientBuilder, proxy: ProxyState) -> reqwest::ClientBuilder {
    // Do not reuse an idle connection to a proxy from a previous runtime/network
    // configuration. Active requests finish using their admitted connection.
    builder
        .pool_max_idle_per_host(0)
        .proxy(reqwest::Proxy::custom(move |request| {
            proxy
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|proxy| proxy.selected_for(request))
        }))
}

#[cfg(target_os = "android")]
fn android_proxy() -> ProxyState {
    static PROXY: std::sync::OnceLock<ProxyState> = std::sync::OnceLock::new();
    std::sync::Arc::clone(PROXY.get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(None))))
}

/// JNI publishes ConnectivityManager's current default ProxyInfo at each
/// nativeStart. No environment proxy, proxy credentials, or PAC script is read.
#[cfg(target_os = "android")]
pub async fn set_android_http_proxy(
    host: Option<&str>,
    port: u16,
    exclusions: Vec<String>,
) -> Result<(), String> {
    let mut proxy = host
        .map(|host| SystemProxy::new(host, port, exclusions))
        .transpose()?;
    if let Some(proxy) = &mut proxy {
        proxy.resolve_address().await;
    }
    *android_proxy()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = proxy;
    Ok(())
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
