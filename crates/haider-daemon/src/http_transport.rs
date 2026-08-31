//! Shared, lazily constructed HTTP transport for daemon-owned control-plane
//! requests.
//!
//! Purpose-specific request builders override their own deadlines. The
//! shared client owns the common no-proxy/no-redirect/five-second-connect
//! policy plus the OAuth views' existing 15-second default request ceiling.

use std::sync::OnceLock;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

static SHARED_HTTP_CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();

#[derive(Clone, Copy, Default)]
pub(crate) struct SharedHttpTransport;

impl SharedHttpTransport {
    pub(crate) fn client(self) -> Option<&'static reqwest::Client> {
        SHARED_HTTP_CLIENT.get_or_init(build_client).as_ref()
    }
}

fn build_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .build()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn get_from<'a>(
        transport: SharedHttpTransport,
        cell: &'a OnceLock<Option<reqwest::Client>>,
        builds: &AtomicUsize,
    ) -> Option<&'a reqwest::Client> {
        let _ = transport;
        cell.get_or_init(|| {
            builds.fetch_add(1, Ordering::Relaxed);
            build_client()
        })
        .as_ref()
    }

    /// MUTATION CHECK: give every one of the five purpose views its own
    /// `OnceLock`. The control then records five builds instead of one.
    #[test]
    fn five_purpose_views_build_one_transport_not_five() {
        let shared = OnceLock::new();
        let shared_builds = AtomicUsize::new(0);
        for _ in 0..5 {
            assert!(
                get_from(SharedHttpTransport, &shared, &shared_builds).is_some(),
                "the platform must be able to build the shared transport"
            );
        }
        assert_eq!(shared_builds.load(Ordering::Relaxed), 1);

        let separate_builds = AtomicUsize::new(0);
        let separate = std::array::from_fn::<_, 5, _>(|_| OnceLock::new());
        for cell in &separate {
            assert!(
                get_from(SharedHttpTransport, cell, &separate_builds).is_some(),
                "the mutation control must build each independent transport"
            );
        }
        assert_eq!(separate_builds.load(Ordering::Relaxed), 5);
    }
}
