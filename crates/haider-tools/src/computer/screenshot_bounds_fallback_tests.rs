use super::*;

#[test]
fn desktop_screenshot_bounds_report_unavailable() {
    assert!(matches!(
        bound_computer_screenshot_png(b"desktop-only capture"),
        Err(ComputerError::Unavailable { .. })
    ));
}
