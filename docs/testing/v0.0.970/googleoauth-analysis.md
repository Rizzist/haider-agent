# PLAN.md

Research date: 2026-09-03  
Scope: wave 970, read-only research and implementation plan. No files changed and no credentials were printed.

## Decision

**RECOMMENDED — Option B: integrate Google Antigravity as a distinct Haider account/provider through Google’s official ACP agent.**

Do not implement direct Google subscription OAuth or reuse private Cloud Code Assist endpoints in 970. Keep the existing API-key Gemini provider unchanged.

Personal Google OAuth should initially be off by default/nightly and must pass a policy release gate. Google publishes the ACP executable and documents installing it in Zed, but its current Antigravity terms also prohibit third-party software from accessing the service. Haider is not explicitly named as an approved client.

## Findings

### Haider today

- **VERIFIED — code:** Haider’s release-owned OAuth allowlist records issuer, authorization/token endpoints, client ID, scopes, redirect policy, identity verification, inference endpoint/header set, and refresh policy. The generic browser flow uses a fresh numeric loopback listener and random path. [oauth.rs](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:189)

- **VERIFIED — code:** OpenAI OAuth uses authorization code, verified ID-token identity, bearer authentication, and the Codex subscription Responses endpoint/header dialect. [oauth.rs](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:301)

- **VERIFIED — code:** Anthropic OAuth uses its Claude authorization/token endpoints, a token-endpoint-derived local identity, bearer authentication, Anthropic’s OAuth beta headers, and conservative refresh. [oauth.rs](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:332)

- **VERIFIED — code:** OAuth tokens are encoded into a versioned binary envelope with zeroizing fields; debug output redacts access, refresh, and ID tokens. Adapters receive only an access-token handle. [oauth.rs](/Users/rizzist/haider-run/wt-965/crates/haider-accounts/src/oauth.rs:1)

- **VERIFIED — code:** Haider’s default vault is prompt-free file storage with `0700` directories, `0600` files, atomic replacement, and zeroizing reads. [file_vault.rs](/Users/rizzist/haider-run/wt-965/crates/haider-accounts/src/file_vault.rs:1)

- **VERIFIED — code:** Gemini is currently registered as API-key-only. [provider_registry.rs](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/provider_registry.rs:1389)

- **VERIFIED — code:** `GeminiProvider` is bound to a resolved API key and sends GenerateContent requests with `x-goog-api-key` to a fixed, guarded model endpoint using SSE. [gemini.rs](/Users/rizzist/haider-run/wt-965/crates/haider-provider/src/gemini.rs:158), [gemini.rs](/Users/rizzist/haider-run/wt-965/crates/haider-provider/src/gemini.rs:383)

- **VERIFIED — code:** The account-backed factory has a Gemini/API-key branch but no Gemini/OAuth branch. OpenAI and Anthropic OAuth are separate sanctioned branches. [accounts.rs](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/accounts.rs:7880)

- **VERIFIED — code:** Account descriptors contain provider, authentication method, identity, status, label, optional account identity, and creation time, but no structured source or credential-owner field. [credential.rs](/Users/rizzist/haider-run/wt-965/crates/haider-protocol/src/credential.rs:59)

- **VERIFIED — code:** Explicit account selection already exists through `--account <alias>`. [run.rs](/Users/rizzist/haider-run/wt-965/crates/haider-cli/src/run.rs:278)

### T3 Code PR #9348 and the official agent

- **VERIFIED — URL:** PR #9348 merged on 2026-09-03 and was included in nightly `v0.0.39-nightly.20260903.1268`. It added Antigravity as an off-by-default provider driven entirely through Google’s ACP executable. [pingdotgg/t3code PR #9348](https://github.com/pingdotgg/t3code/pull/9348)

- **VERIFIED — URL:** This is not an npm package or ordinary CLI integration. Google’s registry entry identifies a proprietary `antigravity-acp` binary distribution authored by Google LLC, downloaded from `dl.google.com`. The executable is `agy_acp_server.par` on Unix-like systems and `.exe` on Windows. [official ACP registry entry](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json)

- **VERIFIED — URL:** The registry currently lists macOS ARM64, Linux x64/ARM64, and Windows x64/ARM64 packages. There is no Intel macOS package. [registry entry](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json)

- **VERIFIED — URL:** T3 downloads and hash/size-verifies the archive, keeps immutable version directories, leases versions used by running processes, and validates an installation with ACP `initialize`. Each account gets an isolated `GEMINI_HOME`; Google’s agent owns its credential files. [PR #9348](https://github.com/pingdotgg/t3code/pull/9348), [T3 Antigravity documentation](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-antigravity.md)

- **VERIFIED — URL:** The current Linux x64 installation is approximately 543 MB compressed and 1.65 GB extracted; T3 recommends at least 2.5 GB free. These are T3 observations, not a Google resource guarantee. [T3 Antigravity documentation](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-antigravity.md)

- **VERIFIED — URL:** Authentication is agent-owned. The agent emits an OAuth URL outside the normal ACP JSON stream, so T3 recognizes only an exact expected prefix, supplies a controlled browser helper, and supports loopback completion or securely forwarding the failed loopback return URL from another device. [PR #9348](https://github.com/pingdotgg/t3code/pull/9348)

- **VERIFIED — URL:** ACP uses JSON-RPC 2.0. Its normal sequence is `initialize`, optional `authenticate`, `session/new` or `session/load`, `session/prompt`, streamed `session/update` notifications, permission/filesystem callbacks, and `session/cancel`. Agents normally run as client subprocesses. [ACP protocol overview](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/protocol/v1/overview.mdx)

- **VERIFIED — URL:** Authentication methods are advertised by the agent. Protocol-driven authentication uses `authenticate`; terminal-style authentication launches a separate interactive process. Logout is capability-negotiated. [ACP authentication specification](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/protocol/v1/draft/authentication.mdx)

- **VERIFIED — URL:** T3 maps ACP file reads/writes and permission requests into its own workspace containment and approval surfaces, supports session resume and cancellation, and treats ACP as an agent runtime—not merely another GenerateContent HTTP dialect. [PR #9348](https://github.com/pingdotgg/t3code/pull/9348)

### Models and quotas

- **VERIFIED — URL:** The official Antigravity catalog currently includes Gemini 3.8 Flash and Gemini 3.7 Flash, with thinking-level variants. Google’s CLI documentation shows account-visible model slugs such as 3.8 Flash High/Medium and 3.7 Flash High/Medium. Availability remains account-dependent. [Antigravity model documentation](https://antigravity.google/docs/models/), [headless CLI documentation](https://antigravity.google/docs/cli/headless/)

- **VERIFIED — URL:** During T3’s live test, Google returned 11 models and identified 3.7 Flash High as its default. T3 overrode the new-thread default to 3.8 Flash High when available and grouped older models as legacy. [PR #9348](https://github.com/pingdotgg/t3code/pull/9348)

- **VERIFIED — URL:** Google does not publish fixed prompt counts. Pro and Ultra baseline quota refreshes every five hours until a weekly ceiling is reached; non-Pro/Ultra accounts use a weekly refresh. Limits are capacity- and workload-dependent and may change. [Antigravity plans](https://antigravity.google/docs/plans)

- **VERIFIED — URL:** The Antigravity CLI has a `/usage` panel, but the ACP integration used by T3 does not expose structured plan or remaining-quota data to the client. T3 explicitly reports neither paid tier nor remaining subscription quota. [Google quota UI](https://www.antigravity.google/docs/cli/commands/usage), [T3 Antigravity documentation](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-antigravity.md)

### oh-my-pi and Gemini CLI direct OAuth

- **VERIFIED — URL:** oh-my-pi implements an authorization-code browser flow itself, retrieves user information, discovers or provisions a Cloud Code Assist project, stores access/refresh credentials through its own credential layer, and refreshes them directly. [shared Google OAuth implementation](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/registry/oauth/google-oauth-shared.ts)

- **VERIFIED — URL:** Its Gemini CLI lane uses Google’s installed-application registration, the `cloudcode-pa.googleapis.com` service, and private `v1internal:loadCodeAssist`/`onboardUser` operations. [google-gemini-cli.ts](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/registry/oauth/google-gemini-cli.ts)

- **VERIFIED — URL:** Its Antigravity lane uses a separate OAuth registration, additional scopes, Antigravity metadata, and the `daily-cloudcode-pa.googleapis.com` service. [google-antigravity.ts](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/registry/oauth/google-antigravity.ts)

- **VERIFIED — URL:** oh-my-pi obtains per-model `remainingFraction` and `resetTime` by directly calling the private `v1internal:retrieveUserQuota` endpoint. [Gemini usage collector](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/usage/gemini.ts)

- **VERIFIED — URL:** The official Gemini CLI source confirms the installed-app OAuth registration, Cloud Platform and user-info scopes, random loopback callback, alternate user-code PKCE flow, and credential persistence/refresh. [Gemini CLI OAuth source](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/code_assist/oauth2.ts)

- **VERIFIED — URL:** The official CLI also implements `loadCodeAssist`, `onboardUser`, GenerateContent, streaming, and `retrieveUserQuota` against Code Assist. [Gemini CLI Code Assist server](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/code_assist/server.ts)

- **VERIFIED — URL:** Google ended consumer Google AI Pro/Ultra and individual/free Gemini CLI service on 2026-06-18, directing those users to Antigravity. Enterprise and paid API-key cases remain supported. Therefore the Gemini CLI direct route is also a retired consumer target. [Google transition announcement](https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/)

### Terms and policy

- **VERIFIED — URL:** Google’s current Gemini CLI documentation explicitly says that directly accessing the services behind Gemini CLI with third-party software violates applicable terms and may lead to account suspension or termination. [Gemini CLI terms and privacy guide](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md)

- **VERIFIED — URL:** Antigravity’s current terms likewise state that third-party software accessing Antigravity—including through Antigravity OAuth—breaches the agreement and can result in suspension or termination. [Google Antigravity terms](https://antigravity.google/terms)

- **VERIFIED — URL:** Conversely, Google publishes the proprietary ACP binary and its official Zed documentation instructs users to install Antigravity from Zed’s external-agent registry and configure `oauth-personal`. [Google’s Zed instructions](https://antigravity.google/docs/ide/extensions/zed), [ACP registry entry](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json)

- **ASSUMED:** Publishing an ACP agent in a registry intended for arbitrary ACP clients indicates broader interoperability intent. The public documentation explicitly approves Zed, however, not Haider; it does not clearly override the general third-party restriction.

- **ASSUMED:** Theo’s reported statement that DeepMind employees consider the clause obsolete and unenforced is accurate as hearsay. I could not locate a primary public post, recording, Google policy statement, or PR comment containing that assurance. PR #9348 verifies that Theo shipped the feature, but not the ToS assertion.

- **VERIFIED — URL:** Google’s Generative AI Prohibited Use Policy separately prohibits harmful/illegal activity, attacks on infrastructure, bypassing safety protections, and other enumerated misuse. Haider must preserve its own safety and permission enforcement. [Google Generative AI Prohibited Use Policy](https://policies.google.com/terms/generative-ai/use-policy)

## Options

| Option | Architecture | Benefits | Problems | 970 verdict |
|---|---|---|---|---|
| A — Native OAuth | Haider owns Google OAuth tokens and calls Code Assist/Antigravity HTTP services directly. | Lowest process/disk overhead; direct access to per-model quota buckets; could reuse some Gemini normalization. | **VERIFIED:** requires private `v1internal` endpoints and a different bearer/project/request dialect, so the existing API-key Gemini adapter is not reusable unchanged. **VERIFIED:** current Google documentation expressly prohibits this third-party access. **VERIFIED:** the consumer Gemini CLI path has been retired. | Reject. |
| B — Official ACP agent | Haider supervises Google’s proprietary ACP subprocess; Google’s agent owns OAuth, models, sessions, and credentials. | Uses the Google-authored package; follows the same basic integration Google documents for Zed; dynamic account-specific catalog; no Google token enters Haider’s vault. | Large managed runtime; full agent/runtime semantics rather than ordinary model-provider semantics; no structured ACP quota API; unresolved ToS wording for non-listed clients. | **Recommend, release-gated.** |
| C — Both | Ship native HTTP and ACP paths. | Maximum fallback and experimentation. | Doubles credential ownership, model catalogs, tests, account naming, failure modes, and policy exposure. Native path remains expressly prohibited and could silently diverge from ACP behavior. | Reject for 970. |

## Recommended 970 design

### 1. Distinct provider and execution kind

Create `google-antigravity` as a separate built-in provider. Do not change the meaning of the existing `gemini` provider.

Add a registry execution discriminator equivalent to:

- `model_provider`: existing normalized HTTP `Provider` flow.
- `acp_agent`: supervised JSON-RPC agent session.

This avoids pretending the Antigravity agent is a GenerateContent transport and prevents its internally executed tools from entering Haider’s local LLM tool loop a second time.

### 2. Account registration and source badge

Register an account only after authentication and model discovery both succeed:

- `provider`: `google-antigravity`
- `auth_method`: `oauth`
- `credential_owner`: external official-agent profile
- `source`: `google_antigravity_acp`
- visible badge: `Google · official Antigravity ACP`
- plan: absent unless the agent reports it
- identity: sanitized agent-reported email/name when available; otherwise `Google account`, without claiming verified OIDC identity

Add optional, backward-compatible `source` and `credential_owner` descriptor fields. An ACP account must not require a dummy vault secret. The resolver should select an external agent profile instead of resolving a `SecretHandle`.

Each alias gets its own private profile directory. Multiple accounts must never share `GEMINI_HOME`, authentication state, sessions, or logout effects.

### 3. Login UX

Support both:

- `/login google` — preferred shortcut
- `/login google-antigravity oauth` — explicit existing-grammar form

The flow:

1. Show the official-agent source, proprietary license, approximate download/storage cost, Google terms link, and interaction-data notice.
2. If missing, offer an explicit managed download; never download silently.
3. Start an authentication-scoped agent with a controlled environment.
4. Strip ambient Google/Gemini credential variables; pass only the per-account profile path and required fixed launch configuration.
5. Recognize only the exact expected OAuth URL output. Treat all other stdout as ACP protocol data.
6. Open the URL locally or allow the same-attempt loopback return URL to be pasted for remote login.
7. Never journal, log, display in history, or include in errors the return URL query, authorization code, or token material.
8. Reconnect, `initialize`, confirm authenticated access, fetch models, then commit the account descriptor.
9. On cancellation, expiry, client disconnect, or source-surface replacement, retire the attempt and reject late callbacks.
10. Logout through the advertised ACP capability, stop sessions for that alias, remove the agent-owned credential profile, and update the account state.

The policy warning is informational, not a substitute for the release gate below.

### 4. Runtime and ACP mapping

Use one supervised worker per active account, multiplexing its ACP sessions only if the agent advertises safe session support.

Required mapping:

- `initialize`: require compatible protocol version and exact required capabilities.
- `authenticate`: select only `oauth-personal`; never fall back to API key, ADC, or another Google account.
- `session/new`/`session/load`/`session/resume`: retain the ACP session ID as provider-opaque continuation state.
- `session/prompt`: one Haider turn at a time per ACP session.
- message chunks → `TextDelta`
- thought chunks → `ReasoningDelta`
- agent-executed tool activity → display-only server-tool events
- permission requests → Haider’s durable permission/menu system
- file reads/writes → Haider-controlled workspace operations
- cancel → `session/cancel`, followed by bounded child termination if unresponsive
- stop reason → exactly one normal Haider terminal envelope

Do not advertise filesystem or terminal capabilities that Haider cannot enforce. Absolute-path validation, symlink resolution, workspace containment, attachment-directory containment, lockdown restrictions, and write approval must be applied before any effect.

Haider’s stable tool IDs and exactly-one-terminal replay guarantees remain authoritative. [JSONL contract](/Users/rizzist/haider-run/wt-965/docs/jsonl-run-contract-v1.md:61), [provider lockdown](/Users/rizzist/haider-run/wt-965/docs/provider-lockdown-v1.md:30)

### 5. Installer and process security

- Download only from the official registry-resolved Google archive.
- Ship a release-owned pin containing agent version, archive URL, expected size, and SHA-256.
- Reject redirects away from approved Google origins.
- Defend extraction against traversal, links, duplicates, and oversized output.
- Validate both the ACP executable and its sibling helper before activation.
- Use immutable version directories and an atomic active-version pointer.
- Lease active versions so updates cannot replace a running executable.
- Require private directory/file permissions and reject unsafe ownership or world-writable binaries.
- Drain bounded stderr independently so a noisy child cannot deadlock.
- Bound line/message sizes, pending JSON-RPC requests, startup time, prompt time, cancellation time, and restart attempts.
- No automatic upgrades during 970; updates are explicit and version-pinned.

### 6. Models

Use only the account-specific catalog returned by the official agent.

- Prefer `gemini-3.8-flash-high` for a new session only when that exact model is offered.
- Otherwise use the agent-designated default and record the resolved model.
- Preserve reasoning variants as distinct model entries.
- Mark older generations as legacy through Haider’s release-owned model manifest.
- On resume, refuse with a model-selection remedy if the stored model disappeared; do not silently substitute.
- Do not promise Claude or other third-party models: the present official ACP surface observed by T3 exposes Gemini models only.

### 7. Accounts calendar and quota

Haider’s current usage protocol can represent metered, unavailable, and local-only states, including named windows and exact reset instants. [usage.rs](/Users/rizzist/haider-run/wt-965/crates/haider-protocol/src/usage.rs:293)

For 970:

- When the official ACP agent publishes structured remaining fraction and reset time, map each model bucket to `utilization = 1 - remaining_fraction`, retain its exact reset timestamp, and write the existing integer-basis-point calendar sample.
- When an upstream quota error includes an exact retry/reset instant, set the account to `Limited { until_ms }` and publish a fully consumed, model-labelled retry window to the calendar.
- Otherwise return `AccountMeterStateV1::Unavailable` with a stable reason such as `official ACP agent does not expose structured quota`.
- Show Google’s five-hour/weekly cadence only as explanatory plan text. Do not synthesize an individual reset timestamp or percentage from it.
- Do not read the agent’s credential files, scrape `/usage`, or call `v1internal:retrieveUserQuota` directly.
- Do not label an account Pro or Ultra merely because it exposes a particular model.

This produces less quota detail than oh-my-pi, but preserves Haider’s rule that absent provider data is not rendered as zero or fabricated.

## Tests

### Protocol and account tests

- Additive descriptor round-trip with and without source/owner fields.
- Existing account fixtures remain byte/meaning compatible.
- Source badge appears consistently in CLI, TUI, account list, and usage views.
- External-owned accounts never invoke vault resolution or create placeholder secrets.
- `--account` selects the exact Google alias and rejects aliases from other providers.
- Removing or logging out one alias cannot affect another.

### Fake ACP conformance tests

- `initialize`/authentication/session/prompt/update/stop happy path.
- Dynamic models, thinking variants, missing saved model, and no-silent-fallback behavior.
- Split/coalesced JSON messages, malformed JSON, unknown additive fields, duplicate IDs, oversized frames, stderr floods, unexpected stdout, and early EOF.
- Authentication URL recognition versus arbitrary text containing a URL.
- Callback attempt correlation, cancellation, timeout, disconnect, and late-result rejection.
- Session resume, crash/restart, cancellation, and guaranteed child reaping.
- Exactly one durable terminal and byte-identical replay.

### Security tests

- Ambient credential variables are absent from the child.
- Tokens, authorization codes, callback queries, and profile contents never appear in logs, RPC frames, errors, debug output, snapshots, or JSONL.
- Archive traversal, symlink, ownership, hash, size, and partial-install failures.
- Workspace traversal and symlink escapes for every ACP filesystem call.
- Permission mapping for supervised, auto-edit, full-access, denial, stale answers, and lockdown.
- Agent-executed tools cannot be re-dispatched as local model tool calls.

### Usage tests

- Exact remaining-fraction normalization and reset preservation when structured data exists.
- Exact `Limited` and calendar behavior from provider retry timestamps.
- Missing quota data produces `Unavailable`, not zero usage.
- Documented five-hour/weekly cadence never becomes a fabricated timestamp.
- Unknown plan remains absent.

### Regression and live acceptance

- Existing Gemini API-key request bytes and catalog behavior remain unchanged.
- OpenAI and Anthropic OAuth login, refresh, source display, and next-turn selection remain unchanged.
- Real smoke tests on every advertised platform: install, OAuth, model discovery, 3.8 Flash turn, supervised file change, restart/resume, explicit account selection, cancellation, logout, and removal.
- Do not advertise a platform until its official binary has passed the live launch matrix.

## Release acceptance

970 is implementation-complete only when:

- `/login google` creates an account carrying the official-ACP source badge.
- No Google token is stored, resolved, or transmitted by Haider.
- Two Google aliases remain isolated across login, runtime, restart, and logout.
- `haider run … --account <alias>` deterministically uses the requested alias.
- Models come from the authenticated agent; 3.8/3.7 variants are not fabricated.
- ACP filesystem and permission effects pass through Haider’s policy and durable event system.
- A child crash, malformed frame, timeout, or cancellation leaves no orphan process and exactly one terminal event.
- Usage/calendar output shows exact provider reset data or an honest unavailable state.
- The existing Gemini API-key provider passes its unchanged regression suite.
- Download provenance, version, size, hash, installed footprint, cold-start latency, and steady-state RSS are recorded in the release evidence.
- **Policy gate:** counsel/owner records a go/no-go against the current Antigravity terms, ideally backed by written/public Google confirmation that third-party clients using Google’s official ACP agent are permitted. Until then, personal OAuth remains off by default/nightly.

## Deferred

- Native Google OAuth and all direct private Code Assist/Antigravity HTTP calls.
- Option C and automatic fallback between ACP, Gemini API key, and other authentication modes.
- Importing Gemini CLI, Antigravity CLI, IDE, or oh-my-pi credentials.
- Scraping agent profile files or `/usage` to obtain quota.
- Fixed numeric quota promises or inferred Pro/Ultra plan labels.
- Gemini Enterprise, Agent Platform, ADC, and ACP API-key authentication.
- Global Antigravity hooks, MCP configuration, custom skills, and background subagent orchestration.
- Conversation rewind where the agent does not support it.
- Intel macOS support unless Google publishes an official package.
- Automatic runtime updates and cross-version live-session migration.
- Replacing or merging the existing `gemini` API-key provider.

SHIP (plan delivered)