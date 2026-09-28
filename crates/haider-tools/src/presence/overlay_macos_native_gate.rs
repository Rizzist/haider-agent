//! Native window-server assertions for the harness=false AppKit executable.

use super::*;

fn wait_until_on_screen(numbers: &[u32; 3]) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if matches!(panels_absent(numbers), Ok(false)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("AppKit panel did not enter the real on-screen window list");
}

fn wait_until_off_screen(numbers: &[u32; 3]) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if matches!(panels_absent(numbers), Ok(true)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("AppKit panels did not leave the real on-screen window list");
}

pub(super) fn run() {
    let Some(mtm) = MainThreadMarker::new() else {
        panic!("native gate must run on the process main thread");
    };
    if NSScreen::mainScreen(mtm).is_none()
        || CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, kCGNullWindowID)
            .is_none_or(|windows| windows.is_empty())
    {
        eprintln!("NATIVE_GATE: SKIP: no window-server session or primary screen");
        return;
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    let mut overlay = match Overlay::new(mtm) {
        Ok(overlay) => overlay,
        Err(error) => panic!("create the real overlay panels: {error}"),
    };
    for panel in [&overlay.pointer, &overlay.ring, &overlay.badge] {
        panel.setSharingType(NSWindowSharingType::ReadOnly);
    }
    let numbers = [
        overlay.pointer.windowNumber(),
        overlay.ring.windowNumber(),
        overlay.badge.windowNumber(),
    ]
    .map(|number| match u32::try_from(number) {
        Ok(number) => number,
        Err(error) => panic!("valid panel window number: {error}"),
    });

    // Each of the three ids must individually prevent a successful verify.
    for panel in [&overlay.pointer, &overlay.ring, &overlay.badge] {
        panel.orderFrontRegardless();
        CATransaction::flush();
        wait_until_on_screen(&numbers);
        assert!(
            verify_panels_absent(&numbers).is_err(),
            "an ordered-front panel was incorrectly verified absent"
        );
        panel.orderOut(None);
        CATransaction::flush();
        wait_until_off_screen(&numbers);
    }

    // An absent first list cannot Ack until a fresh display callback arrives.
    BLOCK_GATE_TICKS.store(true, Ordering::Release);
    assert!(
        verify_panels_absent(&numbers).is_err(),
        "an absent list without a display tick was acknowledged"
    );
    BLOCK_GATE_TICKS.store(false, Ordering::Release);

    // Re-front between the first absent list and the refresh. The second
    // window-list query must catch the newly visible panel.
    assert!(
        verify_panels_absent_after_first_list(&numbers, || {
            overlay.badge.orderFrontRegardless();
            CATransaction::flush();
            wait_until_on_screen(&numbers);
        })
        .is_err(),
        "panel reappeared before the second list check"
    );

    assert!(matches!(panels_absent(&numbers), Ok(false)));
    let failed = overlay.conceal_capturable(91, |renderer| {
        // Re-front after the real orderOut so the native verification call
        // must refuse Ack. This makes both M1n and M2n observable.
        renderer.badge.orderFrontRegardless();
        CATransaction::flush();
        wait_until_on_screen(&numbers);
    });
    assert!(
        matches!(failed, PresenceEvent::ConcealFailed { seq: 91, .. }),
        "native Conceal acknowledged a panel still on screen: {failed:?}"
    );
    assert!(matches!(panels_absent(&numbers), Ok(false)));

    let ack = overlay.conceal_capturable(92, |_| {});
    assert_eq!(ack, PresenceEvent::Ack { seq: 92 });
    assert!(matches!(panels_absent(&numbers), Ok(true)));
    for index in 0..3 {
        match index {
            0 => overlay.pointer.orderFrontRegardless(),
            1 => overlay.ring.orderFrontRegardless(),
            _ => overlay.badge.orderFrontRegardless(),
        }
        CATransaction::flush();
        wait_until_on_screen(&numbers);
        let ack = overlay.conceal_capturable(100 + index as u64, |_| {});
        assert_eq!(
            ack,
            PresenceEvent::Ack {
                seq: 100 + index as u64
            }
        );
        wait_until_off_screen(&numbers);
    }
    overlay.pointer.orderOut(None);
    overlay.ring.orderOut(None);
    overlay.badge.orderOut(None);
    CATransaction::flush();
    eprintln!("NATIVE_GATE: PASS: visible panels refused; real orderOut acknowledged");
}
