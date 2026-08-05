# G1 seam map — real todo_write tool (Explore agent, 2026-08-05, repo @ v0.0.71)

## Todos today: protocol-real, demo-fed

- `TodoItem { id: u32, text, state: TodoState, dep: Option<u32> }` —
  protocol history.rs:111-118; `TodoState { Listed, Processing, Completed }`
  :122-126. `TurnItem::Plan { items }` — item.rs:49-51. `NodeKind::Todos`
  — history.rs:69-71 (golden fixture node_todos.json exists; node NEVER
  committed in live code).
- TUI: `TodoPanel { item_id, items, pinned }` projection.rs:159-183, stored
  as `SessionProjection.todos` (:208). Started{Plan} pins (:600-605, stale
  guard :589-598); Completed{Plan} replaces items; all-Completed unpins into
  transcript (:622-638). Render: render.rs:2552-2684 layout, :2941-3010
  panel + Hit::TodosToggle/TodoRow, todo_row :6211, finished cell
  :6474-6507. Plain: plain.rs:80-97, :183-196. Collapse: session.rs:74,139.
- PROOF demo-only: TurnItem::Plan constructed only in tui mock.rs:104-109,
  210, 226 (todos() helper :50-67), script.rs:1412-1459 (whole-list
  Completed re-emissions per step, ONE stable item_id), tests. Zero hits in
  core/daemon/tools/provider. demo_store.rs TodosDto :362-366 is
  "DEMO-ONLY persistence". Live path (live.rs on_event → app.rs
  route_raw:7967 → absorb_raw_active:8007 → projection.apply) would render
  a real Plan item TODAY if the daemon emitted one.

## Tool anatomy

- Template: haider-tools spawn_subagent.rs:21-129 (args struct,
  from_tool_args validator, EffectOperation impl, *_manifest() →
  ToolManifest { name, description, effects, dispatch, input_schema }).
  Non-effect template: request_input.rs:1-6,29-67 — bypasses broker
  ("asking a question is not a side effect"). ToolManifest/DispatchMode/
  ToolPermissionDefault: protocol tool.rs:9-41.
- Registry: daemon worker.rs:3895-4008 `registered_tools()` →
  RegisteredTool { manifest, default, route }; lookup :4010-4017;
  TurnToolFactory :411-419 (definitions and dispatcher must agree);
  per-turn wiring :2726, :3279. Inventory: tool_inventory_snapshot
  :4641-4653.
- Execution: BrokerToolDispatcher::execute worker.rs:4114-4461;
  request_input/spawn_subagent/message_subagent special-cased BEFORE broker
  match (:4163-4304); unreachable arm :4459-4461.
- Core actor: ToolDispatcher trait core actor.rs:358-416; start_tool
  commits Started{ToolCall} (:2114-2148); complete_tool → ToolResult fact
  (prompt_verbatim_render) + Completed item (:2178-2273). commit_item maps
  some Completed items to a paired NodeCommitted (:3181-3216) — the seam
  for committing NodeKind::Todos when a plan finishes.
- Render presets: actor.rs:3985-4020. Prompt compiler: TurnItem::Plan
  falls through `_ => {}` (prompt_history.rs:737-791); ToolResult echo IS
  replayed (:761-779) — the model sees its own list via the tool result.
- Subagent packs: delegation.rs:187-231 Grant { tools, effect_ceiling } —
  decide whether children get todo_write.

## Wire

- Facts cross as RawEnvelope inside WireFrame::Event — NEW FACT/ITEM KINDS
  NEED ZERO haider-rpc TYPE CHANGES. Touch only wire_golden_tests.rs
  fixture transcript (UPDATE_FIXTURES=1, tests :62-110, tolerance
  :126-152) and protocol golden_tests.rs (add item_started_plan.json,
  todo_write ToolCall fixture).

## Tests to extend

- TUI projection_tests.rs: plan_pins_updates_and_unpins... (:314),
  finished_plan_redelivery... (:708), stale_plan_started... (:720);
  mock_tests :62-67, plain_tests.
- Core runtime_tests / deferred_tool_tests / prompt_history_tests.
- Daemon runtime_tests, subagent_core_tests, pair_switch_runtime_tests
  (fact-assertion style :352), model_select_tests (state-fact precedent).
- daemond live_turn_rpc_tests.rs (live turn over RPC).

## Recommended shape (settled)

ONE `todo_write` tool, WHOLE-LIST REPLACE (Claude Code TodoWrite
semantics; items carry ids so the model updates/completes specific todos
by re-sending the list). The machinery is emphatically whole-list
(item.rs:82-83 "replace semantics"; projection replaces wholesale; demo
models progress as repeated Completed{Plan} on one item_id). Granular ops
would force new protocol shapes + merge reducer + idempotency rules the
codebase avoids.

Sketch: crates/haider-tools/src/todo_write.rs — args
`{ items: [{id, text, state, dep?}] }` reusing TodoItem verbatim;
from_tool_args validates unique ids + acyclic deps; manifest effects: [],
DispatchMode::Await, ToolPermissionDefault::NotApplicable (request_input
pattern, no broker). Register in registered_tools() with
RegisteredToolRoute::TodoWrite; execute arm holds per-run list, returns
small BoundedResult echo. Actor/dispatcher emits Started{Plan} on first
write, Completed{Plan} on each subsequent write under ONE item_id per run
— existing projection pins/updates/unpins with zero TUI changes. On
all-done commit NodeKind::Todos via commit_item node-pairing seam
(actor.rs:3186-3210). Claude-Code-vocab aliases (content→text,
pending/in_progress/completed→Listed/Processing/Completed) tolerated in
from_tool_args key-repair. Grant todo_write to subagents? Default NO
(root-session planning surface), revisit.
