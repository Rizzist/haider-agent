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

W3b2 completion (2026-07-27): both triggered rows below were resolved deliberately. `OutboundLane`
is now an explicit-close, clonable, attachment-keyed fair outbox with one aggregate frame/byte
bound; session actors and replay tasks remain a separate owned task stratum because their
cancellation/checkpoint semantics differ from socket writers. The R17 scope is now
`ServerDraining` at the next complete-frame boundary, followed only by already-queued checkpoint
traffic under the SAME deadline. The uncharged reserve, mid-frame deadline adoption, and
runtime-owned writer join are unchanged. Private mutation-checked writer tests pin notice-before-
checkpoint ordering, round-robin fairness, and detach-budget refunds. `tracing` was also selected
at this trigger and connection task failures are no longer silently discarded. Final independent
verification additionally pinned ownership-locked detach/purge, synchronous hub drain rejection,
abort-on-drop guards installed before the first await, replay-handle reaping, internal catch-up
overflow recovery through the store, and the committed-event wake from the hub into a registered
harness (without a second `MenuAnswered` append).

| Where | Idea | Why deferred |
|---|---|---|
| crates/haider-daemon/src/endpoint.rs | `remove_owned` and `remove_verified_stale` share a claim → verify → act skeleton with different verification steps | Two flows do not justify the abstraction, and the verifications are what the laws are about. Trigger: a third claim-based flow. |
| crates/haider-daemond/tests/lifecycle_tests.rs | The writer-registry re-drain (D1-1) has no discriminating test: removing it left all 34 cases green across 6 runs. `connections_racing_the_shutdown_request_are_torn_down_completely` exercises the shape (accepts racing the request) but cannot force a registration to land after the first collection | The window is a scheduling coincidence between a connection's first poll on another worker and the barrier's collection. Correct by construction instead: only connection tasks send on that channel, all of them are joined before the second collection, and the runtime's own sender never sends. |

## W3b2 efficiency rider (gpt-5.6, 2026-07-27) — LATER items ledgered

The rider audited f8d23f3..678f833 and flagged three NOW items, all landed in the follow-up
round: attachment admission limits (per-connection + global, rejected `overloaded` before any
actor/channel work), byte-bounding the internal attachment buffers (catch-up byte ledger +
`Store::read_page` byte-budgeted replay pages), and replay burst pacing against the real outbox
quota (`capacity_for`/`drain_progress`; an actual `try_send` refusal or lag-while-blocked still
detaches immediately). Items 4-12 are deferred here with the rider's risk lines preserved.

| Where | Idea | Why deferred |
|---|---|---|
| crates/haider-daemon/src/session_hub.rs (`try_send_attachment`), crates/haider-daemon/src/connection.rs (`ConnectionFrameSink::try_send`) | Rider 4: every event holds the profile-wide attachment mutex while `uds_codec::encode` allocates/serializes and while the connection outbox mutex is acquired. Prepare encoded bytes outside the ownership lock, then reacquire, recheck ownership, and enqueue synchronously | Risk: Medium-high; the send-versus-detach/purge barrier must remain atomic, although neither §5.5 invariant is involved. |
| crates/haider-daemon/src/session_hub.rs (`publish`, `SessionHub::append`), crates/haider-core/src/sqlite_store.rs | Rider 5: publication is `O(batch × attachments-for-this-session)` with deep `RawEnvelope` clones (not `N × M`). Use `Arc<RawEnvelope>` internally and borrowed event encoding after traffic profiles justify the change | Risk: High; persist-before-publish and actor publication order must remain byte-for-byte equivalent. |
| crates/haider-store/src/event_store.rs (`read`/`read_page`), crates/haider-core/src/sqlite_store.rs (`run_blocking`) | Rider 6: replay paging is keyset-based (no prefix re-reads) but allocates a fresh `Vec` + JSON `String` per row per page through a new `spawn_blocking` job, and a dropped future does not cancel the blocking read. Reserve page capacity and record discarded blocking reads before considering a cancellable/dedicated store executor | Risk: Medium; do not move receiver registration or `H` capture around an await. |
| crates/haider-daemon/src/session_hub.rs (catch-up overflow, `reregister`, `lag_and_detach`) | Rider 7: catch-up overflow is bounded and correct but churns allocations and re-deserialization. Add counters for catch-up overflow, discarded envelopes/pages, re-registration, and outbox detach, then tune only from W3c traces | Risk: Low; instrumentation leaves both store-resume transitions unchanged. |
| crates/haider-store/src/event_store.rs (`resolve_menu`, `resolve_menu_transaction`, `historical_resolution`) | Rider 8: `MenuAnswer` CAS is deliberately expensive but low-rate; the historical suffix scan deserializes every event after `request_seq`. Ledger an indexed, transactionally backfilled menu-lifecycle projection only if long-pending menus make the historical suffix scan measurable | Risk: Very high—do not precheck outside the transaction, change CAS ordering, or treat the projection as authoritative. |
| crates/haider-daemon/src/connection.rs (`OutboundLane` enqueue/dequeue/credit) | Rider 9: fair scheduling costs several constant-time mutex/hash operations per frame (`O(1)`, small constants). Profile first and consider a slab/indexed active-lane ring only if scheduler CPU becomes visible | Risk: High; a rewrite must preserve round-robin fairness, in-flight byte charging, and every bounded-queue law. |
| crates/haider-daemon/src/session_hub.rs (`session_list`, `holds_control_attachment`), crates/haider-store/src/event_store.rs (`session_ids`) | Rider 10: session pagination is the real rescan shape — every page loads all `M` session IDs then up to 100 sequential `latest_seq` calls (`O(M²/L)` across pages); menu authorization scans all attachments. Add a keyset store query returning `LIMIT + 1` session IDs and heads in one call, plus a connection/session control-attachment index if menu scans become visible | Risk: Low-medium; the control index must be updated atomically with attachment ownership. |
| crates/haider-core/src/sqlite_store.rs (`StoreOwner::with_store`), crates/haider-store/src/event_store.rs (connection mutex) | Rider 11: no hub/outbox mutex guard crosses an `.await`; the actual long hold is store serialization — `with_store` holds its outer mutex for the entire blocking database operation, serializing replay reads, appends, lists, and CAS across all sessions. Measure store queue/hold time in W3c and consider a read-only SQLite pool or dedicated store executor only if contention is material | Risk: Very high; writes and MenuAnswer CAS must remain on the existing serialized ordering path. |
| crates/haider-daemon/src/session_hub.rs (`actor_for`, actor registry, `shutdown`) | Rider 12: session actors are never retired — every session ever appended/attached keeps its task, command channel, handle, and registry entry until daemon shutdown. Ledger race-safe idle actor retirement with single-flight recreation after W3c establishes the real session-working-set size | Risk: Very high; naïve eviction can create two actors for one session and directly break both §5.5 invariants. |
| crates/haider-daemon/src/session_hub.rs (`SessionHub::append`, `StoreHandle` impl) | Append-exclusivity discipline gap (self-flagged in the W3b2 clean-code pass): "the hub is the only live-daemon append seam" holds by discipline, not code shape — `SqliteStoreHandle::append` remains directly callable. Structural fix: W3c hands workers the hub as their `StoreHandle`; until then discipline-only, documented at both sites | CLOSED for live workers by W3c1: every worker holds only a lease-fenced `HubStoreHandle` (`SessionHub::append` documents the seal). Discipline now covers only the pre-hub paths — startup recovery, standalone CLI, test seeding. |

### W3c R12 execution note (2026-07-27)

The W3c triggers above have now executed: the hub is split into
`session_hub/{mod,actor,replay,rpc}.rs`; `SessionHubConfig` is carried by
`DaemonConfig`; shared UDS support backs the production-runtime gate; workers
receive only lease-fenced `HubStoreHandle`s. Rider 7 counters now expose
catch-up overflow, discarded envelopes/store pages, store resumes,
re-registrations, and outbox detaches. Rider 11 now traces blocking-pool queue
wait and store-operation hold time under the `haider.store` target. These are
measurement hooks, not a read-pool/executor rewrite; the very-high-risk
serialization redesign remains deferred until measurements cross its stated
trigger.

## W3b2.3 review residuals + deferred design items (dual review r1, 2026-07-27)

| Where | Idea | Why deferred |
|---|---|---|
| crates/haider-daemon/src/session_hub.rs (`deliver_frame` wait states) | gpt P2-6 (= the implementer's flagged residual): a QUIESCENT stuck client parks its attachment indefinitely — outbox camped, no commits arriving, so neither an actual refusal nor lag-under-stall ever fires. Bounded by the admission caps (≤16 attachments/connection, ≤256 hub-wide) and the per-attachment byte budgets, and freed when the socket dies. Future fix: an idle/dead-peer deadline that detaches an attachment whose sink made no drain progress across N liveness probes | v0.1 accepts the bounded park; a deadline is a liveness policy that needs W3c/W3d's real client heartbeat/ping cadence to calibrate. Trigger: W3c attach CLI or W3d WS clients shipping. |
| crates/haider-core/src/sqlite_store.rs (`run_blocking`) | gpt P2-7 companion (ledgered rider item 6): an already-started `spawn_blocking` store read cannot be cancelled; it may finish on the blocking pool AFTER `SessionHub::shutdown` returns. Shutdown docs now say "no hub-OWNED task retains the store" with this exception named | A cancellable/dedicated store executor is rider item 6/11 territory (Very high risk on the write path). Trigger: measured store-close contention in W3c. |
| crates/haider-daemon/src/session_hub.rs (whole file) | Fable D3: the split trigger has ARRIVED (file > 2200 lines). Split into `session_hub/{mod,actor,replay,rpc}.rs` as W3C'S FIRST COMMIT, before new surface grows | Deferred out of THIS round only to keep the re-review diff reviewable against the same file anchors. Doing it mid-fix-round would shuffle every line the delta re-review needs to audit. |
| crates/haider-daemon/src/session_hub.rs (`SessionHubConfig`) + config.rs | Fable D3: two config dialects — hub knobs are code-level defaults, not operator-tunable through `DaemonConfig`, and validation is lumped into two multi-field checks | Wiring hub knobs into DaemonConfig/env belongs with W3c's CLI flag surface; splitting validation is cosmetic until then. Trigger: the first operator-facing config file/flag work. |
| crates/haider-daemon/src/session_hub.rs (`HubConnection::menu_answer`) | Fable D3: nine positional arguments; introduce a `MenuAnswerRequest` command struct mirroring the wire frame | The seam has exactly two callers (connection.rs and tests); reshaping it now would churn the re-review delta. Trigger: the W3d WS transport adding a third caller. |
| crates/haider-daemon/src/session_hub.rs (`register` → `spawn_replay`) | Fable D3: registration and replay-spawn are a forgettable two-step pairing; fuse into one seam that cannot be half-done | `session_attach` is the only pairing site today and the close-sweep sits between the steps deliberately. Fuse when W3c adds a second attach entry point. |
| crates/haider-daemon/tests/session_hub_tests.rs + crates/haider-daemond/tests | Fable D3: shared envelope/seed/client test helpers are duplicated across daemon test files; extract a test-support module | Trigger: the NEXT daemon test file. Extracting now would touch every test the re-review diffs. |
| crates/haider-store/src/event_store.rs (`latest_run_states`), crates/haider-daemon/src/worker.rs (`durable_runs`) | W3c1 clean-code pass: every accept/cancel/settle transaction and every supervisor admission/reconciliation re-scans and re-deserializes the whole session journal to derive run states — `O(journal)` per command. Natural fix: a durable run-state index maintained inside the same transactions (the `menu_resolutions` pattern) | Trigger: any session reaches 3,000 envelopes OR p95 `turn.submit`/`turn.cancel` durable latency exceeds 20 ms for 5 minutes. DO-NOT-DO: substitute an in-memory cache, update the index outside the receipt/event transaction, or trust a projection without migration backfill plus corruption fallback. |
| crates/haider-store/src/event_store.rs (`lookup_session_create_receipt` vs `lookup_command_response`) | W3c1 clean-code pass: the session-create receipt lookup reimplements the generic helper plus extra coordinate cross-checks the turn receipts never get — two sites for one R2 lookup law, with inconsistent rigor | Merging changes which corruption shapes each command detects; do it deliberately with tests, not inside a clean-code pass. Trigger: the W3c2 `account.login_api` receipt, which will need the generic path anyway. DO-NOT-DO: drop session-create's indexed-coordinate cross-checks or move durable replay behind the generation fence merely to share code. |
| crates/haider-daemond/tests/support/mod.rs + lifecycle_tests.rs (`hello`/`handshake`) | W3c1 clean-code pass: lifecycle_tests re-implements the support handshake with parametrized protocol bounds, and `connect_control` swallows the `Welcome` a feature-assertion test then has to re-fetch with a hand-rolled raw `Hello`. Fold a `connect_with_welcome` variant into support | Test-only churn across three suites; batch it with the next daemond test file (same trigger as the existing shared-helper row above). |

## W3c1 efficiency rider (2026-07-27) — NOW results and LATER triggers

NOW landed:

- core failure terminalization appends adjacent `RunFailed` + `Errored` in one
  transaction (the worker-start and recovery paths already batched them);
- prompt attachment resolution now borrows ordinary text/tool blocks instead
  of deep-cloning every compiled-history block merely to discover attachments;
- prompt-compilation and the two pre-ready recovery phases expose trace timings
  (`haider.worker` / `haider.recovery`);
- the lost-submit gate replaced a 30 ms sleep plus eight scheduler yields with
  durable-terminal polling and a positive supervisor-FIFO fence turn.

The external SQLite probe (debug build, median of three) measured prompt
compilation at 5.9 ms for 301 envelopes, 57.7 ms for 3,001, and 278.0 ms for
15,001. Compilation occurs once per logical turn, not once per provider
request. It is still an unbounded `O(session envelopes)` read/reduction, and
each later provider request deep-clones the compiled messages. SQLite
`EXPLAIN QUERY PLAN` showed `sessions.id` and `command_receipts.command_id`
lookups, plus receipt finalization, using their primary-key autoindexes.
Receipt claim uses the same primary-key uniqueness check; no secondary index
is justified for the new metadata/receipt access paths.

| Where | Idea | Why deferred / exact trigger |
|---|---|---|
| crates/haider-core/src/prompt_history.rs (`PromptHistoryCompiler::compile`) | Add an additive, transactionally maintained prompt projection or a terminal-head-keyed compiled-history cache so a new turn reads `O(delta)` instead of the entire session | Trigger: `haider.worker` `compile_micros` p95 exceeds 50,000 for 5 minutes OR any session reaches 3,000 envelopes. DO-NOT-DO: cache partial/nonterminal output, omit branch + agent + prompt-render policy from the key, or trust a projection without migration backfill and corruption fallback to the journal. |
| crates/haider-core/src/actor.rs (`TurnRequest` construction) | Replace the per-provider-request `messages.clone()`/tools/attachments clone with an immutable shared request spine plus a small continuation delta | Trigger: sampled request construction exceeds 10% of worker CPU AND p95 provider requests per logical turn is at least 3, or the median compiled prompt exceeds 1 MiB. DO-NOT-DO: lend request data across an async provider lifetime, replay normalized reasoning, or let retries/tool continuations observe a mutable message prefix. |
| crates/haider-core/src/actor.rs (`commit_payload`), crates/haider-core/src/sqlite_store.rs, crates/haider-daemon/src/session_hub/{mod,actor}.rs | Consider bounded adjacent-delta batches and/or `Arc<RawEnvelope>` ownership to reduce one SQLite transaction plus several deep clones per streamed provider event | Trigger: turns sustain at least 100 committed deltas AND `haider.store` append `operation_micros` p95 exceeds 2,000 for 5 minutes, or allocation sampling attributes at least 10% of daemon CPU to envelope clone/serialization. DO-NOT-DO: buffer across cancellation, item completion, `RunFailed`/terminal state, or any publish boundary; persist-before-publish and exact sequence order remain invariant. |
| crates/haider-daemon/src/worker.rs (`run_manager`/`run_supervisor`), crates/haider-daemon/src/session_hub/mod.rs (`actor_for`) | Retire truly idle supervisor + actor pairs with single-flight recreation; W3c1 adds one manager entry, supervisor task, 64-slot command channel, watch channel, metadata, and lease for every session that ever runs, while the active harness/provider/broker are already dropped per turn | Trigger: a 10,000-session run-then-idle soak raises steady-state RSS by more than 64 MiB OR resident idle supervisors exceed 10,000 in production. DO-NOT-DO: infer cancellation from client absence, evict an active/queued/pending-menu session, remove before stop+join, or permit two actors/supervisors or leases for one session. |
| crates/haider-core/src/recovery.rs, crates/haider-daemon/src/turn_recovery.rs, crates/haider-daemon/src/runtime.rs | Replace the two pre-ready full-journal passes (effect reconciliation, then interrupted-turn reduction) with transactionally maintained recovery projections or a designed shared scan | Trigger: either `haider.recovery` phase exceeds 1,000,000 µs p95 across starts, or a 100,000-envelope profile adds more than 64 MiB peak RSS during recovery. DO-NOT-DO: decide turns before ambiguous effects become `Unknown`, reissue active provider/effect work, or use a projection without atomic append coupling, migration backfill, and journal corruption fallback. |
| crates/haider-daemon/src/session_hub/mod.rs (`offer_attachment*`), crates/haider-daemon/src/connection.rs (`ConnectionFrameSink::offer*`) | Prepare encoded event bytes once outside the profile-wide attachment-ownership lock and reuse them across Busy/ticket retries | Trigger: profiling at the 256-attachment cap attributes at least 5% daemon CPU to `uds_codec::encode` retries OR attachment-lock wait p95 exceeds 1 ms for 5 minutes. DO-NOT-DO: weaken the atomic send-versus-detach/purge barrier, enqueue after ownership disappears, or let an event overtake its staged attach response. |
