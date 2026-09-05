# v0.0.970 providerrebind

Branch: `lane-970-providerrebind`. Changes are uncommitted. This implements
AHRB item 6 (approved 2026-09-03): a receipt-backed per-session provider route
and an explicit cache/reuse declaration for economy harnesses.

## Implementation

- Added the negotiated `session.provider.rebind` RPC and
  `haider session provider rebind --session <id> --provider <id>
  [--base-url <url>] [--account <name>]` CLI. The CLI emits a JSON receipt.
- `session_provider_rebound` is an additive typed journal event. The event,
  metadata projection and retry receipt commit in the existing session-config
  transaction. Replay performs no provider traffic. Missing endpoint/account
  arguments clear the corresponding override. The model remains unchanged.
- Every core request-loop boundary checks a cheap daemon revision. Only a
  changed revision reads metadata, and only a new session rebind identity
  rebuilds an adapter. Preparation and transport retain the same immutable
  adapter; an earlier in-flight stream is unaffected. A later explicit
  rebind with identical arguments still resolves current registry/account
  defaults. There is no new write or external wait on the ordinary path.
- Factory construction overrides local copies of both the registry endpoint
  and account endpoint; the adapter cache key separates changed endpoints.
  Fixed first-party/OAuth/agent endpoints cannot be redirected. Registered
  custom and `openai-compatible` proxy endpoints use the existing TrustedLan
  validation; Bedrock and Vertex use their existing enterprise templates.
- Active routing preserves the run's frozen permission ceiling. The RPC
  checks it under the admission selection lock, and the request boundary
  checks the actual resolved adapter again to cover registry/account changes
  after receipt. Live model and reasoning coordinates come from the core
  actor, with current-model registry validation at pickup. Recovery keeps the
  rebound provider, URL and account together, restores completed automatic
  model changes, and retains the accepted run's frozen authority identity.
- A durable rebind identity contributes to the actual prompt-cache domain,
  including after restart. Unbound request/cache bytes remain unchanged.
- Factory-time alternate-account selection is journaled before the rebound
  request and consumes the same logical-turn rotation allowance; a rebind
  cannot refund an already consumed allowance.
- `daemon.caching` declares prompt-cache support, provider-view CAS,
  resident/one-shot process policy, effective idle TTL and adapter regime.
  The scalar regime describes the active account; the provider map supports
  mixed/rebound sessions. Old daemons remain unknown (`null`). OpenAI-family
  baseline is `automatic-prefix`; Anthropic is `explicit-breakpoints`.

The automation contract defines the request linearization boundary,
omission semantics, registry restrictions, typed errors, cache scope and
old-daemon behavior. The event changelog and client feature/RPC tables are
updated. Session metadata's new optional fields preserve legacy bytes.

## Integration with wave-970

Starting HEAD was `2ef44708757e0f87b4437ec4ab1594c6a680814e`.
Both required original-metadata commands were attempted:
`git fetch origin wave-970` failed opening protected `FETCH_HEAD`;
`git merge --no-commit origin/wave-970` failed locking protected `ORIG_HEAD`.
The local current ref was `f1cf80c9238bfe5b014e61b5e406723c38fa6e5d`, a descendant
of starting HEAD. Its complete content diff was integrated with a temporary
index/object directory; the single RPC export conflict preserved both the
read-only and provider-rebind feature exports. Git then refused the external
`MERGE_RR` lock, so the conflict was resolved in the files. The original Git
index, refs and merge metadata remain untouched; the orchestrator must record
the merge. Upstream-added files are present and uncommitted. The local ref then advanced
to `7694ef9cbd2fbbcedb24fee14dbf4b12b1c4cd39` (winclip). That complete
content delta applied cleanly, with its baseline count handled by the required
recount tool rather than a manual merge.

The supplied LANE-COMMON, LANE-BRIEF and turnperf/turnperf2 evidence were read
and remain input evidence, not proposed commits. No protected OAuth file was
modified. The minimal cross-lane touches are the core request-boundary hook,
worker resolver installation/recovery reconstruction and the session actor's
existing commit/fanout path; no retirement, workflow, budget, stream-reconnect or durable-attempt
boundary was changed.

## Evidence audit

| Supplied evidence | Audit |
| --- | --- |
| Early FACTS estimate of 6–8 full-device journal syncs per turn | Wrong; PROPOSAL already corrects it. SQLite journal uses WAL/NORMAL; provider-view CAS has its separate ordered barrier. |
| `event_store.rs:176` NORMAL citation | Drifted; the current `DEFAULT_STORE_SYNCHRONOUS` declaration is authoritative. No durability policy changed. |
| D2/R2-24 provider-view prepare near `provider_view_store.rs:106` | Correct construct neighborhood; `finish_ordered_batched_puts` still fences CAS. Old `cas.rs:290` line drifted. |
| D3 provider pool lifetime citations | Drifted line numbers; unchanged ten-minute adapter pool lifetime. Endpoint override participates in adapter cache identity. |
| FACTS load <6 / old registry load <4 | Drifted. Current warm harness requires load <3, 5 warmups +25 measured per shape, stable PID/generation/Idle and exact 1/2 provider counts. |
| Round-one atomic admission/fused RPC proposals | Rejected evidence retained: fused admission duplicated a provider request; fused RPC regressed latency. This lane preserves those boundaries. |

## Verification

Named coverage includes production HTTP routing and named-account selection,
in-flight isolation, other-session isolation, registry-default refresh,
receipt idempotency, typed refusals, restart/event replay parity, durable store
projection parity, adapter-cache separation, actual model pickup, frozen-trust
regressions, effective cache epoch and status wire/CLI goldens.

Builds/tests use `RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`,
with `CARGO_BUILD_JOBS=2` for the broad builds. Daemon-driven checks use
prebuilt siblings and `HAIDER_TEST_SIBLINGS_PREBUILT=1`. Final debug binaries:
`haider` 110,060,336 bytes; `haiderd` 199,140,464 bytes (above 10 MiB).
Windows/Linux are **by inspection**, not executed here.

| Check | Result |
| --- | --- |
| Actual CLI rebind with throwaway profile/proxy | PASS; JSON receipt, same daemon PID/generation, declared custom-provider cache regime ([evidence](providerrebind/cli-smoke.json)) |
| Production UDS + HTTP proxy routing | 3/3 PASS: in-flight A/next B, unrelated session isolation, same-coordinate registry refresh, explicit account selection, typed refusals, receipt retry and stop/restart replay ([log](providerrebind/rpc-proxy-tests.log)) |
| Daemon factory/boundary/recovery regressions | 11/11 PASS, including three actual worker recovery variants ([log](providerrebind/recovery-tests.log)) |
| Core effective cache epoch | 1/1 PASS |
| Core durable rotation/retry allowance | 2/2 PASS, including a fresh-allowance positive control |
| Store rebind transaction/replay | 4/4 PASS |
| Protocol golden + schema changelog | 58/58 + 1/1 PASS |
| RPC wire goldens | 102/102 PASS; new pair is generated from typed frames ([log](providerrebind/wire-goldens.log)) |
| Typed automation examples | 1/1 PASS; caching object decodes as `DaemonCachingWire` and matches the CLI golden |
| CLI status golden, status compatibility and cache regimes | PASS |
| Prompt/request/text/tool turn goldens | 9/9 PASS, regenerated using `UPDATE_FIXTURES=1` ([log](providerrebind/turnhygiene.log)) |
| Served Welcome feature pin | PASS, 114 tokens |
| Instruct-pipe byte pin | PASS, **13,552 → 13,552** (merged prompt unchanged) |
| `xtask test-count --update` | **4,860** ([log](providerrebind/test-count.log)) |
| `bash run.sh test` from `scripts/qa-gate` | **65/65 PASS** ([log](providerrebind/qa-gate.log)) |
| Final merged `cargo test --workspace --no-fail-fast` | **PASS, exit 0, no failed targets** ([full log](providerrebind/workspace-tests.log)) |
| Formatting and whitespace | **PASS** at final gate completion |

The first broad run exposed four incomplete contract/test declarations: the
status golden, exhaustive RPC method pair, typed documentation JSON fence,
and a nested account-test declaration that the reachability guard could not
recognize. Each was corrected without weakening a test. The final run below
covers the corrected merged tree and passes all four previously failing targets.
The machine-readable [gate summary](providerrebind/gate-summary.json) records
the completed checks.

## Warm wall ABBA

**PASS: both warm shapes remain within the predeclared MAD tolerance.**
Both frozen binary pairs use the same `release`
profile (fat LTO, one codegen unit); A is the merged wave baseline
`7694ef9cbd2fbbcedb24fee14dbf4b12b1c4cd39`, B includes this lane. All 1,562
tracked baseline files match that commit
([source check](providerrebind/baseline-source-check.json)). Workspace
crate artifacts were rebuilt between the two sources to prevent stale package
reuse. The order is A B B A, with the unchanged harness's 5 warmups +25
measured rows per shape on every leg. The comparison is declared before
measurement: B median must be at most A median plus the larger MAD, using all
50 measured rows per variant/shape and removing no outliers. Every leg passed
the existing load-below-3, exact 1/2 request, stable identity, Idle and
cleanup gates. The repository reserves this shipping performance gate for release binaries;
no CI-profile timing is used. This is the ordinary warm single/tool-turn path;
the explicit rebind command's routing behavior is verified separately by the
held HTTP proxy ledger tests above.

| Shape | A median / MAD (ms) | B median / MAD (ms) | B − A (ms) | Allowed increase (ms) | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Single | 39.166 / 2.697 | 40.974 / 2.794 | +1.808 | 2.794 | PASS |
| Tool | 58.405 / 3.788 | 59.374 / 2.345 | +0.969 | 3.788 | PASS |

This was the first and only measurement attempt. All four legs exited 0;
start/mid/end load ranged from 2.295 to 2.353. Each leg retained 10 warmup
rows, 50 measured rows and all 90 provider-ledger entries. Daemon identity
remained stable within each leg and every daemon stopped cleanly. No
correctness failure, rejected measurement or excluded sample occurred.

Raw legs: [1-A](providerrebind/abba/1-A.json),
[2-B](providerrebind/abba/2-B.json), [3-B](providerrebind/abba/3-B.json),
[4-A](providerrebind/abba/4-A.json). The
[aggregate](providerrebind/abba/summary.json) contains exact medians/MADs;
the [binary manifest](providerrebind/abba/release-manifest.json) records sizes
and SHA-256 identities, which match every raw leg. A's `haider`/`haiderd`
sizes are 34,634,000 / 54,307,344 bytes; B's are 34,782,848 / 54,522,320 bytes.
The [A build](providerrebind/abba/release-A-build.log) and
[B build](providerrebind/abba/release-B-build.log) both passed. The
[readiness log](providerrebind/abba/load-readiness.jsonl) records the wait
for host load below 3 before timing.

The [runner](providerrebind/abba/run-abba.sh) invokes the unchanged command
below for each frozen pair in A B B A order; no load, warmup or sample-count
override was supplied:

```sh
python3 scripts/qa-gate/turn_wall_harness.py \
  --bin-dir /tmp/providerrebind-release-<A-or-B> \
  --commit-label <A-or-B>-7694ef9c-providerrebind-release \
  --output /tmp/providerrebind-abba/<leg>-<A-or-B>.json
```

## Independent verifier value

Six findings changed code/tests: frozen active trust versus mutable session
metadata (including post-receipt account races); actual model after automatic
fallback versus initial/pending metadata; identical arguments versus changed
registry defaults; and the real prompt-cache epoch versus an overwritten
usage-scope assignment; cross-provider recovery mixing old headless provider
settings with rebound coordinates and rebinding frozen Full authority; and
dropped factory-time rotation facts/allowance during active pickup. None was
rejected as noise.

The [independent final verdict](providerrebind/verifier.md) is **SHIP**. The
verifier recomputed release medians/MADs from all raw rows and checked binary
hashes, load acceptance, 240 cases, all 360 physical provider requests, stable
daemon identities and cleanup. No outstanding finding remains.

## CI error registry walk

The complete #1–96 delta walk is appended under **v0.0.970 providerrebind
delta walk** in
[CI_REGISTRY_WALK_QAGATE3.md](../../../scripts/qa-gate/CI_REGISTRY_WALK_QAGATE3.md).
It covers additive serde/golden compatibility; durable commit/replay ordering;
hermetic real-provider-path tests and prebuilt binary size; frozen permission
boundaries; derived observation deadlines and keepalive; and unchanged warm
harness correctness/load pins. All required functional and release wall
gates passed on the merged tree.
