# v0.0.969 turnhygiene lane report

Status: **NO_SHIP**. The accepted performance changes and correctness battery pass,
but the mandatory footprint battery does not: daemon retention is above its
191 KiB/turn ceiling, and the diagnostic client-run floor is above 3 MiB. Client
wire measurements are additionally environment-blocked by denied `vmmap` task-port
access and therefore cannot be promoted to accepted passes.

## Recovery and scope

The continuation recovered the uncommitted worktree after the ENOSPC crash, re-read `LANE-COMMON.md`, the original brief recovered from the prior session log, `turnhygiene-tests.md`, and all `turnperf2` evidence, then rebuilt the deleted target from scratch. Rejected experiments are absent from the current tree. No commit, release, workflow, OAuth, oneshotboot, or removed-row work was performed.

Environment law for Rust work: `RUST_MIN_STACK=8388608`, `HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`, `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, two Cargo jobs, and prebuilt sibling binaries for process tests. Disk space was checked before every build.

Citation audit: the proposal's constructs were correct but nearly all line numbers drifted. Representative current anchors are budget estimation at `actor.rs:3550`, the turn-start bundle at `worker.rs:7261` / `sqlite_store.rs:362`, hook outbox insertion at `event_store.rs:19875`, the submit race buffer at `headless.rs:1193`, and instruction discovery at `project_instructions.rs:137,684` / `worker.rs:7643`. The `run.rs:553` unconditional profile-resolution citation remained exact; its companion read-only resolver moved from `profile.rs:313` to `profile.rs:316`. No cited construct was substantively wrong.

## Per-item warm ABBA

Each admitted cohort used the exact trace-off harness: 5 warm-ups and 25 retained samples per shape, alternating single/tool, with load1m below 3 at start/mid/end. Values are pooled untrimmed median ± MAD across the two A and two B cohorts (50 retained samples per shape), except the explicitly named R2-19 quiet hold-out. A is the item baseline and B is the isolated candidate. Negative deltas are improvements.

| Item | Verdict | Single wall A -> B, delta (ms) | Tool wall A -> B, delta (ms) | Combined CPU single/tool A -> B (ms) | Reason |
|---|---|---:|---:|---:|---|
| R2-12 unbudgeted estimator skip | KEEP | 60.462 ± 6.209 -> 57.057 ± 3.210, -3.405 | 80.396 ± 8.814 -> 77.534 ± 6.846, -2.862 | 4.668 -> 4.548 / 5.702 -> 5.434 | Repeatable wall and CPU improvement; capped-run decision pin stayed exact. |
| R2-19 exact turn-start bundle | KEEP | quiet hold-out 56.154 ± 2.754 -> 54.051 ± 2.482, -2.103 | quiet hold-out 75.568 ± 3.594 -> 73.472 ± 3.164, -2.096 | 4.901 -> 5.393 / 5.514 -> 6.307 | The first full ABBA was heteroscedastic despite admitted load; the ordered quiet hold-out reproduced a >=0.8 ms wall win. CPU increased 0.492/0.794 ms in that cohort and is reported, not credited. |
| R2-23 transaction-local append reuse | REVERT | 56.401 ± 4.287 -> 57.461 ± 5.446, +1.060 | 77.075 ± 6.568 -> 77.334 ± 6.681, +0.259 | 4.643 -> 4.719 / 5.716 -> 5.873 | Null/regression; fully reverted before the next item. |
| R2-21 hook-relevant outbox only | REVERT | 55.271 ± 3.359 -> 56.062 ± 3.973, +0.792 | 75.577 ± 5.727 -> 76.067 ± 8.367, +0.490 | 4.509 -> 4.504 / 5.223 -> 5.342 | Durable rows fell, but wall regressed and tool CPU rose; fully reverted. |
| R2-11 memory-first submit race buffer | KEEP | 59.762 ± 7.082 -> 55.392 ± 4.114, -4.371 | 82.285 ± 11.979 -> 80.862 ± 13.152, -1.423 | 5.053 -> 4.708 / 6.163 -> 5.427 | Wall, client CPU, combined CPU, and client peak RSS all improved; bounded spill path stayed pinned. |
| R2-14 direct bounded output sink | REVERT | 51.636 ± 5.199 -> 50.851 ± 3.395, -0.785 | 68.319 ± 2.286 -> 70.243 ± 4.129, +1.925 | 4.503 -> 4.806 / 5.401 -> 6.037 | Single wall gain missed the 1 ms family floor; tool wall and CPU regressed; fully reverted. |
| R2-22 read-only warm profile | REVERT | bracketed hold-out 52.353 ± 3.269 -> 73.007 ± 20.335, +20.654 | bracketed hold-out 71.137 ± 3.613 -> 96.862 ± 21.307, +25.726 | 4.672 -> 4.441 / 5.598 -> 5.388 | The closing baseline returned to the opening baseline range while the candidate wall cohort regressed severely; all nine R2-22-only files were restored byte-for-byte to `HEAD`. |
| R2-15 linear instruction walk + bounded cache | KEEP | 55.952 ± 5.075 -> 53.832 ± 4.732, -2.121 | 79.040 ± 8.858 -> 76.503 ± 6.754, -2.536 | 4.960 -> 4.836 / 6.149 -> 5.763 | Both admitted candidate cohorts improved tool wall; the closing half of ABBA also improved both shapes and CPU. The cache is capped at four entries / 256 KiB and revalidates anchored directory plus file identity, size, mtime, and ctime stamps. |

R2-19's full pooled ABBA is retained for audit rather than credited: single 71.858 ± 13.217 -> 58.363 ± 4.073 ms and tool 98.007 ± 18.126 -> 79.876 ± 9.740 ms. The much tighter quiet hold-out above is the acceptance evidence.

## Counts, bytes, and client resources

- R2-19 changes the one-page root-session turn-start shape from six store entries to one. The warmed fixture measured 85 allocations / 11,865 bytes for the legacy reads versus 29 allocations / 3,921 bytes for the exact bundle. Exact journal bytes, observed head, delegation lineage, graph state, and reducer inputs are mutation-pinned.
- R2-21's rejected prototype reduced durable hook-outbox rows from 21 to 5 for the single shape and 48 to 10 for the tool shape. That count reduction was not allowed to override the measured wall/CPU regression.
- R2-11 pooled client CPU was 4.676 -> 4.364 ms single and 5.528 -> 4.799 ms tool. Pooled client peak-RSS medians were 11,264 -> 10,816 KiB single and 11,648 -> 11,376 KiB tool (-448/-272 KiB).
- R2-14 pooled client CPU regressed 4.126 -> 4.449 ms single and 4.834 -> 5.432 ms tool. Client peak-RSS medians also rose 120/88 KiB.
- R2-22's bracketed baseline and candidate client CPU medians were 4.289 -> 4.082 ms single and 5.062 -> 4.796 ms tool. The CPU reduction did not override the admitted 20.654/25.726 ms wall regression.

## Family and one-shot results

| Scope | Warm single | Warm tool | One-shot wall | One-shot CPU total | Result |
|---|---:|---:|---:|---:|---|
| Family 4 start -> accepted R2-12 + R2-19 | 60.066 ± 5.415 -> 58.512 ± 3.968, **-1.554 ms** | 81.881 ± 9.520 -> 79.482 ± 5.161, **-2.400 ms** | Not required for Family 4 acceptance | Not separately scored | **PASS**: quiet hold-out exceeds the 0.8 ms warm-single floor |
| Family 3 start -> accepted R2-11 | 59.762 ± 7.082 -> 55.392 ± 4.114, **-4.371 ms** | 82.285 ± 11.979 -> 80.862 ± 13.152, **-1.423 ms** | 161.745 ± 23.789 -> 146.325 ± 24.114, **-15.420 ms** overall | 2,555.787 -> 2,372.774 ms, **-183.013 ms** | **PASS**: warm, one-shot, and CPU-total floors all exceeded |
| Final accepted tree vs original start | Family-specific cumulative warm results above | Family-specific cumulative warm results above | 186.118 ± 38.776 -> 177.912 ± 33.286, **-8.206 ms** overall | 3,440.577 -> 3,268.865 ms, **-171.712 ms** | Overall one-shot and CPU improve; footprint gate independently fails |

The 21-case TTL=0 harness validates the accepted-first contiguous JSONL stream, exactly one typed terminal, exact provider request ledger, monotonic tool effect, and owned-daemon teardown for every case. It records per-shape wall/CPU median ± MAD, suite CPU/wall totals, peak client RSS, binary hashes, and load at start/mid/end.

Family 4 also had a complete direct ABBA before the quieter hold-out: single
57.446 ± 4.147 -> 57.141 ± 3.983 ms (-0.305 ms), tool 80.915 ± 6.621 ->
76.339 ± 4.037 ms (-4.576 ms), with combined CPU 4.924 -> 4.971 ms single
and 5.872 -> 5.732 ms tool. Because its single delta was below the family floor,
the reported acceptance uses the predeclared quiet hold-out; its opening and
closing baseline cohorts bracket the candidate and every load snapshot is below 3.

For Family 3's one-shot comparison, the single shape was 143.077 ± 23.540 ->
141.467 ± 28.076 ms (-1.610 ms) and the tool shape was 175.376 ± 20.330 ->
162.399 ± 22.775 ms (-12.977 ms). Client CPU medians were 51.020 -> 47.648 ms
single and 71.135 -> 68.394 ms tool. Suite wall totals were 6,611.369 ->
6,462.078 ms, and maximum client peak RSS was 12,112 -> 11,968 KiB (-144 KiB).

The final-tree one-shot comparison is deliberately reported without hiding its
shape split: single wall was 158.762 ± 34.806 -> 164.937 ± 25.977 ms
(+6.175 ms), while tool wall was 205.027 ± 34.109 -> 203.129 ± 30.897 ms
(-1.898 ms). Suite wall total improved 7,739.652 -> 7,633.469 ms and maximum
client peak RSS improved 12,208 -> 11,952 KiB (-256 KiB). All one-shot cohorts
used 21 cases, TTL=0, A/B/B/A ordering, and admitted load below 3.

## Correctness and verification

Completed in this continuation:

- clean from-scratch debug build of `haider` and `haiderd` after deletion of `target/`;
- orchestrator behavior pins: 9/9 green (single/tool JSONL and provider body, detached/replay, profile binding, hook discovery, instruction discovery, early stdout);
- accepted-item pins: R2-12 estimator, R2-19 exact/allocation bundle and worker reducer, R2-11 memory/spill, and R2-15 linear/bounded/loss-detecting discovery all green;
- R2-15 new loss-detecting cache test plus existing same-turn/edit-next-turn, delete/promote, durable change-only fact, and sibling-workspace isolation pins green;
- full affected suites green: `haider-core`, `haider-store`, `haider-client`, `haider-cli`, and `haider-daemon` (921 passed / 3 pre-existing ignored in the daemon unit binary; 103/103 session-hub integration tests);
- scoped `--tests --no-deps` Clippy with `-D warnings` green for all five affected crates;
- `cargo fmt --all -- --check`, `git diff --check`, and the unsafe-count guard green; the latter records four test-only `GlobalAlloc` methods and no production increase from the allocation probe;
- `bash scripts/qa-gate/run.sh test`: 48/48 green.
- test-count baseline updated and verified at 4,375 (4,368 + seven accepted-item tests).

Final crash/resource battery:

- cumulative warm and 21-case one-shot measurements completed under the same
  correctness and load admission rules;
- N=3 daemon footprint completed with no rejected samples;
- N=3 client status/run diagnostics completed, but denied `vmmap` makes them
  `measurement_accepted=false`; the run diagnostic also exceeds its ceiling;
- SIGKILL matrix completed 47/47 with no duplicate provider request;
- final scoped Clippy, fmt, diff, QA self-tests, unsafe-count guard, and test-count
  checks are green.

## Footprint and crash matrix

| Gate | Budget | Result |
|---|---:|---:|
| Daemon settled idle | <= 5,683,282 B | **PASS**: N=3 median 5,554,632 ± 49,176 B |
| Daemon retention | <= 195,584 B/turn | **FAIL**: N=3 median 352,666.8 B/turn (growth 14,106,672 ± 65,536 B over 40 turns) |
| Daemon post-40-turn footprint | <= 13,506,642 B | **FAIL**: N=3 median 19,661,328 ± 49,152 B |
| Client status wire floor | <= 2,516,583 B | **ENVIRONMENT_BLOCKED**, not a pass: diagnostic N=3 median 2,392,448 B, max 2,408,832 B; every `vmmap` exited 255 |
| Client run wire floor | <= 3,145,728 B | **FAIL diagnostic / ENVIRONMENT_BLOCKED**: diagnostic N=3 median 3,146,136 B, max 3,178,904 B; every `vmmap` exited 255 |
| Peak RSS | <= 53,687,091 B | **PASS**: maximum daemon RSS in the N=3 footprint protocol 42,139,648 B; final one-shot maximum client RSS 12,238,848 B |
| SIGKILL boundaries | 47/47; duplicate provider requests = 0 | PASS: 47/47, zero failures, every store integrity check `ok`, zero repeated `(case_id, logical_ordinal)` requests |

Daemon raw settled growth was 14,106,672, 8,814,640, and 14,172,208 B,
or 352,666.8, 220,366.0, and 354,305.2 B/turn. Raw idle footprints were
5,603,808, 5,554,632, and 5,489,120 B. All three runs were accepted by the
protocol with load below 4. The absolute retention ceiling is binding even
though this lane's accepted items were selected for turn latency rather than
memory reduction.

The client harness successfully verified the typed status response, successful
headless terminal, exactly one provider request per run, and owned-daemon cleanup.
The managed host denied all six `vmmap -summary` task-port reads, so the harness
correctly marked both N=3 summaries rejected diagnostics. A prior run-surface
attempt was separately rejected when its settled-read load reached 4.55; none of
that overloaded sample is included above.

## CI registry walk and final verdict

- Registry #64: release `haiderd` is 52,374,224 B, above the 10 MiB minimum.
- Registry #77: the unsafe-count guard passes with 189 production and 20 test
  occurrences; the four-count test increase is the deliberate allocation probe.
- Registry #94: no deadline or timeout was added.
- Registry #95: no negotiated-connection wait or keepalive obligation was added.
- No QA-gate registry or workflow source was changed by this lane.

Accepted source items are R2-12, R2-19, R2-11, and R2-15. R2-23, R2-21,
R2-14, and R2-22 were fully reverted under the hold-out rule. The accepted code
is correctness-green and meets both family performance gates, but shipment is
blocked by the mandatory absolute footprint thresholds above.

## Optional rows

X1-1, C3-4, and C1-3 were not eligible: no isolated measured attribution of at least 0.3 ms was established. No optional code was added.
