# Output-token limits

`session.create.max_tokens` is the per-provider-request output budget. It is
not a context-window size and it is not the headless run's cumulative token
budget.

The daemon owns model-limit truth. A provider catalog's positive
`max_output_tokens`, `max_tokens`, or `outputTokenLimit` declaration wins.
When discovery is unavailable or the row omits limits, the daemon uses the
pinned provider/model-family table in `haider-provider::model_limits`; exact
known generations take precedence over a conservative provider-family
default. `haider-provider::output_budget::model_output_limit` applies the
bounds: the resulting output maximum is never larger than the largest budget
supported by a registered adapter (384,000) and stays strictly below a known
context window (a maximum that would reach the window becomes half of it, so
the worker always has room for input). `haider models` exposes both values.
Session creation rejects an explicit request above the row's explicit ceiling
instead of silently clamping it.

## Unverified fallback rows

Rows with no published output maximum (custom OpenAI/Anthropic-compatible
endpoints, older Claude such as Sonnet 4 / Opus 4.1 / 3.7, unlisted Gemini
IDs, unknown aliases) project the unverified 8,192 guess
(`StaticModelLimits::output_limit_sourced == false`). The guess bounds only
the DERIVED budget. An explicit budget (`--max-output-tokens`, `session
config --max-output-tokens`, `session.create`/`session.select_model`
`max_tokens`) may go up to 384,000, strictly below a known context window
(`output_budget::explicit_output_ceiling`); if the provider's real maximum is
lower, the provider-stated one-shot retry below lowers it for that turn. A
catalog declaration or a sourced table row, including a sourced 8,192 (Gemini
1.5/2.0), is exact and is also the explicit ceiling.

Every client without an explicit override sends `session.create.max_tokens = 0`.
The daemon resolves the smaller of the shared 30,000 default and the selected
model maximum, then persists/emits that effective nonzero value. TUI model
details may be unavailable when a session is created; zero remains safe then.

## CLI flags

- `haider run --max-output-tokens N` is the exact per-response output budget
  (sent as `session.create.max_tokens`); omitted, the daemon derives it.
- `haider run --max-tokens N` keeps its 0.0.972 meaning: the cumulative run
  token budget (`RunBudgetV1.max_tokens`, `budget_exhausted`, exit 77). It is
  NOT a per-response cap. An earlier 973 candidate briefly repurposed it as the
  per-response budget and moved the run budget to `--max-total-tokens`; that
  silently removed the spend cap from existing scripts and broke the
  `t0.budget.max_tokens_binds` QA gate, so it was reverted before release.
  `--max-total-tokens` never shipped and is not accepted: one spelling per
  budget, no alias.
- `haider session <id> config --max-output-tokens <n|auto>` sets a user
  budget on an existing session or returns it to the derived one.

## Model switches and explicit budgets

Session metadata records whether the budget is `derived` or `user_set`
(`max_tokens_source`). Metadata written before 973 has no source; a stored
4,096, 8,192 or 30,000 (the pre-973 client defaults) is derived, any other
value was an explicit override. `session.select_model` re-applies the source
to the new model, so a switch never fails on the output budget:

- derived: `min(30,000, new model maximum)`, silently;
- user-set: `min(requested, new explicit ceiling)`; a clamp returns a typed
  `output_budget.clamped` notice (TUI flash, CLI stderr). The requested value
  is kept, so switching back to a larger model restores it.

`session.select_model.max_tokens` (and `haider session <id> config
--max-output-tokens <n|auto>`) sets a user budget or returns to the derived
one; an explicit value above the selected model's explicit ceiling is refused,
like `session.create`. The automatic provider pair-switch path has no validated
model row and keeps the stored budget; the provider retry below covers it.

Each run records its pinned provider, model, budget and source in a durable
`run_output_pair_pinned_v1` fact. Headless runs pin the spec at admission;
native turns pin the current selection under the selection lock at worker
pickup. A selection committed while a native turn is still queued therefore
applies to that turn. Selections after pickup apply to the next turn.
Same-run headless steer and subturn deliveries retain the original admission
route; matching the current provider is required only for a new admission.
The pin retains the complete route: endpoint, account, rebind identity, resolved
alias and seen state, selection and reset epochs, including an absent explicit
selector. Absence of a snapshot is reserved for older journals. Recovery
of an older native run uses its reconstructed started pair, then writes the
pin under the same lock so later selections cannot replace its pair or route.
Provider rebind, child inheritance and recovery read this same run record,
including subsequent automatic pair switches and explicit rebind facts.
Native pickup currently reduces the whole journal while holding the selection
lock. The merged wave has no indexed run-pair reader; that optimization is
deferred to 974.

A restored explicit route carries its own resolved account, seen flag and
selection epoch for that run. Resolving or rotating it writes a silent
`run_output_route_resolved_v1` journal fact for explicit rebinds, or
`run_output_route_snapshot_v1` for routes without a rebind identity. Both carry
the complete snapshot. It also publishes session-level
resolved-route metadata when provider, model, endpoint, account, rebind identity
and selection epoch still match and the reset fence permits the commit.
Diverged routes remain run-only. The fact uses the branch of the durable run
pin or legacy accepted run, including fork branches. Releasing the session pin advances
`route_reset_epoch`; the next turn resolves the current session selection.
Recovery reads the same run facts, so a running turn cannot inherit the next
turn's resolved account. Historical reconstruction uses frozen create/fork
metadata and route/selection facts at the run boundary. A model-shaped request
alone cannot prove that an old internal account setter was absent. Missing
account arguments, served aliases, or epochs remain unknown until a complete
historical witness or a provable rebind reset resolves them. Missing receipts
and genuinely unavailable route fields fail closed with `StoreCorrupt` and the
`run-route-history-unavailable` presentation. No current selection supplies
missing history.

A legacy recovery checkpoint written after a run started cannot move its
boundary past next-turn selections. Recovery reconstructs that run from its
original start and records the first complete pin before dispatch. This
complete checkpoint includes intentional run switches and rebinds already
applied; later ordinary selections and duplicate pins cannot replace it.
When an old checkpoint borrowed a later epoch, replay cannot use it to invent a
rotation of the run's unchanged account. Automatic-switch facts also retain the
resolved account as an observation; they preserve the run's request-start epoch.
A later owned account rotation can advance it. An observed default account never
becomes an explicit selector.
Legacy automatic-switch facts without a recorded resolved account cannot prove
the run's observation state. A later complete owned witness or explicit rebind
may resolve that missing history; otherwise recovery returns the typed error.

Rebind always advances the selection epoch, including when the provider and
endpoint stay the same but the output budget clamps or restores. Context-meter
clients therefore invalidate the previous budget with the same epoch rule.
An explicit child provider selector starts under that provider's own account
and endpoint and re-applies the budget source, even when the provider and model
are unchanged. A model-only selection within the inherited provider keeps its
account and endpoint selectors; omitting a selector preserves the admitted pair, route and budget.
Delegated-create receipts bind both budget intent and inherited route. Replay
uses the original admitted budget; exact legacy schemas may add those fields
only when the original response and parent history prove the same choices.
An old explicit same-provider child selector can have retained its parent's
route. An unchanged retry keeps that committed route and budget, even when
the current selector would clear the route or a catalog edit changes the cap.
Current receipts and changed provider, model, budget intent or nonempty route
coordinates remain strict.
Route choices recorded in the legacy digest take precedence over an old reply
that omitted route fields. For a pre-route digest, the frozen child response
takes precedence over the parent's next-turn state; creation-bounded parent
history supplies choices only when that reply did not record a route. It uses
the original delegation's run pin and subsequent run changes, bounded by the child's original creation,
so a next-turn pick cannot supply a running parent's route. Before the durable
delegation link exists, an implicit route requires a unique running parent;
queued runs do not supply active inheritance, and ambiguous histories cannot
upgrade the receipt.

An explicit rebind of a queued headless run has no active-run budget receipt.
Recovery intentionally applies its pinned budget source to the rebound route
using the current catalog, just as pickup does; the original headless spec
and admitted source remain durable. This permits a refreshed model ceiling to
bound the request without changing the user's requested budget. Active runs
instead retain the effective budget recorded in the rebind fact.
For legacy active headless runs without that active-budget record, recovery
uses the rebind's budget or the historical metadata at that rebind, then
re-applies the model limit. It never substitutes the old admission budget or
the next turn's metadata for that owned change.

## Provider-stated maxima

If a provider rejects a request because `max_tokens` exceeds the model's
maximum and states that maximum (Anthropic `max_tokens: A > B, which is the
maximum allowed number of output tokens`, OpenAI-compatible `supports at most
B completion tokens` (a token unit must follow the number, so e.g.
`supports at most 10 images` is ignored), DeepSeek `valid range of
max_tokens is [1, B]`), the actor retries that request once at `B` and keeps `B` for the rest of the
turn. A second rejection surfaces as the ordinary provider error; the retry
never loops.

## Sourced static rows

`model_limits.rs` cites the official page for every non-fallback row.
DeepSeek `deepseek-flash` (V4.1-Flash) and `deepseek-v4-pro`: 1M context,
384,000 output. Kimi Code (`kimi-oauth`) and Haider Code publish context
windows but no output ceiling, so documented rows use the shared 30,000
default; xAI documents a 128,000 default output budget and per-model windows.
Only IDs without any published limit stay on the unverified 8,192 fallback.

## Partial tool calls at the limit

An Anthropic `max_tokens`, OpenAI `length`, or Responses `incomplete` terminal
that arrives while tool arguments are open is an output-limit truncation, not
malformed model JSON. Adapters surface the start and argument deltas without a
`ToolCallEnd`. The actor then:

1. records the raw partial arguments and a typed `output_limit_truncation`
   result;
2. never dispatches the truncated call;
3. auto-continues with instructions to split large content across smaller
   tool calls; and
4. after three consecutive truncations, gives the model a typed
   `output_limit_truncation_repeated` tool error with guidance. The run remains
   active and the model decides its next action.

A provider response without an output-limited partial tool call resets the
truncation streak. A completed call in the same truncated response does not.
Malformed calls retain their existing typed `invalid_tool_call` strike path.
The AX-2 malformed-call strike predicate in `haider-core` matches only
`ToolResultData::InvalidToolCall`; keep `OutputLimitTruncation` outside that
predicate when integrating the lanes. The live path and
`recover_tool_repair_state` both use `invalid_tool_call_result` for malformed
strikes; the truncation streak is stored separately. This makes the
973-ax2-tool-friction predicate change a no-op at integration.
