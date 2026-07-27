# W3c research: daemon-owned live turns, auto-attach, login, and TUI cutover

Date: 2026-07-27

Source pin: `w3-c` at `12bb3a6eb9c16a4a1e3f93c23882f4d94819b377`

TUI seam pin: `w5-tui4` at `906636fd193acf7ef80b6b56450f9e6fecfda937`

## Executive summary

W3a/W3b built the correct daemon substrate. The profile lock is the singleton authority, startup recovery finishes before readiness, the UDS endpoint is published conservatively, the wire handshake is versioned, and `SessionHub` already supplies the session-scoped replay/live barrier, bounded delivery, fair multiplexing, and durable `MenuAnswer` CAS required by the D1 report. `SessionHub` also implements `StoreHandle`, which is the exact seam live workers must receive. Those parts should be extended, not redesigned (`crates/haider-daemon/src/session_hub.rs:1-68`, `crates/haider-daemon/src/session_hub.rs:729-814`, `crates/haider-daemon/src/session_hub.rs:1243-1267`).

The keystone is not wired yet. The production daemon has no provider, accounts, tools, or worker manager dependency; it creates the store, hub, and listener and then serves only list/read/attach/detach/menu operations (`crates/haider-daemon/Cargo.toml:9-23`, `crates/haider-daemon/src/runtime.rs:192-255`). The wire has no session-create, turn-submit, or turn-cancel request (`crates/haider-rpc/src/frame.rs:255-301`). Bare `haider` prints help and exits, while `haider tui` rejects every non-demo invocation (`crates/haider-cli/src/main.rs:53-80`, `crates/haider-cli/src/main.rs:83-114`).

The core actor is a useful but incomplete execution engine. It serializes turns, handles cancellation while opening and reading provider streams, supports a multi-request tool-result loop, and implements durable `request_input` wake-up from the hub. However, it sends only the current user message to the provider, advertises no tools, executes only the special `request_input` call, pins one provider for the actor lifetime, and discards its spawned join handle (`crates/haider-core/src/actor.rs:286-350`, `crates/haider-core/src/actor.rs:385-438`, `crates/haider-core/src/actor.rs:905-927`). A second user message arriving during a turn is merely deferred despite being journaled later as `DeliveryMode::Steer`; it is not actually injected at a safe boundary (`crates/haider-core/src/actor.rs:1404-1434`).

W3c should add a daemon-owned `WorkerManager` with one lightweight supervisor per active session. The supervisor, not the session hub actor, owns the harness task, bounded accepted-turn queue, active cancellation token, provider factory, prompt-history compiler, effect broker, and task joins. The hub actor must remain the short critical section that serializes durable mutation and publication; awaiting a provider or tool inside it would break attachment and menu liveness.

Every mutation must be a correlated request with a durable `command_id`. `session.create`, `turn.submit`, and `turn.cancel` require transactional command receipts. In particular, `turn.submit` must atomically commit its receipt plus `Queued` and `UserMessage` envelopes before responding or starting provider work. This closes the lost-response duplicate-turn window and gives restart recovery a durable queue. There should be no raw “append a `UserMessage` envelope” RPC: clients submit semantic commands; the daemon is the only envelope author.

Provider credentials should be resolved once at the beginning of each logical turn and pinned across all provider requests in that turn. This makes `/login anthropic api` affect the next turn without changing an in-flight tool loop. The existing Anthropic adapter should retain ownership of HTTP/SSE decoding and no-retry transport policy; the turn engine should add bounded, cancellation-aware retry only before the first event of an individual provider request. Retrying after streamed output or a dispatched effect is not safe.

The login slice needs a redacted, non-journaled, connection-scoped `vault.stage` RPC followed by idempotent `account.login_api`. The daemon validates an Anthropic key with a minimal one-token Messages call, writes the Keychain, updates the single-writer account descriptor store, and finalizes a durable command receipt. Raw key bytes must never enter an envelope, account descriptor, tracing field, `Debug` output, or golden transcript. The TUI owns only a masked transient buffer.

Bare `haider` should resolve one shared profile configuration, connect to the deterministic UDS endpoint, and complete `Hello`/`Welcome`. On `ENOENT` or `ECONNREFUSED`, it should spawn the sibling `haiderd` detached and poll that same handshake. Concurrent launchers may both spawn; the lifetime store lock elects one daemon, the loser exits 75, and both CLIs attach to the winner. Clients must never unlink a stale socket. There is no claim file today and W3c should not invent a second authority: endpoint discovery is deterministic and readiness is a successful `Welcome { lifecycle_phase: Ready }`.

The TUI cutover is four ordered changes: first migrate session identity from `u64` to opaque protocol `SessionId` while retaining a separate local UI generation; second make the live reducer consume `RawEnvelope` and map agent events into chips; third confine `PurgeDemoStore` and demo persistence to demo mode; fourth add `LiveDriver` while retaining `DemoDriver` behind `haider tui --demo`. The demo is a shipped mode, not scaffolding.

The recommended lane split is W3c1 (wire/store mutation primitives, worker orchestration, provider/tools/history, fake-provider UDS gate), W3c2 (shared client, auto-spawn, account RPC/login), and W3c3 (TUI identity/reducer/driver cutover). A live Anthropic call is an ignored diagnostic, never the release gate. The release gate is a real daemon and real UDS transport driving the real turn engine with `FakeProvider`, including history, input CAS, cancellation, restart, duplicate command, and secret-leak checks.

## Scope and method

This report is an implementation design over the checked-out source, not a fresh daemon architecture. The binding D1 decisions in §§5.4–5.7 and R8–R14 are treated as law: explicit session attachment, `RawEnvelope.seq` as the sole replay cursor, atomic receiver registration plus high-water capture, at-least-once delivery with an idempotent reducer, bounded queues with the store as lag buffer, and durable first-committed-wins menu arbitration (`docs/research/d1-daemon-research-report.md:589-708`, `docs/research/d1-daemon-research-report.md:1208-1220`).

The source was inspected at the pins above. The TUI review named by the brief is not present in the W3c tree, so its authoritative seam report and the corresponding TUI source were read directly from `w5-tui4@906636fd`. Citations prefixed `w5-tui4@906636fd:` refer to that commit. No claim below relies on memory where code was available.

The W3b2 readiness review is also binding: `HubConnection` is the client seam, live workers receive `SessionHub` as `StoreHandle`, and W3c triggers a policy for quiescent stuck attachments (`docs/briefs/W3b2-review-4-SHIP_WITH_FIXES.md:92-98`). The current `session_hub.rs` is 2,812 lines; the optimization ledger explicitly requires its split as W3c's first commit (`docs/OPTIMIZATIONS.md:168-178`).

Verification on the source pin: `cargo test --workspace`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings` pass. The intentionally ignored Keychain round-trip and live-Anthropic promotion/smoke tests remain non-gating; W3c's deterministic gate is the real-daemon/fake-provider UDS suite specified in §6.1.

## 1. Implemented baseline

### 1.1 `haider-core`: exact worker interface and present behavior

The persistence port is deliberately small:

```text
StoreHandle:
  append(&mut [RawEnvelope]) -> CommittedRange
  read(session_id, since_seq, limit) -> Vec<RawEnvelope>
  latest_seq(session_id) -> u64
```

The store assigns `seq` and `committed_at_ms`; publication is legal only after `append` succeeds (`crates/haider-core/src/lib.rs:42-65`). `SessionHub` already implements this exact trait, routing appends through the session actor while reads go directly to committed storage (`crates/haider-daemon/src/session_hub.rs:1243-1267`).

`HarnessConfig` binds a worker to session, optional branch/agent, device, authority epoch, worker generation, model, token limit, and channel/request bounds. It has no working directory, provider name/account alias, tool registry, effect policy, system prompt, or prompt-history policy (`crates/haider-core/src/actor.rs:46-100`). `SubmitTurn` contains only text (`crates/haider-core/src/actor.rs:102-111`).

`HarnessActor` owns one fixed `Arc<dyn Provider>` and one `Arc<dyn StoreHandle>`. `spawn` creates a detached task and drops the `JoinHandle`; there is no stop command or join contract (`crates/haider-core/src/actor.rs:286-350`). The command loop accepts and executes turns strictly serially. A `TurnHandle` supplies cooperative cancellation and a one-shot outcome (`crates/haider-core/src/actor.rs:193-210`, `crates/haider-core/src/actor.rs:353-380`).

The live turn behavior is:

1. mint a run ID;
2. commit `Queued`;
3. commit `UserMessage`;
4. commit `Thinking`;
5. call `Provider::stream_turn`;
6. reduce streamed text, reasoning, tools, usage, and finish events to envelopes;
7. if a tool result exists, add assistant/tool-result messages and make another provider request;
8. close all items and commit one terminal run state.

Those steps are visible in `crates/haider-core/src/actor.rs:383-438` and `crates/haider-core/src/actor.rs:579-735`. Cancellation is biased over provider-open and stream progress, closes open items as `Cancelled`, and commits terminal `RunState::Cancelled` (`crates/haider-core/src/actor.rs:441-497`, `crates/haider-core/src/actor.rs:1196-1209`). This is the correct cancellation discipline to preserve.

The C2.1 multi-request loop is present, but general tool execution is not. `ToolCallEnd` special-cases only the exact name `request_input`; every other tool item is completed with status `Pending`, removed, and produces no tool-result message (`crates/haider-core/src/actor.rs:905-927`). In addition, every provider request sets `system_prompt: None`, `tools: Vec::new()`, and no attachments, so Anthropic cannot request even the shipped tool implementations (`crates/haider-core/src/actor.rs:430-437`).

`request_input` itself is strong. It commits `MenuOpened` and `RunState::InputRequired`, waits on cancellation, actor commands, or the committed-menu watch, emits `MenuClosed(Cancelled)` on cancel and `MenuClosed(Dismissed)` on actor loss, validates a committed answer, commits `ToolResult`, and resumes the provider loop (`crates/haider-core/src/actor.rs:930-1003`, `crates/haider-core/src/actor.rs:1005-1119`). `HarnessHandle::apply_committed_menu_event` explicitly wakes without re-appending the answer, which matches W3b's hub CAS path (`crates/haider-core/src/actor.rs:231-253`).

Every ordinary core event is stamped with the configured session/branch/run/agent/device/authority/generation and then appended through `StoreHandle` before the actor's local broadcast (`crates/haider-core/src/actor.rs:1316-1352`). That local broadcast must not become a second daemon event path; live clients continue to receive only hub publication.

There are three material local-store assumptions:

- `SqliteStoreHandle` remains a direct `StoreHandle` and `CasSink`, and its owner mutex spans the blocking operation (`crates/haider-core/src/sqlite_store.rs:182-245`).
- The standalone CLI constructs this direct handle and gives it to the actor (`crates/haider-cli/src/main.rs:197-210`).
- `EffectBroker::JournalSink` documentation assumes the sink is the sole underlying journal handle, an assumption written before the hub became the sole daemon append seam (`crates/haider-tools/src/broker.rs:60-81`).

The standalone path may keep its direct store only after it has acquired the same profile lock. It does not satisfy that law today: `run_jsonl` resolves/imports accounts before `SqliteStoreHandle::open` acquires the lock (`crates/haider-cli/src/main.rs:183-205`, `crates/haider-cli/src/main.rs:525-548`). W3c must reorder lock acquisition before all account access or remove that mutating standalone bootstrap. The daemon path must never give a live worker `SqliteStoreHandle`; it gets a hub-backed event committer and daemon-owned CAS service.

The shipped effect/process machinery is otherwise suitable. `EffectBroker` owns the canonical workspace descriptor, session/generation, permissions, process registry, and finalizer `JoinSet`; construction canonicalizes and opens the root with `O_NOFOLLOW` (`crates/haider-tools/src/broker.rs:632-716`). `close` cancels processes, drains finalizers, and reconciles unterminated dispatches to `Unknown` (`crates/haider-tools/src/broker.rs:735-799`). Process execution uses an anchored cwd, cleared environment with an allowlist, a separate process group, TERM-to-KILL escalation, and a zombie-preserving group sweep that avoids recycled-PGID kills (`crates/haider-tools/src/process.rs:547-715`, `crates/haider-tools/src/process.rs:1357-1401`).

Startup recovery currently covers effects only. It replays every session, finds `Dispatched` effects lacking an outcome, and appends `Unknown` before daemon readiness; it does not reconstruct prompt history, terminalize an interrupted run, close an interrupted menu/item, or restart a safely queued turn (`crates/haider-core/src/recovery.rs:1-13`, `crates/haider-core/src/recovery.rs:33-109`).

Finally, there is no conversation-history compiler. The first provider request is initialized as only `Message::user_text(submit.text)`, regardless of committed prior turns (`crates/haider-core/src/actor.rs:408-438`). Although the envelope has `PromptRender` and the protocol has `NodeKind::{UserTurn,AssistantCommit,ToolExchange}`, no current daemon/core path compiles them into `TurnRequest.messages` (`crates/haider-protocol/src/envelope.rs:17-32`, `crates/haider-protocol/src/history.rs:18-66`).

### 1.2 `haider-provider`: Anthropic, retries, configuration, and accounting

The provider-neutral contract is small and adequate: `TurnRequest` carries messages, model, max tokens, optional system prompt, tool definitions, and resolved attachments; `Provider` exposes capabilities and an asynchronous stream (`crates/haider-provider/src/lib.rs:117-129`, `crates/haider-provider/src/lib.rs:186-194`). `FakeProvider` records requests and can assert that a later provider request contains a preceding tool result, making it the correct daemon E2E test provider (`crates/haider-provider/src/lib.rs:271-340`).

The Anthropic adapter is a real streaming Messages client:

- API URL `https://api.anthropic.com/v1/messages` and API version `2023-06-01` are hardcoded;
- connect timeout is 10 seconds and per-chunk idle timeout is 90 seconds;
- reqwest redirects and retries are disabled;
- the constructor binds one resolved `SecretHandle` and one model;
- the API URL is overrideable for explicit tests/capture;
- the turn's requested model must exactly match the provider's configured model.

These facts are in `crates/haider-provider/src/anthropic.rs:18-41`, `crates/haider-provider/src/anthropic.rs:43-107`, and `crates/haider-provider/src/anthropic.rs:141-180`. The request builder already emits system and tools when supplied (`crates/haider-provider/src/wire/mod.rs:14-64`).

The adapter sends a sensitive `x-api-key` header, requires a successful status before starting its SSE decode task, applies the 90-second chunk-idle deadline, and maps decoded events onto the normalized stream (`crates/haider-provider/src/anthropic.rs:155-223`, `crates/haider-provider/src/anthropic.rs:254-307`). There is no response-header/total request deadline. The decoder task is detached; dropping the receiver only makes its next send fail, so a task blocked in `response.chunk()` can live until the idle timeout. `FakeProvider` also detaches its script task (`crates/haider-provider/src/lib.rs:322-340`). HTTP classification is typed: 401 authentication, 403 permission, 429 rate limit, 529 overload, and other 5xx transport, with `Retry-After` parsed into milliseconds (`crates/haider-provider/src/anthropic.rs:354-407`).

Retry ownership is currently a hole, not a duplicate. The adapter's only policy is `Never`, with a comment that the actor owns retry/backoff (`crates/haider-provider/src/anthropic.rs:22-33`), but the actor immediately terminalizes any provider-open or stream error (`crates/haider-core/src/actor.rs:483-495`, `crates/haider-core/src/actor.rs:1212-1227`). W3c must add the missing owner once, in the logical turn engine.

Model capability limits are code heuristics keyed by model-name prefixes, not provider discovery or operator configuration (`crates/haider-provider/src/anthropic.rs:226-251`). The model itself is configurable in `HarnessConfig` and the CLI, but the API version, production endpoint, timeouts, and retry ceiling are not. W3c needs session-level model/provider configuration and daemon-level retry/timeout knobs; it does not need to expose the production endpoint in the TUI.

Usage accounting is provider-reported and careful within one request: input includes cache-creation input, cached is cache-read input, output is provider output, reasoning is currently zero, and the account alias is attached. Overflow is checked (`crates/haider-provider/src/wire/mod.rs:470-486`, `crates/haider-provider/src/wire/mod.rs:526-547`). Across a multi-request tool turn, core commits each request's usage as-is while the TUI replaces its meter with the latest event, so the visible total becomes only the last request (`crates/haider-core/src/actor.rs:620-624`, `w5-tui4@906636fd:crates/haider-tui/src/projection.rs:227`).

There is no OpenAI or other concrete adapter in `haider-provider`. The provider-neutral trait, message/block IR, capabilities document, and fake adapter are the only second-provider scaffolding. TUI help text mentioning OpenAI/Google/local is product vocabulary, not implementation (`w5-tui4@906636fd:crates/haider-tui/src/commands.rs:215-230`).

### 1.3 `haider-accounts`: current vault and resolver

`SecretHandle` is intentionally non-serializable and non-cloneable, redacts `Debug` and `Display`, and best-effort scrubs on drop. Raw bytes are available only through the explicit accessor (`crates/haider-accounts/src/vault.rs:19-61`). `Vault` is synchronous and supports put/resolve/delete/list; `MemoryVault` is the deterministic test implementation (`crates/haider-accounts/src/vault.rs:64-137`).

On macOS, `KeychainVault` stores generic passwords under service `ai.haider.agent`; put, resolve, delete, and list use the credential alias as the account key (`crates/haider-accounts/src/keychain.rs:17-18`, `crates/haider-accounts/src/keychain.rs:65-76`, `crates/haider-accounts/src/keychain.rs:89-159`). These calls may display OS UI and are synchronous, so daemon integration must run them on the blocking pool while preserving single account-command order. The type exists elsewhere, but every operation returns a non-retryable internal “requires macOS Security.framework” error; there is no Linux vault backend (`crates/haider-accounts/src/keychain.rs:177-202`).

Credential descriptors live in `<profile>/accounts.json`. The JSON store fsyncs a fixed temporary file and renames it atomically, but does not fsync the parent after rename; its own comment says the fixed name assumes one writer until daemon integration (`crates/haider-accounts/src/store.rs:23-111`). `AccountStore` validates snapshots, prohibits duplicate aliases, preserves exactly one active account per provider, and exposes list/get/active selection (`crates/haider-accounts/src/store.rs:138-205`, `crates/haider-accounts/src/store.rs:231-258`).

`Resolver::resolve_for_provider` selects the active usable descriptor, applies the rate-limit rotation callback, and resolves the alias through the vault (`crates/haider-accounts/src/resolver.rs:59-90`, `crates/haider-accounts/src/resolver.rs:139-145`). The environment bridge imports a secret exactly once as deterministic alias `<provider>-env`, scrubs its local buffer, and never reads the environment during later resolution (`crates/haider-accounts/src/env_bridge.rs:15-57`). Keychain's service is global and its account key is only that alias, whereas descriptors are profile-local; without profile namespacing, two profiles can overwrite each other's secret.

The current standalone Anthropic CLI already composes these pieces: if there is no active Anthropic descriptor, it imports `HAIDER_ANTHROPIC_API_KEY` into Keychain, adds a descriptor, resolves it, and constructs `AnthropicProvider` with the alias (`crates/haider-cli/src/main.rs:525-556`). That is useful bootstrap precedent but not an interactive login: there is no key prompt, credential validation, daemon account owner, secret-staging RPC, or crash-safe multi-store command receipt.

The domain law remains strict: `CredentialDescriptor` carries aliases and status, never secret bytes (`crates/haider-protocol/src/credential.rs:1-38`). `MenuKind::Secret` further requires a dedicated non-journaled vault operation followed by an opaque vault reference in the durable menu answer (`crates/haider-protocol/src/menu.rs:32-58`, `crates/haider-rpc/src/frame.rs:412-429`).

### 1.4 `haider-rpc`, daemon, and daemond: what is already on the wire

Wire protocol v1 is strict only about the top-level version. Unknown frame kinds, request methods, and object fields are tolerated (`crates/haider-rpc/src/frame.rs:10-21`). Existing handshake fields cover protocol range, client identity/kind, requested capabilities, receive limit, daemon instance/generation/profile/version, granted capabilities, frame limit, and lifecycle phase (`crates/haider-rpc/src/frame.rs:124-195`). Negotiation selects only an implemented overlapping version and otherwise fails (`crates/haider-rpc/src/negotiation.rs:28-92`).

Existing request methods are exactly:

- `session.list`;
- `session.read`;
- `session.attach`;
- `session.detach`.

Existing correlated responses add `menu.answer` success and typed errors (`crates/haider-rpc/src/frame.rs:255-359`). Existing top-level frames are `Hello`, `Welcome`, `Request`, `Response`, `Event`, `AttachCaughtUp`, `MenuAnswer`, `Lagged`, `ServerDraining`, `Ping`, `Pong`, `ProtocolError`, and tolerant `Unknown` (`crates/haider-rpc/src/frame.rs:431-549`).

`MenuAnswer` already carries durable `command_id`, optional request correlation, session/menu/request-sequence/worker-generation coordinates, selected option, and optional non-secret or vault-reference input (`crates/haider-rpc/src/frame.rs:412-429`, `crates/haider-rpc/src/frame.rs:485-510`). Post-handshake, the production connection accepts ping, requests, and menu answers; a known but directionally invalid frame is fatal (`crates/haider-daemon/src/connection.rs:1118-1184`).

`SessionHub` is production-ready for its current surface. Its six stated laws are:

1. persist before publish;
2. register receiver plus observe head `H` in one actor step;
3. use the store as the bounded lag buffer;
4. never mention an attachment ID before its response;
5. pace delivery through atomic, fair sink admission;
6. cap attachment admission before actor/channel work.

The law text and W3c seams are explicit at `crates/haider-daemon/src/session_hub.rs:1-68`. `open_connection` is the handshake-to-session boundary, and `append` is the only legal live-daemon append seam by discipline (`crates/haider-daemon/src/session_hub.rs:729-795`). `register_harness` exists specifically so a daemon worker can receive committed menu resolutions while using the hub as its `StoreHandle` (`crates/haider-daemon/src/session_hub.rs:797-814`).

The hub lazily creates one actor per observed session and never retires it (`crates/haider-daemon/src/session_hub.rs:816-864`). The actor serializes store append followed by synchronous publication; receiver insertion and high-water capture are adjacent; menu CAS publication wakes the registered harness only after commit (`crates/haider-daemon/src/session_hub.rs:1853-1924`, `crates/haider-daemon/src/session_hub.rs:1948-1985`).

There is no worker orchestration hidden in the daemon. Production dependencies include core/protocol/RPC but provider is dev-only and accounts/tools are absent (`crates/haider-daemon/Cargo.toml:9-23`). Runtime startup acquires the profile lock, opens the store, advances daemon generation, reconciles ambiguous effects, constructs a default-config hub, binds the endpoint, and advertises ready (`crates/haider-daemon/src/runtime.rs:173-255`). `DaemonConfig` contains only endpoint/queue/connection/handshake/drain settings; it has no provider, account, worker, retry, cwd, or idle policy (`crates/haider-daemon/src/config.rs:6-80`).

The store schema has session metadata capacity but no API uses it. Migration v1 created `sessions.meta_json`; ordinary append auto-creates the row with literal `{}` (`crates/haider-store/src/migrations.rs:24-48`, `crates/haider-store/src/event_store.rs:837-858`). Migration v4 added only the specialized menu-resolution idempotency index; there is no generic command receipt (`crates/haider-store/src/migrations.rs:69-91`).

The golden-wire discipline must remain intact. Exact WS bodies and length-prefixed UDS streams are fixture-pinned; unknown future methods and fields decode safely; wrong wire versions fail; and `Event` is tested not to grow a parallel cursor (`crates/haider-rpc/tests/wire_golden_tests.rs:47-119`, `crates/haider-rpc/tests/wire_golden_tests.rs:326-347`). Menu success has a named mutation-check test (`crates/haider-rpc/tests/wire_golden_tests.rs:204-219`). Therefore new request/response method variants are additive under v1; removing or retyping existing fields is not.

The existing daemond UDS suite is the right E2E pattern. It starts the real daemon, opens a real `UnixStream`, performs `Hello`, sends encoded requests, and asserts response-before-event plus replay order (`crates/haider-daemond/tests/session_rpc_tests.rs:1-143`, `crates/haider-daemond/tests/session_rpc_tests.rs:145-266`). Its comments state the mutation and exact expected failure; new W3c cases should do the same.

### 1.5 TUI seam at `w5-tui4`

The Fable seam report accurately identifies the live-swap touch map and three cuts (`w5-tui4@906636fd:docs/briefs/TUI4-review-1-NO_SHIP.md:170-181`). Its item 7 says to delete `demo_store.rs`; W3c's explicit product requirement supersedes that disposal recommendation. The correct boundary is to retain the demo implementation behind the demo driver while removing its vocabulary from common/live state.

`SessionState.id`, `active_session`, `last_detached`, and `next_session_id` are `u64`; live protocol `SessionId` is an opaque string newtype (`w5-tui4@906636fd:crates/haider-tui/src/session.rs:27-43`, `w5-tui4@906636fd:crates/haider-tui/src/app.rs:861-875`, `crates/haider-protocol/src/ids.rs:1-33`). New sessions are locally minted by incrementing `next_session_id`, then opened before any daemon response exists (`w5-tui4@906636fd:crates/haider-tui/src/app.rs:2238-2260`).

`SessionState::absorb` consumes `DemoEvent`, not `RawEnvelope`. Its chip add/state/emit/note/token/question/resolve/remove variants are all demo vocabulary; only the nested envelope half is shared (`w5-tui4@906636fd:crates/haider-tui/src/session.rs:115-227`).

The projection already understands most head-turn envelopes. It deduplicates by sequence, records gaps, honors `render.ui`, and reduces Harness/Session/Run/Menu/User/Item/Usage events. But it currently continues applying after a gap, whereas live D1 behavior must stop and reattach. It explicitly ignores Effect, ToolResult, NodeCommitted, all three Agent events, GateReport, and Rotation (`w5-tui4@906636fd:crates/haider-tui/src/projection.rs:195-268`).

This means a basic head-only live turn can render through `UserMessage`, `Item`, `Usage`, and terminal `RunState`, but live subagents will not populate or update chips. `ToolResult` being ignored is acceptable for display because the corresponding `Item::ToolCall` lifecycle is visible; ignored Agent events are not acceptable because chip state otherwise has no live source. The full protocol vocabulary is at `crates/haider-protocol/src/lib.rs:32-80`.

`run_demo` owns `DemoDriver`, demo arm/meter state, the `(generation, DemoEvent)` channel, frame-cadence/quit persistence, and the answer-echo outbox (`w5-tui4@906636fd:crates/haider-tui/src/runtime.rs:189-250`, `w5-tui4@906636fd:crates/haider-tui/src/runtime.rs:272-384`). `AppRequest::PurgeDemoStore` is explicitly runtime-owned demo persistence vocabulary (`w5-tui4@906636fd:crates/haider-tui/src/app.rs:649-655`).

Composer submission already produces `AppRequest::SubmitText`, but launcher submission first creates a local session and active-turn UI state (`w5-tui4@906636fd:crates/haider-tui/src/app.rs:1445-1533`). Menu outbox entries contain a local epoch and domain `MenuAnswer`, not the wire's command ID, opening request sequence, worker generation, or protocol session ID (`w5-tui4@906636fd:crates/haider-tui/src/app.rs:1696-1710`, `w5-tui4@906636fd:crates/haider-tui/src/app.rs:1971-1989`).

`/login <provider> <oauth|api>` is registered and documented, but command execution groups it with W3 stubs (`w5-tui4@906636fd:crates/haider-tui/src/commands.rs:15-85`, `w5-tui4@906636fd:crates/haider-tui/src/app.rs:1911-1929`). Only `/theme` has executable argument slots (`w5-tui4@906636fd:crates/haider-tui/src/commands.rs:109-153`). The composer renderer displays ordinary text and has no secret-input state (`w5-tui4@906636fd:crates/haider-tui/src/render.rs:2303-2366`).

The TUI identity defaults to provider `anthropic` and display alias `fable-5`; that short label is not a deploy-valid provider model ID (`w5-tui4@906636fd:crates/haider-tui/src/app.rs:792-809`). The adapter's prefix table is only a capability heuristic and does not prove Anthropic accepts a name; it sends the configured string unchanged, and the current Anthropic CLI deliberately requires `--model` (`crates/haider-provider/src/anthropic.rs:141-180`, `crates/haider-cli/src/main.rs:453-458`). Live creation must use the release-owned full model ID from resolved profile configuration, then derive a short display label.

Two further live mismatches follow from the code:

- boot-to-launcher currently waits for an envelope `HarnessStatus::Ready`, but a live client learns readiness from `Welcome.lifecycle_phase`; daemon readiness is not currently emitted into every session (`w5-tui4@906636fd:crates/haider-tui/src/app.rs:1992-2003`, `crates/haider-rpc/src/frame.rs:173-195`);
- live creation must wait for `session.create`, then attach, then submit; it must not synthesize a user row before the daemon's durable event.

The demo must remain independently runnable as `haider tui --demo`. The correct interpretation of the seam report is “remove demo vocabulary from the live driver,” not “delete the shipped demo.”

### 1.6 Bare CLI, endpoint discovery, and singleton behavior

Bare `haider` currently prints a usage sentence and exits success. `haider tui` only permits `--demo`; `run --jsonl` is a standalone direct-store harness (`crates/haider-cli/src/main.rs:53-114`, `crates/haider-cli/src/main.rs:168-210`). The CLI depends on RPC but not `haider-daemon`, and no reusable daemon client exists (`crates/haider-cli/Cargo.toml:13-22`).

The current profile resolver is only `HAIDER_PROFILE_DIR` or `~/.haider/dev-profile`. It does not produce the profile ID/runtime directory triple required by `DaemonConfig` (`crates/haider-cli/src/main.rs:578-590`). `haiderd` requires explicit `--profile`, `--store-dir`, and `--runtime-dir`; it has no detach, log, idle, or default-profile flags (`crates/haider-daemond/src/main.rs:1-52`).

The endpoint path is deterministic: a 32-hex-character prefix of the BLAKE3 profile-ID hash under `runtime_dir` (`crates/haider-daemon/src/config.rs:82-91`). There is no endpoint claim file or readiness file. A client discovers the socket by resolving the same profile ID and runtime directory.

The daemon already implements the necessary claim discipline. It acquires the lifetime store lock before endpoint inspection or store open (`crates/haider-daemon/src/runtime.rs:173-196`). The lock is a real OS advisory lock released on process death; PID/timestamp file contents are diagnostic only and never authoritative (`crates/haider-store/src/profile_lock.rs:1-9`, `crates/haider-store/src/profile_lock.rs:21-75`). A loser returns typed `AlreadyRunning`, mapped to sysexits 75 (`crates/haider-daemon/src/error.rs:58-67`, `crates/haider-daemond/src/main.rs:1-5`).

Only the lock winner performs endpoint recovery. It creates/verifies a `0700` owner directory, binds a random staging socket, verifies/chmods it to `0600`, and non-replacing-renames it to the public path (`crates/haider-daemon/src/endpoint.rs:215-309`, `crates/haider-daemon/src/endpoint.rs:338-375`). Preflight removes only a verified owner socket after `ECONNREFUSED` and a second probe; a timeout is treated as live and left untouched (`crates/haider-daemon/src/endpoint.rs:377-517`, `crates/haider-daemon/src/endpoint.rs:520-537`).

The client's readiness test is therefore not “claim file exists.” It is:

```text
connect deterministic UDS
send Hello for the intended profile/capabilities
receive Welcome for the intended profile with lifecycle_phase = Ready
```

The public socket is bound only after recovery and immediately before the accept loop/Ready transition (`crates/haider-daemon/src/runtime.rs:192-257`). A successful ready `Welcome` is the externally meaningful gate.

## 2. Gap matrix for the W3c keystone

| Keystone promise | Implemented substrate | Missing W3c work |
|---|---|---|
| bare `haider` starts daemon | lifetime lock, deterministic UDS, safe stale cleanup, thin `haiderd` | shared profile resolver, RPC client, detached spawn, handshake poll, loser handling |
| TUI attaches live | explicit attach, replay/live barrier, bounded fair outbox | `LiveDriver`, reconnect cursor, string IDs, raw-envelope router, response-to-model effects |
| create a real session | sessions table auto-created on first append | typed metadata, daemon-minted ID, `session.create`, transactional command receipt |
| submit a real turn | core turn actor and hub `StoreHandle` | `turn.submit`, durable acceptance, worker manager, provider factory, history compiler |
| Anthropic streams | real Messages/SSE adapter | daemon provider dependency, account resolution, retry owner, configured session model |
| tools execute | robust `EffectBroker`/process supervision | definitions in provider request, general dispatcher, hub journal/CAS adapters |
| input answers work | durable menu CAS and harness wake are complete | supervisor registration/lifecycle and wire coordinate storage in TUI |
| cancel/kill works | cooperative turn cancellation and process TERM/KILL | `turn.cancel`, durable intent/receipt, queued cancellation, shutdown ownership |
| restart is honest | generation bump and effect-unknown reconciliation | interrupted-run/item/menu reduction, safe queued-turn replay, no active-turn auto-retry |
| `/login anthropic api` | Keychain, descriptor store, resolver, Anthropic error classifier | masked card, secret stage, validator, daemon account actor, crash/idempotency receipt |
| version skew is legible | protocol-range negotiation and daemon version | additive feature advertisement and “running daemon too old” handling |

## 3. Recommended architecture

### R1 — Keep the hub actor pure; add a separate per-session worker supervisor (DECIDED)

**Decision.** Add daemon `WorkerManager`, with one lazy `SessionWorker` supervisor per session. The manager owns a `JoinSet` (or equivalent tracked tasks), a bounded supervisor command channel, and shutdown. A supervisor owns:

- session metadata and canonical cwd;
- accepted-turn queue;
- active run ID and cancellation token;
- one live harness handle and its join handle;
- turn-scoped provider resolution;
- prompt-history compilation;
- the session's effect broker/tool dispatcher;
- registration/unregistration of the harness with `SessionHub`.

The `SessionHub` actor remains responsible only for short ordered operations: append/publish, receiver registration/high-water capture, durable command mutations, menu CAS, and harness-wake delivery.

**Rejected alternative: put provider execution in the existing session hub actor.** This would await network streams and tools in the same command loop whose adjacency proves the replay/live invariant. Attach, menu answer, publication, and cancellation would be hostage to provider latency.

**Rejected alternative: one unstructured task per RPC turn.** It would permit two workers for one session, lose serial prompt history, make menu wake ownership ambiguous, and recreate the detached-join problem already present in `HarnessActor::spawn` (`crates/haider-core/src/actor.rs:341-350`).

**Required lifecycle changes.**

- Construct `HarnessActor::new` and put its `run()` future in the manager's tracked set; do not call the detached `spawn`.
- Add explicit core stop/shutdown so dropping handles is not the only exit.
- Make hub harness registration generation/token aware. An old worker may not overwrite or unregister its successor. Current registration is a bare `Option` replacement (`crates/haider-daemon/src/session_hub.rs:1983-1985`).
- Treat store `worker_generation` as a daemon-restart fence, not a same-process task lease. It is allocated once when the store opens, and ordinary append currently accepts caller-stamped generation without comparing it (`crates/haider-store/src/event_store.rs:155-198`, `crates/haider-store/src/event_store.rs:837-917`). `register_harness` must mint an opaque `WorkerLeaseId` and return a worker-only `HubStoreHandle { session_id, worker_generation, lease_id }`. Every worker append carries that lease out-of-band to the hub actor; replacement revokes the old lease before the successor starts. The worker gets this restricted `StoreHandle`, never the raw hub facade or SQLite.
- Stop and join a supervisor before removing it. Actor retirement remains deferred until real working-set data exists; W3c does not need speculative eviction.

### R2 — Make session creation and mutation receipts first-class store transactions (DECIDED)

**Decision.** Add migration v5 with a generic `command_receipts` table and typed access to `sessions.meta_json`.

Recommended receipt shape:

```text
command_receipts
  command_id       TEXT PRIMARY KEY
  method           TEXT NOT NULL
  request_digest   TEXT NOT NULL
  request_json     TEXT NOT NULL       -- canonical secret-free recovery coordinates
  state            TEXT NOT NULL       -- pending | committed | failed
  session_id       TEXT NULL
  run_id           TEXT NULL
  accepted_seq     INTEGER NULL
  response_json    TEXT NULL
  created_at_ms    INTEGER NOT NULL
  updated_at_ms    INTEGER NOT NULL
```

For every retry:

1. same `command_id` + same method/semantic digest returns the original committed response; method definitions explicitly omit ephemeral transport coordinates such as a fresh login `vault_reference`;
2. same `command_id` + different coordinates is `invalid_argument`;
3. pending external account operations resume their explicit reconciliation path;
4. no client request ID is used as a durable key.

This generalizes the already-correct specialized menu rule, where same-command replay precedes generation fencing and returns the original resolution coordinate (`crates/haider-store/src/event_store.rs:270-303`, `crates/haider-store/src/event_store.rs:415-455`).

Add typed `SessionMetadataV1` in `sessions.meta_json`:

```text
cwd          canonical UTF-8 path
provider     "anthropic" | "fake" in tests
model        provider model ID
max_tokens   u64
created_at_ms
```

`session.create` atomically:

- claims/validates the command receipt;
- mints an opaque random `SessionId`;
- inserts the typed metadata row;
- appends `SessionState::Created` at sequence 1;
- finalizes the receipt with the response.

`cwd` must be an absolute path supplied by the client. The daemon rejects relative input, canonicalizes and opens the workspace before commit, and persists that identity; it must never resolve against the daemon cwd inherited from whichever terminal happened to start it. That descriptor becomes the `EffectBroker` anchor (`crates/haider-tools/src/broker.rs:685-700`).

Ordinary append's `{}` metadata insertion remains a legacy/test fallback (`crates/haider-store/src/event_store.rs:851-858`); production live creation must use the typed transaction.

Extend existing `SessionSummary` and `SessionReadResult` with `metadata: Option<SessionMetadataV1>` using `serde(default, skip_serializing_if = "Option::is_none")`. They currently expose only ID/head/generation or envelopes (`crates/haider-rpc/src/frame.rs:236-253`). Old sessions with `{}` return `None`; new sessions let the launcher show cwd/provider/model without replaying the entire journal. This is an additive wire-v1 field, not a new listing method.

**Rejected alternative: create a session by appending the first user message.** It cannot return durable configuration before the worker starts, conflates identity with a turn, and duplicates on a lost response.

**Rejected alternative: store cwd/provider/model as an `AnnotationKind::Other`.** The history type permits it (`crates/haider-protocol/src/history.rs:57-66`), but configuration lookup would require journal reduction before every list/start and would leave the existing metadata column unused. Metadata is authoritative configuration; the Created envelope is the observable event.

### R3 — Durably accept a turn before provider work; do not expose raw append (DECIDED)

**Decision.** `turn.submit` is a semantic command. Inside the session actor's existing serialized order it atomically commits:

1. the command receipt;
2. `RunState::Queued` for a daemon-minted `RunId`;
3. `UserMessage { text, attachments, mode }`;
4. the receipt response coordinates.

Only after this transaction is durable and published does `WorkerManager` enqueue/start provider work. The correlated response returns `run_id` and the `UserMessage` sequence (`accepted_seq`). This preserves response loss idempotency and ensures attached clients can always reconstruct what was accepted.

Provider/store failure also needs a durable observable cause. Today the typed `HaiderError` survives only in the in-process `TurnOutcome`; the journal receives unit `RunState::Errored` (`crates/haider-core/src/actor.rs:149-155`, `crates/haider-core/src/actor.rs:1283-1293`). Add an additive protocol payload `RunFailed { code: ErrorCode, message: String, retryable: bool }`, committed immediately before `Errored`, with UI/durable render and prompt omission. The message must be sanitized and bounded; provider response bodies and secrets are never eligible. Typed readers retain the existing `RawEnvelope` fallback for unknown payload kinds (`crates/haider-protocol/src/envelope.rs:1-9`).

The core actor needs a `submit_committed_turn` entry that receives the existing `run_id` and does not append `Queued`/`UserMessage` again. The old `submit_turn` may remain as a standalone convenience implemented through a local acceptance adapter.

The worker queue is reconstructed from durable runs that have `Queued` plus `UserMessage` and no later active/terminal state. A queued run is safe to start after restart because no provider request was begun. A run with `Thinking`, `Streaming`, or `RunningTool` is not safe to resume automatically. A run parked on a durable `request_input` menu is a separate checkpoint case handled by R5: reconstruct its waiter without reissuing the preceding provider request or protected effect, and resume only after the one durable answer.

The daemon session actor is the aggregate lifecycle owner. Acceptance of the first runnable/queued turn appends `SessionState::ActiveRun` in the same transaction; after the final queued run terminalizes it appends `SessionState::Idle { interrupted: false }`. Cancellation/recovery owns the interrupted variant. Core and worker code must not independently synthesize session state. No production core/daemon path emits normal `ActiveRun`/`Idle` today, although the variants already exist (`crates/haider-protocol/src/state.rs:34-47`).

**Rejected alternative: reply when `HarnessHandle::submit_turn` returns.** Today that return happens before the actor commits `Queued` or `UserMessage` (`crates/haider-core/src/actor.rs:193-210`, `crates/haider-core/src/actor.rs:353-400`). Losing the response or process in that window gives the client no authoritative acceptance coordinate.

**Rejected alternative: add `user_message.append`.** It would let clients author protocol facts, bypass run/provider/cancellation invariants, and require a separate “start” race. One `turn.submit` owns both accepted input and execution intent.

`DeliveryMode` must become real. For W3c:

- `Queue` creates a distinct queued run after the active run;
- `Steer` on an active run is accepted into a bounded active-turn queue and injected at the next provider-request boundary; if the current request ends the run before such a boundary, it becomes the next queued run with an explicit response disposition.

The response should include `disposition = started | queued | steer_pending`. If the full steer behavior is not implemented in W3c1, live TUI must disable it and request `Queue`; it must not preserve the demo's “steered” note while merely deferring a fresh turn. The current core behavior is exactly that false seam (`crates/haider-core/src/actor.rs:1404-1434`).

### R4 — Compile committed history and wire general tool execution before calling the provider (DECIDED)

**Decision.** Add a deterministic `PromptHistoryCompiler` in core, a versioned daemon-owned `SystemPromptBuilder`, and a daemon `ToolDispatcher` port.

For each new logical turn, the compiler pages the session journal through the worker's hub `StoreHandle` and constructs messages from committed, terminal prior runs:

- include `UserMessage` and final completed assistant text for `Done` runs;
- include completed tool-call/tool-result exchanges in order;
- honor `RenderTargets.prompt`;
- exclude UI-only state, menus, usage, reasoning marked omit/pruned, and partial assistant output from errored/interrupted runs;
- append the current accepted user message exactly once.

Compilation is scoped by the harness's `branch_id`, `agent_id`, and run ancestry. `StoreHandle::read` is session-wide while those identities are envelope fields; filtering only by session would mix a branch or child agent into the head transcript (`crates/haider-protocol/src/envelope.rs:42-49`, `crates/haider-core/src/actor.rs:49-52`).

The store guarantee is complete committed sequence order, not restoration of in-memory provider state. `RawEnvelope.seq` and prompt render policy support deterministic replay (`crates/haider-protocol/src/envelope.rs:1-65`). Until tree commits are emitted consistently, compile from the event stream; do not pretend the currently sparse `NodeCommitted` stream is complete history.

`SystemPromptBuilder` deterministically binds the canonical cwd, tool/effect policy, and model/provider-independent coding-agent instructions. Its version is recorded in session/run metadata and snapshot-tested; every provider request in a pinned logical turn receives the same non-`None` system prompt. Provider adapters do not invent product policy.

The dispatcher supplies actual `ToolDefinition`s to `TurnRequest.tools`, validates streamed arguments, executes supported operations through `EffectBroker`, emits bounded `ToolResult`, and returns `Message::tool_result` so the existing multi-request loop continues. `request_input` remains the interactive special case but uses the same dispatcher contract.

Do not replay normalized reasoning in an Anthropic follow-up. Core currently pushes each `ReasoningDelta` into `assistant_blocks` and sends that message on the next tool-loop request, while the Anthropic encoder explicitly rejects `Block::Reasoning` because signed thinking cannot be reconstructed (`crates/haider-core/src/actor.rs:586-590`, `crates/haider-core/src/actor.rs:699-709`, `crates/haider-provider/src/wire/mod.rs:91-95`). W3c keeps reasoning as UI/durable prompt-omitted output but excludes it from follow-up messages. Preserving provider-signed opaque thinking is a later IR extension; it must never be synthesized.

Define `EventPayload::Usage` as a checked cumulative snapshot for the current logical turn. Core retains only the latest provider-reported usage for each individual request, adds that request once at finish/error to a turn accumulator, and emits the cumulative snapshot with the pinned account. This lets replacement projection remain correct and prevents double-counting repeated updates. Two-request, cache-token, error-after-usage, and `u64` overflow tests are release gates.

Provide two daemon-owned adapters:

- `HubJournalSink`: stamps worker identity/generation and appends effect envelopes through `SessionHub`;
- `HubArtifactStore`: offers CAS operations without exposing `SqliteStoreHandle` to a worker.

The wording in `EffectBroker::JournalSink` that assumes a sole direct journal handle must be updated to mean a sole worker mutation authority. The daemon owns the SQLite handle; the worker owns only the hub adapter. General event IDs must come from one thread-safe worker-generation namespace so core and broker events cannot collide.

**Rejected alternative: ship Anthropic text-only while advertising tools.** Empty `TurnRequest.tools` makes the product a chatbot, while advertising definitions without executing them can trap a real model in an unproductive multi-request loop.

**Rejected alternative: let `EffectBroker` append directly to SQLite.** It would violate the W3b law that every live worker append is ordered through the hub and reopen the replay/live race (`crates/haider-daemon/src/session_hub.rs:751-759`).

### R5 — Make cancellation and restart terminalization durable and generation-fenced (DECIDED)

**Decision.** `turn.cancel` atomically records a command receipt and appends `RunState::Cancelling` before waking the supervisor.

- Active run: signal the existing `CancelToken`; core closes open items, process execution escalates TERM→KILL as needed, effect broker journals terminal/unknown truth, and core appends `Cancelled`.
- Queued run: supervisor/session actor appends `Cancelled` without starting a provider.
- Already terminal run: return idempotent `already_terminal` with its terminal sequence.
- Stale worker generation: reject before signaling any in-memory worker.

At startup, after existing effect reconciliation and before Ready, reduce every session:

- a prior-generation run with only accepted `Queued` is safe to re-enqueue;
- `Cancelling` becomes `Cancelled`;
- a run parked at a durable `request_input` checkpoint keeps its pending menu and is reconstructed as a waiter without a provider/effect call;
- any other nonterminal prior-generation run becomes `Errored`;
- open items are completed `Failed`, except a durable cancelling run uses `Cancelled`;
- a non-resumable run's open menu remains durable for history but is closed with additive `MenuCloseReason::RecoveryInterrupted`; a pending checkpoint menu is not closed (today the enum has only `Cancelled`/`Dismissed`, so silently reusing either would lie) (`crates/haider-protocol/src/menu.rs:110-116`);
- a terminalized session becomes `Idle { interrupted: true }`; a checkpointed session remains active/input-required.

Recovery appends one deterministic, transactional batch per interrupted run. A rerun either sees the whole terminal batch or none. Pending menus are binding durable state, not casualties of losing the in-memory waiter (`docs/research/d1-daemon-research-report.md:667-708`).

For a checkpointed `request_input`, reconstruct the assistant tool-call context and menu from committed envelopes. `MenuAnswer` continues to present the opening `request_seq` and opening generation. The menu CAS must validate those durable opening coordinates but stamp a newly committed answer with the current store generation; the current implementation instead rejects any pre-restart opening because it first requires `command.worker_generation == current_worker_generation` (`crates/haider-store/src/event_store.rs:446-477`). If the answer committed before waiter registration, startup observes it; otherwise the hub wakes the reconstructed waiter. Append the tool result exactly once, then rebuild history and issue only the *next* provider request. Never replay the provider request that produced the menu, and never redispatch an effect already `Dispatched`.

Never auto-reissue an active provider request or tool after process death. The journal can prove accepted input and committed events; it cannot prove what a provider or external effect performed after the last commit. Existing recovery already applies that honesty rule to ambiguous effects (`crates/haider-core/src/recovery.rs:1-13`).

Enforce two distinct fences:

- commands and worker envelopes carry the current persisted `worker_generation`, which rejects pre-restart clients/work;
- the hub actor accepts a worker append only from the currently registered `WorkerLeaseId`, which rejects a superseded task in the same daemon generation.

Only menu CAS compares generation today (`crates/haider-store/src/event_store.rs:446-477`); ordinary append does not (`crates/haider-store/src/event_store.rs:837-917`). A generation-only check therefore cannot distinguish two same-process supervisors. R1's hub-issued lease is mandatory, including token-aware register/unregister and cancellation delivery.

### R6 — Resolve and pin a provider per logical turn; retry only before first event (DECIDED)

**Decision.** Replace the actor-lifetime fixed provider with a daemon `ProviderFactory`:

```text
resolve_for_turn(session_metadata)
  -> ResolvedTurnProvider {
       provider: Arc<dyn Provider>,
       provider_name,
       model,
       account_alias
     }
```

Resolution occurs after durable acceptance but before `Thinking`/provider work. It reads the active account through the daemon-owned account manager, creates `AnthropicProvider`, and pins that provider/account for every request in the logical turn. A login or account switch during the turn affects the next logical turn only.

**Rejected alternative: resolve on every `stream_turn` call.** A multi-request tool loop could switch credentials/models halfway through one turn, corrupt accounting, and produce provider-incompatible conversation state.

**Rejected alternative: keep one provider on the long-lived harness.** `/login` would not affect the next turn unless the whole worker were torn down, and an in-flight worker/account replacement race would remain implicit.

Add turn-engine retry with these rules:

- maximum three attempts per individual provider request;
- only `retryable` authentication-independent transport/rate-limit/overload errors;
- respect `retry_after_ms`, otherwise exponential full jitter with a configured ceiling;
- cancellation wins every wait;
- append `RunState::Waiting { ProviderBackoff | RateLimit }` while delayed;
- retry only if that request has emitted no stream event and dispatched no effect;
- after any stream event, surface the typed error and terminalize; do not duplicate text/tool calls.

The Anthropic adapter retains `retry_policy: Never`; this keeps retry ownership singular and testable (`crates/haider-provider/src/anthropic.rs:22-41`).

Add an explicit response-open deadline in addition to connect/chunk-idle deadlines. Replace detached provider decoder/script tasks with an owned stream guard whose drop aborts/joins the task, or select the producer on receiver closure so cancellation does not leave a decoder blocked until the 90-second idle timeout. `WorkerManager` must be able to prove all provider tasks joined at drain.

W3c session configuration makes provider/model/max-tokens configurable. Keep the production Anthropic URL and version adapter-owned; keep the endpoint override test-only. Expose timeout/retry bounds through `DaemonConfig` only if needed operationally. There is no W3c OpenAI promise: unknown providers return a typed unsupported/invalid argument rather than falling back.

### R7 — Add correlated request methods under wire v1 and advertise method features (DECIDED)

**Decision.** Extend `RequestBody`/`ResponseBody`; do not add a second event cursor, uncorrelated mutation notification, or raw append frame.

Exact additions:

```text
Request session.create                         requires Control
  command_id: CommandId
  cwd: String
  provider: String
  model: String
  max_tokens: u64

Response session.create
  session_id: SessionId
  created_seq: u64
  worker_generation: u64
  metadata: SessionMetadataV1

Request turn.submit                            requires Control + control attachment
  command_id: CommandId
  session_id: SessionId
  worker_generation: u64
  text: String
  attachments: Vec<AttachmentBlock>
  mode: DeliveryMode

Response turn.submit
  session_id: SessionId
  run_id: RunId
  accepted_seq: u64
  worker_generation: u64
  disposition: started | queued | steer_pending

Request turn.cancel                            requires Control + control attachment
  command_id: CommandId
  session_id: SessionId
  worker_generation: u64
  run_id: RunId

Response turn.cancel
  session_id: SessionId
  run_id: RunId
  status: accepted | already_terminal
  terminal_seq: Option<u64>

Request vault.stage                            requires Control
  stage_id: String
  purpose: api_key | menu_secret
  secret: SecretWire

Response vault.stage
  stage_id: String
  vault_reference: String
  expires_at_ms: u64

Request account.login_api                      requires Control
  command_id: CommandId
  provider: String
  alias: Option<String>
  vault_reference: String
  validation_model: Option<String>

Response account.login_api
  descriptor: CredentialDescriptor

Request account.list                           requires View
  provider: Option<String>

Response account.list
  descriptors: Vec<CredentialDescriptor>
```

`turn.submit` and `turn.cancel` require a control attachment to match existing menu policy; `session.create` cannot require an attachment because the session does not exist yet. `vault.stage` and account login are harness/profile operations and require connection-level Control, not a session attachment. `session.list/read/attach` retain View rules.

Use the existing RPC codes where they fit (`invalid_argument`, `not_found`, `stale_generation`, `draining`, `overloaded`, `capability_denied`) and add stable string constants mirroring the already-stable domain taxonomy for `unauthorized`, `permission_denied`, `credential_missing`, `run_not_active`, `busy`, and `provider_error` (`crates/haider-protocol/src/error.rs:15-47`). During validation, 401 is non-retryable `unauthorized`; 403 is non-retryable `permission_denied` because the key authenticated but cannot use the selected model/endpoint; 429/529/5xx/transport is retryable `provider_error`. Human messages are never load-bearing.

`vault.stage` is intentionally non-durable; it must not write a command receipt containing a secret. `stage_id` is an ephemeral client nonce used only to deduplicate a same-connection retry: the same ID and bytes returns the same reference, while the same ID with different bytes is invalid. The client retains one zeroizing copy until the stage response, then wipes it; disconnect before acknowledgement wipes it and requires re-entry. References are random, connection- and daemon-instance-scoped, single-use, and expire after five minutes. Disconnect or drain wipes all staged secrets.

W3c exposes this method only on an authenticated, same-UID local UDS connection—the daemon already verifies peer UID before serving it (`crates/haider-daemon/src/connection.rs:767-786`). `Control` alone must not make raw-secret staging available to a future WebSocket transport. `SecretWire` belongs only to the transport crate, has redacted `Debug`, and must never be converted through a loggable `serde_json::Value`; domain `haider-protocol` remains secret-free.

Redacted types alone are insufficient because today's generic codecs serialize the JSON body into an ordinary `Vec<u8>`, copy it into another framed `Vec<u8>`, and retain inbound JSON in an ordinary decoder buffer (`crates/haider-rpc/src/codec.rs:83-152`, `crates/haider-rpc/src/uds_codec.rs:18-36`, `crates/haider-rpc/src/uds_codec.rs:137-173`). W3c needs a sensitive UDS encode/write path (or zeroizing body and framed buffers) that zeroes outbound bytes after the write and inbound bytes after deserialize. Stage frames are excluded from debug/capture/golden-body logging; tests assert that ordinary frame formatting never reveals them.

For `account.login_api`, command identity covers the semantic provider/resolved-model/alias operation and deliberately excludes the ephemeral vault-reference token. `validation_model: None` means the release-owned full model ID in the resolved profile, so bare `/login anthropic api` needs no hidden TUI model choice. A lost-response retry may supply a newly staged reference with the same command ID and still recover the original committed account result; changing provider/resolved-model/alias with that ID is rejected.

Add to `Welcome`:

```text
features: BTreeSet<String>   // serde(default), skip if empty
```

W3c daemon advertises at least:

```text
session_mutation_v1
turn_control_v1
account_login_api_v1
vault_stage_v1
```

Capabilities answer “may this connection control?”; features answer “does this daemon implement this additive method?” Do not infer method compatibility from package-version string. Adding the defaulted/skipped field is wire-v1 additive and keeps old fixtures byte-identical when empty. Older readers ignore it; a new client connected to an old v1 daemon reports “running daemon is too old for live turns/login” rather than sending an unknown method and spawning a second daemon.

The existing `session.list` and `session.read` response structs also gain the optional session metadata defined in R2. No existing field changes type or meaning.

Every new request/response pair receives:

- compact WS and UDS golden entries;
- older-reader unknown-method tests;
- unknown-additive-field tests;
- a same-command/different-body rejection fixture;
- a named mutation-check comment with the exact production mutation and expected failure.

Do not await a network validator or Keychain UI in the connection read loop. It currently awaits each `HubConnection::request` inline, which blocks subsequent Ping, MenuAnswer, and drain observation (`crates/haider-daemon/src/connection.rs:835-904`, `crates/haider-daemon/src/connection.rs:1118-1131`). Short session commands may stay inline. `account.login_api` atomically claims/transfers the stage to the bounded daemon account actor, which owns the long operation and later sends the correlated response through the normal sink. The connection remains readable; disconnect drops only the response route, not an already durable login command, and a retry recovers its receipt.

### R8 — Auto-start by connect, spawn, and handshake-poll; the lock elects the winner (DECIDED)

**Decision.** Add one shared `ResolvedProfile` used by `haider` and `haiderd`:

```text
profile_id
store_dir
runtime_dir
endpoint_path
default_provider
default_model
default_max_tokens
```

Preserve `HAIDER_PROFILE_DIR` and the current default store directory. Resolve it to an absolute path, create it if absent, then canonicalize it; `profile_id` is the lowercase BLAKE3 hex digest of a version tag plus the canonical store-path bytes. Both processes use that exact shared function.

For W3c, do not accept `HAIDER_RUNTIME_DIR`. On Linux, use a verified owner-private `XDG_RUNTIME_DIR/haider` when available; otherwise, and on macOS, use the short constant base `/tmp` plus the child `haider-<effective-uid>`. The endpoint implementation verifies the final child is a real, non-symlink directory owned by the current UID and forces `0700` (`crates/haider-daemon/src/endpoint.rs:338-374`). The socket remains the fixed-length profile hash under that child. This satisfies D1's short private-directory rule and prevents an environment override from making `create_dir_all`/`fchmod` target a broad directory (`docs/research/d1-daemon-research-report.md:729-731`, `docs/research/d1-daemon-research-report.md:757-762`). A future override requires a separately reviewed containment rule.

The W3c defaults are provider `anthropic` and 4,096 max output tokens. `default_model` is a release-owned full Anthropic model ID from profile config or `HAIDER_MODEL`; the packaged clean-profile value must be chosen and verified by the ignored live API smoke before release. The report deliberately does not promote the product label `fable-5` or the adapter's capability-prefix examples into an API promise. Both processes call the same resolver—no duplicated path logic. The full resolved ID, never TUI `model_short`, enters `session.create` and login validation.

Bare `haider` algorithm:

1. Resolve profile and deterministic endpoint.
2. Try UDS connect and `Hello`/`Welcome`.
3. If ready and required features are present, launch `LiveDriver`.
4. On profile mismatch, no version overlap, permission error, or malformed handshake: fail; do not spawn or unlink.
5. Only on `NotFound` or `ConnectionRefused`, spawn sibling `haiderd` with exact profile/store/runtime arguments.
6. Detach it from terminal I/O: stdin null, stdout/stderr to an owner-only profile daemon log, separate process group where supported, `kill_on_drop(false)`.
7. Poll connect plus handshake with bounded backoff until a configurable 30-second startup deadline. Observe child early exit for diagnostics, but an exit 75 is an expected race loser while polling.
8. Attach to whichever daemon wins the store lock.

The sibling executable next to `current_exe()` is the packaging authority; a PATH fallback may be diagnostic convenience, not silent execution of an arbitrary incompatible binary.

Two simultaneous terminals can both reach step 5. Both may spawn. Exactly one child acquires the store lock; the other exits 75. Both parent CLIs continue polling the same endpoint and attach to the winner. No client takes the profile lock and no client removes a socket.

Stale endpoint recovery remains exclusively inside the lock-winning daemon. The endpoint code already proves the required conservative claim/re-probe/unlink behavior (`crates/haider-daemon/src/endpoint.rs:377-517`).

**Version skew.**

- wire overlap plus required advertised features: attach; differing `0.0.x` package strings produce at most a diagnostic;
- wire overlap but missing W3c feature: fail with “stop/upgrade the running daemon”; never kill it automatically;
- no wire overlap: fatal protocol mismatch; never spawn a competing daemon because the singleton is already serving active state.

**Shutdown policy.** W3c daemon lingers indefinitely after the last TUI detaches. A client exit never implies turn cancellation or daemon shutdown. Idle daemon shutdown is a later product policy requiring telemetry and pending-menu/queued-turn rules; it is not part of this keystone.

### R9 — Add worker-aware drain ordering and a real dead-peer policy (DECIDED)

The current drain closes hub admission before hub shutdown (`crates/haider-daemon/src/runtime.rs:260-306`). `SessionHub::actor_for` rejects every call once `draining` is set, even for an existing actor (`crates/haider-daemon/src/session_hub.rs:816-843`). That is correct without workers but would prevent a just-cancelled worker from appending its final `Cancelled`/effect outcome.

**Decision.** Introduce a separate external mutation/admission gate and order shutdown:

1. stop accepting sockets;
2. atomically close all external `HubConnection` request/menu admission and `WorkerManager` new-turn admission; `Ping` may remain serviceable, and already-admitted attachment delivery may drain, but no existing connection may list/read/attach/detach/mutate or answer a menu after the gate;
3. cooperatively cancel/settle workers and close effect brokers while the hub still accepts already-owned worker appends;
4. under the same global deadline, call `hub.begin_draining()` and `hub.shutdown()` so all queued appends/CAS persist and publish;
5. issue `ServerDraining` after final envelope frames are queued, preserving current W3b ordering;
6. drain writers, flush/close store, remove the exact owned endpoint, release the profile lock last.

If the deadline wins, report forced shutdown and let next-generation recovery terminalize honest state. Do not reopen hub admission to improve graceful statistics.

The W3c client heartbeat policy should be:

- client sends `Ping` every 15 seconds while connected, including quiescence;
- server closes after 45 seconds with no Ping/read activity, covering a silent-but-open peer;
- each Ping queues Pong through normal bounded/fair accounting; no Pong/write progress for 45 seconds closes the connection, covering a peer that writes but never reads;
- client reconnects/reattaches after 45 seconds without a matching Pong;
- tests use paused Tokio time.

This activates the explicitly ledgered quiescent-stuck-client trigger (`docs/OPTIMIZATIONS.md:168-173`). The constants are initial policy, not eternal truth; instrument no-progress detach and tune from W3c traces.

### R10 — Implement `/login anthropic api` as staged secret, validation, recoverable commit, and next-turn resolution (DECIDED)

**Decision.** End-to-end flow:

1. TUI parses `/login anthropic api` and opens a harness-level local `LoginCard`.
2. Card enters `SecretInput` mode. Keystrokes/paste live only in a transient zeroizing buffer. The screen renders bullets, never the value. The palette, history, flash text, selection-copy, and demo persistence cannot observe it.
3. Submit sends `vault.stage`; the TUI retains one zeroizing copy only through same-connection acknowledgement/retry, then wipes it. Disconnect before acknowledgement wipes it and requires re-entry.
4. Daemon places the secret in a connection-scoped in-memory stage and returns an opaque reference.
5. TUI sends idempotent `account.login_api` with a new durable `command_id`; the account actor atomically claims the pending receipt and consumes the stage into command-owned zeroizing memory.
6. Account manager chooses a stable physical vault alias derived from `profile_id + command_id`; an optional user alias is display identity only. Its canonical `request_json` stores provider/resolved-model/display-alias/physical-alias but excludes the ephemeral vault reference and all secret material.
7. A provider-specific `CredentialValidator` resolves the staged handle and performs a minimal Anthropic Messages request with `max_tokens = 1` and `validation_model`, or the resolved-profile default when it is absent. It consumes enough of the stream to prove successful authentication and then cancels/drains safely.
8. 401 is an invalid key; 403 is a valid identity lacking permission for the selected model/endpoint; 429/529/5xx/transport is “validation unavailable” and retryable. Nothing persistent is written on a definitive validation failure.
9. On success, write Keychain first, add/select the descriptor through the single daemon-owned `AccountStore`, fsync the descriptor directory after atomic rename, then finalize the command receipt. If descriptor save fails synchronously, delete the just-written vault alias.
10. On restart, reconcile pending *and committed* login receipts: vault-only resumes descriptor commit; neither waits for the same command with a fresh stage; descriptor+vault finalizes/validates the committed response. Receipt metadata never contains the secret.
11. Response closes the card with a success/error state. Errors carry stable code and retryability, never provider body/key text.
12. The next logical turn's `ProviderFactory` reads the now-active descriptor and uses it. An in-flight turn remains pinned to its existing provider/account.

The one-token Messages request is chosen because it uses the already-audited adapter path; W3c should not invent an undocumented authentication endpoint. It has a tiny cost and should say “validating…” in the UI. Tests inject a fake validator and never use the network.

Once `account.login_api` claims a stage, the command—not the connection—owns the secret for a bounded five-minute pending-command TTL, and the account actor may finish after client disconnect. A retryable validation result keeps the pending command/secret until that TTL so the same command can retry without retyping; expiry or daemon restart wipes it and returns an explicit `restage_required` recovery action. A 401/403 or success wipes it immediately. Drain stops new account commands and joins/cancels the account actor under the same global deadline.

**Platform decision.** W3c's `/login anthropic api` release gate is macOS, the only platform with a working vault today. On non-macOS, the daemon rejects the command before staging/validation with stable `vault_unsupported`, never the current generic internal message. A real Linux secret-service backend is explicitly ledgered for the first Linux-supported release; storing plaintext or silently falling back to an environment variable is rejected.

`accounts.json` and Keychain cannot participate in one SQLite transaction. The pending receipt is therefore the recovery protocol, not a claim of impossible cross-store atomicity. One daemon account actor plus the profile lifetime lock satisfies the current JSON store's single-writer assumption (`crates/haider-accounts/src/store.rs:98-110`).

Before relying on that protocol, harden `JsonFileStore::save` to open and `sync_all` the parent directory after rename. Also namespace every physical Keychain account key by the resolved profile identity; current service-plus-alias lookup is global while descriptors are profile-local (`crates/haider-accounts/src/keychain.rs:17-18`, `crates/haider-accounts/src/keychain.rs:89-97`). Cross-profile login tests must prove the same display alias cannot overwrite or resolve another profile's secret.

`MenuKind::Secret` should reuse `vault.stage` for future provider-requested secrets, returning only `MenuInput::SecretVaultReference`. Login itself remains a harness-level account card, not a fake session menu; it can run before any session exists.

### R11 — Cut the TUI over in identity, reducer, demo boundary, then driver order (DECIDED)

**Decision and order.**

**Cut 1: identity.** Replace TUI session keys with protocol `SessionId` (or a transparent `UiSessionId(SessionId)`) everywhere: session map, active/last detached, hit targets, outbox, and persistence DTO. Demo seeds use stable string IDs such as `demo-session-1`. Do not repurpose a session ID as the stale-timer epoch; add a separate local monotonically increasing `UiGeneration(u64)` for demo arms and asynchronous response guards.

The retained demo store needs a real upcaster: its v1 `SessionDto.id` is `u64` and hydration scans those numeric IDs (`w5-tui4@906636fd:crates/haider-tui/src/demo_store.rs:212-230`, `w5-tui4@906636fd:crates/haider-tui/src/demo_store.rs:482-505`). Decode a legacy untagged numeric-or-string ID, map `n` to `demo-session-{n}`, bump the demo-store schema version, and rewrite on the next save. A v1 fixture must hydrate without silently reseeding.

**Cut 2: envelope vocabulary.** Introduce one `absorb_raw(&RawEnvelope)` route for active and background sessions. It:

- validates frame session ID;
- drops `seq <= last_applied`;
- on `seq > last_applied + 1`, stops applying and asks `LiveDriver` to reattach after `last_applied`;
- applies typed head projection;
- maps `AgentSpawned` to chip creation using manifest ID/parent/callsign/model;
- maps `AgentChipState` to chip state;
- maps `AgentReport` only to report summary/verification content; `AgentChipState` remains the sole chip-state authority (`crates/haider-protocol/src/agent.rs:60-93`);
- records `MenuOpened` ownership in a menu-ID-to-agent/scope map, then uses that map to route `MenuAnswered`/`MenuClosed`.

Do not continue after a gap as current `SessionProjection::apply_raw` does (`w5-tui4@906636fd:crates/haider-tui/src/projection.rs:195-217`). The client cursor advances only after the reducer fully applies the envelope.

**Cut 3: demo-only vocabulary.** Move `PurgeDemoStore` out of common `AppRequest` into a `DemoRequest` consumed only by `DemoDriver`/`run_demo`; likewise keep demo frame/quit saves, meter priming, arm ownership, and `DemoEvent` chip variants there. The common `AppModel` emits semantic requests; live reset must never delete demo persistence, and demo reset must never send a profile mutation. This intentionally overrides seam-report item 7's deletion of `demo_store.rs` while preserving its architectural diagnosis.

**Cut 4: driver.** Add `run_live(AppModel, LiveDriver)` or a generic runtime over a driver trait while retaining `run_demo`. `LiveDriver` owns:

- one `haider-rpc` client task;
- pending request map keyed by `RequestId`;
- durable command ID generation for mutations;
- per-session attachment and last-applied cursor;
- an attachment working set capped below the server's 16-per-connection limit: active session first, then visible/running/pending-menu sessions, with cold sessions represented by list/read metadata and LRU detach before a new attach (`crates/haider-daemon/src/session_hub.rs:135-152`);
- reconnect/reattach;
- response-to-model actions;
- bounded outbound and inbound channels.

Entering live mode transitions boot on ready `Welcome`, lists sessions, and attaches only on selection. Launcher text follows:

```text
session.create response
session.attach response
turn.submit response OR UserMessage event
the other may follow
```

No locally fabricated row or numeric session exists. The existing gate proves response-before-event only for `session.attach`, not for turn submission (`crates/haider-daemon/src/session_hub.rs:1544-1568`). Because the session actor publishes accepted envelopes before `HubConnection` can necessarily write the submit response, `LiveDriver` must tolerate either order: the pending command is correlated independently, while the authoritative `UserMessage` envelope renders/deduplicates the row. Do not add a second submit gate merely for presentation order.

Menu answers in live mode are built from the committed opening envelope: `command_id`, protocol session ID, menu ID, `request_seq`, `worker_generation`, key/index, and optional vault reference. Current epoch-only `OutboundAnswer` is demo-only.

**Rejected alternative: replace `DemoDriver` in place.** It would delete a shipped mode, entangle live reconnect with scripted arms, and make every TUI regression require a daemon.

**Rejected alternative: keep a numeric-to-string map at the RPC boundary.** IDs occur in session state, hits, timers, persistence, and answer origins; a boundary map would preserve two authorities and recreate stale-answer bugs.

### R12 — Make optimization-ledger triggers explicit, and do not mix risky rewrites into the seam (DECIDED)

W3c activates these ledger rows now:

- split `session_hub.rs` first into `session_hub/{mod,actor,replay,rpc}.rs` (`docs/OPTIMIZATIONS.md:174`);
- structurally close the append-discipline gap by handing workers only the hub (`docs/OPTIMIZATIONS.md:166`);
- expose `SessionHubConfig` through `DaemonConfig`/shared CLI config (`docs/OPTIMIZATIONS.md:175`);
- extract shared daemon UDS test helpers when adding the W3c test file (`docs/OPTIMIZATIONS.md:178`);
- add catch-up/resume/lag/outbox-detach counters (`docs/OPTIMIZATIONS.md:160`);
- measure store queue/hold time, without changing write/CAS serialization (`docs/OPTIMIZATIONS.md:164`);
- implement the quiescent dead-peer deadline (`docs/OPTIMIZATIONS.md:172`);
- add `/model`, `/provider`, `/login`, `/account`, and `/queue` argument slots, of which W3c must make `/login` executable (`docs/OPTIMIZATIONS.md:30`);
- evaluate per-block and wrapped-height caches on real long-session traces; row 14's session-attach trigger has arrived, while row 17 still uses its stated >2–3k rows or p95 >8–10 ms threshold (`docs/OPTIMIZATIONS.md:14-17`).

Add a new LATER row for idle daemon shutdown. Trigger review only after W3c records last-client-detach duration plus queued-run, active-run, and pending-menu occupancy; the policy must never infer cancellation from client absence. W3c deliberately lingers indefinitely.

Do not in the same lane:

- redesign store execution because `spawn_blocking` is uncancellable, unless measured close contention triggers it (`docs/OPTIMIZATIONS.md:173`);
- add session-actor retirement before real working-set data (`docs/OPTIMIZATIONS.md:165`);
- rewrite deep-clone publication, store paging, menu indexing, or fair scheduling riders 4–10 without their measurement triggers (`docs/OPTIMIZATIONS.md:155-164`);
- fuse a second server-side attach entry unless one is actually introduced. `LiveDriver` uses the existing RPC attach path, so the register→replay seam remains single-call-site (`docs/OPTIMIZATIONS.md:177`).

## 4. Orchestration flows

### 4.1 New session and first turn

```text
TUI                  HubConnection       SessionHub/Profile store       WorkerManager
 | session.create(cmd)      |                       |                         |
 |------------------------->|  Control + digest     |                         |
 |                          |---------------------->| create metadata +       |
 |                          |                       | Created + receipt        |
 |<-------------------------| session_id, seq=1     |                         |
 | session.attach(id, 0)    |                       |                         |
 |------------------------->| register + capture H  |                         |
 |<-------------------------| attach response       |                         |
 |<-------------------------| Event seq=1           |                         |
 |<-------------------------| AttachCaughtUp(H=1)   |                         |
 | turn.submit(cmd, gen)    |                       |                         |
 |------------------------->| Control attachment    |                         |
 |                          |---------------------->| receipt + Queued +       |
 |                          |                       | UserMessage, publish     |
 |                          |-----------------------------------------------> | enqueue run
 |<-------------------------| submit response       |                         |
 |<-------------------------| queued/user events    |                         |
 |                          |                       |<-------------------------| hub StoreHandle
 |<-------------------------| thinking/stream/items/usage/done events         |
```

Response/event relative order for `turn.submit` need not promise response-before-event the way attach does; the acceptance transaction is authoritative and the client reducer is idempotent. For simpler UI correlation, the hub may enqueue the response before publishing that transaction's events, but correctness must not depend on socket order. The durable `command_id` and `accepted_seq` close the ambiguity.

### 4.2 `request_input` round trip

```text
core worker -> hub StoreHandle -> MenuOpened + InputRequired durable/published
TUI reducer stores menu opening seq + worker generation
TUI -> MenuAnswer(command_id, session, menu, request_seq, generation)
HubConnection checks Control attachment
session actor -> Store::resolve_menu transaction
first committed answer wins and is published
session actor -> HarnessHandle::apply_committed_menu_event
core validates committed answer, emits ToolResult, resumes same pinned provider turn
```

There is no direct `HarnessHandle::answer_menu` call on the daemon RPC path. The durable CAS remains the sole authority (`crates/haider-daemon/src/session_hub.rs:1599-1729`).

### 4.3 Crash and restart

```text
old process dies
OS releases lifetime store lock
new daemon acquires lock and advances generations
replay all sessions
reconcile dispatched effects to Unknown
reconstruct parked request_input checkpoints without closing their menus
terminalize other old active/cancelling runs and close their open UI lifecycles
requeue only runs proven never to have left Queued
construct hub + worker manager
bind endpoint
advertise Ready
clients reattach from last fully applied seq
```

Store replay guarantees all committed envelopes in contiguous sequence order. It does not restore an HTTP response body not yet committed, provider TCP/SSE state, process memory, an uncommitted tool result, or knowledge of an external side effect beyond the effect journal. The design deliberately recovers view and safe intent, not an unknowable continuation.

## 5. Risk register

| Existing law/invariant | Naive W3c mistake | Required guard/test |
|---|---|---|
| Single profile writer | standalone resolves/imports accounts before its store lock, or bare CLI opens SQLite | all live paths use RPC; explicit standalone acquires the profile lock before any account/store access |
| INV-1 persist before publish | worker broadcasts core events directly or response claims acceptance before commit | worker receives hub committer only; transaction completes before response/start |
| INV-2 receiver + `H` atomic | `LiveDriver` or worker manager adds a second read-then-subscribe path | every live client uses existing `session.attach`; boundary interleaving tests remain |
| Sole replay cursor | TUI persists demo epoch/frame count beside `RawEnvelope.seq` | cursor map contains only greatest fully applied `seq` per session/attachment |
| At-least-once reducer | TUI continues applying after a gap, as current projection does | stop attachment on gap, reattach after last applied; duplicate/gap mutation tests |
| Store is lag buffer | unbounded worker/TUI channel buffers replay or provider deltas | bounded supervisor/client channels; store resume and typed overload |
| Unknown attachment ID | live driver accepts an event before attach response | retain hub response gate; client rejects unknown attachment ID |
| Atomic/fair sink admission | heartbeat or W3c response bypasses lane accounting | use existing priority/system lane with byte charge; no direct socket writes |
| Attachment caps | reconnect attaches every listed/materialized session or leaks old ownership | active/running/pending-menu working set, LRU detach, cold list/read, reconnect soak beyond 16 sessions |
| Menu first-commit wins | RPC calls `HarnessHandle::answer_menu` or appends `MenuAnswered` directly | only `Store::resolve_menu` CAS, then wake committed event |
| Pending menus survive | restart closes a parked `request_input` because its waiter vanished | preserve/reconstruct checkpoint; validate opening coordinates across generation; never repeat preceding provider/effect |
| Durable command idempotency | retry after lost `turn.submit` response starts Anthropic twice | receipt+acceptance transaction; same/different-body tests |
| Generation fencing | old supervisor appends after same-process replacement or receives cancel | persisted generation for restart plus hub-issued active worker lease for append/register/unregister/cancel |
| Drain barrier | set hub draining before workers append cancellation/effect terminals, or leave existing external connections admitted | close all external request/menu admission, settle workers on their internal append lane, then hub drain under one deadline |
| Admission discipline | worker starts provider before queue/receipt is durable | supervisor only receives a committed accepted-run record |
| Honest crash recovery | automatically retries an active provider request or tool | only Queued and a proven parked-input continuation resume; other active work becomes Errored; effect Unknown scan first |
| Item lifecycle | crash/cancel leaves open text/tool items forever or marks cancelled tools Failed | recovery/cancel closes each item with correct terminal status before run terminal |
| Provider retry safety | retry a stream after text/tool events were committed | per-request `emitted_any`/`effect_dispatched` fence; pre-first-event retries only |
| Provider/account pinning | resolver changes account between tool-loop requests | resolve once per logical turn and retain alias in usage |
| Provider continuation | replay normalized reasoning as if it were signed Anthropic thinking | omit reasoning from follow-up messages until provider-opaque signed state exists |
| Usage truth | replace a multi-request turn total with only the last request | emit checked cumulative logical-turn snapshots; two-request/overflow tests |
| Secrets never durable/logged | derive `Debug` on raw key, journal login card, persist composer, copy it through ordinary codec buffers, or include key in golden | same-UID UDS-only stage, sensitive zeroizing codec path, redacted `SecretWire`, local masked buffer, recursive leak scan |
| Durable failure truth | reduce a failed real turn to unit `Errored`, losing the typed cause after RPC acceptance | sanitized bounded `RunFailed` immediately before `Errored`; fake-provider E2E and unknown-payload golden |
| Account single writer | CLI and daemon both rewrite fixed `accounts.json.tmp` | daemon account actor owns mutations; standalone takes the profile lock before bootstrap |
| Cross-store account recovery | crash after Keychain put/descriptor rename leaves irreconcilable or falsely committed state | parent-directory fsync; pending+committed secret-free receipt reconciliation |
| Vault namespace | same human alias in two profiles overwrites one global Keychain account | physical alias includes profile identity; cross-profile isolation test |
| Workspace boundary | trust client cwd string or hand worker an arbitrary path | canonicalize during session create; broker opens descriptor with `NOFOLLOW` |
| Task ownership | use detached `HarnessActor::spawn`/provider decoder tasks past daemon close | manager JoinSet, explicit stop, abort-on-drop/receiver-closed provider producer, bounded join |
| Process kill discipline | turn cancel drops child without group sweep | route through `ProcessExecution::cancel` and `EffectBroker::close` |
| Quiescent availability | a silent peer or write-only peer occupies attachment slots forever | 15 s Ping, 45 s read/Pong-write deadlines, client missing-Pong reconnect, paused-time tests |
| Version skew | new CLI kills/replaces an old but active v1 daemon | feature advertisement; explicit upgrade error; never auto-kill incumbent |
| Demo is shipped | live cutover deletes `DemoDriver`/DemoStore and golden behavior | separate `run_demo` and `run_live`; both modes in CI |
| Opaque identity | preserve numeric IDs behind a lossy mapping | string `SessionId` end-to-end; separate local UI generation |
| Platform credential support | claim login support where `KeychainVault` always errors | macOS release gate; named `vault_unsupported`; ledger a real Linux backend |

## 6. Chunk plan and release gates

### 6.1 W3c1 — durable orchestration and wire

**Dependency reason.** CLI and TUI cannot attach to work that the daemon cannot create or run. This chunk must establish semantic commands and a fake-provider-complete daemon before process auto-start or UI concerns obscure failures.

**First mechanical commit.**

- Split `session_hub.rs` into `session_hub/{mod,actor,replay,rpc}.rs` without behavior changes.
- Extract shared daemon/daemond UDS test support.
- Wire `SessionHubConfig` through `DaemonConfig`.

**Implementation.**

- wire-v1 method variants and `Welcome.features`;
- session metadata and command-receipt migration/APIs;
- `session.create`, `turn.submit`, `turn.cancel`;
- external admission gate and worker-aware drain;
- `WorkerManager`/per-session supervisor with owned joins;
- persisted-generation plus active-worker-lease fencing and token-aware harness registration;
- turn-scoped provider factory;
- branch/agent-scoped prompt-history compiler and versioned system-prompt builder;
- general tool dispatcher, hub journal adapter, daemon CAS adapter;
- reasoning-safe Anthropic continuation and cumulative logical-turn usage;
- durable `RunFailed` payload and aggregate `SessionState` ownership;
- retry owner;
- interrupted-run recovery;
- daemon provider/accounts/tools production dependencies and injectable test factories.

**Primary gate: real daemon + real UDS + real core + fake provider.**

One `haider-daemond/tests/live_turn_rpc_tests.rs` should:

1. start the production runtime with an injected `FakeProvider` factory;
2. open a real `UnixStream`, handshake with Control, create and attach a session;
3. submit a turn and assert contiguous `Created`, `Queued`, `UserMessage`, `Thinking`, `Streaming`, item delta/completion, Usage, and Done;
4. retry the same submit command after dropping its first response and assert one run/user message/provider request;
5. submit a second turn and inspect `FakeProvider.requests()` to prove prior completed conversation is present;
6. script `request_input`, answer from a second control attachment using the committed coordinates, and prove one menu resolution, a tool result in the next provider request, and Done;
7. race two answers and assert first committed wins;
8. script a hanging provider, cancel over wire, and assert open items close and no post-Cancelled event appears;
9. kill/restart around Queued versus Streaming and assert only Queued resumes;
10. restart while parked on `request_input`, answer the replayed menu, and assert the preceding provider request/effect is not repeated and only the next request runs;
11. hold an effect dispatch across restart and assert `Unknown`, never redispatch;
12. run a reasoning-plus-tool two-request script and assert no normalized reasoning replay, cumulative usage, and one `RunFailed` before `Errored` on a typed failure;
13. mutate each production seam named in its test comment and observe the stated failure.

No live API is involved.

### 6.2 W3c2 — reusable client, auto-spawn, and `/login`

**Dependency reason.** Auto-spawn must poll a handshake that advertises W3c methods, and login must update the provider factory used by W3c1 workers.

**Implementation.**

- shared `ResolvedProfile`;
- reusable UDS `RpcClient` in `haider-rpc` or a thin new client module, not in TUI;
- pending request correlation, bounded writer/reader, ping, reconnect primitives;
- bare `haider` connect/spawn/poll;
- sibling `haiderd` discovery and owner-only log;
- feature/version-skew diagnostics;
- daemon account actor;
- sensitive same-UID UDS stage codec and bounded command-owned secret lifetime;
- `vault.stage`, `account.login_api`, `account.list`;
- fake-injectable `CredentialValidator`;
- parent-directory-fsynced, profile-namespaced account persistence;
- pending/committed-login receipt reconciliation;
- release-owned full Anthropic model configuration and ignored live smoke.

**Tests.**

- two subprocess CLIs start simultaneously: one daemon winner, one child exit 75, both parents complete Ready handshake;
- stale owner socket is recovered only by the winner;
- live but old/missing-feature daemon is not killed or replaced;
- a no-overlap daemon yields protocol mismatch;
- process parent exits while daemon remains;
- fake validator success writes a `MemoryVault`/temp descriptor and the next fake turn observes the selected alias;
- 401 versus 403, retryable validation/restage, descriptor-save/fsync failure, and each crash boundary reconcile correctly;
- identical display aliases in two profiles resolve distinct secrets;
- a unique sentinel API key is absent from events, receipts, descriptor JSON, daemon log, formatted frames/errors, and TUI/demo snapshots;
- ping/no-progress policy uses paused time.

### 6.3 W3c3 — TUI live swap with demo retained

**Dependency reason.** The TUI should consume a stable real client and wire, not define daemon semantics while rendering them.

**Implementation.**

- `SessionId` migration plus separate `UiGeneration`;
- raw-envelope session router and strict gap behavior;
- capped active/running/pending-menu attachment working set with cold list/read;
- agent-event chip projection;
- live response/action model;
- `LiveDriver` reconnect/reattach and command outbox;
- live launcher create/attach/submit order;
- live menu coordinates;
- `/login` argument slots, masked card, stage/login result handling;
- demo-only `DemoEvent`, persistence, reset, arm, meter, and answer echo remain under `run_demo`;
- demo-store v1 numeric-ID upcaster and v2 string-ID rewrite;
- bare `haider` and `haider tui` enter live mode; `haider tui --demo` remains deterministic.

**Tests.**

- reducer duplicate is a no-op; gap stops and emits reattach request before later state mutates;
- reconnect restores the bounded priority working set after its last applied cursors, LRU-detaches before the 17th attach, and leaves cold sessions listable/readable;
- background-session envelopes route by opaque session ID;
- agent spawn/state/report populate nested chips;
- attach response precedes first event and unknown attachment IDs are rejected;
- launcher does not create a row/session until daemon responses/events arrive;
- menu answer includes exact opening sequence/generation and same-command retry;
- secret typing, paste, redraw, copy, error, quit, and panic-safe teardown never reveal the key;
- all existing demo snapshots/goldens continue to pass under `haider tui --demo`;
- a persisted v1 numeric-ID demo fixture upcasts without reseeding and rewrites as v2 strings;
- render benchmark records p95 on 1k/3k/5k-row replays and activates the ledgered cache only at the stated threshold.

### 6.4 Final acceptance matrix

W3c is complete only when all are true:

- `haider` from a clean profile leaves one detached `haiderd` and enters the live TUI;
- a second terminal attaches to the same session and sees contiguous live events;
- on the W3c-supported macOS credential path, a real Anthropic turn can run after `/login anthropic api`;
- the same path passes deterministically with `FakeProvider` and no network;
- menu answers from either control attachment resume the one worker exactly once;
- turn cancellation kills supervised processes and reaches one terminal state;
- daemon restart never repeats ambiguous work, resumes never-started queued runs, and preserves/reconstructs pending `request_input`;
- a lost mutation response does not duplicate session, turn, cancel, or login;
- old daemon/version-feature mismatch is explicit and non-destructive;
- `haider tui --demo` remains runnable and regression-pinned;
- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass in a socket-capable environment;
- ignored live-Anthropic smoke is optional evidence only, never the merge gate.

## RECOMMENDATIONS

R1. **DECIDED:** add a daemon `WorkerManager` with one owned, bounded supervisor per active session; never await provider/tool work in the session hub actor.

R2. **DECIDED:** add typed session metadata and generic durable command receipts; create a session in one transaction with its first `Created` envelope.

R3. **DECIDED:** make `turn.submit` atomically durable before provider work and expose no raw envelope-append RPC.

R4. **DECIDED:** compile branch/agent-scoped committed history, build a versioned system prompt, and wire the shipped effect/tool broker through hub-owned journal/CAS adapters.

R5. **DECIDED:** make cancellation intent durable, use generation plus active-worker leases, terminalize non-resumable interrupted runs, and reconstruct durable pending-input checkpoints without retrying prior work.

R6. **DECIDED:** resolve credentials/provider once per logical turn and retry only pre-first-event failures in the turn engine.

R7. **DECIDED:** add `session.create`, `turn.submit`, `turn.cancel`, `vault.stage`, `account.login_api`, and `account.list` as correlated additive wire-v1 methods, with `Welcome.features`.

R8. **DECIDED:** bare `haider` connects first, spawns detached only for missing/refused endpoint, and handshake-polls; the store lock elects concurrent-launch winners.

R9. **DECIDED:** separate external admission from internal worker completion during drain, then add 15-second Ping and 45-second read/Pong-write deadlines.

R10. **DECIDED:** implement API login as masked local input, same-UID sensitive staging, provider validation, profile-namespaced recoverable Keychain/descriptor commit, and next-turn resolver pickup; W3c's vault gate is macOS.

R11. **DECIDED:** migrate TUI identity first, then raw-envelope/chip reduction, then demo vocabulary isolation, then `LiveDriver`; retain `haider tui --demo`.

R12. **DECIDED:** execute the W3c-triggered structural/config/test/metrics ledger rows now and leave high-risk performance rewrites behind their measured thresholds.
