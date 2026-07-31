# W7 architecture research — context management

Read-only audit completed. No files were modified.

The central finding is that W7’s protocol vocabulary already exists, but its runtime foundation does not: compaction nodes, resume causes, `RunState::Compacting`, and transcript items are frozen, while production still compiles prompts directly from journal events and never emits a durable history tree. W7a therefore must make the tree/projection contract real before overflow recovery can be correct.

## Q1 — daemon/core seams

### 1. Prompt assembly today

A live turn is durably accepted as `Queued + UserMessage`, optionally with `SessionState::ActiveRun`, in one transaction. The user message is marked `PromptRender::Verbatim`; lifecycle state is `Omit` ([event_store.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:1109), [event_store.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:1186)).

Before starting provider work, the worker calls `PromptHistoryCompiler::compile`, resolves CAS-backed attachments, builds `HarnessConfig`, and submits the already-committed turn ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:1794), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:1845), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:1874)).

The compiler currently:

- Pages the entire session journal.
- Filters by branch and agent.
- Includes only prior `RunState::Done` runs plus the current accepted user message.
- Excludes partial output from interrupted/errored runs.
- Reconstructs user, assistant, tool-call/result, and provider-opaque messages.
- Skips only envelopes whose prompt target is `Omit`.

Evidence: [prompt_history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/prompt_history.rs:17), [prompt_history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/prompt_history.rs:44), [prompt_history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/prompt_history.rs:69), [prompt_history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/prompt_history.rs:83).

Inside the actor, that compiled vector becomes mutable in-memory history. Every provider round clones it into `TurnRequest`; tool results and nudges extend it between rounds ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:948), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:952), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:976), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1391)).

So today the real pipeline is:

`journal envelopes → terminal-run filter → Message[] → mutable actor tail → TurnRequest`

It is not yet `tree → render plan → prompt`.

#### Render-target status

`RenderTargets` and `PromptRender::{Verbatim, Pruned, Omit}` are frozen at [envelope.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/envelope.rs:17). Actor helpers consistently stamp conversation content as `Verbatim`, bookkeeping as `Omit`, and provider-native opaque continuation as prompt-visible but UI-hidden ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:3280)).

However, `Pruned` is presently a stub: the compiler checks only `Omit`, so `Pruned` behaves exactly like `Verbatim`. W7 must give pruning/compaction a projection-level meaning rather than mutating historical envelopes.

### 2. Frozen tree/compaction contracts: existing versus live

The protocol already freezes:

- `TreeNode { node, parent, kind }`.
- `NodeKind::Compaction`.
- Inclusive covered endpoints.
- A CAS summary reference.
- Before/after token counts.
- `CompactionResume::{AutoMidTurn, ManualIdle}`.

See [history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/history.rs:10), [history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/history.rs:43), and [history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/history.rs:68).

The client-facing equivalent is `TurnItem::ContextCompaction`, with optional before/after counts ([item.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:52)). `RunState::Compacting` exists and is explicitly classified as parked/auto-continuing ([state.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:52), [state.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:131)). `NodeCommitted` is part of the durable event union ([lib.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/lib.rs:72)).

But production currently emits no `NodeCommitted` events at all. There is no tree head, no branch ancestry projector, no compaction-node producer, and no compiler arm for compaction. The only non-test producer of `ContextCompaction` is the demo TUI script.

#### What compaction should replace

A compaction must not delete or rewrite envelopes. The frozen contract says the covered range remains navigable and forkable.

The correct semantic model is an immutable overlay:

1. The compaction node is appended after the current head.
2. `covers_from..=covers_to` identifies a contiguous range on that node’s ancestry.
3. The compiler skips that range for the active projection.
4. It injects the content of `summary_artifact` at the cut.
5. Nodes after `covers_to` and descendants after the compaction node remain verbatim.
6. Forks whose heads precede the compaction continue compiling the original history.

Thus compaction does not “replace one prefix node”; it adds a node instructing the compiler to substitute a summary for a covered ancestral interval.

Nested compactions need validation: both endpoints must exist on the active ancestry, `covers_from` must precede `covers_to`, and a later compaction must not create ambiguous crossing ranges.

#### Important tree-rendering gap

`NodeKind::AssistantCommit` carries text but not provider-opaque continuation state, and `NodeKind::ToolExchange` carries only a summary, not exact call IDs/results. The current journal compiler preserves those provider-valid details.

Therefore a naive “serialize `NodeKind` directly to messages” rewrite would regress tool and reasoning continuation. The safer architecture is:

`active tree ancestry → inclusion/substitution plan → render eligible journal fragments/CAS artifacts → Message[]`

The tree decides what history exists in the prompt; render targets and provider sidecars decide how the selected history is encoded.

### 3. CAS/compiler seam

A compaction node stores only `summary_artifact`. Core’s `StoreHandle` exposes append/read/latest but no artifact access ([lib.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/lib.rs:64)). The daemon’s `HubStoreHandle` has CAS put/get methods ([mod.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/mod.rs:2030)).

W7a therefore needs either:

- An additive artifact-reader port passed to the prompt compiler; recommended because it keeps the event-store contract narrow.
- Or CAS reads added to `StoreHandle`, with `MemoryStore` and SQLite implementations.

The compiler must fail as `StoreCorrupt` when a committed compaction node references a missing or invalid summary. It must never silently fall back to the original range after the durable projection has switched.

### 4. Model context windows: where the path stops

W5g-1 already carries provider-declared windows through:

`DiscoveredModel.context_window → cached model source → ProviderSummaryWire.model_details → account management snapshot → TUI`

Evidence: [catalog.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/catalog.rs:44), [provider_registry.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/provider_registry.rs:456), [frame.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:512), [accounts.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/accounts.rs:595).

`AccountsProviderFactory` already holds the management snapshot and can look up the provider summary ([accounts.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/accounts.rs:4643), [accounts.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/accounts.rs:4729)).

The window is then dropped:

- `ResolvedTurnProvider` has no context-window field ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:68)).
- `resolve_for_turn` does not copy model details into its result ([accounts.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/accounts.rs:5012)).
- `HarnessConfig` has model and output `max_tokens`, but no context window ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:73)).

Minimal bridge:

- Add `context_window: Option<u64>` to `ResolvedTurnProvider`.
- Resolve it by exact provider/model match in `ProviderSummaryWire.model_details`.
- Pin it into `HarnessConfig` for the logical turn.

Do not use `Provider::capabilities().context_limit` as fallback. Those values are presently inferred from model-name tables, while W5g explicitly requires provider-declared windows and `None` when the provider does not declare one ([W5g-1 brief](/Users/rizzist/haider-run/haider-agent/docs/briefs/W5g-1-context-windows-brief.md:13)).

When the window is `None`, threshold auto-compaction must stay disabled. Manual compaction and provider-reported forced compaction can still work.

### 5. Usage and context-meter truth

`Usage` is accounting telemetry, not a context-footprint snapshot.

Core initializes `completed_usage` per logical turn, combines usage across repeated provider requests, and emits cumulative snapshots ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:948), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1286), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:3158)).

Consequences:

- The daemon has no session-wide “tokens currently in the next prompt” value.
- Summing durable Usage events double-counts repeated cumulative frames.
- Multi-request tool turns bill the same history repeatedly; cumulative input is not current prompt size.
- OpenAI reports cached tokens as a subset of input, whereas Anthropic stores non-cached and cache-read input separately. The TUI’s current `input + cached` formula can therefore double-count OpenAI.
- The latest `Usage` source may be provider-reported, locally exact, or estimated, but that source describes the accounting frame—not necessarily the compiled prompt’s current footprint.

The TUI currently acknowledges this as an approximation and sums input, cached, output, and reasoning ([projection.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/projection.rs:698)).

W7b should introduce a separate durable context-footprint snapshot, for example:

```text
ContextFootprint {
    used_tokens,
    context_window,
    reserved_output,
    source,        // provider_reported | locally_exact | estimated
    as_of_seq
}
```

The exact name is secondary; separating occupancy from billing is the critical law.

Provider-reported exactness should be derived from request-local usage before it is folded into logical-turn cumulative billing. Where a tokenizer is exact, publish `LocallyExact`; otherwise publish and visibly label `Estimated`.

### 6. Reserved output budget

`SessionMetadataV1.max_tokens` and `TurnRequest.max_tokens` are output caps ([session.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/session.rs:10), [lib.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/lib.rs:157)). The actor forwards the value unchanged ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:976)).

Today the TUI chooses a 30k cap and sends it during session creation ([live.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/live.rs:610), [live.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/live.rs:1937)). The daemon validates only nonempty model and positive `max_tokens`, not compatibility with the context window ([rpc.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1541)).

The reserve must become daemon-owned or daemon-validated. Otherwise another client can create a session whose declared output budget invalidates all threshold calculations.

Recommended pinned values:

```text
W = provider-declared context window
R = daemon-validated reserved output budget
soft_compact_at = min(floor(0.85 * W), W - R)
hard_fit = estimated_input + R <= W
```

This preserves the simulator’s 85% behavior while forcing earlier compaction for unusually large output reserves. For a 200k window and 30k reserve, both limits meet at 170k.

Run the hard-fit/threshold check immediately before every provider request, not merely once before the logical turn. Tools, nudges, MaxTokens continuation, and prior provider output all change the next request.

### 7. Compaction operation and crash safety

Do not route manual compaction through ordinary `turn.submit`:

- `turn.submit` requires nonempty user text.
- Its compiler requires a committed current user message.
- Compaction is daemon-authored work and must not create a fake user row.

Instead, add a serialized supervisor/context-manager operation that uses the pinned provider but has its own durable job kind.

Recommended state machine:

1. Durably record a compaction intent/plan: cause, covered endpoints, before count, active/manual coordinates.
2. Commit `RunState::Compacting`.
3. Execute a daemon-authored summarization request privately.
4. Put the resulting UTF-8 summary into CAS.
5. Atomically append:

   - `NodeCommitted(NodeKind::Compaction)`.
   - Completed `TurnItem::ContextCompaction`.
   - The new context-footprint snapshot when W7b exists.
   - `Thinking` for auto/forced continuation, or terminal/idle settlement for manual idle.

6. Recompile from the durable projection before the next provider request.

The final compaction node is the durable projection switch. A crash:

- Before node commit leaves the original prompt active.
- After node commit deterministically uses the summary.
- Between CAS put and node commit leaves only an unreachable content-addressed object, which is safe.

For resumability rather than mere projection safety, `RunState::Compacting` alone is insufficient: it carries no covered range or trigger. Current recovery terminalizes every nonterminal state except queued, menu, and child-wait checkpoints ([turn_recovery.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/turn_recovery.rs:4), [turn_recovery.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/turn_recovery.rs:148), [turn_recovery.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/turn_recovery.rs:241)).

W7a therefore needs a durable compaction intent—an additive prompt-omitted event is preferable to hiding recovery data only in a command receipt. The same event can later drive W7b’s pre-announcement.

### 8. Manual `/compact` RPC

No compact method exists in either request or response unions ([frame.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:671), [frame.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:882)).

Minimal RPC shape:

```text
context.compact {
    command_id,
    session_id,
    worker_generation
}
```

Response should return the compaction job/run coordinate and accepted sequence. It should:

- Require Control plus a control attachment.
- Be generation-fenced.
- Use the existing command-receipt discipline for response-loss idempotence.
- Advertise a new `context_compaction_v1` feature.
- Be idle-only in W7a.

Mid-turn manual compaction should remain rejected until its interaction with active tools/effects is explicitly defined. Automatic/overflow compaction is different: the actor invokes it only at a safe provider-request boundary.

### 9. Provider context overflow

`ProviderErrorKind` has no context-exceeded variant ([lib.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/lib.rs:171)).

Today:

- OpenAI unrecognized HTTP/stream error codes become `InvalidRequest` ([openai.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/openai.rs:1449), [openai.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/openai.rs:1505)).
- Anthropic unrecognized HTTP errors become `InvalidRequest` ([anthropic.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/anthropic.rs:508)).
- Anthropic’s `model_context_window_exceeded` stop reason is conflated with output exhaustion as `FinishReason::MaxTokens` ([wire/mod.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/wire/mod.rs:691)).
- The actor retries only transport/rate-limit/overload errors. `InvalidRequest` becomes a durable errored turn ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:3132)).

W7a should add `ProviderErrorKind::ContextExceeded`, classify provider-native structured codes in both HTTP and streaming paths, and stop mapping Anthropic context overflow to `MaxTokens`.

Intercept it before generic retry/rotation at both opening-error sites ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1043), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1171)).

Recovery behavior:

1. If no provider content/effect has been committed for that request, compact.
2. Recompile from the committed compaction projection.
3. Retry within the same logical run.
4. Permit only one forced-compaction recovery per unchanged projection/request epoch.
5. If the compacted prompt still overflows, durably surface `RunFailed/Errored`; never panic or process-crash.

The current `provider_event_seen` boolean is too coarse because a usage frame counts as an event. W7 should track “provider content/effect seen” separately so a context rejection accompanied only by usage metadata remains safely recoverable.

### 10. `FinishReason::MaxTokens`

Normalization exists:

- Protocol variant: [provider.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/provider.rs:78).
- OpenAI Responses incomplete reason: [openai.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/openai.rs:1094).
- Chat Completions `length`: [openai.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-provider/src/openai.rs:2411).

But core special-cases only `Cancelled` and `Error`. `MaxTokens` falls through, completes items, and commits `Done` ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1312), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1469), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2618)).

Correct continuation behavior:

1. Close the partial assistant item.
2. Add its canonical assistant blocks to the next request.
3. Add a hidden daemon-authored “continue exactly where you stopped” instruction.
4. Roll the request-local usage into the logical-turn accumulator.
5. Reset provider-attempt state.
6. Run context hard-fit, compact if needed, then continue the provider loop.
7. Count each continuation against the existing 32-request logical-turn ceiling ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:63)).

The continuation boundary should be durable. Otherwise a crash loses both the partial output and continuation cause because the compiler intentionally excludes current-run output ([prompt_history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/prompt_history.rs:20), [prompt_history.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/prompt_history.rs:97)). A typed prompt-omitted continuation marker/checkpoint is preferable to reusing in-memory `pending_nudges`.

Also factor request-usage finalization into a common inter-request helper: today it advances only on the tool-results path, not on every continuation boundary ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1435)).

## Proposed W7a/W7b split

| Wave | Scope |
|---|---|
| W7a — correctness/recovery | Activate durable tree projection; CAS-aware compiler; daemon-owned reserve/hard-fit; manual idle compact RPC; recoverable compaction job; typed context-exceeded classification; forced compact/retry; durable same-run MaxTokens continuation |
| W7b — proactive UX/truth | Threshold auto-compaction; pre-announcement event; exact/estimated context-footprint payload; live context meter and `/tokens`; turns-to-threshold estimates |

### W7a build order

1. **Protocol/provider taxonomy**

   Add `ContextExceeded`, split it from `MaxTokens`, add provider HTTP/SSE fixtures, and update exhaustive matches in core/accounts.

2. **Window and reserve plumbing**

   Thread `Option<u64>` through `ResolvedTurnProvider → HarnessConfig`; validate the output reserve daemon-side. Unknown windows remain unknown.

3. **Durable tree foundation**

   Materialize user/assistant/tool nodes and active ancestry, including acceptance/crash boundaries. Preserve provider-opaque/tool-detail sidecars needed for exact prompt rendering.

4. **Projection compiler**

   Split tree selection from message rendering; add CAS summary resolution and compaction-range validation. Recompile at every provider-request boundary after a projection change.

5. **Compaction state machine**

   Add durable intent, private summarizer, CAS put, atomic final node/item/state batch, and interrupted-compaction recovery.

6. **Manual RPC**

   Add request/response variants, feature flag, receipts, control-attachment checks, generation fencing, live TUI routing, and response-loss replay tests.

7. **Forced overflow**

   Intercept typed overflow, compact once, recompile, and retry the same run. Add unknown-window and repeated-overflow tests.

8. **MaxTokens continuation**

   Add durable continuation checkpoint, common usage-boundary finalization, hidden continuation instruction, loop-cap tests, and compact-before-continuing tests.

Release-gate tests should include replay before/after the atomic compaction-node commit, crash after CAS put, missing artifact, nested compactions, branch fork before a compaction, provider opaque preservation, manual RPC replay, overflow twice, and repeated MaxTokens until the request ceiling.

### W7b build order

1. Add request-local context-footprint accounting and honesty source.
2. Emit durable `ContextFootprint` snapshots.
3. Run soft threshold checks before every provider request.
4. Emit a typed pre-announcement/compaction-intent event, then `RunState::Compacting`.
5. Teach live Ratatui to render source quality and `/tokens`.
6. Remove demo-only local threshold authority from live paths; the daemon remains authoritative.

## Q2 — simulator UX and Ratatui port

### Simulator `/compact`

The command is session-only and described as preserving history in the tree at [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:172). Help lists it at line 601.

Manual behavior at [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1791):

- Captures the current branch token count.
- Targets 6% of the model window.
- Immediately enters `COMPACTING`.
- Keeps the old meter visible for 1200 ms.
- Atomically resets the meter and appends the before→after row.
- Returns to idle.
- Emits no pre-announcement.

Because slash dispatch precedes normal busy-input handling, the JS simulator technically allows `/compact` during a turn ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1966)).

### Simulator auto-compaction

At normal turn completion:

- Queued input first gets direct consumption without an idle seam ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1201)).
- Hotness is sampled as `tokens/window >= 0.85` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1510)).
- After a 30 ms gap, it announces:  
  `· context at 85% — compacting (dead branches first, live path last)`
- It enters `COMPACTING` for 1400 ms.
- It appends the compaction row and resets tokens to 6% of the window.
- It calls `finishTurn()` again, so input queued during compaction continues directly instead of stopping at idle ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1520)).

The simulator explicitly describes this as the same auto-continue spine used by other parked states ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:960)).

### Simulator visuals and meter

- Active tokens and percent are `branch.tokens / model.window` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:774)).
- Exact badge: `⊟ COMPACTING` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2813)).
- Status bar shows tokens, cells, percent, and model window ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2835)).
- Completed row says `summary retained · originals stay in /tree` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3919)).
- `/tokens` shows main and child models, simulated in/out/cache splits, and estimated turns to 85% ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2946)).

There is no distinct “before meter” widget. The old meter simply remains unchanged during `COMPACTING`, then flips to the after value when compaction completes.

The simulator’s token panel does not expose exact-versus-estimated provenance.

### Ratatui already present

One correction to the premise: `compaction_beats` is not defined in `runtime.rs`; it is defined in [script.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/script.rs:1516), imported and invoked by `runtime.rs`.

Already implemented:

- `/compact` and `/tokens` command descriptions: [commands.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/commands.rs:65).
- Semantic `AppRequest::Compact`: [app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:1289).
- Demo manual compaction: [runtime.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/runtime.rs:1112).
- Demo 85% auto-compaction: [runtime.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/runtime.rs:1823).
- Exact pre-announcement, `Compacting`, 1200/1400 ms timing, compaction item, meter reset, and auto `TurnEnd`: [script.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/script.rs:1524).
- Per-session demo meters and `UsageSource::Estimated`: [runtime.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/runtime.rs:2011).
- `⊟ COMPACTING` projection and warning tone: [projection.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/projection.rs:621).
- Completed compaction card: [render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:4323).
- Plain rendering: [plain.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/plain.rs:178).
- Beat and continuation tests: [turn_engine_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/tests/turn_engine_tests.rs:601), [turn_engine_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/tests/turn_engine_tests.rs:881).

The Rust demo intentionally refuses `/compact` during an active turn, unlike the JS simulator ([app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:4498)).

### Ratatui live-mode gaps

- Live `/compact` is explicitly refused because no daemon RPC exists ([app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:4498), [live.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/live.rs:2117)).
- Threshold checking and meter reset exist only in `DemoDriver`.
- The pre-announcement is a demo-local `DemoEvent::Note`; there is no generic live note payload.
- `/tokens` and Ctrl-G remain honest stubs ([app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:2614), [app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:4700)).
- The live meter ignores `Usage.source`.
- `/tree` is also a stub, so live mode cannot yet navigate the originals promised by the compaction card.
- No local continuation choreography is needed once the daemon emits the correct stream. Existing projection code can render `Compacting → ContextCompaction/footprint → Thinking/Streaming`.

Live mode should never copy the demo’s timers or optimistic token reset. The daemon must own the operation and event timing.

## FINDINGS SUMMARY — Q1

- Production prompt assembly is journal-based, not tree-based.
- `PromptRender::Pruned` is currently behaviorally identical to `Verbatim`.
- Compaction/tree protocol shapes are frozen but unused outside tests/demo.
- A compaction is an immutable ancestry substitution, not deletion.
- W7a must preserve exact tool/provider-opaque render fragments when activating tree compilation.
- Catalog windows reach the account/TUI snapshot but stop before `ResolvedTurnProvider` and `HarnessConfig`.
- Existing `Usage` is cumulative logical-turn billing telemetry, not context occupancy.
- Reserved output is presently selected by the TUI and inadequately validated by the daemon.
- Compaction requires a CAS-aware compiler and a durable intent for crash recovery.
- No manual compact RPC exists.
- Provider context overflow is indistinguishable from generic invalid requests, and Anthropic conflates it with `MaxTokens`.
- `FinishReason::MaxTokens` currently ends the run instead of continuing.
- W7a should deliver correctness/recovery; W7b should deliver proactive thresholding, pre-announcement, and meter truth.

## FINDINGS SUMMARY — Q2

- The simulator uses an 85% trigger, a pre-announcement, `⊟ COMPACTING`, a 1400 ms auto beat, and a reset to 6%.
- Manual `/compact` uses 1200 ms and no pre-announcement.
- Queued input during compaction auto-continues through a second `finishTurn()` check.
- Ratatui already has the visual vocabulary, demo choreography, protocol item, and tests.
- `compaction_beats` lives in `script.rs`, not `runtime.rs`.
- Live mode still lacks the RPC, daemon-owned threshold, typed pre-announcement, truthful context payload, `/tokens`, and `/tree`.
- Live continuation should be entirely event-driven by the daemon.
