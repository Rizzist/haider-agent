# Per-subagent metrics view — proposal (elapsed · tools · tokens · cost)

> gpt-5.6 Sol read-only research, 2026-08-11. No-code proposal. Sequenced AFTER parallel tool calls. Cost is cache-aware (CM1) and — per owner — gated on metered/API-key auth (OAuth/subscription accounts omit $).

# Recommended first slice

Add a daemon-derived, direct-agent metrics snapshot and extend the existing S4 right-aligned seam to render:

`25m 18s · live · 6 tools · 34K tokens · $0.27`

For completed agents, omit `live`. Scope this slice to each agent’s own totals—no descendant rollup yet. The row edit itself is small, but the overall slice is small-to-medium because the parent TUI currently receives neither child tool items nor child usage: the delegation mirror forwards only child prompts and run-state-derived chip states ([delegation.rs:1217](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:1217)), while S4 currently obtains tokens through a child-session summary join ([app.rs:1156](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:1156)).

Prefer one compact replace-by-`head_seq` metrics snapshot over mirroring every raw child event. Compute and price it in the daemon—the TUI does not depend on `haider-provider` today ([Cargo.toml:9](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/Cargo.toml:9)).

## 1. What to show per subagent row

The wide inline row should become:

`elapsed · live? · N tools · NK tokens · $cost`

Examples:

- Running: `25m 18s · live · 6 tools · 34K tokens · $0.27`
- Settled: `42s · 12 tools · 39K tokens · $0.40`
- Unknown price: `42s · 12 tools · 39K tokens · $—`

This extends, rather than replaces, the S4 layout. Today the row already derives elapsed and tokens, budgets a two-cell gap, pads the metadata to the right edge, and renders it dimly ([render.rs:4195](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:4195)). Elapsed already ticks while live and freezes from journal timestamps at terminal state ([app.rs:1020](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:1020)).

Definitions:

- **Tools:** attempted model tool calls, counted once per durable `ToolCall` item.
- **Tokens:** cumulative agent consumption aligned with the cost fold—not merely the latest context footprint. Prefer normalized `logical_input + billed_output`, adding reasoning only when its accounting says it is additional. Cached reads are part of logical input and must not be added again; normalized usage explicitly distinguishes logical, uncached, cache-read/write input and billed output ([provider.rs:202](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/provider.rs:202)).
- **Cost:** cache-aware estimated total cost, not `CacheCostEstimate.input_with_cache_usd` alone. `CacheCostEstimate` is explicitly input-only ([provider.rs:281](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/provider.rs:281)); total pricing belongs to `estimate_normalized_usage_cost_usd`, which also avoids reasoning double-counting ([pricing.rs:463](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/pricing.rs:463)).
- **Live:** means partial totals. The existing animated state glyph remains, but the explicit label prevents a partial dollar figure from looking final.

For very small nonzero costs, increase precision rather than showing `$0.00`; keep the owner’s two-decimal form for ordinary values.

Width behavior should retain S4’s whole-segment, never-truncate mechanism ([render.rs:4058](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:4058)). Recommended shedding order is `live`—already conveyed by the state glyph—then elapsed, then cost, then tools, leaving the existing token figure last. That deliberately revises S4’s current tokens-first shedding test and should be pinned explicitly ([s4_subagent_rows_tests.rs:469](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/tests/s4_subagent_rows_tests.rs:469)).

The existing row click already opens a dedicated subagent view ([app.rs:10418](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:10418)). Add an expandable/detail block there:

- `own — 6 tools · 34K tokens · $0.27`
- `subtree — 12 tools · 39K tokens · $0.40` when it has descendants
- `tokens — in 30K · out 4K · cached 21K · cache write 2K`
- `cache — 70.0% hit` or `hit n/a`
- Per-model lines: provider/model, main/delegated/compaction lane, tokens and cost
- Later, after the error wave: `8 ok · 2 failed · 1 denied · 1 cancelled`

Cache-hit percentage must be shown only when telemetry covers all logical input; the existing fold already returns `None` for incomplete coverage and distinguishes missing telemetry from a real 0% hit rate ([cache_usage.rs:200](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/cache_usage.rs:200)).

## 2. Where the metrics appear

### Inline S4 subtree

Extend the existing recursive `render_subtree` row. It is already shared between Session and Subagent screens and renders depth-first rows ([render.rs:4079](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:4079), [render.rs:4128](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:4128)).

Add a summary beneath its header:

`subagents total — 31 tools · 112K tokens · $0.94`

This total excludes the main agent so the label is unambiguous.

### `/usage`

Add an `AGENTS — CURRENT SESSION` block immediately after the existing current-session cache block. `/usage` already renders live session cache totals and provider/model/cache-epoch/request-lane breakdowns there ([render.rs:2017](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:2017)).

Suggested structure:

- `session total` — main plus all descendants
- `main` — head-agent direct metrics
- One row per subagent — direct and subtree totals
- Selected-agent expansion with normalized token/cache/model detail

Do not bury this under account tabs: the durable `/usage` report is currently account-centric and its public shapes contain no run or agent field ([usage.rs:14](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/usage.rs:14), [usage.rs:78](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/usage.rs:78)).

The session rollup line should be:

`session total — 44 tools · 146K tokens · $1.21`

Here, “session” means the root/head agent plus every descendant exactly once.

### Future fleet view

The fleet screen should show each node’s subtree total by default, with expansion revealing `own` versus `children`. The design should not claim that this screen exists today: `Screen` has Session, Subagent and Usage but no Fleet variant ([app.rs:195](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:195)).

The current UI has useful recursive scaffolding—`ChipModel.children`, depth-first flattening and recursive live counting ([app.rs:808](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:808), [app.rs:1149](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:1149), [app.rs:1604](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:1604))—but production does not yet assemble a complete cross-session fleet. Recursive children spawn in separate child sessions, and the current mirror does not forward descendant `AgentSpawned` facts ([delegation.rs:1217](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:1217)).

## 3. How metrics are computed

### Tool count

Fold the durable child-session journal. Envelopes already carry exact session, run and optional agent identity ([envelope.rs:37](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/envelope.rs:37)); child actors stamp their configured `agent_id` on every envelope ([actor.rs:4064](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:4064)).

Maintain a set keyed by `(session_id, item_id)`:

1. On the first `Started` `TurnItem::ToolCall`, increment attempts.
2. If historical/replayed data contains only `Completed`, count it as the fallback.
3. Never count `Delta`.
4. Never count the same item twice.

This matches the item lifecycle: `Started → Delta* → Completed`, keyed by `ItemId` ([item.rs:82](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/item.rs:82)). The projection already replaces a started block with its completion and suppresses duplicate item IDs ([projection.rs:593](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/projection.rs:593)).

Count `ToolCall` only, not the separate `CommandExecution` variant ([item.rs:25](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/item.rs:25)). A `spawn_subagent` call counts in the caller’s direct bucket; the child’s later calls belong to the child.

### Tokens and cost

Reuse CM1’s latest-snapshot rule exactly. The fold key is:

`(run, agent, provider, model, cache epoch, request kind)`

The daemon already keeps this key and replaces prior cumulative snapshots rather than summing them ([usage_report.rs:667](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/usage_report.rs:667), [usage_report.rs:714](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/usage_report.rs:714)). Delegated usage already has `run`, `agent`, and `DelegatedAgent` request kind stamped before it is journaled ([actor.rs:4461](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:4461)).

After replacement:

- Sum normalized logical input, billed output, cache detail and costs across distinct keys.
- Keep main, delegated and compaction lanes additive.
- Group detail by agent, then provider/model/request lane.
- Carry an `all_lanes_priced` flag; one unknown-price lane makes the compact aggregate `$—`.

The daemon’s existing `SessionFolder` already performs normalized total pricing when available and legacy compatibility pricing otherwise ([usage_report.rs:765](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/usage_report.rs:765)). It currently loses agent identity when `finish` aggregates into account totals ([usage_report.rs:853](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/usage_report.rs:853)); the new plumbing must preserve that dimension.

The live TUI fold also already keys by agent, but `totals()` collapses the result to provider/model/epoch/request kind and exposes no per-agent accessor ([cache_usage.rs:11](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/cache_usage.rs:11), [cache_usage.rs:53](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/cache_usage.rs:53)). That is reusable logic, not currently usable row data.

### Live versus settled

Publish a compact snapshot keyed by agent and child-session `head_seq`. The same freshness pattern already exists in `SessionSummary`, whose counts are paired with a committed head sequence ([frame.rs:799](/Users/rizzist/haider-run/b2b-tui/crates/haider-rpc/src/frame.rs:799)).

- While the child is live, replace the snapshot whenever its journal head advances and display `live`.
- On Done, Error or Cancelled, force one snapshot through the terminal sequence and remove `live`.
- On replay/restart, rebuild from the durable journal.
- Do not reset metrics on failure or cancellation; they are partial but valid committed work.

The parent’s current live mirror already pages through the child journal, making it a reasonable first transport seam, but it should publish a compact metric snapshot rather than copy every tool and usage event into the parent journal ([delegation.rs:1223](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:1223)).

## 4. Aggregation model

Store **direct/exclusive** metrics at each node. Derive inclusive values postorder:

`subtree(node) = direct(node) + Σ subtree(child)`

Rules:

- A leaf’s direct and subtree totals are identical.
- A parent row’s compact headline shows subtree totals.
- Its detail view separates `own` and `children`.
- The root/session total is main-agent direct metrics plus every top-level subagent subtree.
- A parent’s `spawn_subagent` attempt remains in the parent bucket; the child’s work remains in the child bucket, so nothing is counted twice.

The durable relation already provides the needed graph coordinates: `agent_id`, `child_session_id`, `parent_agent_id`, `root_session_id` and depth ([event_store.rs:131](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:131)). The implementation should not encode today’s depth limit into the fold; execution currently caps recursion at three, but the aggregation should accept arbitrary depth ([delegation.rs:42](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:42), [delegation.rs:1042](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:1042)).

Cost completeness propagates upward: if any descendant has an unpriced lane, the parent and session totals show `$—`. Tokens and tool attempts can still roll up independently.

## 5. Edge cases

- **Unknown model:** display `$—`, never `$0`. Model lookup deliberately returns `None` for unknown families ([pricing.rs:345](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/pricing.rs:345)); existing `/usage` renders an absent estimate as `est —` ([render.rs:2298](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:2298)).
- **Mixed known and unknown models:** aggregate cost is `$—`, not the sum of only known lanes. Do not copy the current device footer’s `filter_map` behavior, which can show a partial known-cost sum ([render.rs:2383](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:2383)).
- **Cache telemetry unavailable:** cache hit and savings are `n/a`. For a known model, total cost may still be shown as a conservative estimate because normalized pricing falls back to billing logical input at the normal rate ([pricing.rs:463](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/pricing.rs:463)).
- **Still running:** show committed partial totals with `live`; never extrapolate final cost.
- **Failed or cancelled:** retain partial totals and freeze them at the terminal sequence. Open tools are durably closed as Failed or Cancelled on terminal paths ([actor.rs:3472](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3472)).
- **No usage truth:** preserve S4’s honesty discipline—do not invent `0 tokens`. Its current join returns `None` when no child source knows the value ([app.rs:1156](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:1156)).
- **Denied/failed tools:** count attempts. Tool starts are journaled before parsing, approval and dispatch ([actor.rs:2528](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2528)).
- **Success breakdown caveat:** do not infer success from outer `ToolStatus::Completed`. Policy denials can be encoded inside a typed `ToolResult`, returned as a completed dispatch, and closed as Completed ([worker.rs:5290](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:5290), [worker.rs:5534](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:5534)). The existing error audit identifies this exact green-looking tool problem ([error-handling-analysis-gpt56.md:347](/Users/rizzist/haider-run/b2b-tui/docs/research/error-handling-analysis-gpt56.md:347)).

## 6. Parallel tool calls

A batch containing N calls must contribute **N tools**, not one batch.

Each provider `ToolCallStart` gets a fresh durable item ID, and core can hold multiple open tool accumulators ([actor.rs:2528](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2528)). The current subturn cleanup explicitly accounts for parallel calls whose arguments may still be partial ([actor.rs:3492](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3492)).

Display nuance:

- Compact row: only `N tools`.
- Detail: show individual call outcomes once the error wave provides honest per-tool terminal status.
- Do not show “N batches” or peak concurrency yet. `TurnItem::ToolCall` has call identity but no batch/wave identity ([item.rs:25](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/item.rs:25)).
- The metrics fold is ready for parallel dispatch as long as the parallel-tool work preserves one item lifecycle per call.

Delegated spawning has an additional current constraint: its coordinator explicitly documents a sequential spawn/report pairing contract ([delegation.rs:7](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:7)). The parallel-tool wave must resolve that execution constraint separately; the counting rule itself does not change.

## 7. Build order and effort

1. **Direct inline metrics — small/medium.** Factor an agent-preserving metrics fold from the existing durable usage fold; add unique tool-attempt counting; publish direct child snapshots; store them beside `ChipModel`; extend S4’s row metadata. CM1 pricing is already sufficient. The main work is transport/projection, not rendering.

2. **Subagent detail and `/usage` — medium.** Add own token/cache/model breakdown, an `AGENTS — CURRENT SESSION` block, and a session total. Preserve the agent dimension before the current account aggregation discards it.

3. **Honest outcome breakdown — medium, after the error wave.** Add `ok/failed/denied/cancelled` only when terminal tool outcomes stop hiding inside successful-looking `ToolResult`s. Total attempts do not depend on this work.

4. **Fleet tree and subtree rollups — large.** Assemble the durable delegation graph across child sessions, expose parent/root coordinates to the client, compute postorder rollups, and support the future fleet screen. The recursive renderer is available, but production cross-session ancestry/materialization is not.

5. **Polish and compatibility — small.** Preserve the old S4 elapsed/token row when connected to an older daemon with no metrics snapshot; add wide/narrow rendering laws, unknown-price cases, mixed-price subtree cases, replay idempotency, live-to-settled transitions, failures/cancellation, and parallel N-call batches.

No files were modified and no builds were run.

## 8. OAuth / subscription auth — omit $ (owner requirement)

Per-token `$` is only meaningful for **metered / API-key** accounts. OAuth /
subscription accounts (Claude Pro/Max, ChatGPT Plus, Kimi Code Plan, Codex)
pay a flat plan, so a per-token dollar figure priced at the pay-as-you-go API
rate is misleading — the marginal cost is plan-covered.

Rule: **gate the `$` on auth-method = ApiKey.** For an OAuth/subscription lane
show `elapsed · tools · tokens` and omit the dollar segment (optionally a quiet
`· plan` marker). This is the same "don't render a misleading number"
discipline CM1 already applies for missing telemetry (`n/a`) and unknown price
(`$—`) — an OAuth lane is not "unknown price", it is "not metered".

Mechanism: `UsageScope` already carries `auth_scope`; the per-(run,agent) fold
should also carry the credential's `AuthMethod` per lane. Aggregation: a subtree
that mixes metered and OAuth lanes shows `$` for the metered portion only and
labels it partial (e.g. `$0.27 (metered lanes)`), or omits `$` entirely if the
head agent is OAuth — pick one rule and pin it. Tokens/tools/elapsed always show
regardless of auth.

Applies to BOTH the new per-subagent view AND the already-shipped CM1 `/usage`
cost display — verify whether `/usage` currently gates cost on auth; if not, the
same OAuth-omit-$ fix belongs there (small follow-up), so we never show an
API-equivalent dollar cost for a subscription session.
