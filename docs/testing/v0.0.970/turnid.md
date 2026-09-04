# v0.0.970 turnid — implementation and verification

## Outcome

Branch `lane-970-turnid`, working-tree base/HEAD
`15ead186a728629be9b7071c910d59dbd40bcb5b`.

Every turn- or durable session-inference-owned outbound provider HTTP request
now carries the locked `X-Haider-Turn` and `X-Haider-Request-Kind` headers. The
same coordinates are committed before provider I/O in a request-attempt
journal marker and are emitted by `HAIDER_DAEMON_TRACE=1`. Provider bodies are
unchanged unless the adapter both declares metadata support and the operator
opts in; native OpenAI Responses is the only built-in adapter declaring that
support.

## Contract and implementation

- `X-Haider-Turn` is exactly
  `<session_id>/<run_id>/<turn_ordinal>/<request_ordinal>`. Both ordinals are
  unpadded, unsigned, nonzero integers; turn ordinals are session-monotonic and
  request ordinals are physical-attempt-monotonic within one run.
- `X-Haider-Request-Kind` is exactly lowercase `primary`, `side`, or explicit
  unmeasured `warmup`. Root turn continuations are primary. Delegation,
  compaction/summarization/estimation, Loom drafting, Gemini cache-resource
  work, and provider-facing tool support are side.
- All built-in OpenAI native/compatible, Anthropic, and Gemini request builders
  obtain headers from the shared provider scope. Retried physical sends obtain
  the next ordinal rather than reusing a coordinate.
- `cache_request_attempt_v1.correlation` carries model-request coordinates;
  the prompt-omitted `provider_request_attempt_v1` extension carries auxiliary
  coordinates. Strict provider JSON bodies do not receive correlation fields.
  `HAIDER_PROVIDER_BODY_METADATA=1` mirrors `haider_turn` and
  `haider_request_kind` only for native OpenAI Responses.
- Recovery separates the first logical model-attempt boundary from the maximum
  physical request ordinal. Warmup/cache/tool-support attempts can precede a
  model attempt without making recovery reuse an identity. Queued turns,
  manual retry, compaction, streaming checkpoints, and restart handoff restore
  the validated maximum plus one.
- Loom drafting commits additive raw payload
  `provider_operation_reserved { request_kind: side }` to reserve its durable
  session ordinal, then commits request attempt `/1` before inference. This is
  not a conversation `RunState`: observe, hooks, and usage timing exclude it.
  Forks retain the source audit facts but omit the entire reserved operation
  run from the child, preventing embedded parent coordinates from poisoning
  child recovery.
- Out-of-turn control-plane traffic with no durable session/run owner—catalog
  reads, credential validation, and unowned cache cleanup—remains outside the
  contract. ACP uses supervised stdio JSON-RPC, not provider HTTP.

The public contract is in `docs/automation-contract-v1.md` section 9. The
additive marker, reservation payload, compatibility, fork, and recovery rules
are recorded in `docs/event-schema-changelog.md`.

## Verification

- `cargo check --workspace --all-targets`: PASS.
- `cargo run -p xtask --locked -- test-count`: PASS, 4,764 tests against the
  reviewed 4,764 baseline (16 new pins over 4,748).
- `bash scripts/qa-gate/run.sh test`: PASS, 65/65.
- Provider suite: PASS, 260 tests. Real loopback fake-proxy ledgers pin exact
  headers for OpenAI native/compatible, Anthropic, and Gemini, and compare the
  captured JSON bytes to unchanged-body goldens.
- Named daemon/store pins cover primary continuation, delegation side kind,
  tool-support side kind, explicitly enabled warmup, Gemini cache operations,
  Loom drafting, journal/trace/proxy coordinate equality, request retries,
  queued/retry/compaction/restart ordinal restoration, malformed/mixed marker
  rejection, and prompt-sniff removal.
- Loom isolation: `provider_operation_reservation` filter PASS (3/3): no
  observe replacement, hook classification, or agent metrics.
- Fork/restart isolation:
  `fork_after_provider_operation_omits_parent_correlation_before_child_restart`
  PASS; the parent retains its audit, the child contains no operation envelope,
  and reopening the child reconstructs no parent operation ordinal.
- `authoring_rpc_registers_and_executes_each_confirmed_hash`: PASS with exact
  Loom side marker and store ordinal.
- `cargo fmt --all -- --check` and `git diff --check`: PASS.
- Protected `crates/haider-daemon/src/oauth.rs` and `oauth_tests.rs`: unchanged.
  `crates/haider-tui` is also unchanged. The unsafe-count gate still reports
  the pre-existing, untouched `haider-tui` test mismatch (baseline 0, actual
  4); its baseline was not weakened.

Final binaries used for measurement:

| Binary | Bytes | SHA-256 |
|---|---:|---|
| `target/release/haider` | 34,319,760 | `c331a82c64564bb2518900d6c373ac9ae9b81e1007676ec2da9adaeeaa49db72` |
| `target/release/haiderd` | 54,125,408 | `352d3c4025070ede1988f1c4c0a484cb6aca288dbc71432961de94e0cc536fbb` |

`haiderd` remains above the registry #64 10 MiB floor.

## Performance

The authoritative comparison used release artifacts because the supplied
v0.0.969 baseline is explicitly a release build. Each accepted block used one
warmed daemon, five unreported warmups and 25 measured samples per shape in
ABBA order, trace off for wall authority, and start/midpoint/end load strictly
below 3.

| Shape | v0.0.969 baseline | v0.0.970 turnid | Delta | Baseline MAD gate |
|---|---:|---:|---:|---|
| Single | 54.3 ± 2.7 ms | 40.636 ± 3.067 ms | -13.664 ms | PASS; no regression |
| Tool | 73.2 ± 4.3 ms | 61.080 ± 4.119 ms | -12.120 ms | PASS; no regression |

The trace-off block was accepted and passed with load 2.178/2.324/2.324,
daemon PID 71343 and generation 1 unchanged, clean daemon shutdown, zero
correctness failures, exactly one provider request per single case and two per
tool case, and 90 exact proxy-ledger rows across warmups and measurements.
Tracing was disabled and emitted zero trace records. Evidence:
`/tmp/turnid-release-current.json`, SHA-256
`be768806dad9193797c5650041475dd1e30935e05b46573a926a371db3180407`.

The required trace-on companion was independently accepted and passed at load
2.387/2.275/2.275 with stable PID 71649/generation 1, clean shutdown, the same
exact request cardinality and coordinate joins, zero correctness failures, and
4,141 total trace records. All 451 request-scoped trace records joined exactly
across 90 provider IDs; the 60 client-terminal records intentionally retain
their separate legacy client correlation. Its diagnostic medians were 43.677 ±
1.718 ms single and 75.039 ± 3.941 ms tool; trace-on wall is not used as the
performance authority. Evidence: `/tmp/turnid-release-trace.json`, SHA-256
`9737f21f1aba9b0fc06b45389b245136fab45abc1559ba75a0bb9a06507c5e25`.

An initial `target/debug` diagnostic was excluded from the baseline comparison
because it did not match the baseline's optimization profile. It did not alter
the release artifacts or the accepted release evidence above.

## Scope and lane reconciliation

The provider/protocol/cache paths are this lane's direct territory. The
smallest necessary cross-lane touches were made in worker/runtime,
session-hub/actor/RPC, delegation, recovery, usage projection, fork projection,
and their tests. They are required to allocate one durable session ordinal,
share one physical-attempt allocator across model and auxiliary calls, restore
that allocator across queued/retry/restart/compaction boundaries, and keep the
non-conversation Loom operation out of observe/hooks/metrics/fork recovery.
No unrelated behavior or timeout was changed.

The supplied `LANE-COMMON.md`, `LANE-BRIEF-turnid.md`, `turnperf/`, and
`turnperf2/` evidence remains untracked and uncommitted. This implementation
and report are intentionally uncommitted for orchestrator ownership.

SHIP
