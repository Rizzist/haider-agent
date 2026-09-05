**Yes, but the credible remaining improvement is modest: approximately 7 ms off the 148 ms per-case median, with a planning range of 4–12 ms. I recommend deferring implementation to 973.** The earlier large opportunities have mostly landed. Reaching roughly 40–60 ms requires an actually resident daemon and different lifecycle accounting.

Analysis is pinned to `wave-970 @ 471b9d68`. No files were changed, no bench was run, and no daemon was started or stopped.

**What the measurements actually establish**

| Fair eight-case comparison | Median wall | Mean wall | Interpretation |
|---|---:|---:|---|
| 969 linger → 969 TTL=0 | 98 → 126 ms | 106 → 160 ms | Same version; includes teardown and boot-cache policy differences |
| 969 TTL=0 → 970 TTL=0 | 126 → 148 ms | 160 → 155 ms | Median worsened; mean improved slightly |
| 969 linger → 970 TTL=0 | 98 → 148 ms | 106 → 155 ms | Mixes version and lifetime policy; **not a lifecycle decomposition** |

There is no measured 970 linger baseline in the supplied results. Forecasts against 98/106 below therefore use **969 linger as a conditional baseline**, not as an observed 970 warm result.

Across all 21 cases, 969 TTL=0 has 18 PASS / 1 FAIL / 2 SKIPPED; 970 has 16 / 3 / 2. `malformed_tool_call` and `no_second_model_or_auxiliary_provider` regress. Excluding timeout, total wall increases from 3,737 to 3,914 ms, although changed outcomes prevent treating this as a correctness-equivalent speed comparison. The fair eight-case set remains the appropriate primary comparison.

**“Persistent” still cold-spawns in this bench**

The adapter assigns a disposable `HOME` and `HAIDER_PROFILE_DIR` to each invocation. Configuration materialization occurs before the client timer starts. The client then resolves the canonical profile path, hashes it into the profile identity, and derives a profile-specific endpoint. The daemon launch explicitly receives that identity through `--profile`. Sources: [adapter.toml:58](/Users/rizzist/Documents/CODING/haidercode-web/bench/adapters/haider-agent/adapter.toml:58), [runner.py:400](/Users/rizzist/Documents/CODING/haidercode-web/bench/conformance/runner.py:400), [profile.rs:320](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/profile.rs:320), [spawn.rs:650](/Users/rizzist/haider-run/wt-965/crates/haider-platform/src/spawn.rs:650).

Consequently:

- **Linger:** each fresh profile still pays daemon launch, store initialization, readiness, the entire provider turn, journal persistence and JSONL delivery. The client exits without awaiting eventual daemon retirement.
- **TTL=0:** pays those stages **plus authenticated daemon shutdown, drain, checkpoint, SQLite close and child reap** before returning.
- Positive TTL also retains SQLite’s boot working set; TTL=0 releases reclaimable SQLite memory before accepting the first client. The two modes differ before the turn as well as afterward.

These distinctions are explicit in [spawn.rs:78](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/spawn.rs:78), [headless.rs:3606](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/headless.rs:3606), [runtime.rs:1657](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/runtime.rs:1657) and [runtime.rs:2160](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/runtime.rs:2160).

“Client-only” therefore describes the observed process boundary, **not the work its wall clock includes**. The runner timestamps client launch through observed client exit and extracts resource figures from `wait4`; it does not independently measure a simultaneously resident daemon. The 59 ms client-only CPU total cannot be compared directly with whole-lifecycle CPU as evidence of equivalent total work. See [runner.py:421](/Users/rizzist/Documents/CODING/haidercode-web/bench/conformance/runner.py:421) and [runner.py:541](/Users/rizzist/Documents/CODING/haidercode-web/bench/conformance/runner.py:541).

**Measured floor and current path**

I timed `/usr/local/bin/haider --version`, which reports `0.0.969`, using subprocess launch through completion with captured output: two warm-ups, then 30 retained samples.

| Read-only measurement | Median | Mean | Minimum | p90 |
|---|---:|---:|---:|---:|
| Installed `haider --version` | 3.92 ms | 3.89 ms | 3.40 ms | 4.35 ms |
| `status --json --no-spawn` | Unavailable | Unavailable | — | — |

Load was 7.45 during the version samples, above the supplied bench’s 3.5–5.6. The status attempt failed because the sandbox denied the UDS connection with `Operation not permitted`; I did not retry through another access path.

Thus **approximately 4 ms is a measured executable-start surrogate**, not a universal lower bound. `--version` and `run` select different runtime paths; headless commands already use a current-thread runtime and avoid constructing the TUI dispatcher. See [main.rs:73](/Users/rizzist/haider-run/wt-965/crates/haider-cli/src/main.rs:73).

A lightweight client plus one immediate loopback HTTP exchange plausibly costs **approximately 5–8 ms**, but the HTTP increment is an estimate, not a measurement made here. Status would measure profile resolution, UDS, Hello/Welcome and a status RPC—not HTTP.

**The requested stage decomposition is underdetermined.** Neither aggregate medians nor static source can identify individual stage times, and 98 ms is not a measured warm 970 command. The following provides a transparent reconciliation rather than inventing measured phase attribution.

Numbers are illustrative budgets in milliseconds. The residual explicitly retains unidentified costs, additional tool/request work, version differences and scheduling variation. Stage medians cannot generally be added.

| Stage | Mechanism evidence | Linger budget | TTL=0 budget |
|---|---|---:|---:|
| Client launch and runtime | Measured executable surrogate; [main.rs:157](/Users/rizzist/haider-run/wt-965/crates/haider-cli/src/main.rs:157) | 4 | 4 |
| Profile resolution/materialization | Canonical path, identity, endpoint and defaults; [profile.rs:330](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/profile.rs:330) | 1 | 1 |
| **Cold daemon launch and initialization** | Store, generation, recovery, registry and hub before Ready; [runtime.rs:753](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/runtime.rs:753), [runtime.rs:934](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/runtime.rs:934) | **30** | **30** |
| UDS attach, credentials, Hello/Welcome | [client.rs:290](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/client.rs:290) | 2 | 2 |
| Session create and attach/caught-up | Separate acknowledged operations; [headless.rs:2997](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/headless.rs:2997), [headless.rs:3130](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/headless.rs:3130) | 7 | 7 |
| Submit, worker admission and request assembly | Durable context reads and provider/tool setup; [worker.rs:8132](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/worker.rs:8132) | 20 | 20 |
| Provider-view CAS and attempt publication | Ordered CAS preparation plus atomic index/journal commit; [event_store.rs:2241](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/event_store.rs:2241) | 6 | 6 |
| Local HTTP and SSE consumption | HTTP starts after attempt commit; [actor.rs:4432](/Users/rizzist/haider-run/wt-965/crates/haider-core/src/actor.rs:4432) | 2 | 2 |
| Completion journal, projection and terminal | Durable narrative/terminal publication; [journalview.md:37](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/journalview.md:37) | 6 | 6 |
| JSONL flushing and client completion | Output adapter is joined; terminal/queue drain flushes; [run.rs:869](/Users/rizzist/haider-run/wt-965/crates/haider-cli/src/run.rs:869), [run.rs:1849](/Users/rizzist/haider-run/wt-965/crates/haider-cli/src/run.rs:1849) | 2 | 2 |
| **Owned daemon drain, checkpoint, close and reap** | [headless.rs:2400](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/headless.rs:2400), [event_store.rs:12904](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/event_store.rs:12904) | **0** | **25** |
| **Unattributed residual** | Cannot resolve from supplied totals | **18** | **43** |
| **Reconciled total** | **Budget, not measured attribution** | **98** | **148** |

The rough anchors are historical cold-status ≈36 ms and a historical accept-to-attempt gap of 23.5 ms, recorded in Git at `fa2174a2^:docs/testing/v0.0.970/turnperf2/FACTS2.md:26` and `…/TRACE-FINDINGS.md:15`. Those files were subsequently removed; their numbers are historical evidence, not current measurements.

A reasonable lifecycle hypothesis is **20–45 ms startup in either mode**, plus **10–45 ms teardown under TTL=0**. The illustrative midpoint is 30 ms within the 98 ms result and 55 ms within the 148 ms result. Confidence is low. Neither the old 85–90 ms lifecycle estimate nor the current cross-version 50 ms difference is a measured current attribution.

**The six old fixes: current status**

| Earlier proposal | State at `471b9d68` | Remaining credit |
|---|---|---|
| Replace 25/50/100 ms readiness polling | **Fixed for normal owned cold launch.** Awaits readiness notification; backoff remains for fallback/competing endpoint cases. [spawn.rs:570](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/spawn.rs:570) | No normal-case 25 ms saving |
| Replace 65,533 individual closes | **Fixed for the cited cost; platform-specific implementation.** Linux has `CLOSE_RANGE_CLOEXEC`; macOS enumerates `/dev/fd` and uses a bounded CLOEXEC sweep with 64-slot headroom. [process.rs:764](/Users/rizzist/haider-run/wt-965/crates/haider-platform/src/process.rs:764), [process.rs:794](/Users/rizzist/haider-run/wt-965/crates/haider-platform/src/process.rs:794) | No 65K-syscall tax to remove |
| Fresh-profile migration bootstrap | **Fixed.** Schema-zero databases install latest schema and migration audit in one transaction; current schema version is 28. [migrations.rs:22](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/migrations.rs:22), [migrations.rs:1224](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/migrations.rs:1224) | Zero |
| Avoid repeated expiry sweep | **Fixed.** Open sweeps once; subsequent persists sweep when due—64 persists or one hour. [event_store.rs:2122](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/event_store.rs:2122), [provider_view_store.rs:31](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/provider_view_store.rs:31) | Zero for ordinary fresh cases |
| Batch provider-view/cache-attempt facts | **Fixed, further than originally proposed.** View index, attempt facts and pending Thinking transition share publication. [actor.rs:10823](/Users/rizzist/haider-run/wt-965/crates/haider-core/src/actor.rs:10823) | Zero |
| Replace 20 ms child-exit polling | **Fixed.** Retained child uses OS exit notification through `wait_for_child_exit`. [headless.rs:2594](/Users/rizzist/haider-run/wt-965/crates/haider-client/src/headless.rs:2594) | Zero |

The old provider-view flush model also needs replacement. Current provider-view blobs receive plain file/directory syncs followed by **one `F_BARRIERFSYNC` ordering fence**. Generic CAS/checkpoint groups retain Full durability. SQLite remains WAL/NORMAL. Therefore “≥8 full flushes × 4 ms per request” does **not** describe this path. Sources: [provider_view_store.rs:148](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/provider_view_store.rs:148), [cas.rs:574](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/cas.rs:574), [event_store.rs:199](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/event_store.rs:199).

Group commit has no intentional batching timer: it drains already queued requests and commits immediately. Serial actor awaits cannot be coalesced by adding a delay. [session_hub/mod.rs:1621](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/session_hub/mod.rs:1621).

**What the named 970 work contributes**

| Work | Relevance to this command |
|---|---|
| `turnhygiene3` | Skips budget estimation when no guard consumes it and improves instruction discovery. Already present; cannot credit again. [actor.rs:4152](/Users/rizzist/haider-run/wt-965/crates/haider-core/src/actor.rs:4152), [project_instructions.rs:105](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/project_instructions.rs:105) |
| `tpsfix` | TUI estimator/widget work; no headless wall-time saving. [tpsfix.md:1](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/tpsfix.md:1) |
| `replyarena2` | Shared reply storage and incremental hashing reduce copying/RSS. Commit `fa2174a2` reports wall-neutral results. [replyarena2.md:45](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/replyarena2.md:45) |
| `codepagediet` | **No product optimization shipped.** PGO regressed warm and one-shot timing and was rejected. [codepagediet.md:238](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/codepagediet.md:238) |
| `casstream` | Large-upload improvement, not transferable to small text-only conformance commands. [casstream.md:81](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/casstream.md:81) |
| `economydiet` | Smaller tool envelopes/manual. Its documented development-profile ABBA improved warm shapes but left one-shot median unchanged: 107.42→107.43 ms. [economydiet.md:123](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/economydiet.md:123) |
| `providerrebind` | Preserves request-boundary authority; release ABBA was within MAD, not a speed claim. [providerrebind.md:147](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/providerrebind.md:147) |
| `journalview` | Adds durable request correlation and conditional terminal markers; the ordinary final-text suffix avoids an extra append. [journalview.md:37](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/journalview.md:37) |
| `ceilingdecl` | Adds exact workspace-baseline work before first dispatch, committed with the attempt. Removing it would lose declared evidence. [ceilingdecl.md:32](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/ceilingdecl.md:32) |
| `agentcli` | **Not landed at this SHA.** `git merge-base --is-ancestor 3b2293e9 HEAD` returns 1; exclude it from attribution. |

The `turnperf12*.md` documents are under **v0.0.969**, not v0.0.970. Their tracing and crash matrix are useful infrastructure; their old stage measurements are not a current 970 decomposition.

**Ranked proposals**

These are investigation candidates, not individually measured improvements. Savings below are **median / mean milliseconds per command**. Ranking considers expected benefit, risk and implementation effort.

| Rank | Proposal and location | Linger saving | TTL=0 saving | Risk, size and contract impact |
|---|---|---:|---:|---|
| **P1** | **Group creation of the two CAS namespaces.** Store open creates `cas/` then `provider-view-cas/`; each new namespace separately fully syncs the same profile directory. Create both under one store-owned initialization operation and fully sync before either is used. [event_store.rs:2120](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/event_store.rs:2120), [cas.rs:288](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/cas.rs:288) | **3 / 4** | **3 / 4** | Low–medium, **M**. Intended published-contract change: **none**. Retain full namespace durability before use; preserve partial-creation/error recovery. No request barrier or journal boundary moves. |
| **P2** | **Group diagnostic-key file and directory durability.** Creation currently performs Full sync on the file and then Full sync on its directory. Investigate plain phase-one synchronization followed by one final Full group flush before returning the key. [session_hub/mod.rs:385](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/session_hub/mod.rs:385) | **3 / 4** | **3 / 4** | Medium, **M**. Intended contract change: **none**, conditional on proving unchanged Full-at-return and failure semantics. Keep the persistent key and fail-closed errors. Requires filesystem ordering/power-loss reasoning, beyond SIGKILL tests. |
| **P3** | **Retain SQLite boot pages through the first TTL=0 turn.** Avoid releasing pages immediately before reusing them; consider release at a genuine idle boundary. [runtime.rs:1657](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/runtime.rs:1657) | **0 / 0** | **1 / 2** | Low correctness risk, **S**; memory tradeoff. No intended wire/durability contract change. Must pass RSS/retention gates and preserve truthful `warm` reporting. Saving could be zero. |

P1 and P2 each have a plausible **2–5 ms median / 2–6 ms mean** saving. Together they target two identifiable full flushes, not hypothetical journal flushes. The owner’s historical ≈4 ms/full-flush measurement supports the order of magnitude, but not a current isolated saving.

P2 cannot simply become “lazy key creation”: the first provider request builds cache diagnostics using that key, so laziness would mainly move the cost into the same timed command. [actor.rs:4340](/Users/rizzist/haider-run/wt-965/crates/haider-core/src/actor.rs:4340).

I would credit **no fresh-profile median saving** for eliminating provider-view barriers through deduplication: new request blocks still require the fence. Removing exact views, bypassing CAS-before-index ordering, or issuing HTTP before the committed attempt changes correctness guarantees.

Also exclude:

- RPC/admission fusion: previous experiments regressed latency or duplicated a provider request. [providerrebind.md:83](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/providerrebind.md:83).
- Reintroducing the old read bundle or memory-first submit buffer unchanged: their retention investigation is already assigned to 972. [OPTIMIZATIONS.md:7](/Users/rizzist/haider-run/wt-965/docs/OPTIMIZATIONS.md:7).
- Skipping orderly checkpoint/reap to improve TTL=0 wall: that changes the operation being measured and its lifecycle guarantees.

**BEFORE → EXPECTED AFTER**

All cells are milliseconds. These are conditional planning translations of the supplied baselines, **not measured candidate results**. Constant savings are an approximation; actual case-specific changes can reorder the median.

| Proposal applied alone | Linger median | Linger mean | TTL=0 median | TTL=0 mean | Confidence |
|---|---:|---:|---:|---:|---|
| P1: namespace group | 98 → **95** | 106 → **102** | 148 → **145** | 155 → **151** | Low–medium mechanism; low numerical |
| P2: diagnostic-key group | 98 → **95** | 106 → **102** | 148 → **145** | 155 → **151** | Low numerical; proof-dependent |
| P3: retain boot pages | 98 → **98** | 106 → **106** | 148 → **147** | 155 → **153** | Low |

| Cumulative portfolio | Linger median | Linger mean | TTL=0 median | TTL=0 mean |
|---|---:|---:|---:|---:|
| Before | **98** | **106** | **148** | **155** |
| P1 | 95 | 102 | 145 | 151 |
| P1 + P2 | 92 | 98 | 142 | 147 |
| P1 + P2 + P3 | **92** | **98** | **141** | **145** |
| Plausible portfolio range | **88–94** | **94–102** | **136–144** | **140–150** |

The ranges are judgment ranges, not statistical confidence intervals. Zero benefit or a regression remains possible until tested. The central forecast is approximately **5% median improvement under TTL=0**, substantially less than the old 964 bundle estimates.

**What a genuinely resident path can reach**

Current 970 release evidence already records approximately **41 ms single-request / 59 ms tool-command medians** with a stable resident daemon. The harness starts the daemon before timing and subsequently launches each client command against that same identity. [providerrebind.md:149](/Users/rizzist/haider-run/wt-965/docs/testing/v0.0.970/providerrebind.md:149), [turn_wall_harness.py:534](/Users/rizzist/haider-run/wt-965/scripts/qa-gate/turn_wall_harness.py:534).

A reasonable resident-path target is **35–45 ms for a small single-request command and 50–65 ms for the two-request tool shape**, retaining existing durability. These are different shapes from the fair eight-case aggregate.

| Accounting alternative | Before | Expected timed-client result | Qualification |
|---|---:|---:|---|
| Fresh-profile linger → genuinely resident | 98 median / 106 mean | **40–65 median / 50–80 mean** | Low-confidence fair-set extrapolation |
| Fresh-profile TTL=0 → genuinely resident | 148 median / 155 mean | **40–65 median / 50–80 mean** | Startup and final retirement move outside the timer |

Those larger apparent savings are **not cumulative with P1–P3**: residency removes or amortizes the stages those proposals optimize. Prewarming every disposable profile merely moves setup outside the timer. Real repeated use of one profile amortizes it.

Rick’s 38 ms fair-set median is a useful observed comparator, but not a universal floor or proof that haider can match its durability-free, single-process design.

**Recommendation: defer implementation to `973-commandwall`**

The bounded P1–P3 portfolio is worth investigating, but not worth treating as an established 970 performance win. Two candidates alter filesystem synchronization sequencing; the third trades memory for a small speculative saving. Meanwhile, 970 has two newly failing conformance cases, and the loaded single-pass measurements provide weak evidence for small improvements.

The future lane’s scope should be **cold startup synchronization and first-turn cache retention**, with no request-admission, provider-view exactness or terminal-contract changes.

Acceptance should require:

1. Re-run the **same old 21-case conformance bench**, frozen adapters/proxy and release binaries, interleaved baseline/candidate, separately for linger and TTL=0. Preserve all cases and raw outcomes; keep the fixed fair eight-case comparison.
2. Obtain a real **970 linger baseline**. Record monotonic phase durations for launch→Ready, create/attach/accept, request preparation, terminal→exit and shutdown/checkpoint/reap. Join process traces by correlation; do not subtract unrelated clocks.
3. Require repeatable improvement beyond paired noise—approximately **≥5 ms fair-set median and mean improvement** for the combined cold portfolio—without new correctness failures, duplicate provider requests, missing effects, replay differences or orphan daemons.
4. Preserve existing readiness, crash/recovery, JSONL and lifecycle gates. For P1/P2, additionally prove full durability before publication and recovery from each partial initialization boundary; SIGKILL alone does not prove power-loss ordering.
5. Pass the existing warm-wall and memory/retention gates. Keep timeout and retry behavior intact; their deliberate waiting must not be shortened merely to improve the score.

VERDICT: DEFER