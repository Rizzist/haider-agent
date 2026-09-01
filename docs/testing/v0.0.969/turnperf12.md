# turnperf12 — steady-state turn proof (v0.0.969)

Date: 2026-09-01  
Base commit: `9ed86565bd5ac42c9e942a5ac16fc5c6600cf8bd` plus this uncommitted lane  
Verdict: the proof infrastructure and fresh crash sweep pass, but the lane does not meet the owner targets. The final-binary/final-support trace-off medians are 56.702 ms for the one-request shape and 77.955 ms for the two-request tool shape, above the required 40/60 ms ceilings. A final paired baseline, full trace-on batch, and CI confirmation were refused because load(1m) remained above 4; their earlier artifacts are retained only as historical evidence. No performance lever was retained by this proof-only lane.

## Delivered proof surfaces

- `scripts/qa-gate/turn_wall_harness.py` and `turnperf_support.py`: a stdlib-only, standalone loopback provider and ABBA steady-state harness. It uses one warmed/settled daemon, 5 unreported warm-ups plus 25 retained samples per shape, exact provider counts 1/2, median and untrimmed MAD, combined process CPU, simultaneous and conservative component-sum peak RSS, start/mid/end `load(1m) < 4`, unchanged PID/generation, contiguous durable JSONL, one typed terminal, and exact cleanup through `haider daemon stop`.
- `docs/testing/v0.0.969/turnperf/baseline-v0.0.968.json` and `baseline-v0.0.968-confirmation.json`: accepted v0.0.968-main baseline and confirmation. The installed binary hashes are pinned in both reports.
- `docs/testing/v0.0.969/turnperf/current-v0.0.969.json`: accepted final-binary/final-support trace-off measurement. Its harness hash predates only the closing trace-on cardinality correction from 24 to 23 transactions; the trace-off measurement path is unchanged. `current-v0.0.969-trace-on.json` is the accepted pre-final opt-in trace companion; the final full refresh was load-blocked.
- Trace-only `haider.turn` points: accept, durable request-attempt commit, provider open, raw first byte, provider stream, SSE decode, SQLite journal transaction, session-actor journal wait, event projection/fanout, terminal commit, and client terminal seen. The audited subscriber accepts only the named phase and numeric fields.
- `scripts/qa-gate/turnperf_sigkill_matrix.py`: discovery plus serial real-`SIGKILL` boundary sweep, backed by a disk-synced external provider ledger and a test-only post-transaction observer.
- `scripts/qa-gate/checks/t1/t1.turn.wall_budget.py`: CI-runnable wall check with exact 1.10x budgets derived from the accepted confirmation baseline, a named 6,142-second outer budget, persistent report publication, load refusal, and exact cleanup. `LAW (TURN-WALL-1)` and registry item #96 are in `scripts/qa-gate/README.md` and `CI_REGISTRY_WALK_QAGATE3.md`.

The candidate fake at `/Users/rizzist/Documents/CODING/haidercode-web/bench/conformance/fake_proxy.py` is context-managed and stdlib-only, but it has no public per-case reset and no deterministic post/body/header/chunk gates. The vendored minimal stdlib server was therefore required for exact request attribution and the SIGKILL matrix.

## Measurement protocol and identity

Every published timing report has exactly 5 warm-ups and 25 retained samples per shape. One daemon PID/generation serves the full report and is stopped exactly afterward. The single shape makes one physical provider request; the tool shape makes two and executes one unique monotonic tool effect. All retained cases have one typed terminal and a contiguous journal. Loads were strictly below 4 at all three observation points; no refused run is used below.

Final release binaries used by the source-exact trace-off and matrix artifacts:

| Binary | SHA-256 | Bytes | Registry #64 |
| --- | --- | ---: | --- |
| `haider` | `6a7c8aae1f851628c11ccacce328a4558b678134cc44676c20956a2d0bbcbf47` | 34,632,320 | n/a |
| `haiderd` | `85b9cefeae8ba338f954b4ec8d93048a4ea8cfefa32145f340a2e361a97a329b` | 52,258,288 | PASS, greater than 10 MiB |

The final trace-off loads were 3.045/3.441/3.441. It kept one generation-1 daemon for the whole run and ended with `stopped_cleanly`. A same-support baseline retry was refused at 4.069/4.384/4.384; later load rose as high as 5.31, so no later timing batch is published. The artifact pins harness `3b47f95a...`; current harness `4dfcc688...` differs only in the trace-enabled expected-transaction constant.

## v0.0.968 baseline

Wall and CPU cells are `median +/- MAD`. `Peak` is the maximum simultaneously sampled combined client+daemon RSS. `Conservative peak` is the daemon lifetime maximum plus the maximum client lifetime peak, even if they occurred at different instants.

| Run | Shape | Wall ms | Combined CPU ms | Peak KiB | Conservative peak KiB |
| --- | --- | ---: | ---: | ---: | ---: |
| v0.0.968 primary | single | 56.832 +/- 5.712 | 4.691 +/- 0.214 | 27,072 | 27,344 |
| v0.0.968 primary | tool | 98.101 +/- 9.438 | 5.512 +/- 0.311 | 27,488 | 27,632 |
| v0.0.968 confirmation | single | 56.022 +/- 4.006 | 4.899 +/- 0.238 | 27,072 | 27,456 |
| v0.0.968 confirmation | tool | 92.693 +/- 4.895 | 5.882 +/- 0.360 | 27,536 | 27,696 |

Primary baseline identity: commit `d75a8ea1a579afe5fd6120004b7e191415544f7e`, `haider` SHA-256 `63d44ff6ad630ab9e96067829a0b15fd8e287f4333e9666e267edec893dc70c5`, and `haiderd` SHA-256 `71142ec1c913214633173608f8079de11efa695caa896e040ac52fd5013111cc`. Loads were 1.446/1.490/1.490; the external ledger contains exactly 90 requests.
The accepted confirmation used support SHA-256 `4bcdea7e62732131c7ebdabc1c1c68eabe6da215ee6619919b7aa541b3f12149`. The final corrected support source is `ed4d632d4ea92f6f6e97e46badc5f91c566e034624e2d438da098210efbe9143`: it explicitly selects local `process_exec` instead of substring-matching `ssh_shell` and adds exact harness/source hashes. The same-source rerun was load-refused, so the table above remains the accepted historical baseline and is not used for a final CPU/RSS claim. The current CI source derives its 61.624/101.962 ms budgets from the accepted confirmation medians.

## Final before/after and three-column verdict

| Run | Shape | Wall ms | Combined CPU ms | Peak KiB | Conservative peak KiB | Owner target | CI budget |
| --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| v0.0.968 primary | single | 56.832 +/- 5.712 | 4.691 +/- 0.214 | 27,072 | 27,344 | FAIL | n/a |
| v0.0.969 final trace-off | single | 56.702 +/- 3.474 | 5.453 +/- 0.268 | 57,680 | 58,080 | **FAIL 40 ms** | n/a |
| v0.0.968 primary | tool | 98.101 +/- 9.438 | 5.512 +/- 0.311 | 27,488 | 27,632 | FAIL | n/a |
| v0.0.969 final trace-off | tool | 77.955 +/- 3.868 | 6.768 +/- 0.244 | 58,464 | 58,528 | **FAIL 60 ms** | n/a |

| Retained change | Wall verdict | CPU verdict | Peak-RSS verdict |
| --- | --- | --- | --- |
| Trace-only points, trace disabled | **Owner FAIL.** 56.702/77.955 ms misses 40/60. A final source-exact paired delta is unavailable because the baseline retry was load-refused. | **UNPROVEN final pair.** The accepted candidate is 5.453/6.768 ms, but unlike-source historical baselines are not used for a final regression claim. | **UNPROVEN final pair.** The accepted candidate is 57,680/58,464 KiB with the corrected sampler; the same-source baseline attempt was rejected by load. |
| Test-only journal observer | Inert without its explicit test variables; no warm-path speed claim. | Inert by default. | Inert by default. |
| Ambiguous provider-delivery fail-close | N/A on the warm path: it runs only during restart recovery. | N/A on the warm path. | N/A on the warm path. |
| Python harness and CI check | No product-runtime code. | No product-runtime code. | No product-runtime code. |

The hold-out law is satisfied: this lane retained no claimed speed lever. The proof instrumentation and crash-safety repair are retained for correctness, while the missing final paired CPU/RSS proof and the absolute wall misses both independently keep the lane at NO_SHIP. MAD relaxes neither owner target.

## Opt-in trace result

The accepted pre-final trace-on companion measured 58.299 +/- 2.240 ms single and 103.397 +/- 4.067 ms tool at loads 3.684/3.684/3.789. It pins the historical stage cardinality below, but its old binary/support hashes mean it is not subtracted from the final trace-off batch. A final 1+1 correctness smoke on the final binaries had zero correctness failures and exact transaction joins: 31 SQLite transactions matched 31 session-actor waits (8 single, 23 successful-tool), with 2 accepts/terminals and 3 request-attempt/open/first-byte/stream/SSE points. It was not a publishable timing run because its deliberately reduced sample count and load 5.31 violate the proof pins.

Across the 50 retained turns, the trace has exactly 50 accepts and client terminals, 75 request-attempt/provider-open/first-byte/provider-stream/SSE records, 50 terminal commits, 725 SQLite transaction and matching session-actor journal-wait records, 725 projection fanouts, and 600 core fanouts. The full raw file contains 3,960 allowlisted records including warm-ups. For every retained turn, the multiset of positive transaction ordinals in `journal_transaction` equals the one in `journal_append_wait`; the observed ordinal union is 1..22. Selected measured medians are 871 us for attempt commit, 3,768 us for provider open, 124 us for the SQLite transaction, 173 us for the actor append wait, 4 us for projection fanout, 3,442 us for provider stream, and 11 us for SSE decode.

Daemon phases share one `Instant` anchored at durable acceptance. `client_terminal_seen` deliberately uses a separate client-local monotonic clock anchored when the client receives the accepted coordinate. It is joined by the same numeric turn ordinal; cross-process timestamps are not subtracted.

Trace-off is a cached process-local branch before task-local scope, registry lookup, transaction-map access, or clock reads. Records contain no prompt, IDs, body, or payload: only allowlisted phase and numeric timing/ordinal fields.

## SIGKILL boundary matrix and durability

Fresh final-source discovery enumerated 11 single-request and 27 tool-request journal transactions. The matrix killed and restarted one isolated daemon at all 38 post-transaction boundaries, then added 9 provider transport gates: after the POST was externally recorded, before headers, and between SSE chunks for request 1 and, where applicable, request 2. Result: **47/47 PASS** (14 single, 33 tool; 8 success terminals, 39 failure terminals).

The artifact retains 55 disk-synced physical requests. Within each isolated case the maximum count for any logical request ordinal is 1. Every store integrity check is `ok`; every tool case has at most one physical effect and at most one result, with no result lacking an effect. The matrix validates the committed pre-kill prefix against restart replay, strict durable sequence, a second sealed replay through the same terminal, exactly one typed terminal, and no duplicate physical provider request.

The matrix exposed an unsafe pre-existing admission retry: a durable request-attempt marker proves that transport was admitted, but after `SIGKILL` cannot prove whether its POST reached the provider. Recovery now fails this narrow shape closed with the existing single transactional interrupted terminal instead of reissuing logical ordinal 1.

No durable point was moved or merged in this lane:

- **Journal as truth:** trace points observe existing commit/fanout boundaries. The recovery decision is reduced only from durable journal facts. Its error terminal is appended as one existing transactional batch.
- **Replay parity:** a pause occurs only after the store reports persistence. Live publication may be absent after the kill, but attach/replay repairs it from the journal. All 47 cases matched the durable prefix and sealed terminal projection.
- **Exactly one terminal:** recovery either sees the existing terminal or appends the ordinary typed interrupted terminal. The matrix found exactly one typed terminal in every case.
- **No double provider issue:** an attempt marker without a durable response is ambiguous, so recovery terminalizes instead of reopening the provider. The external ledger has at most one physical row per logical ordinal in every case.
- **Tool effects:** the journal remains authoritative for effect reconciliation. A dispatched-without-outcome boundary uses typed `session recover --abandon`; completed effect facts suppress redispatch. The matrix observed at most one effect/result and no result without an effect.

The observer is test-only, disk-syncs the boundary row before parking, serializes ordinal allocation, requires an absolute ledger whose canonical parent is within `HAIDER_PROFILE_DIR`, and is inert without its test environment. Real `SIGKILL` execution is POSIX-only; Windows behavior is by inspection and the harness reports `ENV_BLOCKED` where `SIGKILL` is unavailable.

## Citation audit

The lens citations were re-resolved against this worktree before implementation:

| Evidence claim | Audit |
| --- | --- |
| Durable JSONL cursor and gap repair | **Correct:** `docs/jsonl-run-contract-v1.md:15-19`. |
| Exactly one typed terminal | **Correct:** `docs/jsonl-run-contract-v1.md:87-123`. |
| Sole durable client cursor | **Drifted:** the current authority is `docs/client-contract-v1.md:925-939`. |
| Replay makes zero provider/tool calls and appends nothing | **Correct at current lines:** `docs/client-contract-v1.md:2718-2727`; durable projection is `:2732-2741`. |
| Exact t0 replay check | **Correct:** `scripts/qa-gate/checks/t0/t0.run.replay_resume_recover.py:244-306`. |
| Forced replay/live boundary and slow-client resume tests | **Correct:** `crates/haider-daemon/tests/session_hub_tests.rs:3415` and `:6140`. |
| Existing real kill check at old `:443` | **Wrong line:** current POSIX capability is `scripts/qa-gate/checks/t1/t1.daemon.kill9_midturn.py:116-123`; the real kill is `:231-250`. It did not sweep transaction boundaries or retain an external provider ledger. |
| Workflow recovery cited at `core_loop_e2e_tests.rs:1858` | **Drifted/insufficient:** `:1858` is a between-stage recovery test. The new no-reissue process test is `:1983-2165`. |
| SQLite blocking dispatch cited around `sqlite_store.rs:2939` | **Drifted:** current `run_blocking` and queue/operation trace are `:2943-2968`. |
| Current transaction trace correlation | **New verified seam:** `sqlite_store.rs:1907-1925,1991-2000` and `session_hub/actor.rs:342-417` share the ordinal keyed by the first durable event ID. |

## Verification

- Guard #77: `scripts/check-unsafe-counts.sh` passed at production=188/test=16 before continuation edits and after the final harness repair. Python self-tests pass **47/47**.
- `python3 -m py_compile` for the support, harness, matrix, and CI check: PASS.
- `cargo fmt --all -- --check` and `git diff --check`: PASS.
- Targeted Rust tests: `turn_trace_transaction_ordinal_is_shared_by_batch_identity`, `first_provider_delivery_is_ambiguous_only_before_any_response_fact`, `kill9_after_provider_admission_fails_closed_without_reissue`, `workflow_recovery_after_budget_admission_fails_closed_without_reissue`, and the safe telemetry allowlist test: PASS.
- Scoped `cargo check` for `haider-provider`, `haider-daemon`, and `haider-cli`: PASS. Final release build completed with one Cargo job and the mandated environment; binary size guard passes.
- Final trace-off report: PASS, 5+25 samples/shape, 90-request ledger, accepted load, same daemon identity, exact cleanup. Its measurement path predates only the closing trace-enabled 24-to-23 transaction-count correction.
- Final trace-on refresh: **ENVIRONMENT-BLOCKED** by load(1m) above 4; the accepted pre-final companion retains exact phase/request/transaction cardinality and allowlist evidence.
- SIGKILL matrix: PASS 47/47, 55-row external ledger, no logical ordinal physically issued twice, exact replay/terminal/store/effect checks.
- Final targeted `t1.turn.wall_budget`: **ENVIRONMENT-BLOCKED** by load. The runnable source and unit pin prove exact confirmation-derived budgets of 61.624/101.962 ms; its earlier passing artifact is historical and has pre-final hashes.
- OAuth-owned files were not touched. The work remains uncommitted for the orchestrator.

NO_SHIP
