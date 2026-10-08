#![allow(clippy::expect_used)]

use super::computer_screenshot_target_size;
use haider_protocol::tool::{COMPUTER_SCREENSHOT_MAX_DIMENSION, COMPUTER_SCREENSHOT_MAX_PIXELS};

#[test]
fn target_size_is_none_inside_the_envelope_and_bounds_both_limits_outside_it() {
    assert_eq!(computer_screenshot_target_size(1_280, 720), None);
    assert_eq!(
        computer_screenshot_target_size(COMPUTER_SCREENSHOT_MAX_DIMENSION, 700),
        None
    );
    // 16:9 desktop: the pixel-area limit is the binding one.
    assert_eq!(
        computer_screenshot_target_size(1_920, 1_080),
        Some((1_429, 804))
    );
    // Very wide capture: the long-edge limit is the binding one.
    let (width, height) =
        computer_screenshot_target_size(4_000, 400).expect("wide capture must resize");
    assert_eq!(width, COMPUTER_SCREENSHOT_MAX_DIMENSION);
    assert_eq!(height, 156);
    for (source_width, source_height) in [(2_560, 1_600), (3_840, 2_160), (1_569, 1)] {
        let (width, height) = computer_screenshot_target_size(source_width, source_height)
            .expect("out-of-envelope capture must resize");
        assert!(width <= COMPUTER_SCREENSHOT_MAX_DIMENSION);
        assert!(height <= COMPUTER_SCREENSHOT_MAX_DIMENSION);
        assert!(u64::from(width) * u64::from(height) <= COMPUTER_SCREENSHOT_MAX_PIXELS);
    }
}
