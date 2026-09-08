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
        // The control client is shared across daemon restarts. Resolve the
        // latest OS proxy at request time, rather than freezing the first DEK
        // owner's network configuration into the process-wide client.
        return builder.proxy(reqwest::Proxy::custom(|_| {
            ANDROID_PROXY
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }));
    }
    #[cfg(not(target_os = "android"))]
    builder
}

#[cfg(target_os = "android")]
static ANDROID_PROXY: std::sync::Mutex<Option<reqwest::Url>> = std::sync::Mutex::new(None);

/// Called by the JNI owner before runtime startup using ConnectivityManager's
/// current default ProxyInfo. The input is an OS proxy address, never credentials.
#[cfg(target_os = "android")]
pub fn set_android_http_proxy(host: Option<&str>, port: u16) -> Result<(), String> {
    let proxy = host
        .map(|host| {
            let host = if host.contains(':') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            reqwest::Url::parse(&format!("http://{host}:{port}"))
                .map_err(|_| "invalid Android system proxy".to_owned())
        })
        .transpose()?;
    *ANDROID_PROXY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = proxy;
    Ok(())
}
