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
| crates/haider-tui | ~~Compaction before/after token counts in the transcript card~~ | SHIPPED (TUI3b): additive optional `tokens_before`/`tokens_after` on `ContextCompaction` (the numbers were already protocol-blessed on `NodeKind::Compaction`; existing goldens byte-identical, one new fixture) — the gold card renders `⊟ compacted 170k → 12k · summary retained · originals stay in /tree` | shipped — TUI3b |
| crates/haider-tui | Todos collapse toggle (`▸ todos — … · ■ current` + `full list` button); multi-menu queue (`· N more queued` footer) | The ⧗ queued-messages panel SHIPPED with TUI3b (`/queue turn`, sacred-ledger slot between todos and composer, consumed at turn end with no idle); collapse needs persistent UI prefs, and the demo holds ONE open menu (`Option<Menu>`) so the queued-menus footer has nothing to count yet | planned → daemon wave (W3) |
| crates/haider-tui | ~~Hover-select/hover-highlight (sim `onMouseEnter`)~~ | SHIPPED (TUI3a): `MouseEventKind::Moved` → `handle_hover`, dirty only on target change; palette/menu hover moves the selection, rows/chips take hover chrome | shipped — TUI3a |
| crates/haider-tui | Boot-time input queueing (sim submit step 8: `· queued — sends when startup completes`, multi-submits `\n`-concatenated) | Unreachable by construction: the boot screen swallows input entirely (review r1 P2 — hidden input must not accumulate), so nothing can be typed to queue; revisit when boot gains a live composer | planned → daemon-era input stack |
| crates/haider-tui | `/compact` mid-turn is refused with a flash (the sim runs it over a live turn) | The sim's single-threaded state writes tolerate mid-turn compaction; the envelope demo would have two scripts fighting over `RunState` — refusing honestly beats clobbering a live turn | deliberate |
| crates/haider-tui | Mid-composer cursor movement (←→/click editing; the cursor is end-of-text) | TUI2.4 shipped real multi-line entry (⇧⏎ where reported + ⌥⏎ universally, growth to 5 rows, vertical+horizontal tail windows, newline-preserving small pastes); a movable cursor + selection is an editor feature | planned → daemon-era input stack |
| crates/haider-tui | `\t` in pre-wrap agent bodies expands to a fixed 4 cells | Terminal buffer cells cannot render a tab; a fixed expansion is the one deliberate divergence from CSS pre-wrap (review r3 P2-5, documented at `wrap_body`) | deliberate |
| crates/haider-tui | The حيدر mark renders at cell size (sim: 52px) | Terminal cells cannot scale fonts. DECDWL (ESC#6 double-width rows) was evaluated and SKIPPED: ratatui's cell-diff redraw addresses columns with no double-width-line awareness, so partial repaints on a DECDWL row corrupt the grid — not provably clean. The real fix is the 3-tier Arabic plan's graphics-protocol tier (kitty/iTerm2 image or sixel mark) | planned → graphics tier |
| crates/haider-tui | Live-geometry scroll parity: a wheel-up between a resize and the next frame clamps to the ≤1-frame-stale range (holds the last-known top) instead of the not-yet-measured new range | The sim reads DOM geometry synchronously (tui.js:2648); we reconcile at 33ms frames — reconcile-then-apply (r5 P2-2) trades one frame of optimism for burst-debt safety; accepted r6 P3-2, pinned in `review4_fix_tests` | deliberate |

## Adopted

| Where | Idea | Patch |
|---|---|---|
| crates/haider-store | Persistent connection + cached prepared statements (highest ROI per rider) | adopted — W1/M1 |
| crates/haider-tui/runtime.rs | Loss-free theme detection: parse the OSC-11 reply inside the sole input reader instead of termbg's owning probe (TUI1 review P2 — an 80ms pre-UI window can consume one keystroke) | Needs the unified input stack; termbg window shrunk + documented meanwhile | planned → daemon-era input stack |

## haider-rpc efficiency rider (W3a, gpt-5.6, 2026-07-26)

Adopted now: encoder growth policy — `try_reserve_exact` per serde write re-copied the
accumulated JSON worst-case O(n²); replaced with geometric growth capped at frame_limit
(exact length check unchanged, no wire-byte change).

Ledgered pending real daemon profiles (rider items 2-9, full text in ~/haider-run/w3a-efficiency.log):

| Where | Idea | Why deferred |
|---|---|---|
| uds_codec encode | Serialize into a prefix-placeholder buffer (kills the second body copy) | prefix accounting risk; needs golden transport parity proof |
| codec/ws_codec | Optional `encode_into` caller-scratch APIs | scratch lifetime vs async sends; wait for daemon call patterns |
| uds_codec decode | Decode complete bodies from the input slice; stage only fragments | state/poison/coalesce transitions get subtle |
| uds_codec | Reusable fragmented-body scratch w/ retention ceiling | can pin ~frame_limit per connection; needs daemon shape |
| transport seam | BytesMut adapter + borrowed frame view (only if daemon uses BytesMut) | added dep, lifetimes across await, duplicate types |
| codec decode | Drop explicit UTF-8 pass, let serde_json validate | changes InvalidUtf8→Json error semantics; benchmark first |
| frame.rs serde | Manual order-independent visitor to avoid flatten/content buffering | DO-NOT-DO casually: tag layout/field order = wire bytes |
| Event payloads | Typed fast-path decode beside RawEnvelope | DO-NOT-DO: closed enum would break unknown-event tolerance |
## Fable design review — TUI3 arc (2026-07-27, verdict FIX_IN_MERGE; D2-1/2/3 + quick D3s folded)

| Where | Item | Trigger |
|---|---|---|
| crates/haider-tui/tests | D2-4: paused-time driver harness triplicated (~130 lines × turn_engine/tui31_lifecycle/subagent_aura) — extract to tests/common/mod.rs | MUST land before the next TUI test file is created (sessions/tree/accounts wave) |
| crates/haider-tui/src/render.rs | D3-5: four hand-rolled shed-ladder dialects (session/subagent/aura/launcher); split render.rs (2730 lines) per-screen | before the NEXT-ROUND screens land on it |
| crates/haider-tui/src/runtime.rs | D3-6: channel tag is an ARM id but still named `generation` in consume/dispatch_input signatures + docs — one vocabulary | next runtime-touching round |
| crates/haider-tui/src/script.rs | D3-8: `respond_branch`'s `voice` param consumed by `let _ = voice;` — drop or justify | next script-touching round |
| crates/haider-tui/src/render.rs | D3-9: aura streaming cursor wraps `{text}▮` in text ink while item_lines splits it back out gold — aura stream reads deader than session | next aura polish |
| crates/haider-tui/src/render.rs | D3-10: status bar has no horizontal shed order — at 90 cols the voice chip clips mid-chip (dangling `[ ◉`); ellipsize-or-drop-segments rule | next status-bar touch |
| crates/haider-tui/src/render.rs | D3-11: session @ 90×5 w/ menu renders header-rule + input-rule adjacent with all content shed — collapse to one rule (taste; behavior pinned) | next ledger touch |
