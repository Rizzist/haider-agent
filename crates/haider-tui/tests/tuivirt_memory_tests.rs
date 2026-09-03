//! tuivirt memory pin scaffold (v0.0.970).
//!
//! The re-architecture's memory law: the RENDER side of the client stays
//! flat as the session grows — a 50k-row session may retain at most 1.5×
//! what a 1k-row session retains after its first frame, because only a
//! viewport window is ever laid out and cached. The raw transcript itself
//! is O(N) by construction (bytes + small metadata) and is reported but
//! not gated here.
//!
//! Measurement is IN-PROCESS and deterministic: a counting global allocator
//! (the `haider-provider::allocation_probe` pattern) tracks live heap bytes,
//! so the pin does not depend on host RSS noise. The process-level harness
//! (`scripts/perf/client-footprint-budget.py --tui-replay-rows N`) separately
//! exercises the real interactive binary; see the lane report for its
//! environment-qualified RSS samples.
//!
//! The pin is always enabled. It reports model/raw bytes separately from the
//! bounded render-side retention:
//!
//! ```text
//! cargo test -p haider-tui --test tuivirt_memory_tests -- --nocapture
//! ```
#![allow(clippy::expect_used, clippy::unwrap_used)]
// The counting allocator is the one sanctioned reason for `unsafe` in a
// test binary (precedent: `crates/haider-provider/src/lib.rs`
// `allocation_probe`). It wraps `System` and only counts.
#![allow(unsafe_code)]

use haider_tui::app::AppModel;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

mod tuivirt_common;
use tuivirt_common::{push_agent, replayed, session_model};

thread_local! {
    /// Allocator accounting is scoped to the current test thread. The test
    /// harness may allocate and free concurrently, so a process-global
    /// counter makes an otherwise deterministic retention pin flaky.
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static TRACKED_BYTES: Cell<isize> = const { Cell::new(0) };
}

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            track_delta(isize::try_from(layout.size()).unwrap_or(isize::MAX));
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            track_delta(isize::try_from(layout.size()).unwrap_or(isize::MAX));
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        track_delta(-isize::try_from(layout.size()).unwrap_or(isize::MAX));
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let resized = unsafe { System.realloc(pointer, layout, new_size) };
        if !resized.is_null() {
            track_delta(
                isize::try_from(new_size).unwrap_or(isize::MAX)
                    - isize::try_from(layout.size()).unwrap_or(isize::MAX),
            );
        }
        resized
    }
}

fn track_delta(delta: isize) {
    let _ = TRACKING.try_with(|tracking| {
        if tracking.get() {
            let _ = TRACKED_BYTES.try_with(|bytes| bytes.set(bytes.get().saturating_add(delta)));
        }
    });
}

struct AllocationScope;

impl AllocationScope {
    fn start() -> Self {
        TRACKED_BYTES.with(|bytes| bytes.set(0));
        TRACKING.with(|tracking| tracking.set(true));
        Self
    }

    fn current(&self) -> usize {
        TRACKED_BYTES.with(|bytes| usize::try_from(bytes.get()).unwrap_or(0))
    }

    fn finish(self) -> usize {
        let bytes = self.current();
        TRACKING.with(|tracking| tracking.set(false));
        bytes
    }
}

/// Bytes retained by the model (raw transcript + projection) and by the
/// first frame (layout cache + frame buffers), measured separately.
struct Retained {
    rows: usize,
    model_bytes: usize,
    render_bytes: usize,
}

fn retained(rows: usize) -> Retained {
    let model_scope = AllocationScope::start();
    let model: AppModel = replayed(rows);
    let model_bytes = model_scope.finish();
    let mut terminal = Terminal::new(TestBackend::new(118, 36)).expect("test terminal");
    let render_scope = AllocationScope::start();
    terminal
        .draw(|frame| {
            render(&model, frame);
        })
        .expect("draw");
    // A second frame at the same position: a stable cache retains nothing
    // new; whatever the first frame kept is the render-side footprint.
    terminal
        .draw(|frame| {
            render(&model, frame);
        })
        .expect("draw");
    let render_bytes = render_scope.finish();
    drop(terminal);
    drop(model);
    Retained {
        rows,
        model_bytes,
        render_bytes,
    }
}

const RATIO: f64 = 1.5;

#[test]
fn render_side_retention_is_flat_from_1k_to_50k_rows() {
    let _ = retained(64);
    let small = retained(1_000);
    let large = retained(50_000);
    for r in [&small, &large] {
        println!(
            "tuivirt memory @ {} rows: model={} KiB render={} KiB total={} KiB",
            r.rows,
            r.model_bytes / 1024,
            r.render_bytes / 1024,
            (r.model_bytes + r.render_bytes) / 1024
        );
    }
    let ceiling = (small.render_bytes as f64 * RATIO) as usize;
    assert!(
        large.render_bytes <= ceiling,
        "render-side retention must stay within {RATIO}× from 1k to 50k rows: \
         1k={} KiB 50k={} KiB (ceiling {} KiB)",
        small.render_bytes / 1024,
        large.render_bytes / 1024,
        ceiling / 1024
    );
}

/// An extreme single entry keeps raw text in the model but retains only its
/// visible formatted window. This catches accidentally restoring the old
/// full-entry `Vec<Line>` cache while the many-entry ratio still looks flat.
#[test]
fn megabyte_entry_retains_only_a_viewport_window() {
    let mut model = session_model();
    let mut text = String::with_capacity(1 << 20);
    while text.len() < (1 << 20) {
        text.push_str("the quick brown fox jumps over the lazy dog — viewport window probe\n");
    }
    push_agent(&mut model, "memory-megabyte", &text);
    let mut terminal = Terminal::new(TestBackend::new(118, 36)).expect("test terminal");
    let render_scope = AllocationScope::start();
    for _ in 0..2 {
        terminal
            .draw(|frame| {
                render(&model, frame);
            })
            .expect("draw");
    }
    let retained = render_scope.finish();
    println!("tuivirt megabyte render retention={} KiB", retained / 1024);
    assert!(
        retained <= 256 * 1024,
        "a 1 MiB raw entry must retain only a viewport-sized layout: {} KiB",
        retained / 1024
    );
}

/// Always on: the allocator accounting itself is sound (a Vec that grows
/// and drops returns the counter to where it started) — the retention pin
/// cannot pass or fail for a bookkeeping reason.
#[test]
fn counting_allocator_balances_alloc_and_free() {
    let scope = AllocationScope::start();
    let grown: Vec<u8> = (0..1_000_000u32).map(|n| n as u8).collect();
    assert!(scope.current() >= 1_000_000, "growth is counted");
    let mut boxed = grown.into_boxed_slice().into_vec();
    boxed.push(1);
    drop(boxed);
    assert_eq!(scope.finish(), 0, "release is counted back");
}
