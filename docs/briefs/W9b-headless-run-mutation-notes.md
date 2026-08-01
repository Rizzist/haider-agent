# W9b headless-run mutation notes

Each mutation below is paired with a runtime test. “Expected RUNTIME failure”
means the named test must fail by assertion, timeout, or typed protocol error;
a compile-only failure is not the claimed evidence.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| Send `turn.submit` before the Control attach/CaughtUp barrier, discard pre-response events, or reduce response-loss replay before recovering `run_id`. | `haider-client/tests/headless_run_tests.rs` ordering and response-loss tests | The peer observes the wrong method order, the stable command id changes, or the correlated Done/response disappears. |
| Advance the cursor across a gap, emit a duplicate, use a lossy output send, let a blocked presentation sink stop deadline cancellation, or wait forever when pressure drops/withholds `AttachCaughtUp`. | Duplicate/gap, saturated-channel, blocked-output timeout, and withheld-barrier headless tests | Reattach does not resume at the fully-applied cursor, the barrier/deadline hangs, cancellation waits for output drain, or the one-slot consumer sees a missing/duplicate durable sequence. |
| Treat a parked state as terminal, select permission denial by label/index, guess non-permission input, or resume an unknown effect. | Headless permission, natural-terminal, and blocked-input tests | Typed `RejectOnce` is not selected, Waiting ends the run, Cancelled is misclassified, Done is missed, or the run fails to cancel with the exact blocking reason. |
| Treat socket enqueue as durable permission resolution, freeze the menu-opening generation, discard the stable answer intent on reconnect, or accept a competing `AllowOnce` as the selected rejection. | Permission response-loss and competing-resolution headless tests | The peer observes a changed command id/stale generation, the checkpoint hangs, or a mismatched durable winner does not become typed blocked cancellation. |
| Start the wall clock after setup/submit, let a healthy peer withhold correlation forever, send timeout cancellation twice, freeze its pre-restart generation, discard its delivery error, ignore an unconfirmed cancel at grace expiry, stop replaying on a grace-period disconnect, or replace the forced timeout with its later Cancelled terminal. | Preacceptance/withheld-submit, timeout, cancel-response-loss, and unconfirmed-cancellation headless tests | The bound is defeated, stable submit/cancel identity changes, the runner returns a forced result without durable confirmation, disconnect skips remaining replay grace, or the final outcome is not Timeout after a confirmed cancel. |
| Omit permission overrides from wire defaults, the create digest, durable metadata, effective policy, or Welcome features; forge either allowed write/exec as user-typed preauthorization. | RPC golden, store create, live daemon create, permission-policy, Welcome feature, and real CLI write/exec tests | Legacy decode stops being fail-closed, changed flags replay one receipt, reopen loses flags, Ask/Allow precedence changes, a flagged effect still asks/is not ordinary `Allow`, or feature discovery omits the seam. |
| Change a CLI output byte/null rule, timeout bound, or exit mapping. | `haider-cli/tests/cli_tests.rs` parser/output/exit tables and migrated JSONL subprocess oracles | The byte golden, additive ten-field v1 object, duration refusal, RawEnvelope stream, or stable exit code changes. |

The JSONL subprocess tests remain the migration oracles for LF framing,
monotonic durable sequences, correlated terminal envelopes, provider/cancel
exit codes, slow-pipe completeness, and one-shot terminal-append failure. They
now start the real sibling daemon with explicit fake-provider/fault test seams;
no in-process run authority remains.
