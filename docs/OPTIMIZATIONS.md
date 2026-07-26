# Deferred optimizations ledger

Ideas surfaced by the efficiency rider (gpt-5.6 analysis in the clean-code pass) that were
NOT adopted at the time — kept here for later adoption. Each entry: where, idea, why deferred.
The clean-code reviewer moves entries in/out; adopted entries get the patch tag noted.

| Where | Idea | Why deferred | Status |
|---|---|---|---|
| scripts/supervise-process-lib.sh | Replace 1s ps-polling with kqueue NOTE_TRACK/NOTE_FORK process tracking (needs a tiny compiled helper) | Interim bash tooling; the Rust worker supervisor (W4/E1) owns this properly | planned → E1 |
| scripts/journal-cat.sh | Per-run monotonic seq field instead of timestamp sort keys | 1s ts + stable sort suffices for human reading | open |
| crates/haider-store | Batch append API for event bursts | API already exists; wire HarnessActor to batch per commit boundary | planned → W1 merge |
| crates/haider-store | CAS inline-small-blobs-in-SQLite threshold | Thousands-of-events scale doesn't need it | open |
| crates/haider-tui | Replace ratatui `unstable-rendered-line-info` (Paragraph::line_count scroll math) with our own unicode-width wrap/measure module (research rec 15) | v0-thin transcript; the width module arrives with markdown rendering | planned → TUI markdown wave |
| crates/haider-tui | Per-block rendered-line cache keyed revision × width × theme (research rec 9); today every frame rebuilds all lines | Demo transcripts are small; trigger = scroll-back over long sessions | planned → session-attach wave |
| crates/haider-tui | plain.rs + render.rs each encode item display shapes (exhaustive matches keep them honest; strings could drift) | Unify into shared block atoms when markdown/wrapping forces a re-touch anyway | open |
| crates/haider-tui/projection.rs | Open-item `item_id → index` map for delta routing (rider #1: crossover ~35 entries amortized; 1000 deltas over 5k entries ≈ 1.6ms total today) | Trigger: avg scan distance > ~1000 entries after long-session attach + ≥1% CPU in lookup | open |
| crates/haider-tui/render.rs | Wrapped-segment/height cache + viewport-only block selection (rider #7: 5k lines = 17ms/frame ≈ 51% core at 30fps; per-block Vec<Line> caching alone saves little — line_count re-traverses) + fix u16 saturation >65535 wrapped rows | Trigger: >~2-3k logical rows or p95 render >8-10ms (session-attach wave) | planned → session-attach wave |
| crates/haider-tui/runtime.rs | Input pump = process-lifetime blocking thread (rider #11: ~2MiB stack each if run_demo re-entered in-process; parked reader may eat one host input event) | v0.1 CLI exits the process; fix with cancellable polling + join when the runtime is embedded/re-entered | open |
| crates/haider-tui/app.rs | Single-pass paste normalization (rider #12: ~4× transient of pasted bytes today) | Trigger: 1MiB+ paste support or measured paste latency | open |

Rider adoptions 2026-07-26 (TUI0): #3 release args_fragments at completion · #4 capacity-bounded command tails (bound before append) · #6 Cow output_text (zero-copy valid-UTF-8 path) · #9 overflow-free fmt_tok M-tier + saturating context_tokens · #10 guarded frame tick (no idle wakeups). SUFFICIENT confirmed: String::push_str streaming accumulation, no ring buffer at 8KiB cap, compile-time blends.

## Adopted

| Where | Idea | Patch |
|---|---|---|
| crates/haider-store | Persistent connection + cached prepared statements (highest ROI per rider) | adopted — W1/M1 |
