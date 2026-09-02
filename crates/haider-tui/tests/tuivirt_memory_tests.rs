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
//! so the pin does not depend on host RSS noise. The process-level RSS pin
//! (`scripts/perf/client-footprint-budget.py`) has no long-transcript
//! surface yet — see `docs/testing/v0.0.970/tuivirt-tests.md` "needs a
//! hook".
//!
//! IGNORED TODAY, deliberately: the shipped cache pre-renders every entry,
//! so first-frame retention is O(N) and a 50k-row session retains ~50× the
//! 1k-row figure. The implementation lane removes the `#[ignore]`; the
//! numbers print either way:
//!
//! ```text
//! cargo test -p haider-tui --test tuivirt_memory_tests -- --ignored --nocapture
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
use std::sync::atomic::{AtomicUsize, Ordering};

mod tuivirt_common;
use tuivirt_common::replayed;

static LIVE: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let resized = unsafe { System.realloc(pointer, layout, new_size) };
        if !resized.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            LIVE.fetch_add(new_size, Ordering::Relaxed);
        }
        resized
    }
}

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// Bytes retained by the model (raw transcript + projection) and by the
/// first frame (layout cache + frame buffers), measured separately.
struct Retained {
    rows: usize,
    model_bytes: usize,
    render_bytes: usize,
}

fn retained(rows: usize) -> Retained {
    let before_model = live();
    let model: AppModel = replayed(rows);
    let model_bytes = live().saturating_sub(before_model);
    let mut terminal = Terminal::new(TestBackend::new(118, 36)).expect("test terminal");
    let before_render = live();
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
    let render_bytes = live().saturating_sub(before_render);
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
#[ignore = "target memory shape of the tuivirt re-architecture: today's first frame retains an O(N) pre-rendered cache; un-ignore when the bounded render cache lands"]
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

/// Always on: the allocator accounting itself is sound (a Vec that grows
/// and drops returns the counter to where it started) — the ignored pin
/// cannot pass or fail for a bookkeeping reason.
#[test]
fn counting_allocator_balances_alloc_and_free() {
    let before = live();
    let grown: Vec<u8> = (0..1_000_000u32).map(|n| n as u8).collect();
    assert!(live() >= before + 1_000_000, "growth is counted");
    let mut boxed = grown.into_boxed_slice().into_vec();
    boxed.push(1);
    drop(boxed);
    assert_eq!(live(), before, "release is counted back");
}
