Read-only audit complete. No files were changed.

## Cross-cutting baseline

- Haider has a strong normal failure terminal: open partial items are closed, then `RunFailed` and `RunState::Errored` are appended atomically. The TUI renders both the visible error transcript line and `✗ ERRORED`; its F2e fallback synthesizes a line if an errored run somehow lacks one. [actor.rs:3412](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3412), [actor.rs:3459](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3459), [worker.rs:3797](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3797), [projection.rs:372](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/projection.rs:372), [projection.rs:423](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/projection.rs:423)
- Provider retries are deliberately pre-content only. Retrying commits `RunState::Retrying`, honors `Retry-After` up to 60 seconds, and renders `API error · Retrying in Ns · attempt K/max`. [actor.rs:1139](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1139), [actor.rs:1271](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1271), [actor.rs:2185](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2185), [render.rs:2984](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:2984)
- The main limitation is the public error contract. `ProviderError` has only ten broad kinds; all terminal provider failures become `ErrorCode::ProviderError`, while `RunFailed` carries only code/message/retryable—not structured provider reason, request ID, reset time, or recovery action. [provider/lib.rs:221](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/lib.rs:221), [actor.rs:4086](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:4086), [protocol/lib.rs:38](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/lib.rs:38)
- `sanitized_failure_message` is not what makes messages useless: it only removes controls and bounds them to roughly 512 bytes. Actionable provider body fields are normally discarded earlier by adapters. [actor.rs:4334](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:4334), [openai.rs:2062](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2062), [gemini.rs:1406](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:1406)
- A second broad limitation affects tools: many failures are encoded in `ToolResult`, but the actor marks them `Completed` and the TUI explicitly ignores `ToolResult`. This produces green-looking denied/conflicted/nonzero tools, sometimes alongside a separate `effect failed` line. [worker.rs:5196](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:5196), [actor.rs:2448](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2448), [projection.rs:512](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/projection.rs:512)

## 1. Transport and connection

### DNS, refused connection, TLS, request/open/idle timeout

- **Occurs:** reqwest request creation/opening or stream reads fail.
- **Today:** All map to retryable `Transport`; core auto-retries up to ten times only before content. The UI says generic “API error,” not DNS/TLS/connect/idle. [openai.rs:3223](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:3223), [provider/lib.rs:265](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/lib.rs:265), [actor.rs:2035](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2035)
- **Gap:** Permanent certificate, proxy, and endpoint-configuration failures waste the full retry budget; no jitter or transport-specific guidance.
- **Recommended:** **AUTO-RETRY** transient DNS/connect/reset/timeouts with jitter and a specific label. TLS trust, invalid endpoint, or proxy configuration should be **FATAL-WITH-GUIDANCE**, naming the endpoint and corrective setting.

### Clean EOF before finish

- **Today:** Several adapters classify a prematurely closed stream as nonretryable `MalformedFrame`, even before content. [openai.rs:1187](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:1187), [actor.rs:1334](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1334)
- **Gap:** A proxy/socket EOF does not receive the otherwise-correct pre-content retry.
- **Recommended:** Introduce `StreamInterrupted`; **AUTO-RETRY** before content, otherwise use the mid-content policy.

### Mid-content disconnect

- **Today:** Once any non-usage event arrives, retry is suppressed. Text deltas have already been durably committed; failure closes that text and appends the error/Errored terminal. The user therefore sees partial output followed by an error. [actor.rs:1271](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1271), [actor.rs:1422](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1422), [actor.rs:1903](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1903), [actor.rs:3412](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3412)
- **Gap:** Partial text is labelled `Completed`, and the UI never explains that replay was intentionally suppressed to avoid duplicated text or tool effects.
- **Recommended:** **SURFACE-AND-CONTINUE** with an explicit “incomplete—connection lost after partial output” marker. Offer `c Continue from partial response` or `r Retry from scratch`. Never blind-replay after committed semantic content or possible effects unless the provider offers a resumable cursor/idempotency contract.

## 2. Provider HTTP

### 400 invalid request

- **Today:** Immediate nonretryable `InvalidRequest`; only recognized context-overflow codes are carved out. Model-not-found, unsupported parameter/tool, bad replay signature, and malformed request otherwise collapse together. [openai.rs:2022](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2022), [anthropic.rs:817](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/anthropic.rs:817)
- **Gap:** Generic “HTTP 400 returned invalid request” is safe but not actionable.
- **Recommended:** Usually **FATAL-WITH-GUIDANCE**, using safe subcodes. Invalid model should **PARK-FOR-USER** with `m Choose model`; optional unsupported capability should **RECOVER/DEGRADE**.

### 401 unauthorized/token expired

- **Today:** Before content, OAuth/gcloud credentials get one forced refresh attempt and may rotate to another account; the H2 budget prevents an infinite refresh loop. After content, the turn errors immediately. [accounts.rs:6114](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:6114), [actor.rs:1959](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1959), [actor.rs:1991](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1991)
- **Gap:** Permanent invalid BYOK, expired OAuth, revoked account, and failed import can finish as the same generic Authentication line.
- **Recommended:** Successful refresh/rotation is **RECOVER/DEGRADE**, with a small visible “Refreshing credentials…” status. Failure is **PARK-FOR-USER** with `r Re-login`, `i Re-import`, `e Edit key`, and `a Switch account`.

### 403 forbidden or org-disabled provider tool

- **Today:** Immediate `PermissionDenied`. Whole-account/org denial and a single provider-hosted tool denial are indistinguishable. [openai.rs:2032](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2032), [gemini.rs:1390](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:1390)
- **Gap:** No local-tool fallback or account/admin guidance.
- **Recommended:** Account/org denial → **PARK-FOR-USER** with switch-account/contact-admin actions. Explicit provider-tool denial → **RECOVER/DEGRADE** in the same turn to local `web_fetch`/search if available.

### 404/410 endpoint or model gone

- **Today:** Normal provider calls collapse to `InvalidRequest`. Local OpenAI alpha search is better: 404/410 latches the capability off for the session, although the TUI does not visibly explain why it disappeared. [web_search.rs:193](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/web_search.rs:193), [worker.rs:4922](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4922)
- **Recommended:** Model gone → **PARK-FOR-USER**, `m Choose model`. Removed API endpoint → **FATAL-WITH-GUIDANCE** to update/configure Haider. Optional capability endpoint → **RECOVER/DEGRADE**, visibly identifying the fallback.

### 413 too large

- **Today:** No dedicated kind; known context codes become `ContextExceeded`, otherwise `InvalidRequest`. [openai.rs:2022](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2022)
- **Gap:** History overflow and request-body/attachment limits cannot be distinguished.
- **Recommended:** History overflow → **RECOVER/DEGRADE** by compaction. Attachment/body limit → **PARK-FOR-USER** with remove/compress-attachment or reduce-output-limit actions.

### 429 rate limit

- **Today:** Pre-content **AUTO-RETRY**, honoring `Retry-After`. Rotation is considered only when a header exists and is ≤60 seconds. Retry-After above 60 seconds becomes an immediate retryable error. [actor.rs:2195](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2195), [actor.rs:4109](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:4109)
- **Gap:** No-header throttles miss account rotation; long reset windows error rather than park.
- **Recommended:** Short transient limit → **AUTO-RETRY**. Long reset → **PARK-FOR-USER** showing reset time, with `w Wait`, `a Switch account`, `m Switch model`, `r Retry now`.

### 500/503/529 overloaded

- **Today:** OpenAI 503, Gemini 503 and Anthropic 529 become retryable overload; most other 5xx become retryable transport. Gemini only records retry-after for rate limits, not overload. [openai.rs:2032](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2032), [anthropic.rs:817](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/anthropic.rs:817), [gemini.rs:1390](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:1390)
- **Recommended:** Pre-content **AUTO-RETRY** with jitter and provider label; honor overload retry-after consistently. Post-content remains incomplete-output **SURFACE-AND-CONTINUE**.

### Provider-specific quota/credit exhaustion

- **Today:** There is no quota kind. OpenAI `insufficient_quota` can be classified `RateLimited` because HTTP 429 wins; Gemini `RESOURCE_EXHAUSTED` always becomes rate-limit; Anthropic billing variants may become invalid-request. [openai.rs:2032](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2032), [gemini.rs:1398](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:1398), [anthropic.rs:824](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/anthropic.rs:824)
- **Gap:** Haider can futilely retry a condition only billing action can fix.
- **Recommended:** Add `QuotaExhausted`/`CreditExhausted`. Auto-rotate once to a funded account if available; otherwise **PARK-FOR-USER** with reset time, billing/top-up guidance, `a Switch account`, and `m Choose cheaper model`. Never consume the ten-attempt generic retry budget.

## 3. Authentication and credentials

### OAuth expiry and imported Codex/Anthropic credentials

- **Today:** Proactive refresh exists, followed by one forced refresh after a 401. Internal errors already carry concepts such as `oauth_relogin_required` and `reimport_required`, but those details do not reach `RunFailed`. [oauth.rs:4112](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/oauth.rs:4112), [oauth.rs:5321](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/oauth.rs:5321), [accounts.rs:6056](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:6056)
- **Recommended:** Success → **RECOVER/DEGRADE**. Permanent failure → **PARK-FOR-USER** with one-key re-login/re-import, preserving the broker’s specific safe reason.

### Refresh-token rotation failure

- **Today:** Token-endpoint 429/5xx gets bounded retries; `invalid_grant` is permanent. Persistence failures fail closed because the server may already have rotated the refresh token. [oauth.rs:4494](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/oauth.rs:4494), [oauth.rs:4633](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/oauth.rs:4633)
- **Recommended:** Endpoint transient → **AUTO-RETRY**. Permanent/ambiguous persistence failure → **PARK-FOR-USER**, explicitly “credential rotation may have invalidated the stored refresh token,” with `r Re-login`. Never replay refresh blindly.

### Refresh loop

- **Today:** Correctly bounded twice: resolver refresh is guarded by `AtomicBool`, and core accounts it under `MAX_API_RETRIES`. [accounts.rs:6121](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:6121), [actor.rs:1991](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1991)
- **Gap:** Exhaustion still ends as generic Authentication/ProviderError.
- **Recommended:** Keep the cap; on exhaustion **PARK-FOR-USER** with re-login/switch-account instead of plain Errored.

### Account exhausted or no usable alternate

- **Today:** Credential state supports only Ok, Limited, Expired and Revoked. Resolver errors say no active credential, limited-until, expired or revoked; quota exhaustion is not representable. [credential.rs:24](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/credential.rs:24), [resolver.rs:107](/Users/rizzist/haider-run/b2b-tui/crates/haider-accounts/src/resolver.rs:107), [resolver.rs:211](/Users/rizzist/haider-run/b2b-tui/crates/haider-accounts/src/resolver.rs:211)
- **Recommended:** Add quota-exhausted state and **PARK-FOR-USER** with top-up/reset/switch actions.

### BYOK invalid or revoked key

- **Today:** Authentication error may mark the account expired and rotate; without an alternate, the original generic provider error terminates.
- **Recommended:** **PARK-FOR-USER** with `e Edit key` and `a Switch account`. Do not label it “token expired” unless auth type is OAuth.

### Device-credential import

- **Today:** Disabled discovery, unavailable candidate, unsupported source, file-read and import failures are returned precisely. The TUI shows `✗ import failed — message` inline and releases the pending gate. [accounts.rs:4036](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/accounts.rs:4036), [live.rs:2282](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/live.rs:2282)
- **Gap:** No direct retry action.
- **Recommended:** Malformed/logged-out source → **FATAL-WITH-GUIDANCE**. Transient read/vault error → **SURFACE-AND-CONTINUE** with `r Retry import`.

## 4. Model and response semantics

### Model refusal and reasoning-extraction refusal

- **Today:** `RefusalDelta` is explicitly discarded. `FinishReason::Refusal` then falls through normal completion to `RunState::Done`, so a refusal-only response may produce empty output and an idle-looking successful turn. [actor.rs:1449](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1449), [actor.rs:1597](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1597), [actor.rs:1860](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1860)
- **Gap:** This is a genuine silent semantic failure, including Fable/reasoning-extraction refusal paths.
- **Recommended:** **SURFACE-AND-CONTINUE** with a durable refusal row and normalized safe reason; remain Done, not Errored. Offer `e Edit request`/`r Retry`.

### Context-window exceeded

- **Today:** Before content, core forces compaction once and reissues. Repeated overflow or compaction failure ends Errored. [actor.rs:1200](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1200), [actor.rs:2048](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2048), [actor.rs:4041](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:4041)
- **Gap:** No distinction between transient compactor failure and irreducibly large context; compactor provider calls do not use the shared retry seam.
- **Recommended:** First overflow → existing **RECOVER/DEGRADE**. Transient compactor transport failure → bounded retry or explicit retry action. Repeated overflow → **PARK-FOR-USER** with `m Larger-context model`, `n New session`, `a Remove attachments`, `c Compact again`.

### Max-tokens truncation

- **Today:** Automatically continues with a synthetic “Continue exactly where you stopped…” message until a continuation cap; cap exhaustion is `LoopLimit`. [actor.rs:1742](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1742), [actor.rs:4061](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:4061)
- **Recommended:** Keep **RECOVER/DEGRADE**, but visibly show continuation K/max and offer Stop. At cap, **PARK-FOR-USER** with `c Continue in new turn`.

### Malformed JSON, missing fields, protocol ordering

- **Today:** Nonretryable `MalformedFrame`; deterministic schema error and truncated EOF share much of the same surface. [wire/mod.rs:1207](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:1207), [openai.rs:3254](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:3254)
- **Recommended:** True schema/protocol violation → **FATAL-WITH-GUIDANCE**, with provider/request ID and switch-provider action. Premature EOF → transport policy.

### Unexpected finish/stop reason

- **Today:** Unknown OpenAI/Anthropic values become `MalformedFrame`. Known Gemini failure reasons become `FinishReason::Error`, after which core only says “provider finished the turn with an error.” [openai.rs:3199](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:3199), [wire/mod.rs:1207](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:1207), [gemini.rs:1078](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:1078), [actor.rs:1608](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1608)
- **Recommended:** Known policy/content outcome → **SURFACE-AND-CONTINUE** with exact normalized reason. Unknown protocol value → **FATAL-WITH-GUIDANCE**.

### Thinking-block replay/signature mismatch

- **Today:** Normalized reasoning is intentionally not replayed; foreign provider opaque blocks are stripped on provider-family changes. Same-provider signature rejection still collapses to generic 400. [actor.rs:1442](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1442), [worker.rs:3227](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3227)
- **Recommended:** Only for an explicit signature-mismatch subcode, **RECOVER/DEGRADE** by rebuilding from a safe transcript boundary without stale opaque blocks and retry once pre-content. Otherwise **PARK-FOR-USER** with new-conversation/switch-model actions.

### Tool-call arguments do not parse

- **Today:** Parsing happens before dispatch; malformed JSON becomes provider `MalformedFrame` and terminalizes the turn. No side effect has run. [actor.rs:3979](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3979)
- **Gap:** Recoverable model syntax error is treated as a provider-fatal turn.
- **Recommended:** **RECOVER/DEGRADE** by returning a bounded synthetic failed tool result and allowing one repair attempt. Repeated malformed arguments → **SURFACE-AND-CONTINUE**.

## 5. Tool execution

### General tool-result visibility

- **Today:** `ToolError` already distinguishes permissions, workspace/path changes, stale reads, anchor mismatches, conflicts, I/O, journal, CAS, ledger, runtime and lifecycle errors. Only a whitelist becomes structured rejected/conflict results; other errors become wrongly labelled `ProviderError`. [tools/error.rs:35](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/error.rs:35), [worker.rs:5196](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:5196), [worker.rs:5767](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:5767)
- **Recommended:** Preserve `Completed`, `Rejected`, `Conflict`, `Failed`, `Cancelled`, `Unknown` end-to-end; render the bounded reason inline. Routine tool failures are **SURFACE-AND-CONTINUE**.

### Shell nonzero, signal/killed, timeout, output cap, possible OOM

- **Today:** Supervisor distinguishes nonzero/signals/limits, but ordinary nonzero or signal may still have `EffectOutcome::Ok`; the worker serializes the true status, which the TUI ignores. Direct user-shell execution is rendered correctly, unlike model-invoked shell. [process.rs:784](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/process.rs:784), [process.rs:1313](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/process.rs:1313), [worker.rs:5581](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:5581), [render.rs:6776](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:6776)
- **Gap:** `exit 1` or SIGKILL can appear green. Haider cannot positively prove OOM from SIGKILL.
- **Recommended:** **SURFACE-AND-CONTINUE** with exit code/signal/limit and retained output tail. Say “killed by signal 9; OOM is possible.” Never auto-retry arbitrary commands; an `r Run again` action should confirm mutating commands.

### Background-task failure

- **Today:** Any numeric exit code, including nonzero, is classified `Completed`; only no-exit-code faults become `Failed`. CAS failure storing full output is warning-only, silently dropping the artifact. [tasks.rs:493](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/tasks.rs:493), [tasks.rs:509](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/tasks.rs:509), [tasks.rs:531](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/tasks.rs:531)
- **Recommended:** Nonzero/signal → **SURFACE-AND-CONTINUE** as failed. Eight-task cap → surface with open/kill-task action. Output-CAS loss → **RECOVER/DEGRADE**, retain tail and say full output is unavailable. Completion-journal failure → retry append, then **FATAL-WITH-GUIDANCE** because durable task state is uncertain.

### Filesystem I/O, staleness and anchor-not-found

- **Today:** `fs_write`/`fs_edit` enforce fresh prior-read digests; stale/unread targets and anchor mismatch are structured for the model but look Completed to the human. Generic I/O can end the whole turn. [filesystem.rs:1493](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/filesystem.rs:1493), [filesystem.rs:1643](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/filesystem.rs:1643), [filesystem.rs:1660](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/filesystem.rs:1660)
- **Recommended:** **SURFACE-AND-CONTINUE**. Agent may recover by re-reading and issuing a newly fenced edit. For zero/multiple anchors, refine the anchor; never silently switch to `replace_all`.
- A ledger failure after atomic replacement is outcome-sensitive: the file may already be changed. [filesystem.rs:1391](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/filesystem.rs:1391)
- **Recommended:** **RECOVER/DEGRADE** through re-read/reconciliation; visibly say “file changed, ledger failed.” Never blindly repeat the write.

### `web_fetch`

- **Today:** Enforces HTTPS-public/HTTP-loopback origin fencing, DNS/redirect validation, MIME, 4 MiB source, 96 KiB returned output, connect/open/chunk/total deadlines. Errors become bounded tool failures and failed effects. [webfetch.rs:1](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/webfetch.rs:1), [webfetch.rs:185](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/webfetch.rs:185), [webfetch.rs:329](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/webfetch.rs:329), [worker.rs:4808](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4808)
- **Recommended:** Origin/MIME/nonretryable HTTP → **SURFACE-AND-CONTINUE**. Size cap → **RECOVER/DEGRADE** with narrower-fetch guidance. DNS/connect/read timeout → one or two visible **AUTO-RETRY** attempts because GET is idempotent, preferably only before body bytes.

### `web_search`

- **Today:** Local search has no retry. 404/410 degrades the session; provider server-tool `max_uses_exceeded` is parsed and marked failed, but its reason is not rendered. [web_search.rs:95](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/web_search.rs:95), [wire/mod.rs:985](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:985), [actor.rs:1509](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1509)
- **Recommended:** timeout/5xx → bounded **AUTO-RETRY**. 404/410 → **RECOVER/DEGRADE**, visibly disable search and suggest `web_fetch`. `max_uses_exceeded` → **SURFACE-AND-CONTINUE**, “search budget exhausted for this turn.”

### `request_input`

- **Today:** Correctly opens a durable menu, commits `InputRequired`, and parks. Invalid arguments can instead become a turn-ending drive error. [actor.rs:2780](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2780), [actor.rs:2884](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2884)
- **Recommended:** Valid call remains **PARK-FOR-USER**. Invalid arguments should be a rejected tool result and **SURFACE-AND-CONTINUE**, allowing model repair.

### `spawn_subagent` depth/model/validation

- **Today:** Depth is capped at three. Depth and model-selection refusal become structured results but appear Completed; some other argument validation can become turn-fatal. [delegation.rs:42](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:42), [worker.rs:4525](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4525), [worker.rs:4539](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4539)
- **Recommended:** **SURFACE-AND-CONTINUE** as a red rejected row with exact remedy. No retry for depth; continue locally or shorten/fix arguments.

### `message_subagent` to missing/dead child

- **Today:** Missing/non-owned child becomes a hidden structured result. A terminal child is silently restarted with a fresh turn when messaged. [worker.rs:4618](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4618), [delegation.rs:512](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:512)
- **Recommended:** Missing/not-owned/invalid → **SURFACE-AND-CONTINUE**. Terminal child should return `child_terminal {can_restart:true}` or explicitly say that a new child run started; avoid silent resurrection.

## 6. Permission and policy

### Tool permission Ask/Deny

- **Today:** Ask correctly opens an exact-effect approval menu and commits `PermissionRequired`; Deny becomes a structured result but can look Completed. [broker.rs:327](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/broker.rs:327), [worker.rs:4981](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4981), [actor.rs:2518](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2518)
- **Recommended:** Ask remains **PARK-FOR-USER**. Deny is **SURFACE-AND-CONTINUE** with a visible denied row and `p Inspect policy` where editable.

### Grant-ceiling violation

- **Today:** Protocol says a child may only subdivide the parent’s tool/effect ceiling, but production dispatch does not enforce manifest grants; delegated children are created with writes and exec enabled, and filtering only removes `todo_write`. [agent.rs:23](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/agent.rs:23), [delegation.rs:178](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:178), [worker.rs:3331](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3331)
- **Gap:** This is a security-enforcement gap, not merely an error-presentation gap.
- **Recommended:** Enforce at declaration filtering and dispatch. Ordinary violation → **SURFACE-AND-CONTINUE** as `grant_ceiling_violation`; corrupt/inconsistent grant state → **FATAL-WITH-GUIDANCE**. Never ask the user to elevate a child beyond its parent.

### Org policy blocks provider-hosted tool

- **Today:** A broad Anthropic invalid-request seam degrades provider web tooling only after the failed turn; 403 is not specifically recognized. [worker.rs:1904](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:1904), [worker.rs:6248](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:6248)
- **Recommended:** Explicit capability-only denial → same-turn **RECOVER/DEGRADE** to local tool. Whole-account policy denial → **PARK-FOR-USER** with admin/account actions.

## 7. Subagent and delegation

### Child errored/cancelled

- **Today:** Becomes a red child report and is fed back to the parent model, so the parent can continue. The associated spawn tool is nevertheless closed as Completed. [delegation.rs:921](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:921), [actor.rs:3094](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3094), [actor.rs:3201](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3201)
- **Recommended:** Parent-level **SURFACE-AND-CONTINUE** is correct. Mark spawn Failed/Partial and offer `r Restart child`, `m Send repair`, `o Open transcript`.

### Spawn establishment/launch failure

- **Today:** Failure before durable establishment ends the operation; failure after establishment becomes a red deferred child report, avoiding blind duplicate spawn. [worker.rs:4567](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4567), [worker.rs:4595](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4595)
- **Recommended:** Pre-establishment transient contention may **AUTO-RETRY** only under an idempotent fence. Post-establishment is **SURFACE-AND-CONTINUE**, never automatic respawn.

### Supervision/stall timeout

- **Today:** Parent waits under `Waiting { LocalChild }`; after 120 seconds supervision nudges once, then cancels at the next deadline. Descendant permission/input suppresses stall cancellation. [actor.rs:1680](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1680), [delegation.rs:421](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:421), [delegation.rs:1092](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/delegation.rs:1092)
- **Recommended:** Existing nudge/cancel is **RECOVER/DEGRADE**, but surface “stalled; nudged; auto-cancel in N.” Offer extend/cancel/open-child. Child permission/input should promote the exact menu and **PARK-FOR-USER**.

### Cross-device lane unavailable/cellular drop

- **Today:** Placement protocol says current implementation is local-only. `RemoteChild` and `DeviceUnreachable` states, badges and notifications exist, but no daemon production path emits them. [agent.rs:49](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/agent.rs:49), [state.rs:102](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/state.rs:102), [projection.rs:1147](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/projection.rs:1147)
- **Gap:** This is schema/UI scaffolding, not implemented remote-error handling.
- **Recommended:** Initial flap → visible bounded **AUTO-RETRY** with device identity and attempts. Threshold → **PARK-FOR-USER** with `r Reconnect`, `l Reroute locally`, `c Cancel lane`. If a remote effect may have run, enter outcome-unknown reconciliation before any reroute/retry.

### Failed-lane verdict aggregation

- **Today:** No dedicated multi-lane aggregator; red child reports are simply passed to the parent model for synthesis. [actor.rs:3148](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:3148)
- **Recommended:** Preserve per-lane status. A failed/unreachable required lane must make the aggregate Partial/Red, never Verified. Optional-lane failure → **RECOVER/DEGRADE**; required quorum failure → **PARK-FOR-USER**.

## 8. Session, workspace and daemon

### Initial UDS connection/handshake

- **Today:** Typed missing/refused/permission/I/O/handshake/protocol errors; missing/refused can trigger bounded daemon spawn/redial. CLI prints differentiated failure and exits. [client.rs:75](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/client.rs:75), [spawn.rs:125](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/spawn.rs:125)
- **Recommended:** Missing/refused → existing **AUTO-RETRY**/spawn. Permission/protocol/profile mismatch → **FATAL-WITH-GUIDANCE**, with `r Retry`, `l Open daemon log`.

### Established UDS disconnect/daemon crash

- **Today:** Pending correlations are cleared; TUI preserves cursors, shows “reconnecting,” reattaches and replays durable commands. Receipt-free uploads and staged login are dropped with limited guidance. [client.rs:338](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/client.rs:338), [live.rs:2504](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/live.rs:2504), [live.rs:2801](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/live.rs:2801)
- **Critical gap:** Link uses `connect`, not `ensure_daemon`, ignores every failure, and retries forever up to every five seconds. A real crash with no external restart leaves permanent “reconnecting.” Successful reconnect does not fully revalidate version/features/profile. [link.rs:241](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/link.rs:241)
- A dead Link reply channel can make live runtime return `Ok(())`, silently exiting successfully. [runtime.rs:2813](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/runtime.rs:2813)
- **Recommended:** Bounded **AUTO-RETRY**, then **PARK-FOR-USER** with `r Restart daemon`, `l Open log`, `q Exit`. Unexpected Link termination is **FATAL-WITH-GUIDANCE**.

### Daemon restart mid-turn

- **Today:** Queued/parked checkpoints may be reconstructed. Other nonterminal runs close items and append “run was interrupted by daemon restart,” Errored, then interrupted Idle. [turn_recovery.rs:1](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/turn_recovery.rs:1), [turn_recovery.rs:565](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/turn_recovery.rs:565)
- **Gap:** Failure is marked retryable, but no retry action exists.
- **Recommended:** **PARK-FOR-USER** with `r Retry turn`, explicitly warning that prior provider/tool work may already have occurred.

### `EffectOutcomeUnknown`

- **Today:** Startup detects Dispatched-without-Outcome and appends `EffectOutcome::Unknown`; it never retries. TUI says “reconcile via the recovery menu” and has a badge. [recovery.rs:1](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/recovery.rs:1), [runtime.rs:719](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/runtime.rs:719), [projection.rs:442](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/projection.rs:442)
- **Major gap:** Production does not emit `RunState::EffectOutcomeUnknown` or construct a Recovery menu; the UI instruction is false.
- **Recommended:** **PARK-FOR-USER** with exact-effect actions: `p Probe`, `s Mark succeeded`, `f Mark failed`, `r Retry` only if provably idempotent. Until implemented, remove the recovery-menu promise.

### Session metadata vanished/corrupt

- **Today:** Missing row and legacy empty metadata both return `None`; worker then mislabels them `InvalidArgument`. Corrupt JSON is `StoreCorrupt`, but RPC mapping collapses many internal/store errors to `invalid_argument`. [event_store.rs:1021](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:1021), [worker.rs:3261](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3261), [session_hub/rpc.rs:3755](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/session_hub/rpc.rs:3755)
- **Recommended:** Missing row → **RECOVER/DEGRADE** by refreshing roster. Legacy/corrupt session → **FATAL-WITH-GUIDANCE**, preserving export/repair. Add distinct stable codes.

### Workspace root gone/unreadable

- **Today:** Creation validates the canonical directory. Later project-instruction failure only logs and silently omits instructions; tool-factory failure becomes wrongly labelled ProviderError. [session_hub/rpc.rs:3876](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/session_hub/rpc.rs:3876), [project_instructions.rs:64](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/project_instructions.rs:64), [broker.rs:835](/Users/rizzist/haider-run/b2b-tui/crates/haider-tools/src/broker.rs:835)
- **Recommended:** **PARK-FOR-USER** under `workspace_unavailable`, with `r Recheck`, `c Choose/rebind workspace`, `o Open parent`. Previously-present instructions disappearing should produce a durable warning.

### Compaction failure

- **Today:** Compaction/provider/store failures terminalize cleanly; repeated overflow has an explicit internal message. Manual compaction rejects Busy/stale generation. [actor.rs:2064](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2064), [worker.rs:2785](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:2785), [worker.rs:2916](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:2916)
- **Gap:** UI cannot distinguish retry-summary, larger-model, or new-session recovery.
- **Recommended:** Transient summarizer failure → explicit retry/**SURFACE-AND-CONTINUE**. Irreducible context → **PARK-FOR-USER** with model/new-session/compact actions.

### Generation/fencing conflict and Busy

- **Today:** Stable stale-generation/overloaded/busy codes exist. Revision conflicts refresh account/provider snapshots. Other retryable durable commands remain dormant in the outbox and may execute only after an unrelated reconnect. [rpc/frame.rs:94](/Users/rizzist/haider-run/b2b-tui/crates/haider-rpc/src/frame.rs:94), [live.rs:2378](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/live.rs:2378), [live.rs:2801](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/live.rs:2801)
- **Recommended:** Stale generation → **RECOVER/DEGRADE** via refresh/reattach and one semantic replay. Busy → visible bounded **AUTO-RETRY** under the same command ID, with cancel. Never leave retryable mutations dormant until a future reconnect.

### CAS artifact missing/corrupt/unavailable

- **Today:** CAS writes atomically and verifies reads. Missing becomes InvalidArgument; corruption becomes StoreCorrupt; I/O is Internal. Turn admission masks every `get` error—including corruption—as `attachment_not_found`. [cas.rs:149](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/cas.rs:149), [cas.rs:214](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/cas.rs:214), [session_hub/rpc.rs:4654](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/session_hub/rpc.rs:4654)
- **Recommended:** Missing → **RECOVER/DEGRADE**, retain local source and offer `u Re-upload`. Corruption → **FATAL-WITH-GUIDANCE**, quarantine/re-upload. Transient I/O → bounded retry. Never describe corruption as ordinary absence.

## 9. Infra and local

### Disk full/read-only/local-store I/O

- **Today:** SQLite only treats busy/locked as retryable; ENOSPC/read-only generally becomes Internal. CAS maps all I/O to Internal. Critically, supervisor failure handling ignores errors while appending failure/Idle events, so disk-full at the journal boundary can leave a durably nonterminal or silent run. [event_store.rs:6098](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/event_store.rs:6098), [cas.rs:382](/Users/rizzist/haider-run/b2b-tui/crates/haider-store/src/cas.rs:382), [worker.rs:1775](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:1775)
- **Gap:** This falls outside the ordinary `RunFailed`/F2e guarantee because the error journal itself is unavailable.
- **Recommended:** Profile-level **FATAL-WITH-GUIDANCE**: stop accepting mutations, keep safe reads/exports, and show an out-of-band persistent banner naming the path: “Free space, then press r.” Add `disk_full`, `read_only`, `store_unavailable`.

### Self-update failure/rollback

- **Today:** Strong transactional design: typed CLI errors, staging before mutation, exact-pair verification, rollback on commit/restart/health failure, and interrupted-marker recovery on a later invocation. [update/mod.rs:89](/Users/rizzist/haider-run/b2b-tui/crates/haider-cli/src/update/mod.rs:89), [transaction.rs:448](/Users/rizzist/haider-run/b2b-tui/crates/haider-cli/src/update/transaction.rs:448), [restart.rs:188](/Users/rizzist/haider-run/b2b-tui/crates/haider-cli/src/update/restart.rs:188)
- **Gap:** “recovery assets retained” does not name the marker/path or give an explicit recovery command.
- **Recommended:** Existing rollback is **RECOVER/DEGRADE**. Ambiguous rollback/drain → **FATAL-WITH-GUIDANCE**, naming transaction path and a real recover/doctor command; never send another signal automatically.

### Core config malformed

- **Today:** Deliberately loud and path-specific; daemon prints error and exits. [profile.rs:91](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/profile.rs:91), [daemond/main.rs:55](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemond/src/main.rs:55)
- **Recommended:** **FATAL-WITH-GUIDANCE** with file plus JSON line/column, `o Open config`, `r Retry`. Ensure the established-TUI reconnect path exposes this rather than reconnecting forever.

### Hooks malformed/timeout/nonzero

- **Today:** Malformed documents/entries become `HookNotice`s and are skipped. Hook execution is bounded and killed on timeout. Decision-hook failure proposes no decision, so the original permission Ask remains. Details are mainly visible on `/hooks`, not the session transcript. [hooks.rs:2050](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/hooks.rs:2050), [hooks.rs:1395](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/hooks.rs:1395), [hooks.rs:1571](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/hooks.rs:1571)
- **Recommended:** Optional malformed hook → **SURFACE-AND-CONTINUE** with durable session notice. Decision-hook failure → **PARK-FOR-USER** on the original permission question, explicitly naming the timeout/failure; offer retry/disable/open-config.

### STT/voice device errors

- **Today:** Rich `SttError` taxonomy; missing model/runtime opens setup, invalid key remains inline, mic health issues flash, and partial ghost text is preserved. But post-start CPAL error callbacks discard errors, and a dead Talk supervisor channel is ignored, risking silence or stuck state. [stt/lib.rs:77](/Users/rizzist/haider-run/b2b-tui/crates/haider-stt/src/lib.rs:77), [app.rs:5532](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/app.rs:5532), [capture.rs:472](/Users/rizzist/haider-run/b2b-tui/crates/haider-stt/src/capture.rs:472), [stt_runtime.rs:97](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/stt_runtime.rs:97)
- **Recommended:** Mic permission/device → **PARK-FOR-USER**, `o Open microphone settings`, `r Retry`. Missing model/runtime → existing **RECOVER/DEGRADE**. Network failure after audio capture → preserve partial and **SURFACE-AND-CONTINUE**; never auto-replay live audio. Supervisor death must settle state and be persistent.

### Attachment MIME/size/encoding/upload

- **Today:** Client enforces 5 MiB loading, image magic sniffing and strict UTF-8 fallback; daemon independently checks count, MIME, basename, sizes and aggregate limit. TUI shows validation errors, but upload failure removes the chip/source; disconnect says `/attach again`. [headless.rs:104](/Users/rizzist/haider-run/b2b-tui/crates/haider-client/src/headless.rs:104), [runtime.rs:2440](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/runtime.rs:2440), [session_hub/rpc.rs:4583](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/session_hub/rpc.rs:4583), [live.rs:2527](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/live.rs:2527)
- **Recommended:** Validation refusal → **SURFACE-AND-CONTINUE**, retaining a failed chip with Remove/Retry. Transport/store failure → **RECOVER/DEGRADE**, retaining source until submit acceptance and offering `r Retry upload`.

### TUI settings load/save — additional class found

- **Today:** Malformed/unreadable settings silently default; save returns a private boolean and callers discard create/write/rename failure. [settings.rs:84](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/settings.rs:84), [settings.rs:112](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/settings.rs:112)
- **Recommended:** Load → **RECOVER/DEGRADE** with a one-time note. Save → **SURFACE-AND-CONTINUE**: “applied for this run; could not save to PATH,” with retry.

## Other cross-cutting gaps found

- Unknown future event payloads are counted but silently ignored; sequence gaps trigger reattach. A sustained compatibility mismatch should show a durable “client/daemon incompatible—update” diagnostic rather than merely omitting data. [projection.rs:273](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/projection.rs:273), [projection.rs:322](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/projection.rs:322)
- Desktop notifications deliberately ignore Retrying and only notify on final Errored. That is appropriate, but long parked retry/reset states should notify when user action becomes necessary. [notify.rs:46](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/notify.rs:46)
- Anthropic non-success response bodies are not bounded like OpenAI/Gemini. Add the same 64 KiB cap before parsing/logging. [anthropic.rs:539](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/anthropic.rs:539)

## Top eight improvements

1. **Introduce an end-to-end typed error-presentation contract.** Carry stable subcode, safe title/detail, provider status/request ID, retry/reset time, scope, and allowed recovery actions through `RunFailed` and tool results. Do not expose raw provider bodies.

2. **Fix tool terminal status and rendering.** Denied, conflicted, nonzero, killed, malformed, depth-capped and missing-child tools must not render green `Completed`; show the bounded reason inline.

3. **Add first-class auth and quota recovery cards.** Distinguish OAuth expiry, invalid BYOK, revoked account, rate limit and credit exhaustion. Provide one-key re-login/re-import/edit-key/switch-account/top-up actions; never retry billing exhaustion ten times.

4. **Make partial-stream interruption explicit.** Mark partial assistant output incomplete, explain why blind retry was suppressed, and offer “continue from partial” versus “retry from scratch.”

5. **Close the silent-error holes outside the journal.** Add profile-level handling for disk-full/read-only/store failure, unexpected Link/Talk supervisor death, and post-start microphone errors.

6. **Implement actual effect-outcome reconciliation.** Production must emit `EffectOutcomeUnknown` and provide probe/mark/retry/abandon choices—or stop promising a nonexistent recovery menu.

7. **Build the remote-lane state machine before enabling cross-device placement.** Include device identity, bounded reconnect, local reroute/cancel, effect ambiguity reconciliation, and failure-aware lane aggregation.

8. **Use same-turn bounded recovery where safety permits.** Reclassify pre-content premature EOF as transport; retry idempotent web GETs; fall back from explicitly rejected provider-hosted tools; allow one repair attempt for malformed tool JSON; add visible bounded retries for Busy under the same command ID.
