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

## Sim-parity deferrals (TUI2.2 composer/mouse pass)

Deliberate gaps against the `/tui` sim, each with its landing wave:

| Where | Gap | Why deferred | Status |
|---|---|---|---|
| crates/haider-tui | In-app mouse drag-select in the transcript | Mouse capture is ON (kills scrollback bleed); native ⇧-drag selection already works in every emulator — reimplementing OS selection in-app buys nothing at demo scale | left to native ⇧-drag |
| crates/haider-tui | Arg-slot table beyond `/theme` (sim `argSlots`: /model, /provider, /login, /account, /queue) | TUI2.3/2.4 shipped the inline ghost + `/theme`'s full slot semantics (lead rows at exact `/theme`, ⏎/click/tab slot entry — the one command executable today); the rest take real args only once the daemon wires them | planned → daemon wave (W3) |
| crates/haider-tui | Compaction before/after token counts in the transcript card | The protocol's `ContextCompaction` carries only `summary_artifact` — no counts to show honestly; the gold card renders without numbers | planned → protocol usage-delta field |
| crates/haider-tui | Todos collapse toggle + queued-messages panel; multi-menu queue (`· N more queued`) | Demo opens one card and pins one plan; collapse needs persistent UI prefs | planned → daemon wave (W3) |
| crates/haider-tui | Hover-select/hover-highlight (sim `onMouseEnter`) | Left-click + wheel only — mouse-move reporting floods the input channel for cosmetic gain | open |
| crates/haider-tui | Mid-composer cursor movement (←→/click editing; the cursor is end-of-text) | TUI2.4 shipped real multi-line entry (⇧⏎ where reported + ⌥⏎ universally, growth to 5 rows, vertical+horizontal tail windows, newline-preserving small pastes); a movable cursor + selection is an editor feature | planned → daemon-era input stack |
| crates/haider-tui | `\t` in pre-wrap agent bodies expands to a fixed 4 cells | Terminal buffer cells cannot render a tab; a fixed expansion is the one deliberate divergence from CSS pre-wrap (review r3 P2-5, documented at `wrap_body`) | deliberate |
| crates/haider-tui | Live-geometry scroll parity: a wheel-up between a resize and the next frame clamps to the ≤1-frame-stale range (holds the last-known top) instead of the not-yet-measured new range | The sim reads DOM geometry synchronously (tui.js:2648); we reconcile at 33ms frames — reconcile-then-apply (r5 P2-2) trades one frame of optimism for burst-debt safety; accepted r6 P3-2, pinned in `review4_fix_tests` | deliberate |

## Adopted

| Where | Idea | Patch |
|---|---|---|
| crates/haider-store | Persistent connection + cached prepared statements (highest ROI per rider) | adopted — W1/M1 |
| crates/haider-tui/runtime.rs | Loss-free theme detection: parse the OSC-11 reply inside the sole input reader instead of termbg's owning probe (TUI1 review P2 — an 80ms pre-UI window can consume one keystroke) | Needs the unified input stack; termbg window shrunk + documented meanwhile | planned → daemon-era input stack |
