# B2 branch-wave research

Read-only audit of branch `b2-branches` on 2026-08-01. No Rust or simulator code was changed. The central finding is that B2 is not starting from zero: W7a already has exact branch/agent prompt scoping. What is absent is the durable named-ref registry, a production origin for non-`None` branch scope, fork-lineage resolution, and the TUI's per-branch state.

## Q1

### Durable branch model today

`BranchId` is an opaque string newtype documented as “a branch (named ref) within a session's history tree” ([`crates/haider-protocol/src/ids.rs:30-40`](../../crates/haider-protocol/src/ids.rs#L30-L40)). The only durable branch coordinate is the optional `branch_id` on every `EventEnvelope`, beside the session, run, and agent identities ([`crates/haider-protocol/src/envelope.rs:37-49`](../../crates/haider-protocol/src/envelope.rs#L37-L49)); payload re-wrapping preserves it ([`crates/haider-protocol/src/envelope.rs:154-173`](../../crates/haider-protocol/src/envelope.rs#L154-L173)).

`TreeNode` itself contains only `node`, optional `parent`, and `kind`, so a node's branch membership comes from its containing envelope, not from the node payload ([`crates/haider-protocol/src/history.rs:16-22`](../../crates/haider-protocol/src/history.rs#L16-L22)). The history contract describes immutable, single-parent nodes; compaction keeps its covered range navigable, and cross-branch reuse is explicit through `ResultImport` ([`crates/haider-protocol/src/history.rs:49-62`](../../crates/haider-protocol/src/history.rs#L49-L62)).

Storage has one monotonic sequence per session. SQLite stores the complete envelope JSON under `(session_id, seq)` and has neither a branch column nor a branch table/index ([`crates/haider-store/src/migrations.rs:33-47`](../../crates/haider-store/src/migrations.rs#L33-L47)). Append allocates the next session-global range and serializes each envelope unchanged after stamping `seq` and `committed_at_ms` ([`crates/haider-store/src/event_store.rs:3879-3928`](../../crates/haider-store/src/event_store.rs#L3879-L3928)). Thus `branch_id` is durable, but branch names, fork coordinates, branch heads, and active branch are not modeled.

W7a's prompt compiler already accepts `Option<&BranchId>`, `Option<&AgentId>`, and the current run ([`crates/haider-core/src/prompt_history.rs:30-44`](../../crates/haider-core/src/prompt_history.rs#L30-L44)). Its scope predicate is exact equality on both identities ([`crates/haider-core/src/prompt_history.rs:680-686`](../../crates/haider-core/src/prompt_history.rs#L680-L686)); `latest_head` and `TreeProjection::build` use that scope ([`crates/haider-core/src/prompt_history.rs:152-169`](../../crates/haider-core/src/prompt_history.rs#L152-L169), [`crates/haider-core/src/prompt_history.rs:251-285`](../../crates/haider-core/src/prompt_history.rs#L251-L285)). The law test `branch_agent_and_nonterminal_history_are_excluded_structurally` stamps matching, wrong-branch, wrong-agent, and nonterminal histories and proves that only the matching completed history reaches the provider ([`crates/haider-core/tests/prompt_history_tests.rs:897-1011`](../../crates/haider-core/tests/prompt_history_tests.rs#L897-L1011)). B2 must preserve that law.

There is one fork-lineage gap behind that existing support. `TreeProjection::build` filters out other branch scopes before constructing its node-id index, and ancestry traversal rejects a parent missing from that index ([`crates/haider-core/src/prompt_history.rs:260-285`](../../crates/haider-core/src/prompt_history.rs#L260-L285), [`crates/haider-core/src/prompt_history.rs:308-329`](../../crates/haider-core/src/prompt_history.rs#L308-L329)). Therefore the first node on branch B cannot simply name a parent node whose envelope belongs to branch A. This does not mean basic branch scoping is missing; it means B2 needs a lineage-aware overlay that admits only B's declared ancestors through its fork coordinate.

### What writes a non-`None` branch today

The generic core actor is capable of doing so: `HarnessConfig` owns `branch_id`, its tree-parent lookup is scoped by it, and every actor envelope clones it ([`crates/haider-core/src/actor.rs:80-86`](../../crates/haider-core/src/actor.rs#L80-L86), [`crates/haider-core/src/actor.rs:3380-3391`](../../crates/haider-core/src/actor.rs#L3380-L3391), [`crates/haider-core/src/actor.rs:3410-3439`](../../crates/haider-core/src/actor.rs#L3410-L3439)). Two other paths propagate an existing branch rather than originate one: core effect recovery copies the dispatched envelope's scope ([`crates/haider-core/src/recovery.rs:67-95`](../../crates/haider-core/src/recovery.rs#L67-L95)), and menu resolution copies the opening envelope's branch/run/agent ([`crates/haider-store/src/event_store.rs:3482-3504`](../../crates/haider-store/src/event_store.rs#L3482-L3504)).

The shipped daemon originates no non-`None` branch:

- `HarnessConfig::for_session` defaults branch and agent to `None` ([`crates/haider-core/src/actor.rs:140-160`](../../crates/haider-core/src/actor.rs#L140-L160)). Daemon `start_turn` sets `agent_id` but never sets `branch_id`, and calls prompt compilation with `None` ([`crates/haider-daemon/src/worker.rs:2879-2888`](../../crates/haider-daemon/src/worker.rs#L2879-L2888), [`crates/haider-daemon/src/worker.rs:2911-2927`](../../crates/haider-daemon/src/worker.rs#L2911-L2927)).
- Wire `turn.submit` has no branch coordinate ([`crates/haider-rpc/src/frame.rs:761-771`](../../crates/haider-rpc/src/frame.rs#L761-L771)). `TurnAcceptCommand` and the receipt-stored `AcceptedTurn` also omit it ([`crates/haider-store/src/event_store.rs:169-205`](../../crates/haider-store/src/event_store.rs#L169-L205)).
- Turn acceptance asks for the head in branch `None`; its envelope constructor hard-codes `branch_id: None` ([`crates/haider-store/src/event_store.rs:1469-1482`](../../crates/haider-store/src/event_store.rs#L1469-L1482), [`crates/haider-store/src/event_store.rs:3049-3075`](../../crates/haider-store/src/event_store.rs#L3049-L3075)).
- Supervisor output, command-output/effect sinks, restart recovery, and parent-session chip projection all hard-code `None` ([`crates/haider-daemon/src/worker.rs:3256-3289`](../../crates/haider-daemon/src/worker.rs#L3256-L3289), [`crates/haider-daemon/src/worker.rs:4311-4338`](../../crates/haider-daemon/src/worker.rs#L4311-L4338), [`crates/haider-daemon/src/worker.rs:4367-4390`](../../crates/haider-daemon/src/worker.rs#L4367-L4390), [`crates/haider-daemon/src/turn_recovery.rs:701-737`](../../crates/haider-daemon/src/turn_recovery.rs#L701-L737), [`crates/haider-daemon/src/delegation.rs:905-935`](../../crates/haider-daemon/src/delegation.rs#L905-L935)).

The protocol golden and scope/memory tests can manufacture `Some(BranchId)`, but there is no daemon-side production origin. The practical main branch today is `None`.

### Active branch state

`SessionMetadataV1` currently contains only cwd/provider/model/output limit/system-policy/permissions/creation time ([`crates/haider-protocol/src/session.rs:29-54`](../../crates/haider-protocol/src/session.rs#L29-L54)). `SessionSummary` exposes that metadata plus a global head sequence, not branch state ([`crates/haider-rpc/src/frame.rs:660-672`](../../crates/haider-rpc/src/frame.rs#L660-L672)). The Rust TUI explicitly calls itself a single-branch port: it owns one projection/chip/queue set and only a numeric branch count ([`crates/haider-tui/src/session.rs:31-70`](../../crates/haider-tui/src/session.rs#L31-L70)); `AppModel` has an active session but no active branch ([`crates/haider-tui/src/app.rs:1883-1895`](../../crates/haider-tui/src/app.rs#L1883-L1895)).

The minimum durable branch state is a registry keyed by session and branch with:

- branch id and display name;
- source branch (optional for main), exact fork node id, and fork node's committed sequence;
- immutable creation sequence/time;
- current head node and head sequence, updated transactionally with branch-local node commits.

For compatibility, treat `None` as the legacy/main branch unless a migration explicitly rewrites old journals; inventing a concrete main id while old envelopes remain `None` creates two mains.

“Active branch” should be separated into two meanings:

1. The TUI requires `active_branch_id` per session plus per-branch projections/chips/menus/todos/context, matching the simulator. This selection decides what is displayed and which explicit branch is captured when a command is issued.
2. A daemon-stored active branch can be a roaming/default preference, but it must not be the routing authority for a turn. Multiple controllers can choose different branches; a hidden session-global default races them. Every mutation whose result is branch-scoped must carry an immutable branch coordinate. If product requirements demand shared activation, add a separately serialized, receipt-backed branch-activate/CAS operation and define it as “next turn only”; an already accepted run remains pinned to its receipt's branch.

The session replay cursor must remain session-global because `seq` is session-global. Branch projections are display/history partitions beneath that one cursor, not independent subscriptions.

## Q2

### `branch.create` wire and durable transaction

Recommended additive request:

```text
branch.create {
  command_id,
  session_id,
  worker_generation,
  source_branch_id: Option<BranchId>,
  fork_node_id: NodeId,
  fork_seq: u64,
  name: Option<String>
}
```

The node/sequence pair is deliberate. `NodeId` states the semantic fork point; `fork_seq` pins the exact committed `NodeCommitted` envelope and gives the store an unambiguous immutable boundary. The transaction must verify that `(session, source branch, fork_seq)` contains that exact node, that the node is on the source branch's declared ancestry, and that it is a root/head-agent node rather than a sibling branch or child-agent node. For B2, restrict creation to a stable terminal turn boundary or an idle compaction node; forking a live turn would duplicate apparent ownership of its open menu/tools/effects/children. Do not infer a fork point from display text or from a client-side row ordinal.

For a user-turn row, the TUI must decide which durable node the row denotes. The simulator copies the selected user and every following non-user entry through the next user boundary ([`tui.js:1652-1659`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1652>)). To preserve that behavior, the daemon-provided tree row should identify the terminal semantic node of that completed turn group, while retaining the user node as the display anchor. Forking an exact low-level node can remain legal, but the UI must not label a user row as “after the turn” and send the earlier user-node coordinate.

The response/receipt should contain stable, secret-free coordinates such as `{session_id, branch_id, source_branch_id, fork_node_id, fork_seq, created_seq, worker_generation, name}`. The store transaction should atomically insert the branch ref/head, append a durable `BranchCreated` fact for replay, and finalize the command receipt. A branch table is useful for validation/head CAS; the event is needed so an attached client can rebuild topology without trusting a response it may never have received.

Follow the existing R2 law exactly. The semantic `command_id`, not transport request id, is the durable key; same id/method/digest returns the original response across reconnect/restart, while the same id with a different body is `invalid_argument` ([`crates/haider-store/src/event_store.rs:1051-1074`](../../crates/haider-store/src/event_store.rs#L1051-L1074)). As `session.create` demonstrates, receipt lookup must precede mutable validation so a lost response still replays after the world has advanced ([`crates/haider-daemon/src/session_hub/rpc.rs:1894-1903`](../../crates/haider-daemon/src/session_hub/rpc.rs#L1894-L1903)). Fresh execution remains generation-fenced and requires a live control attachment, consistent with the current session-mutation policy ([`crates/haider-daemon/src/session_hub/mod.rs:1759-1777`](../../crates/haider-daemon/src/session_hub/mod.rs#L1759-L1777)). Include source branch, node, sequence, and name in canonical request JSON/digest; daemon-minted `branch_id` belongs in the stable response.

### Named-ref fork semantics

Production should not clone/re-id the ancestor journal as the JavaScript demo does. The protocol calls branches named refs and nodes form one single-parent tree. The new branch's head initially points at the selected source node; its first new node names that node as parent. The compiler then resolves the new branch's declared lineage and renders:

- source scopes only through each lineage fork coordinate;
- new branch scope after divergence;
- the requested agent only;
- only terminal prior runs plus the current run, preserving the W7a law.

To do this, `TreeEntry` must retain the branch scope that owns each ancestor fragment. Current rendering filters every ancestry fragment using the requested leaf branch ([`crates/haider-core/src/prompt_history.rs:447-517`](../../crates/haider-core/src/prompt_history.rs#L447-L517)); a lineage-aware projection must instead filter each selected fragment by its owning lineage branch and upper sequence boundary. Unrelated branches remain structurally excluded.

### Branch-scoped turns

Add optional `branch_id` to `turn.submit` (omission means legacy/main), include it in canonical request JSON, `TurnAcceptCommand`, and receipt-stored/returned `AcceptedTurn`. Existing turn RPC code shows every seam that must change: request digest construction ([`crates/haider-daemon/src/session_hub/rpc.rs:1462-1495`](../../crates/haider-daemon/src/session_hub/rpc.rs#L1462-L1495)), command construction ([`crates/haider-daemon/src/session_hub/rpc.rs:1511-1526`](../../crates/haider-daemon/src/session_hub/rpc.rs#L1511-L1526)), and the stable response coordinates ([`crates/haider-rpc/src/frame.rs:984-995`](../../crates/haider-rpc/src/frame.rs#L984-L995)).

Acceptance validates the branch ref, takes that branch's current head as the new user node parent, stamps the entire acceptance batch with the branch, and updates the head in the same transaction. The accepted branch must then travel through queued work, worker startup, compiler calls, `HarnessConfig`, compactor, tool/effect sinks, menus, cancellation/terminalization, and recovery. `AcceptedTurn` currently has no branch ([`crates/haider-store/src/event_store.rs:197-205`](../../crates/haider-store/src/event_store.rs#L197-L205)); restart recovery reconstructs it from run/seq/generation only ([`crates/haider-daemon/src/turn_recovery.rs:503-515`](../../crates/haider-daemon/src/turn_recovery.rs#L503-L515)). Both are branch-retarget hazards.

Session aggregate events may remain unbranched, but run states and anything prompt/UI scoped to a run must use its accepted branch. Payload type must distinguish an aggregate `SessionState` with `branch_id: None` from main-branch content; `None` cannot blindly mean both after B2 routing is introduced.

### TUI active-branch switch

The simulator's session owns `activeBranchId` and `branches[]` ([`tui.js:497-511`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:497>)); the displayed branch is looked up by that id ([`tui.js:774-780`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:774>)). All mutations take explicit session and branch ids, and queued/async work captures the branch at issuance rather than re-reading active selection later ([`tui.js:794-833`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:794>), [`tui.js:1597-1600`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1597>), [`tui.js:2018-2041`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2018>)). That capture rule is the important semantic law.

The simulator's `/tree` opens at the root branch, not necessarily the active branch ([`tui.js:1735-1741`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1735>)). It displays one branch at a time: header, user/compaction nodes, and child-fork markers immediately below their exact fork entry; breadcrumbs follow parent branches ([`tui.js:2321-2345`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2321>)). Esc climbs to the parent before leaving ([`tui.js:2507-2515`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2507>)). Enter on a fork marker drills into it; Enter on a branch/node makes that row's branch active and returns to the session; `f` forks a selected user node ([`tui.js:2574-2596`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2574>)). Rendering marks active `●` versus inactive `○` and supplies the branch/fork vocabulary ([`tui.js:3366-3427`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3366>)).

`forkAtNode` immediately activates the new branch, clears its live chips, records `forkFrom`, and returns to the session ([`tui.js:1652-1686`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1652>)). Live Rust should not fabricate that branch optimistically: issue receipt-backed `branch.create`, then let the correlated response/committed branch fact install and activate it once. A later submit captures that active id into `AppRequest`/`LiveCommand`/RPC. A branch switch must swap transcript, footprint/tokens, menus/todos, chips and branch-local draft/scroll state as one operation; it must not retarget an already queued request.

The simulator keeps run state session-wide ([`tui.js:782-791`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:782>)). B2 should keep that policy unless concurrency is explicitly broadened: switching is always allowed for reading, but a submit on branch B while branch A runs is an explicit B-scoped queued turn (or a typed refusal), never a steer silently delivered to A.

## Q3

### Why a numeric transcript index is not enough

Current `/tree` rows are strings built from the single display projection; they contain neither node id nor sequence ([`crates/haider-tui/src/app.rs:934-977`](../../crates/haider-tui/src/app.rs#L934-L977)). `TranscriptEntry::User` likewise has display data but no durable node/sequence identity, and the projector does not make `NodeCommitted` a transcript row ([`crates/haider-tui/src/projection.rs:34-58`](../../crates/haider-tui/src/projection.rs#L34-L58), [`crates/haider-tui/src/projection.rs:388-397`](../../crates/haider-tui/src/projection.rs#L388-L397)). B2 must preserve a stable node-to-display-entry association from replay/tree projection; text matching and adjacency inference are not honest identities.

The scroll model is row-based and render is the authority. `scroll_back` is rendered rows from the bottom, while `scroll_max` is written/reconciled by the last frame ([`crates/haider-tui/src/app.rs:1935-1946`](../../crates/haider-tui/src/app.rs#L1935-L1946)). Render expands entries into logical lines, calculates width-dependent wrapped-row prefix sums with `Paragraph::line_count`, computes `max_scroll`, and only then derives the viewport ([`crates/haider-tui/src/render.rs:1778-1823`](../../crates/haider-tui/src/render.rs#L1778-L1823)). Sticky-user jumping already converts a producing prompt's logical line into a bottom offset ([`crates/haider-tui/src/render.rs:1832-1845`](../../crates/haider-tui/src/render.rs#L1832-L1845)), and the reducer applies the value-carrying offset ([`crates/haider-tui/src/app.rs:6060-6069`](../../crates/haider-tui/src/app.rs#L6060-L6069)).

### Honest jump design

Use a durable pending anchor, resolved by render:

1. Replace string tree rows with typed `Branch`, `Fork`, and `Node` rows carrying stable `{branch_id, node_id, node_seq}` plus a display-entry anchor. Enter on a fork drills only; Enter on a branch returns at the branch's normal tail; Enter on a node activates the branch and sets `pending_jump = {branch_id, node_id}`.
2. In the first session render for that branch, record the starting logical line for every display entry while calling `transcript_lines`. For a user row, anchor the actual prompt line rather than its preceding spacer, matching the existing sticky map at [`crates/haider-tui/src/render.rs:1780-1786`](../../crates/haider-tui/src/render.rs#L1780-L1786).
3. In the same frame, resolve node to entry index, entry index to logical line, and logical line to wrapped row using the renderer's current width/row prefix sums. Set `target_top = min(target_row, max_scroll)` and `scroll_back = max_scroll - target_top`. Clear the pending anchor only after it resolves; suppress sticky until a real wheel event.
4. A near-tail target cannot be top-aligned without fake padding. Clamp it honestly and optionally highlight the target for one frame. If replay has not materialized the node, retain the pending anchor until catch-up or show a precise “node unavailable” status; never guess another entry.

An entry index can be the renderer's internal lookup, but the cross-screen pending identity must remain `{branch,node}`. Do not cache wrapped row offsets: resize and terminal width change them. Current Rust Enter merely returns to the session, and `f` is still a stub ([`crates/haider-tui/src/app.rs:2738-2756`](../../crates/haider-tui/src/app.rs#L2738-L2756)); the current tree renderer also has no semantic row hit map ([`crates/haider-tui/src/render.rs:2225-2261`](../../crates/haider-tui/src/render.rs#L2225-L2261)).

The JavaScript hint says “jump,” but its Enter arm only changes `activeBranchId` and screen—no transcript anchor is carried ([`tui.js:2581-2591`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2581>), [`tui.js:3350-3354`](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3350>)). B2 should implement the promised effect, not port that honesty gap.

## Q4

### B2a — daemon/core/protocol/store

Scope:

1. Add the feature gate, branch descriptor/created event, additive `branch.create` request/response, and branch id on turn acceptance/response. Keep old wire bytes compatible by omitting optional main-branch fields.
2. Add the durable branch registry/head and one atomic R2 transaction for branch creation. Validate exact node/seq/source lineage; publish the committed branch event only after transaction success.
3. Carry branch through acceptance, queued work, recovery, worker startup, compiler, actor, tool/effect/menu/cancel/terminal output, compaction, and parent delegation projections. The hard-coded `None` sites identified in Q1 form the mechanical audit list.
4. Extend the already branch-scoped compiler with named-ref lineage: source ancestors stop at each fork coordinate, each fragment renders under its owning scope, siblings stay excluded. Do not clone/re-id durable ancestors.
5. Make compaction branch-ancestry scoped. Current automatic planning and head lookup pass `None` ([`crates/haider-daemon/src/worker.rs:145-160`](../../crates/haider-daemon/src/worker.rs#L145-L160), [`crates/haider-daemon/src/worker.rs:244-276`](../../crates/haider-daemon/src/worker.rs#L244-L276)); manual compile/plan also pass `None` ([`crates/haider-daemon/src/worker.rs:2474-2493`](../../crates/haider-daemon/src/worker.rs#L2474-L2493)). Both must use the accepted/selected branch.
6. Keep compaction's existing global-session head CAS as the safe B2 minimum: it snapshots the global latest sequence and rejects if any session event advances it ([`crates/haider-daemon/src/worker.rs:244-256`](../../crates/haider-daemon/src/worker.rs#L244-L256), [`crates/haider-daemon/src/session_hub/actor.rs:202-226`](../../crates/haider-daemon/src/session_hub/actor.rs#L202-L226)). It is conservatively correct but sibling traffic can cause false retries. Refine it only by adding an atomic selected-branch head/ancestry CAS; never simply ignore the global mismatch without that replacement.
7. Recovery must derive and retain each run's branch. Current reduction records user sequence but not envelope branch, and reconstructs acceptance without it ([`crates/haider-daemon/src/turn_recovery.rs:297-311`](../../crates/haider-daemon/src/turn_recovery.rs#L297-L311), [`crates/haider-daemon/src/turn_recovery.rs:503-515`](../../crates/haider-daemon/src/turn_recovery.rs#L503-L515)). Manual compaction's pending-receipt journal fallback must also match branch, not only operation id/node ([`crates/haider-daemon/src/worker.rs:2311-2369`](../../crates/haider-daemon/src/worker.rs#L2311-L2369)).
8. Delegation must pin the parent branch in `SpawnCoordinates`/`DelegationRecord` and all parent-side chip/report/tool-result emissions. Those structures currently record parent session/run/agent but no branch ([`crates/haider-daemon/src/delegation.rs:54-66`](../../crates/haider-daemon/src/delegation.rs#L54-L66), [`crates/haider-store/src/event_store.rs:119-138`](../../crates/haider-store/src/event_store.rs#L119-L138)). A child session can retain its own main branch; its parent-visible effects stay on the source parent branch even if the UI later switches.

Minimum B2a law tests:

- **R2 fork receipt:** response loss/restart retry returns the exact original branch id/coordinates; same id with changed node, seq, source branch, or name is rejected; revert/transaction mutation cannot leave branch/event without receipt or vice versa.
- **Fork-coordinate validation:** node and sequence must match the same session/source lineage/agent; a sibling or fabricated pair is rejected. Multiple distinct commands may legally fork the same node.
- **Lineage compilation:** a fork sees exactly the source prefix through its fork, then its own history; it excludes the source suffix, siblings, wrong agents, and nonterminal output. Extend, do not replace, the existing W7a scope law.
- **Branch propagation:** user/node/run/item/tool/effect/menu/terminal envelopes for a non-main turn all carry its branch; main remains `None`; a response-replay or restart does not move the run to a later active branch.
- **Queued/recovery:** interleaved accepted runs on A/B start on their receipt-pinned branches after restart. Cancellation and checkpoint recovery emit on the original branch.
- **Compaction divergence:** fork before a source compaction renders original immutable ancestry; fork after it inherits the applicable summary; a new compaction affects only its branch; sibling traffic can at most force a safe retry and never orphan or incorrectly validate it; pending manual compaction resumes/finalizes on the same branch. Pin both crash boundaries: an intent-only crash is abandoned without changing the prompt ([`crates/haider-daemon/src/context_core_tests.rs:304-312`](../../crates/haider-daemon/src/context_core_tests.rs#L304-L312), [`crates/haider-daemon/src/context_core_tests.rs:440-487`](../../crates/haider-daemon/src/context_core_tests.rs#L440-L487)), while a committed node with an unfinished receipt is found by deterministic operation/node ids and finalized ([`crates/haider-daemon/src/worker.rs:2311-2370`](../../crates/haider-daemon/src/worker.rs#L2311-L2370), [`crates/haider-daemon/src/worker.rs:2410-2430`](../../crates/haider-daemon/src/worker.rs#L2410-L2430)).
- **Delegation:** a child spawned on A remains represented on A after switching to B; late report/tool outcome cannot paint B; a fork starts with no duplicated live child execution.

### B2b — TUI/live client

Scope:

1. Unfold `SessionState` into session-global cursor/lifecycle plus `active_branch_id` and actual branch records. Each branch owns projection, context footprint/tokens, menus/todos, chips, and any branch-local display state. Derive branch count rather than storing the current scalar.
2. Admit every envelope once through the session-global sequence cursor, then route by branch and then agent. Current routing chooses session and passes only payload plus `agent_id` ([`crates/haider-tui/src/app.rs:5224-5293`](../../crates/haider-tui/src/app.rs#L5224-L5293)); branch must become the outer scope. Inactive branch events must still materialize so switching is immediate.
3. Add `AppRequest::BranchCreate`, `LiveCommand::BranchCreate`, link/RPC mapping, receipt correlation, and branch capture on `SubmitText`. Current branch-less seams are `AppRequest::SubmitText` ([`crates/haider-tui/src/app.rs:1362-1370`](../../crates/haider-tui/src/app.rs#L1362-L1370)), `LiveCommand::Submit` ([`crates/haider-tui/src/live.rs:91-97`](../../crates/haider-tui/src/live.rs#L91-L97)), and the link's `TurnSubmit` construction ([`crates/haider-tui/src/link.rs:543-556`](../../crates/haider-tui/src/link.rs#L543-L556)).
4. Implement typed tree rows, root/drill/breadcrumb/Esc behavior, active marker, `f` and `/fork`, stable value-carrying hits, and the render-resolved jump from Q3. The existing `/tree` command already gates to a session but only opens the main-line screen ([`crates/haider-tui/src/app.rs:4909-4917`](../../crates/haider-tui/src/app.rs#L4909-L4917)); `/fork` remains an honest stub ([`crates/haider-tui/src/app.rs:5129-5141`](../../crates/haider-tui/src/app.rs#L5129-L5141)).
5. Switching branches must atomically swap all branch-local surfaces and close/reset a stale subagent view path. Preserve the session-global attachment and replay cursor. Capture the branch in every pending mutation/menu coordinate so a later switch cannot retarget completion.
6. Gate live behavior on the advertised branch feature. Without it, keep an honest unsupported notice; never fabricate a live branch locally. Demo/persistence can mirror the simulator, but durable live truth remains daemon-owned.

Minimum B2b law tests:

- **Topology/navigation:** nested fork markers occur immediately under the exact fork node; drill and Esc walk parent/root; `●` follows the session's active branch; selection/window clamping works; a stale value-carrying hit cannot activate a replaced row.
- **Fork issuance:** `f` emits exact session/source-branch/node/seq; no live branch appears before daemon truth; replayed success installs one branch and activates only the originating session; typed failure leaves topology unchanged.
- **Activation/capture:** branch switch swaps transcript/header/tokens/todos/chips and survives session A → session B → session A checkout; a submit queued before a later switch still carries the original branch.
- **Jump geometry:** long wrapped text, explicit newlines, wide glyphs, narrow/wide widths, resize, and near-tail targets all land on the correct visible entry; sticky does not cover the revealed row.
- **Replay:** interleaved session seqs for A/B advance one continuous cursor and materialize both branches; switching neither attaches nor rewinds; detach/reattach duplicates neither. Attach is session plus `after_seq`, not branch ([`crates/haider-rpc/src/frame.rs:748-756`](../../crates/haider-rpc/src/frame.rs#L748-L756)), and the current strict cursor lives inside the single projection ([`crates/haider-tui/src/projection.rs:248-295`](../../crates/haider-tui/src/projection.rs#L248-L295)); split those responsibilities carefully.
- **Children/compaction:** inactive-branch child updates remain there, forked branch has no live chip, launcher aggregate counts all branches, and sibling compaction/footprint never leaks into the active branch.

## Risks

1. **Compaction ancestry versus divergence is the highest semantic risk.** A compaction node is an immutable projection switch over an ancestry range ([`crates/haider-core/src/prompt_history.rs:343-438`](../../crates/haider-core/src/prompt_history.rs#L343-L438)). A fork before it must bypass that later switch and recover original envelopes; a fork after it may inherit it. Selecting compactions by session-global order or active branch at recovery will leak summaries across siblings or hide forkable originals.
2. **Exact W7a scoping is necessary but not sufficient for a named-ref fork.** Simply parenting B to an A node fails today's filtered index; simply allowing all branches breaks the existing isolation law. Resolve only the registered lineage, with per-fragment owning scope and fork-sequence ceilings.
3. **Per-branch replay cursors are incorrect.** Envelope `seq` is monotonic per session ([`crates/haider-protocol/src/envelope.rs:37-48`](../../crates/haider-protocol/src/envelope.rs#L37-L48)). If A sees seq 10 then 12 because seq 11 belongs to B, A's current strict projector reports a false gap. Admit once at session scope, route afterward, and keep inactive branches warm.
4. **Mutable active-branch lookup can retarget asynchronous work.** Turn submission, queued runs, menus, compaction, cancellation, and child completion must carry their original branch. A daemon-global active branch is at most a preference; it is unsafe as the execution coordinate for multiple controllers.
5. **Delegation is parent-branch scoped even when the child is another session.** Current records lack that coordinate and current chip projection hard-codes main. Forking must copy historical parent-visible facts through the fork point, never a live child lease/process; late child output stays on its originating parent branch.
6. **Aggregate versus main-branch `None` must be explicit.** Legacy main content and session-global events both currently use `None`. Route aggregate payloads by type before branch routing, or introduce an explicit scope distinction; otherwise session status can disappear from all forks or pollute main history.
7. **Branch head is not “latest session seq.”** Interleaved sibling events make global max seq unsuitable for parent selection. The current global compaction CAS remains a safe, over-conservative guard, but a future less-conflicting CAS must use an atomic branch head. Likewise, queued turns can expose subtle parent-ordering problems if later user nodes commit before earlier assistant nodes; tests must pin the intended branch-head/run ordering rather than choosing the last `NodeCommitted` by session sequence.
8. **TUI checkout state can strand branch data.** The present architecture checks one projection/chip set out of a session slot. Adding a second branch checkout layer without one atomic authority risks drafts, menus, chips, or scroll offsets crossing branches. Test branch/session round trips, not only same-session toggles.
9. **Tree compilation already scans the full journal.** W7a's optimization ledger flags the full read and quadratic compaction validation at 3,000 envelopes/100 compactions ([`docs/OPTIMIZATIONS.md:294-305`](../OPTIMIZATIONS.md#L294-L305)). Branch lineage adds lookup pressure; correctness should remain event-transaction-derived, with indexes/caches added only under those measured triggers.
10. **The simulator is a UX oracle, not a durable-data oracle.** Its fork clones/re-ids entries and its “jump” does not scroll. Production should preserve the simulator's root/drill/active/fork interaction while using shared immutable node ancestry and an actual render-authoritative anchor jump.
