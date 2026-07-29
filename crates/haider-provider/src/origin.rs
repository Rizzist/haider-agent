use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::openai::blocked_credential_target;
use crate::{ProviderError, ProviderErrorKind};

#[async_trait]
pub(crate) trait FixedDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

#[derive(Debug)]
pub(crate) struct SystemFixedDnsResolver;

#[async_trait]
impl FixedDnsResolver for SystemFixedDnsResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

pub(crate) struct FixedOriginGuard {
    endpoint: reqwest::Url,
    host: String,
    port: u16,
    resolver: Arc<dyn FixedDnsResolver>,
    validated: OnceCell<Result<Arc<[SocketAddr]>, ProviderError>>,
    #[cfg(test)]
    connection_resolutions: AtomicUsize,
    #[cfg(test)]
    stall_connection_resolution: AtomicBool,
}

struct PinnedAddrs {
    addresses: Arc<[SocketAddr]>,
    next: usize,
}

impl Iterator for PinnedAddrs {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<Self::Item> {
        let address = self.addresses.get(self.next).copied();
        self.next += usize::from(address.is_some());
        address
    }
}

impl FixedOriginGuard {
    pub(crate) fn new(
        endpoint: &str,
        trusted_host: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        let parsed = reqwest::Url::parse(endpoint)
            .map_err(|_| invalid_origin("fixed inference endpoint is not a valid URL"))?;
        if parsed.scheme() != "https"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed
                .host_str()
                .is_none_or(|host| !host.eq_ignore_ascii_case(trusted_host))
            || parsed.port_or_known_default() != Some(443)
        {
            return Err(invalid_origin(
                "fixed inference endpoint is outside its trusted HTTPS origin",
            ));
        }
        Ok(Self {
            endpoint: parsed,
            host: trusted_host.to_owned(),
            port: 443,
            resolver,
            validated: OnceCell::new(),
            #[cfg(test)]
            connection_resolutions: AtomicUsize::new(0),
            #[cfg(test)]
            stall_connection_resolution: AtomicBool::new(false),
        })
    }

    pub(crate) async fn validate_endpoint(&self, endpoint: &str) -> Result<(), ProviderError> {
        let requested = reqwest::Url::parse(endpoint)
            .map_err(|_| invalid_origin("fixed inference endpoint is not a valid URL"))?;
        if requested != self.endpoint {
            return Err(invalid_origin(
                "credential-bearing request was redirected outside its fixed endpoint",
            ));
        }
        self.validated_addresses().await.map(|_| ())
    }

    async fn validated_addresses(&self) -> Result<Arc<[SocketAddr]>, ProviderError> {
        self.validated
            .get_or_init(|| async {
                let addresses =
                    self.resolver
                        .resolve(&self.host, self.port)
                        .await
                        .map_err(|error| {
                            ProviderError::new(
                                ProviderErrorKind::Transport,
                                format!(
                                    "could not resolve fixed inference host `{}`: {error}",
                                    self.host
                                ),
                            )
                        })?;
                validate_addresses(&self.host, addresses)
            })
            .await
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn stall_connection_resolution(&self) {
        self.stall_connection_resolution
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn connection_resolution_count(&self) -> usize {
        self.connection_resolutions.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for FixedOriginGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FixedOriginGuard")
            .field("endpoint", &self.endpoint)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("validated", &self.validated.get().is_some())
            .finish_non_exhaustive()
    }
}

impl reqwest::dns::Resolve for FixedOriginGuard {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        #[cfg(test)]
        {
            self.connection_resolutions.fetch_add(1, Ordering::SeqCst);
            if self.stall_connection_resolution.load(Ordering::SeqCst) {
                return Box::pin(std::future::pending());
            }
        }
        let requested = name.as_str();
        let result: Result<
            reqwest::dns::Addrs,
            Box<dyn std::error::Error + Send + Sync + 'static>,
        > = if !requested
            .trim_end_matches('.')
            .eq_ignore_ascii_case(self.host.trim_end_matches('.'))
        {
            Err(Box::new(io::Error::other(format!(
                "fixed resolver refused unexpected host `{requested}`"
            ))))
        } else {
            match self.validated.get() {
                Some(Ok(addresses)) => Ok(Box::new(PinnedAddrs {
                    addresses: Arc::clone(addresses),
                    next: 0,
                })),
                Some(Err(error)) => Err(Box::new(io::Error::other(error.message.clone()))),
                None => Err(Box::new(io::Error::other(
                    "fixed origin was not validated before connection",
                ))),
            }
        };
        Box::pin(std::future::ready(result))
    }
}

fn validate_addresses(
    host: &str,
    addresses: Vec<SocketAddr>,
) -> Result<Arc<[SocketAddr]>, ProviderError> {
    if addresses.is_empty() {
        return Err(invalid_origin(format!(
            "fixed inference host `{host}` resolved to no addresses"
        )));
    }
    let mut pinned = Vec::with_capacity(addresses.len());
    for address in addresses {
        if blocked_fixed_credential_target(address.ip()) {
            return Err(invalid_origin(format!(
                "fixed inference host `{host}` resolved to a private, link-local, or special-use IP address"
            )));
        }
        if !pinned.contains(&address) {
            pinned.push(address);
        }
    }
    Ok(pinned.into())
}

fn blocked_fixed_credential_target(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => blocked_fixed_ipv4_target(address),
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.to_ipv4().is_some_and(blocked_fixed_ipv4_target)
                || blocked_credential_target(IpAddr::V6(address))
        }
    }
}

fn blocked_fixed_ipv4_target(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_loopback()
        // RFC 6598 shared address space includes cloud metadata endpoints such
        // as 100.100.100.200 and is never a valid answer for a trusted host.
        || (octets[0] == 100 && (octets[1] & 0xc0) == 0x40)
        || blocked_credential_target(IpAddr::V4(address))
}

fn invalid_origin(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}
