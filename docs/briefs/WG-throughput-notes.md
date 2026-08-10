# W-G live throughput indicator notes

A live token-throughput readout above the composer while a turn streams —
`Throughput ▁▂▃▄▅ 126 tps · μ 119 · p95 154`. Implemented per
`docs/briefs/WG-throughput-brief.md` on branch `wg-throughput` (off
v0.0.82). TUI-only: a pure tps tracker feeding a status row, driven by the
streaming output-token count on the EXISTING frame clock — no new timer,
zero idle cost, an honest fallback when a provider reports no incremental
usage. Test ledger 2149 → 2156 (the +7 counted integration laws; the 13
pure-tracker unit laws are inline in `src/throughput.rs`, which
`xtask test-count` does not scan — same as prior lanes' inline suites).
Commits are per cohesive piece so an interruption preserves progress.

## Commits

| Commit | Scope |
|---|---|
| `01a953f` | `throughput.rs` + `lib.rs` — the pure tracker (tps/μ/p95, sparkline, 13 unit laws) |
| `8b55a23` | `projection.rs` + `app.rs` + `runtime.rs` — feed it on the live frame clock |
| `79751dc` | `render.rs` + `plain.rs` (+ test call-sites) + `wg_throughput_tests.rs` — the visible row, 7 render laws |

## Design as built

- **Source.** The streaming OUTPUT-token count. `note_throughput`
  (`app.rs`) samples `clock_ms` while `projection.is_streaming()`
  (`Streaming | RunningTool`), preferring provider usage
  (`Usage::output`, exact) and falling back to a text-derived count
  (`SessionProjection::streamed_output_tokens_approx()` = streamed
  assistant chars / 4, marked approximate) when no incremental usage is
  reported. Fed ONLY at the existing clock-advance sites — active-session
  applied envelopes in `route_raw` and the `run_live` anim tick — so there
  is NO new timer.
- **The math is pure (WG1).** `ThroughputTracker` is a ring buffer;
  `observe(now_ms, cumulative_output_tokens, exact)` records a raw
  `(t, tokens)` observation, computes the instantaneous windowed rate
  (token delta over the retained ~1s span, per wall-second) and appends a
  per-interval tps sample. `tps`/`μ`/`p95`/`spark` are pure functions of
  the buffer, so tests seed a scripted stream over mock time and the probe
  replays reproduce byte-for-byte. p95 is nearest-rank
  (`⌈p/100·n⌉`, clamped) → `percentile(1..=100, 95) == 95`.
- **Sparkline (WG2).** `spark(samples, width)` maps the last `width`
  samples across the window's own [min, max] to the `▁▂▃▄▅▆▇█` ramp — a
  rising series ramps monotonically, a flat/zero window renders flat-low,
  empty/short/zero-width never panics.
- **Placement + idle cost (WG3).** One ambient line in the live-work band
  above the composer (below the task band), LOWEST shed priority — it
  yields before the task band and todos when space is tight. Its height is
  zero unless `throughput_readout()` is `Some`, which is gated on the run
  actively streaming, so an idle screen never carries the row and idle
  frames are byte-identical across ticks. The tracker resets to its empty
  resting shape once off-stream (`note_throughput`) and on `fresh_session`.
- **Degradation (WG5).** Before a rate exists the row is absent; before
  `STATS_MIN` (4) samples exist it shows the current `tps` alone with no
  μ/p95 (never faked). When the source is approximate the readout wears a
  leading `~` — sticky per turn: once ANY exact usage lands, the tilde
  drops and does not flicker back. The honest option is APPROXIMATE (not
  hidden) so a compatible endpoint still shows a useful rate, clearly
  marked as an estimate.
- **Reset (WG4).** A new turn's cumulative output restarts small; the
  tracker auto-resets when the count regresses, and the per-turn streamed-
  char tally resets at each turn opening (`RunState` idle→non-terminal), so
  the exact→approx source switch can never fabricate a spurious rate.
- **Plain + theme parity (WG6).** `render_plain` now threads the readout
  and prints the equivalent line via `ThroughputReadout::plain_text()`; the
  styled `throughput_line` builds the SAME glyphs and numbers with theme
  spans (gold label/spark/rate, dim μ/p95). A render law asserts the styled
  row's text equals the plain line from the same readout, and the row is
  legible across all five themes.

## Laws (all green)

- WG1 pure tps/μ/p95 — `wg1_percentile_…`, `wg1_mean_…`,
  `wg1_windowed_rate_…` (unit).
- WG2 sparkline mapping — `wg2_sparkline_ramps_…`, `wg2_flat_buffer_…`,
  `wg2_width_…` (unit).
- WG3 idle no-op — `wg3_off_stream_hides_the_row_…`,
  `wg3_idle_frames_are_byte_identical_across_ticks`,
  `wg3_streaming_shows_the_row` (render) + `wg3_idle_reset_…` (unit).
- WG4 streaming rise + reset — `wg4_scripted_stream_rises_…`,
  `wg4_cumulative_regression_resets_the_turn`, `wg4_degrades_…` (unit).
- WG5 fallback honesty — `wg5_approx_source_is_marked_…`,
  `wg5_exact_usage_drops_the_tilde_…`, `wg5_plain_text_omits_…` (unit) +
  `wg6_approx_readout_wears_the_tilde_in_plain_and_styled` (render).
- WG6 plain + theme parity — `wg6_styled_row_matches_the_plain_readout_text`,
  `wg6_plain_omits_the_row_when_idle`,
  `wg6_row_renders_legibly_in_every_theme` (render).

Four EXECUTED mutation kills (p95 math, sparkline mapping, idle no-op,
fallback honesty) — see `WG-throughput-mutation-notes.md`.

## Deviations / decisions

- **Demo path deliberately not fed.** The tracker is fed only from the LIVE
  runtime (`route_raw` + `run_live` anim tick), not the demo/typed
  `handle_envelope` path. The demo emits no realistic incrementing token
  stream, and keeping it unfed means the demo/ladder screens never carry
  the row — the ladder stays 16/16 and the row's SGR bytes are not
  probe-visible in the demo. Per the brief's probe clause this is the
  sanctioned "else cover via render-law" case, covered by the WG3/WG6
  render laws that drive the real `render()`/`render_plain()` buffers.
- **Approx = chars/4.** A coarse, honest proxy for output tokens when the
  provider reports no incremental usage. It is only ever a `~`-marked
  estimate; the moment real usage arrives the exact path takes over.
- **`render_plain` gained an `Option<&ThroughputReadout>` parameter.** The
  readout is model-level state, not projection state, so the plain oracle
  is threaded the readout explicitly; the 7 pre-existing call sites pass
  `None` (behaviour unchanged when idle).

## Verification

- `cargo test -p haider-tui` green (909 → 923 with the new laws; +13 inline
  unit + 7 counted integration). `cargo fmt --all -- --check` exit 0 at
  every commit; no leftover merge-conflict markers; clippy clean on the
  touched files.
- Ladder: `scripts/tui-probes/ladder.sh` (release `haider` + `haiderd`
  built first) → `16/16 PASS (14 demo + 2 live)` — unchanged, since the
  live-only feed keeps the row off the demo/ladder screens.
- Pre-existing neighbours stay green: the thinking-line / W-E shimmer laws
  (LE1–LE7), the S4 subagent-row elapsed/token laws, and the W-A task-band
  laws.
