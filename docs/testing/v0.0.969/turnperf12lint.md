# turnperf12lint — wave-969 Clippy and CU2 gate repair

Date: 2026-09-01  
Base: `origin/wave-969` / `eddd42f`  
Integration under audit: `a4dc70c` (trace port authored on first parent `a7bdce1`)

## Verdict

The four provider SSE functions now carry route policy and per-turn trace coordinates in one request-local `SseRequestContext`. Every affected signature is exactly seven arguments, every production trace capture remains before `tokio::spawn`, and no lint suppression was added. The exact workspace/all-target Clippy gate and the full provider, core, and daemon suites pass.

The macOS CU2 failure did not reproduce. The named test passed 10/10 in separate pre-edit invocations and passed again inside the post-edit full daemon suite. Static path analysis also rules out a trace-port, recovery, or derived product-deadline interaction. The prior failure is adjudicated as a load/scheduler flake in an old fixed-time test observation loop; no CU2 product or timeout change is justified by the evidence.

## Guard #77 and starting state

Before any product edit:

```text
bash scripts/check-unsafe-counts.sh
unsafe-count gate: PASS production=189 test=16
```

The branch and base were exact, and the only initial untracked paths were the supplied `LANE-COMMON.md`, `LANE-BRIEF-turnperf12lint.md`, and `turnperf/` evidence. Those supplied files were read first and were not edited. OAuth-owned files were not touched.

After all product and report edits, the same guard passed again at production 189 / test 16.

## Citation and provenance audit

| Brief citation or claim | Audit |
| --- | --- |
| Branch base `eddd42f` | **Correct.** `HEAD`, `origin/wave-969`, and `wave-969` all resolved to `eddd42f` before edits. |
| `anthropic.rs:1545` | **Correct before the repair:** `stream_response` had 8 arguments. It is now at `:1543` with 7. |
| `anthropic.rs:1608` | **Correct before the repair:** `stream_sse_source_with_native` had 8 arguments. It is now at `:1603` with 7. |
| `openai.rs:2220` | **Correct before and after line placement:** `stream_response` had 8 arguments and now has 7. |
| `openai.rs:2261` | **Correct before the repair:** `stream_sse_source` had 8 arguments. It is now at `:2258` with 7. |
| `cu2_computer_runtime_tests.rs:522` | **Correct:** this is the exact timeout panic. The named test itself has drifted to `:1125`. |
| Trace arguments came from `a4dc70c` | **Correct integration provenance:** the merge introduced them to wave-969; direct blame identifies first parent `a7bdce1` as their authoring commit. |
| Common-file base `8952219` | **Stale for this lane.** The lane-specific brief and actual branch agree on `eddd42f` and are authoritative. |

## Provider repair

`crates/haider-provider/src/lib.rs` now defines the crate-private carrier:

```text
SseRequestContext {
    route_gating,
    turn_trace,
}
```

`SseRequestContext::capture` reads the current task-local trace coordinates before the decoder producer is spawned. This ordering matters because Tokio task-local values do not propagate to a new task. The carrier then travels unchanged through the response wrapper into the SSE source, where it is destructured into the same route and trace values used before the repair.

The production captures are at `anthropic.rs:1019` and `openai.rs:2080`, both immediately before `tokio::spawn`. The four repaired functions are at `anthropic.rs:1543`, `anthropic.rs:1603`, `openai.rs:2219`, and `openai.rs:2258`; each has seven arguments. The OpenAI direct source tests now construct the same carrier with an explicit absent trace. The Anthropic test wrapper retains its six-argument test API and constructs the carrier internally. `TurnRequest`, provider wire bytes, timeout accounting, route gating, and trace fields are unchanged.

No `#[allow(clippy::too_many_arguments)]` or other lint suppression was added.

## CU2 10x evidence and adjudication

Each repetition used a separate Cargo test process with the mandated environment:

```text
RUST_MIN_STACK=8388608 \
HAIDER_DISCOVERY_DISABLED=1 \
HAIDER_TEST_DEVICE_NAME=test-mac \
CARGO_INCREMENTAL=0 \
CARGO_PROFILE_DEV_DEBUG=0 \
HAIDER_TEST_SIBLINGS_PREBUILT=1 \
cargo test -p haider-daemon --locked --lib \
  worker::cu2_computer_runtime_tests::screenshot_reaches_provider_click_journals_control_and_viewport_is_post_cu1 \
  -- --exact --test-threads=1
```

Result: **10/10 PASS**. Cargo-reported test durations were:

```text
1.77, 1.78, 1.85, 1.98, 1.84, 1.85, 2.02, 2.12, 2.07, 2.05 seconds
```

Minimum 1.77 seconds, maximum 2.12 seconds, mean 1.933 seconds. The first shell invocation took 279 seconds only because it performed the clean non-incremental build and test-binary link; the test itself took 1.77 seconds. Warm invocations took 2–3 seconds wall. No repetition emitted a failure artifact. The same test later passed again during the full daemon suite at `--test-threads=4`.

The semantic path is deterministic:

1. Fake provider request 4 ends the turn; core consults the daemon graph-finalization guard.
2. The store persists `GraphFinalizationDeferred`, and core sends the required continuation.
3. Fake provider request 5 reaches the same unfinished state; the store persists `GraphAbandonConfirm` as `MenuOpened`.
4. The test resolves `abandon-and-finish`; the graph is abandoned, authority is re-consulted, and the run reaches `Done`.

The suspected trace, recovery, and request-deadline changes are inactive on this path:

- `HAIDER_DAEMON_TRACE` was unset, so the cached trace gate is false and no trace context, registry entry, clock read, or emission exists for the turn.
- The test uses `FakeProvider`; it never enters the Anthropic or OpenAI SSE functions repaired in this lane.
- The test performs a normal interactive submit with no recovery API or restart path.
- Interactive session setup has no headless request deadline, so recent request-deadline classification is inactive.
- The integration also contains the wave's incremental `GraphReductions` cache, which is active here. It is full-reduction-equivalent, was not changed by this lane, and remained deterministic across the isolated and full-suite repetitions.
- The 12-second timeout at helper lines 485–510 is test-only, predates the integration, and repeatedly calls `store.read` through `spawn_blocking` plus the store mutex. It is sensitive to full-suite/host contention, but it is not a production graph-finalization deadline.

The maximum isolated execution used only 2.12 of those 12 seconds, and the full daemon suite also passed. No payload tail from the original failed CI run was available beyond the panic text, so this is a nonreproducing contention-flake adjudication rather than a payload-specific diagnosis. It is not evidence for a real trace, recovery, graph, or deadline defect. Registry #94 does not call for a change: this lane adds no deadline, and there is no demonstrated product wait requiring a derived outer bound.

## Verification

All builds used:

```text
RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
```

Daemon tests additionally used `HAIDER_TEST_SIBLINGS_PREBUILT=1` and four test threads. Disk was checked before every build and remained far above the 700 MiB stop threshold.

| Command or proof | Result |
| --- | --- |
| `cargo fmt --all -- --check` | PASS |
| `git diff --check` | PASS |
| `cargo clippy -p haider-provider --all-targets --locked -- -D warnings` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | **PASS**, exact requested gate |
| `cargo test -p haider-provider --locked` | PASS: 348 passed, 7 explicitly gated live tests ignored |
| `cargo test -p haider-core --locked` | PASS: 229 passed, 1 manual timing test ignored |
| `cargo test --no-fail-fast -p haider-daemon --locked -- --test-threads=4` | PASS: 1,018 passed, 3 live/manual tests ignored |
| CU2 named test, 10 separate exact invocations | PASS 10/10; 1.77–2.12 seconds |
| CU2 named test inside full daemon suite | PASS; eleventh local pass |
| Prebuilt sibling binaries | PASS |
| Registry #64 daemon identity floor | PASS: `target/debug/haiderd` = 185,049,104 bytes, greater than 10 MiB |

## Independent verification

- The provider/code verifier independently audited all four signatures and call sites, confirmed capture-before-spawn and unchanged route/trace ownership, reran provider all-target Clippy with warnings denied, and returned **SHIP**.
- The CU2 verifier independently inspected the finalization, trace, recovery, deadline, and registry surfaces. It then ran the existing exact daemon test binary 10 additional times: **10/10 PASS**, 1.74–2.05 seconds. It returned **SHIP** after the registry/cache wording refinements incorporated here.

## CI error registry walk

The full #1–#96 registry was reviewed. Items not named below were checked with no affected surface in this provider-only signature repair.

| Registry item | Result |
| --- | --- |
| #1 | The new `SseRequestContext` is integrated at every construction and consumption site; full compilation, suites, and Clippy pass. |
| #2 / #12 | All four Rust function signatures and every caller were migrated together; provider, core, and daemon API consumers compile and pass. |
| #3 | The owned carrier moves once into the spawned producer and is destructured once; route and trace ownership are neither cloned nor dropped early. |
| #4 | The carrier and constructor visibility are crate-private only; no public API or wire contract changes. |
| #7 | No dependency or lockfile change. |
| #9 | The exact all-target Clippy surface passes with `-D warnings`; no lint allow was added. |
| #10 | No dead helper: provider and workspace all-target Clippy pass with warnings denied. |
| #14 | The new `Debug` derive adds no formatting or logging call site; no new value is emitted by production code. |
| #19 | Rust formatting and diff whitespace checks pass. |
| #20 | No test was added, removed, ignored, or weakened. Existing call sites were migrated to the carrier. |
| #21 / #54 | Every build and Rust test used `RUST_MIN_STACK=8388608`; stack-sensitive full suites pass. |
| #22 | Trace content/allowlist is unchanged. The carrier holds only the existing route enum and content-free numeric trace coordinates. |
| #45 / #77 | No unsafe block was added. Guard #77 passed before edits and at closeout at production 189 / test 16. |
| #64 | Fresh `haiderd` is 185,049,104 bytes and passes the 10 MiB floor. |
| #67 | Fresh `haider` and `haiderd` siblings were prebuilt before daemon tests; subprocess recursion through Cargo was disabled. |
| #72 | All hermetic tests ran with native discovery disabled. |
| #73 | No fixed-byte source window or generated-code assumption was added. |
| #94 | No deadline was added or changed. The old CU2 test-only 12-second observation was audited and is not a product deadline interaction. |
| #95 | No negotiated connection or external-state wait changed. |
| #96 | Turn-wall harness, timing acceptance, and performance budgets are untouched. |

The working tree remains uncommitted for the orchestrator.

SHIP
