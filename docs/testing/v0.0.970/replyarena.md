# replyarena — append-once reply arena evidence

Date: 2026-09-03

Branch: `lane-970-replyarena`

Starting HEAD: `0cb2cfb2106e8a63b6fa552a10627244d1153109`

Disposition: uncommitted, as required by the lane brief

## Verdict

The arena implementation produces a large and repeatable M1 peak reduction,
passes the settled-footprint budget, improves the warm wall/CPU medians, keeps
the journal bytes compatible, and passes the complete 47-boundary SIGKILL
sweep. It is nevertheless **not shippable under the owner design**.

The strict acceptance condition was not merely an RSS target. It required one
canonical reply representation across every adapter and an exact provider-view
CAS hash updated as deltas arrive. The final audit found full reply-sized dual
representations in live Anthropic paths and in signed/provider-opaque Gemini and
OpenAI paths. The provider-view hash is streamed from final ranges, but it is
computed during final request preparation rather than updated per reply delta.
Those are product gaps, not missing measurements, so good aggregate RSS cannot
turn this verdict into SHIP.

## Inputs and baseline

The supplied `LANE-COMMON.md`, `LANE-BRIEF-replyarena.md`, `codepagediet.md`,
`turnperf/`, and `turnperf2/` were read before implementation. The supplied
untracked inputs were not modified and remain uncommitted.

| Baseline fact | Bytes | MiB | Source/status |
|---|---:|---:|---|
| M1 daemon peak RSS | 53,706,752 | 51.22 | exact paired peak in `codepagediet.md` |
| Physical footprint | 31,506,984 | 30.05 | `codepagediet.md` |
| File-backed/shared part | 22,199,768 | 21.17 | RSS minus footprint |
| Heap at peak | 28,295,168 | 26.98 | source diagnostic in `codepagediet.md` |
| Full live reply copies | at least 4,456,448 logical | 4.25 | four copies of the 1,114,112-byte reply |
| Held take-not-clone attempt | 54,558,720 peak | 52.03 | `lane-970-peakrss2`; null/worse result |
| Adopted settled idle | 5,472,736 | 5.22 | `memdaemon.md` / `codepagediet.md` adopted N=5 baseline |
| Adopted settled retention | 276,481 B/turn | — | `memdaemon.md` / `codepagediet.md` adopted N=5 baseline |
| Adopted post-40 footprint | 16,515,600 | 15.75 | `memdaemon.md` / `codepagediet.md` adopted N=5 baseline |
| Held turnhygiene retention | 352,666.8 B/turn | — | `lane-969-turnhygiene` final N=3 median |
| Held turnhygiene post-40 | 19,661,328 | 18.75 | `lane-969-turnhygiene` final N=3 median |

The current line citations in this report were rediscovered in this worktree.
The owner brief itself names subsystems rather than stale product-code line
numbers; none of its old analysis paths were trusted without a fresh search.

## Implementation stages

| Stage | Implemented result | Verification | Gate |
|---|---|---|---|
| 1. Canonical arena type | Added `ReplyArenaWriter` and immutable `ReplyText` ranges over reference-counted `Bytes` chunks. Append transfers the delta allocation into the arena; range clones do not copy reply bytes. The last handle releases the arena. | `reply.rs:633` proves one 1 MiB chunk, pointer identity across delta/whole handles, and last-handle release. | PASS for the core type |
| 2. Protocol and journal compatibility | Assistant text/reasoning fields use `ReplyText`. Specialized JSON and named-MessagePack writers stream the range chunks while retaining legacy string-scalar bytes. | Every reply path, multi-chunk Unicode/control escaping, replay/re-encode, and MessagePack string headers 0/1/31/32/255/256/65535/65536 pass (`envelope.rs:812`, `:867`). | PASS |
| 3. Actor, replay, and durable store | The actor's `TextAccumulator` owns an arena writer. Delta/completed/node facts share its ranges. SQLite inserts use `ZeroBlob` plus incremental BLOB writes. Store replay re-canonicalizes delta/completed/node ranges. | Protocol 79/79, store 51/51, core 56/56; the cross-page mutation test drives 256 independently decoded deltas through paginated store replay and prompt-cache replay. | PASS |
| 4. Downstream consumers | Prompt history keeps arena identity across store page boundaries; route replay prefixes, hook digests, RPC frames, JSONL, observe projections, and TUI live/viewport state stream or retain ranges. The prompt body budget walks resident structures instead of serializing the reply merely to count it. | Core cross-page test, CLI goldens/pins, daemon suites, client suites, and targeted TUI/tuivirt tests passed. | PASS for exercised paths |
| 5. Outgoing provider request/CAS | OpenAI, compatible, Anthropic, and Gemini prepared bodies use marker bindings and exact final wire streaming. Provider-view CAS blobs can contain `JsonString(ReplyText)` segments and write/hash them without joining the reply. | Provider library 213/213; exact rendered-wire and one-transient-view allocation tests pass. | PARTIAL: the CAS hash is still computed from the completed provider view, not incrementally per incoming delta |
| 6. Incoming provider/native replay audit | Plain adapters emit normal deltas into the actor arena, but several decoder/native-replay paths still retain a second full representation. | Static audit at the current lines listed below; independent verifier confirmed the findings. | **FAIL** |

### Peak checkpoints

All rows use the exact M1 two-request large-reply case. Runs admitted only below
the pinned load limit and retained the expected JSONL anchors and provider count.

| Checkpoint | N | Load(1m) | Peak median bytes | Peak max bytes | Max MiB | Versus exact baseline |
|---|---:|---:|---:|---:|---:|---:|
| Exact clean baseline | 5 | 2.50 paired diagnostic | 53,493,760 (A2) | 53,706,752 exact selected peak | 51.22 | reference |
| Held take-not-clone | — | held evidence | — | 54,558,720 | 52.03 | +1.6%, reject |
| Arena integration checkpoint | 5 | 2.66–2.72 | 45,432,832 | 46,202,880 | 44.06 | -14.0% max |
| Final release binaries | 5 | 2.63–2.66 | 34,406,400 | **38,240,256** | **36.47** | **-28.8% max** |

Final M1 raw maxima were 36,306,944; 38,240,256; 34,078,720;
34,209,792; and 34,406,400 bytes. The median MAD was 327,680 bytes. Every
run produced 3,382,983 JSONL bytes and exactly two physical provider requests.
The <=45 MiB hard cap passes by 8.53 MiB at the worst sample.

Artifacts: `target/m1-rss-replyarena-final/*/summary.json`. These measurement
artifacts are intentionally outside the change set.

## Settled footprint and retention

Command protocol: N=3, 40 turns, 60-second idle settle, 60-second post-turn
settle, `load(1m) < 4`, with retention attribution enabled. Attempts 1 and 3
were rejected when unrelated load rose above the limit; they are excluded from
the summary.

| Accepted attempt | Max load(1m) | Idle footprint bytes | Post-40 bytes | Settled growth bytes | Bytes/turn | KiB/turn |
|---:|---:|---:|---:|---:|---:|---:|
| 2 | 3.956 | 5,472,736 | 14,746,104 | 9,273,368 | 231,834.2 | 226.40 |
| 4 | 3.927 | 5,571,016 | 14,189,048 | 8,618,032 | 215,450.8 | 210.40 |
| 5 | 2.927 | 5,423,584 | 15,106,576 | 9,682,992 | 242,074.8 | 236.40 |
| **median** | — | **5,472,736** | **14,746,104** | **9,273,368** | **231,834.2** | **226.40** |
| MAD | — | 49,152 | 360,472 | 409,624 | 10,240.6 | 10.00 |

The repository budgets pass (idle <=6,020,010 and post-40 <=18,167,160).
Candidate retention is 44,646.8 B/turn (16.1%) below the adopted 276,481
B/turn reference and 120,832.6 B/turn (34.3%) below the held turnhygiene N=3
median. Candidate post-40 is 1,769,496 bytes (10.7%) below the adopted
16,515,600-byte reference and 4,915,224 bytes (25.0%) below the held N=3
median. Those deltas exceed the candidate MAD. Candidate idle exactly matches
the adopted 5,472,736-byte median and is within the combined held/candidate MAD,
so the idle-unchanged condition passes.

Artifact: `target/replyarena-footprint/measurement.json`.

## Warm wall and CPU ABBA

The final binaries ran one stable daemon with 5 unreported warmups and 25
measured samples per shape in ABBA order. Load was 2.360/2.360/2.491 at
start/mid/end. Provider counts, JSONL continuity, one terminal, daemon identity,
and cleanup all passed.

| Shape | 969 reference wall ms | Final wall ms | 969 reference CPU ms | Final combined CPU ms | Result |
|---|---:|---:|---:|---:|---|
| Single request | 56.702 +/- 3.474 | **40.719 +/- 2.888** | 5.453 +/- 0.268 | **4.331 +/- 0.109** | wall and CPU improve beyond reference MAD; 40 ms owner target missed by 0.719 ms |
| Tool/two requests | 77.955 +/- 3.868 | **58.505 +/- 4.926** | 6.768 +/- 0.244 | **5.235 +/- 0.344** | wall and CPU improve beyond reference MAD; 60 ms owner target passes |

The lane brief requires no wall/CPU regression rather than the turnperf owner
targets, so the replyarena latency condition passes. Artifact:
`target/replyarena-warm-abba.json`.

## Durability, goldens, and affected suites

Every command used the lane environment:
`RUST_MIN_STACK=8388608`, `HAIDER_DISCOVERY_DISABLED=1`,
`HAIDER_TEST_DEVICE_NAME=test-mac`, `CARGO_INCREMENTAL=0`, and
`CARGO_PROFILE_DEV_DEBUG=0`; sibling-process tests also used
`HAIDER_TEST_SIBLINGS_PREBUILT=1`. `df -m /` was checked before each build and
never approached the 700 MiB stop threshold.

| Proof | Result |
|---|---|
| `haider-protocol --lib` | 79/79 PASS |
| `haider-provider --lib` | 213/213 PASS |
| Provider offline integrations | PASS |
| `haider-store --lib` | 51/51 PASS |
| final `haider-core --lib` | 56/56 PASS |
| CLI `turnhygiene_pin_tests` final rerun | 9/9 PASS, including text/tool JSONL goldens, replay equality, detached parity, request-body golden, and delayed-output pin |
| CLI `one_shot_jsonl_stream_matches_the_normalized_golden` final rerun | 1/1 PASS |
| Daemon library/integration set | 921 PASS, 3 pre-existing ignored; session-hub set 103 PASS |
| Client full affected suite | PASS |
| Targeted TUI/tuivirt projections and goldens | PASS |
| SIGKILL transaction-boundary sweep | **47/47 PASS**, 0 failures (14 single, 33 tool cases) |
| Release artifacts | `haider` 32,846,640 bytes; `haiderd` 51,464,896 bytes (>10 MiB plausibility pin) |
| Formatting and whitespace | `cargo fmt --all -- --check` PASS; `git diff --check` PASS |

SIGKILL artifact: `target/replyarena-sigkill-matrix.json`. The exact release
SHA-256 identities used by M1, ABBA, and SIGKILL were:

- `haider`: `2d59351dc944fa5ccedcef070e389674705b448f07ba03657c051d90500d7413`
- `haiderd`: `6d65636c6901d49ea4df5a55e29493f23d26428f4d8221447b027d106946a9a2`

A whole-workspace green claim is deliberately not made: the branch inherits a
known unrelated `tpsfix` expectation mismatch from `683da3c`. No tpsfix or
OAuth territory was changed to hide that baseline failure.

## Unresolved owner-design violations

| Gap | Current evidence | Why it blocks SHIP |
|---|---|---|
| Anthropic text | `wire/mod.rs:1193-1195` appends each `WireDelta::Text` into `OpenBlock::Text.text: String` and also emits that text to the actor arena. Citation-bearing completion then embeds the full text again in provider-opaque JSON at `:1400-1411`. | A plain streamed reply has decoder accumulation plus actor arena until block stop; citation replay persists another native representation. This is a full reply-sized dual representation. |
| Anthropic thinking | `wire/mod.rs:1237-1241` appends each reasoning delta to `OpenBlock::Thinking.thinking: String` and emits a reasoning delta; signed completion embeds the accumulated thinking in provider-opaque JSON at `:1368-1380`. | Signed reasoning is retained both as arena ranges and native replay JSON. |
| Gemini signed text/thought | `gemini.rs:2171-2250` reads `part.text`, emits a text/reasoning delta, and for `thoughtSignature` clones the complete native `part` into `ProviderOpaque`. | The signed provider-native part includes the full reply text beside the canonical arena. |
| OpenAI Responses reasoning | `openai.rs:2745-2750` clones the completed reasoning item into `ProviderOpaque` after its reasoning deltas have already populated the arena. | Full reasoning remains in two persisted representations. |
| OpenAI-compatible fragmented assistant text | `openai.rs:5257-5272` creates `joined_copy: String` when legal text blocks are not joinable arena ranges, then sends that copy at `:5312-5319`. | Some valid histories flatten a full assistant reply even though the source ranges remain live. |
| Provider-view hash timing | `provider/lib.rs:872-897` constructs the segmented final view; `protocol/cache.rs:158-200` then hashes its complete escaped bytes, and the store recomputes before publication. | This is bounded, zero-join final hashing, but it is not the owner-required CAS hasher updated incrementally per incoming delta. |
| Peak copy proof | `reply.rs:633-658` proves one allocation and last-handle release for the arena itself; M1 proves aggregate process peak improvement. | Because the adapter gaps above remain, the requested universal “~1x live reply” proof is false even though the canonical core path is one chunk. |

Fixing these cleanly requires extending provider stream/native replay types so
opaque provider fields can reference arena ranges, and carrying a provider-view
hashing state alongside the stream. Merely deleting the native copies would
break exact tool-loop/signed/citation replay. Materializing them later would
only move the duplicate allocation and would not satisfy the owner design.

## CI registry walk

- Registry #64: PASS. Final `haiderd` is 51,464,896 bytes, above the 10 MiB
  sibling plausibility threshold.
- Registry #94: no new production timeout or deadline was introduced, so no
  deadline arithmetic exception is present.
- Registry #95: no new wait on external state while a negotiated connection is
  open was introduced.
- Tests and goldens were not weakened, ignored, or platform-gated.
- The two supplied turnhygiene fixtures match the held lane hashes and are
  intended additions; supplied lane research files remain untouched and must
  not be committed.

The memory, retention-budget, latency, journal, golden, and crash gates pass.
The canonical-representation and incremental-hash gates do not.

NO_SHIP
