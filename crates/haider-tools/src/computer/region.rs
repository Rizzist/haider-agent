//! Region crops are applied after full-frame redaction and before CU-1 admission.
use super::{ComputerError, ComputerResult};
use serde::{Deserialize, Serialize};

/// Region in a full-display reference screenshot (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerScreenshotRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub reference_width: u32,
    pub reference_height: u32,
}

/// Exact native pixel rectangle actually encoded, for subsequent input mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputerScreenshotCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
}

impl ComputerScreenshotRegion {
    pub fn validate(self) -> ComputerResult<()> {
        if self.width == 0
            || self.height == 0
            || self.reference_width == 0
            || self.reference_height == 0
            || u64::from(self.x) + u64::from(self.width) > u64::from(self.reference_width)
            || u64::from(self.y) + u64::from(self.height) > u64::from(self.reference_height)
        {
            return Err(ComputerError::InvalidAction {
                message: "screenshot region must be nonempty and inside its full-display reference dimensions".into(),
            });
        }
        Ok(())
    }

    pub fn resolve(
        self,
        source_width: u32,
        source_height: u32,
    ) -> ComputerResult<ComputerScreenshotCrop> {
        self.validate()?;
        if source_width == 0 || source_height == 0 {
            return Err(ComputerError::InvalidAction {
                message: "screenshot source must be nonempty".into(),
            });
        }
        let scale = |v: u32, source: u32, reference: u32| {
            (u64::from(v) * u64::from(source) / u64::from(reference)) as u32
        };
        let x = scale(self.x, source_width, self.reference_width);
        let y = scale(self.y, source_height, self.reference_height);
        // Round the far edge outward; mapping uses this exact integer crop.
        let right = (u64::from(self.x + self.width) * u64::from(source_width))
            .div_ceil(u64::from(self.reference_width)) as u32;
        let bottom = (u64::from(self.y + self.height) * u64::from(source_height))
            .div_ceil(u64::from(self.reference_height)) as u32;
        Ok(ComputerScreenshotCrop {
            x,
            y,
            width: right - x,
            height: bottom - y,
            source_width,
            source_height,
        })
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn crop_screenshot_png(
    png: &[u8],
    region: ComputerScreenshotRegion,
) -> ComputerResult<(Vec<u8>, ComputerScreenshotCrop)> {
    use haider_protocol::tool::{
        TOOL_RESULT_IMAGE_MAX_DECODE_ALLOC, TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES,
        TOOL_RESULT_IMAGE_MAX_SOURCE_PIXELS,
    };
    use image::{ImageFormat, ImageReader, Limits};
    use std::io::Cursor;
    let failure = |error| ComputerError::Backend {
        message: format!("screenshot region: {error}"),
    };
    if png.len() > TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES {
        return Err(failure("source exceeds image byte limit".to_string()));
    }
    let (width, height) = ImageReader::with_format(Cursor::new(png), ImageFormat::Png)
        .into_dimensions()
        .map_err(|error| failure(error.to_string()))?;
    if u64::from(width) * u64::from(height) > TOOL_RESULT_IMAGE_MAX_SOURCE_PIXELS {
        return Err(failure("source exceeds image pixel limit".to_string()));
    }
    let crop = region.resolve(width, height)?;
    let mut reader = ImageReader::with_format(Cursor::new(png), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_alloc = Some(TOOL_RESULT_IMAGE_MAX_DECODE_ALLOC);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| failure(error.to_string()))?;
    let mut output = Cursor::new(Vec::new());
    image
        .crop_imm(crop.x, crop.y, crop.width, crop.height)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|error| failure(error.to_string()))?;
    Ok((output.into_inner(), crop))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn crop_screenshot_png(
    _png: &[u8],
    _region: ComputerScreenshotRegion,
) -> ComputerResult<(Vec<u8>, ComputerScreenshotCrop)> {
    Err(ComputerError::Unavailable {
        platform: std::env::consts::OS.into(),
        message: "screenshot regions unavailable on this platform".into(),
    })
}

#[cfg(test)]
#[path = "region_tests.rs"]
mod tests;
