# Speed / hardware-maxxing optimization plan for Haider

> gpt-5.6 Sol read-only analysis, 2026-08-10. Single-machine focus (NOT distributed). Excludes prompt-cache work (separate cachemaxxing effort). Implementation happens AFTER cachemaxxing ships.

## Top 5

1. **Remove an accidental ~1-second cold-launch delay.** A successful daemon spawn is treated like a race loser and polled 40×25 ms before returning. Big win, low effort. [spawn.rs:258-281](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/spawn.rs:258)
2. **Eliminate full-journal validation on every streamed delta, then microbatch commits.** Today streaming trends toward O(history × deltas) and can incur multiple durable SQLite operations per fragment. Big win, medium effort. [event_store.rs:5312-5399](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:5312)
3. **Virtualize and cache transcript rendering.** Repository benchmarks show 5,000-row p95 already exceeds the 33 ms frame budget. Big UI responsiveness and CPU win. [OPTIMIZATIONS.md:17](/Users/rizzist/haider-run/b2b-tui/docs/OPTIMIZATIONS.md:17)
4. **Reuse provider HTTP transports across turns.** A fresh `reqwest::Client` and DNS guard are currently created per logical turn, losing connection pooling and TLS reuse. Big warm-turn TTFT win. [accounts.rs:5355-5455](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:5355)
5. **Compile each turn from one indexed journal projection.** Turn startup currently performs several full journal reads, while prompt fragment selection can approach O(n²). Big mature-session win. [prompt_history.rs:91-133](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:91)

Current critical paths:

- **Before typing:** daemon attach/spawn → store/recovery/accounts/hooks → endpoint Ready → erroneous one-second child wait → custom-command/theme/STT/settings/wordmark work → input thread. [main.rs:236-273](/Users/rizzist/haider-run/b2b-tui/crates/haider-cli/src/main.rs:236), [runtime.rs:247-335](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/runtime.rs:247), [TUI runtime.rs:2678-2755](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/runtime.rs:2678)
- **Before the first provider request:** provider/account resolution → delegation lookups → project-instruction filesystem scan → instruction history scan → prompt compilation → attachments → tool factory/schema assembly → durable run-state scan. [worker.rs:3313-3487](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3313)
- **For each streamed fragment:** provider channel → actor → durable append → full-history run validation → FULL-sync transaction → hook outbox work → publication. [actor.rs:1435-1481](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1435), [event_store.rs:4356-4395](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:4356)

## Optimize now

Ranked by estimated win divided by implementation effort.

### 1. Remove the unconditional post-handshake wait

- **Where:** [spawn.rs:258-281](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/spawn.rs:258); the peer PID already exists in [client.rs:188-230](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/client.rs:188).
- **Current cost:** after receiving `Attach::Ready`, the launcher polls any still-running spawned child 40 times with 25 ms sleeps. The normal winning daemon is intentionally long-lived, so an ordinary cold spawn pays approximately one second.
- **Change:** compare `Child::id()` with the connected daemon’s kernel peer PID. Return immediately if they match; perform bounded reaping only when the connected peer is a different PID.
- **Expected win:** **Big**, approximately one second from normal cold launches.
- **Effort:** Low.
- **Risk:** Low.
- **Success signal:** Ready-to-`ensure_daemon` return falls from about 1,000 ms to near zero, while race-loser exit-75/zombie tests continue to pass.

### 2. Replace per-append full-history validation with transactional run heads

- **Where:** every worker append invokes validation at [event_store.rs:5312-5326](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:5312), which obtains state by scanning and decoding every session event at [event_store.rs:4356-4395](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:4356). Text fragments await this path at [actor.rs:1903-1955](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1903).
- **Current cost:** O(history) JSON reads per text, reasoning, and tool-argument fragment. A 10,000-event session receiving 500 deltas can perform roughly five million envelope decodes merely to validate run transitions.
- **Change:** add a normalized `run_heads(session_id, run_id, state, seq, branch_id, worker_generation)` projection. Backfill it once, then validate and update affected run rows transactionally with event insertion.
- **Expected win:** **Big**; append latency should become nearly independent of transcript length.
- **Effort:** Medium.
- **Risk:** Medium—migration, backfill, and state-machine invariants need property and recovery tests.
- **Success signal:** `append_worker` executes no full `events` scan; latency curves stay flat as history grows; cancellation/terminal-transition tests remain unchanged.

### 3. Microbatch stream commits and stop hook work for irrelevant deltas

- **Where:** every fragment is committed individually at [actor.rs:1903-1955](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1903); each append creates and commits a transaction at [event_store.rs:5312-5385](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:5312) under `synchronous=FULL` at [event_store.rs:6053-6068](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:6053). Almost every ordinary event also gets a hook-outbox row at [event_store.rs:4964-4980](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:4964), followed by one acknowledgement delete at [hooks.rs:695-712](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/hooks.rs:695).
- **Current cost:** the actor stops polling the provider during storage awaits. An irrelevant token delta can cause a FULL-sync append plus later outbox classification and another SQLite write.
- **Change:** coalesce adjacent deltas for the same item into bounded 5–15 ms or 16–32 KiB batches, flushing at first output, semantic boundaries, cancellation, and finish. Add one exhaustive `hook_relevant` predicate covering classified triggers and decision/trust facts, and acknowledge hook batches in one transaction.
- **Expected win:** **Big** for stream smoothness, SQLite CPU, and write amplification.
- **Effort:** Medium.
- **Risk:** Medium—durable-before-publish, cancellation, and hook replay semantics must remain exact.
- **Success signal:** transactions/fsyncs per 100 deltas fall by more than 10×; irrelevant deltas produce no outbox rows; p95 inter-token gaps shrink without exposing uncommitted events.

### 4. Reuse provider transports across logical turns

- **Where:** turn startup resolves/builds the provider at [worker.rs:3313-3316](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3313) and [accounts.rs:5355-5455](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:5355). Each adapter builds a new client in [openai.rs:187-224](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:187), [anthropic.rs:227-263](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/anthropic.rs:227), and [gemini.rs:79-119](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:79). The fixed-origin DNS guard is adapter-local at [origin.rs:32-42](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/origin.rs:32).
- **Current cost:** consecutive turns can repay DNS, TCP, and TLS establishment rather than using reqwest’s pool.
- **Change:** create a daemon-owned transport registry keyed by normalized origin and origin-security policy. Share `reqwest::Client` and guarded DNS state; keep credentials, account, model, and request tuning turn-local. Bound DNS lifetime and invalidate on origin-policy changes.
- **Expected win:** **Big** for warm-turn request-open/TTFT; smaller for long generations.
- **Effort:** Medium.
- **Risk:** Medium, mainly origin isolation and DNS pinning.
- **Success signal:** consecutive turns to the same origin reuse an accepted connection and avoid a second handshake within the configured lifetime.

### 5. Replace the pre-exec descriptor loop with a bulk-close primitive

- **Where:** [spawn.rs:410-419](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/spawn.rs:410).
- **Current cost:** every daemon spawn calls `close` for FDs 3 through 65,535—65,533 syscalls before `exec`, even if nearly all are already closed.
- **Change:** use `closefrom`, `close_range`, or macOS `F_CLOSEM`, with a carefully tested portable fallback.
- **Expected win:** **Medium to big** cold-spawn improvement.
- **Effort:** Medium.
- **Risk:** Medium because descriptor isolation is correctness/security-sensitive.
- **Success signal:** fork-to-exec close work becomes O(1) or O(open FDs); descriptor-leak and EOF regression tests remain green.

### 6. Virtualize and cache transcript rendering

- **Where:** the primary transcript is reconstructed at [render.rs:2952-3048](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:2952), subagent history repeats it at [render.rs:4323-4369](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:4323), and Markdown reparses all source lines at [md.rs:154-188](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/md.rs:154).
- **Current cost:** every dirty frame reparses, wraps, allocates, clones, and measures the full transcript. Streaming repeatedly processes an ever-growing prefix, producing cumulative O(n²) work. The repository ledger records release p95 of **12.8 ms at 1k, 24.5 ms at 3k, and 42.4 ms at 5k rows**; reference 5k runs reached 50.9–96.9 ms. [OPTIMIZATIONS.md:17](/Users/rizzist/haider-run/b2b-tui/docs/OPTIMIZATIONS.md:17), [w3c3_render_bench_tests.rs:135-147](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/tests/w3c3_render_bench_tests.rs:135)
- **Change:** cache completed block render atoms and wrapped heights by entry revision × width × theme; maintain prefix heights; select only viewport blocks plus overscan; rerender only the mutable streaming tail. A `Vec<Line>` cache alone is insufficient because `Paragraph::line_count` still traverses it.
- **Expected win:** **Big** CPU and interaction-latency improvement.
- **Effort:** High.
- **Risk:** Medium-high around selection, sticky scrolling, and jump mappings.
- **Success signal:** 5k-row p95 below 8–10 ms, 5k:1k ratio below 2×, no missed 33 ms frames, and unchanged render goldens.

The scheduler itself is sound: redraws are dirty-gated at 33 ms and missed ticks are skipped at [runtime.rs:2769-2846](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/runtime.rs:2769). Ratatui already diffs terminal output; the expensive part is model-to-buffer reconstruction.

### 7. Build one indexed turn-start projection

- **Where:** instruction facts scan the journal at [worker.rs:3948-3983](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3948); prompt compilation reads it again at [prompt_history.rs:91-133](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:91); ancestry fragment selection repeatedly filters the full vector at [prompt_history.rs:572-648](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:572); terminal states are recomputed at [prompt_history.rs:671-718](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:671). Durable run-state checks add another reduction at [worker.rs:2400-2463](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:2400).
- **Current cost:** typically three or more full journal reads around a turn, plus repeated JSON clones and potentially O(envelopes × ancestry nodes) prompt selection.
- **Change:** read/decode one journal snapshot into a typed projection containing branch indices, run heads, instruction state, and terminal states. Generate prompt history from indexed slices. Retain a final targeted durable `run_heads` cancellation fence. In parallel, `try_join!` independent provider resolution, delegation lookups, and project-instruction I/O currently serialized at [worker.rs:3313-3409](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3313).
- **Expected win:** **Big** for long or branched sessions; medium on new sessions.
- **Effort:** Medium-high.
- **Risk:** Medium because prompt equivalence is correctness-sensitive.
- **Success signal:** one journal traversal from submit to provider-open; doubling history costs at most about 2.2×; golden prompt messages remain identical.

This is local journal/prompt compilation work, not provider prompt-cache optimization.

### 8. Move nonessential discovery behind the first interactive frame

- **Where:** custom-command discovery precedes TUI launch at [main.rs:435-465](/Users/rizzist/haider-run/b2b-tui/crates/haider-cli/src/main.rs:435) and can examine up to 20,000 entries per source at [custom_commands.rs:134-146](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/custom_commands.rs:134). STT/settings/graphics work precedes input at [runtime.rs:2693-2755](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/runtime.rs:2693). Fixed themes still pay an 80 ms terminal probe at [runtime.rs:80-96](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/runtime.rs:80). Separately, an unchanged provider registry is durably rewritten during daemon boot at [accounts.rs:6931-6943](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:6931) and [provider_registry.rs:114-148](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/provider_registry.rs:114).
- **Current cost:** filesystem walks, duplicate settings parsing, terminal timeout, image/config decoding, and unnecessary file/directory fsyncs delay readiness or first input.
- **Change:** skip provider-registry saves unless its semantic contents changed; load settings once; skip the probe for fixed themes; paint/start input with built-ins and fallback branding, then load custom commands, STT config, and graphics capabilities through background events.
- **Expected win:** **Medium**, potentially big on slow/encrypted filesystems or huge command trees.
- **Effort:** Medium overall; the no-op registry write and fixed-theme skip are low effort.
- **Risk:** Low-medium around terminal-response routing.
- **Success signal:** unchanged startup writes no `providers.json`; fixed themes issue no OSC probe; Welcome-to-first-input p95 remains below one frame and is insensitive to a 20k-entry command tree.

### 9. Keep attachment and clipboard work off UI/link owners

- **Where:** pending dispatch deep-clones commands at [runtime.rs:2777-2791](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/runtime.rs:2777); `ArtifactBytes` owns up to 5 MiB at [app.rs:2549-2580](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:2549). Clipboard handling can sleep for up to 300 ms at [clipboard.rs:23-67](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/clipboard.rs:23). Attachments may be read twice at [headless.rs:104-178](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/headless.rs:104). Link-side base64 encoding creates roughly a 6.7 MiB string inline at [link.rs:607-615](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/link.rs:607).
- **Current cost:** large memcopies and blocking I/O/CPU pause the sole UI or link task. A full channel can repeat the 5 MiB clone every wake; link encoding temporarily prevents incoming-event forwarding.
- **Change:** pop commands by ownership and recover the value from `TrySendError::Full`; read each attachment once; use background result channels for filesystem/clipboard work; prepare base64 in a bounded blocking stage while an ordered send sequencer preserves protocol order.
- **Expected win:** **Medium**, big tail-latency improvement during uploads/copy.
- **Effort:** Medium.
- **Risk:** Low-medium; link ordering requires explicit tests.
- **Success signal:** 5 MiB upload/copy creates no UI scheduling gap above 33 ms, no payload-sized repeated clone, and no link lost-event increase.

### 10. Let CAS use the machine independently of SQLite

- **Where:** the entire `Store` is held behind one lifecycle mutex at [sqlite_store.rs:36-43](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/sqlite_store.rs:36), held across operations at [sqlite_store.rs:1015-1023](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/sqlite_store.rs:1015), including CAS at [sqlite_store.rs:911-935](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/sqlite_store.rs:911). The underlying store already separates the connection and `FileCas` at [event_store.rs:772-783](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:772).
- **Current cost:** large artifact hashing/read/write/fsync serializes with every SQLite operation and other CAS work across all sessions.
- **Change:** clone an `Arc<Store>` or `FileCas` under a short lifecycle lock, release it, then perform CAS on the blocking pool. Closing should detach first and drain in-flight operations.
- **Expected win:** **Medium** overall, **big** for attachments and concurrent agents.
- **Effort:** Medium.
- **Risk:** Medium around close/profile-lock lifetime.
- **Success signal:** a deliberately slow CAS put does not raise unrelated journal queue time; independent hashes can use multiple cores/storage queues.

### 11. Remove low-effort allocation churn in turn construction

- **Where:** the built-in tool registry and nested JSON schemas are rebuilt at [worker.rs:4171-4312](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4171), again for definitions at [worker.rs:4386-4393](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4386), and again during route lookup at [worker.rs:4314-4321](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4314). Each text fragment becomes a separate cloned assistant block at [actor.rs:1435-1440](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1435), and the resulting history is cloned into later requests at [actor.rs:1131-1138](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1131). Compaction clones its full covered context at [worker.rs:229-255](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:229).
- **Current cost:** repeated schema trees, thousands of tiny content blocks, and context-sized copies increase allocator traffic and provider-body construction time.
- **Change:** put immutable built-in tools/routes/provider definitions in `OnceLock`/`Arc`; append adjacent text into the last block; calculate compaction token estimates before moving—rather than cloning—the covered messages.
- **Expected win:** **Small to medium**, with a meaningful peak-RSS reduction on compaction or long continuations.
- **Effort:** Low.
- **Risk:** Low.
- **Success signal:** one built-in registry construction per process, O(semantic blocks) rather than O(stream fragments), and approximately one fewer history-sized allocation during compaction.

## Defer for later

- **Materialized startup recovery state** — Effect recovery, interrupted-turn recovery, and hook hydration each replay every session before bind/Ready at [recovery.rs:40-109](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/recovery.rs:40), [turn_recovery.rs:110-160](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/turn_recovery.rs:110), and [hooks.rs:585-607](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/hooks.rs:585). Transactional pending-effect/run/hook projections could make clean startup O(active work), a **big** win, but effort/risk are **high/high**. Defer until `run_heads` lands and realistic large-profile cold-start traces plus replay-vs-projection property tests exist.

- **SQLite writer actor plus read-only pool** — One connection/mutex serializes all sessions at [event_store.rs:772-821](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:772), while each call also traverses the outer owner at [sqlite_store.rs:958-1071](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/sqlite_store.rs:958). A writer plus machine-sized WAL reader pool could be a **big** multi-session win, but effort/risk are **high/high** due read-after-write and shutdown semantics. Unblock when queue-wait traces still show contention after scan, batching, hook, and CAS fixes.

- **Concurrent independent tool calls** — Providers advertise parallel calls at [openai.rs:2690-2694](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2690), [anthropic.rs:524-530](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/anthropic.rs:524), and [gemini.rs:262-269](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:262), but the actor awaits each tool inline at [actor.rs:2374-2406](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2374), while broker/permission locks span execution at [worker.rs:4481-4525](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4481). Potential win is **big**, effort/risk **high/medium-high**. Defer until a conflict classifier and deterministic journal/result-order contract exist; then bounded-fan-out permission-free reads first.

- **Parallel recursive search/glob** — Repository traversal is serial inside one blocking job at [filesystem.rs:536-604](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/filesystem.rs:536), [filesystem.rs:945-1067](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/filesystem.rs:945), and [filesystem.rs:1173-1255](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/filesystem.rs:1173). This leaves cores idle on large trees and has a **big** possible win, but effort/risk are **high/high** because anchored-FD confinement, symlink rejection, deterministic results, limits, and FD pressure must survive. Unblock with a representative corpus and 1/2/4/8-worker correctness/scaling harness.

- **Durable session snapshots plus tail replay** — Fresh selected-session attach replays retained events at [replay.rs:45-125](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/session_hub/replay.rs:45) and parses each envelope at [event_store.rs:3905-3977](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:3905). Snapshots could be a **big** huge-session win, but effort/risk are **high/high** because projection and schema versions must remain replay-equivalent. Defer until attach-to-caught-up p95 demonstrates a product-visible problem.

- **Borrowed/direct provider serialization** — Core clones request state at [actor.rs:1131-1138](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1131), then providers construct dynamic JSON trees, for example [openai.rs:2236-2411](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2236), before reqwest serializes again. Borrowed wire structs and streamed/base64 bodies offer a **medium** allocation/RSS win, but effort/risk are **medium-high/medium** across three dialects. Unblock with allocation profiles and golden wire-payload fixtures.

- **Cursor-based SSE framing** — OpenAI, Gemini, and Anthropic repeatedly front-drain `String`s, allocate data lines, and join them at [openai.rs:1128-1203](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:1128), [gemini.rs:870-939](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:870), and [wire/mod.rs:335-478](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:335). A shared `BytesMut`/cursor framer is **small** for remote APIs, potentially **medium** for high-rate local endpoints; effort/risk **low-medium/low-medium**. Defer until parsing exceeds roughly 5% of streamed-turn CPU.

- **Lower SQLite durability** — `synchronous=FULL` is explicit at [event_store.rs:6053-6068](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:6053). `NORMAL` might reduce fsync latency, but it weakens the current crash/power-loss contract. Defer until batching has reduced transaction count and a deliberate durability decision is supported by fault-injection tests.

- **Full owned/`Arc` event pipeline** — Envelopes are cloned entering the hub at [session_hub/mod.rs:2491-2507](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/session_hub/mod.rs:2491), restamped/copied in [event_store.rs:5348-5384](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:5348), and cloned for hooks at [hooks.rs:229-240](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/hooks.rs:229). Owned append plus `Arc<RawEnvelope>` fan-out offers a **medium** allocation win, with **medium-high/medium** effort/risk. Defer until the far larger scan/fsync costs are removed and allocation profiles establish the residual value.

- **Credential caching** — Each turn re-enters serialized account/vault paths at [accounts.rs:1465-1480](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:1465), [accounts.rs:1885-1980](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:1885), and [oauth.rs:3999-4031](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/oauth.rs:3999). Expected win is **small-medium**, effort/risk **medium/high** because revocation, refresh, switching, and secret lifetime require generation-fenced invalidation. Reuse non-secret transports now; defer credential caching pending security acceptance.

- **TUI micro-optimizations** — Throughput/sparkline work is bounded to 24 samples at [throughput.rs:24-39](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/throughput.rs:24), while `animated()` and window-title computation allocate/scan at [app.rs:3699-3791](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:3699) and [runtime.rs:2880-2883](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/runtime.rs:2880). Wins are **small**, effort/risk **low/low**. Defer until transcript virtualization is complete and a profile still attributes meaningful CPU here.

- **Release-profile/binary tuning** — Release already uses thin LTO and strips symbols at [Cargo.toml:64-66](/Users/rizzist/haider-run/b2b-tui/Cargo.toml:64). Fat LTO, `codegen-units=1`, feature splitting, or `panic=abort` offer an uncertain **small** startup/runtime win with **medium-high** build effort and potentially **high** daemon-isolation risk. Unblock with binary-size, cold-page-fault, RSS, and cold-launch benchmarks.

Finally, hardcoding more Tokio workers is not recommended: the workspace enables `rt-multi-thread` at [Cargo.toml:49](/Users/rizzist/haider-run/b2b-tui/Cargo.toml:49), and both entry points use the default multi-thread runtime at [CLI main.rs:43-44](/Users/rizzist/haider-run/b2b-tui/crates/haider-cli/src/main.rs:43) and [daemon main.rs:48-49](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemond/src/main.rs:48). The machine is underutilized because work is waiting behind actors, fsyncs, history scans, and locks—not because Tokio lacks worker threads.
