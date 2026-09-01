# v0.0.969 memdaemon2 part-2 report

Verdict: **NO_SHIP**. Correctness-safe derived-state trimming reduces settled
growth from the adopted 276,481 B/turn to 186,369 B/turn, but that remains
3.64x the 50 KiB target. A client that remains attached for the complete
60-second settle measures 16,122,360 B, 3,539,448 B over the generous 12 MiB
interpretation of the target. Mimalloc is materially worse on footprint, peak,
and turn CPU, so it is feature-gated and not default. The OAuth constructor is
now lazy and its full behavior suites pass.

## Recovery and scope

The continued worktree was recovered with `git status`, `git diff --stat`, and
this report before any new action. Part 1 is accepted as instructed and is used
only as the before row. No commit, rebase, release, or workflow action was
performed. `vmmap` was not retried; the sampler no longer invokes it.

The part-2 source changes are:

- opt-in per-turn SQLite/CAS structure snapshots and an opt-in runtime cache
  shape trace with no journal content;
- correctness-safe prompt-history compaction at every terminal idle boundary,
  retaining a compiled prefix, the minimal tree-node ancestry spine, and the
  one exact source key required by manual retry; the delayed idle release still
  drops all prompt, observe, and turn-setup state after five seconds;
- a 16-terminal-run bound on each observe projection;
- a 30-second TTL for a durably quiescent worker supervisor;
- OAuth coordinator construction retains only the zero-sized shared transport
  handle and acquires its client on the first HTTP operation;
- an opt-in `haider-daemond/mimalloc` Cargo feature, not in default features;
- an attached-settle checkpoint in the deterministic workload and sampler.

The observe bound and supervisor TTL are the two smallest changes in the
parallel-owned retain territory. They are named explicitly because the part-2
brief directly requested attached projection bounds and reconstructible worker
state retirement. The OAuth change stays inside the brief's narrowly lifted
fence.

## Measurement protocol and artifact identity

macOS physical footprint and CPU come from `proc_pid_rusage` for the exact
daemon PID. A run waits 60 seconds before idle, drives one 40-turn session,
keeps the RPC client and event stream attached for another 60 seconds, closes
the client, and waits 60 seconds before the post-turn sample. Runs with any
sampled load1m >= 4 are rejected.

Final correctness-safe sample:

- result: `/tmp/memdaemon2-final-fixed-system-n1.json`;
- one admitted run at maximum load1m 2.894; one load-4.767 run rejected and
  replaced;
- daemon: `/tmp/memdaemon2-final-fixed-system-haiderd`, SHA-256
  `287939da96a62966e919904f91563318e21f555318e1038025ba20c8a18caee8`,
  52,291,472 bytes;
- driver: `/tmp/memdaemon2-workload`, SHA-256
  `a5d9b64f10816206205c171affe1d34244d843fb7a2e2bcd5e4c5e67a18d6460`,
  13,542,112 bytes.

The final semantic repair followed an exhaustive test run that exposed missing
tree parents in branch/fork/retry paths. Therefore the final release binary has
N=1, while the immediately preceding candidate has N=5. That preceding N=5 is
useful corroboration (151,553 B/turn and 15,974,928 B attached), but is not
claimed as shippable because it had the correctness defect.

## Final before/after row

The before row is the orchestrator-adopted part-1 N=5 median. The after row is
the admitted correctness-safe final sample. CPU was collected with the opt-in
retention trace enabled, so it includes the diagnostic counter work.

| Metric | Adopted part 1, N=5 | Correctness-safe part 2, N=1 | Delta |
|---|---:|---:|---:|
| Settled idle footprint | 5,472,736 B | 5,587,400 B | +114,664 B (+2.1%) |
| Settled post-40 footprint | 16,515,600 B | 13,042,168 B | -3,473,432 B (-21.0%) |
| Settled growth over 40 turns | 11,059,248 B | 7,454,768 B | -3,604,480 B (-32.6%) |
| Settled bytes/turn | 276,481 B | 186,369 B | -90,112 B/turn (-32.6%); **target fail** |
| Turn-20 daemon CPU | 9,265,622 ns | 11,753,109 ns | +2,487,487 ns (+26.8%); material |
| CPU during 60 s initial idle | 66,033 ns | 55,631 ns | -10,402 ns; immaterial near-zero delta |
| Immediate turn-40 footprint | 20,840,976 B | 19,792,376 B | -1,048,600 B (-5.0%) |
| Exact large-reply peak | 50,741,248 B | not rerun after semantic repair | no final claim |

The target comparison is against 50 KiB = 51,200 B/turn. Even the invalidated
but repeated N=5 candidate median was 151,553 B/turn, so sampling uncertainty
cannot bridge the target gap.

## Per-structure attribution

Durable structure slopes are turn 1 to turn 40 divided by 39 intervals in the
admitted final run. Runtime cache values are the opt-in trace immediately after
per-turn compaction; every runtime structure in the table was zero at the
delayed `released` snapshot while the client was still attached.

| Structure | Turn 1 | Turn 40 | Observed slope | Retention disposition |
|---|---:|---:|---:|---|
| Journal JSON | 26,898 B | 996,217 B | 24,854 B/turn | Durable authority; not evictable |
| Journal events | 44 rows | 1,522 rows | 37.90 rows/turn | Durable authority |
| Effect records | 4 rows | 160 rows | 4.00 rows/turn | Durable effects; no duplicate heap ledger found |
| Hook outbox | 39 rows | 1,362 rows | 33.92 rows/turn | Durable hook replay authority; hook snapshot remained 0 B |
| Command receipts | 3 / 1,248 B | 41 / 13,869 B | 0.97 rows and 324 B/turn | Durable idempotency authority |
| Run heads | 2 / 34 B | 40 / 640 B | 0.97 rows and 16 B/turn | Durable latest-state projection |
| Provider-view ledger / CAS refs / CAS files | 0 | 0 | 0 | No retention in this workload |
| Projection checkpoints | 0 | 0 | 0 | No retention in this workload |
| Graph projection | 1 / 823 B | 1 / 16,162 B | 393 B/turn | One durable reconstructible row; small relative to residual |
| SQLite logical pages | 103 | 589 | 12.46 pages/turn | About 51,044 logical B/turn; file grew about 60,495 B/turn |
| SQLite WAL | 1,388,472 B | about 4.16 MB | plateaued by turn 4 | Reused fixed high-water, not continuing per-turn growth |
| Prompt cache after compaction | 3,425 B | 330,764 B | 8,393 B/turn | 284,730 B node spine + 46,034 B shared projection; zero after 5 s |
| Prompt journal indexes | 0 | 0 | 0 | Rebuilt on demand; exact source count stays 1 for retry correctness |
| Observe projection | 0 B / 0 runs | 23,407 B / 16 runs | bounded at 16 terminal runs | Zero after 5 s |
| Turn-setup cache | 1 entry | 1 entry | flat | Zero after 5 s |

At turn 40, the pre-compaction prompt contained 508,098 B: 462,064 B of
decoded envelopes and 46,034 B of projection. Compaction reduced that to
330,764 B; the five-second idle release then reduced prompt, observe, and
turn-setup counters to zero. The remaining settled process slope is therefore
not an unbounded prompt/observe cache. Its dominant measured correlates are the
durable SQLite page growth and allocator/database high-water, plus durable
journal structures that cannot be discarded without a retention policy or
schema-level change.

## Lazy shared OAuth client

Before, `OAuthCoordinator::new_with_vault` called `SharedHttpTransport.client()`
and cloned the resulting `reqwest::Client` during ordinary daemon construction.
After, the coordinator retains `SharedHttpTransport` and calls `client()` only
inside device authorization, device polling, or authorization-code exchange.
No refresh preparation, generation fence, vault, timeout, redirect, or request
payload behavior changed.

| OAuth row | Before | After | Hold-out result |
|---|---:|---:|---|
| Client acquisition during coordinator construction | 1 | 0 | PASS, mutation-pinned |
| Combined daemon idle footprint | 5,472,736 B | 5,587,400 B | +2.1%; not isolated from retention work |
| Combined turn-20 CPU | 9,265,622 ns | 11,753,109 ns | not isolated |
| Combined peak | 50,741,248 B | not rerun on final binary | no isolated performance claim |

Verification passed: all 90 OAuth module tests, all 4 real-UDS
`oauth_rpc_tests`, and 20/20 independent executions of
`cancelled_resolver_does_not_abandon_or_duplicate_refresh_flight` with the
required disk preflight before every process.

## Mimalloc A/B

The allocator experiment was performed on the immediately preceding candidate,
before the final prompt ancestry correctness repair. That repair applies
identically above the allocator boundary. Because mimalloc failed every owner
adoption condition by margins many times MAD, it was not rebuilt for another
15-minute N=5 settle after the conclusion was already decisive. It remains an
opt-in compile-checked feature and is not default.

System allocator artifact: `/tmp/memdaemon2-final-system-n5.json`, daemon
SHA-256 `6ae61a153c179563f9cd8e7cfc7f7096a85b677a137c7b337395ca096a350363`.
Mimalloc artifact: `/tmp/memdaemon2-final-mimalloc-n5.json`, daemon SHA-256
`a9922192568a57469316bd1d048d1a1f9e0521f475c49df8fe38a3f0be5726cb`.
System admitted 5/5; mimalloc rejected one load-4.011 attempt and replaced it.

| Metric, median (MAD), N=5 | System allocator | Mimalloc | Delta |
|---|---:|---:|---:|
| Settled idle | 5,521,864 B (49,176) | 7,488,040 B (81,920) | +1,966,176 B (+35.6%) |
| Settled post-40 | 11,715,088 B (344,088) | 27,918,984 B (753,640) | +16,203,896 B (+138.3%) |
| Settled growth | 6,062,128 B (163,840) | 20,414,560 B (933,888) | +14,352,432 B (+236.8%) |
| Settled bytes/turn | 151,553 B | 510,364 B | +358,811 B/turn (+236.8%) |
| Attached after 60 s | 15,974,928 B | 28,574,320 B | +12,599,392 B (+78.9%) |
| Turn-20 CPU | 8,950,500 ns (248,082) | 10,473,597 ns (804,650) | +1,523,097 ns (+17.0%), beyond MAD |
| Initial idle CPU | 42,169 ns (7,072) | 35,330 ns (4,541) | -6,839 ns; immaterial near zero |
| Exact 1,114,112-byte reply peak | 49,840,128 B (1,146,880) | 57,786,368 B (1,294,336) | +7,946,240 B (+15.9%) |

Peak summaries are under `/tmp/memdaemon2-m1-system*` and
`/tmp/memdaemon2-m1-mimalloc`. Five valid samples per side had load1m <= 2.69,
two provider requests, and exact large-delta/completed-item/done anchors. The
system side discarded sampler-window misses and used the first five valid
samples; incomplete runs were not counted.

**Allocator verdict: REJECT AS DEFAULT.** Footprint and peak both regress
materially, and turn CPU is worse beyond MAD.

## Attached-client result

The initial pre-bound attached diagnostic was 13,550,048 B. The repeated N=5
candidate median after the observer/supervisor bounds was 15,974,928 B, and the
correctness-safe final sample was 16,122,360 B. The trims did not reach the
target.

The final runtime `released` trace still reported one attachment while prompt,
observe, and turn-setup retention were all zero. Closing that client and
settling another 60 seconds reduced footprint to 13,042,168 B, a 3,080,192 B
drop. This attributes the remaining attached excess to the live connection,
writer/outbox, replay/catch-up task graph, and allocator pages owned by those
objects rather than the now-bounded observer projection. The default catch-up
channel is already frame-bounded (64) and byte-bounded (8 MiB), with the store
as the lag buffer; its empty capacity is not preallocated as 8 MiB, so simply
lowering the declared byte ceiling would not explain or safely recover the
measured 3.08 MB.

| Attached row | Before trim | Final | Result |
|---|---:|---:|---|
| Footprint after 60 s attached | 13,550,048 B (N=1) | 16,122,360 B (N=1) | +2,572,312 B; **target fail** |
| Footprint after detach + 60 s | not captured in first diagnostic | 13,042,168 B | connection-owned delta 3,080,192 B |
| Turn-20 CPU | diagnostic not isolated | 11,753,109 ns | no attached-only CPU claim |
| Large-reply peak | diagnostic not isolated | allocator A/B above | no attached-only peak claim |

## Functional verification

- Current correctness-safe source: `cargo fmt --all -- --check`,
  `git diff --check`, Python source compilation, shell syntax, and scoped
  all-target deny-warnings Clippy passed. Clippy explicitly enabled
  `haider-daemond/mimalloc`.
- Current `haider-core`: 230 active tests passed, one unchanged manual timing
  probe ignored.
- Current `haider-daemon --lib`: 916 passed, 3 unchanged live/platform ignores.
  This includes the complete OAuth module and all new prompt/observe/TTL pins.
- Current real-daemon prompt-fork regression and all 4 `oauth_rpc_tests` passed.
  The 8 manual-compaction interaction tests and retry-of-retry mutation test
  passed after the ancestry repair.
- The earlier combined affected run had all client, core, session-hub, and
  daemon integration groups green except the six prompt ancestry regressions
  it discovered. Those six were repaired and rerun; the full combined command
  was not repeated after two unrelated long-running live-turn cases had already
  made the failed invocation non-admissible.
- `cargo run -p xtask --locked -- check` passed: 4,349 tests versus baseline
  4,336; only the existing soft LOC warnings were emitted.
- The final release daemon is a 52,291,472-byte Mach-O, above registry #64's
  10 MiB truncation floor, and was executed in the admitted settled workload.

No tests were weakened, ignored, or platform-gated.

## CI error registry walk

No new registry class was discovered.

| Class | Result |
|---:|---|
| 1/2/3/5/6/9/10/11/39 | Affected source compiles; scoped all-target Clippy with `-D warnings` passes. |
| 7/34 | Optional mimalloc dependency is lockfile-resolved; all verification used `--locked`; it is not a default feature. |
| 8/19 | Rustfmt and `git diff --check` pass; no mechanical unrelated sweep. |
| 20 | Test-count guard passes at 4,349 versus baseline 4,336. |
| 21/54/67 | Required 8 MiB stack and discovery environment used; daemon siblings were prebuilt. |
| 23/24/27/31/42/52/66/72/76 | No schema, provider authority, Windows wire, Android, TUI UI, STT, discovery authority, or public wire-field change. |
| 33/74 | Measurements use explicit temporary roots and exact daemon PIDs; no HOME mutation. |
| 44 | `vmmap` known denial was not retried; authoritative samples use `proc_pid_rusage`. |
| 45 | No new unsafe code. |
| 48/61 | Each behavior claim has a named mutation pin; measurement identity/load/anchors are enforced. |
| 64/71 | Real 52 MB release binaries were executed for footprint, CPU, attached, and peak cases. |
| 70/78 | No workflow trigger or publishing change. |
| 94/95 | Five-second derived-cache release and 30-second supervisor TTL are cache policies, not protocol deadlines; no negotiated connection waits on external state. |

Green correctness checks do not override the two binding footprint failures,
the material turn-CPU regression in the final diagnostic, or the missing final
N=5/peak rerun after the semantic repair.

NO_SHIP
