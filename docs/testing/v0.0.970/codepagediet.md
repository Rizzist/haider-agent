# v0.0.970 codepagediet report

## Scope and evidence audit

The measured source is `0fed562` (`wave-970`).  `LANE-COMMON.md` names
`8952219` as its base, so that citation has drifted.  The only product delta
from the v0.0.969 tag is the client-footprint calibration merge; the daemon
source is unchanged.  The requested root `run.sh` does not exist: the current
QA entry point is `bash scripts/qa-gate/run.sh test`.

There is no PGO branch, profile, order file, or release wiring in any local
ref/worktree.  The separate `lane-970-peakrss2` worktree has a completed but
uncommitted `SharedText` experiment, so its result is coordination evidence,
not code present in this tree.  Its accepted A-only measurement lowered the
pooled large-reply daemon maximum by 2,768,896 bytes; its compact-node B was
rejected after increasing client peak by 1,179,648 bytes.

The 969 memory evidence distinguishes physical footprint (private heap and
committed stack pages) from RSS (which additionally counts resident
file-backed image pages).  Its settled final release rows were 5,472,736-byte
footprint versus 17,022,976-byte RSS at idle and 16,515,600-byte footprint
versus 41,926,656-byte RSS after 40 turns.  It never obtained region detail.
The exact M1 allocator A/B measured a 49,840,128-byte system-allocator median
(MAD 1,146,880 bytes) and rejected mimalloc at 57,786,368 bytes.

## Step 0: exact release baseline

The exact current release daemon was built with the repository release profile
and the lane environment.  Artifact identity:

| artifact | SHA-256 | bytes |
|---|---|---:|
| `haiderd` from `0fed562` | `777c6b90f4bf487bb197ca15f7298c61582d80d1581125bac3e41b46da4d061a` | 52,341,120 |
| installed v0.0.969 `haider` measurement client | `968ce4c52a094566e8cee8b720ffddb574216f263a0420d9a0d1aab460ed8c50` | 34,700,288 |

The Mach-O section accounting is:

| segment/section | bytes |
|---|---:|
| `__TEXT` | 50,397,184 |
| `__text` | 41,960,380 |
| `__gcc_except_tab` | 1,971,232 |
| `__unwind_info` | 580,832 |
| `__eh_frame` | 3,433,848 |
| `__DATA_CONST` | 1,146,880 |
| `__DATA` | 262,144 |

`vmmap -summary`, full `vmmap`, `sample`, and `footprint` all fail on this
managed runner because `task_for_pid` is denied, including for a same-user
child.  The lane therefore adds an auditable fallback using
`PROC_PIDREGIONPATHINFO`, which exposes mapped path, protection, VM tag, share
mode, and resident/private/shared/dirty page counters without `task_for_pid`.
The M1 sampler preserves a validated snapshot at every RSS high and at a
10 ms refresh cadence above the configured threshold.  Run analysis then
requires and names the helper-observed maximum whose trigger and capture
timestamps both fall inside the item-to-item+30 ms window.  It records sampler
trigger RSS, helper RSS/footprint, and their delta explicitly; the helper can
observe growth while its scan blocks the 1 ms sampler.  Helper failure is
fail-once, not a per-sample retry loop.  A separate uninstrumented N=5 run
remains the performance authority.

The exact probe is `patch_truncated`: 1,114,112 assistant bytes, two provider
requests, a 3,374,303 +/- 65,536-byte JSONL stream, exact large delta/completed
item/done anchors, and the `value.txt == "fixed\n"` tool-effect pin.  This lane
tightens the probe's old load `<8` gate to the acceptance load `<3` and carries
the required test device and stack environment into its `env -i` child.

The uninstrumented performance pass was accepted at load 2.53 for all five
runs:

| run | pre-reply RSS | item RSS | 30 ms peak RSS |
|---:|---:|---:|---:|
| 1 | 32,636,928 | 48,906,240 | 48,906,240 |
| 2 | 31,571,968 | 48,005,120 | 52,838,400 |
| 3 | 33,488,896 | 46,465,024 | 46,465,024 |
| 4 | 31,571,968 | 47,775,744 | **53,805,056** |
| 5 | 33,882,112 | 45,023,232 | 49,184,768 |
| median | 32,636,928 | 47,775,744 | 49,184,768 |

The maximum is 51.31 MiB, reproducing the canonical 51.2 MiB within sampler
resolution/run variance.  Every run saw exactly two requests and the exact
large reply/tool/terminal anchors; JSONL output was 3,382,281 bytes.

The refreshed region diagnostic obtained an exact paired peak in run 4 at
load 2.50 before and after.  Its sampler trigger was 53,706,752 bytes at
`1788406693008055000` ns and the helper observed the same RSS at
`1788406693014620000` ns, 6.565 ms later; both are inside the item-to-item+30 ms
window.  The selected raw file is `daemon-regions-peak.tsv`, named in that
run's `summary.json`.  This yields the additive high-level accounting:

| peak RSS accounting | bytes | MiB |
|---|---:|---:|
| physical footprint (heap, committed stacks, private dirty state) | 31,506,984 | 30.05 |
| RSS above footprint (file-backed/shared pages) | 22,199,768 | 21.17 |
| **process RSS** | **53,706,752** | **51.22** |

The requested category view is:

| category | bytes | accounting status |
|---|---:|---|
| resident daemon code/const pages (`__TEXT`) | 27,000,832 | raw libproc region counter; overlaps the net file-backed/shared row below |
| heap peak (malloc VM tags 1/2/3/7) | 28,295,168 | private resident, inside footprint |
| full reply copies alive | at least 4,456,448 logical | source-derived subset of heap ownership |
| committed stacks | 933,888 private; 999,424 raw | inside footprint; 8 MiB virtual reservations excluded |
| SQLite | 32,768 | private SHM mapping; database itself not mmap-resident |
| everything else in footprint | 2,245,160 | arithmetic residual after heap/private stacks/SQLite |
| net file-backed/shared RSS overhead | 22,199,768 | additive `RSS - footprint`, includes code and other images |

Raw region counters answer the separate code-page question but are not
additive RSS components: main-daemon `__TEXT` reports 27,000,832 resident
bytes; address-bounded `__DATA_CONST`/`__DATA`/`__LINKEDIT` reports 1,327,104;
stack tag 30 reports 999,424; and all raw resident fields total 145,866,752.
Shared-cache views explain why that raw total exceeds process RSS.  The
source-audited reply copies are at least 4,456,448 logical bytes (four x
1,114,112); they are a subset of heap ownership, not a measured resident
subcategory.

## Static resident-text ownership proxy

`cargo-bloat` 0.12.1 was installed for the requested audit, but its Cargo
subprocess cannot populate the read-only shared Cargo cache in this managed
runner.  The exact build's matching 223 MB dSYM was therefore analyzed by
address-sorted `nm`, inferred next-symbol sizes, and `rustfilt`.  The inferred
symbols cover 41,970,868 of the 41,960,380 `__text` bytes (the small overage is
alignment/section-boundary inference).  This is a static size proxy, not a
claim that every function is resident.

| owner group | inferred text bytes |
|---|---:|
| `std`/`core`/`alloc` | 11,555,304 |
| `haider-daemon` | 6,773,644 |
| serde/JSON/MessagePack monomorphizations | 5,237,472 |
| `haider-protocol` | 3,556,492 |
| Tokio/futures/mio | 2,619,876 |
| `haider-core` | 1,187,424 |
| `haider-rpc` | 1,151,512 |
| `haider-store` | 1,144,828 |
| `haider-provider` | 838,268 |
| AWS-LC | 731,632 |
| SQLite/rusqlite | 691,176 |
| russh/SSH | 679,824 |
| rustls/webpki | 405,652 |
| image/PDF stack | 422,972 |

The largest individual functions are the turn actor (210,028 bytes), session
actor (152,964), hub request dispatcher (134,312), daemon `start_turn`
(115,128), broker tool dispatcher (96,476), account actor (88,168), daemon
runtime (82,060), supervisor (71,396), connection server (69,708), and tool
registry construction (67,644).  The top-60 proxy also contains cold-path
recovery, delegation, hooks, OAuth, PDF/image, SSH, and AWS-LC functions.

## Reachability arithmetic

The binary-unit 35 MiB target is 36,700,160 bytes.  In the exact paired region
row, reaching it while holding footprint and other mappings fixed requires a
17,006,592-byte (16.22 MiB) reduction.  Against 27,000,832 raw resident
main-image `__TEXT` bytes, only 9,994,240 bytes (9.53 MiB) may remain:
**63.0% of those pages must disappear**.  The separate uninstrumented N=5
authority has a slightly higher 53,805,056-byte maximum, requiring a
17,104,896-byte (16.31 MiB) total RSS reduction.

The target is therefore arithmetically possible but not credible from a small
crate diet: even removing every measured AWS-LC, SQLite, russh, rustls, and
image/PDF text byte from the resident set would save only about 2.80 MiB.  A
layout lever has to prevent most cold code from faulting in, or a separate
heap/reply-copy change must lower the non-code floor.

## Live reply-copy audit

On the exact terminal path, `commit_post_stream_facts` keeps the completed
`TextAccumulator`, clones its text into the tree node, clones it into the
completed item, and materializes both payloads as separate JSON values.  That
leaves at least four simultaneous full 1,114,112-byte logical copies
(4,456,448 bytes total) before store append/fanout.  The store then builds a
separate MessagePack `encode_envelope` buffer; it is serialized reply data, not
necessarily a fifth `String`.  SQLite, allocator slack, and unrelated daemon
state account for the balance of the malloc-tagged working set.  This is an
ownership audit, not a heap-profiler claim.  The coordinated `peakrss2`
SharedText result is intentionally not duplicated here because it has already
measured this lever and has not landed.

## Lever decisions

### PGO hot/cold layout A/B

No profile, order file, PGO branch, or release-path wiring exists in the local
refs/worktrees.  An isolated daemon-only experiment therefore used rustc LLVM
22.1.3 and `llvm-profdata` 22.1.8.  The generate build trained on the complete
warm single/tool ABBA corpus and complete TTL=0 one-shot corpus.  Both had
empty correctness-failure ledgers; their relaxed load pin was training-only,
not accepted timing evidence.  The merged profile covers 65,413 functions,
1,708,688 instrumented blocks, and 475,577,056 total counter hits.
The initial warm JSON was created at `2026-09-03T02:49:50Z`, the one-shot JSON
at `02:50:17Z`, and the profile merged at `02:50:30Z`, before the candidate
build.  A later `03:24:55Z` warm rerun only persisted independently parseable
evidence (10 warmups, 25 single, 25 tool); it did not influence the candidate.

The profile-use artifact is
`bc5b153b4f7cec27521a342044bb5e4e7f6297a8feba4769960d537c703b150e`.
It is 45,373,872 bytes versus 52,341,120 baseline (-6,967,248 bytes,
-13.3%).  Static `__TEXT` is 43,499,520 versus 50,397,184 bytes, while
`__text` is 36,590,732 versus 41,960,380 bytes.  These are encouraging size
results, not an RSS acceptance result; the hold-out rows below decide whether
the lever is retained.

The uninstrumented exact-M1 hold-out used A1/B1/B2/A2 ordering.  All 20 runs
had load below 3 at every available gate, two provider requests, the exact
large reply, durable tool effect, and terminal anchors.  A1 predates the
post-run gate and records per-run pre-load only; B1/B2/A2 record both pre and
post load:

| arm | N | load range | peak median bytes | peak max bytes |
|---|---:|---:|---:|---:|
| A1 baseline | 5 | 2.53 | 49,184,768 | 53,805,056 |
| B1 PGO | 5 | 2.63-2.69 | 46,284,800 | 47,611,904 |
| B2 PGO | 5 | 2.70-2.96 | 47,988,736 | 48,529,408 |
| A2 baseline | 5 | 2.36 | 53,493,760 | 53,723,136 |
| pooled A | 10 | 2.36-2.53 | 52,477,952 | **53,805,056** |
| pooled B | 10 | 2.63-2.96 | 46,325,760 | **48,529,408** |

PGO therefore lowers the pooled median by 6,152,192 bytes (11.7%) and the
hold-out maximum by 5,275,648 bytes (9.8%).  Its maximum is still 11,829,248
bytes (11.28 MiB) above 35 MiB.  Layout alone does not make the owner target
reachable in this build.

The independent warm hold-out used 10 warmups and 25 measured turns per shape
in each pass.  All four passes were accepted at their start/mid/end load gates
and had empty correctness-failure ledgers.  Pooling the two A passes and two B
passes without trimming gives:

| shape/arm | N | wall median (MAD), ms | CPU median (MAD), ms | daemon peak, KiB |
|---|---:|---:|---:|---:|
| single A | 50 | 40.828 (3.132) | 5.083 (0.674) | 44,496 |
| single B | 50 | 48.422 (8.435) | 5.923 (1.214) | 37,088 |
| tool A | 50 | 60.730 (4.585) | 6.161 (0.936) | 44,496 |
| tool B | 50 | 75.204 (12.268) | 7.200 (1.756) | 37,088 |

The PGO warm result regresses single wall by 7.594 ms, 2.42 baseline MADs,
and tool wall by 14.474 ms, 3.16 baseline MADs.  Median CPU regresses 16.5%
and 16.9%, respectively.  This violates both the within-MAD wall criterion and
the 2% CPU ceiling.

The conformance one-shot cross-check used five warmups plus 21 measured
processes per arm.  Both arms were accepted, with exact request, terminal, and
lifecycle pins and empty correctness-failure ledgers:

| arm | load start/mid/end | wall median (MAD), ms | CPU total/21, ms | daemon peak, KiB |
|---|---:|---:|---:|---:|
| A baseline | 2.496/2.496/2.496 | 85.371 (4.094) | 648.063 | 28,416 |
| B PGO | 2.459/2.459/2.459 | 99.525 (4.155) | 1,059.883 | 22,128 |

Although this small-reply lifecycle probe also sees the code-page RSS win, PGO
regresses its wall median by 14.154 ms (16.6%, 3.46 baseline MADs) and total CPU
by 63.5%.  The candidate is therefore rejected.  No PGO flags, profile,
artifact, or release-path wiring are retained in the product build.

- `panic=abort` is rejected without an A/B: the daemon catches supervisor
  panics and converts them into durable terminal outcomes.
- Mimalloc is rejected by the 969 measured regressions (+15.9% M1 peak, +17%
  turn CPU, +35.6% idle footprint, +138.3% post-40 footprint).
- SQLite already uses a 512 KiB page-cache ceiling.  Lowering it has too small
  a ceiling to bridge the resident-code target and remains measurement-only.
- Smaller stacks are not a free lever: a mutation test deliberately touches
  3 MiB on a daemon worker and pins explicit 8 MiB headroom.  Only the TTL=0
  two-worker experiment is behavior-plausible.
- Ring-only rustls could remove the second TLS implementation, but reqwest's
  `rustls-no-provider` mode requires process-wide provider installation and
  loses AWS-LC's post-quantum preference.  It needs real HTTPS compatibility
  evidence; the local HTTP warm harness cannot establish parity.

## Verification and CI registry

No tests have been weakened, ignored, or platform-gated.  The authoritative
registry #44 is the real UDS daemon/TUI probe and is unchanged; it does not
grant `task_for_pid`, so the denied `vmmap` attempt is reported as an
environmental limitation.  The libproc fallback records exact daemon PID and
raw counters.  Registry #64 is satisfied by both the 52,341,120-byte baseline
and 45,373,872-byte PGO candidate.  No schema, provider-authority, OAuth,
Windows wire, or durability boundary has been changed.

The maintained QA entry point, `bash scripts/qa-gate/run.sh test`, passes all
64 tests.  The user-named root `bash run.sh test` cannot be executed because
this branch has no root `run.sh`.  The M1 sampler self-test, Python bytecode
compile, shell syntax check, and `clang -Wall -Wextra -Werror` build of the
Darwin region helper also pass.  Four focused named tests additionally pin the
exact-PID/schema/all-row validator, rejection of an empty exit-zero helper,
bounded nonzero/missing-helper failure, and timeout cleanup.

The current CI footprint budgets remain 6,020,010 bytes idle and 18,167,160
bytes post-40.  They are not changed: no product or release-profile lever is
retained, so the shipped daemon is byte-identical to the measured baseline.
Updating the ceiling from the isolated PGO artifact would make CI certify a
configuration this branch does not ship.  The footprint budget is therefore
not regressed, and the delta registry walk records this measurement-only
surface explicitly.

Principal evidence artifacts are `/tmp/codepagediet-m1-baseline-clean`,
`/tmp/codepagediet-m1-baseline-a2`,
`/tmp/codepagediet-m1-baseline-regions-v2/20260903T033812Z-run4`,
`/tmp/codepagediet-m1-pgo-b1`, `/tmp/codepagediet-m1-pgo-b2`, and
`/tmp/codepagediet-holdout`.  The static ownership inputs are
`/tmp/codepagediet-symbol-sizes.tsv` and the exact matching baseline dSYM.

## Verdict

The 35 MiB owner target is missed by 11.28 MiB after the only lever with a
material measured RSS win, and that lever independently violates both warm and
one-shot wall/CPU hold-outs.  The product and release profile therefore remain
unchanged; only the fail-closed peak-region measurement tooling, its tests,
the CI registry walk, and this report are retained.

NO_SHIP
