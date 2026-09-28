//! Computer-screenshot size envelope applied after redaction/crop and before
//! CAS admission. The CAS image's delivered width/height are what the backend
//! later maps model coordinates from, so resizing here stays coordinate-safe.

use super::{ComputerError, ComputerResult};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use haider_protocol::tool::{COMPUTER_SCREENSHOT_MAX_DIMENSION, COMPUTER_SCREENSHOT_MAX_PIXELS};

/// Resizes a PNG into the computer-use compatibility envelope while keeping
/// both axes available to the backend's exact delivered-to-native mapping.
/// Generic tool images retain their wider CU-1 allowance. In-envelope input is
/// returned byte-for-byte.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn bound_computer_screenshot_png(png: &[u8]) -> ComputerResult<Vec<u8>> {
    use haider_protocol::tool::{
        TOOL_RESULT_IMAGE_MAX_DECODE_ALLOC, TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES,
        TOOL_RESULT_IMAGE_MAX_SOURCE_PIXELS,
    };
    use image::imageops::FilterType;
    use image::{DynamicImage, ImageFormat, ImageReader, Limits};
    use std::io::Cursor;

    let failure = |error: String| ComputerError::Backend {
        message: format!("computer screenshot resize: {error}"),
    };
    if png.len() > TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES {
        return Err(failure("source exceeds image byte limit".into()));
    }
    let (width, height) = ImageReader::with_format(Cursor::new(png), ImageFormat::Png)
        .into_dimensions()
        .map_err(|error| failure(error.to_string()))?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0 || height == 0 || pixels > TOOL_RESULT_IMAGE_MAX_SOURCE_PIXELS {
        return Err(failure(format!(
            "source dimensions {width}x{height} exceed the safe image limit"
        )));
    }
    let Some((target_width, target_height)) = computer_screenshot_target_size(width, height) else {
        return Ok(png.to_vec());
    };

    let mut reader = ImageReader::with_format(Cursor::new(png), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_alloc = Some(TOOL_RESULT_IMAGE_MAX_DECODE_ALLOC);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| failure(error.to_string()))?;
    let resized = image.resize_exact(target_width, target_height, FilterType::Lanczos3);
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(resized.into_rgba8())
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|error| failure(error.to_string()))?;
    Ok(output.into_inner())
}

/// Android captures use the mobile screenshot path. The desktop computer
/// backend has no screenshot source there, so it cannot bound or admit one.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn bound_computer_screenshot_png(_png: &[u8]) -> ComputerResult<Vec<u8>> {
    Err(ComputerError::Unavailable {
        platform: std::env::consts::OS.into(),
        message: "desktop computer screenshots unavailable on this platform".into(),
    })
}

/// Delivered size for a non-empty `width`x`height` capture, or `None` when it
/// already fits the envelope. One aspect-preserving scale satisfies both the
/// long-edge and pixel-area limits; each axis is floored so neither limit is
/// exceeded by rounding.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn computer_screenshot_target_size(width: u32, height: u32) -> Option<(u32, u32)> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width <= COMPUTER_SCREENSHOT_MAX_DIMENSION
        && height <= COMPUTER_SCREENSHOT_MAX_DIMENSION
        && pixels <= COMPUTER_SCREENSHOT_MAX_PIXELS
    {
        return None;
    }
    let dimension_scale =
        f64::from(COMPUTER_SCREENSHOT_MAX_DIMENSION) / f64::from(width.max(height));
    let pixel_scale = (COMPUTER_SCREENSHOT_MAX_PIXELS as f64 / pixels as f64).sqrt();
    let scale = dimension_scale.min(pixel_scale).min(1.0);
    let target_width = (f64::from(width) * scale).floor().max(1.0) as u32;
    let target_height = (f64::from(height) * scale).floor().max(1.0) as u32;
    debug_assert!(target_width <= COMPUTER_SCREENSHOT_MAX_DIMENSION);
    debug_assert!(target_height <= COMPUTER_SCREENSHOT_MAX_DIMENSION);
    debug_assert!(
        u64::from(target_width) * u64::from(target_height) <= COMPUTER_SCREENSHOT_MAX_PIXELS
    );
    Some((target_width, target_height))
}

#[cfg(all(
    test,
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
#[path = "screenshot_bounds_tests.rs"]
mod tests;

#[cfg(all(
    test,
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
#[path = "screenshot_bounds_fallback_tests.rs"]
mod fallback_tests;
