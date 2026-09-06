//! The installer must probe the companion without an active desktop session.
#![allow(clippy::expect_used)]

use std::process::{Command, Stdio};

#[test]
fn wayland_companion_version_matches_the_bundle_probe_without_a_desktop() {
    let output = Command::new(env!("CARGO_BIN_EXE_haider-wayland-portal"))
        .arg("--version")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XDG_RUNTIME_DIR")
        .env("DBUS_SESSION_BUS_ADDRESS", "invalid:bundle-version-probe")
        .stdin(Stdio::null())
        .output()
        .expect("run companion version probe");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        format!("haider-wayland-portal {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}
