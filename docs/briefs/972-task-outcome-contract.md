# Typed task failure (M2, V1)

Decision: a completed provider response is successful runtime completion.
Assistant text such as `{"status":"FAILURE","category":"scripted"}` remains
text and ends as `done`, exit **0**, subject to the existing finalization guards.
Haider does not parse prose, JSON in assistant text, or arbitrary tool output
to decide task success or failure.

Models can explicitly end an unsuccessful task through the existing provider
tool-call surface. The actor-owned `task_outcome` tool accepts:

```json
{"status":"failure","reason":"Required input is unavailable"}
```

V1 supports **failure only**. Success uses ordinary provider completion so that
todo, workflow, and other finalization guards still run. The signal reports the
model's assessment; it does not independently verify effects or undo work.
The reason must be nonblank, contain no control characters, and fit in 1,024
UTF-8 bytes. Unknown fields and other status values are rejected. The model
cannot choose the process exit code.

An accepted signal immediately ends the current run without a further model
request. It must be a standalone call with no other open or deferred tool
calls. Invalid arguments or pending tools produce an ordinary rejected tool
result, permitting correction. Existing cancellation and latched runtime
failure take precedence. A task outcome never grants permissions: the tool
must fit the actor's advertised ceiling. This change adds it to the ordinary
tool catalog and discovery core, without expanding restricted grants or the
Android standalone tool allowlist.

The accepted tool result, completed tool item, tree node, adjacent
`run_failed(code="task_failed", retryable=false)` and `run_state(errored)`
share one durable append. The terminal payload additionally carries:

```json
{
  "task_outcome_version": 1,
  "task_outcome": {"status":"failure","reason":"Required input is unavailable"}
}
```

`haider run` returns exit **1** for `task_failed`. JSON output includes
`error.code="task_failed"`; JSONL and journal replay preserve the typed
terminal and its correlated run identity. Runtime/provider/storage failures
retain their existing exit mappings. Rejected calls do not create a task
terminal. If the terminal append fails, no accepted signal is persisted and
normal storage-error cleanup applies.

`session.observe` and `haider session <id> --json` expose the same optional
`task_outcome` and `task_outcome_version` fields for the selected run. They
come from its committed errored terminal, correlated with `run_id` and the
active branch. Live observation and journal reconstruction after restart use
the same reduction, including terminals committed in older worker generations.
Selecting a new run clears the previous outcome. Ordinary completion, legacy
terminals without the metadata, and metadata-only reads omit both fields;
unknown versions are not interpreted as V1 outcomes.

Compatibility: this uses existing ToolCall/ToolResult, Item, RunFailed and
RunState carriers. `TaskOutcomeV1` and `task_failed` are additive, and the
terminal metadata is versioned. Older readers ignore the metadata and decode
the new error code as `unknown`, retaining the errored state (their numeric
exit need not be 1). No RPC request method, schema version, fsync policy or
existing fixture is replaced; the 136-method pin remains unchanged.

AHRB row 10's historical free-text fixture still represents an unsupported
textual contract. It must emit the typed tool call to test this contract.
This implementation does not retrospectively turn that historical FAIL into
PASS; a new evaluated typed-signal fixture is required.

Regression coverage: actor tests exercise literal FAILURE text, atomic typed
failure, rejected/unadvertised calls and injected terminal-store failure;
tool tests cover strict byte bounds; the real CLI regression exercises both
exit paths with an isolated fake-provider daemon.
