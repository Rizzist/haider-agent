# Lane oneshotboot — v0.0.969

Date: 2026-09-02  
Worktree: `lane-969-oneshotboot`, changes intentionally uncommitted  
Retained product item: **R2-03 only**

## Verdict

**NO_SHIP.** The retained R2-03 change clears the cold and warm performance
thresholds, all behavior pins, the 47/47 SIGKILL matrix, and the cold 51.2 MiB
peak ceiling. Shipment is blocked by the mandatory memory protocol: the prior
N=3 result exceeded both daemon idle and per-turn retention ceilings, and the
continuation's fresh result is recorded below. Strict client-floor evidence is
also recorded below rather than being inferred from rejected diagnostics.

## Continuation inventory and method

The continuation began after the earlier ENOSPC crash with the worktree intact
and `target/` absent. The checked-in `LANE-BRIEF-oneshotboot.md` had been
overwritten by the continuation wrapper, so the original 36-line brief was
recovered from the prior session record at
`/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/969-oneshotboot-brief.md`. The untracked
lane brief/common and `turnperf/`/`turnperf2/` evidence copies remain uncommitted.

All Rust builds and tests used:

```text
RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
```

Daemon tests additionally used `HAIDER_TEST_SIBLINGS_PREBUILT=1` after sibling
prebuild. Disk was checked before builds and never approached the 700 MiB stop
threshold. The clean rebuild produced:

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| candidate `target/release/haider` | 34,665,408 | `99cec1cca7b7a9b40e85126a336a4f05bce87bc4fdbdf4dfb352fcbec8fbc069` |
| candidate `target/release/haiderd` | 52,341,120 | `64b0264c0f4f1670bde2a629cee9ed6327f7078b06fb9bb524b051a1b31cba26` |
| clean-HEAD `/private/tmp/oneshotboot-baseline/haider` | retained comparison binary | `da66a222268c9c96037049a5a204965d6ab30ee68ec92c4933fbec4bb81df446` |
| clean-HEAD `/private/tmp/oneshotboot-baseline/haiderd` | retained comparison binary | `cd28b11a202acac4ef9ce1d05ab2b6f9a84bc7804654784d72cb42c3dc2936c3` |

The candidate daemon is above registry #64's 10 MiB minimum. Clean-HEAD and
candidate A/B blocks used the same rebuilt source base, fake provider, trace-off
measurement mode, fresh profile/TTL=0 for cold runs, 5 warmups + 21 or 25
measured samples as required, and load1m strictly below 3. MAD is the median
absolute deviation. An invalid block is never pooled.

## Implementation retained

R2-03 changes the durable representation of the default SSH scope from an
explicit vault row to absence, which already decodes as `All`:

- default `All` is not written before or after session creation;
- non-default `None`/narrow scopes are still durable before session visibility;
- receipt and idempotent replay never reapply or cache create-time scope, so a
  later narrowing cannot be reopened, including after daemon restart;
- after a successful default create, the in-memory cache receives `All` with
  `or_insert`, so a concurrent/later narrowing cannot be overwritten;
- injected non-default vault failure proves no session, actor, response, or
  vault alias becomes visible.

This necessarily touches the retain lane's broad session-hub territory in
`session_hub/mod.rs` and `session_hub/rpc.rs`. The change is limited to SSH-scope
create/replay semantics and its pin; no worker-supervisor or retention-cache
behavior moved. The orchestrator must reconcile this overlap explicitly.

Proof infrastructure adds a real `--one-shot` harness mode, exact-profile
cleanup/PID and lock/socket checks, lifecycle trace stages, normalized 21-case
CPU and peak gates, and the shipped `t1.turn.one_shot_budget.py` guard. The
ordinary warm guard is tightened to the proposal's 56.7/78.0 ms ceilings.

## Ordered item-level hold-out table

Delta is candidate minus its paired baseline; negative is an improvement. Each
candidate was attempted alone in brief order and conservatively reverted when it
did not produce valid threshold-clearing evidence. A final hash audit found that
R2-09 and R2-18's nominal A/B files contain identical client and daemon hashes;
those timing differences are invalid diagnostics, not A/B evidence. R2-04,
R2-07, and the valid R2-10 raw JSON did not survive the ENOSPC cleanup. These
evidence defects are reported rather than reconstructed or treated as passes;
under the hold-out rule they remain null results and reverted.

| Family/item | cold wall delta | CPU-total/21 delta | warm delta | Decision |
| --- | ---: | ---: | ---: | --- |
| R2-04 process diagnostic key | -1.584 ms | +2.340 ms | no accepted warm win | **reverted**: misses 2 ms wall and 15 ms CPU thresholds |
| R2-07 provider registry persistence | -6.672 ms | -2.292 ms | zero stable regression/win | **reverted**: CPU gain is below 15 ms |
| R2-13 ephemeral runtime | about -0.02 ms after ABBA drift | about -16.69 ms | single +0.15 ms, tool -5.17 ms | **reverted**: null wall result and a small single-shape regression |
| R2-16 background runtime disposal | +3.166 ms | +2.672 ms | no accepted win | **reverted**: wall and CPU regress |
| R2-03 default SSH-scope writes | **-23.970 ms** | **-199.188 ms** | single **-18.775 ms**, tool **-19.266 ms** | **retained**: clears Family 1 thresholds with lower CPU |
| R2-18 known-zero head probes | invalid same-binary diagnostic: +1.169 ms | invalid: +12.101 ms | no accepted win | **reverted**: null evidence; not a valid A/B |
| R2-09 admission capsule | invalid same-binary diagnostic: +1.160 ms | invalid: +11.901 ms | no accepted win | **reverted**: null evidence; not a valid A/B |
| R2-05 attach/start pipeline | +5.640 ms | +221.098 ms | no accepted win | **reverted**; the pin preserves separate ordered requests/receipts |
| R2-10 lockdown/read-only overlap | invalid first block at load 3.52; valid retry missed threshold | no accepted improvement | no accepted win | **reverted**; invalid block excluded, exact valid artifact unavailable |

R2-13's surviving traced blocks are
`/private/tmp/r213-stage-{A1,B1,B2,A2}.json`; their wall medians are 97.732,
96.776, 93.846, and 92.935 ms, demonstrating time-order drift rather than a
candidate effect. Surviving R2-03 final blocks are
`/private/tmp/final4-{A1,B1,B2,A2}.json` and
`/private/tmp/final4-warm-{A1,B1,B2,A2}.json`.

No optional X1-8, X1-5, or D1-3 item received stable >=1 ms attribution, so none
was implemented. No proposal-rejected item or CAS barrier removal was attempted.
The missing/invalid rejected-item artifacts mean the lane does not satisfy the
brief's complete per-item evidence requirement even apart from the memory gate.

## Fresh cumulative cold A/B

The continuation reran clean-HEAD/candidate ABBA from the clean release rebuild.
All four admitted blocks used 5 warmups + 21 samples and load below 3.

| Block | Build | wall median ± MAD | CPU total normalized to 21 | peak RSS | load1m range |
| --- | --- | ---: | ---: | ---: | --- |
| A1 | clean HEAD | 105.188 ± 2.720 ms | 752.230 ms | 39,600 KiB | 2.703–2.727 |
| B1 | R2-03 | 86.802 ± 3.928 ms | 691.450 ms | 39,424 KiB | 2.668 |
| B2 | R2-03 | 79.957 ± 1.514 ms | 693.820 ms | 39,472 KiB | 2.896 |
| A2 | clean HEAD | 103.497 ± 3.412 ms | 737.518 ms | 39,584 KiB | 2.611–2.722 |

Pooled clean wall is 104.343 ms versus candidate 83.380 ms, a **20.963 ms
improvement**. Pooled normalized CPU is 744.874 ms versus 692.635 ms, a
**52.239 ms improvement**. A B2 attempt whose final load was 3.09 was rejected
and replaced; it is not present in these results. Candidate wall, CPU, and cold
peak clear 124 ms, 1,059 ms, and 51.2 MiB respectively.

## Fresh cumulative warm A/B

Each block used 5 warmups + 25 samples per shape, trace off, load below 3.

| Block | Build | single wall ± MAD | tool wall ± MAD | single/tool CPU | combined peak single/tool |
| --- | --- | ---: | ---: | ---: | ---: |
| A1 | clean HEAD | 58.916 ± 1.792 ms | 79.100 ± 4.166 ms | 4.865 / 5.739 ms | recorded in JSON |
| B1 | R2-03 | 63.296 ± 10.302 ms | 92.156 ± 13.857 ms | 4.761 / 5.623 ms | recorded in JSON |
| B2 | R2-03 | 39.686 ± 2.510 ms | 59.398 ± 2.676 ms | 4.407 / 5.203 ms | recorded in JSON |
| A2 | clean HEAD | 83.160 ± 8.913 ms | 115.515 ± 30.013 ms | 5.082 / 6.011 ms | recorded in JSON |

The shared host drifted substantially across the sequence, but pooled candidate
still improves single by **19.547 ms** and tool by **21.531 ms**, with lower CPU.
A separate final candidate guard at load 2.233 passed both strict budgets:
38.400 ± 2.666 ms single and 57.469 ± 2.751 ms tool. Its sampled combined peaks
were 54,736/55,664 KiB; this warm-harness value is disclosed separately from the
brief's cold one-shot peak measurement and is not used to conceal the memory
protocol blocker.

Artifacts: `/private/tmp/oneshotboot-resume/warm-{A1,B1,B2,A2}.json` and
`/private/tmp/oneshotboot-resume/warm-final.json`.

## Lifecycle trace and teardown proof

The final trace-on candidate run was correctness-clean and passed all cold gates
at load 2.325:

| Measurement | Median ± MAD |
| --- | ---: |
| complete one-shot wall | 83.190 ± 3.253 ms |
| spawn -> Ready | 33.263 ± 1.121 ms |
| Ready -> client Accepted seen | 0.721 ± 0.048 ms |
| Accepted -> terminal | 35.460 ± 2.567 ms |
| terminal -> process exit | 6.944 ± 0.342 ms |

Normalized CPU was 620.184 ms/21 and peak was 39,424 KiB. Every case proved the
observed daemon PID dead, `lock.owner` absent, the real profile lock acquirable,
all Unix sockets removed, and `status --no-spawn` returning the expected 69.
Artifact: `/private/tmp/oneshotboot-resume/cold-trace-final.json`.

An independent review then found that the first cache hardening could mask a
durable narrowing on create-receipt replay after daemon restart. The product fix
now caches implicit `All` only for `SessionCreateOutcome::Committed`, never for
receipt or idempotent replay, and the R2-03 pin performs create -> narrow ->
restart -> receipt replay. After rebuilding release, a fresh admissible 5+21
trace at load 2.845 passed again: **108.158 ± 5.766 ms** wall, **877.143 ms/21**
CPU, **39,616 KiB** peak, with stages spawn->Ready **46.511 ± 2.888 ms**,
Ready->accept **1.090 ± 0.100 ms**, accept->terminal **41.979 ± 3.277 ms**, and
terminal->exit **12.043 ± 0.743 ms**. Artifact:
`/private/tmp/oneshotboot-resume/cold-trace-after-replay-fix-valid.json`.

The rebuilt release also passed the strict warm guard at load 2.574: single
**43.311 ± 3.336 ms** and tool **67.361 ± 10.585 ms**. Artifact:
`/private/tmp/oneshotboot-resume/warm-after-replay-fix.json`.

## SIGKILL recovery matrix

The rebuilt post-replay-fix release matrix passed **47/47**, with zero failed cases and zero
duplicate physical provider requests for any logical request ordinal. The
provider ledger contained 55 entries, and grouping by case/logical ordinal had a
maximum multiplicity of one. Artifact:
`/private/tmp/oneshotboot-resume/sigkill-after-replay-fix.json`.

## Footprint and client floors

The continuation completed a fresh N=3 release-daemon run with 60-second idle
and post-turn settle windows, 40 turns, and retention attribution. Attempts 1
and 4 were rejected because load crossed 4; attempts 2, 3, and 5 were admitted.

| Gate | Fresh median ± MAD | Required ceiling | Result |
| --- | ---: | ---: | --- |
| settled idle | **5,538,248 ± 0 B** | 5,420,000 B | **fail by 118,248 B** |
| retention | **310,478 ± 12,288 B/turn** | 195,584 B/turn (191 KiB) | **fail by 114,894 B/turn** |
| post-40-turns | **17,957,368 ± 491,520 B** | 13,243,360 B | **fail by 4,714,008 B** |

Accepted per-run `(idle, post, bytes/turn)` values were
`(5,538,248, 18,448,888, 322,766.0)`,
`(5,521,888, 16,204,304, 267,060.4)`, and
`(5,538,248, 17,957,368, 310,478.0)` B. Their maximum load1m values were 3.934,
3.810, and 3.968. The fresh artifact is
`/private/tmp/oneshotboot-resume/footprint-final.json`; the command correctly
exited 1 for the failed budgets.

The later replay fix only removes cache publication from receipt/idempotent
replay. The footprint workload uses unique fresh commits and never executes that
branch, so rerunning the already-failing six-minute N=3 protocol would measure
the same product paths; the failed accepted result is retained and this scope is
stated explicitly.

The strict release-client guards were then run serially. Each completed one
valid low-load sample before refusing to continue because this sandbox denies
`vmmap -summary` with exit 255:

| Surface | rejected diagnostic sample | configured upper guard | Formal result |
| --- | ---: | ---: | --- |
| `status --json --no-spawn` | 2,408,832 B; 107,085 us; 1 thread; load 3.20/3.54 | 2,703,783 B | **environment-rejected: vmmap 255** |
| completed headless `run` | 3,195,288 B; 163,752 us; 1 thread; load 3.50/3.18 | 3,406,683 B | **environment-rejected: vmmap 255** |

The counters are consistent with the requested 2.4/3.0 MB wire floors and below
their standing guards, but they are not accepted N=5 evidence. The harness
correctly aborts instead of treating `proc_pid_rusage` as a substitute for the
required `vmmap` result. Diagnostics are in
`/private/tmp/oneshotboot-resume/client-status-final-2/run-1/` and
`/private/tmp/oneshotboot-resume/client-run-final-2/run-1/`. Its Darwin binding
self-test passed with positive footprint and one thread.

For comparison only, the pre-crash N=3 result was already non-mergeable:

| Gate | Prior median | Required ceiling | Result |
| --- | ---: | ---: | --- |
| settled idle | 5,439,944 B | 5,420,000 B | **fail by 19,944 B** |
| retention | 242,483.8 B/turn | 195,584 B/turn (191 KiB) | **fail by 46,899.8 B/turn** |
| post-40-turns | 15,155,704 B | 13,243,360 B | **fail** |

That artifact is `/private/tmp/oneshotboot-final4-memory.json`. Its three idle
samples were 5,439,944, 5,423,584, and 5,456,352 B; retention samples were
276,890.8, 163,022.0, and 242,483.8 B/turn.

## Verification battery

| Check | Result |
| --- | --- |
| R2-03 exact daemon pin, including restart replay | 1 passed after final fix |
| R2-18 exact daemon pin | 1 passed |
| R2-05 exact client pin | 1 passed |
| R2-09 exact store pin | 1 passed |
| orchestrator `oneshot_boot_tests` | 10 passed |
| orchestrator `core_loop_e2e_tests` | 20 passed |
| `bash scripts/qa-gate/run.sh test` | **53/53 passed** after registry update |
| affected-crate scoped clippy with `-D warnings` | passed |
| `cargo run -q -p xtask -- test-count` | **4367 / baseline 4367** |
| `git diff --check` and Python compile | passed |
| rebuilt-release `core_loop_e2e_tests` | 20/20 passed after final fix |
| post-fix rebuilt-release cold/warm guards | passed |
| post-fix rebuilt-release SIGKILL matrix | 47/47, zero duplicates |
| `cargo fmt --all -- --check` | passed |

No test was weakened, ignored, or platform-gated. The four new pins cover both
the retained fast path and reverted candidates' semantic boundaries. The R2-03
pin includes default absence/cache/replay narrowing, explicit `None`, and injected
vault-failure non-visibility. OAuth files are untouched. Windows/Linux behavior
of the Rust changes is portable by inspection; the performance protocols are
Darwin-only because they use Darwin process accounting.

## CI guards and registry walk

- #64: release `haiderd` is 52,341,120 B, above 10 MiB.
- #71/#72/#74: clean-HEAD and candidate release binaries were both exercised;
  discovery was disabled only for hermetic profiles; every performance profile
  is throwaway and exact-profile cleanup is verified.
- #77: Python registry and harness tests run before final Rust acceptance.
- #94: no product deadline was added. Existing harness teardown deadlines are
  documented as bounded proof timeouts; the stale unrelated R2-13 comment was
  removed.
- #95: no new wait on external state while a negotiated connection is open was
  added.
- The new shipped cold guard enforces 124 ms wall, 1,059 ms normalized CPU, and
  51.2 MiB peak. The warm guard enforces 56.7/78.0 ms rather than the previous
  10% slack.

## Citation audit

All proposal links pointed into the old `wt-965` and were searched by construct
before implementation. Only current constructs were used for decisions:

| Proposal citation | Audit against this tree |
| --- | --- |
| R2-03 omitted scope/default and create writes | **correct construct, drifted lines**: request default is around `rpc.rs:14980`; retained replay/create logic is now `rpc.rs:15026` and `rpc.rs:15302`; absent means `All` remains `ssh/store.rs:297` |
| R2-03 vault publication | **correct construct, incomplete old span**: `file_vault.rs:103-113` covers file and directory durability, not line 103 alone |
| R2-04 diagnostic key | **correct construct** at `session_hub/mod.rs:338-387` and construction around `2349`; candidate was reverted |
| R2-07 provider registry | **correct construct, drifted lines**: store save path is `provider_registry.rs:227-302`; bootstrap reconciliation around `439-484`; candidate was reverted |
| R2-13 runtimes | **correct construct, drifted lines**: daemon runtime is `haider-daemond/src/main.rs:122`; usage timer runtime is `session_hub/mod.rs:236-245`; candidate was reverted |
| R2-16 runtime disposal | **correct construct, drifted lines**: CLI shutdown sites begin around `main.rs:178`; daemon finalization remains in `haider-daemon/src/runtime.rs`; candidate was reverted |
| R2-05 ordered frames | **correct construct, drifted lines**: `begin_request` is `client.rs:776`; attach is `rpc.rs:16518`; the semantic pin proves separate receipts after candidate revert |
| R2-18 fresh head probes | **correct construct, drifted lines**: actor creation query is `session_hub/mod.rs:6311`; attach probe is `rpc.rs:16526`; candidate was reverted |
| R2-09 admission rereads | **correct ownership, heavily drifted worker lines**: acceptance/store and supervisor constructs were found by symbol; candidate was reverted before touching retain-owned worker code |
| R2-10 lockdown ordering | **correct construct**: `lockdown/mod.rs:263` bind and `:364` activate remain separate; candidate was reverted |

The overwrite of `LANE-BRIEF-oneshotboot.md` itself is an evidence defect, not a
product change; the original recovered brief was used and the supplied lane
copies remain uncommitted as instructed.

## Final hygiene and independent review

The worktree is intentionally left uncommitted. No lane evidence copy is staged
or committed. Final `cargo fmt --all -- --check`, `git diff --check`, Python
compile, and protected OAuth-file checks pass. The independent verifier confirmed
the artifact arithmetic, binary hashes, harness fixes, protected-file boundary,
and mandatory memory failure. It also found the restart replay defect and the
invalid R2-09/R2-18 same-binary measurements; both are now handled explicitly:
the product defect has a green restart pin and rebuilt-release matrix, while the
invalid measurements remain null hold-outs and an additional evidence blocker.
Independent verdict: **NO_SHIP**.

NO_SHIP
