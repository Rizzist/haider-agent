# S1 subagents-firstclass mutation notes

Each mutation below was applied to production code, its separate observer was
run to an assertion-level RUNTIME failure, and the mutation was reverted. No
compile-only failure is claimed as evidence.

| Production mutation (applied → reverted) | Runtime observer | Observed RUNTIME failure |
|---|---|---|
| Changed the Unicode-safe parent preview ceiling from 200 characters to 199. | `subagent_core_tests::message_subagent_steers_running_child_and_journals_bounded_parent_fact` | The non-degenerate `界` fixture failed at RUNTIME with `left: 199`, `right: 200`. |
| Changed the handoff `.gitignore` bytes from the literal `*` to `ignored`. | `subagent_core_tests::message_subagent_steers_running_child_and_journals_bounded_parent_fact` | The real file read failed at RUNTIME with the written `ignored` bytes instead of `[42]`. |
| Classified the active-run delivery receipt as `delivered_queued` instead of `delivered_steer`. | `subagent_core_tests::message_subagent_steers_running_child_and_journals_bounded_parent_fact` | The receipt assertion failed at RUNTIME with `DeliveredQueued` instead of `DeliveredSteer`. |
| Restored the old current-run history filter that kept only the first durable user message. | `prompt_history_tests::current_run_recovery_keeps_every_durable_steer_message` | Restart compilation failed at RUNTIME with only `inspect the parser`; the non-degenerate Unicode-boundary STEER was absent. |
| Forced `StopIfQuiescent` to stop the actor after a non-quiescent result. | `session_hub_private_tests::deletion_barrier_preserves_a_prefence_accepted_turn_and_its_actor` | The deletion barrier correctly returned `false`, then the lease probe failed at RUNTIME with `SendError` because the actor had stopped. |

The same runtime observer also pins two replay mutations without degenerate
input: reusing its command with changed text must return the typed
`invalid_argument` semantics conflict, while the exact replay must leave the
provider-request count at two (no duplicate STEER injection).
