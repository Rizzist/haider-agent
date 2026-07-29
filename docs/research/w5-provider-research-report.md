# W5 research: provider breadth, OAuth accounts, management wire, and TUI surfaces

Date: 2026-07-29

Source pin: `w5-providers` at `40d0a62ccb342e615322489286f9a47e7b91ca50`

Simulator pin: `/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js`, SHA-256 `a6b7873c22dd4644f4ae6717c4b98c85440f59a91c27202017b76d7fffd797c8`

## Executive summary

W5 is an extension of a working credential and turn-resolution system, not a new accounts subsystem. The repository already has a provider-neutral streaming trait, a production Anthropic adapter, a deterministic `FakeProvider`, globally unique account aliases, one-active-account-per-provider validation, atomic descriptor persistence, Keychain-backed secret storage, a one-hop rotation seam, API-key staging, a bounded account actor, durable login receipts, next-turn provider resolution, and a fully modal masked login card. Replacing those pieces would discard the crash and secrecy work shipped in W3c2/TUI6 (`crates/haider-provider/src/lib.rs:117-175`, `crates/haider-provider/src/lib.rs:231-236`, `crates/haider-accounts/src/store.rs:168-329`, `crates/haider-accounts/src/resolver.rs:59-145`, `crates/haider-daemon/src/accounts.rs:1-25`, `crates/haider-tui/src/app.rs:708-743`).

The real backend deltas are narrower and more exact: a native OpenAI Responses adapter, a separate OpenAI-compatible Chat Completions adapter, a durable provider registry for base URLs, models, defaults, and availability; an authorization-code/PKCE engine whose live provider configurations are explicitly approved; token-bundle storage and refresh; additive account/provider management RPCs; account-actor mutations and receipts; resolver-backed turn selection; safe-boundary account rotation; and two real TUI management screens. Google does not have a compatible adapter in this tree and should appear as unavailable, not as a fake success. `local` and custom endpoints are real in W5 through the explicit Chat Completions compatibility family.

The native OpenAI adapter should use the Responses API. OpenAI's current model guidance recommends Responses for reasoning, tool use, and multi-turn work, and its typed stream maps directly to Haider's text, reasoning, tool-argument, usage, and finish events. OpenAI-compatible local/custom endpoints instead use Chat Completions because that is the broadly implemented compatibility surface. W5 must additionally preserve opaque/encrypted reasoning continuation items in the native family; displaying a reasoning summary while silently dropping continuation state would make later turns behaviorally incorrect (`crates/haider-protocol/src/provider.rs:9-63`).

OAuth capability and live subscription access are separate claims. W5b should build the complete native-app authorization-code flow: numeric loopback binding on an OS-assigned port, PKCE S256, high-entropy state, exact-path callback validation, token exchange, Keychain storage, single-flight refresh, refresh-token rotation handling, cancellation, and redacted progress. It must not copy a first-party client ID or fixed redirect from another CLI. OpenAI's public “Sign in with ChatGPT” is currently identity access and does not independently grant model tokens; the Codex CLI grant is Codex-specific. Anthropic explicitly says third parties may not offer Claude.ai login or route Free/Pro/Max credentials. Therefore live ChatGPT or Claude Max buttons remain unavailable unless Haider has a sanctioned client registration and permitted inference scopes; API-key OpenAI and Anthropic are the release's live remote providers.

The simulator is binding where it actually has a design. It defines several accounts per provider, API-key versus OAuth methods, global aliases, one selection per provider, provider-grouped `/accounts`, click-to-switch, one global add-action row, and login buttons (`tui.js:141-154`, `tui.js:3588-3688`). It does **not** define a plural `/providers` command, a standalone providers screen, remove-account, default-model management, alias entry, keyboard row selection, or real OAuth progress states. Those owner-directed W5 additions must be called new design, not described as a 1:1 port. The binding `/accounts` core is a 1:1 port; its named W5 management/accessibility extensions get separate goldens.

The most important local schema correction is alias identity. The protocol says `CredentialDescriptor.alias` is the global account alias, but current daemon login invents a profile/provider/command-hashed descriptor alias and puts the user's requested alias into `identity`; an end-to-end test pins that accidental behavior (`crates/haider-protocol/src/credential.rs:7-17`, `crates/haider-daemon/src/accounts.rs:351-389`, `crates/haider-daemond/tests/account_rpc_tests.rs:290-312`). W5 must restore `alias` as the public global alias, keep `identity` for verified email/handle, and namespace Keychain slots internally by profile. `/account <alias>` must never search `identity`.

Live rotation is also not wired today. `AccountsProviderFactory` bypasses `Resolver`, resolves the snapshot's active descriptor directly, and pins that provider for the whole logical turn. Core retries the same provider only before the first event; it rejects account changes in one turn's usage (`crates/haider-daemon/src/accounts.rs:872-954`, `crates/haider-daemon/src/worker.rs:62-84`, `crates/haider-core/src/actor.rs:776-820`, `crates/haider-core/src/actor.rs:2355-2373`). W5 should rotate only before the first provider event of the current provider request. After any text, reasoning, or tool delta, the error surfaces honestly; no alternate continues that request, and the next logical turn resolves durable account status afresh.

The recommended dependency order is W5a provider registry plus the native Responses and compatible Chat families; W5b generic OAuth/PKCE, token vault, and fake authorization server; W5c account/provider management wire, actor mutations, receipts, alias migration, and resolver service; W5d `/providers` and `/accounts`, OAuth/API modal flows, and safe-boundary live rotation. No live API or live OAuth grant is a gate. The release gates are fixture adapters, `FakeProvider`, a malicious-capable fake OAuth server, real UDS RPC, crash-boundary receipts, mutation checks, and a sentinel sweep covering API keys, access tokens, refresh tokens, authorization codes, and PKCE verifiers.

## Scope and method

This report treats the checked-out implementation, W3c2/TUI6 reviews, and the named simulator as binding prior art. All local claims were checked at the source pin above. Two independent read-only verification passes rechecked the provider/accounts/RPC/daemon citations and the simulator/TUI citations. The simulator was searched negatively as well as read positively, which matters because several requested management behaviors are not present there.

The current external protocol and provider-policy claims were checked against primary sources:

- OpenAI's [current model guidance](https://developers.openai.com/api/docs/guides/latest-model) says to use Responses for reasoning, tool-calling, and multi-turn workflows; the [Responses streaming reference](https://platform.openai.com/docs/api-reference/responses-streaming) defines the typed text, reasoning-summary, function-argument, refusal, completion, and usage events used below.
- [Sign in with ChatGPT](https://help.openai.com/en/articles/20001410-sign-in-with-chatgpt) currently describes identity sharing and says it does not independently share tokens; [Codex CLI and Sign in with ChatGPT](https://help.openai.com/en/articles/11381614-api-codex-cli-and-sign-in-with-chatgpt) describes a Codex-specific grant that can generate an API key. OpenAI Codex's own loopback implementation says ports 1455/1457 are tied to the Codex redirect allow-list, which is evidence not to copy that client configuration ([source](https://github.com/openai/codex/blob/main/codex-rs/login/src/server.rs)).
- Anthropic's [authentication documentation](https://code.claude.com/docs/en/authentication) describes Claude Code's subscription credentials, while its [legal and compliance documentation](https://code.claude.com/docs/en/legal-and-compliance) expressly prohibits third-party Claude.ai login and routing requests through Free/Pro/Max credentials.
- [RFC 7636](https://www.rfc-editor.org/rfc/rfc7636.html) governs PKCE; [RFC 8252 §7.3](https://datatracker.ietf.org/doc/html/rfc8252#section-7.3) requires loopback-only native-app listeners and recommends numeric loopback literals with ephemeral ports; [RFC 9700](https://www.rfc-editor.org/rfc/rfc9700.html) requires sender-constrained or rotating refresh tokens for public clients.

This is a research/brief artifact. No source or simulator file was changed. The only intended write is this report.

Verification on the source pin: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass. The existing ignored interactive-Keychain and live-Anthropic tests remain non-gating, which is the same boundary this report requires for W5.

## 1. Gap census: the implemented foundation and W5 delta

### 1.1 Provider boundary: already production-shaped, one adapter only

`TurnRequest` is already provider-neutral: messages, requested model, maximum output, optional system prompt, tool definitions, and resolved attachments (`crates/haider-provider/src/lib.rs:117-129`). `ProviderError` normalizes authentication, permission, rate-limit, overload, invalid request, transport, malformed frame/UTF-8, and internal failure, with retryability and `retry_after_ms` owned by the normalized error (`crates/haider-provider/src/lib.rs:131-175`). It has no dedicated “unsupported” kind today. `ProviderStream` owns its producer and aborts it on drop (`crates/haider-provider/src/lib.rs:188-229`). The trait itself is the intended narrow seam:

```text
Provider:
  capabilities() -> CapabilityDoc
  stream_turn(TurnRequest) -> ProviderStream
```

That exact boundary is at `crates/haider-provider/src/lib.rs:231-236`. `FakeProvider` is deterministic, records each request, and replays scripted events; its tests cover event mapping, malformed input, UTF-8 assembly, multiple requests, and producer abort (`crates/haider-provider/src/lib.rs:323-374`, `crates/haider-provider/tests/fake_provider_tests.rs:25-178`). It remains the turn-engine authority after OpenAI lands.

Only `AnthropicProvider` is exported as a production implementation (`crates/haider-provider/src/lib.rs:29-33`). It owns one resolved `SecretHandle`, account alias, model, HTTP client, and endpoint (`crates/haider-provider/src/anthropic.rs:45-56`). Its transport is the template: redirects disabled, HTTP retry disabled, bounded connect/open/chunk-idle time, sensitive auth headers, explicit status classification, incremental SSE decoding, and an owned producer (`crates/haider-provider/src/anthropic.rs:22-43`, `crates/haider-provider/src/anthropic.rs:66-88`, `crates/haider-provider/src/anthropic.rs:157-230`). The wire layer maps canonical messages/tools and normalizes text, thinking, tool calls, usage, and finish (`crates/haider-provider/src/wire/mod.rs:14-64`, `crates/haider-provider/src/wire/mod.rs:169-316`, `crates/haider-provider/src/wire/mod.rs:356-503`). HTTP 401/403/429/529/5xx already map to the shared taxonomy (`crates/haider-provider/src/anthropic.rs:373-407`).

There is no OpenAI, Google, Gemini, local, Hugging Face, or generic OpenAI-compatible adapter. The Anthropic endpoint override is explicitly a capture/test hook, not durable provider configuration (`crates/haider-provider/src/anthropic.rs:104-109`). Capability discovery is a model-name heuristic, not a registry or endpoint probe (`crates/haider-provider/src/anthropic.rs:239-258`).

### 1.2 Accounts: the credential substrate is done

The protocol descriptor already contains the product account shape: `alias`, `provider`, `auth_method`, `identity`, `status`, and `active`; `AuthMethod` already has `ApiKey` and `OAuth`; statuses already include `Ok`, rate-limited-until, `Expired`, and `Revoked`; rotation already has from/to aliases and a cause (`crates/haider-protocol/src/credential.rs:1-55`). OAuth is defined only as data today. No browser flow, token bundle, refresh, or OAuth-capable provider exists.

The store is stronger than the task shorthand implies. There is no type named `CredentialStore`; the actual persistence port is `StoreLike::{load, save}` and the domain owner is `AccountStore` (`crates/haider-accounts/src/store.rs:23-32`, `crates/haider-accounts/src/store.rs:168-179`). It already:

- atomically writes, syncs, renames, and syncs the parent directory (`crates/haider-accounts/src/store.rs:77-118`);
- adds globally unique aliases and selects a new active account without an invalid intermediate memory view (`crates/haider-accounts/src/store.rs:182-218`);
- supports `select`, `remove`, `list`, `get`, and `active_for_provider` (`crates/haider-accounts/src/store.rs:220-281`);
- validates exactly one active credential for every non-empty provider group before committing memory (`crates/haider-accounts/src/store.rs:283-329`).

`Vault` already has `put`, `resolve`, `delete`, and `list`; `MemoryVault` is the deterministic seam; macOS Keychain is the production backend (`crates/haider-accounts/src/vault.rs:64-150`, `crates/haider-accounts/src/keychain.rs:65-159`). `SecretHandle` is non-cloneable and formatting-redacted, though its own drop comment correctly calls byte filling best-effort; W5 OAuth buffers should use `Zeroizing` through every intermediate as W3c2's `SecretWire` already does (`crates/haider-accounts/src/vault.rs:19-61`, `crates/haider-rpc/src/frame.rs:249-295`). Non-macOS vault support remains explicitly unavailable (`crates/haider-accounts/src/keychain.rs:177-202`). `import_env` is one-shot and scrubs its local copy (`crates/haider-accounts/src/env_bridge.rs:15-57`).

`Resolver` is complete as a **seam**, not as live daemon policy. It resolves the active descriptor, rejects expired/revoked credentials, invokes rotation exactly once for a limited active credential, verifies that the alternate belongs to the same provider and is usable, and resolves a single alternate hop (`crates/haider-accounts/src/resolver.rs:1-7`, `crates/haider-accounts/src/resolver.rs:59-145`). Its tests pin the one-hop behavior (`crates/haider-accounts/tests/accounts_tests.rs:143-290`).

### 1.3 API-key login, actor, receipts, and next-turn resolution are done

The account wire already has `vault.stage`, durable `account.login_api`, and read-only `account.list`; the corresponding feature strings are `vault_stage_v1` and `account_login_api_v1` (`crates/haider-rpc/src/frame.rs:127-134`, `crates/haider-rpc/src/frame.rs:451-487`, `crates/haider-rpc/src/frame.rs:550-571`). `SecretWire` is transport-only, redacts formatting, and zeroizes on drop. Stage purpose currently includes API key and menu secret, not OAuth material (`crates/haider-rpc/src/frame.rs:249-310`).

Daemon staging is connection-scoped, single-use, TTL-bounded, random-reference, digest-deduplicated, and zeroizing (`crates/haider-daemon/src/accounts.rs:256-339`). Raw-secret RPC additionally requires Control authority and authenticated same-UID local UDS (`crates/haider-daemon/src/session_hub/rpc.rs:266-298`). The connection handler claims the stage and `try_send`s an owned job to a bounded account actor; it does not await validation or Keychain inline (`crates/haider-daemon/src/session_hub/rpc.rs:361-449`). That is the R7 hand-off law.

The account actor is the single descriptor writer and refreshes a read-only shared snapshot after mutation (`crates/haider-daemon/src/accounts.rs:415-554`). Login claims a semantic receipt, validates through the Anthropic-only validator, writes the vault, persists the descriptor, and finalizes a secret-free receipt (`crates/haider-daemon/src/accounts.rs:602-719`, `crates/haider-daemon/src/accounts.rs:784-815`). Startup reconciliation covers claimed-only, vault-only, vault-plus-descriptor, and committed-but-missing-descriptor boundaries before readiness (`crates/haider-daemon/src/accounts.rs:957-1053`, `crates/haider-daemon/src/runtime.rs:295-320`). The sentinel test scans SQLite/receipts, `accounts.json`, WAL artifacts, descriptors, and formatted frames (`crates/haider-daemond/tests/account_rpc_tests.rs:650-752`).

Production provider construction is still Anthropic-only (`crates/haider-daemon/src/accounts.rs:817-870`, `crates/haider-daemon/src/worker.rs:137-153`). `AccountsProviderFactory` reads the active descriptor directly from the snapshot, requires `Ok`, resolves it from the vault, and builds an adapter; it does **not** call `Resolver` (`crates/haider-daemon/src/accounts.rs:872-954`). Resolution happens once at the start of a logical turn and the account is stamped into usage (`crates/haider-daemon/src/worker.rs:62-84`, `crates/haider-daemon/src/worker.rs:1635-1713`).

One known debt becomes W5 work: synchronous Keychain/JSON operations still run on the async account actor task. W3c2 review P3-4 explicitly assigned that fix to the next accounts touch (`docs/briefs/W3c2-review-1-SHIP_WITH_FIXES.md:8-12`). W5 must move blocking vault/filesystem calls to the blocking pool without surrendering single-writer ordering.

### 1.4 TUI and simulator: exact existing versus designed behavior

Rust TUI already has the hard login interaction. `LoginCard` owns a zeroizing buffer, redacts `Debug`, exposes only length to rendering, consumes all keyboard/hit/wheel/hover interaction, replaces the composer band, and closes through one path that restores the draft and retires attempts (`crates/haider-tui/src/app.rs:708-832`, `crates/haider-tui/src/app.rs:1879-1894`, `crates/haider-tui/src/app.rs:2558-2644`, `crates/haider-tui/src/app.rs:3884-3901`, `crates/haider-tui/src/app.rs:4082-4089`, `crates/haider-tui/src/app.rs:4129-4134`, `crates/haider-tui/src/render.rs:2462-2493`). Stage attempts are issuance-scoped and late replies are identity-gated through `LiveDriver`/`Link` (`crates/haider-tui/src/live.rs:837-856`, `crates/haider-tui/src/link.rs:445-470`, `crates/haider-tui/src/live.rs:1413-1429`). The stays-put law says reducer, projection, animation, rendering, hit map, sticky band, and input pump remain in place; `LiveDriver` is pure/no-await and `Link` owns IO (`crates/haider-tui/src/live.rs:1-18`, `crates/haider-tui/src/link.rs:1-16`).

The command registry contains singular `/provider`, `/accounts`, `/account`, and `/login`, but not plural `/providers` (`crates/haider-tui/src/commands.rs:15-85`). Current arg slots only implement theme/login; login offers Anthropic only and calls OAuth deferred (`crates/haider-tui/src/commands.rs:149-180`). API login opens the real card, while provider/account/accounts execution and the launcher Accounts row remain honest stubs (`crates/haider-tui/src/app.rs:2988-3013`, `crates/haider-tui/src/app.rs:3204-3219`, `crates/haider-tui/src/app.rs:3911-3917`). `Screen` has no Accounts or Providers variant (`crates/haider-tui/src/app.rs:195-204`). The launcher account counts are demo constants (`crates/haider-tui/src/render.rs:585-606`, `crates/haider-tui/src/mock.rs:269-273`).

The simulator's binding account laws are explicit: a provider has several accounts; each account is API key or OAuth subscription; aliases are global; exactly one account is selected per provider; `AUTH_LABEL` renders `oauth` or `api key` (`tui.js:141-145`). Its seed records include OpenAI OAuth/API, Anthropic OAuth/API, Google API, local API plus base URL, and Hugging Face API plus base URL (`tui.js:146-154`). `/login` slots name `openai`, `anthropic`, `google`, `local`, and call OAuth “subscription — ChatGPT / Claude Max (loopback PKCE)” (`tui.js:201-213`). `/account` completes all aliases and marks the selected entry `in use` (`tui.js:215-217`).

`/accounts` opens the provider-grouped screen; `/account <alias>` performs a case-insensitive global lookup and selects only within that alias's provider (`tui.js:1765-1780`). The screen shows provider/base URL, selected dot, alias, auth label, identity, status, `in use`, click-to-select, and add buttons (`tui.js:3588-3628`). The modal copy describes a browser callback, PKCE, vault, and refresh, but Confirm only fabricates a demo account (`tui.js:3629-3681`). It has no masked key entry and no real waiting/exchanging/failure state.

Negative findings are requirements, not trivia:

- the simulator has no `/providers`, no `screen === "providers"`, and no standalone providers screen; its singular `/provider` only switches a session to a static provider model (`tui.js:172-199`, `tui.js:1720-1727`);
- it has no remove-account, set-default-model, provider configure/enable, or alias-entry action; account creation only fabricates display records on confirm (`tui.js:2158-2207`, `tui.js:3588-3688`);
- its OAuth callback is fixed `http://localhost:1455/callback`, which RFC 8252 says not to copy as a general native-app listener (`tui.js:3641-3646`);
- seed/created accounts contain only the display fields in `seedAccounts`/`confirmAuth`, and browser local storage persists those records; no raw key/token exists in that simulator model (`tui.js:146-154`, `tui.js:698-772`, `tui.js:2174-2207`);
- Hugging Face/custom buttons are present even though `/login` slots exclude them, while the method slot offers OAuth for Google/local without a real matching flow (`tui.js:203-213`, `tui.js:3621-3628`).

### 1.5 W5 delta matrix

| Surface | Exists now | W5 adds |
|---|---|---|
| Provider contract | `Provider`, typed errors/stream, `FakeProvider` | opaque continuation event and any minimal capability additions required by Responses |
| Remote adapters | Anthropic Messages | OpenAI Responses |
| Endpoint config | Anthropic test override | durable provider registry, validated base URL, models/default, API family, auth requirement, health/capability probe |
| Accounts | add/select/remove domain methods, one active/provider, global alias law | correct daemon alias mapping, actor commands, provider settings, removal cleanup |
| Secrets | API keys in Keychain/MemoryVault | versioned OAuth token bundle, zeroizing exchange/refresh |
| OAuth | enum/golden spelling only | loopback PKCE, exchange, progress, cancellation, refresh, legal/provider availability gate |
| Wire | stage/login_api/list | OAuth start/status/add/cancel, set-active, remove, set-default-model, provider list/configure, additive list fields/features |
| Resolution | active snapshot direct, next-turn pin | account service resolution, refresh, resolver callback, safe-boundary rotation |
| TUI | masked API login card; account commands stubbed | real `/accounts`, new `/providers`, dynamic slots, OAuth progress card, management actions |

### R1 — Preserve the account substrate, correct public alias semantics, and add one provider registry (DECIDED)

`CredentialDescriptor.alias` becomes the user-facing globally unique alias required by the simulator and protocol. `identity` becomes only a verified provider identity such as email, organization handle, or a neutral non-secret label. The current hashed descriptor alias is not exposed as the product alias after W5. Alias input is normalized to a bounded lowercase ASCII grammar (`[a-z0-9][a-z0-9._-]{0,63}`) and uniqueness is checked after normalization, so the simulator's case-insensitive `/account` lookup cannot become ambiguous.

Keychain namespacing moves behind the `Vault` implementation: a `KeychainVault` instance receives the profile namespace and derives its service/account slot from profile plus public alias. `MemoryVault` keeps the same logical alias behavior. This avoids leaking a physical vault key onto the protocol while preventing two profiles with alias `work` from colliding. A pre-ready, crash-reconciled migration moves current hashed slots to namespaced public aliases. It joins a legacy descriptor's physical alias to the durable login receipt/canonical login identity, uses that receipt's requested `display_alias` when valid and unique, and otherwise derives a deterministic `<provider>-legacy-<short-hash>` alias with a collision suffix. It never guesses from `CredentialDescriptor.identity`, which can be a provider label and is not an alias authority. The migration exposes a non-secret notice for later rename support. Its order is copy secret to the new slot, persist the replacement descriptor, then delete the old slot, so interruption leaves at worst an orphan secret and never loses the only credential. Current code proves the mismatch and the receipt-owned display/physical aliases at `crates/haider-daemon/src/accounts.rs:351-389` and its pinned test at `crates/haider-daemond/tests/account_rpc_tests.rs:290-312`.

A versioned `ProviderProfileV1` store becomes the one authority for:

```text
provider_id, display_name, api_family,
base_url, enabled, auth_requirement,
configured_models, default_model,
built_in/custom provenance, last_probe
```

Account descriptors do not absorb endpoint or model fields. Safe provider settings such as configured models, default model, enabled state, and latest probe apply to all accounts for that provider; the default must be a configured model. A session's already-durable provider/model remains pinned and is not silently rewritten when the picker/default changes (`crates/haider-protocol/src/session.rs:5-25`). Disabling a provider blocks new turn resolution explicitly rather than mutating session metadata. Built-in API family, origin, and auth-header requirement are immutable. For a custom provider, those three fields define provider identity and are create-only; changing one requires a new provider ID with no inherited accounts. This prevents a configuration edit from forwarding an existing key/token to a different origin. The account actor, renamed internally if useful to `ProviderAccountActor`, is the single writer for both account and provider settings and publishes one coherent snapshot revision.

The current creatable-provider set is computed and installed once at daemon startup, and `session.create` consults that static set (`crates/haider-daemon/src/runtime.rs:322-351`, `crates/haider-daemon/src/session_hub/rpc.rs:727-741`). W5 replaces that read with the actor-published provider-registry snapshot. A newly configured/enabled compatible provider becomes creatable without restart; disabling it blocks new sessions and new turn resolution while an already-open provider request may finish.

Alternatives rejected:

- keeping the current hashed descriptor alias makes `/account <alias>` search the wrong field and overloads identity;
- putting a physical `vault_ref` on `CredentialDescriptor` exposes storage implementation and complicates profile migration;
- putting default model/base URL on every credential duplicates provider truth and lets sibling accounts disagree;
- deriving `/providers` from hardcoded TUI models repeats the simulator's static catalog and cannot represent custom endpoints or health.

## 2. OpenAI adapter and provider breadth

### 2.1 API choice and canonical request mapping

The OpenAI adapter should mirror Anthropic's ownership shape: one configured endpoint/model, one resolved credential handle, one optional usage alias, a redirect-free/retry-free client, bounded open/idle deadlines, early HTTP classification, incremental SSE parsing, and an abort-owned producer. Core remains the retry owner (`crates/haider-provider/src/anthropic.rs:45-88`, `crates/haider-provider/src/anthropic.rs:168-230`, `crates/haider-core/src/actor.rs:776-820`).

W5 targets `POST {base_url}/responses`. The request mapping is:

| Haider IR | OpenAI Responses |
|---|---|
| `system_prompt` | top-level `instructions` |
| user/assistant text | `message` input items with role/content |
| assistant tool call | `function_call` item preserving `call_id`, name, arguments |
| tool result | `function_call_output` with the same `call_id` |
| `ToolDefinition` | `tools[]` entry of type `function`, name, description, JSON schema |
| attachment/image | supported `input_image` content only when capability allows; otherwise existing typed `InvalidRequest` with a capability-safe message |
| prior OpenAI opaque block | exact provider output item/encrypted reasoning continuation, accepted only when provider key is `openai` |

This matches the existing canonical `Block::{Text,Reasoning,ToolCall,ToolResult,Attachment,ProviderOpaque}` model (`crates/haider-protocol/src/provider.rs:9-36`). As Anthropic already does, replay of another provider's `ProviderOpaque` block must fail closed, not serialize unknown bytes (`crates/haider-provider/src/wire/mod.rs:86-162`).

Use `store: false` by default so Haider remains the conversation authority. Current OpenAI guidance says manually managed histories must preserve every response output item, including encrypted reasoning items when applicable. Therefore W5a adds `StreamEvent::ProviderOpaque { provider, data }`. Core appends it to the current request's `assistant_blocks`, marks `provider_event_seen`, and commits a prompt-verbatim/UI-omitted completed `TurnItem::Extension { kind: "provider_opaque", ... }`; `PromptHistoryCompiler` rehydrates only that exact bounded/provider-keyed extension into `Block::ProviderOpaque` (`crates/haider-protocol/src/item.rs:14-68`, `crates/haider-core/src/prompt_history.rs:77-134`, `crates/haider-core/src/actor.rs:827-829`). This uses the existing tolerant item escape hatch rather than adding a UI item variant. A visible reasoning summary alone is not continuation state. Opaque items are never rendered as reasoning text and are accepted for replay only by the same provider family.

### 2.2 Streaming, error, finish, and usage mapping

The Responses decoder maps typed events as follows:

| Responses stream event | Haider event/action |
|---|---|
| `response.output_text.delta` | `StreamEvent::TextDelta` |
| reasoning summary text delta | `StreamEvent::ReasoningDelta` |
| function-call output item added | `ToolCallStart { id/call_id, name }` |
| `response.function_call_arguments.delta` | `ToolCallArgsDelta` |
| function-call output item done | `ToolCallEnd`, exactly once |
| complete encrypted/opaque reasoning item | new opaque-block event, committed but not rendered |
| refusal delta/done | accumulate refusal without relabeling it as ordinary assistant text; finish `Refusal` |
| `response.completed` | final usage, then one `Finish` |
| `response.incomplete` | map `max_output_tokens` to `MaxTokens`; other provider reason to typed failure/refusal |
| `response.failed` or stream `error` | typed `ProviderError`, no fabricated finish |

Usage maps provider-reported `input_tokens`, cached input tokens, `output_tokens`, and reasoning-token detail into `Usage`; arithmetic remains overflow-checked and stamps the resolved account alias, as Anthropic does (`crates/haider-provider/src/wire/mod.rs:450-499`, `crates/haider-protocol/src/provider.rs:78-99`). Unknown additive events are ignored only after their framing is validated; unknown output item types needed for continuation are retained as OpenAI opaque items rather than dropped. One terminal event and tool start/args/end balance are fixture-pinned invariants.

HTTP mapping follows the shared taxonomy: 401 authentication, 403 permission, 429 rate limit with bounded `Retry-After`, explicitly documented quota/billing exhaustion as permission or rate-limit according to the provider code, 5xx overload/transport, malformed SSE/JSON as malformed response. Error bodies are bounded and sanitized before display; authorization headers, request bodies, response IDs containing query data, and opaque reasoning blobs never enter `Debug`/tracing. The adapter does no autonomous HTTP retry.

`CapabilityDoc` remains the adapter contract (`crates/haider-protocol/src/provider.rs:101-118`). W5a fills it from provider-family defaults plus the configured model/probe: streaming tool arguments, parallel tools, vision, thinking/reasoning, and context window. The provider registry separately reports availability/auth/API family; `CapabilityDoc` should not become a second config store. Optional additive fields may distinguish reasoning summaries and opaque continuation if the turn compiler needs to gate them.

### 2.3 Custom, local, Google, and Hugging Face scope

The base URL is a provider profile, not a command-line escape hatch. It denotes an API root; endpoint construction uses a URL library and the fixed endpoint for the selected family. Native OpenAI uses `/v1/responses`; `openai-compatible` uses `/v1/chat/completions`, the broadly implemented compatibility surface for vLLM, Ollama, LM Studio, LiteLLM, TGI, and similar gateways. Reject user-info, fragments, query strings, scheme-relative forms, traversal, and credentials embedded in the URL. Redirects remain disabled so a bearer token cannot be forwarded to another origin. Canonical origin, API family, and auth-header mode are identity-defining: built-ins cannot edit them, and a custom provider cannot edit them after creation. A different endpoint is a new provider ID and starts with no accounts. User-created profiles may choose API-key bearer or no-auth only; TUI/RPC cannot configure arbitrary OAuth authorization/token endpoints or turn a custom origin into a subscription-login issuer.

Remote custom endpoints require HTTPS by default. Plain HTTP is accepted only for an explicit local provider whose resolved host is a numeric loopback address; hostname `localhost` is normalized/displayed carefully but not trusted as proof of loopback. DNS names that currently resolve private are not enough because rebinding can move them. The probe is unauthenticated where possible and otherwise uses a resolved account without logging it. Probe results are advisory status with timestamp; they never become capability truth for an in-flight turn.

Provider scope is:

- **Anthropic**: real, API key, existing Messages adapter.
- **OpenAI**: real in W5a, API key, Responses adapter.
- **local**: real through the explicit `openai-compatible` Chat Completions family; optional bearer auth is supported, and `auth_requirement = none` does not fabricate an account. Provider construction uses `ResolvedAuth::{Credential { descriptor, secret }, None}`; `None` is legal only for a profile whose immutable auth requirement is `none`, and usage then carries no account alias. This replaces the current builder's unconditional secret/alias parameters (`crates/haider-daemon/src/accounts.rs:817-835`, `crates/haider-daemon/src/accounts.rs:903-947`).
- **custom/Hugging Face**: real through the same explicit Chat Completions compatibility family after a conservative `/v1/models` availability probe; each gets its own provider ID, endpoint, model list, and account namespace. The probe does not invent tool, vision, reasoning, or context-limit capabilities.
- **Google**: registered as unavailable/deferred with a reason. Gemini's request, streaming, reasoning, tools, auth, and errors require a native adapter and fixture corpus. A stub that accepts login but cannot turn is worse than an unavailable provider.

There is no automatic Responses-to-Chat fallback and no endpoint guessing. Native OpenAI stays on Responses so encrypted reasoning continuation, typed output items, and richer semantics survive. OpenAI-compatible endpoints form a separate, explicit Chat Completions family with their own request decoder, capability policy, fixtures, and provider identity.

### R2 — Ship two explicit OpenAI families (DECIDED)

Implement native OpenAI on `POST /v1/responses` and `openai-compatible` on `POST /v1/chat/completions`, both beside `AnthropicProvider` and both with the same transport/retry/capture ownership shape. Add opaque-continuation stream/history support to the native family for correct reasoning replay. The provider registry selects one explicit API family; it never guesses from a base URL and never silently falls back between the two.

Native OpenAI and validated Chat Completions-compatible local/custom endpoints are real W5 providers. Google is visible but unavailable; it is not a no-op implementation. `FakeProvider` remains the daemon/core release authority. Adapter tests use request snapshots, arbitrary SSE chunking, unknown events, tool interleaving, reasoning continuation, refusal, usage, typed errors, timeout/cancel, and redacted formatting. Recorded fixtures require provenance manifests and secret scrubbing, matching the Anthropic capture discipline (`crates/haider-provider/tests/anthropic_provider_tests.rs:54-119`, `crates/haider-provider/tests/anthropic_provider_tests.rs:287-370`).

Alternatives rejected: moving native OpenAI to Chat Completions, automatic Responses-to-Chat fallback, hardcoded custom endpoint strings, accepting redirects, and advertising Google/local success without a compatible transport.

## 3. OAuth authorization-code/PKCE, token vault, and refresh

### 3.1 Product truth and provider authorization

The simulator's “ChatGPT / Claude Max (loopback PKCE)” label is desired product vocabulary, not evidence that Haider is entitled to those grants (`tui.js:208-213`). The generic engine can be fully implemented and release-tested without a live subscription provider. A live provider configuration requires all of:

1. a client registration issued or approved for Haider;
2. documented authorization/token endpoints and scopes;
3. a redirect policy compatible with native loopback clients;
4. explicit permission to use the resulting token for the provider inference surface;
5. refresh rotation/sender-constraining semantics Haider can honor.

The OpenAI Codex client ID, issuer assumptions, fixed ports, and scopes are not reusable registration. “Sign in with ChatGPT” identity is not inference authorization. Anthropic's current published policy positively forbids the third-party Claude Max behavior. W5 can show an unavailable OAuth method with a precise reason; it must not show a button that starts an unapproved flow.

### 3.2 Flow ownership and wire-to-browser sequence

OAuth belongs to a daemon-owned `OAuthCoordinator` under the account actor's authority. `oauth_start/status/cancel` and OAuth `account.add` require Control authority **and** authenticated same-UID local UDS, matching the existing secret-surface gate (`crates/haider-daemon/src/session_hub/rpc.rs:266-298`). A remote caller cannot reach the daemon's loopback listener and is not allowed to allocate one. Each unclaimed flow, ready reference, and transient URL is bound to daemon instance plus initiating connection nonce; disconnect cancels and wipes those unclaimed objects.

The connection loop performs only those authorization checks and a bounded hand-off. It never binds a listener, opens a browser, waits for a callback, exchanges a code, calls Keychain, or refreshes a token inline. The coordinator owns a bounded map of `OAuthFlowId -> FlowState`, cancellation tokens, deadlines, listener tasks, and zeroizing secrets. The start response carries the full URL in a concrete `OAuthAuthorizationWire` type with the same redacted `Debug`, zeroizing drop, and codec/sentinel tests as `SecretWire`; it is not an ordinary `String`.

The recommended sequence is:

```text
TUI /login provider oauth
  -> account.oauth_start { provider, desired_alias, attempt_id }
daemon:
  validate provider registration/method/alias
  bind 127.0.0.1:0 and retain the listener
  mint flow_id, state, verifier, challenge, OIDC nonce, random callback path
  -> authorization_url + safe display origin + expires_at
TUI Link opens browser; card retains flow_id/attempt, not tokens
browser -> exact loopback callback
daemon validates method/Host/path/state, consumes code once
daemon exchanges code using verifier
daemon validates token response and identity
  -> account.oauth_status = ready { opaque_flow_ref, identity, expiry }
TUI -> account.add { command_id, method: oauth, alias, opaque_flow_ref }
RPC router:
  atomically claim flow exactly once; move bundle into owned actor job
account actor job:
  claim durable command receipt
  vault.put(versioned token bundle)
  AccountStore.add(descriptor AuthMethod::OAuth)
  finalize durable receipt; publish new snapshot
```

Splitting non-durable `oauth_start/status` from durable `account.add` deliberately mirrors `vault.stage` plus `account.login_api`. It keeps access/refresh tokens daemon-local, makes the final mutation idempotent, and avoids a durable receipt remaining “pending” for minutes while a browser is open. A flow reference is random, daemon-instance-scoped, single-use, TTL-bounded, formatting-redacted, and invalid after restart. Restarting browser authorization is acceptable because no account mutation has occurred.

OAuth `account.add` routing must mirror the strongest existing API-key hand-off: before `try_send`, it atomically claims/removes the ready reference from the coordinator and moves the owned zeroizing token bundle into the actor job (`crates/haider-daemon/src/session_hub/rpc.rs:390-430`). After successful hand-off the command, not the connection, owns the bundle; disconnect drops only the response route and durable receipt replay recovers the result. A full mailbox returns the job to the router, which may restore the still-live same-connection ready reference; a closed actor wipes it. Disconnect cleanup touches only unclaimed flows/bundles. Alias/revision rejection happens before consuming the reference where possible; a late actor-detected conflict returns the owned bundle to a fresh same-connection ready reference or wipes it if that connection is gone.

`oauth_cancel { flow_id, attempt_id }` is idempotent and closes the listener/exchange task. Status returns only `waiting_browser`, `exchanging`, `ready`, `failed { public_code }`, `expired`, or `cancelled`. It never returns authorization code, state, verifier, nonce, access token, refresh token, raw token error, or a full authorization URL after the start response. The authorization URL is transient `OAuthAuthorizationWire` data because its state/query are sensitive; rendering shows only the provider origin and loopback port.

### 3.3 Loopback listener security

Listener rules are non-negotiable:

- bind the socket **before** forming the redirect URI, to numeric `127.0.0.1` on port `0`; optionally attempt `[::1]:0` as a separately exact redirect, but never bind `0.0.0.0`, `[::]`, a LAN address, or a user-selected interface;
- do not use `localhost`; RFC 8252 recommends an IP literal to avoid hostname resolution/interface surprises;
- use an unpredictable callback path as defense in depth, an independently random state value of at least 256 bits, and an independent OIDC nonce when requesting/using an ID token;
- use a cryptographically random PKCE verifier meeting RFC 7636 length rules and S256 only; never allow `plain` downgrade;
- require one `GET`, exact Host including assigned port, exact path, and constant-time state equality; accept exactly one of `code` or a standard OAuth `error` value, never both. A state-valid `access_denied`/provider error is a terminal sanitized denial; missing/wrong state remains interference. Reject fragments, duplicate critical parameters, oversized query/header/body, non-ASCII ambiguity, and all other methods;
- apply accept/read/total deadlines and a small connection cap; the first invalid request does not consume the valid callback, but repeated invalid requests are bounded so a local process can at most force a visible restart;
- return a tiny static page with `Cache-Control: no-store`, `Pragma: no-cache`, `Referrer-Policy: no-referrer`, no scripts/images/external resources, and no code/state in the page or URL rewrite;
- close the listener immediately after a valid single-use callback or cancellation.

A different local process can connect to a loopback port. It cannot be authenticated by source address. State prevents login CSRF/flow mix-up; PKCE prevents a stolen authorization code from being redeemed without the verifier; the random path reduces blind interference. A local attacker can still deny service, so the correct outcome is a bounded “callback interfered with; retry login,” never a relaxed validation fallback.

The provider registration must allow the exact path and variable loopback port. If it only permits another application's fixed port, Haider does not bind that port or borrow that registration.

### 3.4 Token exchange, bundle, identity, and storage

Authorization and token endpoints come from release-owned/provider-approved metadata. They are HTTPS, exact-origin allowlisted, redirect-free, and not accepted from an account or authorization response. A public native client has no embedded client secret. Exchange body, authorization code, verifier, and response bytes remain `Zeroizing`; tracing records only flow ID, provider, phase, duration, and public error class.

The vault stores one opaque versioned payload under the account alias:

```text
OAuthTokenBundleV1:
  provider_id
  issuer
  token_type
  access_token
  refresh_token?
  expires_at_unix_ms
  refresh_expires_at_unix_ms?
  granted_scopes
  subject_hash / verified display identity metadata
  generation
```

The serialized buffer is created directly inside a zeroizing byte vector and passed to `Vault::put`; no intermediate `String`, pretty JSON, receipt, descriptor, event, or file contains it. W5b should also change `SecretHandle`'s private backing from plain boxed bytes plus best-effort `fill(0)` to an equivalent `Zeroizing<Box<[u8]>>`/zeroizing owned buffer; its public non-cloneable/redacted API stays the same (`crates/haider-accounts/src/vault.rs:19-61`). Access and refresh tokens are both secrets. ID tokens are also secrets and need not be retained after verified claims are extracted. An identity is populated only from a signature/issuer/audience/nonce-validated ID token or authenticated user-info response. Decoding an unsigned JWT payload for display is not verification.

Current Keychain is the only production vault. On unsupported platforms OAuth is unavailable for the same reason API-key durable login is unavailable (`crates/haider-accounts/src/keychain.rs:177-202`). W5 does not add plaintext `0600` token JSON as a convenience fallback; that would violate the existing “secret never persisted in cleartext” product law.

### 3.5 Resolve-time refresh and race control

OAuth resolution cannot remain the direct snapshot-plus-vault read in `AccountsProviderFactory`. A credential broker command resolves by provider/alias and:

1. reads the descriptor/snapshot revision;
2. resolves and parses the bundle on the blocking pool;
3. if expiry is beyond a safety skew, returns an ephemeral `SecretHandle` for the access token;
4. if refresh is required, joins or starts exactly one in-flight refresh for `(provider, alias, generation)`;
5. exchanges the refresh token without holding the account snapshot/write lock;
6. persists the returned access token and any rotated refresh token **before** making the new access token available;
7. applies the result only if descriptor identity and token generation still match.

Concurrent turns share the one refresh future. A compare-by-generation fence prevents a slow response overwriting a newer rotated refresh token. Account removal/cancel increments or tombstones the generation so a late refresh cannot recreate deleted credentials. Keychain calls run on `spawn_blocking`; the account actor serializes completion application, not the network wait.

Refresh parsing requires the expected bearer token type, bounded positive expiry, and all provider-required scopes. If the response contains a new refresh token it atomically replaces the old one; if an approved provider explicitly permits omission, the existing refresh token is retained rather than accidentally erased. Scope loss, nonsensical expiry, or an unexpected issuer/resource is a terminal refresh failure.

If a provider rotates refresh tokens and the server succeeds but Keychain persistence fails, the new access token is not used and the credential is durably marked existing status `Expired`. Retrying the old refresh token can trigger replay detection and is forbidden. Crash after server rotation but before durable vault write is an unavoidable public-client failure boundary; the safe recovery is re-login, not guessing which token is valid.

Failure policy:

- `invalid_grant`, revoked consent, issuer/audience mismatch, missing required scopes, or expired refresh token marks the descriptor `Expired` or `Revoked`, then offers the resolver's checked same-provider/single-hop alternate seam with `RotationCause::Error`;
- transient token endpoint failure leaves a still-valid access token usable; if it is already expired, resolution returns retryable unavailable/limited and may rotate to another usable account;
- a provider 401 before any response event permits one forced refresh for OAuth, then one fresh request; repeated 401 marks the credential expired and enters rotation;
- no refresh or rotation occurs after partial provider output.

### R3 — Build generic, standards-conformant OAuth; enable only sanctioned provider grants (DECIDED)

W5b ships a complete authorization-code/PKCE loopback engine, fake-provider registration, token vault, refresh, and management UI state machine. It does not ship a copied OpenAI Codex client, a guessed ChatGPT scope, or Claude Max third-party login. Live subscription methods become enabled through release-owned immutable provider metadata only after legal and technical approval.

Device authorization may be added later for headless/remote clients if a provider documents and registers it. It is not a fallback that weakens loopback checks. Manual token paste remains API/bearer credential entry, not an OAuth flow.

Alternatives rejected: fixed port 1455, hostname binding, `0.0.0.0`, state-only without PKCE, PKCE without state, accepting provider metadata from the callback, browser code copy/paste, plaintext token files, logging the authorization URL, and silently treating a first-party CLI's grant as reusable.

### R4 — Refresh through a single-flight credential broker and fail closed at rotation boundaries (DECIDED)

Move resolve/refresh behind the daemon account service. Never hold the actor or store lock across HTTP, and never let two refreshes race. Persist a rotated bundle before returning its access token; generation-fence late completion; convert permanent refresh failure into durable account status and a checked extension of the resolver seam. The current resolver invokes policy only for `Limited`; W5 introduces `RotationTrigger::{RateLimit { until_ms }, AuthExpired, RefreshFailed}` and factors its same-provider/usable-target/single-hop checks into one alternate-resolution helper. Policy is invoked exactly once and protocol output maps the latter two triggers to `RotationCause::Error`. It must not lie by storing an arbitrary fake rate-limit deadline (`crates/haider-accounts/src/resolver.rs:80-137`, `crates/haider-protocol/src/credential.rs:40-55`). This closes stale-token and token-resurrection races while preserving R7.

## 4. Account/provider management wire and daemon ownership

### 4.1 Additive wire vocabulary

Wire v1 already tolerates unknown frame kinds, request methods, and object fields (`crates/haider-rpc/src/frame.rs:13-19`, `crates/haider-rpc/src/frame.rs:380-492`). W5 adds variants and fields; it does not rename `account.login_api`, retype existing `account.list.descriptors`, or make an old field mandatory.

Advertise method-level feature strings in `Welcome.features`:

```text
account_management_v1
account_oauth_pkce_v1
provider_management_v1
account_rotation_v1
```

The existing `account_login_api_v1` and `vault_stage_v1` remain (`crates/haider-rpc/src/frame.rs:127-134`, `crates/haider-daemon/src/connection.rs:1317-1334`). Clients hide/disable only the methods whose feature is absent.

Recommended requests:

| Method | Class | Required coordinates | Result |
|---|---|---|---|
| `account.oauth_start` | non-durable staged flow | provider, desired alias, attempt ID | flow ID, transient authorization URL, safe display origin, expiry |
| `account.oauth_status` | read | flow ID, attempt ID | phase, verified identity, opaque ready ref or public error |
| `account.oauth_cancel` | idempotent transient | flow ID, attempt ID | cancelled/already terminal |
| `account.add` | durable mutation, OAuth v1 | command ID, provider, alias, method, opaque OAuth ref | descriptor |
| `account.set_active` | durable mutation | command ID, global alias | selected descriptor, prior alias, revision |
| `account.remove` | durable mutation | command ID, global alias, optional expected revision | removed alias, replacement active alias, revision |
| `account.set_default_model` | durable provider mutation | command ID, provider, model, expected revision | provider summary |
| `provider.list` | read | optional provider | provider summaries and revision |
| `provider.configure` | durable create/safe-update | command ID, provider ID, create-only API family/origin/auth requirement, mutable enabled/models/default, expected revision | provider summary |

`account.login_api` remains the compatibility path and uses the same corrected alias semantics. New clients always send the card alias. For an old client that omits the currently optional alias, the actor derives a stable canonical `<provider>-api-<short-command-hash>` alias, records that choice in the pending receipt identity, and replays it; it does not reuse `identity` or choose from a changing snapshot. A future version may unify API-key add under `account.add`, but W5 does not make an existing client construct a new union.

`account.list` keeps `descriptors` and adds optional, defaultable:

```text
revision
provider_active: [{ provider, alias }]
provider_defaults: [{ provider, model }]
```

The descriptor already carries `active`; `provider_active` makes grouping and invariant checking cheap without changing old clients. Provider endpoint, model inventory, auth methods, availability, and probe status live in `provider.list`, not duplicated in each descriptor.

`AuthMethod` and `CredentialStatus` are currently closed Serde enums with no `Unknown` arm (`crates/haider-protocol/src/credential.rs:19-38`). W5 freezes their emitted v1 variants at API-key/OAuth and Ok/Limited/Expired/Revoked; it does not add a value that would make an older client fail the whole descriptor. New provider-availability/API-family enums include a `#[serde(other)] Unknown` artifact from their first release. Future account method/status expansion needs an additive tolerant account-view field or feature, never silent coercion to API key/healthy.

### 4.2 Actor commands, receipts, and crash order

`AccountCommand` currently has only `Login` and `Shutdown`; `AccountsFacade` exposes only the login sender, snapshot, and vault flag (`crates/haider-daemon/src/accounts.rs:393-431`). Add owned mailbox variants for OAuth finalize/add, set-active, remove, set-default-model, provider-configure, resolve-for-turn, refresh completion, mark-limited, and shutdown. Short reads may use an atomically published snapshot; every mutation and refresh completion goes through the single writer.

R7 remains exact. RPC handlers authenticate and parse, construct an owned job, and `try_send` to the bounded actor. A correlated response is delivered later through the existing sink. No connection task awaits Keychain, JSON save, endpoint probe, token exchange, or provider validation. Mailbox full/closed is a typed retryable/unavailable response, not an inline fallback (`crates/haider-daemon/src/session_hub/rpc.rs:361-449`).

Every durable mutation has a semantic command identity and digest excluding ephemeral stage/flow references and all secret material. Receipt payloads contain only public descriptor/provider result data, as existing login receipts do (`crates/haider-store/src/event_store.rs:206-245`, `crates/haider-store/src/event_store.rs:892-1032`).

Crash-safe mutation rules:

- **set active:** claim receipt, persist `AccountStore::select`, publish snapshot, finalize. Recovery derives the selected alias from durable descriptor state.
- **set default/provider configure:** claim, validate against registered API family/model invariants, reject any built-in or existing-custom identity-field change, persist provider store, publish coherent combined revision, finalize.
- **OAuth add:** claim ready flow exactly once, vault-put bundle, add descriptor, finalize. Reconciliation matches existing login's vault/descriptor cases, but never serializes a token into the receipt.
- **remove:** claim and durably reserve/tombstone the alias, remove the descriptor, publish no-resolve state, delete the vault slot, then finalize and release the alias reservation. A crash may leave an orphan secret but never an active descriptor pointing at a deleted secret. Pending remove receipts rebuild the reserved-alias set before Ready; reconciliation retries deletion, and add rejects a reserved alias until cleanup finalizes. If removing the active member of a non-empty provider, the current store deterministically selects a remaining entry; the response names that successor (`crates/haider-accounts/src/store.rs:238-259`).

An account cannot be removed while a turn merely holds an ephemeral resolved handle; removal prevents new resolution, while the already-pinned turn may finish. A refresh completion for a removed generation is discarded and its buffer scrubbed.

Provider revision and account revision should be published together as one monotonically increasing management snapshot revision even if their files remain separately atomic. The receipt database owns that counter: after the external account/provider/vault mutation succeeds, receipt finalization and the next revision commit in one SQLite transaction; the actor publishes only afterward. Pre-ready receipt reconciliation advances a missing final revision exactly once. Receipt replay is checked before expected-revision comparison, so a previously committed command remains idempotent even after later revisions. A genuinely new mutation supplies an optional expected revision; mismatch returns new stable RPC code `revision_conflict` with `retryable: true` and bounded structured details `{ expected_revision, current_revision }`, never secrets. The constant and response body are golden-pinned beside the existing stable RPC codes (`crates/haider-rpc/src/frame.rs:70-125`).

### 4.3 Resolver integration and provider factory

`AccountsProviderFactory::resolve_for_turn` becomes a client of the credential broker rather than reading `Arc<Vec<CredentialDescriptor>>` and vault itself. Provider creation also consults the same live registry revision rather than the static startup whitelist. The broker:

- chooses the active alias from `AccountStore`;
- refreshes OAuth if needed;
- invokes today's `Resolver` for a limited active credential and the R4 checked alternate extension for an expired/revoked refresh result;
- applies an approved same-provider alternate through the actor;
- returns a resolved secret, provider profile, chosen alias, snapshot revision, and auth method;
- builds the adapter only after profile/account validation.

Factory resolution currently happens before the harness exists (`crates/haider-daemon/src/worker.rs:1635-1675`, `crates/haider-daemon/src/worker.rs:1725-1729`). Therefore if initial resolution rotates a limited/expired A to B, `ResolvedTurnProvider` also carries `initial_rotation: Option<RotationEvent>` and `rotation_budget_consumed`. The new harness must commit that event before its first call to B and mark the turn's one rotation allowance consumed; commit failure prevents the provider call. This makes initial A→B visible and forbids a later B→C in the same turn.

Initial turn resolution still pins provider family, API family, endpoint, and model, and a manually selected account remains fixed for that logical turn (`crates/haider-daemon/src/worker.rs:62-84`). Only R8's automatic, pre-first-event attempt resolver may replace the account/provider instance once inside the turn. A manual switch affects the next logical turn and never rewrites committed usage. `account.set_active` validates the alias server-side and derives its provider; a client-supplied provider is not trusted.

### R5 — Add management methods under wire v1 with explicit feature negotiation (DECIDED)

Use the methods and features above, preserve all old request/response shapes, and extend `account.list` only with optional fields. OAuth start/status is transient staging; only `account.add` is the durable mutation. Provider configuration has its own read/write methods even though `/accounts` consumes a joined view.

Alternatives rejected: replacing `account.login_api`, putting secrets or OAuth URLs in receipts, making OAuth browser wait one long RPC response, changing wire version for additive methods, and encoding provider settings on credentials.

### R6 — Keep one account/provider actor, durable receipts, and R7 hand-off (DECIDED)

All mutations, OAuth finalization, status changes, and refresh completion go through the actor. Blocking Keychain/file work runs on the blocking pool and network work runs in owned jobs; completion is generation-fenced before the actor applies it. `AccountStore::select/remove` are never called from RPC handlers. Remove chooses descriptor-first safety and pre-ready orphan cleanup.

## 5. `/accounts`, `/providers`, login, and management TUI

### 5.1 `/accounts`: 1:1 simulator core backed by daemon truth

Add `Screen::Accounts` and route `/accounts` plus the launcher Accounts row to it. Its visual hierarchy is the simulator's:

```text
provider group / optional base URL
  selected dot  alias  AUTH_LABEL  identity  status  "in use"
  sibling accounts...
one global add row after all groups:
  OpenAI OAuth/API · Anthropic OAuth/API · HF · Custom
```

The exact source design is `tui.js:3588-3688`; selection behavior is `tui.js:2158-2168`. Rows come from `account.list`, never seed constants or browser-style local storage. Provider grouping comes from `provider.list`. The global add row stays in the simulator's exact post-groups position; dynamically disabling an action with a registry/legal reason is a W5 extension and gets separate snapshots. Protocol `CredentialStatus::Ok` renders the simulator's literal `active`, while selection separately renders `in use`; `Limited { until }`, `Expired`, and `Revoked` are additive W5 status vocabulary with their own snapshots (`crates/haider-protocol/src/credential.rs:28-38`). The selected row is exactly the descriptor selected by the daemon. The static launcher counts and identity line become projections of the same management snapshot, not a second authority (`crates/haider-tui/src/render.rs:585-606`, `crates/haider-tui/src/app.rs:1255-1274`).

Keyboard/mouse actions:

- click a non-selected usable row -> `account.set_active`, exactly as the simulator;
- keyboard highlight plus Enter performs the same action as a separately goldened W5 accessibility extension;
- `/account <alias>` -> same request after dynamic global-alias completion;
- provider add button -> `/login` slot/card flow;
- remove/re-login/status affordances are separately goldened W5 management extensions; diagnostics never show a raw provider body;
- with an auth card open, Esc cancels it; otherwise Accounts returns to the attached Session or to Launcher when no session is attached, matching `tui.js:2516-2520`.

Optimistic selection is forbidden. The dot moves only after a correlated daemon result or a newer snapshot revision. Late results are revision- and command-ID-gated.

### 5.2 `/providers`: owner-directed new screen, not claimed simulator parity

Add plural `/providers` to the registry/help and a `Screen::Providers`. Because the simulator has no such command/screen, the following is a W5 design derived from its provider-group styling, not a 1:1 port:

```text
provider  availability/health
  API family / base URL (safe display)
  configured models; default marked
  active account alias + auth label/status
  actions: set default, configure custom/local, open accounts, login
```

Built-ins appear even when unavailable so Google can say “adapter not installed” and subscription OAuth can say “provider registration unavailable.” Custom provider rows are created/edited through `provider.configure`. Editing requires an explicit form/card and expected revision; a raw base URL is never interpolated into shell/browser commands. Remote HTTP and redirecting endpoints fail validation with an actionable reason.

Before W5d implementation, this new screen needs one owner-approved static screenshot/golden because the simulator supplies no exact layout law. That approval is a design gate, not a reason to invent a second reducer/render architecture. It uses the existing screen, band, hit-map, and live envelope patterns.

### 5.3 Dynamic argument slots

Replace hardcoded login candidates with daemon data:

- `/login` provider candidates are enabled/configurable provider summaries;
- the method slot contains only methods legal and configured for that provider, with unavailable methods visible but disabled when useful;
- `/account` candidates are the live globally unique aliases and mark `in use`;
- `/provider` candidates come from registry providers and configured defaults;
- `/model` candidates come from the active provider's configured models.

This preserves the simulator slot semantics (`tui.js:201-228`) while correcting its inconsistent Google/local OAuth and Hugging Face/custom coverage. Slot replies carry snapshot revision; stale selection revalidates server-side.

`/login` remains the simulator's two advertised slots: provider, then method. Both API and OAuth cards open with a visible editable alias field prefilled from provider/method plus the smallest free numeric suffix in the current management snapshot (for example `openai-api`, then `openai-api-2`). The daemon canonicalizes and rechecks R1's alias grammar/uniqueness at commit, so concurrent clients cannot win the same alias. A `revision_conflict`/alias collision keeps the card open, refreshes the snapshot, and proposes the next suffix. API submit still wipes the local key at issuance and therefore requires re-entry/restaging after a late conflict; it never retains the key for convenience. OAuth validation rejects the collision before consuming the ready reference, so the user can edit the alias and retry with a fresh command ID. The current Rust command's optional alias token may prefill that field for compatibility, but it is not a third required slot (`crates/haider-tui/src/app.rs:2991-3003`, `crates/haider-tui/src/app.rs:722-726`). Alias editing is a W5 extension because the simulator auto-generates aliases (`tui.js:2196-2205`).

### 5.4 API-key and OAuth cards

The API-key path reuses `LoginCard` without weakening TUI6: total modality, zeroizing buffer, masked length-only renderer, fresh stage issuance on every submit, deadline expiration before reply application, attempt identity through `LiveDriver`/`Link`, and one retirement/close path (`crates/haider-tui/src/app.rs:2558-2644`, `crates/haider-tui/src/live.rs:837-856`, `crates/haider-tui/src/runtime.rs:2139-2187`).

The OAuth card is also total-modal and occupies the same composer band. Its states are:

```text
Starting
WaitingForBrowser { provider_origin, loopback_port }
Exchanging
ReadyToCommit
Committing
Failed(public code/message)
Expired / Cancelled
```

It stores provider, desired alias, flow ID, attempt identity, deadline, and safe display data. It does not store tokens, code, verifier, state, or a full authorization URL in `App`, session state, projection, demo store, clipboard history, or snapshots. `Link` transiently retains the start URL in a zeroizing flow/attempt-keyed shell object, automatically asks a direct platform browser API to consume it, and deletes it on callback/cancel/expiry; the URL is never interpolated into a shell command or logged as a process argument. If automatic open fails, `o` reopens and explicit `y` copies through `Link` with a one-time-link warning; `Debug` is redacted and retry remints an attempt/flow.

Those actions require modal-owned card hits, not the ordinary hit map that today's login card swallows. `OAuthOpen`/`OAuthCopy` hits carry `{ flow_id, attempt_id, action }`, are matched and revalidated against the open card **before** the modal's underlying-hit swallow, and enqueue only a semantic shell request. Every non-card hit remains swallowed. A stale card hit is dropped. Closing, switching surfaces, link loss, or deadline retires the attempt, deletes the URL object, and sends best-effort cancel. A late callback/status/commit reply cannot reopen the card or select an account unless flow ID, attempt ID, and open card all match (`crates/haider-tui/src/app.rs:3884-3901`).

`LiveDriver` gains semantic commands/replies for list/configure/switch/remove/OAuth; it remains pure and awaits nothing (`crates/haider-tui/src/live.rs:10-18`, `crates/haider-tui/src/live.rs:61-260`). `Link` alone constructs wire requests and correlates replies (`crates/haider-tui/src/link.rs:424-677`). Reducers and renderers never call RPC.

### R7 — Port the `/accounts` binding core exactly; separately golden W5 extensions and `/providers` (DECIDED)

The account grouping, row fields/order, `active` status, selected indicator, global add-row placement, click-to-switch, and screen Esc destination follow the simulator. The simulator supplies the auth copy/action intent, but its inline `MenuBox` placement and 1/2 controls do not supersede the shipped TUI6 interaction. Composer-band placement, masked API entry, Enter/Esc controls, total modality, keyboard row selection, dynamic unavailable reasons, provider screen, removal confirmation, default-model action, alias entry, and OAuth progress stages follow binding TUI6 prior art or are new W5 designs and require their own snapshot/golden acceptance (`tui.js:2494-2520`, `tui.js:3629-3681`, `crates/haider-tui/src/app.rs:2536-2584`, `crates/haider-tui/src/render.rs:2462-2493`). Both screens reuse the stays-put reducer/render layers and LiveDriver/Link envelope model. Both login methods are total-modal and attempt-identity fenced.

Alternatives rejected: local optimistic state as authority, persisting display accounts in TUI storage, putting RPC in reducers/render, a non-modal browser flow, rendering the authorization query, reusing a cancelled attempt, and pretending the simulator already designed `/providers`.

## 6. Live rotation on real turns

### 6.1 Current retry and pin law

Core owns at most three attempts for a provider request and retries only retryable errors before a provider event (`crates/haider-core/src/actor.rs:59-67`, `crates/haider-core/src/actor.rs:776-820`, `crates/haider-core/src/actor.rs:888-951`). Today every retry uses the same provider/account because the factory resolves once for the logical turn, `HarnessActor` owns one immutable `Arc<dyn Provider>`, and one configured usage alias overwrites every provider usage update (`crates/haider-daemon/src/worker.rs:62-84`, `crates/haider-core/src/actor.rs:474-505`, `crates/haider-core/src/actor.rs:994-999`). Core also rejects cumulative usage changing account within that logical turn (`crates/haider-core/src/actor.rs:2355-2373`). TUI projection currently ignores `EventPayload::Rotation` (`crates/haider-tui/src/projection.rs:379`).

W5 deliberately narrows the W3c pin law for **automatic** rotation: provider family, API family, endpoint, and model remain pinned for the logical turn, while the concrete account/provider instance is pinned per provider-request attempt. Manual account switches remain next-**logical-turn** only. This exception requires a real core seam; the global account actor cannot be called directly from provider-neutral core.

Add an injectable `ProviderAttemptResolver` port to the harness. The daemon implementation owns the credential-broker hand-off and can return `Rotate { provider, account, rotation }`, `Wait`, or `Stop`; the core fake is deterministic. Core invokes it only for an eligible pre-first-event failure, at most once per logical turn, replaces its local provider/account for the retry and remaining tool-loop requests, and stays within the existing total request-attempt budget. The session harness then commits `EventPayload::Rotation` through its `StoreHandle`; the global account actor persists status/selection but never authors a session transcript envelope (`crates/haider-protocol/src/lib.rs:83-86`).

The same budget covers initial factory resolution. `ResolvedTurnProvider.initial_rotation` is committed by the harness before the first provider call and starts the turn with the allowance consumed. An initially clean resolution starts with the allowance available to `ProviderAttemptResolver`. There is no path that rotates once before harness creation and again inside core.

The safe policy distinguishes a **provider request attempt** from a **logical turn**:

1. resolve/pin account A for the request;
2. on 429 before any stream event, record A as limited using bounded `Retry-After`/provider policy;
3. core asks its injected attempt resolver; the daemon broker invokes the typed R4 resolver policy for one same-provider alternate B;
4. account actor durably marks A/selects B and returns a newly built provider plus `RotationEvent { from: A, to: B, cause }`;
5. session core commits that rotation envelope; if commit fails it does not call B;
6. core retries the identical provider request once under B, within the existing total attempt budget;
7. B stays pinned for subsequent tool-loop requests in that logical turn; no second automatic account rotation occurs in the same logical turn.

The current single-account `Usage.account` and core invariant cannot truthfully represent A then B in one logical turn. The decided additive protocol change is `Usage.accounts: Vec<AccountUsage>` with per-account input/output/reasoning/cached/source subtotals, while retaining top-level totals and legacy `account` only when exactly one account contributed. Old clients ignore the additive field and still render totals; new clients can attribute rotation. Do not simply overwrite A with B.

Only these failures trigger automatic rotation:

- HTTP/provider 429 with a credible bounded retry/reset;
- permanent OAuth refresh failure/expired token before request open;
- authentication 401 after one OAuth forced-refresh attempt, before any event, when another usable account exists.

Generic transport/5xx overload is provider-wide or network-wide and retries the same account according to core policy; rotating credentials would hide the real failure. Permission 403, malformed responses, unsupported models, and local endpoint errors do not rotate.

After **any** text, reasoning, tool-call, usage, opaque output, or other provider event, account rotation cannot continue that request safely. Haider commits a public note/wait/error, marks the credential status when warranted, and terminalizes or offers retry. Replaying on another account could duplicate text or tool effects. No alternate is auto-selected on that failed request; the next logical turn resolves current durable status/selection afresh.

### 6.2 User-visible signal and manual switching

On successful automatic rotation, the session harness commits the existing protocol `RotationEvent` with from/to aliases and cause after the account actor's durable selection and before calling the alternate (`crates/haider-protocol/src/credential.rs:40-55`). TUI stops ignoring it and renders a concise durable note:

```text
anthropic: work-max is limited; continued with backup-api
```

It also updates `/accounts`, launcher identity, and status from the newer management snapshot. Token/reset details are public bounded metadata; no provider response body appears.

If no alternate is usable, core uses `RunState::Waiting { RateLimit/ProviderBackoff }` where a credible reset exists or a terminal typed error otherwise (`crates/haider-protocol/src/state.rs:52-110`). It does not spin through every account: `Resolver` remains one-hop per failure. A later scheduler/user retry may resolve again after status changes.

Manual `account.set_active` during a streaming request changes account-store selection immediately but does not swap the current turn's provider, including its later tool-loop requests. TUI says “active for next turn” while a turn is running. Only the automatic, core-gated pre-first-event exception above can change accounts within a logical turn.

### R8 — Rotate once only at a pre-first-event request boundary and report it durably (DECIDED)

Add the injected attempt-resolver port, extend the resolver with R4's typed trigger while preserving its one-call/same-provider/usable/single-hop checks, and connect both to core's pre-first-event retry boundary. A 429 may mark/rotate/rebuild/retry once per logical turn before output; partial output always surfaces honestly. Extend usage attribution before allowing two accounts in a logical turn. The session harness emits `RotationEvent`; TUI consumes it.

Alternatives rejected: swapping an adapter mid-stream, retrying after a tool delta/effect, rotating on generic transport/5xx, silently changing usage alias, looping over all accounts, and treating a manual switch as permission to mutate an in-flight request.

## 7. Chunk plan, deterministic gates, mutation law, and live probes

### R9 — Land W5 in four dependency-ordered chunks (DECIDED)

The dependency graph is alias/provider schema -> adapter/config, OAuth primitives -> management mutation, stable wire -> TUI, account broker -> rotation. Use these four chunks.

### 7.1 W5a — OpenAI Responses adapter and provider registry/custom endpoints

Scope:

- implement provider store/profile schema plus profile-namespaced vault/alias codecs and migration primitives; daemon pre-ready execution lands in W5c;
- add `OpenAiResponsesProvider`, request compiler, SSE decoder, opaque reasoning continuation, error/usage/finish mapping;
- add OpenAI builder/validator and registry-driven factory;
- validate base URLs and implement Chat Completions-compatible local/custom profiles;
- register Google as unavailable, never stub-success.

Deterministic tests:

- fixture requests and arbitrary SSE chunk boundaries;
- interleaved text/reasoning/tool args, opaque items, refusals, unknown events, final usage;
- 401/403/429/5xx, bounded error bodies, `Retry-After`, timeout, cancellation, producer drop;
- endpoint URL parsing, redirect refusal, loopback HTTP exception, origin/auth-header confinement;
- capability/probe snapshots;
- `FakeProvider` regression suite remains unchanged and authoritative.

Secret sweep: OpenAI API key sentinels in request/debug/error/capture/golden/temporary files, plus existing Anthropic sentinel. Mutation checks must state the failure they catch: e.g. enabling redirects fails the redirect-origin test; dropping opaque reasoning fails the second-request fixture; mapping 429 as transport fails the taxonomy test.

Live probe: ignored/manual only, requiring explicit environment and model. It makes one minimal Responses request through the real adapter, asserts at least one text/finish/usage sequence, and prints no body or credential. A configured local endpoint probe is also ignored. Neither gates release.

### 7.2 W5b — OAuth loopback/PKCE, token bundle, refresh, and fake authorization server

Scope:

- generic provider-approved OAuth metadata and availability;
- same-UID local transport/capability gate and concrete zeroizing `OAuthAuthorizationWire`;
- bounded flow coordinator, numeric ephemeral loopback listener, PKCE/state/nonce/path validation, cancel/expiry;
- token exchange and verified identity seam;
- `OAuthTokenBundleV1`, Keychain/MemoryVault storage, single-flight refresh/generation fence;
- public progress/error model;
- no live ChatGPT/Claude Max enablement without sanctioned registration.

The test authority is a configurable fake authorization/resource server, analogous to `FakeProvider`. It must simulate:

- success with S256 and exact redirect;
- wrong/missing/duplicate state, wrong path/Host/port, non-GET, oversized request, early malicious local connection, and a state-valid user denial;
- code replay, verifier mismatch, callback timeout/cancel, token endpoint redirect, malformed/oversized token response;
- access expiry, rotating refresh tokens, concurrent refresh, invalid_grant, transient failure, scope/issuer/audience/nonce mismatch;
- crash/failure before and after vault put, refresh response before vault failure, late completion after remove;
- browser/card disconnect and daemon restart requiring a fresh flow;
- remote/non-same-UID start/status/cancel/add rejection and flow/ready-ref connection binding.

The fake server asserts no client secret is sent by a public client and no token is accepted before durable vault persistence. Tests bind real loopback sockets but never a live provider.

Secret sweep expands to unique sentinels for authorization code, state, verifier, access token, refresh token, ID token, client assertion if ever added, and raw token endpoint error/body. Scan formatted errors/frames, receipts/SQLite/WAL, `accounts.json`, provider config, temp files, tracing capture, TUI snapshots, and browser success HTML. Mutation examples: remove constant-time state check, reuse a verifier, accept a redirect, start two refreshes, or return a token before vault write; each must fail a named test.

Live probe: none in CI. A manually enabled sanctioned provider probe may start a browser and add a disposable alias only in a disposable profile; it must require explicit confirmation and include cleanup. Until sanctioned metadata exists, the only “live” probe is the fake server over real HTTP loopback.

### 7.3 W5c — account/provider management wire, actor, receipts, resolver broker

Scope:

- additive feature strings, requests/responses, golden fixtures, unknown tolerance;
- `account.list` optional extension and `provider.list/configure`;
- OAuth start/status/cancel and durable `account.add`;
- durable set-active/remove/set-default-model/provider-configure;
- alias migration, profile-namespaced vault slots, account/provider coherent revisions;
- actor commands, blocking-pool vault/file calls, receipt reconciliation, descriptor-first remove cleanup;
- credential broker resolve/refresh and actual `Resolver` invocation;
- provider factory uses broker, not raw snapshot/vault, and accepts `ResolvedAuth::None` only for immutable no-auth profiles;
- `session.create` and turn resolution consult the live registry snapshot rather than a startup-only whitelist.

Deterministic gates use a real daemon and UDS, injected `MemoryVault`, fake OAuth server, and `FakeProvider`. Cover duplicate command IDs with same/different semantic digest, actor mailbox full/closed, disconnect before and after the atomic OAuth-ready-bundle hand-off, daemon restart at every receipt boundary, golden `revision_conflict` details, removal of active/last account, descriptor-removed/vault-delete-failed/same-alias-readd rejection, alias normalization/migration, OAuth ready-ref one-use, switch affects next logical-turn resolution, stale refresh completion, and unauthenticated-local construction. A configured provider must become creatable without restart; disabling it must block new create/resolve. Built-in identity edits and custom origin/API-family/auth-mode retargets must fail, and no new provider ID may inherit an old account. Initial resolver rotation must return metadata that the harness commits before provider work and must consume the turn's sole rotation allowance. Existing login-to-next-turn test is the pattern (`crates/haider-daemond/tests/account_rpc_tests.rs:842-930`).

Wire mutation law follows current golden tests: deleting a new method arm, removing a feature, making an optional field required, or decoding unknown method as a known mutation must fail a named test (`crates/haider-rpc/tests/wire_golden_tests.rs:47-119`). Receipt mutation law follows `crates/haider-daemon/src/accounts_tests.rs:412-674`.

Live probe: real daemon/UDS plus `FakeProvider` is mandatory; Keychain round-trip remains ignored/manual and uses a disposable profile/alias. No external API.

### 7.4 W5d — `/accounts`, `/providers`, modal management UI, and safe-boundary rotation

Scope:

- add command/screen variants, launcher path, dynamic slots, joined management snapshot;
- 1:1 `/accounts` binding core (group/row/global-add layout, click switching, screen Esc destination) plus separately goldened TUI6 modal-card behavior and W5 extensions;
- owner-approved `/providers` snapshot, custom/local config and default model;
- remove confirmation and live status/error handling;
- OAuth progress card and existing API card through LiveDriver/Link;
- attempt/revision/command identity gating and stale-reply retirement;
- pre-first-event rotation, multi-account usage attribution, durable rotation notes/wait;
- remove demo counts/static identity as live authorities while retaining demo mode.

Deterministic tests:

- render/snapshot at simulator-equivalent terminal sizes for each account status/provider group, preserving the global add-row placement and sim Esc destinations;
- keyboard/mouse/hit map, Esc, surface switch, resize, link loss, stale snapshot, late OAuth/status/commit reply;
- card total modality over menu arrival; draft restore; renderer sees no secret/full URL;
- `/account` case/alias completion, card alias prefill/edit/collision, and server conflict;
- unavailable Google/OAuth reasons;
- 429 before first event rotates once and continues through `FakeProvider`;
- initial limited-account A→B is visible before the first call and a later B 429 cannot rotate to C;
- 429 after first delta does not rotate/replay;
- no alternate enters honest wait/error;
- manual switch affects the next logical turn, while automatic pre-event rotation alone changes a current turn;
- usage attribution preserves both accounts when rotation spans a tool loop.

Secret sweep consumes captured draw buffers, debug snapshots, app/session persistence, clipboard test seam, LiveCommand/Reply formatting, link contexts, RPC frames, and daemon storage. Mutation checks target the exact TUI6 races: move deadline after inbound reduction, omit attempt gate, fail to retire on close, let modal hits fall through, or expose the full URL; every mutation must fail.

Live probe: real daemon/UDS + `FakeProvider` drives both screens, API login with `MemoryVault`, fake OAuth browser callback, switch/remove/default, and a scripted 429 rotation. Optional real OpenAI API-key and sanctioned OAuth probes remain ignored.

### 7.5 Final acceptance matrix

| Gate | W5a | W5b | W5c | W5d |
|---|---:|---:|---:|---:|
| `cargo fmt --all -- --check` | required | required | required | required |
| `cargo clippy --workspace --all-targets -- -D warnings` | required | required | required | required |
| `cargo test --workspace` | required | required | required | required |
| adapter fixtures / `FakeProvider` | required | regression | required | required |
| fake OAuth server | — | required | required | required |
| real daemon + UDS | smoke | flow smoke | required | required |
| receipt crash matrix | — | vault unit | required | regression |
| sentinel secret sweep | API key | all OAuth material | storage/wire | TUI/link/render |
| mutation checks | adapter | OAuth security | receipts/wire | identity/modal/rotation |
| external live API/OAuth | ignored | ignored/sanctioned only | not a gate | not a gate |

## 8. Risk register

| ID | Invariant | Naive mistake | Consequence | Required guard / gate |
|---|---|---|---|---|
| W5-RK1 | descriptor alias is the canonical global user alias | keep hashed physical alias, byte-sensitive `Work`/`work`, or search `identity` | ambiguous/wrong switch; identity cannot become email | lowercase grammar; receipt-driven migration; descriptor/vault namespace separation |
| W5-RK2 | one active account per non-empty provider | mutate snapshot before durable save or select two rows optimistically | wrong credential resolves after crash/reconnect | `AccountStore` validate-before-view; actor single writer; revision-gated UI |
| W5-RK3 | profile vault isolation | use public alias directly as global Keychain account | profiles overwrite each other's `work` token | profile-namespaced Keychain slots and migration test |
| W5-RK4 | secret never in descriptor/receipt/journal | serialize OAuth bundle or full auth URL for convenience | durable access/refresh/code leakage | opaque flow refs; secret-free receipts; expanded sentinel sweep |
| W5-RK5 | zeroizing transit | parse tokens through ordinary `String`/plain boxed bytes/derived `Debug` | heap/log copies survive | zeroizing owned buffers including `SecretHandle`, redacted wrappers, formatter tests |
| W5-RK6 | loopback only | bind `0.0.0.0`, `[::]`, LAN, or user interface | remote callback/code injection surface | numeric loopback bind helper; socket-address assertion |
| W5-RK7 | exact native redirect | copy `localhost:1455` from simulator/Codex | wrong registration, collision, hostname/interface ambiguity | bind `127.0.0.1:0` first; approved variable-port registration |
| W5-RK8 | login CSRF/mix-up defense | omit/reuse state or accept duplicate parameter | attacker binds callback to wrong login | 256-bit single-use state, constant-time exact comparison |
| W5-RK9 | code interception defense | allow plain PKCE, reuse verifier, log it | local attacker redeems intercepted code | 32-byte random verifier, S256 only, zeroizing, fake-server negative tests |
| W5-RK10 | callback is single-use/bounded | accept any path/method/Host, mishandle a valid denial, or allow unlimited requests | local process steals/DoSes listener or user denial hangs | random exact path, GET/Host/size/time/count bounds, code-xor-error, close after terminal callback |
| W5-RK11 | token endpoints are release-owned | accept issuer/token URL from callback/user account | SSRF and token exfiltration | immutable approved metadata, HTTPS, redirect disabled, exact origin |
| W5-RK12 | public client has no embedded secret | bake a client secret into CLI | extractable shared credential and false security | registered public client + PKCE; assert no client secret |
| W5-RK13 | token-at-rest law | add a `0600` JSON fallback | cleartext refresh token survives | Keychain-capable vault only; unsupported platform unavailable |
| W5-RK14 | verified identity only | decode JWT payload without verifying issuer/aud/nonce/signature | spoofed account identity | verified ID token/user-info seam or neutral label |
| W5-RK15 | one refresh per generation | each concurrent turn refreshes independently | refresh-token replay revokes account | keyed single-flight and generation CAS |
| W5-RK16 | rotated token durable before use | return new access token before Keychain put | crash/loss leaves invalid old refresh and untracked new token | vault-first completion; persistence failure -> re-login |
| W5-RK17 | remove is final and reserves its alias | late refresh writes it back or re-add races failed vault deletion | credential resurrection/new account inherits old secret | durable remove tombstone/generation fence; release alias only after vault cleanup |
| W5-RK18 | R7 no-await connection routing | await browser/token/Keychain in RPC handler | one login stalls connection/session liveness | bounded actor/coordinator `try_send`; async response/status |
| W5-RK19 | account actor single writer | call `select/remove` directly in RPC | receipt/snapshot/store divergence | mailbox-only mutations and crash matrix |
| W5-RK20 | durable mutating receipts | treat OAuth add/switch/remove as best-effort | duplicate accounts or lost acknowledged switch | semantic command digest; recovery before Ready |
| W5-RK21 | provider/account coherent view | publish separate unsynchronized snapshots | UI pairs active alias with wrong endpoint/default | combined revision and one published snapshot |
| W5-RK22 | endpoint auth confined to immutable origin | follow redirects, concatenate URLs, or edit a provider origin under existing accounts | bearer key/token forwarded or exfiltrated; path confusion | URL parser, fixed path, redirects off; built-in immutable/custom-new-ID rule |
| W5-RK23 | custom compatibility explicit | silently fall back between Responses and Chat | tools/reasoning fail mid-turn | explicit native Responses and compatible Chat families with separate fixture contracts |
| W5-RK24 | capability truth is bounded | infer model capability only from name or stale probe | send unsupported tools/images/reasoning | registry config + adapter doc + existing typed `InvalidRequest` capability message |
| W5-RK25 | provider policy/legal availability | copy first-party ChatGPT/Claude subscription credentials | account suspension/legal violation/nonfunctional release | sanctioned registration gate; Claude Max disabled under current policy |
| W5-RK26 | provider/model turn pin; account attempt pin only for auto-rotation | manual switch mutates live turn or core calls daemon actor directly | mixed auth, layer violation | injected attempt-resolver port; manual “next turn” UX |
| W5-RK27 | rotation is one-hop and once per logical turn | rotate during initial resolution, then again on 429 | invisible A→B plus B→C retry storm | initial rotation metadata consumes the same core budget; total attempt cap |
| W5-RK28 | no replay after output | rotate/retry after text/tool delta | duplicate text or tool effects | `provider_event_seen` hard gate; mutation test |
| W5-RK29 | rotate only account-scoped failures | rotate on transport/5xx/model errors | hides outage, burns all credentials | explicit trigger taxonomy |
| W5-RK30 | usage attribution is truthful | overwrite account A with B after rotation | incorrect cost/audit data; current invariant panic | additive per-account usage before live rotation |
| W5-RK31 | OAuth/API modal totality | let underlying hits/keys through | accidental session action while secret flow open | existing card gates and TUI6 mutation suite |
| W5-RK32 | issuance identity fences late replies | gate OAuth only by card/provider | cancelled flow selects account on reopened card | flow + attempt + open-card + revision check; retire on close |
| W5-RK33 | TUI is not account authority | persist rows locally or move dot optimistically | stale/wrong credential selected | daemon snapshot/result only |
| W5-RK34 | simulator truth is represented honestly | call `/providers` or progress states a 1:1 port | unreviewed UX ships under false authority | explicit new-design golden/owner approval |
| W5-RK35 | unavailable is not success | stub Google/local/OAuth login | account appears usable but turns cannot run | unavailable reason, disabled action, adapter/probe gate |
| W5-RK36 | provider registry is live authority | leave `SessionHub`'s startup whitelist installed | configured provider cannot create, disabled provider still creates | actor-published registry consulted by create/resolve; mutation test |
| W5-RK37 | no-auth is explicit | require a fake local key or let any profile omit auth | unusable local endpoint or remote unauth bypass | `ResolvedAuth` enum; `None` only for immutable no-auth profile |
| W5-RK38 | revision conflicts are stable/tolerant | return an invented generic `Conflict` | clients cannot recover or golden-pin behavior | `revision_conflict`, retryable, bounded current-revision details |
| W5-RK39 | native OAuth is local-scoped with atomic ownership hand-off | let remote caller allocate a flow or disconnect wipe a queued add's bundle | listener DoS/ready-ref theft or durable command loses its secret | same-UID UDS; claim bundle before `try_send`; job owns after hand-off; wipe only unclaimed |
| W5-RK40 | credential enums remain decodable under v1 | emit a new closed `AuthMethod`/`CredentialStatus` variant | older client rejects whole account response | freeze v1 variants; tolerant additive view/feature for future expansion |
| W5-RK41 | expiry/scope parsing is conservative | trust negative/huge TTL, erase an omitted refresh token, or accept scope loss | stale token, permanent logout, privilege mismatch | bounded expiry/skew, retain permitted old refresh, exact required-scope checks |
| W5-RK42 | authorization URL is transient, never shell data | interpolate full URL into `sh -c`/argv or copy automatically | state/query leaks to logs/process list/clipboard | direct platform opener; zeroizing Link object; explicit warned copy; delete at terminal state |

### R10 — Make security, crash, and fake-boundary tests release law (DECIDED)

No live provider, billing account, subscription, browser identity, or network availability may determine W5 release health. W5 ships only when adapter fixtures, `FakeProvider`, the adversarial fake OAuth server, real daemon/UDS flows, receipt crash boundaries, and the expanded secret sentinel pass. Every load-bearing test names the mutant it kills, continuing the mutation-check convention in the current account and wire suites (`crates/haider-daemon/src/accounts_tests.rs:412-674`, `crates/haider-rpc/tests/wire_golden_tests.rs:47-119`).

## RECOMMENDATIONS

1. **R1:** keep the account/vault/resolver foundation; restore descriptor alias as the global user alias, namespace Keychain internally, and add one versioned provider registry.
2. **R2:** ship native OpenAI through Responses with opaque reasoning continuation, and ship local/custom OpenAI-compatible endpoints through the separate Chat Completions family; never fall back implicitly between them, and leave Google unavailable.
3. **R3:** implement full native-app PKCE loopback infrastructure but enable live subscription OAuth only with sanctioned provider registration and scopes.
4. **R4:** refresh through a single-flight, generation-fenced credential broker; persist rotated tokens before use and fail closed to re-login/rotation.
5. **R5:** add OAuth/account/provider management methods additively under wire v1 with explicit `Welcome.features`.
6. **R6:** preserve the account actor's single-writer and R7 hand-off laws; use durable secret-free receipts and blocking-pool vault/file operations.
7. **R7:** port the simulator's binding `/accounts` core exactly; separately approve/golden `/providers`, removal/default-model, alias entry, accessibility, and progress states.
8. **R8:** use an injected attempt resolver to rotate one hop/once per logical turn only before the first provider event, preserve multi-account usage, and have the session harness emit the durable `RotationEvent`.
9. **R9:** land W5a adapter/registry, W5b OAuth, W5c management/broker, then W5d TUI/rotation.
10. **R10:** gate release with fixture/fake/UDS/crash/mutation/sentinel tests; keep every external API and OAuth probe ignored and manual.
