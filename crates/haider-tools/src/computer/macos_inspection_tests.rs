#![allow(clippy::expect_used)]
use super::*;

#[test]
fn inspection_finalization_maps_raw_ax_from_crop_to_new_full_downscaled_image() {
    let backend = MacOsComputerBackend::new();
    backend.lock_state().expect("state").pending_display_bounds = Some(CGRect {
        origin: CGPoint { x: 100.0, y: 50.0 },
        size: CGSize {
            width: 1440.0,
            height: 900.0,
        },
    });
    backend
        .set_viewport_region(
            800,
            600,
            Some(super::super::ComputerScreenshotCrop {
                x: 400,
                y: 200,
                width: 800,
                height: 600,
                source_width: 2880,
                source_height: 1800,
            }),
        )
        .expect("old cropped viewport");
    let raw = CGRect {
        origin: CGPoint { x: 400.0, y: 200.0 },
        size: CGSize {
            width: 10.0,
            height: 10.0,
        },
    };
    let old = map_accessibility_bounds(backend.viewport().expect("viewport"), raw);
    assert_eq!(
        old,
        Some(ComputerInspectionBounds {
            x: 200,
            y: 100,
            width: 20,
            height: 20
        })
    );
    backend
        .lock_state()
        .expect("state")
        .pending_inspection_bounds = Some(raw);
    let mut inspection = ComputerInspection {
        role: None,
        label: None,
        title: None,
        bounds: old,
        value: None,
    };
    // Inspect captures the full display, then CU-1 admits it at these dimensions.
    backend
        .set_viewport(720, 450)
        .expect("new admitted full image");
    backend
        .finalize_inspection(&mut inspection)
        .expect("finalize");
    assert_eq!(
        inspection.bounds,
        Some(ComputerInspectionBounds {
            x: 150,
            y: 75,
            width: 5,
            height: 5
        })
    );
    backend
        .finalize_inspection(&mut inspection)
        .expect("consume only once");
    assert_eq!(inspection.bounds, None);
}

#[test]
fn discarded_inspection_does_not_reuse_raw_ax_bounds() {
    let backend = MacOsComputerBackend::new();
    backend
        .lock_state()
        .expect("state")
        .pending_inspection_bounds = Some(CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: 10.0,
            height: 10.0,
        },
    });
    backend
        .discard_inspection()
        .expect("discard failed/cancelled inspection");
    assert!(
        backend
            .lock_state()
            .expect("state")
            .pending_inspection_bounds
            .is_none()
    );
}
