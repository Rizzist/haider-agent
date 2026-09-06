#![allow(clippy::unwrap_used)]
use super::*;
fn region() -> ComputerScreenshotRegion {
    ComputerScreenshotRegion {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
        reference_width: 100,
        reference_height: 100,
    }
}
#[test]
fn region_validation_rejects_empty_outside_and_overflow() {
    for value in [
        ComputerScreenshotRegion {
            width: 0,
            ..region()
        },
        ComputerScreenshotRegion {
            reference_height: 0,
            ..region()
        },
        ComputerScreenshotRegion { x: 99, ..region() },
        ComputerScreenshotRegion {
            x: u32::MAX,
            ..region()
        },
    ] {
        assert!(value.validate().is_err());
    }
    assert!(region().validate().is_ok());
    assert!(region().resolve(0, 100).is_err());
}
#[test]
fn region_retina_crop_is_exact_and_rounds_outward() {
    assert_eq!(
        region().resolve(200, 200).unwrap(),
        ComputerScreenshotCrop {
            x: 20,
            y: 40,
            width: 60,
            height: 80,
            source_width: 200,
            source_height: 200
        }
    );
    let crop = region().resolve(123, 147).unwrap();
    assert_eq!((crop.x, crop.y, crop.width, crop.height), (12, 29, 38, 60));
}
#[test]
fn region_png_preserves_native_edges_and_redaction() {
    use super::super::{
        ExcludeRegionScreenshotRedaction, ScreenshotRedactionPolicy, ScreenshotRedactionRegion,
    };
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;
    let mut source = RgbaImage::from_pixel(200, 200, Rgba([255, 255, 255, 255]));
    source.put_pixel(21, 41, Rgba([255, 0, 0, 255]));
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(source)
        .write_to(&mut png, ImageFormat::Png)
        .unwrap();
    let policy = ExcludeRegionScreenshotRedaction::new(vec![ScreenshotRedactionRegion {
        x: 30,
        y: 50,
        width: 5,
        height: 5,
    }])
    .unwrap();
    let redacted = policy.redact_png(png.get_ref()).unwrap();
    let (cropped, _) = crop_screenshot_png(&redacted, region()).unwrap();
    let decoded = image::load_from_memory(&cropped).unwrap().into_rgba8();
    assert_eq!(decoded.dimensions(), (60, 80));
    assert_eq!(*decoded.get_pixel(1, 1), Rgba([255, 0, 0, 255]));
    assert_eq!(*decoded.get_pixel(2, 1), Rgba([255, 255, 255, 255]));
    assert_eq!(*decoded.get_pixel(10, 10), Rgba([0, 0, 0, 255]));
}
