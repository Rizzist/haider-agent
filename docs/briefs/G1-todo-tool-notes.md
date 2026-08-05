# G1 — todo_write tool — implementation notes

Brief: `docs/briefs/G1-todo-tool-brief.md` (locked decisions). Seam
authority: `docs/research/g1-todo-seam-map.md`, verified against the
code at v0.0.71 — every cited anchor held.

## What shipped

- **`haider-tools/src/todo_write.rs`** — the model-facing surface. ONE
  tool, WHOLE-LIST REPLACE, args reuse protocol `TodoItem` VERBATIM
  (`{items: [{id, text, state, dep?}]}`). `from_tool_args` runs a
  Claude-Code-vocabulary key repair before strict serde: top-level
  `todos`→`items`, per-item `content`→`text`, `status`→`state`,
  values `pending`→`listed` / `in_progress`→`processing`, unknown extra
  fields (`activeForm`) ignored. Ids assigned positionally ONLY when
  every item omits one (mixed presence is ambiguous → rejected).
  Validation: ≤50 items, unique ids, non-empty ≤500-char text, `dep`
  must reference an existing id, dep chains acyclic (self-dep
  included). Empty list is VALID (a clear). Manifest: `effects: []`,
  `DispatchMode::Await`, teaching description (<120 words: when to
  plan, one `processing` at a time, complete immediately, re-send the
  whole list, stable ids, empty clears).
- **`haider-core/src/actor.rs`** — the fact-emission seam
  (request_input pattern: actor-owned, NO broker). `complete_tool`
  short-circuits `todo_write` before the dispatcher; works
  dispatcher-less. Per-lifecycle state `PlanLifecycle { run_id,
  item_id }` on the actor: first accepted write of a run commits
  `Started{Plan}` under a fresh item id; every later write commits
  `Completed{Plan}` under the SAME id (replace semantics). All items
  completed (non-empty) → the `commit_item` node-pairing seam commits
  `NodeKind::Todos { items }` and the lifecycle closes. Empty list with
  an open plan → `Completed{Plan, []}` (panel unpins; NO Todos node — a
  clear is not a completion); empty list with nothing listed → NO facts
  at all. Both close paths force a FRESH item id for any later plan
  (the projection closes finished ids forever). ToolResult:
  `{"ok":true,"counts":{listed,processing,completed}}` committed with
  `prompt_verbatim_render` (the ToolCall args + echo replay the state
  into later prompts — the compiler's generic ToolCall replay covers
  it, `TurnItem::Plan` itself stays prompt-invisible as before).
  Validation failure → typed REJECTED completed tool result
  (`{"status":"rejected","error":{kind:"invalid_argument",…}}`), the
  daemon `typed_tool_result` shape — never a turn failure.
- **`haider-daemon/src/worker.rs`** — registry:
  `RegisteredToolRoute::TodoWrite`, registered after `request_input`
  with `ToolPermissionDefault::NotApplicable`; the dispatcher match
  lists it in the not-dispatched arm (actor-owned). L5:
  `advertised_tool_definitions(factory, delegated_child)` is the ONE
  advertisement seam — a delegation-owned session's `config.tools`
  (and post-compaction tools) drop exactly `todo_write`. delegation.rs
  Grant lists untouched, per the brief.
- **TUI:** zero functional changes — projection/render/plain handle
  Plan facts as-is. One NEW projection test pins the live empty-clear
  contract (`Completed{Plan,[]}` unpins, closes the id, reborn plan
  needs a fresh id). A cleared plan leaves one `✓ plan — 0/0 done`
  history row (projection-fixed behavior; harmless, noted).
- **Goldens (L6):** protocol fixtures `item_started_plan.json` +
  `item_started_tool_call_todo_write.json` (additive; existing fixtures
  byte-identical). haider-rpc wire transcript untouched — facts cross
  as RawEnvelope, no new frame types. One daemond inventory pin
  (`live_turn_rpc_tests.rs` w8a list) re-anchored honestly to include
  `todo_write`.

## Laws → tests

- L1 `todo_write_is_registered_advertised_and_routable` (daemon).
- L2 `duplicate_ids_are_rejected`, `cyclic_deps_are_rejected`,
  `fifty_one_items_are_rejected`,
  `claude_code_vocabulary_is_accepted_and_normalized` (+ canonical-wins,
  bounds, empty-valid, echo tests) — haider-tools.
- L3 `two_live_todo_writes_share_one_plan_item_and_replay_results`
  (daemon FakeStep turn) +
  `two_writes_in_one_run_share_one_item_id_and_replace_the_list`
  (core seam).
- L4 `completed_plan_commits_a_todos_node_in_the_live_tree` (daemon,
  tree-fact assert) + `all_completed_write_commits_a_todos_node` +
  `born_finished_plan_closes_its_lifecycle_immediately` (core).
- L5 `live_child_session_pack_excludes_todo_write` (recorded child
  `TurnRequest.tools`) + `child_tool_pack_excludes_exactly_todo_write`
  (advertisement seam).
- Decision-3 pin
  `empty_list_is_a_noop_when_nothing_listed_and_a_clear_after_a_list`
  (core) +
  `empty_plan_completed_clears_the_pinned_panel_and_closes_the_id`
  (TUI projection). Rejection path
  `invalid_list_is_rejected_without_plan_facts_or_turn_failure` (core).

## Judgment calls (refinements inside the brief's frame)

- "One item_id per run" refined to one item id per plan LIFECYCLE: the
  projection closes an all-completed (or cleared) id forever, so a NEW
  plan after completion in the same run mints a fresh id — otherwise it
  would be invisible. This is the brief's own decision-9 escape hatch
  ("if a real gap surfaces (e.g. item_id reuse), fix minimally").
- A first write already all-completed emits Started AND Completed
  immediately (the projection needs the pair to pin/unpin; the item
  lifecycle stays closed).
- `NodeKind::Todos` is NOT committed for an empty (cleared) list —
  vacuous "all completed" would enshrine an empty plan in history.
- Positional-id assignment (all-or-nothing) goes one step beyond the
  brief's listed repairs: real CC-trained models omit ids entirely;
  rejecting that shape would defeat the tolerance's purpose. Top-level
  `todos` alias accepted for the same reason (legacy CC arg key).
- Invalid args → typed rejection result (daemon `typed_tool_result`
  precedent) instead of request_input's turn-failure conversion: a bad
  list is a model mistake the model can fix on the next call.

## Gaps / known edges

- Daemon-restart mid-run: `PlanLifecycle` is actor-memory, so the first
  write after a checkpoint recovery starts a fresh item id while the
  projection may still pin the pre-restart panel. The projection's
  Started handling replaces the stale panel wholesale — acceptable
  recovery behavior, no durable state added for it.
- A child session whose provider hallucinates `todo_write` (it is not
  advertised) would still execute it actor-side, journaling Plan facts
  into the CHILD journal only. Pack exclusion (L5) is the contract; the
  chip view never renders child todos today.
- Repeated all-completed re-sends each mint a fresh lifecycle and a
  fresh Todos node (model misbehavior tolerated, one node per
  completion).
- The subagent exclusion filters by name at the advertisement seam;
  delegation Grant lists intentionally untouched (brief decision 8).
