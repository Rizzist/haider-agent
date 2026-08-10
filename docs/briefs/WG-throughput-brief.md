# W-G — live throughput indicator (tps sparkline · μ · p95)

Owner ask (screenshot): a live token-throughput readout in the TUI status
area — `Throughput ▁▂▃▄▅ 126 tps · μ 119 · p95 154` — rendered above the
composer bar while a turn streams. Branch: `wg-throughput` off v0.0.82.
TUI-focused; a small token-rate tracker feeding a status row.

## Locked design decisions

1. SOURCE: the streaming output-token rate. Haider already receives
   `StreamEvent::UsageUpdate` / token deltas during a turn and has an
   anim/journal clock (S4 elapsed, W-E shimmer ride it). Sample
   OUTPUT tokens produced per unit wall-clock. Compute instantaneous
   tps over a short sliding window (e.g. last ~1s of deltas), and keep a
   rolling ring buffer of recent per-tick tps samples for the sparkline
   + the aggregate stats.
2. STATS over the current turn (reset per turn; optionally a session
   rolling variant later): `tps` = current windowed rate; `μ` = mean of
   the samples; `p95` = 95th percentile of the samples. Integer tps.
   Pure functions of the sample buffer (reproducible for probes).
3. SPARKLINE: unicode block ramp `▁▂▃▄▅▆▇█` over the last N samples
   (N ~ 16-24), each column scaled to the window's max (or a stable
   ceiling). Reuse any existing sparkline/bar helper if present; else a
   small pure `spark(samples, width) -> String`.
4. PLACEMENT: a status row ABOVE the composer band (same region as the
   thinking line / task band), shown only while a turn is actively
   STREAMING (RunState Streaming/RunningTool producing output); hidden
   when idle (zero idle cost — pin it). Dim/gold theme tokens; renders
   in all themes; plain-mode prints an equivalent line.
5. DEGRADATION: before enough samples exist, show what's available
   (`126 tps` with no μ/p95 or with `—`) rather than fake numbers. When
   the provider reports no incremental usage (some compatible endpoints),
   derive tps from text-delta character/approx-token counting as a
   fallback, marked approximate (`~126 tps`), OR hide — pick the honest
   option and pin it.
6. NO NEW TIMER: ride the existing anim/frame clock for the sample tick;
   the tps math is pure over the buffer. Idle = no animation, no repaint.

## Mandatory laws (haider-tui + wherever the tracker lives)

- WG1 tps/μ/p95 are pure functions of the sample buffer (fixed buffer →
  fixed output); p95 correct on a known distribution.
- WG2 sparkline: monotonic ramp maps sample magnitude to the right glyph;
  empty/short buffers don't panic; a flat buffer renders flat.
- WG3 idle: no streaming → the throughput row is absent and idle frames
  are byte-identical across ticks (zero idle cost).
- WG4 streaming: a scripted stream of token deltas over mock time
  produces a rising tps and a populated sparkline; reset on new turn.
- WG5 fallback: a provider with no incremental usage yields an honest
  approximate (or hidden) readout, never a fabricated exact number.
- WG6 plain-mode parity; theme sweep (renders legibly in all themes).
- Probe: extend the ladder with a streaming-throughput frame check if the
  row's SGR bytes are probe-visible; else cover via render-law.

## Discipline

CARGO_INCREMENTAL=0; `cargo test -p haider-tui` (+ haider-core/daemon if
the tracker lives there) per commit; `cargo fmt --all -- --check` exit 0
verified (not piped through head); named-path adds; commit per cohesive
piece. Ledger `cargo run -p xtask -- test-count --update` before final
(baseline 2149); notes + mutation-notes with >=4 EXECUTED kills (the p95
math, the idle no-op, the sparkline mapping, the fallback honesty). Run
`scripts/tui-probes/ladder.sh` before done (TUI-visual change). No
version bumps/tags/MCP/renames; never delete ~/.codex/sessions.
