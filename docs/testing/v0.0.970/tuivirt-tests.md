# tuivirt — behaviour-preservation pins (v0.0.970)

Lane `tuivirt` re-architects the transcript viewport (viewport-only layout,
estimated row heights corrected on measurement, a bounded render cache —
see `tuivirt-analysis.md`). It must not change what the user sees. The
tests below lock today's rendering and interaction so the re-architecture
has to keep them green; they pin OBSERVABLE OUTPUT (the `TestBackend`
cell grid — text and style —, the hit map, `scroll_back`/`scroll_max`)
and never reach into the cache.

Run (all four files, debug build, ~15 s):

```text
RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac \
cargo test -p haider-tui --test tuivirt_golden_tests --test tuivirt_scroll_tests \
  --test tuivirt_shape_bench_tests --test tuivirt_memory_tests
```

Regenerate goldens ONLY for an owner-approved visual change:

```text
UPDATE_TUIVIRT_GOLDENS=1 cargo test -p haider-tui --test tuivirt_golden_tests
```

then review `git diff crates/haider-tui/tests/fixtures/tuivirt/`. A golden
that moves without such a change is a regression in the lane, not a stale
fixture.

Files:

| file | role |
| --- | --- |
| `crates/haider-tui/tests/tuivirt_common/mod.rs` | shared scaffold: empty LIVE session (`session_model`), the bench-shaped `replayed(N)`, `draw` → `Snapshot` (cells + display rows + hits + transcript rect), the golden dump/check, `assert_same_frame` |
| `crates/haider-tui/tests/tuivirt_golden_tests.rs` | 13 golden-frame tests, 40 fixtures (13 scenarios × 3 sizes + `megabyte_reply_top.118x36`) under `tests/fixtures/tuivirt/` |
| `crates/haider-tui/tests/tuivirt_scroll_tests.rs` | 7 scroll-model / cache-invariant pins on 10k-row replays |
| `crates/haider-tui/tests/tuivirt_shape_bench_tests.rs` | the ledger-row-17 replacement shape gate (`#[ignore]`d — cannot pass today) + its always-on arithmetic pin |
| `crates/haider-tui/tests/tuivirt_memory_tests.rs` | in-process render-side retention pin (`#[ignore]`d — cannot pass today) + its always-on allocator pin |

Golden format: per frame row, `NN|display row|` (wide-glyph continuation
cells dropped so CJK reads naturally) followed by `~ fg/bg/modifier×count`
style runs over EVERY cell. The crate version in the header is masked as
`v<VERSION>` so a release bump never invalidates a fixture. Every scenario
is pinned at 80x24, 118x36 (the bench size) and 160x50; the three
different widths are what make the wrap/table-breakpoint behaviour a pin
rather than a single sample.

Baseline construction: the pins use an EMPTY live session
(`RuntimeMode::Live`, `upsert_live_session` + `open_session`), so a
transcript holds exactly what the test pushes. `w3c3_render_bench_tests::
replayed` attaches the demo's first sample session, which carries seeded
history rows; that difference is deliberate and documented in
`tuivirt_common::session_model`.

## Behaviour → test → what breaks if it fails

### Golden frames (`tuivirt_golden_tests.rs`)

| behaviour | test(s) / fixture | if it fails |
| --- | --- | --- |
| An empty session draws the header, rule, one breathing row, the composer band and status line — nothing else | `empty_session_frames` → `empty_session.*` | the viewport paints phantom rows or loses chrome when the transcript is empty |
| Markdown headings keep their `#` marks, bullets/numbers align, bold/emphasis/inline-code restyle without markers, blockquotes rail | `markdown_headings_and_lists_frames` → `markdown_headings_lists.*` (cf. `f2_markdown_tests::headings_keep_marks_and_style_the_text`, `bullets_and_numbers_mark_aligned` at the `md` seam) | the incremental layout re-flows markdown differently from the all-at-once render |
| Fenced code blocks restyle as a block, keep line geometry, and the text after the fence returns to prose styling | `fenced_code_blocks_frames` → `fenced_code_blocks.*` (cf. `f2_markdown_tests::fenced_code_restyles_without_touching_line_geometry`) | fences lose their pill/ground or bleed into the following paragraph |
| Tool rows: completed = one row (glyph · maroon name · dim desc); running = pulsing glyph at phase 0; failed = err glyph; a process call shows its retained output tail; past 8 KiB the honesty marker; command rows show `· exit N`; `fs_edit` file-change row | `tool_call_boxes_collapsed_and_expanded_frames` → `tool_call_boxes.*` (cf. `w8b_render_tests`) | multi-row entries (output tails) get mis-measured, the truncation marker disappears, or a dynamic (pulsing) row renders a stale phase |
| Wide tables render as a grid at 160/118 and stack at 80 with the same source; a six-column ledger does the same | `wide_tables_frames` → `wide_tables.*` (cf. `g5_table_tests` LB1 breakpoint at the `md` seam) | width-keyed layout is not re-derived at the new width (estimated heights never corrected) |
| A 300-char unbreakable user token and a 40-clause single logical line wrap by display cells with the rail preserved; a long URL wraps | `long_wrapped_lines_frames` → `long_wrapped_lines.*` | wrap points move (height estimate used as truth), the rail drops off continuation rows |
| CJK, emoji (ZWJ sequences, flags), combining marks and Devanagari wrap by display width, never splitting a wide glyph | `cjk_emoji_combining_frames` → `cjk_emoji_combining.*` | width tables cached at ingest disagree with the render-time measurement |
| A ≥ 1 MiB single assistant reply: the tail is bottom-anchored at all three sizes and the very top of the entry renders exactly at 118x36 | `megabyte_reply_frames` → `megabyte_reply_tail.*`, `megabyte_reply_top.118x36` | the extreme-single-row cap/expander changes visible text, or the far end of a huge entry lands on the wrong row |
| A blocking question menu (3 options) replaces the composer; the options are always visible | `input_required_menu_frames` → `input_required_menu.*` | the menu band steals/loses transcript rows differently |
| A zero-option ask keeps the composer as the answer line and renders the question above it | `input_required_ask_frames` → `input_required_ask.*` | same as above for the ask shape |
| The scripted demo turn (plan, streamed text, tool call, answered permission, file change, usage) | `demo_session_frames` → `demo_session.*` | the realistic mix regresses anywhere |
| A streaming partial reply with an unterminated span shows the thinking tail at phase 0 | `streaming_tail_frames` → `streaming_tail.*` | the tail (prefix/suffix lines outside the cache) is placed wrong |
| Scrolled to the middle of a 60-entry history: sticky prompt band on the transcript's top row, bare `Jump to bottom ↓` chip bottom-right, both as hits | `scrolled_history_with_sticky_and_jump_chip_frames` → `scrolled_history.*` | sticky lookup (`user_rows`) or the chip geometry moves |

### Scroll model + cache invariants (`tuivirt_scroll_tests.rs`, 10k-row replay at 118x36 and 80x24)

| behaviour | test | if it fails |
| --- | --- | --- |
| A wheel notch moves the transcript by exactly 3 rows, a drag-autoscroll step by 1, from the bottom, the middle and the top; clamped at both ends; the reverse step restores the frame cell-for-cell; at the bottom the tail row sits above exactly one blank row; at the top the frame reads blank / `■ haider` / `row 0 —` | `wheel_and_drag_steps_from_bottom_middle_and_top_of_10k_rows` (cf. `sim_parity_r2_tests::wheel_clamps_to_the_rendered_scroll_range`, `qol_drag_autoscroll_tests::*`) | scroll coordinates stop being global wrapped rows, or estimated heights make a notch move a different number of rows |
| The frame at a scroll position is a pure function of (transcript, width, theme, `scroll_back`): reached by scrolling (warm cache) == fresh model at that offset (cold cache); the ceiling is identical | `a_scroll_position_renders_identically_warm_and_cold` | the bounded cache or idle layout renders differently depending on history (stale/uncorrected estimates) |
| Following: appended rows land at the tail, `scroll_back` stays 0, no chip. Scrolled back: the view keeps its DISTANCE FROM THE TAIL — appended rows slide the interior up by exactly their height (`scroll_back` is a bottom offset; pinned as-is, it is today's semantics) — and the chip counts `N new`. Jump-to-bottom returns the exact following frame; scrolling back again shows a bare chip | `follow_mode_and_jump_to_bottom_behave_as_today` (cf. `review3_fix_tests::bottom_band_counts_unseen_and_click_returns_to_follow`) | follow/unseen bookkeeping changes, or the re-architecture silently switches to row anchoring |
| A width change re-wraps from the cache exactly like a fresh render at the new width (118→100), the round trip restores the exact frame, the tail stays anchored through 80/160/118, widening from the top stays at the top (offset clamps to the new ceiling) | `resize_rewraps_like_a_fresh_render_and_keeps_the_tail_and_top_anchored` (cf. `review2_fix_tests::wheel_before_first_frame_and_resize_never_bank_debt`, `review4_fix_tests::wheel_notch_between_resize_and_redraw_is_honored`) | width-keyed geometry survives a resize, or the anchored tail/top drifts |
| Every streamed delta shows in the very next frame; completion replaces the streamed text; a tool row flips its glyph the frame after its status flips; a duplicate completion of an earlier item id is a no-op frame; after `Done` the edited history equals a fresh model fed the final history | `edits_appends_and_completions_never_render_stale_rows` | the cache serves stale rows after an edit/append (revision keying broken) |
| A theme switch re-renders from the cache exactly like a fresh render at that theme; switching back restores the frame | `theme_switch_rerenders_from_the_cache_like_a_fresh_render` | interned styles outlive a theme change |
| Clicking the sticky band puts the producing prompt on the transcript's top row, suppresses the band, and the landed frame equals a fresh render at the chosen offset (30 prompts × 100 rows) | `sticky_jump_lands_the_producing_prompt_on_the_transcripts_top_row` (cf. `sim_parity_r2_tests::sticky_origin_line_pins_the_prompt_and_click_stays_at_it`, `review3_fix_tests::sticky_jump_suppresses_the_bar_until_a_real_wheel`) | jump targets computed from estimated heights land on the wrong row |

Node (`/tree`) jumps are already pinned by `b2b_m3_tree_tests::enter_on_a_
node_row_lands_the_render_resolved_jump`, `jump_geometry_survives_wrapping_
newlines_wide_glyphs_and_widths` and `jump_resolves_after_resize_with_fresh_
geometry`; they are not duplicated here.

### Shape gate — ledger row 17 replacement (`tuivirt_shape_bench_tests.rs`)

| behaviour | test | status |
| --- | --- | --- |
| First frame ≤ 33 ms and cached p95 (following AND mid-scroll) ≤ 33 ms at 10k / 50k / 200k rows, both flat within 20 % (+1 ms slack) from 10k to 200k, `--release` only (debug prints SKIP) | `first_frame_and_cached_p95_are_flat_from_10k_to_200k_rows` | `#[ignore]` — CANNOT PASS TODAY: the shipped cache fills O(N) on the first frame (`w3c3_render_bench_tests` pins the current law as `< 250 ms` cold @ 10k). The implementation lane deletes the `#[ignore]` line; nothing else changes. Run: `cargo test --release -p haider-tui --test tuivirt_shape_bench_tests -- --ignored --nocapture` |
| The gate's own arithmetic (1.2× + 1 ms flatness ceiling, percentile pick) | `shape_gate_arithmetic_is_pinned` | always on, green |

The existing row-17 bench (`w3c3_render_bench_tests::cached_viewport_
render_stays_bounded_through_10k_rows`) stays in place — its bounds are
upper bounds and remain true after the re-architecture.

### Memory pin (`tuivirt_memory_tests.rs`)

| behaviour | test | status |
| --- | --- | --- |
| Render-side retention after the first frame (layout cache + frame buffers, measured by a counting global allocator) for a 50k-row session ≤ 1.5× a 1k-row session | `render_side_retention_is_flat_from_1k_to_50k_rows` | `#[ignore]` — CANNOT PASS TODAY. Measured 2026-09-02 (debug): 1k rows model 537 KiB / render 1224 KiB; 50k rows model 29 399 KiB / render 66 384 KiB (54×). Run: `cargo test -p haider-tui --test tuivirt_memory_tests -- --ignored --nocapture` |
| The allocator bookkeeping balances alloc/free | `counting_allocator_balances_alloc_and_free` | always on, green |

The raw transcript (`model` bytes above) is O(N) by construction and is
reported, not gated.

## Not yet pinned / needs a hook

* **Client RSS at 50k rows (process level).** `scripts/perf/client-footprint-budget.py`
  has `tui-demo-no-graphics` / `tui-demo-sixel` surfaces that spawn
  `haider tui --demo`; the demo transcript is fixed-size, so there is no
  long-transcript surface. Hook needed: a replay surface (e.g. a fixture
  session store the TUI attaches to, or a `HAIDER_DEMO_REPLAY_ROWS=N`
  seam in the demo script) so the script can measure a 1k-row vs 50k-row
  settled RSS with its usual `proc_pid_rusage` samples. Until then the
  in-process allocator pin above is the memory gate.
* **Keyboard paging.** PageUp/PageDown/Home/End are not bound in the
  session transcript today (Home/End are composer keys); "page" scrolling
  exists only as wheel notches (3 rows) and drag autoscroll (1 row), which
  are pinned. If the lane adds paging keys, pin them the same way
  (`exercise_step` takes any `fn(&mut AppModel, bool)`).
* **Transcript search.** There is no transcript text search in the TUI
  (the only searches are the `/` palette and the model picker); nothing to
  pin.
* **Row estimates at 200k+ rows vs the `u16` scroll space.** 200k
  replayed rows exceed 65 535 wrapped rows; today's coordinates saturate
  (ledger row 17 calls it a separate compatibility seam). The shape gate
  measures 200k but no pin states what the user sees past saturation.
* **Phase-dynamic rows across many phases.** Running tool rows are pinned
  at `anim_phase = 0` only; the pulse ink per phase is not golden'd.
* **Subagent (chip) transcripts, the plan document surface and image
  rows** render through the same cache but are not golden'd here (existing
  coverage: `s3_subagent_timeline_tests`, `plan_surface_tests`,
  `image_created_tui_tests`).
* **Release-build timing evidence for the shape gate** was not captured in
  this lane (no `--release` build in the time box); the row-17 bench's
  ~250 ms cold @ 10k is the reference.
