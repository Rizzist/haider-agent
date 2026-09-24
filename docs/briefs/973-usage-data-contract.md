# 973 usage data and deferred reset approval

## Provider facts

Only provider-published fields are meters. Missing windows, balances, counts,
and timestamps remain absent; cadence is never used to manufacture a reset.

| Provider | Meter/reset facts | Banked reset operation |
| --- | --- | --- |
| `openai-oauth` | `GET /backend-api/wham/usage` publishes rate-limit windows and a reset-credit summary. The current redacted live fixture has one 604800-second weekly window, no 5-hour window, and exact `available_count` / `applicable_available_count`. `GET /backend-api/wham/rate-limit-reset-credits` publishes the detailed available-credit inventory; the confirmed contract does not publish prior-reset history. | OpenAI Codex implements `POST /backend-api/wham/rate-limit-reset-credits/consume` with a caller-supplied `redeem_request_id`; success returns `code: reset` and `windows_reset`. |
| `anthropic-oauth` | The live `/api/oauth/usage` shape publishes current five-hour/seven-day buckets and optional extra usage. It contains no banked, earned, used, or remaining reset inventory. | None found in the authenticated usage response or provider contract. |
| `haider-code` | `/v1/account` publishes the current weekly allowance (`percent_remaining`, state, exact reset/grace timestamps), credits, and hold state. It does not publish banked-reset inventory or history. | None published. |
| `kimi-oauth` | `/coding/v1/usages` publishes current quota counters/windows and reset time. Recorded/verified shapes contain no banked-reset inventory or history. | None published. |
| `grok-oauth` | The billing response publishes current plan/credit utilization. It contains no banked-reset inventory or history. | None published. |
| API-key/custom providers and supervised ACP providers | No provider quota/reset surface is available to Haider; local accounting or typed unavailability remains the honest state. | None. |

The OpenAI sources of record are the upstream Codex backend client and
app-server protocol/tests for `rate-limit-reset-credits` and `consume`. The
redacted live shape is
`crates/haider-provider/tests/fixtures/usage/openai_wham_usage_live_redacted.json`;
every string value was replaced before persistence.

## Additive usage wire

`AccountUsageReportV1` adds optional `reset_credits` and
`previous_resets`. Reset history carries an explicit `source` of `provider`
or `device_journal`; the latter is made only from exact reset timestamps that
this device previously sampled. `AccountMeterStateV1::Unavailable` adds the
optional exact `retry_at_ms` derived from `Retry-After`.

Haider Code's held account snapshot is now collected for the active account
even when no provider session is attached. Its weekly window uses
`utilization` as the used fraction (`1 - percent_remaining / 100`) and carries
the provider's exact `resets_at_ms`; the remaining fraction is the complement
of that same published percentage.

## Human-only deferred approval

`EffectClass::UsageResetApply` is a sensitive finite-resource class. Ordinary
allow rules, remembered/session grants, autonomous mode, and `auto_allow`
cannot approve it. A request opens `MenuKind::DeferredApproval` with only
approve-once and deny choices. The menu is session-scoped and
`blocking: false`; the initiating RPC/tool immediately returns
`approval_pending` and the run continues.

The committed `menu_answer` is the human authorization fact. On approval the
daemon calls the provider with the same opaque approval id as OpenAI's
idempotency key, then publishes a `usage_reset_apply_result_v1` extension
(`applied`, `denied`, or `failed`) to the existing session event stream. An
active run receives the same typed result through the existing durable steer
notification path; an idle session carries it verbatim into the next prompt.
The agent can also call `usage_reset_apply` again with `approval_id` to poll
the durable state. Denial never sends a consume request. A failed or lost
provider response can leave the spending outcome unknown.

The only consume caller is the hub-owned completion task launched by a committed
human `menu_answer` in `session_hub/usage_reset_rpc.rs`. The RPC persists
`Dispatched` before handing off the single POST and replies without waiting
for the provider or steer delivery. A hub shutdown joins the task; forced
shutdown can leave its outcome unconfirmed. Agent polling in
`worker_usage_reset.rs::usage_reset_status` reads the
result and never calls the provider. Shared identifiers, the approval menu
and the poll-status derivation live in `usage_reset.rs`. If the daemon stops after dispatch but
before recording a result, polling reports `provider_outcome_unconfirmed` and
does not reissue the POST. The same approval id is sent as OpenAI's
`redeem_request_id` for provider-side idempotency. At integration with lane
973-deferred-approval, replace the local `DeferredApproval` menu opening and
`finish_usage_reset_approval` callback with that lane's durable pending
approval mechanism; retain the human-only policy and dispatch-before-POST
ordering.
An interrupted approval with no durable dispatch is reported as
`approval_interrupted_before_dispatch`; it is safe to open a fresh approval.

The notification id for new completions is the already-prefixed approval id
(`usage-reset-<hex>`). Earlier `usage-reset-usage-reset-<hex>` notification
ids, and all persisted menu, item, effect, and event ids, remain valid as
written; no journal migration or replay rewrite is needed.

The active Haider Code account poller fetches `/v1/account` even when no
provider session is attached. That network behavior is intentional so the
account meter stays current. It never polls without an active account; a
provider refresh hint is floored at 15 seconds, and missing hints or
transient failures use a 60-second fallback. Switching the active account
starts a fresh request immediately, so the 15-second floor bounds refreshes
for an unchanged active account rather than user-driven account switches.
