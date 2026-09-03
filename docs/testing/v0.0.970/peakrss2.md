# peakrss2 lane report (v0.0.970)

## Verdict

**NO_SHIP after continuation verification.** Retain lever A as the better
candidate and keep lever B rejected, but do not ship the present build: a fresh
N=5 A-only cohort from the rebuilt binaries recorded a valid daemon maximum of
54,558,720 bytes, 871,629 bytes (0.831 MiB, 1.624%) above the strict 51.2 MiB
(53,687,091-byte) ceiling. The run used the exact fixture, had load1m 2.46,
recorded exactly two provider requests, and placed the peak in the required
post-item terminal window, so it cannot be discarded as overload, PID drift, or
a sampling-window miss.

Lever A removes simultaneous owned copies of a completed assistant reply by
sharing its immutable text between the streaming accumulator, completed item,
and assistant history node. Lever B, a compact JSONL assistant-node reference,
reduced output size but regressed the measured client peak; all B
product/protocol/contract code was therefore removed. Wire JSON and MessagePack
remain legacy string scalars. The remaining compatibility caveat is Rust source
compatibility: public `TurnItem::AgentMessage.text` and
`NodeKind::AssistantCommit.text` fields now use `SharedText` rather than
`String`; callers constructing those variants must use `.into()`.

## Base, inventory, and merge-forward state

- On continuation, `HEAD` and the then-current `origin/wave-970` both resolved
  to the user-pinned `e3fc3f552088a1b8a3d8127df822fee6c0261520`; the requested
  merge-forward was therefore a no-op.
- During final verification another lane advanced the shared local ref to
  `0fed5620459f6081cd992e60f6c098d082fb4d91`. Its two intervening commits touch
  the ship-gate workflow, client-footprint harness/tests, and shared evidence,
  not this lane's product surface. A normal fast-forward could not write the
  linked worktree's `ORIG_HEAD.lock`, which is outside this managed workspace.
  The exact upstream versions of every affected tracked file were therefore
  applied to the writable worktree and compared byte-for-byte with
  `origin/wave-970`; the branch ancestry still reports two commits behind.
- Continuation inventory: 41 tracked files modified (843 insertions, 161
  deletions),
  plus this untracked evidence directory and
  `scripts/perf/m1-client-rusage.py`. The work is deliberately uncommitted.

## Retained implementation: lever A

- `SharedText` is an `Arc<String>` leaf with copy-on-write mutation and custom
  serde that encodes/decodes exactly the legacy string scalar:
  `crates/haider-protocol/src/item.rs:30-138`.
- Completed agent items and assistant history nodes use it at
  `crates/haider-protocol/src/item.rs:186-193` and
  `crates/haider-protocol/src/history.rs:40-43`.
- The provider stream grows one unique `SharedText` allocation
  (`crates/haider-core/src/actor.rs:6235-6279`). Ordinary item completion clones
  only the Arc (`:7171-7200`). The no-boundary terminal path creates usage,
  completed item, assistant node, and Done as one ordered append while sharing
  the text (`:9908-9998`). The headless client consumes ownership when producing
  its final response (`crates/haider-client/src/headless.rs:1791-1795`).
- State is cleared only after a successful append. An independent audit found
  that the earlier pre-append `take()` formulation could lose the open item,
  reasoning, and usage on store rejection. It was corrected to borrow/clone
  before append and clear after success. Mutation tests at
  `crates/haider-core/src/actor_request_attempt_tests.rs:817-950` prove both
  error cleanup and preservation of every pending input.

The A change is allocation behavior only: durable event order, event bytes,
prompt history, terminal behavior, and replay authority remain unchanged.

## Lever B decision

The experiment replaced the duplicate assistant-node text in JSONL with a
reference to the completed item. It reduced median JSONL by 1,114,121 bytes
(32.939%) versus A, but increased the observed client lifetime maximum by
1,179,648 bytes (7.066%, 1.125 MiB). That violates the no-regression rule, so B
is rejected. No compact-node feature token, schema field, serializer,
rehydrator, contract text, or fixture remains. The M1 harness intentionally
retains projection classifiers so historical A/B artifacts remain auditable.

## M1 peak-RSS evidence

The established “1 MiB reply” fixture emits exactly 1,114,112 `x` bytes (1 MiB
+ 64 KiB). Each accepted run recorded exactly two `/v1/chat/completions`
requests, an empty stderr, the large delta/completed-item/done anchors, and the
expected assistant-node projection. Each experimental cohort pools two
independent five-run batches, so N=10 per row; all recorded one-minute loads
were 1.49-1.95, strictly below 3.

| cohort | N | daemon median | daemon max | client lifetime median | client lifetime max | median JSONL |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 10 | 53,673,984 B | 56,098,816 B | 16,056,320 B | 16,711,680 B | 3,382,275 B |
| A | 10 | 50,585,600 B | 53,329,920 B | 16,596,992 B | 16,695,296 B | 3,382,349 B |
| A+B | 10 | 50,921,472 B | 53,608,448 B | 16,121,856 B | 17,874,944 B | 2,268,228 B |

Per-lever peak deltas, using the required maxima:

| transition | daemon max delta | client max delta | decision |
|---|---:|---:|---|
| baseline -> A | -2,768,896 B (-4.936%, -2.641 MiB) | -16,384 B (-0.098%) | retain A |
| A -> A+B | +278,528 B (+0.522%) | +1,179,648 B (+7.066%, +1.125 MiB) | reject B |
| baseline -> A+B | -2,490,368 B (-4.439%) | +1,163,264 B (+6.961%) | reject combined result |

The pooled A maximum is 357,171 bytes below the 51.2 MiB
(53,687,091-byte) daemon ceiling. The A/B daemon executable was identical, so
the +278,528-byte A-to-A+B daemon change is cohort noise rather than a daemon
implementation effect; the separately measured client regression still rejects
B.

After removing B and applying the append-failure correction, the frozen final
A-only binaries received an additional five-run confirmation:

| final source | N | load | daemon median / max | client lifetime median / max | projection / large events |
|---|---:|---:|---:|---:|---|
| final A-only | 5 | 1.81-1.88 | 49,364,992 / 49,725,440 B | 15,564,800 / 15,613,952 B | `legacy_text` / 3 |

Against the pooled baseline maximum, this final confirmation is -6,373,376 B
(-11.361%) daemon and -1,097,728 B (-6.569%) client. Its five JSONL files are
all 3,382,271 bytes. Raw evidence is under:

- `/private/tmp/peakrss2-m1-baseline{,-confirm}`
- `/private/tmp/peakrss2-m1-a{,-confirm}`
- `/private/tmp/peakrss2-m1-ab{,-confirm}`
- `/private/tmp/peakrss2-m1-final-a2`

An independent verifier recomputed all 30 experimental artifacts and agreed:
A-only SHIP; A+B NO_SHIP under the maximum-RSS criterion.

### Continuation rebuild confirmation (controlling result)

The continuation rebuilt `haider` and `haiderd` from the retained A-only tree
and admitted a fresh five-run cohort under the lane's stricter load1m < 3
rule. All five runs are valid: exact 1,114,112-byte reply, exactly two provider
requests, empty stderr, legacy assistant-node text, three large-text JSONL
events, and all required delta/completed-item/Done anchors.

| N | load range | daemon median / max | client lifetime median / max | JSONL median / max |
|---:|---:|---:|---:|---:|
| 5 | 2.46-2.50 | 52,543,488 / **54,558,720 B** | 16,695,296 / 16,760,832 B | 3,382,277 / 3,382,277 B |

The daemon maximum is **871,629 bytes above** the 53,687,091-byte ceiling.
Peak timestamps were 3-12 ms after the durable `Done` anchor (and 14-28 ms
after completed-item publication in the four highest runs), proving that the
failure is in the intended terminal ownership window. Relative to the pooled
baseline maximum, the rebuilt A-only maximum still improves daemon RSS by
1,540,096 bytes (-2.745%, -1.469 MiB), while client maximum increases by
49,152 bytes (+0.294%, +0.047 MiB). Thus A is directionally useful, but the
strict daemon maximum and no-client-regression criteria are not both met by the
controlling rebuild.

Raw evidence:
`/private/tmp/peakrss2-m1-resume-final-a/*/summary.json` and the adjacent
per-millisecond `m1-rss.tsv` files.

## Footprint budget and retention attribution

The final A-only release daemon passed
`scripts/perf/daemon-footprint-budget.py` with retention attribution enabled:

| measure | result | budget |
|---|---:|---:|
| accepted runs | 5 (1 overload rejection) | 5 required |
| maximum accepted load | 3.9155 | < 4 |
| median idle physical footprint | 5,554,656 B | <= 6,020,010 B |
| median post-40-turn footprint | 15,467,024 B | <= 18,167,160 B |
| median settled growth | 9,912,368 B | diagnostic |
| active footprint slope | 228,123.55 B/turn | diagnostic |

Every turn overwrote an exact 60-byte file and emitted an exact 30-byte stdout
payload; the harness verified all 40 durable tool-result previews, so the
retention attribution cannot hide behind variable output volume. Full evidence:
`/private/tmp/peakrss2-footprint-final-a.json` and
`/private/tmp/peakrss2-footprint-final-a-artifacts`.

The continuation also queued the same N=5 command against the rebuilt binary.
Unrelated shared-host work held load1m between 5.21 and 19.29 throughout the
admission window, so the harness admitted zero runs and was stopped while still
inside its pre-run load guard. It created no result JSON, no artifact directory,
and no daemon workload. This is neither a passing nor failing fresh footprint
sample; the table above remains the valid footprint evidence for the unchanged
A-only product source. It cannot override the independent fresh M1 maximum
failure.

## SIGKILL and replay parity

`scripts/qa-gate/turnperf_sigkill_matrix.py` against the frozen final A-only
binaries passed 47/47 cases with zero failures:

- all 47 stores passed integrity checks and contained exactly one typed
  terminal;
- durable replay events equalled the source-event snapshots exactly;
- the recovered live prefix+suffix equalled the same source events after only
  removing the live-only terminal annotations;
- replay issued zero provider requests and did not mutate the provider ledger;
- the on-disk provider ledger contains 55 rows and maximum multiplicity for
  each `(case_id, logical_ordinal)` is one.

Evidence: `/private/tmp/peakrss2-sigkill-final-a.json`, provider-ledger SHA-256
`3b23554a34950510195bfa2bb0dd8025f1a1becdab5a390c50c5471b23d79239`.

The continuation reran the complete matrix against the rebuilt binaries and
again passed **47/47** with zero failures, exact source/replay event parity,
zero replay provider requests, valid store integrity, and one typed terminal
per case. Evidence:
`/private/tmp/peakrss2-sigkill-resume-final-a.json` (SHA-256
`8271245d35a9b3cec5a3f42c06f4b70dc9e9e6cf42ee85453eca6edac7c848af`),
provider-ledger SHA-256
`aa92f499a5602acac760b80b4331a7e7ec1262e8376da7259941ac6e2cdfbbcd`.

## Frozen release artifacts

| cohort | binary | bytes | SHA-256 |
|---|---|---:|---|
| baseline | `haider` | 34,665,408 | `f451aa4cba0798eccef46cbcb599b7ac9b39e75514ee6a2a34261dd19cba4a1e` |
| baseline | `haiderd` | 52,357,648 | `9395ce2e33a56361206fe906c5e4abec366449be68f0bc4e9acf5d60489670c3` |
| A+B experiment | `haider` | 34,681,936 | `5017f3c28f4b88f371e9b3eb0422c2420aff14fe08111d4e68df3cef3a8afa9d` |
| A+B experiment | `haiderd` | 52,357,648 | `e6298b2c1aac524e3d4c0124befc2ff6191043790c07023cd7f7b65261d5ab58` |
| final A-only | `haider` | 34,681,936 | `14720a705d862b76dbf9bfc063be39d39dbb36115a6fe866fdd6d271a0c4bd84` |
| final A-only | `haiderd` | 52,357,648 | `e2dc40878598619a6e2ab71cc977cf051885b804c50ad7ea9f34f09049974de1` |
| continuation A-only | `haider` | 34,665,424 | `f079d1c3284ebaf4974ffed7a2355c188adbb2da085ef6faa946fe55579dfd36` |
| continuation A-only | `haiderd` | 52,357,648 | `67d70a194a650b9c407a29e14513de79a80256792d5705e97d2a29c1f5ee19a3` |

All daemon artifacts exceed the registry #64 10 MiB truncation floor.

## Tests and static gates

The final A-only source passed the complete affected-package gate under the
required environment, locked dependencies, four test threads, prebuilt daemon
siblings, and `CARGO_INCREMENTAL=0`:

```text
cargo test --locked --no-fail-fast \
  -p haider-protocol -p haider-rpc -p haider-core -p haider-client \
  -p haider-cli -p haider-store -p haider-tui -p haider-daemon \
  -- --test-threads=4
```

This includes 70 core runtime tests, 918 daemon unit tests (3 pre-existing
ignored), 103 daemon session-hub tests, all CLI integration suites, and all
remaining protocol/RPC/store/TUI/doc tests. Named mutation coverage includes:

- `shared_text_clones_storage_but_keeps_legacy_wire_bytes`
- `no_boundary_post_stream_facts_share_one_ordered_append`
- `rejected_post_stream_append_keeps_open_item_for_error_cleanup`
- `rejected_post_stream_append_preserves_every_pending_input`

Closing gates:

- `cargo fmt --all -- --check`: PASS
- `git diff --check`: PASS
- `scripts/check-unsafe-counts.sh`: PASS, production 189 / test 16
- `xtask test-count`: PASS, 4,370 / baseline 4,367
- QA-gate Python suite: PASS, 61 tests
- Python compilation of all changed measurement/matrix scripts: PASS
- `sh -n scripts/perf/m1-peak-case.sh`: PASS
- M1 sampler self-test: PASS

Continuation rerun against the retained source:

- release build for `haider-cli` and `haider-daemond`: PASS
- full eight-package test command above: PASS (including 918 daemon unit
  tests with 3 ignored and 103/103 session-hub tests)
- all four named ownership/append-failure mutation tests: PASS
- `cargo fmt --all -- --check` and `git diff --check`: PASS
- unsafe counts: PASS, production 189 / test 16
- `xtask test-count`: PASS, 4,370 / baseline 4,367
- QA-gate Python discovery: PASS, 64 tests
- changed Python compilation and M1 shell syntax: PASS
- M1 sampler self-test: PASS, 42 samples with positive root/descendant RSS

## Citation audit and CI registry walk

The round-1/round-2 lens citations target older `/wt-965` trees. Their ownership
and durability claims remain useful, but the cited lines are drifted for this
tree. In particular, the old actor `:9918`/`:9733` anchors and headless
`:1752-1800` window are not accepted as-is. The current audited anchors are the
protocol fields at `item.rs:30-138,186-193` and `history.rs:40-43`, stream growth
at `actor.rs:6235-6279`, completion at `actor.rs:7171-7200`, the atomic terminal
suffix at `actor.rs:9908-9998`, and headless ownership consumption at
`headless.rs:1791-1795`.

- #10/#19: Python tests/compilation, Rust formatting, shell syntax, and diff
  whitespace all pass; no dead helper remains.
- #20: the exact test recount is 4,370, above the 4,367 baseline.
- #21/#54: every Cargo invocation used the mandated stack/discovery/device,
  no-incremental, and no-dev-debug environment with locked metadata.
- #41/#74: M1, footprint, and SIGKILL use short hermetic throwaway roots and
  clean exact process ownership.
- #64/#71: real frozen release artifacts—not inferred test binaries—were
  measured end-to-end; every daemon is over 10 MiB and hashes are recorded.
- #72: native discovery is disabled only for the intentionally hermetic fake
  provider measurements.
- #77: the unsafe-count guard passes at production 189 / test 16.
- #94/#95: no product timeout or external-state wait was added. The new 50 ms
  attribution pause is measurement-only at a driver-held idle checkpoint; the
  daemon remains live and able to service its connection.
- #96: this is not a turn-wall claim. Its applicable controls are retained:
  exact request counts, fixed binaries and payload, raw per-run artifacts,
  untrimmed median/max reporting, and overload rejection. M1 uses its stricter
  lane-specific load <3 and N=10 per experimental cohort.

The ownership and lifecycle verification remains clean, but it does not waive
the controlling release-RSS maximum failure documented above.

NO_SHIP
