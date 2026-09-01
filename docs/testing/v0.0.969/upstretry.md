# Lane upstretry: bounded 429 retry and durable terminal

## Result

The retry ladder now refuses a backoff unless the remaining deadline can contain the delay plus two provider safety margins. The first margin protects the next request-open cutoff; the second protects scheduler handoff and durable terminal delivery. A complete-body HTTP 429 ladder therefore ends through the ordinary durable provider-error path instead of sleeping until the caller or provider deadline kills the run.

Deadline terminalization is also state-based at one actor decision point. An absolute deadline observed while the durable run is `Retrying` or `Waiting { RateLimit | ProviderBackoff }`, or while retry admission is being computed, commits the bounded `provider_error` terminal with `retryable: false` and `allowed_actions: [none]`. The actor switches that volatile admission state to request-in-flight only when the next provider future is actually polled. A deadline from a genuinely active provider request/stream remains `provider_timeout`; timer wake order no longer chooses the retry ladder's durable terminal class.

The fixed real-daemon test makes six physical requests through the production `OpenAiCompatibleProvider`, publishes exactly five deterministic lower-half-jitter waits, commits adjacent `run_failed` and terminal `run_state` envelopes, and replays the same single terminal. The CLI contract test independently exits with `EX_PROVIDER` (65) and exposes that adjacent failure/terminal pair as exactly one typed JSONL terminal.

This investigation did **not** find a 968 merge that introduced a second 429 bound. The admission/terminalization gap exists in the retry code shared by 967 and 968. The reported 967/968 wall difference is consistent with different run-scoped jitter coordinates and an external process timeout, not with a changed retry or route-wait implementation.

## Reproduction and measurements

The regression fixture runs an in-process production daemon over its real UDS protocol and points the real OpenAI-compatible HTTP adapter at a loopback server. The server returns six complete `429 {"error":"transient"}` responses without `Retry-After`, then a valid SSE success response. The test selects a daemon-minted run ID whose first five deterministic waits total no more than 21.5 seconds; this leaves 10.9 seconds for worker startup plus six loopback/store cycles under the explicit 32.4-second caller deadline. The global lower-half minimum through five waits is 15.5 seconds and attempt six waits at least 15 seconds, so attempt six plus both margins needs at least 32.5 seconds to admit a seventh request; the fixture's strict 100 ms gap forbids it.

| Tree / condition | Wall | HTTP opens | Result |
| --- | ---: | ---: | --- |
| peer v0.0.967 evidence | about 20 s | 6 | bounded provider failure |
| peer v0.0.968 evidence | about 32 s | 6 | external/budget termination, no typed terminal |
| local v0.0.968 mutation, deadline admission guard removed | 32.27 s | 6 | observer elapsed with no typed terminal |
| fixed observation 1 | 19.789 s | 6 | durable provider failure |
| fixed observation 2 | 21.178 s | 6 | durable provider failure and sealed replay |
| continuation CLI, old 3.8 s / 4 s boundary | 5.28 s process wall | fake | `provider_timeout` (flaky timing winner) |
| continuation 2 CLI, off-boundary fix, 20 exact reruns | bounded by 4 s run budget | fake | 20/20 durable `provider_error` |
| continuation 3 CLI, state rule + derived 7 s refusal gap | bounded by 10 s run budget | fake | 20/20 quiet and 20/20 with two CPU hogs |
| continuation 2 real e2e, 20 exact reruns | 20.459--25.065 s | 6 each | 20/20 durable provider failure and sealed replay |
| continuation 3 real e2e, 20 exact reruns | bounded by 32.4 s run budget | 6 each | 20/20 durable provider failure and sealed replay |

The two original fixed observations have median 20.484 seconds and MAD 0.695 seconds, matching the supplied 967 wall (about 20 seconds) within the observed MAD. The continuation's 20-run e2e sample had a 22.191-second median and 20.459--25.065-second range across different daemon-minted jitter coordinates.

One representative fixed sequence was `884, 1785, 2721, 6739, 8899` ms. The test derives its expected values from `retry_jittered_backoff_ms(run_id, attempt)` and checks the complete typed sequence as attempts 2 through 6, `max: 10`, `reason: rate_limit`. It also proves:

- exactly six HTTP requests and six `thinking` cycles;
- one `waiting{rate_limit}` for every retry and no route wait;
- no sixth sleep and no hidden auth, web, replay, rotation, or reconnect request;
- `run_failed{code: provider_error, retryable: false, presentation.allowed_actions: [none]}` followed immediately by `run_state{state: errored, terminal_kind: provider_error}`;
- exactly one typed terminal in the live stream and exactly one in sealed replay.

## Historical attribution

I compared the complete `provider_error_allows_retry` source and the retry sleeper across the requested candidate window. The retry function hashes to `a247e8994ff4...` and the sleeper site to `1cd19bf0869c...` at v0.0.967, every candidate below, and v0.0.968.

| Revision | Candidate | 429 retry/sleep result |
| --- | --- | --- |
| v0.0.967 | release baseline | identical retry gate and sleeper |
| `e53f943` | 968 preview/qafix | identical |
| `87e0afe` | int4/resume | identical; new route wait cannot accept `RateLimited` with HTTP status 429 |
| `b0fc75d` | peakrss1 | identical |
| `f12904c` | seamfix | identical |
| `2f994f5` | retainfix | identical |
| `22f6888` | budget-retirement test retrofit | identical production path |
| `ac815e3` | hygiene merge | identical |
| `0afb8b3` | post-merge hygiene pin | identical |
| v0.0.968 | release | identical |

Consequently there is no honest single-commit duration bisect result in that window. The peer evidence already established the more important cross-version invariant: both releases could exhaust the ladder by ending in silence rather than committing the JSONL contract's required typed terminal.

The brief's source citations were checked again after the continuation insertions. `provider_attempt = usize::from(route_wait.is_some())` is now at `actor.rs:3065`; provider-deadline polling is at `actor.rs:6030`; the single terminal classifier is at `actor.rs:9138`/`10828`; retry admission is at `actor.rs:11165`; and the route-wait predicate is at `actor.rs:11260`. The route predicate remains `NetworkUnavailable | StreamInterrupted`, route unavailable, and no provider HTTP status, so a complete 429 body cannot enter it. The 429 mapping, retry constants, lower-half jitter function, and sleeper site are unchanged.

## Root cause and fix

`provider_error_allows_retry` previously applied deadline admission only to the `provider-timeout` subcode. Rate limits and other retryable provider errors returned early. Their retry sleeper is outside `before_provider_request_deadline`, so the actor could publish a retry and sleep even when `delay + terminal reserve` no longer fit the absolute run deadline. Deadline enforcement then happened on the next provider open or in the caller. That bypassed the actor's normal atomic `commit_terminal_error` path, explaining both the long wall and the missing terminal envelopes.

The first lane fix exposed a second race. It admitted a wait whenever `remaining - delay >= PROVIDER_DEADLINE_SAFETY_MARGIN`, but that was the same one-second margin later consumed by `before_provider_request_deadline`. Retry telemetry publication, store work, scheduler handoff, and request reconstruction sit between those decisions. The original CLI pin placed four 700 ms waits plus the one-second margin at 3.8 seconds under a four-second deadline, leaving only 200 ms and also counting daemon bootstrap against the same clock. Under load, either the fifth rate-limit response reached admission first (`provider_error`) or the request deadline reached the next open first (`provider_timeout`). The exact pre-fix contract test reproduced the latter locally, confirming that the two codes described one hopeless ladder and were selected by timing.

The margin-preserving admission fix computes the exact Retry-After delay or deterministic jittered delay for every automatic provider retry and admits only when `remaining - delay` is strictly greater than two `PROVIDER_DEADLINE_SAFETY_MARGIN` intervals. Equality is a refusal. One interval remains the provider request-open margin; the second is the retry-admission reserve that lets a computable refusal reach the durable terminal path before the caller deadline. Refusal latches the accepted run's error as non-retryable and limits its presentation to `ErrorAction::None`; the existing provider-error path then atomically commits `run_failed` plus `Errored`. Exact Retry-After waits are still honored whenever they and both margins fit; no retry constants or attempt caps were shortened.

That early exit is retained, but it is no longer the authority for terminal class. `provider_failure_outcome_with_items` is the one deadline-to-terminal decision point. It consults the durable provider-retry states plus a narrow volatile admission latch that begins before resolver/backoff policy and remains set across the backoff. The latch clears only inside the admitted provider future, immediately before provider code is polled. Therefore a deadline first noticed at the next request boundary after an overrun backoff is still classified from retry state as `provider_error`, while a timeout after provider polling begins remains `provider_timeout`. Retry-state classification also bypasses the daemon's in-flight deadline mapper, so the same `provider_error` fact is committed regardless of which timer wakes first.

## Tests added

- `haider-core`: pure below/equal/above admission-boundary pins plus a rate-limit Retry-After accepted with five seconds remaining and refused with four; equality deterministically refuses.
- `haider-core`: pure terminal-class pins for `Retrying`, both typed provider `Waiting` reasons, retry admission, in-flight thinking/streaming, and the pre-existing network-route wait class.
- `haider-core`: paused-time actor law that enters a real durable rate-limit backoff, advances beyond the run deadline, releases the backoff, proves a second provider request never opens, and observes exactly one non-retryable `provider_error` terminal.
- `haider-daemond`: real daemon, UDS, real HTTP adapter, six complete 429 bodies, exact jitter ladder, wall/request/wait assertions, atomic terminal assertions, and sealed replay.
- `haider-cli`: JSONL retry exhaustion with a derived ten-second deadline, exactly one typed provider terminal, terminal last, adjacent failure, and exit 65. Its first fifteen-second Retry-After plus two one-second margins needs seventeen seconds, leaving a seven-second refusal gap that cannot drift onto the request-open cutoff under realistic CI load.

Registry #94 deadlines are derived in the fixtures: the CLI's fifteen-second Retry-After plus two one-second provider margins requires seventeen seconds, exactly seven seconds more than its ten-second run budget; the paused-time actor law admits 500 ms plus both margins inside three seconds before advancing past the deadline; the daemon's 32.4-second caller deadline stays 100 ms below the 32.5-second global minimum for six waits plus both margins, and its observer adds two seconds of local terminal-publication grace; replay reuses that local grace; selection cancellation derives five seconds from settlement plus two request/reply store-publication allowances.

## Verification

All commands used `RUST_MIN_STACK=8388608`, `HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`, `CARGO_INCREMENTAL=0`, and `CARGO_PROFILE_DEV_DEBUG=0`. Daemon tests used prebuilt siblings after building both binaries. Only one Cargo job ran at a time, and disk was checked before builds (availability remained above 15,995 MiB throughout this continuation).

- named core deadline-margin test: pass;
- named core state-terminal classifier: pass;
- named paused-time backoff-expiry actor law: pass;
- named CLI JSONL/exit-65 test: 20/20 quiet exact reruns and 20/20 exact reruns while two `yes > /dev/null` CPU hogs ran;
- named real-daemon 429 e2e: 20/20 exact reruns passed, each including sealed-replay parity and exactly one typed terminal in both live and replay streams;
- `cargo test -p haider-core actor_request_attempt_tests --locked`: 10 passed;
- `cargo test -p haider-core --locked`: full package passed (one pre-existing manual benchmark ignored);
- `cargo test -p haider-cli --locked`: full package passed, including 118 CLI tests;
- `cargo test -p haider-daemond --locked`: full package passed, including the real e2e and sealed replay;
- `cargo clippy -p haider-core -p haider-daemond -p haider-cli --all-targets --locked -- -D warnings`: pass;
- `cargo run -p xtask --locked -- check`: pass, 665 files and 4,329 tests versus baseline 4,324; only the nine existing soft LOC warnings;
- `cargo fmt --all -- --check`, `git diff --check`, conflict-marker scan, and unmerged-index check: pass;
- `bash scripts/check-unsafe-counts.sh`: pass, production 188 and test 16;
- final sibling sizes: `haider` 102,972,016 bytes and `haiderd` 184,655,200 bytes, both over the 10 MiB guard.

## CI error-registry audit

- Public/wire/error schemas and public signatures are unchanged (#1, #2, #6, #39); all affected all-target builds and package tests compile.
- The change is platform-neutral and adds no process or `cfg` behavior (#5, #28, #37); Windows behavior was inspected statically.
- No manifest, lockfile, schema, or migration changed (#7, #23, #34, #76).
- The deliberate single-guard mutation reproduced the failure, was immediately restored, and the final diff was checked (#8).
- All-target Clippy with warnings denied passed (#9, #10 and related lint guards).
- The xtask test census increased from 4,324 to 4,329 and all Cargo jobs used the required 8 MiB stack (#20, #21, #54).
- The regression proof crosses the production daemon, UDS, durable store/replay, provider adapter, and real loopback HTTP rather than a source-only seam (#30, #49, #61, #71).
- Unsafe counts and final repository-integrity guards passed (#45, #77).
- Both sibling binaries were explicitly prebuilt and size-checked before daemon execution (#64, #67).
- Discovery was disabled and hermetic test profiles were used (#72, #74).
- Every new test wall deadline has arithmetic tied to the production contract (#94).
- The e2e continuously consumes the negotiated connection and sealed replay has a bounded local-store deadline; it does not wait on unobserved external state (#95).

Territory remained limited to the actor retry admission/terminal classification, its core unit and retry runtime tests, a daemon regression target, this CLI contract test, and this report. No OAuth or hook-owned source was touched. The lane remains intentionally uncommitted and was not rebased, per the lane continuation rules.

SHIP
