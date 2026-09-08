//! Frozen Android JNI v1 lifecycle adapter. RPC stays on filesystem sockets.

#[cfg(any(test, target_os = "android"))]
mod completion;
pub mod contract;
#[cfg(test)]
mod contract_tests;
#[cfg(target_os = "android")]
mod logging;

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod android;

#[cfg(target_os = "android")]
mod test_provider;
