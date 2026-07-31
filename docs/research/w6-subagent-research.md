# W6 architecture research — local subagents as a tool

Read-only research completed. No files were modified.

## Q1 — Daemon/core architecture

### Architectural conclusion

A local child should be a normal durable Haider session with its own worker, supervisor, provider loop, journal, and tool broker. The parent should invoke it through a daemon-owned delegation coordinator.

The important constraint is that `spawn_subagent` cannot simply remain pending inside today’s synchronous tool dispatcher:

- It would leave the parent in `RunningTool`, not `Waiting(LocalChild)`.
- Provider-stream consumption stops at that tool call.
- Multiple sibling spawns in the same provider response would run serially.
- The parked wait would not survive daemon restart.

The protocol already describes the required mechanism: `DispatchMode::Deferred` returns a correlation ticket and delivers the tool result later ([tool.rs:19](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/tool.rs:19)). W6a should activate that model.

### 1. Existing actor/tool loop

`HarnessActor` is the per-session single writer and owns persist-before-publish behavior and item lifecycle closure ([actor.rs:1](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1)).

One accepted logical turn can issue many provider requests:

1. `drive_turn()` enters the request loop after committing `Thinking` ([actor.rs:642](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:642)).
2. Each provider response can stream text, reasoning, and tool calls.
3. `ToolCallStart` creates an open `TurnItem::ToolCall`; argument deltas are journaled ([actor.rs:1051](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1051), [actor.rs:1481](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1481)).
4. At `ToolCallEnd`, the actor currently awaits `complete_tool()` synchronously ([actor.rs:1067](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1067)).
5. `complete_tool()` commits `RunningTool`, awaits the dispatcher, writes `ToolResult`, completes the item, and returns a provider `Message::tool_result` ([actor.rs:1544](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1544)).
6. At provider `Finish`, any tool results are appended to message history, the actor commits `Thinking`, and the same logical turn issues another provider request. With no tool result, it commits `Done` ([actor.rs:1177](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1177)).

That final step is already the basic auto-continue mechanism. It is tested as a two-provider-request turn in [runtime_tests.rs:220](/Users/rizzist/haider-run/haider-agent/crates/haider-core/tests/runtime_tests.rs:220).

The dispatcher is currently await-only:

- `ToolDispatchResult` has only `Completed` and `ApprovalRequired` ([actor.rs:220](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:220)).
- `ToolDispatcher::execute()` returns one future ([actor.rs:262](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:262)).
- `execute_general_tool()` selects only that future versus the cancellation token ([actor.rs:1600](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1600)).

A useful distinction:

- The per-session worker supervisor remains responsive to queued submissions and cancellation while a tool future is pending ([worker.rs:1016](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:1016)).
- The `HarnessActor` itself does not process its command queue inside `execute_general_tool()`. Supervisor-owned cancellation works because it cancels the turn token; a bare harness stop command waits for dispatch to return.

Therefore the W6 wait must be an explicit parked phase that services cancellation, reports, and control commands—not merely a very long dispatcher future.

### 2. Broker effects and C4a terminalization

`EffectClass::AgentSpawn` is already frozen ([effect.rs:9](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/effect.rs:9)).

Broker effects obey:

```text
Intent → Authorized → Dispatched → Outcome
```

The broker:

- Journals normalized intent before authorization ([broker.rs:883](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:883)).
- Authorizes by effect class and argument digest, not merely tool name ([broker.rs:913](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:913)).
- Commits `Dispatched` before crossing the side-effect boundary ([broker.rs:1056](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1056)).
- Owns exactly-once terminalization and turns abandoned dispatched effects into `Unknown` on close ([broker.rs:817](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:817)).

C4a is the cross-process version: startup scans for durable `Dispatched` effects without outcomes and records `Unknown`; it never blindly redispatches them ([recovery.rs:1](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/recovery.rs:1), [runtime.rs:664](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/runtime.rs:664)).

For W6:

- The `AgentSpawn` effect should cover creation of the durable child/link and initial turn acceptance.
- Its `Outcome::Ok` should be written as soon as that durable spawn transaction/saga is established.
- It must not remain `Dispatched` for the child’s entire lifetime.
- Child execution and report collection belong to the durable delegation ticket, not to the broker effect lifetime.
- If a crash happens at the dispatch boundary, C4a should remain honest and record `Unknown`. A separate delegation reconciler may discover an already-created child through deterministic receipts/linkage, but must not invent a second child.

The production broker dispatcher and definitions live in [worker.rs:2147](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2147). It currently exposes `request_input`, filesystem tools, and `exec`; no spawn tool exists.

### 3. Existing session and worker machinery

A child can reuse the current session lifecycle.

The worker manager already maintains one lazy supervisor per `SessionId`:

- `run_manager` keys slots by session and forwards accepted turns ([worker.rs:460](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:460)).
- A missing slot loads session metadata, obtains a lease, creates its channel, and starts `run_supervisor` ([worker.rs:777](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:777)).
- Each supervisor serializes that session’s turns while different sessions can run concurrently ([worker.rs:867](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:867)).
- All workers are runtime-owned in a `JoinSet`; child watchers must follow the same no-detached-task law ([worker.rs:460](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:460)).
- Supervisor panic handling already evicts the incarnation, applies fencing, and terminalizes from durable journal truth ([worker.rs:676](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:676)).

A waiting parent cannot accidentally become idle: the store only commits aggregate `SessionState::Idle` after every durable run is terminal ([event_store.rs:1729](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:1729)).

#### Can the daemon create a child internally?

Yes, but the reusable composition is currently buried in RPC/private hub paths.

`session.create` currently performs:

- Unfenced receipt preflight.
- Provider and workspace validation.
- Session/command construction.
- Routing through the candidate session actor.
- One store transaction for typed metadata, the sequence-1 `Created` event, and the finalized command receipt.

See [rpc.rs:1454](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1454), [session_hub/mod.rs:1041](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/mod.rs:1041), [session_hub/actor.rs:93](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/actor.rs:93), and [event_store.rs:669](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:669).

The receipt law requires:

- A stable semantic `command_id`.
- Identical method and digest on replay.
- Conflict rejection if an ID is reused with different semantics.

That law is implemented at [event_store.rs:644](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:644).

Child turn acceptance must likewise reuse the existing composition:

- Unfenced receipt preflight ([rpc.rs:1297](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1297)).
- Fenced atomic `Queued`/`UserMessage`/`ActiveRun` acceptance ([event_store.rs:841](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:841)).
- Worker-manager handoff only after durable commit ([rpc.rs:1331](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1331)).

The child creator should therefore be a shared daemon-internal service extracted from the RPC composition—not raw store appends or a fabricated RPC connection.

A parent tool must also not receive arbitrary cross-session authority. `HubStoreHandle` deliberately seals appends to one session ([session_hub/mod.rs:638](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/mod.rs:638)). Add an opaque `DelegationHandle` to `WorkerToolContext`, backed by a daemon-owned coordinator with narrowly typed operations.

### 4. Frozen protocol expectations

The agent protocol is already designed for this feature.

`AgentManifest` provides ([agent.rs:8](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/agent.rs:8)):

- Opaque `AgentId`.
- Role.
- Display-only callsign.
- Model profile.
- A grant that may narrow but never grow.
- Tool allowlist and effect ceiling.
- Budget and placement.
- Lease, fencing epoch, and attempt.
- Direct parent `AgentId`.
- Reserved coordinates.

Callsigns must never be used for addressing, receipt keys, logs, metrics, ancestry, or failure identity ([agent.rs:11](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/agent.rs:11)).

`ChipState` already contains `Idle`, `Thinking`, `Streaming`, `Tool`, `Waiting`, `InputRequired`, `Done`, `Error`, and `Closed` ([agent.rs:60](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/agent.rs:60)).

`ChildReport` carries:

- Agent identity.
- Summary.
- `Green`, `Red`, or `Unverified` verification.
- Optional workspace revision.

See [agent.rs:75](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/agent.rs:75).

The parent-facing event surface is frozen as:

- `AgentSpawned`
- `AgentReport`
- `AgentChipState`

See [lib.rs:74](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/lib.rs:74).

The transcript item vocabulary also has `ChildSpawn { agent }` and `ChildResult { report }` ([item.rs:43](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:43)).

Two pieces are intentionally absent and must remain daemon concerns:

- `AgentManifest` has no child `SessionId`.
- It has no human task label such as `tests` or `docs`.

The `AgentId ↔ child SessionId` mapping must be durable but need not become a public address. A display label should come from a deliberate persisted display field/annotation—not from callsign parsing.

### 5. WAITING and auto-continue

`SessionState` has no waiting variant ([state.rs:34](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:34)). The session remains `ActiveRun`.

Waiting is run-scoped:

- `RunState::Waiting { reason }` already exists ([state.rs:52](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:52)).
- `WaitReason::LocalChild` and `RemoteChild` already exist ([state.rs:87](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:87)).
- Waiting is explicitly a parked, auto-continuing state and may not transition directly to idle ([state.rs:131](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:131)).

Core already demonstrates `Waiting → Thinking` for provider retry backoff ([actor.rs:1384](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1384)).

The required child sequence is therefore:

```text
parent provider response
    │
    ├─ spawn call A ── child session A / worker A
    ├─ spawn call B ── child session B / worker B
    │
    └─ provider Finish
          ↓
 RunState::Waiting(LocalChild)
          ↓
 durable ChildReport A + ChildReport B
          ↓
 ToolResult A + ToolResult B
          ↓
 RunState::Thinking
          ↓
 next provider request in the same logical turn
```

It never passes through `SessionState::Idle`.

### 6. Minimal W6a architecture

#### A. Activate deferred tools in core

In [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs), extend the dispatch result with an opaque deferred child ticket, for example:

```rust
ToolDispatchResult::Deferred {
    ticket: DeferredTicket,
    agent: AgentId,
}
```

The exact type can stay core-internal. It must be durably correlated with:

- Parent session/run.
- Provider `call_id`.
- Open tool `ItemId`.
- Child `AgentId`.
- Child session.
- Attempt/fencing epoch.

At each `ToolCallEnd`:

1. Run the short `AgentSpawn` broker effect.
2. Create/link and start the child.
3. Append `AgentSpawned`, initial `AgentChipState`, and `ChildSpawn`.
4. Return a deferred ticket immediately.
5. Leave the corresponding spawn tool item open.
6. Resume consuming the provider response, allowing sibling spawns to start.

At provider `Finish`:

1. Preserve the provider-native assistant tool-call blocks.
2. Commit `Waiting(LocalChild)`.
3. Await all outstanding child tickets while selecting cancellation and safe control commands.
4. For each result, append `AgentReport`, `ChildResult`, and the bounded `ToolResult`; complete the matching tool item.
5. Supply every report to the provider as `Message::tool_result`.
6. Commit `Thinking` and continue the existing request loop.

A child failure, cancellation, or stall must still produce a bounded failure report/result so the provider’s complete set of tool calls is settled.

#### B. Add a daemon-owned delegation coordinator

Proposed new file:

- `crates/haider-daemon/src/delegation.rs`

Responsibilities:

- Validate task, model, workspace, budget, grant, depth, and fan-out.
- Allocate/persist `AgentId`, callsign, child `SessionId`, and delegation record.
- Invoke receipt-backed internal session create/turn accept/cancel.
- Submit committed child work to `WorkerManagerHandle`.
- Observe durable child progress and terminal state.
- Emit parent chip projections and terminal `ChildReport`.
- Resolve child questions/control by `AgentId`.
- Own all watcher tasks and participate in runtime drain.
- Reconcile partial spawn/collection after restart.

Wire it through:

- `WorkerToolContext` in [worker.rs:94](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:94).
- `BrokerToolFactory` and `BrokerToolDispatcher` in [worker.rs:2147](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2147).
- Runtime late binding near hub/manager installation in [runtime.rs:379](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/runtime.rs:379).

The broker lock should cover authorization and durable spawn establishment only—not the child lifetime.

#### C. Extract internal session commands

Refactor the orchestration currently inside:

- [session_hub/rpc.rs:1267](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1267) for turn submit.
- [session_hub/rpc.rs:1454](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1454) for session create.
- [session_hub/rpc.rs:1373](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1373) for cancellation.

The internal and wire callers must share the same receipt-preflight and fenced-accept implementation.

#### D. Persist a delegation relation

Prefer a dedicated delegation table/API in:

- `crates/haider-store/src/migrations.rs`
- `crates/haider-store/src/event_store.rs`

Minimum record:

- Logical child `AgentId`.
- Child `SessionId`.
- Parent `SessionId`, `RunId`, `call_id`, and tool `ItemId`.
- Direct parent `AgentId`, if recursive.
- Root session/tree coordinate and durable depth.
- Callsign and display label.
- Grant, budget, placement, model profile.
- Lease, fencing epoch, and attempt.
- Spawn/accepted/running/reported/collected state.
- Last durable progress sequence/time.

A store operation should create the child session and delegation row together where possible. Parent-journal publication remains a second session-local append, so recovery must repair that boundary.

#### E. Feed the parent/TUI projection

The child session journal remains authoritative. The attached parent needs a parent-facing durable projection:

- `AgentSpawned`
- `AgentChipState`
- Selected child-scoped item/menu/transcript events with `envelope.agent_id`
- `AgentReport`
- `ChildSpawn`/`ChildResult`

The existing TUI is prepared to route agent-scoped parent events. Menu answers, steering, and close should resolve the opaque `AgentId` through the coordinator to the real child session rather than expose or trust callsigns.

The daemon currently leaves `agent_id` unset in several child-relevant paths:

- Harness creation: [worker.rs:1722](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:1722).
- Prompt-history scope: [worker.rs:1699](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:1699).
- Supervisor-generated envelopes: [worker.rs:2015](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2015).
- Store-created session/acceptance envelopes: [event_store.rs:746](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:746).

Those seams must agree if child sessions use agent-scoped history.

### 7. Identity, callsigns, ancestry, and recursion

Recommended model:

- The head session is depth 0.
- A top-level child is depth 1 and has `manifest.parent = None` unless the design explicitly introduces a stable head `AgentId`.
- A recursive child sets `manifest.parent` to its spawning child’s `AgentId`.
- Parent session/run/call coordinates remain in the durable delegation record.
- Callsigns are allocated once, persisted in the manifest, and retained across retries.
- Callsigns never participate in addressing, idempotency, fencing, or ancestry.
- `AgentId` identifies the logical child across attempts.
- A respawn uses a new lease, increments `attempt`, and advances `fencing_epoch`; stale attempts may not publish reports.

Recommended initial policy:

- Hard recursion depth cap: 4.
- Persist and validate depth; never trust a model-supplied depth.
- Add a separate active-child fan-out cap.
- A child grant is the intersection of requested capabilities and the parent grant.
- `AgentSpawn` is included only when recursion is enabled and `depth < cap`.

W6a can initially issue nonrecursive child grants. W6c can turn on recursive `AgentSpawn` after the recovery and fencing model is proven.

### 8. Crash and journal implications

Current recovery does not support this feature:

- It resumes prior-generation `Queued` runs ([turn_recovery.rs:126](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/turn_recovery.rs:126)).
- It reconstructs only `InputRequired` menu/tool checkpoints ([turn_recovery.rs:141](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/turn_recovery.rs:141)).
- Every other nonterminal run—including `Waiting(LocalChild)`—is currently terminalized ([turn_recovery.rs:164](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/turn_recovery.rs:164)).

Add:

```text
RecoveredWork::ChildWait(ChildWaitCheckpoint)
```

The checkpoint must contain or reconstruct:

- All outstanding spawn call IDs and tool item IDs.
- Provider-native assistant tool-call ordering.
- Child agent/session/delegation tickets.
- Presence or absence of each terminal report.
- Presence or absence of each parent tool result.
- Enough prompt state to issue the next provider request without re-spawning.

The existing menu checkpoint is the closest pattern ([actor.rs:199](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:199), [actor.rs:675](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:675)).

Cross-session event batches are rejected ([event_store.rs:3164](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:3164)), so W6a is necessarily either a specialized store transaction plus parent append or a durable saga.

Recovery must handle these windows idempotently:

1. Child session/link exists, but parent `AgentSpawned` is absent.
2. Parent spawn event exists, but child turn was not accepted/submitted.
3. Child terminal event exists, but `AgentReport` is absent.
4. `AgentReport` exists, but the spawn tool has no `ToolResult`.
5. `ToolResult` exists, but the parent continuation was not reactivated.
6. Parent was cancelled while children/watchers remained live.
7. A stale child attempt reports after a kill/respawn.

Use deterministic command IDs derived from opaque parent session/run/call/agent coordinates. Never derive them from callsigns.

Graceful drain must also preserve `Waiting(LocalChild)` as a recoverable parked state; today shutdown preserves only input-required work and cancels other active turns ([worker.rs:1033](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:1033)).

### 9. Stall supervision

Put stall policy in the delegation coordinator, not in the generic worker manager.

A meaningful heartbeat should be based on committed journal progress:

- Latest committed child sequence/time.
- Run-state transitions.
- New items/deltas.
- Tool/effect progress.
- Descendant progress when the child itself is `Waiting(LocalChild)`.

Do not classify these as stalls:

- `InputRequired` waiting on a user.
- A child waiting on descendants that are still making durable progress.

Recommended escalation:

1. Inactivity deadline expires.
2. Persist a nudge decision.
3. Deliver a safe-boundary nudge.
4. After a grace deadline, persist cancellation/kill intent.
5. Use the existing durable cancellation path.
6. If cooperative cancellation fails, terminate the supervisor/incarnation and use existing journal-truth terminalization.
7. Emit terminal `AgentChipState::Error` and a bounded failure `ChildReport`.
8. Always settle the parent’s tool call.
9. Optionally respawn within a bounded retry policy using the same `AgentId`, preserved callsign, `attempt + 1`, and new lease/fence.

A real nudge transport does not exist yet:

- Manager/supervisor commands currently cover submit/recover/shutdown, not steer/nudge ([worker.rs:270](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:270)).
- Active-session “steer” currently degrades to a queued fresh turn ([event_store.rs:890](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:890)).

W6c needs manager → supervisor → harness nudge delivery at a defined safe boundary before claiming nudge semantics. Until then, the honest implementation is deadline warning followed by durable cancellation.

### 10. Ranked build order

#### W6a — daemon core

1. Durable delegation schema and opaque identity/ancestry model.
2. Shared internal session-create, turn-accept, and cancel orchestration preserving receipt laws.
3. Core deferred-tool result plus multi-ticket `Waiting(LocalChild)` collection.
4. Daemon-owned `DelegationCoordinator` and narrow `DelegationHandle`.
5. Production `spawn_subagent` definition, `AgentSpawn` authorization, and child worker launch.
6. Parent `AgentSpawned`/chip/report/item projection and exact-once report-to-tool-result collection.
7. `ChildWaitCheckpoint`, startup recovery, and graceful-drain recovery.
8. Crash-window, multi-sibling, cancellation, and report-idempotency tests.

#### W6b — TUI chips

1. Feed real parent `AgentSpawned`, scoped child events/menus, `AgentChipState`, `AgentReport`, and `ChildResult`.
2. Repair live question/recovery derivation.
3. Decorate durable `Waiting(LocalChild)` with the recursive live-child count.
4. Add a stable task/display label source.
5. Finish live closed/removal and control routing.

#### W6c — stall supervision and recursion

1. Internal durable-progress feed and owned deadlines.
2. Nudge command path.
3. Durable kill/cancel and guaranteed failure-report settlement.
4. Attempt/lease/fencing enforcement and bounded respawn.
5. Recursive `AgentSpawn` grants, depth/fan-out caps, subtree cancellation, and recursive restart tests.

---

## Q2 — Simulator chip surfaces and ratatui port specification

Simulator source: [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js).

### 1. Recursive chip model and states

The simulator’s chip collection is a recursive tree:

- Recursive find/mutate/remove/cleanup: [tui.js:284](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:284), lines 284–307.
- Recursive live-descendant count: [tui.js:308](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:308), lines 308–317.
- A live child with live descendants derives `waiting`, except when input-required: [tui.js:318](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:318), lines 318–322.
- Session-wide recursive live count: lines 324–329.

The exact visual vocabulary is at [tui.js:331](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:331), lines 331–342:

| State | Glyph |
|---|---:|
| idle | `○` |
| thinking | `●` |
| streaming | `▮` |
| running | `◐` |
| tool | `⚒` |
| input required | `?` |
| waiting | `◔` |
| done | `✓` |
| error | `✗` |

`running` is simulator-only; the frozen Rust `ChipState` does not contain that variant.

### 2. Callsigns

- Callsigns are explicitly presentation identities only: [tui.js:344](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:344), lines 344–347.
- Honour-roll ordering is lines 348–390.
- Rollover suffixes are lines 391–399.
- New children claim callsigns at lines 881–886.
- Spawn transcript copy displays callsign/honorific at [tui.js:1262](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1262), lines 1262–1284.

The ratatui port should display callsigns prominently but continue using opaque `AgentId` everywhere operational.

### 3. Chips row/tree panel

Activity text is derived at [tui.js:2366](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2366), lines 2366–2380:

- Unresolved question text.
- `waiting on N children`.
- `report ready`.
- `thinking…`.
- Otherwise the latest tool/text activity.

Tree flattening is depth-first at lines 2381–2387.

The aggregate header is assembled at lines 2388–2407:

```text
? N needs input · ◔ N waiting · ◐ N working ·
✓ N done · ✗ N failed · ○ N idle · ⊘ N closing
```

The actual panel is at [tui.js:2908](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2908), lines 2908–2944:

- `▾/▸ subagents` collapse control.
- Tree indentation/connectors.
- State glyph.
- Callsign and honorific.
- Task/model/device metadata.
- Activity text or `viewing ←`.
- Dim `⊘` closing rows.
- Back-to-parent affordance in the child view.

It appears below the composer and above the status bar at lines 3357–3362.

State styling is at [tui.js:4763](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:4763), lines 4763–4866:

- Running/tool glyphs pulse.
- `?` and its activity pulse amber.
- Done is green.
- Error is red.
- Closing rows are faint.

### 4. Amber question chip

The scripted tests child changes atomically to input-required and installs its question at [tui.js:921](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:921), lines 921–935.

The parent transcript note is line 936:

> subagent tests needs input — its chip is holding an amber ?

The tree activity becomes the question text; the `?` and activity pulse amber at lines 4828–4835. The detail badge pulses amber at lines 5320–5330.

Answering the question records the choice in the child transcript, resumes it, creates its report, and arms parent auto-resume at lines 1057–1074.

### 5. Recovery `⌁`

Recovery has two distinct visuals:

- The tree row remains `✗ error`.
- `⌁` is the recovery-card/menu glyph.

The scripted failure and recovery menu are at [tui.js:939](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:939), lines 939–957.

Recovery choice handling is lines 1033–1054. The composer-replacement card selects `⌁` at [tui.js:3085](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3085), lines 3085–3107. Generic recovery glyph mapping also appears at line 3057.

The card replaces the child composer and offers retry/retain/close behavior; it is not a different chip state.

### 6. Subagent tree/detail “overlay”

The simulator does not use a modal overlay. `screen === "subagent"` is a dedicated session-like detail screen.

- Lookup and ancestry breadcrumb: [tui.js:2348](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2348), lines 2348–2356.
- Pending question/recovery selection: lines 2357–2364.
- Steering placeholder: lines 3008–3015.
- Full detail surface: [tui.js:3430](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3430), lines 3430–3482.

It contains:

- Parent/ancestor breadcrumb.
- Callsign, task, model, and device metadata.
- State badge, including `◔ WAITING · N child`.
- Close control.
- Full child transcript.
- Explicit descendant-wait note.
- Question/recovery card or composer.
- The same recursive subtree panel and status bar.

### 7. Parent `WAITING(subagents)` badge

The simulator derives a parent display state of `WAITING` when its raw state is `IDLE` but any descendant chip is live ([tui.js:2807](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2807), lines 2807–2811).

It renders:

```text
◔ WAITING · N subagent(s)
```

at lines 2825–2832. Badge pulse/style is lines 5531–5564.

Production should not copy the simulator’s raw-`IDLE` trick. W6a should emit the authoritative durable `RunState::Waiting(LocalChild)`, and W6b should decorate it with the recursive count.

### 8. Auto-continue transcript semantics

The intended law is documented at [tui.js:960](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:960), lines 960–964:

```text
last report → waiting → thinking → streaming → turn end
```

It must never transition directly from waiting to idle.

The guards against premature/double resume are lines 965–975. At line 984 the transcript receives:

> all subagents reported — resuming the parked turn (waiting → thinking, never idle)

Lines 985–997 transition through thinking and streaming and render:

> Folding the N subagent reports…

Queued parent input is consumed without an idle gap at lines 998–1005. Individual report/merge notes appear at lines 1052–1053 and 1071–1073. Closing the final live child also discharges the wait at lines 1166–1182.

In production these should be journaled daemon/core events and resumed model output. The live TUI should render them, not run the simulator’s local auto-resume script.

### 9. Simulator recursion defect

The simulator contains an accidental dead-end:

- `cTool` returns nothing at lines 909–915.
- Nested spawn checks `if (!(await ops.cTool(...))) return` at line 1137.
- It therefore always returns before the intended nested waiting/resume code at lines 1138–1149.

The Rust demo intentionally preserves this behavior in [script.rs:2042](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/script.rs:2042) and tests it in [subagent_aura_tests.rs:474](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/tests/subagent_aura_tests.rs:474).

Real recursive W6 agents must not inherit that defect.

### 10. Ratatui projection that already exists

The workspace is substantially beyond initial TUI0 scaffolding.

Already implemented:

- `ChipQuestion` and recursive `ChipModel`, each with a child `SessionProjection`: [app.rs:490](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:490).
- Manifest-to-chip conversion: [app.rs:579](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:579).
- Question-card gate, liveness, descendant-derived waiting, and activity text: [app.rs:617](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:617).
- Recursive counts, search, paths, removal, and flattening: [app.rs:746](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:746).
- Demo/protocol chip-state mapping and glyphs: [script.rs:240](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/script.rs:240).
- Per-session chip tree state and auto-resume flags: [session.rs:31](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/session.rs:31).
- Live routing of agent-scoped events and agent lifecycle payloads: [session.rs:220](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/session.rs:220).
- Recursive manifest parenting and explicit chip-state/report reduction: [session.rs:269](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/session.rs:269).
- Parent counted WAITING badge: [app.rs:2190](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:2190).
- Recursive count and tree renderer: [render.rs:2059](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:2059), [render.rs:2118](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:2118).
- Full child detail screen: [render.rs:2261](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:2261).
- Recovery glyph/card rendering: [render.rs:3132](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:3132).
- `ChildSpawn`/`ChildResult` transcript rendering: [render.rs:4276](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:4276).
- Demo auto-resume sequence: [script.rs:2084](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/script.rs:2084).

### 11. Missing W6b integration checklist

1. **Feed real daemon events.**  
   Drive chips from durable `AgentSpawned → AgentChipState → scoped child events/menu → AgentReport/ChildResult`.

2. **Populate live question/recovery cards.**  
   A scoped `MenuOpened` currently reaches the chip transcript, but `ChipModel::question_menu()` also requires `chip.question`, which is populated only by demo events ([app.rs:617](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:617)). Derive `ChipQuestion` from live `Menu`, `MenuKind`, and menu resolution events.

3. **Preserve agent scope.**  
   Child transcript and menu envelopes must carry the child `agent_id`; the live router uses envelope scope to choose the chip ([session.rs:234](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/session.rs:234)).

4. **Add a task/display label.**  
   `ChipModel::from_manifest` currently leaves `name` empty because `AgentManifest` has no task label ([app.rs:604](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:604)). Persist and project an explicit short label.

5. **Decorate authoritative WAITING with a count.**  
   Today the counted badge only replaces literal `IDLE`. Once W6a emits `Waiting(LocalChild)`, projection renders the generic reason `subagent` ([projection.rs:630](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/projection.rs:630), [projection.rs:802](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/projection.rs:802)). Keep the durable state authoritative while rendering `N subagent(s)`.

6. **Keep auto-continue daemon-owned.**  
   `AgentReport` currently adds report content to the child projection only ([session.rs:296](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/session.rs:296)). W6a must also journal parent `ChildResult`, tool result, state transitions, and resumed stream.

7. **Add real control routing.**  
   Live child steering is currently refused because no RPC exists ([app.rs:3002](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:3002)). Live close is similarly refused ([app.rs:5551](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:5551)). Route controls by opaque `AgentId`.

8. **Define live `Closed`.**  
   Protocol `Closed` currently maps to display `Done`; dim `⊘` and delayed removal are demo-only ([script.rs:256](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/script.rs:256)). Either map live `Closed` into the removal lifecycle or omit the five-second animation.

9. **Do not add a wire `Running` state.**  
   Use the frozen `Thinking`, `Streaming`, `Tool`, `Waiting`, and terminal states. `◐ N working` can remain an aggregate display category.

10. **Do not port the nested simulator dead-end.**  
    The live recursive reducer already attaches children via `manifest.parent`; daemon events should let that existing renderer finish the intended recursive sequence.

## FINDINGS SUMMARY — Q1 (architecture + build order)

- Build `spawn_subagent` as a deferred tool, not a long synchronous dispatcher call.
- Each child is a normal durable session with its own existing lazy worker supervisor.
- Add a daemon-owned delegation coordinator with narrow cross-session authority.
- Reuse extracted receipt-backed session-create, turn-accept, and cancel machinery.
- Keep the `AgentSpawn` effect short: terminalize it after durable child/link creation, not after child completion.
- Park the parent in `RunState::Waiting(LocalChild)`, collect all `ChildReport`s as tool results, then transition to `Thinking` in the same logical turn.
- Persist opaque identity, ancestry, child-session mapping, call correlation, attempts/fences, reports, and collection markers.
- Add `ChildWaitCheckpoint`; current recovery would otherwise terminalize the waiting parent.
- Use callsigns only for display. Start with nonrecursive children in W6a; add depth-capped recursive grants and stall supervision in W6c.
- Build order: W6a durable core and recovery → W6b live chip integration → W6c progress deadlines, nudge/kill/respawn, and recursion.

## FINDINGS SUMMARY — Q2 (port checklist)

- Keep the existing recursive `ChipModel`, tree renderer, detail screen, callsign presentation, question/recovery card, WAITING badge, and `ChildResult` transcript rendering.
- Wire the real daemon event chain into the live reducer.
- Populate `ChipQuestion` from live scoped menus.
- Preserve child `agent_id` on every scoped transcript/menu event.
- Add an explicit task label.
- Render durable local-child WAITING as `◔ WAITING · N subagent(s)`.
- Keep recovery as red `✗` in the tree and `⌁` on its recovery card.
- Render daemon-owned auto-continue; do not run the demo’s local lifecycle in live mode.
- Add real AgentId-based answer/steer/close routing and honest `Closed` projection.
- Do not add a protocol `Running` state or reproduce the simulator’s nested-spawn bug.
