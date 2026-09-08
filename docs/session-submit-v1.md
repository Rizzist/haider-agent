# Submit another turn to a native session

Create the initial session and obtain its native identity from JSON:

```sh
haider run --output json "Remember this project's constraints"
haider run --session <session_id> --output json "Apply the next change"
printf '%s\n' 'Inspect the result' | haider session submit <session_id> --output json -
```

`haider session submit <id> <prompt>` is an alias of `haider run --session <id>
<prompt>`. Both accept `-` for UTF-8 stdin, attachments, `--timeout`,
`--trust-hooks`, and `--output print|json|jsonl` (including `--json`/`--jsonl`).
Use the same profile (`HAIDER_PROFILE_DIR`) as the initial invocation. The ID
must be the daemon's native session ID, not an external harness's session key.
A stopped daemon may be auto-started against that profile's durable store.

Successful `--output json` returns the existing `haider.run.v1` document:

```json
{"schema":"haider.run.v1","session_id":"<stable native session>","run_id":"<new native run>","turn_id":"<same new native run>","outcome":"done"}
```

This is a field excerpt; the complete document also includes response, events,
usage and error details. `turn_id` is an additive alias for `run_id`: Haider
identifies an ordinary turn by its native run ID and does not mint an independent
turn ID. Each submission creates a new run, while `session_id` stays unchanged.
The result's `events` contains only the new run's correlated records. Their
sequence numbers belong to the existing session journal and continue from its
committed history. Session-scoped history remains available through the normal
session observation/export and RPC `session.read` surfaces. Existing-session JSONL emits only the new run's correlated envelopes and retains
the existing envelope format. Use envelope `session_id` and `run_id` for correlation.

The existing session's workspace, provider/model, tuning, permissions and request
ceiling remain authoritative. The caller's current directory does not replace
the session's workspace. Explicit creation/configuration flags and run-budget
pins (`--provider`, `--model`, `--effort`, `--speed`, `--account`, `--ssh-scope`,
permission overrides, `--seed`, and budget flags) are rejected with `--session`.
Use session configuration commands to change durable session configuration.
`--start`, `--status`, `--stop`, `--replay` and `--resume` remain separate run
lifecycle commands. `--timeout` bounds this client's operation and retains the
normal cancellation/drain behavior.

Ordinary submission does not synthesize a recovery prompt or consume a budget
continuation handle. `haider run --resume <run_id>` still requires a durable
budget continuation; `haider resume <session_id> --json` remains the recovery
barrier and accepts no prompt. A currently active session uses the existing
`DeliveryMode::Queue` admission semantics.

Before acceptance, machine output includes an error object and null run/turn
IDs. Unknown native sessions return `error.code = "session_not_found"`;
sessions fenced by the daemon's close/delete lifecycle return `"session_closed"`.
Both are non-retryable and exit nonzero. In this candidate, deletion is the
closed-session lifecycle: its in-process admission fence distinguishes a closed
session from an unknown one. Once a deleted journal is gone and the daemon has
restarted, it is reported as unknown. No deleted session is recreated.

## RPC and client binding

No RPC method name or wire shape is added. A controller reads `session.read`,
attaches with `session.attach` in `control` mode, waits for `AttachCaughtUp`, then
uses existing `turn.submit` (or `turn.submit_with_hook_trust`). Use a fresh
`command_id` for each intended turn, the attachment's `worker_generation`, the
same `session_id`, and `mode: "queue"`. Retrying the same command must retain its
original coordinates for the daemon's durable receipt check. The existing
`turn.submit` response supplies the new `run_id` and `accepted_seq`.

Rust callers can use
`haider_client::submit_headless_with_event_mode_and_interrupts(profile, ensure,
request, session_id, output, event_mode, interrupts)`. Its `HeadlessRunRequest`
requires absent provider/model, empty permission overrides and budgets, and no
journal pin, detached start, seed or replay source. The prompt and attachments
are submitted unchanged. The session determines workspace and output-token
configuration. The common reducer replays session facts, correlates by the new
run ID and retains normal reconnect/receipt recovery.

This surface enables an external storage harness to reuse a native session.
It does not by itself establish S1–S10 or matrix row 52 benchmark outcomes.
