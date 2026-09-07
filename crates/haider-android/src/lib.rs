//! Disposable embedding experiment, NOT the standalone production policy.
//! Uses the real daemon with default vault/tools and a synthetic (unused) DEK.
//! All six JNI calls must run off the Android main thread, serialized by the host.

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod android;
