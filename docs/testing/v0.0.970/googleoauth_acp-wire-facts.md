# ACP wire facts (verified 2026-09-04 against the published v1 JSON schema)

Source: `https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/schema.json`
(v1, 246 KB) plus `docs/protocol/v1/transports.mdx`, `initialization.mdx`, `authentication.mdx`.
Every field name below was read out of that schema, not paraphrased.

## Transport (verbatim from transports.mdx)

- JSON-RPC 2.0, UTF-8.
- stdio: the CLIENT launches the agent as a subprocess; agent reads from `stdin`, writes to `stdout`.
- "Messages are delimited by newlines (`\n`), and **MUST NOT** contain embedded newlines."
- "The agent **MAY** write UTF-8 strings to its standard error (`stderr`) for logging purposes."
- "The agent **MUST NOT** write anything to its `stdout` that is not a valid ACP message."

So: newline-delimited JSON. NOT Content-Length framed.

## Methods (exhaustive, from the schema)

Agent-side (client -> agent): `initialize`, `authenticate`, `session/new`, `session/load`,
`session/resume`, `session/list`, `session/close`, `session/delete`, `session/prompt`,
`session/cancel`, `session/set_mode`, `session/set_config_option`.

Client-side (agent -> client): `session/update` (notification), `session/request_permission`,
`fs/read_text_file`, `fs/write_text_file`, `terminal/create`, `terminal/output`,
`terminal/wait_for_exit`, `terminal/kill`, `terminal/release`.

## Shapes

`ProtocolVersion` = **integer** (uint16), bumped only for breaking changes.

- `InitializeRequest`: required `protocolVersion`; optional `clientCapabilities`, `clientInfo`, `_meta`.
- `InitializeResponse`: required `protocolVersion`; optional `agentCapabilities`, `authMethods` (array),
  `agentInfo`, `_meta`.
- `ClientCapabilities`: `fs`, `terminal` (boolean), `session`, `auth`, `elicitation`, `_meta`.
- `AgentCapabilities`: `loadSession` (boolean), `promptCapabilities`, `mcpCapabilities`,
  `sessionCapabilities`, `auth`, `_meta`.
- `AuthMethod` is a discriminated union on `type`; **absent `type` means `agent`**.
  `AuthMethodAgent` = `{ id, name, description?, _meta? }`. `AuthMethodTerminal` = `{ type: "terminal", ... }`
  ("Client runs the configured agent program as a separate interactive process, without passing this
  method to `authenticate`").
- `AuthenticateRequest`: required `methodId`.
- `NewSessionRequest`: required `cwd`, `mcpServers`; optional `additionalDirectories`.
- `NewSessionResponse`: required `sessionId`; optional `modes`, `configOptions`.
- `PromptRequest`: required `sessionId`, `prompt` (array of `ContentBlock`).
- `ContentBlock` variants: `text`, `image`, `audio`, `resource_link`, `resource`.
- `PromptResponse`: required `stopReason`.
- `StopReason` enum: `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, `cancelled`.
- `SessionNotification` (`session/update` params): required `sessionId`, `update`.
- `SessionUpdate` discriminator `sessionUpdate`, exhaustive variant list:
  `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`,
  `tool_call_update`, `plan`, `available_commands_update`, `current_mode_update`,
  `config_option_update`, `session_info_update`, `usage_update`.
  - `agent_message_chunk` / `agent_thought_chunk` / `user_message_chunk` carry `ContentChunk`
    = `{ content: ContentBlock, messageId? }` ("A change in `messageId` indicates a new message").
  - `tool_call` carries `ToolCall` = `{ toolCallId, title, kind, status, content[], locations[], rawInput, rawOutput }`.
  - `ToolKind` enum: `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`,
    `switch_mode`, `other`.
  - `usage_update` carries `UsageUpdate` = `{ used, size, cost? }` — **"Tokens currently in context"**
    and "Total context window size in tokens". This is context-window occupancy, NOT subscription quota.
- `RequestPermissionRequest`: required `sessionId`, `toolCall`, `options`.
  `PermissionOption` = `{ optionId, name, kind }`; `PermissionOptionKind` includes `allow_once`,
  `allow_always`, (and the reject counterparts).
  `RequestPermissionResponse`: required `outcome`; outcome union includes `cancelled` — and the schema
  states: when the client sends `session/cancel`, it **MUST** respond to all pending
  `session/request_permission` requests with the `cancelled` outcome.

## Google's official agent — registry entry (verified 2026-09-04)

`https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json`

```
id: antigravity-acp   name: Google Antigravity   version: 1.1.1
authors: ["Google LLC"]   license: proprietary   license_url: https://antigravity.google/terms
distribution.binary:
  darwin-aarch64  archive .../releases/macos/agy-acp-server-agy_acp_server_1.1.1-darwin-arm64.zip   cmd ./agy_acp_server.par
  linux-x86_64    archive .../releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-x86_64.zip   cmd ./agy_acp_server.par  args ["--uid="]
  linux-aarch64   archive .../releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-arm64.zip    cmd ./agy_acp_server.par  args ["--uid="]
  windows-x86_64  archive .../releases/windows/agy-acp-server-agy_acp_server_1.1.1-windows-x86_64.zip cmd ./agy_acp_server.exe
  windows-aarch64 archive .../releases/windows/agy-acp-server-agy_acp_server_1.1.1-windows-arm64.zip  cmd ./agy_acp_server.exe
```

**The registry publishes NO checksum and NO archive size.** There is no `sha256`, no `digest`, no
`size` field anywhere in the entry. Archives are ZIP (not tar). There is no `darwin-x86_64` entry.

Archive sizes below were read from `Content-Length` on a `HEAD` to each URL on 2026-09-04
(`Last-Modified: Wed, 02 Sep 2026`), and are provenance evidence only — a size is not an integrity check:

| platform | bytes |
| --- | --- |
| darwin-aarch64 | 316014828 |
| linux-x86_64 | 681969407 |
| linux-aarch64 | 656572786 |
| windows-x86_64 | 468238392 |
| windows-aarch64 | 468521191 |

## Corrections and additions (second research pass + live probe, 2026-09-04)

### Live handshake against the REAL Google binary (run locally in an isolated sandbox)

Extracted `agy_acp_server.par` (darwin-arm64 1.1.1) was launched with only
`PATH/HOME/GEMINI_HOME/TMPDIR` set and sent one `initialize`. Verbatim result:

```
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,
"promptCapabilities":{"image":true,"audio":true,"embeddedContext":true},
"mcpCapabilities":{"http":true,"sse":true},"sessionCapabilities":{"list":{},"resume":{}},
"auth":{"logout":{}}},"authMethods":[
{"id":"oauth-personal","name":"Log in with Google","description":"Log in with your Google account"},
{"id":"oauth-business","name":"Log in with Gemini Enterprise","description":"..."},
{"id":"gemini-api-key","name":"Gemini API key","description":"..."},
{"id":"agent-platform","name":"Gemini Enterprise Agent Platform","description":"..."}],
"agentInfo":{"name":"antigravity-acp","title":"Google Antigravity","version":"agy_acp_server_1.1.1"}}}
```

- Cold start to first response: **14.75 s** (measured). Child RSS at handshake: **230,176 KB ~= 225 MiB**.
- Installed footprint (darwin-arm64, extracted): **906,608 KB ~= 885 MiB**, two Mach-O arm64 files.
- stderr is glog-formatted noise (`I0904 12:03:12.535072 ... main.py:80] Starting AGY ACP Server...`).
  It is NOT protocol and must never be parsed as such, only drained.
- The agent resolves its profile from **`$GEMINI_HOME`** (its own stderr says so) and writes
  `$GEMINI_HOME/antigravity-acp/settings.json` (and `acp_token.json`).
- **We must select `oauth-personal` and never fall back** to `oauth-business`, `gemini-api-key`
  (API key) or `agent-platform` (ADC/Vertex). All four are ACP `agent`-type methods.

### Registry / integrity

- The registry schema DOES define an optional per-target `sha256` (64 hex). Google's entry omits it;
  other agents (goose, amp-acp) populate it. There is no `size` field in the schema at all.
- `dl.google.com` returns no `x-goog-hash` header either.
- **Google gzips the archive in transit**, so a naive comparison of a pinned byte size against a
  gzip-accepting GET will mismatch. Hash and size-check the DECODED archive bytes.
- Linux (and only Linux) requires the extra argv `--uid=` (empty value).
- Registry platform keys are `darwin-aarch64 / linux-x86_64 / linux-aarch64 / windows-x86_64 /
  windows-aarch64`, while Google's FILENAMES say `arm64`. There is no `darwin-x86_64` build.

### The OAuth URL is printed OUTSIDE the JSON stream

Exact prefix: `Open the following link to authenticate the ACP server: ` followed by a
`https://accounts.google.com/o/oauth2/v2/auth?...` URL with a `http://127.0.0.1:<port>/` loopback
redirect. In **1.1.1 it appears on stderr**; earlier builds printed it on **stdout**, violating the
transport rule. Therefore:

- the stdout line reader MUST tolerate a non-JSON line instead of failing the connection;
- both streams are scanned for that exact prefix and a duplicate delivery is ignored;
- only the exact expected origin+path is accepted; the URL, its query, the code and any token
  material are never logged, journalled, or put in an error message.

### Child environment (precedent + our rule)

Strip before launch: `GEMINI_HOME`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`,
`GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_CLOUD_PROJECT`, `AGY_ACP_CCPA_PROJECT`,
`AGY_ACP_ENABLE_OAUTH`, `ANTIGRAVITY_HARNESS_PATH`, `BROWSER`.
Then set only: `GEMINI_HOME=<per-account profile dir>` (0700), `AGY_ACP_FORCE_FILE_STORAGE=1`
(forces file token storage so two accounts cannot collide on one OS-keychain entry),
`PYTHONUNBUFFERED=1`, plus `PATH`, `HOME`, `TMPDIR` and locale.

### Errors, models, quota

- Unauthenticated `session/new` returns exactly **`-32000` / `"Authentication required"`**.
  `-32002` = resource not found (e.g. "Session not found in the current GEMINI_HOME").
  `-32601` = method not found. `-32800` = request cancelled.
- Version negotiation has **no error path**: the agent answers with the newest version it supports
  and the client must inspect the echoed integer and close the connection if it cannot speak it.
  An ACP **v2** schema already exists (`authenticate` becomes `auth_login`), so pin `1` and check.
- `session/cancel` is a NOTIFICATION (no response).
- Models arrive from `session/new` (`availableModels` + a `model` select config option). Observed
  set: `gemini-3.8-flash-{high,medium,low}`, `gemini-3.7-flash-{high,medium,low}`,
  `gemini-3.6-flash-{high,medium,low}`, `gemini-pro-agent`, `gemini-3.1-pro-low`; agent-declared
  default `gemini-3.7-flash-high`. The list drifts server-side, and `gemini-pro-agent` is an
  irregular slug — never parse a slug structurally.
- **No structured quota/plan is exposed over ACP.** Antigravity never sends `usage_update`.
  Quota/subscription failures arrive as unstructured prose, and can appear inside a turn that
  still finishes with `end_turn` — a successful stop reason does not prove success.

---

## CORRECTION (2026-09-04, second pass): where the model catalog actually lives

An earlier instruction to this lane stated that `NewSessionResponse` carries
`models = { availableModels: [{ modelId, name, description? }], currentModelId }` and that a
`session/set_model` method exists. **That is not in any published ACP schema.** Verified by
downloading all three schemas directly:

- `https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v1/schema.json` (246,569 bytes)
- `https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v1/schema.unstable.json` (373,155 bytes)
- `https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v2/schema.json` (288,134 bytes)

(The directory listing comes from `https://api.github.com/repos/agentclientprotocol/agent-client-protocol/contents/schema`.
Note the schema is under `schema/v1/`, not `schema/`.)

In all three, the strings `models`, `availableModels`, `currentModelId`, `modelId` and
`session/set_model` are **absent**:

| schema | `NewSessionResponse` properties | `session/set_model` |
| --- | --- | --- |
| v1 stable | `sessionId`, `modes`, `configOptions`, `_meta` | absent |
| v1 unstable | `sessionId`, `modes`, `configOptions`, `_meta` | absent |
| v2 | `sessionId`, `configOptions`, `_meta` | absent |

`availableModes` / `currentModeId` *do* exist, but on `SessionModeState` (the `modes` block) — these
are session **modes**, not models, which is the likely source of the confusion.

### The real mechanism (v1 stable, which is what Antigravity speaks — it answered `protocolVersion: 1`)

The model catalog is a **session configuration option**:

- `NewSessionResponse.configOptions`: array of `SessionConfigOption`.
- `SessionConfigOption` = required `id` (`SessionConfigId`, a string) and `name`; optional
  `description`, `category`, `_meta`. It is a `oneOf` discriminated by a `type` field:
  - `type: "select"` -> `SessionConfigSelect` = required `currentValue` (`SessionConfigValueId`,
    a string) and `options` (`SessionConfigSelectOptions`).
  - `type: "boolean"` -> `SessionConfigBoolean` = required `currentValue` (bool).
- `SessionConfigSelectOptions` is an `anyOf`: either a **flat** array of `SessionConfigSelectOption`,
  or a **grouped** array of `SessionConfigSelectGroup` = required `group`, `name`, `options`.
- `SessionConfigSelectOption` = required `value` (`SessionConfigValueId`) and `name`; optional
  `description`, `_meta`.

**Identifying which option is the model selector** is spec-supported via
`SessionConfigOptionCategory`, whose reserved constants include `"mode"`, **`"model"`
("Model selector")** and `"model_config"`. The schema is explicit that this is a UX hint:

> "It MUST NOT be required for correctness. Clients MUST handle missing or unknown categories
> gracefully."

So the resolution order is: a `select` option with `category == "model"`, else a `select` option
whose `id == "model"` (an observed agent convention, not a spec guarantee), else **no catalog**.

**Selecting a model** is `session/set_config_option`:
`SetSessionConfigOptionRequest` = required `sessionId` and `configId`, plus a value variant —
`{ type: "boolean", value: <bool> }`, or the **default when `type` is absent on the wire**, a
`SessionConfigValueId` string `value`. The schema notes unknown `type` values with string payloads
also deserialize into that variant. The response, `SetSessionConfigOptionResponse`, returns the full
`configOptions` set again.

Config options can also change mid-session: the `config_option_update` session update carries
`ConfigOptionUpdate { configOptions }` — the **full** set with current values.

`session/set_mode` exists and is separate; modes are not models.

### What remains genuinely unverifiable without an account

Whether Antigravity actually publishes a model selector, what its `id` is, whether it sets
`category: "model"`, and whether its options are flat or grouped. The decoding is written against
the schema and tolerates every documented shape; a live authenticated `session/new` would confirm
which one Google emits.
