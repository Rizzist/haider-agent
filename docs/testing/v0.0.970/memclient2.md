# memclient2 — TUI client memory, part 2

Date: 2026-09-03

Lane: `lane-970-memclient2`

Tested base: `c6ffd7adc565f3b7cef3fe12517ffe3920ede543`

## Verdict

The image-buffer, prompt-recall, allocator-maintenance, measurement-harness,
and CI-budget changes are implemented and their focused and package tests are
green. The fixed 20-turn A/B shows a material 901,120-byte settled-footprint
reduction with no material CPU or peak-memory regression.

This lane is nevertheless **NO_SHIP**. Its dense transcript geometry remains
`O(history)` and can exceed the nominal 512 KiB layout-cache ceiling once the
history is sufficiently large. An individually larger-than-cap visible row is
also formatted again on later redraws. A proposed extreme-row workaround was
removed after independent review found inexact row mapping, unbounded newline
indexing, incomplete item-kind coverage, and visible-content truncation. The
accepted sparse/u64 transcript implementation from `tuivirt` must be reconciled
before this lane can ship, and all hold-outs must then be rerun.

## Provenance and citation drift

- The common brief named `wave-970@8952219`; this worktree actually started at
  `c6ffd7a`, which was also `origin/wave-970` when the baseline binaries were
  frozen. No merge or rebase was performed during the lane.
- `docs/testing/v0.0.970/memclient.md`, claimed by the brief to be present, was
  absent from the starting tree.
- The named 32 MiB prompt-history cache is daemon-owned. This lane did not
  cross that territory; it bounded the separate TUI prompt-recall deques.
- While this lane was active, `origin/wave-970` advanced to
  `0cb2cfb2106e8a63b6fa552a10627244d1153109`. History contains the sparse
  `tuivirt` implementation at `8430c88` and its merge at `c9f042d`, but later
  commit `683da3c` replaced those TUI files with a dense version while landing
  unrelated turn-hygiene work. Integration must preserve the sparse
  `BTreeMap`/u64 viewport design and the later turn-hygiene behavior; choosing
  either broad file version wholesale is unsafe.
- The supplied lane brief, common rules, and `turnperf/` and `turnperf2/`
  evidence remain untracked and unmodified as requested.

## Implemented changes

### Terminal image buffer

- Capability detection retains only a deferred `Picker`; the bundled PNG is
  not decoded and no terminal image protocol exists until a non-empty image
  rectangle is actually drawn.
- First draw creates one fixed 24×2 `ratatui_image::Protocol`. The protocol
  contains no resizeable source and is retained for later frames, preventing
  per-frame re-encoding/reallocation as the launcher/session layout changes.
- Empty rectangles do nothing, decode failure is one-shot, and sessions that
  never draw the graphics wordmark remain deferred.
- After the first encode, the dead temporary image buffers trigger one Darwin
  allocator-pressure-relief request.

### TUI prompt recall and transcript cache

- Active and background prompt recall share one insertion path, capped at 128
  entries and 1 MiB of retained string capacity. An individually oversized
  prompt is not retained. Idle settlement shrinks strings and the deque.
- Transcript formatting no longer clones authoritative `TranscriptEntry`
  values into the cache. Exact per-entry revisions allow an ordinary mutation
  to reformat only the changed row; non-transcript semantic events reformat
  none.
- Retained formatted rows are capped at 128 entries/512 KiB, large uncached
  lines are moved into the current frame instead of duplicated, and settled
  caches are released once per revision. Active main/chip transcripts are not
  evicted by an unrelated background settlement.
- Remaining blocker: the dense `Vec<TranscriptGeometry>` is not itself bounded
  independently of history length, cold fill still walks/formats all rows, and
  a visible row whose formatted form alone exceeds 512 KiB cannot be retained.
  Therefore the cap is not a literal all-history layout bound and the cache
  work is not accepted for shipment.

### Allocator fragmentation

- Paste receipt: a transient paste capacity of at least 256 KiB requests
  pressure relief after reducer handling, when the zeroizing receipt copy is
  dead.
- Image: the first terminal-protocol encode requests relief once.
- Attach/history replay: raw JSON container capacities are accumulated without
  cloning; a valid `CaughtUp` carries that pressure to a scheduled post-frame
  maintenance pass. The pass releases only settled transcript caches, shrinks
  prompt recall, and calls relief when coalesced pressure is at least 256 KiB.
- Terminal settlement schedules the same post-frame path. A `CaughtUp` received
  after the last replay frame re-arms `dirty`, so maintenance cannot strand.
- The combined N=5 A/B below measured the shipped candidate behavior. Relief
  was not isolated as a separate on/off binary, so no causal byte saving is
  assigned to relief alone. The exact peak hold-out shows it did not produce a
  material peak regression. This missing isolated relief A/B is another reason
  not to promote the lane verdict.

### Measurement and CI

- `client-footprint-budget.py` uses `proc_pid_rusage` only. The forbidden
  `vmmap` path and flag were removed and never retried.
- Mach task-time ticks are converted through the timebase; raw tick fields are
  retained beside ns/us fields.
- `--tui-turns` accepts only 0 or exactly 20 and only for a TUI surface. Each
  turn waits for THINKING then IDLE and drains more than seven 33 ms frames.
  Immediate load gates bracket both CPU reads; the per-turn deadline is the
  derived `2 × 45 s = 90 s` headless-terminal budget.
- `m1-peak-case.sh --runs 5` now emits N=5 median/MAD aggregation for client
  peak, process-tree peak, and sanity growth. Its fake proxy emits the exact
  1,114,112-byte truncation response.
- The advisory sixel CI footprint budget is updated from 6,110,043 to
  **6,344,334 bytes**, exactly `ceil(5,767,576 × 1.10)`. Other surface and run
  budgets are unchanged.

## Frozen binaries

| binary | SHA-256 |
|---|---|
| baseline `haider` | `6cb52301cb48d370b2f9388895b50a0ceba57cf050c283e33bf2eb8e1be60f9c` |
| baseline `haiderd` | `ab01b8e981eb5dcb4dfe2f6198aa37f40e4c8983deaae1d8c92917255777d0e2` |
| candidate `haider` | `194edd8f83c408df32a7b7bc8db21b0b368a1a8ad1b7087138fcf4b2223765fd` |
| candidate `haiderd` | `9b2043a7a5c1ffeeecfc355977f4560cbf9d831d8546e6d81857661f7c1fe262` |

The candidate `haiderd` is 50 MiB, above registry #64's 10 MiB floor.

## Fixed 20-turn CPU/footprint A/B

Command shape for both frozen clients:

```text
python3 scripts/perf/client-footprint-budget.py \
  --haider <frozen-haider> --surface tui-demo-sixel \
  --runs 5 --settle-seconds 60 --load-limit 4 \
  --calibrate --tui-turns 20 --output <artifact-dir>
```

All 10 samples completed exactly 20 turns, observed alternate-screen and
graphics-query traffic, and passed the load gates.

| variant | footprint bytes, runs 1–5 | median | MAD |
|---|---|---:|---:|
| baseline | 6,668,696; 7,045,528; 6,668,696; 6,816,152; 6,537,624 | 6,668,696 | 131,072 |
| candidate | 5,767,576; 5,767,576; 5,702,040; 5,685,680; 5,931,416 | 5,767,576 | 65,536 |

| variant | exact 20-turn CPU ns, runs 1–5 | median | MAD |
|---|---|---:|---:|
| baseline | 938,656,917; 1,854,750,958; 1,618,707,667; 558,636,209; 1,377,910,208 | 1,377,910,208 | 439,253,291 |
| candidate | 1,469,653,458; 1,443,087,666; 600,169,583; 959,075,375; 1,988,874,042 | 1,443,087,666 | 484,012,291 |

Materiality result:

- Footprint delta: **−901,120 B (−13.5%)**, larger than the larger 131,072 B
  MAD: material improvement.
- CPU delta: **+65,177,458 ns (+4.7%)**, smaller than the larger 484,012,291 ns
  MAD: not material.

Raw summaries were retained outside the worktree at
`/private/tmp/memclient2-ab-baseline/summary.json` and
`/private/tmp/memclient2-ab-candidate/summary.json`.

## Exact 1.06 MiB peak hold-out

Both frozen clients ran the fake-proxy `patch_truncated` case at N=5 with
`truncation_bytes=1,114,112`. Baseline JSONL files were 3,382,273 bytes;
candidate files were 3,382,274 bytes, both inside the harness size window.

| metric | baseline runs (bytes) | baseline median/MAD | candidate runs (bytes) | candidate median/MAD | delta |
|---|---|---:|---|---:|---:|
| client peak | 14,188,544; 15,368,192; 15,482,880; 15,384,576; 14,614,528 | 15,368,192 / 114,688 | 15,384,576; 15,384,576; 15,400,960; 15,269,888; 15,450,112 | 15,384,576 / 16,384 | +16,384 |
| process-tree peak | 53,510,144; 53,706,752; 53,395,456; 53,395,456; 52,166,656 | 53,395,456 / 114,688 | 53,510,144; 53,035,008; 53,723,136; 52,166,656; 53,362,688 | 53,362,688 / 327,680 | −32,768 |
| sanity growth | 21,708,800 median | 21,708,800 / 770,048 | 22,806,528 median | 22,806,528 / 442,368 | +1,097,728 |

The two absolute peak deltas are smaller than their larger MADs, so peak is
not materially regressed. Sanity growth rose beyond its larger MAD because the
candidate pre-item baseline was lower; the absolute process-tree peak did not
rise and remains the peak hold-out authority.

Raw aggregate files were retained outside the worktree at
`/private/tmp/memclient2-m1-baseline/aggregate.json` and
`/private/tmp/memclient2-m1-candidate/aggregate.json`.

## Verification ledger

- `cargo test --locked -p haider-tui`: PASS (entire affected package,
  including doc tests).
- Focused ledger: 65/65 (`memclient2_cache_tests` 9,
  `app_render_tests` 28, `projection_tests` 22, `prompt_backtrack_tests` 5,
  `w3c3_render_bench_tests` 1).
- `python3 -m unittest ... test_client_footprint_budget.py`: 17/17.
- Footprint harness self-test: PASS.
- M1 RSS sampler self-test: PASS.
- `cargo run --locked -p xtask -- test-count`: 4,393, matching the updated
  baseline (4,384 + 9 new tests).
- Release build: PASS for `haider-cli` and `haider-daemond` under the required
  environment.
- `python3 -m py_compile`, `sh -n`, and `git diff --check`: PASS.
- Added `unsafe`: zero. Linux/Windows allocator behavior is a no-op by
  inspection; those platforms were not executed.

## CI error-registry walk

- #44: honored. `vmmap` was removed from the harness and not retried.
- #64: honored. Frozen candidate `haiderd` is 50 MiB.
- #94: honored. The new turn wait is derived as twice the existing 45-second
  terminal budget, documented as 90 seconds.
- #95: no new wait was inserted while a negotiated connection is open; the
  existing live driver remains responsible for connection servicing.

## Required reconciliation

1. Restore/preserve the sparse `BTreeMap`/u64 transcript viewport and mutation
   indexes from `8430c88`/`c9f042d` while retaining later turn-hygiene changes.
2. Layer the static image protocol, prompt-recall cap, replay-pressure
   accounting, and settled-only cache release onto that implementation.
3. Add literal cache-byte tests at 50k/200k entries and an unchanged-redraw
   test for a single formatted row larger than the cap.
4. Run an isolated allocator-relief on/off A/B for large paste, first image,
   and attach replay; make no causal relief claim before it passes.
5. Rebuild/refreeze and repeat both N=5 hold-outs before changing this verdict.

NO_SHIP
