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
| crates/haider-tui | ~~The sim's `pulse`/`railShimmer` animations (thinking… · badge WAITING/STARTING/PERMISSION/EFFECT_UNKNOWN · running ⚒ · launcher dot+rail · chip glyphs · processing todo · ◉ live hold · boot .sub)~~ | Was a deliberate trade against the efficiency rider (no idle wakeups); SHIPPED (TUI4d item 14, owner ask) WITHOUT breaking it: ONE shared `anim_phase` u8 on the model, a runtime interval gated on `AppModel::animated` (zero wakeups while nothing pulses), render alternates full ink ↔ the sim's 0.35-opacity midpoint (`Rgb::over(bg, 350)`), `% 3` shimmers the rail gold→maroon→gold at the sim's 1.8s; the phase is never persisted (the demo-store hash-skip stays quiet across ticks). Aura orb breathe/ring (scale transforms) stay unported — no cell equivalent | shipped — TUI4d |
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

## haider-daemon efficiency rider (W3b1, gpt-5.6, 2026-07-27)

Adopted now (rider NOW items 1-2): connection admission cap — every accepted same-UID socket
entered an unbounded `JoinSet`, so `DaemonConfig::max_connections` (default 64) is now enforced
by an owned accept-time permit that rides inside the connection task (freed on return, error, and
abort alike), and an over-cap peer gets a fatal `overloaded` `ProtocolError` (new additive
`haider_rpc::ERROR_CODE_OVERLOADED`; codes are open wire strings, no fixture change) then close,
with no task or queue created. Per-connection queued-byte budget —
`DaemonConfig::outbound_queued_bytes` (default 2 × frame_limit) is charged before enqueue and
credited after the write completes, so `outbound_queue_capacity × frame_limit` (32 × 8 MiB) can no
longer be the real memory bound; the frame-count bound and its connection-fatal treatment are
unchanged, and the final `ServerDraining` frame moved to a reserved one-shot path outside both
bounds so no volume of ordinary replies can consume the slot or bytes the notice needs. Test
helper polling in `lifecycle_tests.rs` (child exit, endpoint appearing, freed slot) swapped
`yield_now` spin for a 5 ms poll interval; production waits and the drain-boundary yield untouched.

| Where | Idea | Why deferred |
|---|---|---|
| crates/haider-daemon/src/runtime.rs (pre-ready recovery via `haider_core::reconcile_dispatched_effects`) | Recovery reads a projection instead of full history: an additive, transactionally maintained/indexed recovery projection of only dispatched/outcome rows, with journal backfill for existing stores and a corruption fallback to the full scan | Today's gate enumerates every session and deserializes each one's whole effect history, so peak memory tracks the largest session's history. A casual pending-effects cache is FORBIDDEN: atomic coupling to the append path, migration/backfill, corruption detection, and crash-window equivalence are lifecycle-correctness requirements, not optimizations — slated as its own designed slice alongside W3b2 |
| crates/haider-daemon/src/connection.rs | Consolidate per-connection allocations (buffers/queues) once W3b2 shows the real traffic shape | Merging the reader and writer tasks is DO-NOT-DO: it can alter socket shutdown and drain-delivery ordering, which R17 pins |
| crates/haider-daemon (runtime.rs + lifecycle.rs) | Revisit lock-acquisition ordering only against real W3b2 contention profiles | Left unchanged deliberately. DO-NOT-DO: profile-lock-before-store/socket acquisition, endpoint-cleanup-before-store-close, or replacing the shutdown transition mutex with atomics — each is a documented lifecycle law (R1/R3/R17), not a hot path |
| crates/haider-daemon/src/runtime.rs (drain barrier) | Drain fan-out stays concurrent and deadline-bounded | DO-NOT-DO: serializing per-connection notifications or closing sockets before the notice is enqueued would break "notify every open connection, then bounded completion" (R17) |
| crates/haider-daemon, crates/haider-daemond | No production O(n²) found in this lane | Nothing to defer; recorded so a later pass does not re-litigate it |

## haider-daemon residuals ledgered from review r2 (W3b1.4, 2026-07-27)

Not optimizations — accepted limits, recorded so a later round does not re-litigate them or
mistake them for oversights. Each states the exact trigger.

| Where | Residual | Why it stays |
|---|---|---|
| crates/haider-daemon/src/endpoint.rs (`restore`) | A claim that cannot be restored (a third node appeared at the public name meanwhile) leaves the claimed node under its staging name until the next start's sweep. A LIVE foreign socket claimed in that window loses its public path until its owner rebinds. | Restore is non-replacing on purpose: overwriting the third node would destroy someone else's endpoint, which is strictly worse. Trigger: a same-UID process deliberately creating a node at the public name inside a microsecond-scale claim window; the profile lifetime lock (R1) already keeps a normally-starting peer daemon out of this path entirely. |
| crates/haider-daemon/src/endpoint.rs (`staging_name`) | Unpredictable staging names remove the PRE-KNOWLEDGE race — nothing can be waiting at a name nobody could predict — but they are not secret: `0700` excludes other UIDs, not the owner, so a same-UID process watching the runtime directory (kqueue/FSEvents/polling) can learn each name as it appears and race the pathname operations that follow. | Closing this would need per-operation kernel-level atomicity that POSIX does not offer for socket nodes. Calibration: the adversary here already holds the user's own privileges; the profile lifetime lock (R1) keeps every non-adversarial peer out of this path. |
| crates/haider-daemon/src/endpoint.rs (`rename_no_replace`) | Non-Apple, non-Linux Unix targets fall back to check-then-rename, so publish and restore keep a replacing race there. | `renameat2`/`renamex_np` have no portable equivalent. Trigger: a racer creating a node at the destination between the check and the rename, on a target this workspace does not build for. Revisit only if another Unix target is added. |
| crates/haider-daemon/src/endpoint.rs (`probe`) | A liveness probe that does not settle within `PROBE_TIMEOUT` (2s) resolves the node as LIVE, so a wedged peer can make startup refuse instead of reclaiming a genuinely dead endpoint. | `connect(2)` blocks once a listener's backlog fills; hanging startup forever is worse than refusing. Trigger: a socket with a live-but-never-accepting owner. |
| crates/haider-daemond/tests/lifecycle_tests.rs | No black-box test can distinguish "writer aborted" from "writer aborted AND joined": by the time the daemon reports, an aborted task has been reaped either way. The join is enforced by construction — `ConnectionRuntime` owns every writer handle, collects the registry TWICE (once before the final connection join and once after it, when no sender but the runtime's own remains), and joins under the barrier deadline — but "by construction" is an argument about this code, not a proof that no path escapes it; a design review found exactly such a path once (W3b1.5 D1-1). | Would need a test hook inside teardown. The observable half — that nothing keeps feeding the socket after the barrier — IS tested (never-reading, one-byte, forced-abort cases). |
| crates/haider-daemond/tests/lifecycle_tests.rs | The pre-fix owned-cleanup window (stat then unlink on the PUBLIC name) is not reachable from a test: hitting it needs the daemon's own node present at the stat and replaced before the unlink, inside a sub-microsecond gap. The racing test pins the invariant but does not discriminate that shape. | The adjacent and much wider window — a node that goes live between the preflight probe and the removal — IS covered by `stale_cleanup_never_removes_a_node_that_went_live`, which does discriminate (mutation-verified). |
| crates/haider-daemon/src/connection.rs (`reject_over_limit`) | Raw `EAGAIN`/`EPIPE` on the over-limit rejection write is untested. | No deterministic hook to force a partial or failed `write(2)` on a freshly accepted socket; the loop's behaviour (retry `EINTR`, stop otherwise, close regardless) is inspected, not exercised. |

## haider-daemon design review (Fable 5, W3b1.5, 2026-07-27)

Folded this round: writer-registry re-drain after the final connection join (D1-1); the
`ServerDraining` law re-scoped to "last frame of this lane's traffic" (D2-2); one `barrier_step`
helper with the error-suppression asymmetry named by `StepFailure` (D2-3); `run_inner` split into
phase functions in call order (D2-4); exhaustive test-matrix header (D2-5); re-runnable
`MUTATION CHECK:` comments on every mutation-verified test (D2-6); `outbound_queued_bytes >=
frame_limit` validated with the aggregate worst case documented (D3-9); the drain-boundary
`yield_now` now says what it buys (D3-12).

| Where | Idea | Why deferred |
|---|---|---|
| crates/haider-daemon/src/connection.rs (`OutboundLane`) | W3b2 fan-out needs a clonable outbound handle that carries its own limit, so the session hub can enqueue without threading `outbound_limit` through every call | Today exactly one task writes to the lane and the limit is a local; a handle type before the fan-out exists would be shaped by guesswork. Trigger: the first W3b2 caller outside `connection.rs`. |
| crates/haider-daemon/src/runtime.rs (`ConnectionRuntime`) | Barrier task ownership is bespoke per class — connections in a `JoinSet`, writers in a channel-fed `Vec`. Either generalize into one owned-task stratum or keep the split deliberately | The shapes differ for a reason (a JoinSet cannot adopt a handle a child task created), and W3b2 adds a third class (session actors). Decide once all three exist, not twice. |
| crates/haider-daemon/src/endpoint.rs | `remove_owned` and `remove_verified_stale` share a claim → verify → act skeleton with different verification steps | Two flows do not justify the abstraction, and the verifications are what the laws are about. Trigger: a third claim-based flow. |
| crates/haider-daemon (whole crate) | Adopt `tracing` — the accept loop discards connection exits (`let _ = completed;`) and refused transitions only reach stderr | No observability dependency has been chosen for the workspace yet, and W3b1 has no real clients. DO THIS before real clients attach: W3b2's session hub is where per-connection faults stop being invisible. |
| crates/haider-daemon/src/connection.rs (R17 scope) | W3b2 must relax "one `ServerDraining` is the last frame" to "notice, then keep streaming until checkpoint or deadline" (d1 report §6.6 step 10 closes transports only after final envelopes are broadcast) | Deliberate relaxation, not a regression — and it must keep the deadline discipline: the reserve, the mid-frame deadline adoption, and teardown's ownership of writer completion all stay. |
| crates/haider-daemond/tests/lifecycle_tests.rs | The writer-registry re-drain (D1-1) has no discriminating test: removing it left all 34 cases green across 6 runs. `connections_racing_the_shutdown_request_are_torn_down_completely` exercises the shape (accepts racing the request) but cannot force a registration to land after the first collection | The window is a scheduling coincidence between a connection's first poll on another worker and the barrier's collection. Correct by construction instead: only connection tasks send on that channel, all of them are joined before the second collection, and the runtime's own sender never sends. |
