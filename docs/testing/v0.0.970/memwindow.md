# v0.0.970 lane memwindow — daemon memory follows the live context window

**Verdict: NO_SHIP. The branch is HELD for 972 by orchestrator decision.**

The lane set out to make a compacted session's in-memory working set equal that
of a fresh session with the same live window. It does not, and the measurements
say why: at a compaction boundary the structures this lane can release are
already nearly empty, while the footprint that stays behind is roughly twenty
times larger than everything those structures hold. Three designs were built and
measured; each was null or negative. The valuable output is the attribution, not
the code.

---

## 1. Measurement protocol

macOS physical footprint and CPU come from `proc_pid_rusage` on the exact daemon
PID, sampled by `scripts/perf/daemon-footprint-budget.py`. A run waits for the
daemon to settle, drives the workload through the deterministic fake provider
(`process_exec ":"` per turn), then settles again before the post-turn sample.
Every comparison below is an interleaved A/B (ABBA) so that machine drift
cancels: A and B each occupy positions {1,4} and {2,3}.

- daemon under test — `haiderd` built from this branch, release profile,
  52,983,808 B (over the 10 MiB floor, registry #64);
- baseline — `haiderd` built from `origin/wave-970` @ `39d38929`, 52,983,776 B,
  produced in this worktree by checking the lane's files out to the wave head
  and back; the only source difference during that build was
  `crates/haider-client/examples/memdaemon_workload.rs`, which is not linked
  into `haiderd`, so the baseline binary is exactly the wave head's daemon;
- driver — the lane's `memdaemon_workload` example for both arms, so the two
  arms issue byte-identical RPC sequences;
- statistics — median ± MAD over accepted runs, N given per row.

**This machine was not quiet for the final block.** A peer lane held the
1-minute load average between 4 and 7 for the last two hours. The 40-turn and
uncompacted-200-turn blocks ran under the calibrated `load1m < 4` gate. The
final compaction ABBA could not: it ran with `--max-load-1m 9` (a knob this lane
added, defaulting to the calibrated 4.0) and is reported as such. Absolute
footprints under load are inflated; the interleaved A/B comparison remains valid
because both arms sat in the same load window.

---

## 2. Measurements

### 2.1 Landing rule: 40 turns, no compaction, ABBA, N=4/side, load1m < 4

| | wave head | lane | ratio |
|---|---|---|---|
| settled growth per turn | 253,339 ± 30,720 B | 264,398 ± 5,735 B | **1.044** |
| settled footprint after 40 turns | 15,868,432 ± 1,245,184 B | 16,261,636 ± 286,732 B | +393,204 B |
| idle footprint | 5,743,072 ± 57,344 B | 5,734,868 ± 24,576 B | 0.999 |
| workload wall | 2,223 ± 65 ms | 2,221 ± 20 ms | 0.999 |

Retention ratio 1.044 (≤ 1.15), post-40-turn delta +393 KB (< 1 MB, and inside
the baseline's own 1.25 MB MAD), no wall loss. **This row passes.** It is also
null: the wave head measured 245,966 / 275,662 / 253,339 B/turn on three
separate passes of the *same binary*, a ±12% spread, so a 4.4% difference is
noise. Nothing in this lane fires within 40 uncompacted turns.

### 2.2 Compaction: 200 turns, compacted every 50, ABBA, N=4/side, load1m < 9

| | wave head | lane | ratio |
|---|---|---|---|
| settled footprint | 19,579,408 ± 802,804 B | 24,683,060 ± 1,679,372 B | **1.261** |
| settled growth per turn | 64,348 ± 4,260 B | 90,768 ± 7,701 B | **1.411** |
| workload wall | 70.7 ± 3.0 s | 72.5 ± 4.6 s | 1.025 |

Per interleave block (orchestrator's arithmetic on the same runs): base 22.47
and 18.78 MB (mean 20.62), lane 25.81 and 23.25 MB (mean 24.53) — ratio 1.288,
delta +3.91 MB. **This row fails the landing rule**, and it fails on the exact
workload the brief targets.

### 2.3 Compaction vs no compaction (earlier designs, load1m < 4)

| workload | wave head | lane |
|---|---|---|
| 200 turns, no compaction, N=3 | 20,808,208 ± 32,768 B | 20,824,616 ± 32,768 B |
| 200 turns, compacted every 50, N=3 | 20,677,184 ± 81,920 B | 23,527,976 ± 245,760 B |
| 50 turns, fresh session, N=3 | not measured | 17,859,088 ± 901,120 B |

Two things to read here. First, the lane reaches parity with the wave head when
compaction never runs (20.82 vs 20.81 MB) — the resident-turn window release is
free. Second, on the wave head itself, compacting every 50 turns produces
essentially the same settled footprint as not compacting at all (20.68 vs 20.81
MB): **compaction does not currently return the memory of the window it
discards, before this lane touches anything.** That is the defect the brief
named, and it is confirmed.

**PIN — a 200-turn session compacted every 50 turns settles like a fresh
50-turn session: FAILS.** 23.53 MB against 17.86 MB, MADs 0.25 and 0.90 MB. The
gap is 5.7 MB, far outside MAD.

### 2.4 Fleet probe — 100 sessions × 10 turns

| | value |
|---|---|
| settled footprint | 40,026,688 B (**40.03 MB**) |
| acceptance budget | 262,144,000 B (~250 MB) |
| measured turns | 1,000 |
| workload wall | 45.6 s |
| daemon idle before the fleet | 12,747,256 B |

**ACCEPTANCE — fleet ≤ ~250 MB: PASSES at 40.03 MB**, 15% of budget, with two
caveats that must travel with the number:

- the run exited non-zero, and the failure is the *idle* budget, not the fleet
  budget: idle measured 12.75 MB against the 6.02 MB calibrated ceiling. The
  cause is the harness, not the product — `HAIDER_TEST_FAKE_PROVIDER` carries a
  deterministic script with four steps per turn, so a 100×10 fleet ships a
  ~1,000-turn script into the daemon's environment before the first session
  exists. Pass an explicit `--idle-budget-bytes` for fleet runs;
- 45.6 s for 1,000 turns is consistent with the 55 ms/turn seen in the 40-turn
  runs, so the workload did execute. But the driver closes each session's client
  before opening the next, so the daemon may retire idle session actors as the
  fleet advances. **40.03 MB is therefore a floor for "100 sessions that have
  each run 10 turns", not a proven measurement of 100 simultaneously live
  sessions.** A fleet probe that holds all 100 connections open is the honest
  version and is not yet written.

### 2.5 Retention attribution — 120 turns, compacted every 40

This is the run the branch is being held for. `--retention-attribution` samples
the daemon's own `haider_retention` counters at every phase and the store's
structure at every turn.

Footprint behaviour across the three compaction boundaries:

| metric | value |
|---|---|
| return as % of growth since the previous cycle | **0.0%** |
| residual as % of pre-growth | **131.6%** (MAD 84.2 pp) |
| post-compaction footprint | 25,592,360 B (MAD 2,261,016 B) |
| post-compaction span across cycles | 9,863,168 B |
| settled footprint after 120 turns | 23,118,352 B |
| daemon idle before the workload | 6,128,096 B |

Compaction returns **zero percent** of the growth it should release, and the
footprint immediately after a compaction is *higher* than the growth that
preceded it.

What the tracked structures held at those same moments:

| phase | prompt cache | turn setup | observe | pipe item runs | budget runs |
|---|---|---|---|---|---|
| idle, head_seq 40 (turn 1) | 6 envelopes / 13,065 B envelopes / 13,224 B bodies / 1 prefix / 1 projection / 159 B projections | 1 | 0 B | 0 | 0 |
| compaction_committed, head_seq 1535 | 79 envelopes / 288,522 B / 343,880 B bodies | 1 | 26,606 B | 0 | 0 |
| compaction_committed, head_seq 3069 | 160 envelopes / 585,423 B / 641,015 B bodies | 1 | 33,779 B | 0 | 0 |
| compaction_committed, head_seq 4603 | 241 envelopes / 882,345 B / 938,018 B bodies | 1 | 47,039 B | 0 | 0 |
| swap_released, head_seq 4604 | **0 sessions, 0 envelopes, 0 bytes** | 0 | 0 B | 0 | 0 |

Store structure at turn 120: journal 3,018,542 B of JSON over 4,590 events,
SQLite file 6,696,960 B (1,659 pages, 7 free), WAL 4,210,672 B, projection
checkpoints 13,069 B.

**The conclusion.** At the last boundary the daemon's entire tracked in-memory
session state is under 1 MB — 938 KB of prompt-cache bodies plus 47 KB of
observe digests plus one turn-setup entry — while the footprint sits ~19.5 MB
above the 6.1 MB idle baseline. The generation swap drives every one of those
counters to literal zero, and the footprint does not move. **Under 5% of the
residual megabytes live in any structure this daemon tracks.** The remainder
reads as allocator high-water: large transient copies at the compaction boundary
(`covered_messages.clone()`, the second clone of the request messages, the
streamed summary `String`) dirty pages that `malloc_zone_pressure_relief` does
not give back, and the system allocator keeps the high-water for the process
lifetime.

That conclusion is inference from counters plus source inspection, and those two
together cannot prove it. **The evidence that would prove it** is a region-level
attribution across the boundary: a heap profile, or `vmmap` diffed
pre/post-compaction, assigning the residual pages to a zone. That measurement is
not in this report and is the first thing 972 should run.

One genuine live-byte defect *is* visible in the same table and should not be
lost in the allocator story: the prompt cache's envelope count grows straight
through compactions — 79, then 160, then 241 envelopes at successive compaction
commits, about 81 envelopes and 300 KB per 40 turns. Compaction does not shrink
the cached envelope set, because the post-compaction rebuild path replays the
journal from zero. That is memory following transcript age rather than the
window, exactly as the brief describes. It is worth ~0.94 MB at 120 turns —
real, and two orders of magnitude too small to explain the 19.5 MB.

---

## 3. What is on the branch

| file | change |
|---|---|
| `crates/haider-protocol/src/pipe.rs` | `TranscriptProjector::item_runs` is bounded to live runs: a run's item identity is released when the run reaches a terminal state, in `push` and in `prewarm`. It previously accumulated one entry per run for the life of the session, and on boot one per run in the whole journal. Adds `retained_item_run_count()` for the harness. |
| `crates/haider-daemon/src/worker.rs` | `durable_user_message_seqs` no longer rebuilds a set that follows transcript age: the fold is extracted as `fold_live_turn_nudge_seq`, and an `Idle` fact clears the window. The supervisor's in-memory `delivered_nudges` is cleared at the turn boundary. The compactor calls `release_compacted_session_state` after its journal batch and context-economy record are durable. |
| `crates/haider-daemon/src/session_hub/mod.rs` | `release_session_derived_state` with a `PromptCacheDisposition`; `release_compacted_session_state` at the compaction boundary; a `RESIDENT_TURN_WINDOW` (50) hard cut for sessions that never idle long enough for the 5-second timer; `sweep_dead_run_budget_coordinators`, which drops the dead `Weak` slot each finished run leaves in `run_budget_coordinators`; retention-trace counters for the pipe projector, resident window and budget slots. |
| `crates/haider-daemon/src/pipe_native.rs` | `retention_stats` exposing the reconciled sidecar's retained item-run count. |
| `crates/haider-client/examples/memdaemon_workload.rs` | `--sessions` and `--compact-every`; drives `session.compact` and waits for its terminal `Done` plus `Idle`; per-session command ids; a `compaction` phase event. |
| `scripts/perf/daemon-footprint-budget.py` | `--compact-every`, `--fleet-sessions`, `--compaction-settle-seconds`, `--fleet-budget-bytes`, `--max-load-1m`; pre/post-compaction checkpoints and a compaction-return summary; fleet checkpoints; schema `v2`. |
| `scripts/perf/tests/test_daemon_footprint_budget.py` | 3 unit tests over the harness's script generation and statistics. |
| `crates/haider-daemon/src/worker_live_window_tests.rs` | 3 tests, new file. |
| `crates/haider-daemon/src/session_hub_private_tests.rs` | 3 tests (resident-window cut, budget-slot sweep, swap arming). |

Three of these are unambiguously correct and measured free: the projector's
item-run set, the nudge dedupe window, and the budget-slot sweep. Each was an
allocation that followed transcript age; each is now bounded by the live window.
Together they are worth kilobytes, not megabytes — which is the lane's finding
in miniature.

---

## 4. The three designs, and why each failed

**D1 — drop the whole cached prompt generation at every release.**
40 turns: 281,703 B/turn against 219,035 (ratio 1.29). Removing the cache entry
resets its head, so the next compile reloads a durable prompt checkpoint and
then retains that checkpoint's prefix node-id and run-id sets for the whole
history — more than the evicted shell it replaced. Rejected.

**D2/D3 — trim to compiled prefixes at the compaction boundary and at the
resident-window cut.** 200 turns compacted every 50: 23,527,976 B against
20,677,184 (ratio 1.14 on footprint, 1.20 per turn). The boundary release runs
*before* the prompt cache has consumed the newly appended compaction node, so
the trim compiles and then retains a prefix for the exact epoch the compaction
is discarding, and pays a rebuild for state that is thrown away. Evicting at the
window cut was separately worse (23,233,076 against 20,808,208 over 200
uncompacted turns) because each cut forced a full replay that rebuilt every
envelope it had just dropped; trimming there instead reached parity. Rejected at
the boundary, kept at the window cut.

**D4 — leave the cache alone at the boundary, arm a generation swap for the next
release.** The idea was to let the continuation write the durable
post-compaction prompt checkpoint first, so the later swap seeds from the
summary plus retained suffix rather than replaying. It works mechanically: the
`swap_released` counters go to zero. It moved the footprint the wrong way —
ratio 1.288, +3.91 MB, worse than D2/D3's 1.20 / +2.85 MB. Rejected.

The pattern across all three is the finding: **every design traded one shape of
a sub-megabyte structure for another while the multi-megabyte residual sat
somewhere none of them could reach.**

---

## 5. Recommendation for 972

1. **Settle the allocator question first, before writing any code.** Diff
   `vmmap` or a heap profile across one compaction boundary and attribute the
   residual pages. If they are allocator high-water, no amount of dropping
   `Arc`s will move the number and the whole D1–D4 family is a dead end — which
   is what the counters already suggest.
2. **If it is high-water, the work is (a) eliminating the transient copies at
   the compaction boundary so the high-water never forms** — `covered_messages`
   is cloned, the request messages are cloned a second time, and the summary is
   streamed into a `String`, all live simultaneously with the pre-compaction
   prompt — **and (b) an explicit page-return step after those buffers drop.**
   Ordering matters: any relief call must run after the compactor's buffers have
   gone out of scope, and for automatic compaction after the actor has installed
   summary and suffix.
3. **If a live-byte fix is still wanted, it must run after the prompt cache has
   consumed the compaction node, never before.** Every release this lane placed
   at the boundary ran too early. The narrow real defect is that the
   post-compaction rebuild replays the journal from zero, so the cached envelope
   set grows through compactions (79 → 160 → 241 across three cuts); a fresh
   generation seeded from the summary plus retained suffix is the right shape,
   but it belongs inside `prompt_history.rs` next to the checkpoint machinery
   that already does exactly this, not in the session hub.
4. **Do not reuse the 40-turn uncompacted A/B as an acceptance gate for this
   work.** Nothing about the window fires there; it measured null for all three
   designs.

---

## 6. Gates

Run in the lane worktree with `RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 HAIDER_TEST_SIBLINGS_PREBUILT=1`,
`-j4`, siblings prebuilt, `df -g /` checked before each build.

| gate | result |
|---|---|
| `cargo test --workspace` | **exit 0** — 4,982 passed, 0 failed, 13 ignored, 326 suites |
| `cargo clippy --workspace --tests -- -D warnings` | **exit 0** |
| `xtask test-count --update` | baseline 4,603 → **4,611** (+8) |
| `python3 -m unittest test_daemon_footprint_budget` | 3 passed |

Nothing was weakened, `#[ignore]`d or platform-gated. The two failures found
along the way were both real and both fixed rather than suppressed: routing the
nudge dedupe set through the hub broke the structural pin
`worker_surface_is_structurally_lease_scoped` (and tripped clippy's argument
limit), so that plumbing was reverted and the set stayed supervisor-local.

---

## 7. Unverified, and limits of this evidence

- **The residual is attributed by inference, not by measurement.** Counters plus
  source reading point at allocator high-water; a region-level snapshot would
  settle it and was not run.
- **Automatic compaction is never exercised.** The driver sends
  `RequestBody::SessionCompact` and waits for its terminal `Done`, so every
  compaction number here is *manual idle* compaction. Automatic mid-turn
  soft-threshold compaction — the common case in production, and the one with
  the largest live transients — is untested by this harness.
- **The fleet probe does not hold its sessions open** (§2.4). 40.03 MB is a
  floor.
- **This machine drifts up to 30% under load.** The wave head's own 40-turn
  number moved 245,966 → 275,662 → 253,339 B/turn across three passes. Nothing
  short of a full ABBA is trustworthy here, and the final compaction block ran
  at `--max-load-1m 9` because a peer lane held the load between 4 and 7.
- **Windows and Linux behaviour is by inspection only.**
  `allocator_pressure_relief` is a macOS `malloc_zone_pressure_relief` and
  returns 0 elsewhere, so the page-return half of this lane is macOS-only by
  construction.
- **`fresh50` was measured on the lane binary only**; no wave-head arm was run
  for it, so the PIN comparison in §2.3 is lane-internal.
