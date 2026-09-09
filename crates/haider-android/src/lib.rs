//! Frozen Android JNI v1 lifecycle adapter. RPC stays on filesystem sockets.

#[cfg(all(
    target_os = "android",
    not(any(target_arch = "aarch64", target_arch = "x86_64"))
))]
compile_error!("JNI v1 supports only arm64-v8a and x86_64");

#[cfg(any(test, target_os = "android"))]
mod completion;
pub mod contract;
#[cfg(test)]
mod contract_tests;
#[cfg(all(unix, any(test, target_os = "android")))]
mod logging;

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod android;

#[cfg(all(unix, any(test, target_os = "android")))]
mod owner;
