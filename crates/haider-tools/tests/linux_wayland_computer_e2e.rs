#![cfg(target_os = "linux")]

//! Manual Wayland portal validation.
//!
//! This deliberately cannot run in headless CI: xdg-desktop-portal requires a
//! real logged-in desktop and interactive ScreenCast/RemoteDesktop consent.

use haider_protocol::computer::ComputerAction;
// Keep the manual fixture's backend trait contract explicit in this Linux-only test.
#[allow(unused_imports)]
use haider_tools::ComputerBackend;
use haider_tools::{ComputerCancelToken, ComputerOutput, platform_computer_backend};

#[tokio::test]
#[ignore = "requires HAIDER_CU_WAYLAND_E2E=1, a real Wayland session, portal bridge, and interactive consent"]
// Manual fixture failures need the exact Wayland assertion context.
#[allow(clippy::expect_used)]
async fn real_wayland_portal_capture_and_remote_desktop_pointer_round_trip() {
    assert_eq!(
        std::env::var("HAIDER_CU_WAYLAND_E2E").as_deref(),
        Ok("1"),
        "manual Wayland validation must be explicitly armed"
    );
    assert!(
        std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
            || std::env::var("XDG_SESSION_TYPE")
                .is_ok_and(|value| value.eq_ignore_ascii_case("wayland")),
        "manual Wayland validation requires a positively detected Wayland session"
    );

    let backend = platform_computer_backend();
    let cancel = ComputerCancelToken::new();
    let ComputerOutput::ScreenshotPng(png) = backend
        .execute(&ComputerAction::Screenshot, &cancel)
        .await
        .unwrap_or_else(|error| panic!("consented portal capture must succeed: {error}"))
    else {
        panic!("screenshot action must return PNG bytes");
    };
    let image = image::load_from_memory(&png)
        .unwrap_or_else(|error| panic!("portal screenshot must decode: {error}"));
    assert!(image.width() > 0 && image.height() > 0);
    backend
        .set_viewport(image.width(), image.height())
        .unwrap_or_else(|error| panic!("portal viewport must install: {error}"));

    backend
        .execute(&ComputerAction::MouseMove { x: 1, y: 1 }, &cancel)
        .await
        .unwrap_or_else(|error| panic!("RemoteDesktop pointer motion must succeed: {error}"));
    let cursor_error = backend
        .execute(&ComputerAction::CursorPosition, &cancel)
        .await
        .expect_err("Wayland must not fabricate an authoritative global cursor position");
    assert!(cursor_error.to_string().contains("cursor"));
    let ComputerOutput::ScreenshotPng(after_move) = backend
        .execute(&ComputerAction::Screenshot, &cancel)
        .await
        .unwrap_or_else(|error| panic!("post-input portal capture must succeed: {error}"))
    else {
        panic!("post-input screenshot action must return PNG bytes");
    };
    image::load_from_memory(&after_move)
        .unwrap_or_else(|error| panic!("post-input portal screenshot must decode: {error}"));
}
