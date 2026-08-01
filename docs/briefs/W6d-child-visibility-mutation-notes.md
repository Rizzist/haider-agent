# W6d child-visibility mutation notes

Every mutation below has a separate runtime observer. “Expected RUNTIME
failure” means an assertion, typed UDS response, or bounded completion wait
changes; a compile-only failure is not the claimed evidence.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| Drop coordinator mirroring, omit the additive `PermissionRequired` chip, or map a child permission park to `Thinking`. | `subagent_core_tests::child_permission_park_is_visible_in_the_parent_chip_journal` and `permission_required_chip_state_is_an_additive_wire_value` | The parent journal never carries `AgentChipState { agent, permission_required }`, or the additive wire value no longer round-trips. |
| Supervise a `PermissionRequired` child as silent, nudge/cancel while it is parked, or fail to re-arm the deadline after the committed answer leaves the parked state. | `subagent_core_tests::permission_park_pauses_stall_supervision_and_unpark_rearms_it` | A nudge or `Cancelled` appears during the deliberately over-deadline park, or exactly one nudge plus cancellation never appears after approval and renewed silence. |
| Special-case child sessions out of attach, omit Control ownership, bypass or reject the durable menu CAS, or fail to wake the child's permission checkpoint. | `subagent_core_tests::control_attach_and_menu_answer_over_uds_complete_a_child_session` | The production UDS `session.attach`/`menu.answer` transcript returns a typed error, the child does not reach `Done`, or the parent does not collect the child's own report. |
| Acknowledge a deferred spawn before its child report so the provider round advances without the tool result paired to the call. | Existing `production_spawn_effect_wait_and_report_chain_is_end_to_end` plus the delegation module charter | The parent request history lacks the exact spawn call's `ToolResult`, or the state chain no longer parks in `Waiting(LocalChild)` until collection. |
