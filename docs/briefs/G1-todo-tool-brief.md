# G1 — real model-facing todo tool (`todo_write`)

Owner contract, verbatim: "fix this to be real tool that agent can use,
update, and complete specific todos etc.. like claude code." (The /model
comparison table row: "Haider's Todos cells exist but are demo-only".)

Authority: `docs/research/g1-todo-seam-map.md` (repo seams, file:line) and
`docs/research/g-wave-external-api-research.md` § "Claude Code todo
ergonomics". Branch: `g1-todo-tool`. Read both BEFORE writing code.

## Locked design decisions

1. ONE tool, `todo_write`, WHOLE-LIST REPLACE per call (Claude Code
   TodoWrite semantics). Items carry stable `id`s, so the model updates or
   completes a SPECIFIC todo by re-sending the list with that item's
   `state` changed — this satisfies the owner's "specific todos" ask
   without inventing granular protocol ops the codebase avoids
   (item.rs:82-83 replace semantics).
2. Args reuse protocol `TodoItem` VERBATIM:
   `{ "items": [{ "id": u32, "text": str, "state": "listed"|"processing"|"completed", "dep"?: u32 }] }`.
   Do NOT rename protocol variants. Tolerant key-repair in
   `from_tool_args` (Claude-Code vocab): `content`→`text`,
   `status`→`state` with `pending`→`listed`, `in_progress`→`processing`,
   `completed`→`completed`; unknown extra fields (e.g. `activeForm`)
   ignored, not errors.
3. Validation in `from_tool_args`: unique ids; `dep` must reference an
   existing id and be acyclic; ≤ 50 items; each `text` non-empty, ≤ 500
   chars. Empty `items` list is VALID and clears the plan (panel unpins
   with nothing added to transcript if nothing was ever listed — pick the
   least surprising projection behavior and pin it in a test).
4. Not a side effect: `effects: vec![]`, `DispatchMode::Await`,
   `ToolPermissionDefault::NotApplicable` — request_input pattern, NO
   broker involvement (haider-tools/src/request_input.rs template).
5. Fact emission (the part that makes the demo panel real): per run, the
   FIRST todo_write emits `ItemEvent::Started { TurnItem::Plan }` under a
   fresh item_id; every SUBSEQUENT todo_write in the same run emits
   `ItemEvent::Completed { TurnItem::Plan }` with the full new list under
   the SAME item_id (the projection's replace semantics,
   projection.rs:600-638; the demo script.rs:1443-1457 is the model).
   When every item is `completed`, commit `NodeKind::Todos { items }` via
   the commit_item node-pairing seam (core actor.rs:3181-3216).
6. ToolResult (what the model sees): compact echo —
   `{"ok": true, "counts": {"listed": n, "processing": n, "completed": n}}`
   with prompt_verbatim_render so later turns replay the state implicitly.
7. Manifest description must TEACH usage (this is the model-facing spec):
   when to plan (multi-step work), one `processing` item at a time, mark
   completed immediately after finishing, re-send the whole list each
   call. Keep it under ~120 words; look at spawn_subagent's description
   register.
8. Subagents do NOT get `todo_write` (root planning surface only) — leave
   delegation.rs Grant lists untouched; add a test pinning its absence
   from the child tool pack.
9. TUI: expect ZERO functional changes (projection/render/plain already
   handle Plan facts). If a real gap surfaces (e.g. item_id reuse across
   runs), fix minimally and note it.

## Mandatory laws (runtime, not vacuous)

- L1 registry: `todo_write` appears in `registered_tools()` +
  `definitions()` offered to the provider; name→route lookup resolves.
- L2 validation: dup ids rejected; cyclic dep rejected; 51 items
  rejected; CC-vocab payload (content/status/pending/in_progress)
  ACCEPTED and normalized — each its own test.
- L3 daemon runtime (FakeStep scripted turn): provider calls todo_write
  twice in one run → journal shows Started{Plan} then Completed{Plan},
  SAME item_id, second list replaces first; ToolResult facts present with
  verbatim prompt render.
- L4 completion: a write with all items completed → `NodeKind::Todos`
  node committed to the history tree (assert on the tree, not the panel).
- L5 subagent absence: child session tool pack excludes todo_write.
- L6 goldens: protocol fixture `item_started_plan.json` (+ todo_write
  ToolCall fixture) in haider-protocol/tests; haider-rpc wire transcript
  should need NO new frame types — if you regenerate the fixture
  transcript anyway, re-anchor tail assertions HONESTLY (see
  wire_golden_tests.rs conventions).
- Extend, don't weaken, the existing projection tests
  (projection_tests.rs:314, :708, :720) only if live semantics differ
  from demo semantics.

## Discipline (non-negotiable)

- CARGO_INCREMENTAL=0 for all builds/tests.
- Ledger: `cargo run -p xtask -- test-count --update` before the final
  commit; commit message states old → new count truthfully.
- Write `docs/briefs/G1-todo-tool-notes.md` (what/why, gaps) and
  `docs/briefs/G1-todo-tool-mutation-notes.md`: executed mutations only —
  commit first, apply single-anchor mutation, run the ONE named test
  (require "running 1 test" in output — vacuity check), record the
  observed runtime failure, revert, re-run green. At least 4 mutation
  executions across the fact-emission seam, validation, completion
  commit, and subagent absence.
- `cargo fmt --all -- --check` clean at every commit.
- Do NOT: bump versions, tag, touch MCP, add rpc request types, rename
  existing protocol variants, delete ~/.codex/sessions.
