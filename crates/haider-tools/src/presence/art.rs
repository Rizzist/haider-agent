//! Rasterised overlay art shared by every desktop presence renderer.
//!
//! The shapes are drawn in software (4×4 supersampling) so the macOS,
//! Windows and X11 overlays show the identical agent pointer without a
//! platform vector API, and so the art is unit-testable. Output is straight
//! (non-premultiplied) RGBA8, row-major, top-left origin.

/// Haider gold: the agent pointer fill, deliberately unlike any system cursor.
pub const POINTER_FILL: [u8; 4] = [0xF2, 0xA9, 0x00, 0xFF];
/// Dark outline around the pointer and inside the badge.
pub const INK: [u8; 4] = [0x14, 0x14, 0x18, 0xFF];
/// Light halo so the pointer reads on dark and light content alike.
pub const HALO: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xE6];
/// Badge background.
pub const BADGE_FILL: [u8; 4] = [0x16, 0x16, 0x1C, 0xF0];
/// "Live" dot and Stop button.
pub const STOP_FILL: [u8; 4] = [0xE5, 0x48, 0x4D, 0xFF];

/// Classic arrow outline in a 20×30 design box, tip at the origin.
const ARROW: [(f64, f64); 7] = [
    (0.0, 0.0),
    (0.0, 24.0),
    (6.0, 18.5),
    (10.5, 28.0),
    (14.5, 26.2),
    (10.2, 17.0),
    (18.0, 17.0),
];

/// Design-box margin so the halo is not clipped. The pointer tip therefore
/// sits at `(POINTER_TIP * scale, POINTER_TIP * scale)` in the bitmap.
pub const POINTER_TIP: f64 = 3.0;
/// Pointer bitmap size in points.
pub const POINTER_SIZE: (f64, f64) = (24.0, 34.0);

/// Badge geometry in points.
pub const BADGE_SIZE: (f64, f64) = (372.0, 36.0);
/// Stop button rectangle inside the badge, in points from the badge's
/// top-left corner: `(x, y, width, height)`.
pub const BADGE_STOP_RECT: (f64, f64, f64, f64) = (300.0, 5.0, 64.0, 26.0);
/// Click-ring bitmap size (square) in points.
pub const RING_SIZE: f64 = 44.0;

/// One RGBA8 bitmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Bitmap {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; width as usize * height as usize * 4],
        }
    }

    /// Straight-alpha pixel at `(x, y)`.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let index = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.rgba[index],
            self.rgba[index + 1],
            self.rgba[index + 2],
            self.rgba[index + 3],
        ]
    }

    /// Copy with premultiplied alpha, BGRA order (Win32 `UpdateLayeredWindow`).
    #[must_use]
    pub fn premultiplied_bgra(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.rgba.len());
        for pixel in self.rgba.chunks_exact(4) {
            let alpha = u32::from(pixel[3]);
            let scale = |channel: u8| u8::try_from(u32::from(channel) * alpha / 255).unwrap_or(255);
            out.extend_from_slice(&[scale(pixel[2]), scale(pixel[1]), scale(pixel[0]), pixel[3]]);
        }
        out
    }

    /// PNG encoding for platforms that load images from data (AppKit).
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let image = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| "overlay bitmap has inconsistent dimensions".to_owned())?;
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Png)
            .map_err(|error| error.to_string())?;
        Ok(bytes.into_inner())
    }
}

const SAMPLES: u32 = 4;

/// Renders `shade(x, y) -> Option<[u8;4]>` (point-space sample → colour) with
/// 4×4 supersampling into a bitmap of `width_pt × height_pt` points at
/// `scale` pixels per point.
fn render(
    width_pt: f64,
    height_pt: f64,
    scale: f64,
    shade: impl Fn(f64, f64) -> Option<[u8; 4]>,
) -> Bitmap {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale.min(4.0)
    } else {
        1.0
    };
    let width = (width_pt * scale).ceil().max(1.0) as u32;
    let height = (height_pt * scale).ceil().max(1.0) as u32;
    let mut bitmap = Bitmap::new(width, height);
    let total = f64::from(SAMPLES * SAMPLES);
    for py in 0..height {
        for px in 0..width {
            let mut acc = [0.0f64; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (f64::from(px) + (f64::from(sx) + 0.5) / f64::from(SAMPLES)) / scale;
                    let y = (f64::from(py) + (f64::from(sy) + 0.5) / f64::from(SAMPLES)) / scale;
                    if let Some(color) = shade(x, y) {
                        let alpha = f64::from(color[3]) / 255.0;
                        acc[0] += f64::from(color[0]) * alpha;
                        acc[1] += f64::from(color[1]) * alpha;
                        acc[2] += f64::from(color[2]) * alpha;
                        acc[3] += alpha;
                    }
                }
            }
            let index = (py as usize * width as usize + px as usize) * 4;
            if acc[3] > 0.0 {
                bitmap.rgba[index] = (acc[0] / acc[3]).round() as u8;
                bitmap.rgba[index + 1] = (acc[1] / acc[3]).round() as u8;
                bitmap.rgba[index + 2] = (acc[2] / acc[3]).round() as u8;
                bitmap.rgba[index + 3] = ((acc[3] / total) * 255.0).round() as u8;
            }
        }
    }
    bitmap
}

fn point_in_polygon(polygon: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut inside = false;
    let mut previous = polygon[polygon.len() - 1];
    for &current in polygon {
        if (current.1 > y) != (previous.1 > y)
            && x < (previous.0 - current.0) * (y - current.1) / (previous.1 - current.1) + current.0
        {
            inside = !inside;
        }
        previous = current;
    }
    inside
}

fn distance_to_segment(point: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((point.0 - cx).powi(2) + (point.1 - cy).powi(2)).sqrt()
}

fn distance_to_polygon_edge(polygon: &[(f64, f64)], x: f64, y: f64) -> f64 {
    let mut best = f64::INFINITY;
    let mut previous = polygon[polygon.len() - 1];
    for &current in polygon {
        best = best.min(distance_to_segment((x, y), previous, current));
        previous = current;
    }
    best
}

/// The agent pointer: gold arrow, dark outline, light halo. The tip is at
/// `(POINTER_TIP, POINTER_TIP)` points.
#[must_use]
pub fn pointer(scale: f64) -> Bitmap {
    let arrow: Vec<(f64, f64)> = ARROW
        .iter()
        .map(|&(x, y)| (x + POINTER_TIP, y + POINTER_TIP))
        .collect();
    render(POINTER_SIZE.0, POINTER_SIZE.1, scale, |x, y| {
        let inside = point_in_polygon(&arrow, x, y);
        let edge = distance_to_polygon_edge(&arrow, x, y);
        if inside && edge > 1.3 {
            Some(POINTER_FILL)
        } else if inside || edge <= 1.3 {
            Some(INK)
        } else if edge <= 2.6 {
            Some(HALO)
        } else {
            None
        }
    })
}

/// A click ring (drawn fully opaque; renderers fade it with window alpha).
#[must_use]
pub fn click_ring(scale: f64) -> Bitmap {
    let center = RING_SIZE / 2.0;
    render(RING_SIZE, RING_SIZE, scale, |x, y| {
        let distance = ((x - center).powi(2) + (y - center).powi(2)).sqrt();
        if (distance - 15.0).abs() <= 2.5 {
            Some(POINTER_FILL)
        } else if (distance - 15.0).abs() <= 3.6 {
            Some(INK)
        } else {
            None
        }
    })
}

fn inside_rounded_rect(x: f64, y: f64, rect: (f64, f64, f64, f64), radius: f64) -> bool {
    let (left, top, width, height) = rect;
    if x < left || y < top || x > left + width || y > top + height {
        return false;
    }
    let cx = x.clamp(left + radius, left + width - radius);
    let cy = y.clamp(top + radius, top + height - radius);
    (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius
}

/// Badge background: dark pill, red live dot and a red Stop pill whose
/// label is drawn by the platform text renderer.
#[must_use]
pub fn badge(scale: f64) -> Bitmap {
    let (width, height) = BADGE_SIZE;
    render(width, height, scale, |x, y| {
        if inside_rounded_rect(x, y, BADGE_STOP_RECT, BADGE_STOP_RECT.3 / 2.0) {
            return Some(STOP_FILL);
        }
        let dot = ((x - 18.0).powi(2) + (y - height / 2.0).powi(2)).sqrt();
        if dot <= 5.0 {
            return Some(STOP_FILL);
        }
        if inside_rounded_rect(x, y, (0.0, 0.0, width, height), height / 2.0) {
            Some(BADGE_FILL)
        } else {
            None
        }
    })
}

/// Whether a point (badge-local, top-left origin, in points) hits Stop.
#[must_use]
pub fn badge_stop_hit(x: f64, y: f64) -> bool {
    let (left, top, width, height) = BADGE_STOP_RECT;
    x >= left && x <= left + width && y >= top && y <= top + height
}
