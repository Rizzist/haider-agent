//! Pure Windows computer-use geometry, compiled on EVERY platform so its
//! arithmetic runs in macOS/Linux CI; only `windows.rs` (GDI/SendInput) uses it
//! at runtime. Model pixels map into the captured region of the physical
//! virtual desktop with `floor(model_pixel * region_extent / admitted_extent)`;
//! SendInput `MOUSEEVENTF_VIRTUALDESK` normalization is always against the
//! complete virtual desktop, even after a region crop.

use super::{ComputerError, ComputerResult};
use haider_protocol::computer::ScreenPoint;

const ABSOLUTE_MAX: u64 = 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VirtualScreen {
    pub(super) left: i32,
    pub(super) top: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NativePoint {
    pub(super) x: i32,
    pub(super) y: i32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Viewport {
    pub(super) desktop: VirtualScreen,
    pub(super) image_region: VirtualScreen,
    pub(super) image_width: u32,
    pub(super) image_height: u32,
}

pub(super) fn crop_region(
    desktop: VirtualScreen,
    crop: super::ComputerScreenshotCrop,
) -> ComputerResult<VirtualScreen> {
    if crop.source_width != desktop.width
        || crop.source_height != desktop.height
        || crop.width == 0
        || crop.height == 0
        || u64::from(crop.x) + u64::from(crop.width) > u64::from(desktop.width)
        || u64::from(crop.y) + u64::from(crop.height) > u64::from(desktop.height)
    {
        return Err(ComputerError::InvalidAction {
            message: "invalid Windows screenshot crop bounds".into(),
        });
    }
    let left = i64::from(desktop.left) + i64::from(crop.x);
    let top = i64::from(desktop.top) + i64::from(crop.y);
    Ok(VirtualScreen {
        left: i32::try_from(left).map_err(|_| ComputerError::InvalidAction {
            message: "region x exceeds native range".into(),
        })?,
        top: i32::try_from(top).map_err(|_| ComputerError::InvalidAction {
            message: "region y exceeds native range".into(),
        })?,
        width: crop.width,
        height: crop.height,
    })
}

pub(super) fn map_delivered_pixel(
    viewport: Viewport,
    point: ScreenPoint,
) -> ComputerResult<NativePoint> {
    if point.x >= viewport.image_width || point.y >= viewport.image_height {
        return Err(ComputerError::InvalidAction {
            message: format!(
                "computer coordinate ({}, {}) is outside the delivered {}x{} screenshot",
                point.x, point.y, viewport.image_width, viewport.image_height
            ),
        });
    }
    let relative_x = u64::from(point.x) * u64::from(viewport.image_region.width)
        / u64::from(viewport.image_width);
    let relative_y = u64::from(point.y) * u64::from(viewport.image_region.height)
        / u64::from(viewport.image_height);
    let x = i64::from(viewport.image_region.left)
        .checked_add(
            i64::try_from(relative_x).map_err(|_| ComputerError::Backend {
                message: "mapped Windows x coordinate conversion failed".into(),
            })?,
        )
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| ComputerError::InvalidAction {
            message: "mapped Windows x coordinate exceeds the virtual-screen range".into(),
        })?;
    let y = i64::from(viewport.image_region.top)
        .checked_add(
            i64::try_from(relative_y).map_err(|_| ComputerError::Backend {
                message: "mapped Windows y coordinate conversion failed".into(),
            })?,
        )
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| ComputerError::InvalidAction {
            message: "mapped Windows y coordinate exceeds the virtual-screen range".into(),
        })?;
    Ok(NativePoint { x, y })
}

pub(super) fn normalize_absolute(
    screen: VirtualScreen,
    point: NativePoint,
) -> ComputerResult<(i32, i32)> {
    let relative_x = i64::from(point.x) - i64::from(screen.left);
    let relative_y = i64::from(point.y) - i64::from(screen.top);
    if relative_x < 0
        || relative_y < 0
        || relative_x >= i64::from(screen.width)
        || relative_y >= i64::from(screen.height)
    {
        return Err(ComputerError::InvalidAction {
            message: "mapped Windows coordinate is outside the captured virtual screen".into(),
        });
    }
    Ok((
        normalize_axis(relative_x as u32, screen.width),
        normalize_axis(relative_y as u32, screen.height),
    ))
}

pub(super) fn normalize_viewport_point(
    viewport: Viewport,
    point: NativePoint,
) -> ComputerResult<(i32, i32)> {
    // SendInput's VIRTUALDESK flag always uses the complete virtual desktop,
    // even when the last screenshot shown to the model was a crop.
    normalize_absolute(viewport.desktop, point)
}

fn normalize_axis(offset: u32, extent: u32) -> i32 {
    if extent <= 1 {
        0
    } else {
        (u64::from(offset) * ABSOLUTE_MAX / u64::from(extent - 1)) as i32
    }
}

/// Inverse of [`map_delivered_pixel`] for the live cursor: a native
/// virtual-desktop point inside the captured region -> delivered model pixel.
pub(super) fn model_pixel_for_native(
    viewport: Viewport,
    point: NativePoint,
) -> ComputerResult<(u32, u32)> {
    let relative_x = i64::from(point.x) - i64::from(viewport.image_region.left);
    let relative_y = i64::from(point.y) - i64::from(viewport.image_region.top);
    if relative_x < 0
        || relative_y < 0
        || relative_x >= i64::from(viewport.image_region.width)
        || relative_y >= i64::from(viewport.image_region.height)
    {
        return Err(ComputerError::InvalidAction {
            message: "cursor is outside the region captured by the latest computer screenshot"
                .into(),
        });
    }
    let x = u64::try_from(relative_x).map_err(|_| ComputerError::Backend {
        message: "Windows cursor X coordinate conversion failed".into(),
    })? * u64::from(viewport.image_width)
        / u64::from(viewport.image_region.width);
    let y = u64::try_from(relative_y).map_err(|_| ComputerError::Backend {
        message: "Windows cursor Y coordinate conversion failed".into(),
    })? * u64::from(viewport.image_height)
        / u64::from(viewport.image_region.height);
    Ok((x as u32, y as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_cu1_pixels_map_through_negative_virtual_origin() {
        let desktop = VirtualScreen {
            left: -1_920,
            top: -200,
            width: 5_120,
            height: 1_800,
        };
        let viewport = Viewport {
            desktop,
            image_region: desktop,
            image_width: 2_560,
            image_height: 900,
        };
        let point = match map_delivered_pixel(viewport, ScreenPoint { x: 960, y: 450 }) {
            Ok(point) => point,
            Err(error) => panic!("point must map: {error}"),
        };
        assert_eq!(point, NativePoint { x: 0, y: 700 });
        assert!(map_delivered_pixel(viewport, ScreenPoint { x: 2_560, y: 0 }).is_err());
    }

    fn cropped_viewport(
        desktop: VirtualScreen,
        crop: super::super::ComputerScreenshotCrop,
        image_width: u32,
        image_height: u32,
    ) -> Viewport {
        Viewport {
            desktop,
            image_region: crop_region(desktop, crop)
                .unwrap_or_else(|error| panic!("valid crop: {error}")),
            image_width,
            image_height,
        }
    }

    #[test]
    fn offset_crop_normalizes_against_full_desktop() {
        let desktop = VirtualScreen {
            left: 0,
            top: 0,
            width: 1_920,
            height: 1_080,
        };
        let viewport = cropped_viewport(
            desktop,
            super::super::ComputerScreenshotCrop {
                x: 400,
                y: 200,
                width: 800,
                height: 600,
                source_width: 1_920,
                source_height: 1_080,
            },
            800,
            600,
        );
        for (model, native, normalized) in [
            (
                ScreenPoint { x: 0, y: 0 },
                NativePoint { x: 400, y: 200 },
                (13_660, 12_147),
            ),
            (
                ScreenPoint { x: 799, y: 599 },
                NativePoint { x: 1_199, y: 799 },
                (40_946, 48_528),
            ),
        ] {
            assert_eq!(map_delivered_pixel(viewport, model), Ok(native));
            assert_eq!(normalize_viewport_point(viewport, native), Ok(normalized));
        }
        assert_ne!(
            normalize_absolute(desktop, NativePoint { x: 400, y: 200 }),
            Ok((0, 0))
        );
    }

    #[test]
    fn negative_origin_downscaled_crop_maps_edge_pixels() {
        let desktop = VirtualScreen {
            left: -1_920,
            top: -200,
            width: 5_120,
            height: 1_800,
        };
        let viewport = cropped_viewport(
            desktop,
            super::super::ComputerScreenshotCrop {
                x: 400,
                y: 200,
                width: 800,
                height: 600,
                source_width: 5_120,
                source_height: 1_800,
            },
            400,
            300,
        );
        assert_eq!(
            map_delivered_pixel(viewport, ScreenPoint { x: 0, y: 0 }),
            Ok(NativePoint { x: -1_520, y: 0 })
        );
        assert_eq!(
            map_delivered_pixel(viewport, ScreenPoint { x: 399, y: 299 }),
            Ok(NativePoint { x: -722, y: 598 })
        );
        assert!(map_delivered_pixel(viewport, ScreenPoint { x: 400, y: 0 }).is_err());
        assert!(map_delivered_pixel(viewport, ScreenPoint { x: 0, y: 300 }).is_err());
    }

    #[test]
    fn mapped_pixels_round_trip_through_absolute_virtual_desktop() {
        let desktops = [
            VirtualScreen {
                left: 0,
                top: 0,
                width: 1_920,
                height: 1_080,
            },
            VirtualScreen {
                left: -1_920,
                top: -200,
                width: 5_120,
                height: 1_800,
            },
        ];
        for desktop in desktops {
            let crop = super::super::ComputerScreenshotCrop {
                x: 400,
                y: 200,
                width: 800,
                height: 600,
                source_width: desktop.width,
                source_height: desktop.height,
            };
            for (width, height) in [(800, 600), (400, 300), (1_429, 804)] {
                let viewport = cropped_viewport(desktop, crop, width, height);
                for y in (0..height).step_by(7).chain(std::iter::once(height - 1)) {
                    for x in (0..width).step_by(7).chain(std::iter::once(width - 1)) {
                        let native = map_delivered_pixel(viewport, ScreenPoint { x, y })
                            .unwrap_or_else(|error| panic!("pixel maps into desktop: {error}"));
                        let (dx, dy) = normalize_viewport_point(viewport, native)
                            .unwrap_or_else(|error| panic!("native point normalizes: {error}"));
                        // Approximate SendInput's inclusive 0..=65535 ->
                        // virtual-desktop pixel conversion at each axis.
                        let landed_x = i64::from(desktop.left)
                            + i64::from(dx) * i64::from(desktop.width - 1) / 65_535;
                        let landed_y = i64::from(desktop.top)
                            + i64::from(dy) * i64::from(desktop.height - 1) / 65_535;
                        assert!((landed_x - i64::from(native.x)).abs() <= 1);
                        assert!((landed_y - i64::from(native.y)).abs() <= 1);
                    }
                }
            }
        }
    }

    #[test]
    fn virtual_screen_pixels_normalize_to_sendinput_absolute_space() {
        let screen = VirtualScreen {
            left: -1_920,
            top: -200,
            width: 5_120,
            height: 1_800,
        };
        assert_eq!(
            normalize_absolute(screen, NativePoint { x: -1_920, y: -200 }),
            Ok((0, 0))
        );
        assert_eq!(
            normalize_absolute(screen, NativePoint { x: 3_199, y: 1_599 }),
            Ok((65_535, 65_535))
        );
    }

    #[test]
    fn live_cursor_maps_back_into_delivered_crop_pixels() {
        let desktop = VirtualScreen {
            left: -1_920,
            top: -200,
            width: 5_120,
            height: 1_800,
        };
        let viewport = Viewport {
            desktop,
            image_region: crop_region(
                desktop,
                super::super::ComputerScreenshotCrop {
                    x: 400,
                    y: 200,
                    width: 800,
                    height: 600,
                    source_width: 5_120,
                    source_height: 1_800,
                },
            )
            .unwrap_or_else(|error| panic!("valid crop: {error}")),
            image_width: 400,
            image_height: 300,
        };
        assert_eq!(
            model_pixel_for_native(viewport, NativePoint { x: -1_520, y: 0 }),
            Ok((0, 0))
        );
        assert_eq!(
            model_pixel_for_native(viewport, NativePoint { x: -721, y: 599 }),
            Ok((399, 299))
        );
        for (x, y) in [(0, 0), (123, 45), (399, 299)] {
            let native = map_delivered_pixel(viewport, ScreenPoint { x, y })
                .unwrap_or_else(|error| panic!("pixel maps: {error}"));
            assert_eq!(model_pixel_for_native(viewport, native), Ok((x, y)));
        }
        for outside in [
            NativePoint { x: -1_521, y: 0 },
            NativePoint { x: -720, y: 0 },
            NativePoint { x: -1_520, y: 600 },
        ] {
            assert!(model_pixel_for_native(viewport, outside).is_err());
        }
    }

    #[test]
    fn crop_outside_or_mismatched_with_desktop_is_rejected() {
        let desktop = VirtualScreen {
            left: 0,
            top: 0,
            width: 1_920,
            height: 1_080,
        };
        let crop = |x, width, source_width| super::super::ComputerScreenshotCrop {
            x,
            y: 0,
            width,
            height: 10,
            source_width,
            source_height: 1_080,
        };
        assert!(crop_region(desktop, crop(1_900, 20, 1_920)).is_ok());
        assert!(crop_region(desktop, crop(1_901, 20, 1_920)).is_err());
        assert!(crop_region(desktop, crop(0, 0, 1_920)).is_err());
        assert!(crop_region(desktop, crop(0, 10, 2_560)).is_err());
    }
}
