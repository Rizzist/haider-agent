# G1 — todo_write tool — executed mutation ledger

Protocol per kill: commit BEFORE mutating, apply one single-anchor
mutation, run the ONE named law and require "running 1 test" in the
output (vacuity check), record the observed runtime failure, revert via
`git checkout` against the committed implementation, re-run green. The
four brief-mandated seams (fact emission, validation, completion commit,
subagent absence) are each covered; two extra kills (registry, CC-vocab
repair) add margin.

## 6 executions — 6 kills

1. **Mutation (fact-emission seam):** the open-lifecycle arm of
   `emit_plan_facts` mints a FRESH item id for every subsequent write
   (`Some(item_id)` → `Some(_stale)` + `let item_id =
   self.next_item_id();`) — `haider-core/src/actor.rs`. Ran only
   `two_writes_in_one_run_share_one_item_id_and_replace_the_list`
   ("running 1 test" observed). KILLED
   (`todo_write_runtime_tests.rs:152`): `assertion left == right failed:
   one item id per plan lifecycle — left:
   ItemId("item-todo-write-session-11-1700000000000-2") right:
   ItemId("…-4")`. Reverted, green.
2. **Mutation (validation seam):** the duplicate-id rejection disabled
   (`if !ids.insert(item.id)` → `… && false`) —
   `haider-tools/src/todo_write.rs` `validate`. Ran only
   `duplicate_ids_are_rejected` ("running 1 test" observed). KILLED
   (`todo_write_tests.rs:39`): `panicked: duplicate ids must not
   validate: TodoWrite { items: [TodoItem { id: 1, … }, TodoItem { id:
   1, … }] }` — the mutant VALIDATED the duplicate list. Reverted,
   green.
3. **Mutation (completion-commit seam):** the `commit_item` Plan
   node-pairing guard inverted (`!items.is_empty()` →
   `items.is_empty()`) so a finished plan never pairs `NodeKind::Todos`
   — `haider-core/src/actor.rs`. Ran only
   `all_completed_write_commits_a_todos_node` ("running 1 test"
   observed). KILLED (`todo_write_runtime_tests.rs:233`): `assertion
   left == right failed: exactly one Todos node — left: 0 right: 1`.
   Reverted, green.
4. **Mutation (subagent-absence seam):** the child filter in
   `advertised_tool_definitions` disabled (`if delegated_child` → `…
   && false`) — `haider-daemon/src/worker.rs`. Ran only the LIVE law
   `live_child_session_pack_excludes_todo_write` ("running 1 test"
   observed). KILLED (`g1_todo_runtime_tests.rs:479`): `panicked: a
   delegated child must not see todo_write` — the recorded child
   `TurnRequest.tools` advertised the tool. Reverted, green.
5. **Mutation (registry seam, margin):** the registered entry renamed
   (`manifest.name = "todo_write_disabled"`) in `registered_tools()` —
   `haider-daemon/src/worker.rs`. Ran only
   `todo_write_is_registered_advertised_and_routable` ("running 1 test"
   observed). KILLED (`g1_todo_runtime_tests.rs:235`): `panicked:
   todo_write is registered`. Reverted, green.
6. **Mutation (CC-vocab repair seam, margin):** the
   `in_progress → processing` status-value mapping dropped from
   `repair_args` — `haider-tools/src/todo_write.rs`. Ran only
   `claude_code_vocabulary_is_accepted_and_normalized` ("running 1
   test" observed). KILLED (`todo_write_tests.rs:94`): `panicked:
   CC-vocabulary payload is repaired, not rejected: InvalidArgument {
   message: "invalid todo_write arguments: unknown variant
   \`in_progress\`, expected one of \`listed\`, \`processing\`,
   \`completed\`" }`. Reverted, green.

Post-campaign: working tree clean against the committed implementation;
the touched suites re-ran green (haider-tools 8/8 todo_write tests,
haider-core todo_write_runtime_tests 5/5, haider-daemon g1 5/5).

## Review of record (coordinator, executed post-lane)

Read the full branch diff (tool surface, actor lifecycle, worker registry,
laws, goldens). One structurally-unobserved gate found and closed:

| # | Mutation (seam) | Verdict on lane's laws | Resolution |
|---|---|---|---|
| RM1 | Drop the `plan.run_id == *run_id` filter in `emit_plan_facts` (actor.rs:2549) — the documented "stale lifecycle never leaks across runs" invariant | SURVIVED — all 5 core laws, all 5 daemon laws, all 22 projection tests stay green with run-scoping deleted | New pin `an_unfinished_plan_does_not_leak_into_the_next_run` (core, stages an unfinished plan then a second run; asserts a fresh Started with a distinct item id). Kill verified: "running 1 test" observed; run 2's first plan fact became a Completed under run 1's item id, both asserts failed. Reverted; 6/6 green |

Lane's own 6 kills spot-checked against the notes; no discrepancies. The
lane's deviations (lifecycle vs run scoping, clear-commits-no-node, wider
key repair, typed rejection, daemond inventory re-anchor) all verified
in-diff and correctly documented. Ledger 1925 -> 1926 with this pin.
