# Lane 969 tool-argument rejection

## Provenance and guard

The branch is `lane-969-toolargs` at the requested confirmed-defect base
`d75a8ea`. The requested `969-common.md` is not present in the worktree, any
local ref, `/Users/rizzist/haider-run`, or the searched temporary roots; work
therefore followed the explicit lane contract in the task statement. Before
any product edit, Guard #77 ran as `bash scripts/check-unsafe-counts.sh` and
passed with `production=188` and `test=16`.

## Result

Daemon-owned model argument parsers now return `ToolError::InvalidArgument`.
`BrokerToolDispatcher` parses cached operations once at model-call entry and
routes that typed error through `typed_error_result` as a completed
`ToolResultStatus::Rejected` result with `error.kind == "invalid_argument"`.
The message retains the parser's field name. The core actor durably closes the
tool item/result, sends that result back to the provider, and continues the
same turn.

The same boundary now covers the other daemon-owned model argument decoders:
`monitor`, `workflow_author`, `message_subagent`, `loom_register`,
`task_output`, and `web_search`. Computer/mobile and graph-evidence argument
errors already settled through their typed result paths. Permission-menu wake
validation, RPC/delivery arguments, durable-state validation, attachment
validation, and daemon configuration remain turn-scope errors.

## `InvalidArgument` audit

Command used after the fix:

```text
rg -n 'ErrorCode::InvalidArgument' crates/haider-daemon/src/worker.rs
```

Every remaining hit is classified below. “Model-caused” means the value was
authored in a model tool call; matches in that class are already converted to
a tool result and do not escape as a turn-scope `HaiderError`.

| Current site(s) | Source/classification | Disposition |
| --- | --- | --- |
| `3833` | Non-model: legacy-session RPC/state lacks live-worker metadata. | Turn-scope `InvalidArgument` retained. |
| `5040` | Non-model: client delivery mode attempts queue input against an active harness. | RPC/delivery argument error retained. |
| `7123` | Non-model: accepted session metadata disappeared before turn start. | Turn-scope state error retained. |
| `7828` | Non-model: daemon-resolved reserved-output configuration exceeds the model window. | Turn-scope configuration error retained. |
| `8978` | Non-model: user attachment PDF extraction/admission failure. | Turn-scope attachment error retained with presentation. |
| `9073`, `9094`, `9117`, `9154`, `9269`, `9312`, `9326` | Non-model: durable user attachment bytes, provider capability, or reserved skill-attachment validation. | Turn-scope attachment errors retained. |
| `11328`, `11340` | Non-model at the only worker call site: peer inventory discovery/service validation, not `peer_send` model-address parsing. | Turn-scope peer service error retained. Model-authored `peer_send` field shapes are handled by the typed parser. |
| `12428` | Non-model: typed-agent registry/install dispatch refusal selected by daemon workflow state. | Turn-scope selection error retained (`Busy` while install is pending). |
| `12526` | Non-model: durable workflow activation evidence exceeds the executor input boundary. | Turn-scope workflow-state error retained. |
| `16160` | Model-caused graph-evidence argument decode; this is an error-code value passed to `graph_evidence_rejection`, not a `HaiderError` constructor. | Existing rejected `tool_result`; continuable. |
| `16254` | Model-caused graph-evidence semantic validation returned by the hub. | Explicitly caught and converted to `graph_evidence_rejection`; continuable. |
| `16415` | Model-caused spawn recursion request at the daemon policy ceiling. | Explicitly caught and converted to `recursion_limit_result`; continuable. |
| `16664` | Model-caused `message_subagent` target that is not an owned child. | Explicitly caught and converted to a rejected `tool_result`; continuable. |
| `16730` | Non-model: SSH secret storage is unavailable on this daemon. | Turn-scope capability/configuration error retained. |
| `17302`, `17360` | Model-caused Loom source/record rejected by registry semantic validation. | Explicitly caught and returned as a tool refusal result; continuable. |
| `18311` | Non-model: permission/menu wake answer cannot decode as the sole Retry option. | Turn-scope permission-wake argument error retained, as required. |
| `20079` | Non-model: store rejects a replayed daemon-derived image event id as duplicate. | Explicitly treated as idempotent success; no run failure. |

The former model-caused constructors in `parse_tool_operation` and its
`required_*` / `optional_*` helpers no longer appear in this grep: those
functions return `ToolError::InvalidArgument`. The former “general match did
not dispatch this route” `InvalidArgument` was also reclassified to `Internal`
because reaching that arm is a daemon invariant failure, not model input.

## Pins and contract

- Parser unit: exact `fs_read` arguments `{"message":"..."}` fail on `path`
  and map to rejected/`invalid_argument` through the production conversion.
- Real-daemon CLI e2e: the fake provider emits that exact call, requires its
  correlated tool result on the second provider request, emits a corrective
  answer, and reaches one `done`/`success` terminal with no `run_failed`.
- `docs/jsonl-run-contract-v1.md` documents the existing carrier behavior.
  No JSONL field, payload kind, sequence rule, or terminal kind changed.
- `docs/event-schema-changelog.md` records v0.0.969 as a behavioral
  clarification with no automation schema change.

## Related design candidate — not implemented

Malformed tool argument JSON remains distinct. At
`crates/haider-core/src/actor.rs:4952-4978`, core calls
`close_malformed_tool_failure`, durably emits a failed tool result, and then
terminalizes with `provider_error`. A future design may consider making that
provider-protocol failure continuable too, but this lane does not change or
promise that behavior; doing so needs an explicit compatibility and provider
fault-classification decision.

## Verification

- `bash scripts/check-unsafe-counts.sh` — passed before edits and in the final
  closing pass: `production=188`, `test=16`.
- `cargo check -p haider-daemon --locked` — passed.
- `RUST_MIN_STACK=8388608 cargo test -p haider-daemon --locked
  parser_missing_required_path_becomes_typed_rejected_tool_result` — passed,
  1 selected test.
- `cargo build -p haider-cli -p haider-daemond --bins --locked` — passed and
  produced fresh sibling binaries before the subprocess pin.
- `HAIDER_TEST_SIBLINGS_PREBUILT=1 RUST_MIN_STACK=8388608 cargo test -p
  haider-cli --locked --test cli_tests
  run_model_tool_argument_shape_error_is_rejected_and_continues -- --exact` —
  passed, 1 selected real-daemon test.
- `RUST_MIN_STACK=8388608 cargo test -p haider-protocol --locked
  every_current_automation_kind_is_pinned_in_the_schema_changelog -- --exact`
  — passed, 1 selected changelog test.
- `cargo clippy -p haider-daemon -p haider-cli --all-targets --locked -- -D
  warnings` — passed.
- `cargo fmt --all -- --check` and `git diff --check` — passed.

SHIP
