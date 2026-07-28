//! W3c3 M3 — the render benchmark (report §6.3's last test line).
//!
//! "render benchmark records p95 on 1k/3k/5k-row replays and activates the
//! ledgered cache only at the stated threshold."
//!
//! The ledger rows this measures (`docs/OPTIMIZATIONS.md`):
//!
//! * row 14 — per-block rendered-line cache, trigger "scroll-back over long
//!   sessions", stated as arriving with the SESSION-ATTACH wave (this one);
//! * row 17 — wrapped-segment/height cache + viewport-only block selection,
//!   trigger ">~2-3k logical rows or p95 render >8-10ms".
//!
//! R12 is explicit that these stay behind their MEASURED triggers and that
//! high-risk performance rewrites must not ride the seam. So this file does
//! not add a cache — the W3C seam report's stays-put list names `render`
//! explicitly, and a viewport/wrapped-height rewrite is exactly the kind of
//! remodel that would fail R11. What it DOES is make the trigger
//! self-reporting: it measures, prints the table, and then enforces that
//! the LEDGER agrees with the measurement in both directions. A trigger
//! that fires without the ledger saying so fails; a ledger that claims a
//! trigger which no longer fires fails too. "We'll measure it later" stops
//! being a promise and becomes a gate.
//!
//! Timing is noisy on a shared machine, so the enforced timing assertions
//! are deliberately coarse (order-of-magnitude regression guards, not a
//! stopwatch); the printed p95s are the evidence a reviewer reads.
//!
//! ONLY AN OPTIMIZED BUILD MEASURES THE THRESHOLD. Row 17's numbers
//! (8-10 ms) describe the shipped binary; an unoptimized build is ~15x
//! slower and comparing it to them would either fire the trigger falsely
//! forever or force a debug-shaped threshold that means nothing. So the
//! ledger gate runs under `--release` and prints a loud SKIP otherwise —
//! the probe ladder's own discipline: a bypassed check is announced, never
//! silently folded into a pass. Run it with:
//!
//! ```text
//! cargo test --release -p haider-tui --test w3c3_render_bench_tests -- --nocapture
//! ```
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::app::{AppModel, Screen};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::{Duration, Instant};

mod common;
use common::launcher_model;

/// Row 17's stated p95 trigger.
const P95_TRIGGER: Duration = Duration::from_millis(10);
/// Row 17's stated row-count trigger (the lower bound of ">~2-3k").
const ROWS_TRIGGER: usize = 2_000;
/// The exact phrase `docs/OPTIMIZATIONS.md` must carry once the p95
/// trigger has fired. Keeping the string here — and asserting on it — is
/// what stops the ledger and the measurement from drifting apart.
const TRIGGER_MARKER: &str = "TRIGGERED W3c3";

/// A model whose attached session holds `rows` transcript rows, built the
/// way a REPLAY builds them: committed item envelopes through the reducer.
fn replayed(rows: usize) -> AppModel {
    let mut model = launcher_model();
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    for n in 0..rows {
        model
            .projection
            .apply(&EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(format!("bench-{n}")),
                item: TurnItem::AgentMessage {
                    text: format!(
                        "row {n} — a representative agent line with enough words to wrap at \
                         a normal terminal width and exercise the measurement path"
                    ),
                },
            }));
    }
    model.screen = Screen::Session;
    model
}

/// Samples per size. A debug build is ~15x slower and is only ever
/// informational here, so it takes far fewer.
fn samples() -> usize {
    if cfg!(debug_assertions) { 8 } else { 60 }
}

/// p95 of `samples` full frames at 118x36.
fn p95_frame(model: &AppModel, samples: usize) -> Duration {
    let backend = TestBackend::new(118, 36);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        terminal
            .draw(|frame| {
                render(model, frame);
            })
            .expect("draw succeeds");
        timings.push(start.elapsed());
    }
    timings.sort_unstable();
    timings[(samples * 95 / 100).min(samples - 1)]
}

#[test]
fn render_p95_on_1k_3k_and_5k_row_replays_is_recorded_against_the_ledger() {
    // Warm the allocator/terminal once so the first sample is not the
    // benchmark.
    let _ = p95_frame(&replayed(64), 3);

    let mut table = Vec::new();
    for rows in [1_000_usize, 3_000, 5_000] {
        let model = replayed(rows);
        let p95 = p95_frame(&model, samples());
        println!("render p95 @ {rows} rows = {p95:?}");
        table.push((rows, p95));
    }

    // The recorded evidence, asserted only where a law exists.
    //
    // 1. The scroll-back render is bounded by the VIEWPORT, not by history:
    //    a 5x longer transcript must not cost 5x a frame. This is the
    //    property row 17's cache would protect, and the one whose loss
    //    would make the ledger row urgent rather than open.
    let (_, p95_1k) = table[0];
    let (_, p95_5k) = table[2];
    // (This is a coarse REGRESSION guard, not a claim of sublinearity: the
    // measured curve today is roughly linear in history, which is exactly
    // why row 17 exists. What must never happen is a super-linear blow-up.)
    //
    // W3c3.1 (review P3-8): M3.2 widened this from `8x + 4ms` to
    // `12x + 50ms` for flake headroom without recording that cases the old
    // guard rejected now pass. Re-measured over five release runs on the
    // reference machine, the WITHIN-RUN 5k:1k ratio was 5.11 · 6.17 · 6.41 ·
    // 6.68 · 6.77 (1k p95 9.5-15.7ms, 5k p95 50.9-96.9ms). `8x + 20ms`
    // clears every run with ≥28% headroom while rejecting a 7x blow-up —
    // the original ratio restored, with a small absolute cushion for the
    // fact that the two sizes are measured minutes apart on a machine that
    // may be building something else.
    // Debug builds skip the TIMING comparison exactly like the ledger gate
    // below them (the bounds above are five-run RELEASE measurements; an
    // unoptimized parallel-workspace run measured 132ms@1k and flaked 1-in-3
    // even isolated — post-merge gate, 2026-07-28). The render paths still
    // executed above (crash coverage); release CI enforces the ratio.
    if cfg!(debug_assertions) {
        println!(
            "ratio gate = SKIP (unoptimized build; measured 1k={p95_1k:?} \
             5k={p95_5k:?}). Run with --release to enforce."
        );
    } else {
        assert!(
            p95_5k < p95_1k * 8 + Duration::from_millis(20),
            "render cost must not blow up super-linearly with history: \
             1k={p95_1k:?} 5k={p95_5k:?}"
        );
    }

    // 2. THE LEDGER GATE, both directions. Row 17's trigger is ">~2-3k
    //    logical rows OR p95 render >8-10ms" — either half fires it.
    //
    //    This used to be spelled `&&` (review W3c3 P2-7): a machine fast
    //    enough to render 3k rows under 10 ms would have reported
    //    `fired = false` and demanded the marker be REMOVED, retiring a
    //    trigger whose row-count half had plainly fired. The operator now
    //    matches the row it gates. Note the consequence, deliberately: the
    //    row-count half is true by construction at these sizes, so this
    //    benchmark can no longer authorize removing the marker — only a
    //    benchmark that stops replaying ≥2k rows could, and the rewrite
    //    itself is pinned separately by the cache-absence check below.
    //
    //    MUTATION CHECK: flip `TRIGGER_MARKER` out of docs/OPTIMIZATIONS.md
    //    (or change row 17's status back to `planned`) and this fails with
    //    the measured numbers in hand. Second MUTATION CHECK: change the
    //    `||` back to `&&` and a sub-10ms 3k render passes the gate with
    //    the marker removed.
    let rows_fired = table.iter().any(|(rows, _)| *rows >= ROWS_TRIGGER);
    let p95_fired = table.iter().any(|(_, p95)| *p95 >= P95_TRIGGER);
    let fired = rows_fired || p95_fired;
    let ledger = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/OPTIMIZATIONS.md"),
    )
    .expect("the optimization ledger is readable");
    let recorded = ledger.contains(TRIGGER_MARKER);
    if cfg!(debug_assertions) {
        println!(
            "ledger gate = SKIP (unoptimized build; row 17's 8-10ms threshold describes \
             the SHIPPED binary). ledger records `{TRIGGER_MARKER}` = {recorded}. \
             Run with --release to enforce."
        );
        assert!(
            recorded,
            "the ledger must carry the recorded W3c3 measurement either way"
        );
        return;
    }
    assert_eq!(
        fired, recorded,
        "the ledger and the measurement must agree. measured p95s: {table:?}; \
         row-count half fired = {rows_fired}; p95 half fired = {p95_fired}; \
         ledger records `{TRIGGER_MARKER}` = {recorded}. \
         If it fired, record it (the wrapped-segment/height cache is now REQUIRED \
         work, not planned work — and per R12 it is the NEXT lane's, never the \
         seam's). If it stopped firing, remove the marker."
    );

    // 3. And the ledgered cache is NOT active: W3c3 measures the trigger,
    //    it does not implement the rewrite (R12: "leave high-risk
    //    performance rewrites behind their measured thresholds"). A cache
    //    would make the SECOND identical frame dramatically cheaper than
    //    the first; the p95 over 60 identical frames would collapse.
    let model = replayed(5_000);
    let first = {
        let backend = TestBackend::new(118, 36);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let start = Instant::now();
        terminal
            .draw(|frame| {
                render(&model, frame);
            })
            .expect("draw");
        start.elapsed()
    };
    println!("render cold-frame @ 5000 rows = {first:?}");
    // W3c3.1 (review P3-8): M3.2 widened this ceiling from 250ms to 400ms.
    // Five release runs on the reference machine measured 46.0 · 50.7 ·
    // 51.2 · 56.8 · 61.1 ms — 4x under the original 250ms — so the
    // widening is retired rather than merely explained.
    assert!(
        first < Duration::from_millis(250),
        "even a cold 5k-row frame must stay far under a human frame budget"
    );
    // The stated test: a memoizing cache would make every frame AFTER the
    // first dramatically cheaper, so the steady-state p95 would collapse
    // relative to the cold frame. It does not — every frame rebuilds every
    // line, which is exactly the cost row 17 exists to remove.
    //
    // MUTATION CHECK: add a per-block rendered-line cache to `render.rs`
    // and this fails, correctly: the ledger row would then be DONE, not
    // TRIGGERED, and the two-way gate above must move with it.
    assert!(
        p95_5k > first / 4,
        "no rendered-line cache is active in W3c3 (R12 keeps the rewrite out \
         of the seam): steady-state p95 {p95_5k:?} must not collapse against \
         the cold frame {first:?}"
    );
}
