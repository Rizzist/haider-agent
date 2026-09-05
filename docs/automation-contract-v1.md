# Haider automation contract v1

Status: orchestrator-facing guide for wire protocol `v = 1`  
Byte authority: `crates/haider-rpc/tests/fixtures/wire_transcript.json` and
`client_contract_methods_v1.json`; the caching declaration comes from
`crates/haider-cli/tests/fixtures/observe_status.json`.

This guide is for a controller that has never used the Rust SDK. It summarizes
the existing protocol; it does not add a method, field, event, or compatibility
promise. Every JSON block below is copied either as a complete `ws_body` frame
from `wire_transcript.json` or as an exact request/response body from
`client_contract_methods_v1.json`, or as the exact `daemon.caching` value
from the CLI status golden. The fence tag says which real
`haider-rpc` type parses it. The source note beside each example names its
golden line.

The detailed projection and absence rules remain authoritative in
`docs/client-contract-v1.md:27-49`; release-by-release additive event changes
are in [the event schema changelog](event-schema-changelog.md).

## 1. Discover and authenticate the local endpoint

Run `haider status --json` for the chosen profile and read
`.daemon.socket_path`. Do not reconstruct or scan for the endpoint. The field
is the daemon's resolved Unix-domain socket path on Unix and a named-pipe
address on Windows (`crates/haider-cli/src/observe.rs:239-272`, `:576-598`;
`README.md:177-186`).

The local stream is owner-authenticated before `Hello`: Unix compares the
kernel-reported peer UID with the endpoint owner; Windows compares the peer
process token's user SID with the daemon user
(`crates/haider-daemon/src/connection.rs:1239-1266`;
`crates/haider-platform/src/ipc/mod.rs:520-550`). There is no bearer credential
or authentication secret in `Hello` or any per-request transport field;
authentication occurs before the first frame (`crates/haider-rpc/src/frame.rs:5310-5313`).
Consequently an orchestrator must not put a transport credential on argv.

## 2. Framing and encoding

Each local-stream frame is a four-byte unsigned big-endian body length followed
by exactly that many JSON or MessagePack bytes
(`crates/haider-rpc/src/uds_codec.rs:1-6`, `:22`). `Hello` and `Welcome` are
JSON. If negotiation selects MessagePack, the switch happens at the clean frame
boundary immediately after `Welcome` (`crates/haider-rpc/src/uds_codec.rs:727-736`).

The default body ceiling is 48 MiB, while `Hello.max_receive_frame` and
`Welcome.frame_limit` publish the connection's actual receive/send ceilings
(`crates/haider-rpc/src/frame.rs:45-57`, `:753-762`, `:793-795`). Enforce the
smaller applicable limit before allocating a body. A zero length, an announced
length above the limit, allocation failure, malformed UTF-8/JSON/MessagePack,
or switching encoding in the middle of a frame permanently poisons the
decoder. Discard the decoder with the connection; never scan forward for a new
JSON boundary (`crates/haider-rpc/src/uds_codec.rs:739-751`, `:858-885`,
`:907-940`).

## 3. Hello, Welcome, features, and absence

`Hello.protocol_min..=protocol_max` is the client's inclusive range. The daemon
selects the highest overlap that this v1 crate implements and returns it as
`Welcome.protocol`; a disjoint or invalid range is a fatal protocol error
(`crates/haider-rpc/src/negotiation.rs:32-45`, `:65-100`).
`Welcome.daemon_version` is diagnostic package/build identity, not the
negotiated protocol (`crates/haider-rpc/src/frame.rs:784-811`).

```json wire.hello
{"v":1,"kind":"hello","protocol_min":1,"protocol_max":2,"client_name":"haider-gui","client_version":"0.0.8","client_instance_id":"client-instance-1","client_kind":"gui","capabilities_requested":["view","control"],"max_receive_frame":1048576}
```

Source: `crates/haider-rpc/tests/fixtures/wire_transcript.json:3`.

```json wire.welcome
{"v":1,"kind":"welcome","protocol":1,"instance_id":"instance-featured","daemon_generation":5,"frame_limit":1048576,"profile_id":"profile-1","daemon_version":"0.0.9","lifecycle_phase":"ready","capabilities_granted":["view","control"],"features":["session_mutation_v1","turn_control_v1"]}
```

Source: `crates/haider-rpc/tests/fixtures/wire_transcript.json:119`.

Capabilities authorize this connection: `view` receives reads/replay and
`control` permits mutations. `Welcome.features` independently advertises
additive method/field families (`crates/haider-rpc/src/frame.rs:711-730`,
`:803-822`; `docs/client-contract-v1.md:205-220`). Apply the absence law:

- an absent feature means “do not call or render that feature,” not an empty
  result or permission to synthesize a fallback;
- an omitted optional means unknown unless its field explicitly declares a
  default;
- unknown object fields and top-level kinds are additive and must not corrupt
  already-understood data (`crates/haider-rpc/src/frame.rs:5295-5307`);
- event payload/item/terminal additions follow
  [the schema changelog](event-schema-changelog.md).

For this guide's catalog, gate the following doors before calling or rendering
them. A dash means the door is part of the v1 baseline and has no separate
feature bit.

| Door or shape | Required `Welcome.features` value |
|---|---|
| `session.create` | `session_mutation_v1` |
| `session.list`, `session.diagnostic`, `menu.answer`, `daemon.shutdown` | — |
| `session.list_watch` / readiness composition | `session_list_watch_v1` |
| `session.fork` | `session_fork_v1`; the shown prompt cut additionally requires `session_prompt_fork_v1` |
| `session.select_model` | `session_model_select_v1` |
| `session.seen` | `session_seen_v1` |
| `turn.submit`, its three delivery modes, `turn.cancel` | `turn_control_v1` |
| `agent.message` | `agent_message_v1` |
| `agent.cancel` | `agent_cancel_v1` |
| `status.snapshot` | `status_snapshot_v1`; the shown runtime path/readiness fields additionally require `status_runtime_v1` |
| `command.list`, `command.invoke` | `command_door_v1` |
| recovery composition | `effect_recovery_v1` and `session_observe_v1` |

These tokens are declared with their meanings at
`crates/haider-rpc/src/frame.rs:280-283`, `:363-371`, `:380-443`, and
`:504-506`. Treat the table as minimum gates for the exact shapes shown here;
additional optional fields can have their own advertised feature.

## 4. Correlation, demultiplexing, and replay

There are six foundational protocol-skeleton variants: `Hello`, `Welcome`,
`Request`, `Response`, `Event`, and `AttachCaughtUp`
(`crates/haider-rpc/src/frame.rs:5310-5348`). The current enum is deliberately
larger: roster/account notifications, resident binding, menu answers,
keepalive, draining, descendant/monitor/Loom delivery, and `Unknown` are
additive top-level variants. Do not implement the six as an exhaustive enum.

For an operation, mint a connection-scoped `request_id`; the `Response` echoes
it. That ID is correlation, not cross-connection idempotency. Durable mutations
carry a separate `command_id` (`crates/haider-rpc/src/frame.rs:5316-5327`).

Demultiplex an `Event` by `attachment_id` and `session_id`. Inside it,
`RawEnvelope.seq` is the only replay cursor. Delivery is at least once: apply
only a consecutive sequence, drop a duplicate `seq <= last_applied`, and
reattach from the greatest fully applied sequence after a gap
(`crates/haider-rpc/src/frame.rs:5328-5335`).

```json wire.event
{"v":1,"kind":"event","attachment_id":"attachment-1","session_id":"session-1","envelope":{"schema_version":1,"event_id":"ev-10","seq":10,"session_id":"session-1","device_id":"device-1","authority_epoch":3,"worker_generation":7,"committed_at_ms":1753500000010,"render":{"ui":true,"durable":true,"prompt":"verbatim"},"payload":{"detail":"kept raw","type":"future_event"}}}
```

Source: `crates/haider-rpc/tests/fixtures/wire_transcript.json:63`.

`session.attach.after_seq` is the greatest sequence already fully applied. Its
replay interval is session-relative and open/closed:
`(requested_after_seq, replay_through_seq]`
(`crates/haider-rpc/src/frame.rs:1684-1695`, `:3263-3275`).
`AttachCaughtUp.high_water_seq` is the barrier saying that this attachment has
delivered through that session sequence. It may repeat later with a strictly
higher high-water mark, so process every occurrence identically
(`crates/haider-rpc/src/frame.rs:5337-5348`).

```json wire.attach_caught_up
{"v":1,"kind":"attach_caught_up","attachment_id":"attachment-1","high_water_seq":10}
```

Source: `crates/haider-rpc/tests/fixtures/wire_transcript.json:67`.

## 5. Error shapes

The shared domain error is
`HaiderError { code, message, retryable, details?, presentation? }`
(`crates/haider-protocol/src/error.rs:310-319`). It is not the literal shape of
a correlated socket failure. `ResponseBody::Error` carries
`code`, `message`, `retryable`, and optional typed recovery `data`; a
connection/negotiation `ProtocolError` instead carries `fatal`, optional
`presentation`, and `failed_write_ids`
(`crates/haider-rpc/src/frame.rs:4797-4819`, `:5248-5265`). Never parse
`message` to recover a code or coordinate.

```json wire.response
{"v":1,"kind":"response","request_id":"request-control","body":{"method":"error","code":"capability_denied","message":"control capability required","retryable":false}}
```

Source: `crates/haider-rpc/tests/fixtures/wire_transcript.json:51`.

## 6. Method catalog and golden examples

Complete request/response bodies live in `RequestBody` and `ResponseBody`
(`crates/haider-rpc/src/frame.rs:2986-2994`, `:4138-4142`). The catalog below
is the orchestrator control subset requested by this guide. A `wire.*` block is
a complete golden frame; a `body.*` block is an exact supplemental golden body
and must be placed in the corresponding `Request` or `Response` frame.

### 6.1 Sessions

#### `session.create`

```json wire.request
{"v":1,"kind":"request","request_id":"request-create","body":{"method":"session.create","command_id":"command-create","cwd":"/tmp/workspace","provider":"anthropic","model":"claude-test","max_tokens":4096}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-create","body":{"method":"session.create","session_id":"session-created","created_seq":1,"worker_generation":7,"metadata":{"cwd":"/tmp/workspace","provider":"anthropic","model":"claude-test","max_tokens":4096,"created_at_ms":1753500040000}}}
```

Sources: `wire_transcript.json:95`, `:99`; types
`crates/haider-rpc/src/frame.rs:3025-3079`, `:4157-4165`.

`haider run --account` and wire `account_alias` name a credential descriptor; a
no-auth custom provider has no credential alias and is selected with
`--provider` plus `--model`.

For the packaged headless front door, `haider run -p TEXT` creates the session
with `interaction_mode: "autonomous"`. Every Haider permission-policy `Ask`
then resolves to ordinary `Allow`; unflagged runs may mutate the selected
workspace and execute processes. This rule comes from interaction mode, not
from implied `--allow-writes` or `--allow-exec` flags. Explicit deny policy,
`--read-only`, workspace containment, and provider lockdown remain enforced.
An explicit denial is returned as a typed tool result with its rule reason.
`--read-only` blocks filesystem mutation and local/remote process, Git,
desktop-control, and peer-message effects that could mutate the workspace
indirectly; matching automatic hooks and Loom registry/installer mutation are
also suppressed. The exact direct-write
reason is `write denied: run is --read-only`; any attempted read-only effect's
specific reason also names the run's `permission_denied` terminal.
Clients require the additive `session_read_only_v1` feature before creating a
read-only session, so daemon reuse cannot silently drop this explicit deny.

#### `session.list`

```json wire.request
{"v":1,"kind":"request","request_id":"request-list","body":{"method":"session.list","cursor":"cursor-after-session-0","limit":50}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-list","body":{"method":"session.list","sessions":[{"session_id":"session-1","head_seq":9,"worker_generation":7}],"next_cursor":"cursor-after-session-1"}}
```

Sources: `wire_transcript.json:11`, `:27`; types
`crates/haider-rpc/src/frame.rs:3081-3093`, `:4166-4175`.

#### `session.list_watch`

```json body.request
{"method":"session.list_watch"}
```

```json body.response
{"method":"session.list_watch","accepted":true}
```

Sources: `client_contract_methods_v1.json:23-26`; types
`crates/haider-rpc/src/frame.rs:3100-3102`, `:4204-4206`. Pushes arrive as
additive `SessionRosterDelta` frames; the acknowledgement is not a baseline.

#### `session.fork`

```json wire.request
{"v":1,"kind":"request","request_id":"request-session-prompt-fork","body":{"method":"session.fork","command_id":"command-session-prompt-fork","session_id":"session-prompt-source","worker_generation":7,"source_branch_id":"branch-plan-b","prompt":{"seq":58},"name":"Edit plan B"}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-session-prompt-fork","body":{"method":"session.fork","session_id":"session-prompt-child","source_session_id":"session-prompt-source","source_branch_id":"branch-plan-b","fork_node_id":"node-before-prompt-b","fork_seq":57,"created_seq":58,"worker_generation":7,"metadata":{"cwd":"/tmp/workspace","provider":"anthropic","model":"claude-test","max_tokens":4096,"system_prompt_version":"fork-policy-v1","title":"Chocolate-free child","created_at_ms":1753500041000},"forked_from":{"session_id":"session-prompt-source","seq":58},"draft":{"text":"Revise the implementation plan using this file.","attachments":[{"kind":"file","artifact":"blake3:prompt-b-file","name":"requirements.txt","lines":12}]}}}
```

Sources: `wire_transcript.json:699`, `:703`; types
`crates/haider-rpc/src/frame.rs:3293-3315`, `:4398-4417`.

#### `session.select_model`

```json body.request
{"method":"session.select_model","command_id":"command-model","session_id":"session-1","worker_generation":3,"model":"gpt-test","provider":"openai"}
```

```json body.response
{"method":"session.select_model","session_id":"session-1","provider":"openai","model":"gpt-test","selected_seq":20,"worker_generation":3}
```

Sources: `client_contract_methods_v1.json:179-182`; types
`crates/haider-rpc/src/frame.rs:3467-3485`, `:4523-4534`.

#### `session.seen`

```json body.request
{"method":"session.seen","command_id":"command-seen","session_id":"session-1","worker_generation":3}
```

```json body.response
{"method":"session.seen","session_id":"session-1","seen_at_ms":1753500000000,"seen_seq":21,"worker_generation":3}
```

Sources: `client_contract_methods_v1.json:185-188`; types
`crates/haider-rpc/src/frame.rs:3498-3506`, `:4547-4555`.

#### `session.diagnostic`

Use this durable door to report consumer-detected compatibility loss rather
than degrading silently.

```json body.request
{"method":"session.diagnostic","command_id":"command-diagnostic","session_id":"session-1","code":"consumer_gap","message":"missing projection"}
```

```json body.response
{"method":"session.diagnostic","recorded_seq":16}
```

Sources: `client_contract_methods_v1.json:143-146`; types
`crates/haider-rpc/src/frame.rs:3240-3247`, `:4362-4363`.

### 6.2 Turns and delivery boundaries

#### `turn.submit`

```json wire.request
{"v":1,"kind":"request","request_id":"request-submit","body":{"method":"turn.submit","command_id":"command-submit","session_id":"session-created","worker_generation":7,"text":"hello","attachments":[],"mode":"queue"}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-submit","body":{"method":"turn.submit","session_id":"session-created","run_id":"run-1","accepted_seq":3,"worker_generation":7,"disposition":"started"}}
```

Sources: `wire_transcript.json:103`, `:107`; types
`crates/haider-rpc/src/frame.rs:3360-3383`, `:4461-4471`.

`queue.steer`, `queue.subturn`, and `queue.turn` are not RPC method names. They
are the UI/catalog spellings for the existing `turn.submit.mode` values:

| Catalog selection | Wire `mode` | Delivery boundary |
|---|---|---|
| `steer` | `steer` | next safe provider-request boundary |
| `subturn` | `subturn` | before the next resolved tool call; the provider may revise it |
| `turn` | `queue` | after the active logical turn, as a later turn |

The enum is `DeliveryMode` (`crates/haider-protocol/src/lib.rs:160-168`); the
catalog wording is `crates/haider-rpc/src/command.rs:215-219`; actor boundaries
are pinned in `crates/haider-core/src/actor.rs:1574-1588`, `:8926-8942`. The
goldens exercise `mode: "queue"`; this guide does not fabricate unpinned
steer/subturn frames.

#### `turn.cancel`

```json wire.request
{"v":1,"kind":"request","request_id":"request-cancel","body":{"method":"turn.cancel","command_id":"command-cancel","session_id":"session-created","worker_generation":7,"run_id":"run-1"}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-cancel","body":{"method":"turn.cancel","session_id":"session-created","run_id":"run-1","status":"accepted"}}
```

Sources: `wire_transcript.json:111`, `:115`; types
`crates/haider-rpc/src/frame.rs:3434-3441`, `:4484-4493`.

### 6.3 Delegated-agent control

#### `agent.message`

```json wire.request
{"v":1,"kind":"request","request_id":"request-agent-message","body":{"method":"agent.message","command_id":"command-agent-message","session_id":"session-parent","worker_generation":7,"agent":"agent-child-7","text":"re-read the parser fixture"}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-agent-message","body":{"method":"agent.message","receipt":{"agent":"agent-child-7","delivery":"delivered_steer","child_run_id":"run-child-7","child_run_state":{"state":"streaming"}}}}
```

Sources: `wire_transcript.json:387`, `:391`; types
`crates/haider-rpc/src/frame.rs:3341-3350`, `:4448-4449`.

#### `agent.cancel`

```json wire.request
{"v":1,"kind":"request","request_id":"request-agent-cancel","body":{"method":"agent.cancel","command_id":"command-agent-cancel","session_id":"session-parent","worker_generation":7,"agent":"agent-child-7"}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-agent-cancel","body":{"method":"agent.cancel","agent":"agent-child-7","child_session_id":"session-child-7","child_run_id":"run-child-7","status":"accepted"}}
```

Sources: `wire_transcript.json:723`, `:727`; types
`crates/haider-rpc/src/frame.rs:3351-3359`, `:4450-4459`.

### 6.4 Menu answer and recovery composition

`menu.answer` is a top-level `WireFrame`, not a `RequestBody` method. Its
optional `request_id` asks for a correlated `ResponseBody::MenuAnswer`
(`crates/haider-rpc/src/frame.rs:5397-5421`, `:4792-4796`).

The named transcript has no correlated menu-answer/success pair. The next two
blocks are independent exact goldens and deliberately have different
`request_id` values. They demonstrate the request and success shapes only; on
a real exchange the response must echo the request's `request_id`.

```json wire.menu_answer
{"v":1,"kind":"menu_answer","request_id":"request-menu-1","command_id":"command-1","session_id":"session-1","menu_id":"menu-1","request_seq":8,"worker_generation":7,"option_key":"other","option_index":2,"input":{"kind":"text","text":"custom answer"}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-menu-success","body":{"method":"menu.answer","resolution_seq":10}}
```

Sources: `wire_transcript.json:71`, `:47`.

There is no `recover` RPC. `haider session <id> recover` is a CLI composition:
it identifies the recovery menu, sends the top-level answer, and immediately
re-reads `session.observe`. Its output schema is
`haider.session_recovery.v1` (`crates/haider-cli/src/session_recover.rs:19`,
`:46-79`). With `--json`, showing the card emits `schema`, `session_id`,
`run_state`, `menu_id`, `title`, `body`, `options`, and optional
`parked_since`. A completed action emits the same schema plus `menu_id`,
`chosen_option`, `resolution_seq`, `completed: true`, `resulting_head_seq`,
`resulting_run_state`, and optional resulting-run/replacement-menu IDs. A
typed failure emits `completed: false` and an `error` object with `code`,
`message`, and `retryable`.

A successful card or completed action exits 0. `no_recovery` exits 77 and is
reserved for a clean terminal (`run_state: idle`). A non-terminal digest that
has lost its required recovery card instead exits 69 with retryable
`recovery_incomplete`; it is never collapsed to `no_recovery`. In particular,
after daemon SIGKILL following durable provider admission but before a durable
response, restart parks the run at `effect_unknown`. `recover --probe --json`
must exit 0 with `chosen_option: probe`, `completed: true`, and a replacement
menu ID formed as `<answered-menu-id>-probe-<resolution-seq>`. Probing does not
reissue the ambiguous provider request.

The named fixtures contain no CLI recovery output, so the required wire
request/response examples for this catalog entry are the independent
`menu.answer` request/success shapes and the correlated `session.observe` pair
below; no recovery document is invented. The golden observe pair is:

```json wire.request
{"v":1,"kind":"request","request_id":"request-observe","body":{"method":"session.observe","session_id":"session-1","last_event_limit":20}}
```

```json wire.response
{"v":1,"kind":"response","request_id":"request-observe","body":{"method":"session.observe","digest":{"session_id":"session-1","head_seq":9,"worker_generation":7,"title":"Observe the durable session","run_state":"parked_input","branches":[],"main_head_seq":0,"pending_menus":[],"subagents":[],"updated_at_ms":1753500000009,"last_event_kinds":["run_state","menu_opened"]}}}
```

Sources: `wire_transcript.json:379`, `:383`.

### 6.5 Daemon lifecycle and readiness

#### `daemon.shutdown`

```json body.request
{"method":"daemon.shutdown"}
```

```json body.response
{"method":"daemon.shutdown"}
```

Sources: `client_contract_methods_v1.json:5-8`; types
`crates/haider-rpc/src/frame.rs:2995-2998`, `:4143-4146`. Acceptance is not
completion; `ServerDraining`, disconnect, and the generation-bound completion
receipt are the lifecycle evidence (`docs/client-contract-v1.md:420-440`).

#### `status.snapshot`

```json body.request
{"method":"status.snapshot"}
```

```json body.response
{"method":"status.snapshot","session_count":365,"adoption_available":[{"source":"codex","email":"person@example.invalid"}],"daemon_pid":4242,"socket_path":"/tmp/haider-golden/h.sock","pid_file_path":"/tmp/haider-golden/haiderd.pid","ready":true,"ready_since":1753500000000,"providers_loaded":true}
```

Sources: `client_contract_methods_v1.json:397-398`; types
`crates/haider-rpc/src/frame.rs:3107-3111`, `:4196-4242`.

With `status_runtime_v1`, `ready` is the daemon's positive serving predicate,
not a PID-exists shortcut: the store is open, startup recovery is complete,
the provider registry/factories are loaded, the session hub can accept turns,
and the lifecycle is still `Ready`. `ready_since` is the Unix epoch timestamp
in milliseconds at that positive edge. Its absence means an older response or
a non-ready predicate; automation MUST NOT synthesize it from process start,
the profile-lock owner record, the daemon PID file, or socket existence.

`providers_loaded: true` means only that provider descriptors and factories
were registered. It does not mean an upstream provider is connected or
authenticated: provider connections are established per request. `haider
--ready` and the launcher readiness channel use this same predicate. The
v0.0.969 idle-TTL and warm-retention meanings are unchanged.

`haider sessions wait-ready --json` is a separate CLI document,
`haider.sessions.ready.v1`, not another RPC. It combines daemon readiness,
expected/ready session sets, counts, and typed error state
(`crates/haider-cli/src/automation.rs:44-59`). It is implemented by the
`session.list` and `session.list_watch` request/response examples in §6.1; the
two named fixtures do not contain the resulting CLI document, so this guide
does not invent one.

### 6.6 Shared command catalog

#### `command.list`

```json body.request
{"method":"command.list","query":"model ","in_session":true,"slots":{"providers":[["openai","OpenAI"]],"models":[["gpt-test","OpenAI · gpt-test"]],"accounts":[["work","Work account"]],"efforts":[["high","High effort"]],"custom_commands":[["review","Review changes"]]}}
```

```json body.response
{"method":"command.list","items":[{"kind":"argument","ownership":"daemon_operation","label":"gpt-test","description":"OpenAI · gpt-test","name":"model","value":"gpt-test","session_only":true}]}
```

Sources: `client_contract_methods_v1.json:11-14`; types
`crates/haider-rpc/src/frame.rs:2999-3007`, `:4147-4150`.

#### `command.invoke`

```json body.request
{"method":"command.invoke","command_id":"command-invoke-1","command":"theme dark"}
```

```json body.response
{"method":"command.invoke","outcome":{"kind":"client_owned","command":"theme dark"}}
```

Sources: `client_contract_methods_v1.json:17-20`; types
`crates/haider-rpc/src/frame.rs:3008-3015`, `:4152-4153`.

## 7. Event taxonomy and the two carriers

Every durable envelope carries `schema_version` and a per-session `seq`
(`crates/haider-protocol/src/envelope.rs:34-42`). An attached run
ends with exactly one typed terminal discriminator: `success`, `failure`,
`cancellation`, `timeout`, or `provider_error`. The terminal retains the one
ordinary durable run-state envelope and its sequence; it is not emitted twice
(`crates/haider-client/src/headless.rs:468-486`;
`docs/jsonl-run-contract-v1.md:78-102`).

For `haider run` and the reusable headless control attachment, the first
SIGINT binds to the correlated run as exactly one durable `turn.cancel`.
Idempotent transport retries reuse one command identity. The client drains the
same cursor stream through its single typed `cancellation` terminal, bounded
by the tighter caller deadline, then exits 130. A second SIGINT exits 130
immediately after the first cancellation receipt is durable; the daemon keeps
the durable cancel and finishes draining if necessary. This is a process-exit
contract, not a new RPC, event kind, or terminal shape
(`docs/jsonl-run-contract-v1.md`).

Tool correlation has no Haider-generated substitute ID. The provider call ID
is `TurnItem::ToolCall.call_id`; argument deltas join through `item_id`, the
completed item repeats both identities, and `ToolResult.call_id` repeats the
provider ID (`crates/haider-protocol/src/item.rs:91-103`, `:163-184`;
`crates/haider-protocol/src/lib.rs:104-112`;
`docs/jsonl-run-contract-v1.md:61-76`).

Usage is a separate correlated `payload.type == "usage"` envelope, retained by
the reducer before the terminal; it is not a field on the run-state terminal
(`crates/haider-protocol/src/provider.rs:139-177`;
`crates/haider-client/src/headless.rs:1623-1635`, `:1763-1777`). Exact or
estimated cost is available only where the typed usage/report structures
publish it; absence is unknown, not zero. An orchestrator must therefore fold
the latest correlated usage fact and must not expect usage/cost inside the
terminal payload.

The socket and `haider run --output jsonl` carry the same durable envelope and
payload schema, but their wrappers are not byte-identical:

- the socket wraps an unmodified `RawEnvelope` in `WireFrame::Event` with
  `attachment_id` and `session_id`, and uses `AttachCaughtUp` as its barrier;
- JSONL starts with one non-envelope acceptance object, then emits envelopes
  directly. The journal writer retains `payload.terminal_kind` and optional
  `payload.error_code` on the one terminal envelope, so live JSONL and replay
  serialize that same envelope without changing its `seq`
  (`docs/jsonl-run-contract-v1.md:7-22`, `:87-107`;
  `crates/haider-cli/src/run.rs`).

## 8. Delegation without a spawn RPC

There is no `agent.spawn` RPC. A model delegates through the
`spawn_subagent` tool. Its required arguments are `task` (1–80 bytes) and
`prompt` (1–32 KiB); optional selectors are `model`, `provider` (only with a
model), `workflow`, `workflow_trigger`, `parent_slot`, `workflow_author`, and
`agent_type`. Its advertised JSON schema sets `additionalProperties: false`;
controllers must not send unknown properties
(`crates/haider-tools/src/spawn_subagent.rs:8-46`, `:115-179`).

Creation is observed as `EventPayload::AgentSpawned(AgentManifest)`; the
manifest carries the opaque agent ID and its grant/fencing coordinates
(`crates/haider-protocol/src/lib.rs:113-120`;
`crates/haider-protocol/src/agent.rs:11-46`). The child terminal is the
`TurnItem::ChildResult { report: ChildReport }` item
(`crates/haider-protocol/src/item.rs:109-116`;
`crates/haider-protocol/src/agent.rs:129-147`).

For follow-up instructions, a model uses `message_subagent { agent, message }`;
the message is bounded to 32 KiB and addresses a direct child by opaque agent
ID (`crates/haider-tools/src/message_subagent.rs:7-36`, `:39-64`). An external
controller uses the existing `agent.message` method above. To stop a child and
its descendants, use `agent.cancel`.

## 9. Provider request correlation

Every turn-owned outbound provider HTTP request carries these two headers:

```text
X-Haider-Turn: <session_id>/<run_id>/<turn_ordinal>/<request_ordinal>
X-Haider-Request-Kind: primary|side|warmup
```

`turn_ordinal` is an unsigned, unpadded, 1-based ordinal that increases for
each new run accepted in one session. A steer or subturn delivered to an
existing run keeps that run's ordinal. `request_ordinal` is also unsigned,
unpadded, and 1-based, but is scoped to the turn: every physical request gets
the next value, including transport/provider retries and a request reissued
after restart. The complete slash-delimited value is therefore the stable
join key for one physical provider attempt; neither opaque ID may be empty,
contain `/`, contain non-visible ASCII, or exceed 256 bytes.

`primary` identifies model requests that directly advance a root user turn.
A normal model continuation after a tool result remains `primary`. `side`
identifies delegated-child model requests, context summarization/compaction,
estimation, session-owned Loom drafting inference, provider cache-resource
create/delete operations owned by the turn, and provider-facing tool-support
requests such as subscription web search. `warmup` is reserved for an
explicitly enabled, unmeasured connection prewarm; ordinary startup or
first-turn traffic must not infer it. A deduped or otherwise skipped prewarm
does not allocate a request ordinal.

Headers are the default and authoritative transport surface on OpenAI native
and compatible adapters, Anthropic, Gemini, and turn-owned auxiliary HTTP
provider calls. Correlation is never injected into the ordinary JSON body of
a strict provider schema. An adapter may explicitly declare that its schema
accepts a top-level `metadata` object; when both that declaration and
`HAIDER_PROVIDER_BODY_METADATA=1` are present, Haider also writes
`metadata.haider_turn` and `metadata.haider_request_kind`. Native OpenAI
Responses declares that support. Compatible, Anthropic, and Gemini schemas do
not. The body mirror is diagnostic convenience only; consumers join on the
headers.

Before opening the network request, the daemon commits the same identity in a
prompt-omitted request-attempt journal marker. Model/compaction requests use
the additive `correlation` field of `cache_request_attempt_v1`; auxiliary
provider HTTP calls use `provider_request_attempt_v1`. With
`HAIDER_DAEMON_TRACE=1`, every request-scoped `haider.turn` record carries
`session_id`, `run_id`, the exact `turn_id`, `request_kind`, `turn_ordinal`,
and `request_ordinal`. Turn-level accept/terminal records use request ordinal
zero and do not claim a physical-request ID. Journal, trace, and
proxy/provider ledgers must compare equal on request coordinates; deriving
identity from prompt/body text is forbidden.

This contract is scoped to requests owned by a durable turn or durable
session inference operation. Turn-triggered Gemini cache-resource
create/delete calls, opt-in connection prewarm, and `loom.author.draft` are
therefore covered. Loom drafting first commits the prompt-omitted
`provider_operation_reserved` lifecycle fact, then journals its `side` request
attempt; the reservation participates only in durable turn-ordinal identity
and is excluded from conversation observation, hooks, and agent-usage timing.
This preserves session-monotonic ordinals without introducing a conversation
message or a run-state transition. Session forks omit the reservation run and
all of its parent-owned request-attempt audit facts rather than rewriting their
envelope session while retaining embedded parent coordinates. Catalog reads,
credential validation, cache cleanup
with no durable turn or session-inference owner, and other out-of-turn
control-plane HTTP calls have no run/request-attempt journal coordinate and
omit these headers. ACP adapters use local JSON-RPC over supervised stdio
rather than an HTTP provider request, so there is no HTTP header surface to
mutate.

## 10. Compatibility and no silent degradation

Within wire protocol major v1, evolution is additive: optional fields, feature-
gated methods, unknown-tolerant variants, and raw event families. Existing
fields are not removed or retyped. The currently published client contract
names package `0.0.964` as its N-1 compatibility baseline
(`docs/client-contract-v1.md:3-6`); its additive evolution law is at
`:2614-2618`. Gate optional behavior on
`Welcome.features`; the current `Welcome` has no separate deprecation list, so
do not infer a deprecation schedule from feature absence. Any future
deprecation must be announced through additive `Welcome.features` signaling
before an existing v1 door changes; no deprecation is announced today.

Silent degradation is a protocol fault. If a required feature is absent, an
unknown kind invalidates a projection, or a required field is missing, stop
that projection, retain the last honest cursor/value, and report the problem
through `session.diagnostic` when a session coordinate exists. Never replace
unknown with empty, zero, or a locally guessed default
(`docs/client-contract-v1.md:27-49`).

`haider status --json` publishes `runtime_dir_resolution`: `source` identifies
the selected runtime-directory source; optional `rejections` lists earlier
candidates as `{source, reason}` and is omitted when empty. Use this provenance
alongside `.daemon.socket_path` and `.daemon.pid_file_path`, without inferring
the resolver's choices from paths. This is a CLI status field, not
`ResponseBody::StatusSnapshot`; its projection is in
`crates/haider-cli/src/observe.rs` and its types are in
`crates/haider-client/src/profile.rs`.

## 11. Daemon caching and reuse declaration

Since v0.0.970, `haider status --json` includes `daemon.caching` from the
daemon's additive `status.snapshot.caching` object. An older daemon that
omits this declaration produces `daemon.caching: null`; absence is unknown,
not disabled. A representative declaration is:

```json value.daemon_caching
{"prompt_cache":true,"provider_view_cas":true,"session_reuse":"resident","idle_ttl_ms":30000,"cache_regime":"automatic-prefix","cache_regimes_by_provider":{"openai":"automatic-prefix","anthropic":"explicit-breakpoints"}}
```

Source: `crates/haider-cli/tests/fixtures/observe_status.json:1`,
`daemon.caching`; type: `haider_rpc::DaemonCachingWire`.

| Field | Meaning |
| --- | --- |
| `prompt_cache` | The daemon's cache-aware prompt preparation is enabled. This is support, not a cache-hit metric or a claim that every request emits cache controls: adapter model/auth verification, account/header certainty, model minimums, and the upstream provider determine eligibility and actual cache reads. Auxiliary requests such as credential probes and degraded compaction may omit cache metadata. |
| `provider_view_cas` | The daemon persists content-addressed provider views and their journal facts before dispatch when the adapter supplies a provider view. This local durable artifact store is separate from upstream prompt caching and does not memoize provider replies. |
| `session_reuse` | `one_shot` only when the serving daemon's effective idle TTL is zero; otherwise `resident`, including a direct daemon with no idle limit. This reports process reuse policy, not a promise that every session worker remains in memory. Session journals survive either policy. |
| `idle_ttl_ms` | The serving daemon's effective idle timeout, identical to `daemon.idle_ttl_ms`: zero for one-shot, a positive timeout in milliseconds for finite retention, and `null` for unbounded retention. It is neither a prompt-cache TTL nor a session-worker eviction deadline. |
| `cache_regime` | The adapter regime for the daemon's active account, identified by the same status document's `account.provider`. `null` means there is no active account or its adapter has no declared regime. |
| `cache_regimes_by_provider` | Registry provider IDs mapped to adapter regimes, including custom providers. Use the session's effective provider ID here when annotating a rebound or otherwise overridden session; the daemon active account can differ from that session. Missing entries and `null` values mean unknown. |

OpenAI Responses and OpenAI chat-completions family adapters declare
`automatic-prefix`. A proxy ledger reporting zero explicit breakpoints is
therefore compatible with prompt caching; it must not be interpreted as
disabled caching. This is the baseline family convention; supported OpenAI
models can additionally emit explicit TTL/breakpoint overlays, and the
subscription lite adapter has an opt-in breakpoint experiment. The declaration
does not promise that every OpenAI request has zero markers.
Anthropic Messages family adapters declare
`explicit-breakpoints`, using `cache_control` with `type: "ephemeral"` on
eligible prompt blocks. This includes custom providers using those families.
Other families currently declare `null`, so no cache protocol is guessed for
Gemini or externally supervised ACP agents. A regime describes the adapter's
request convention; it cannot promise that a custom endpoint implements a
cache. Harnesses should record the declaration alongside measured cache-read
usage and breakpoint counts. Daemon lifetime and warm residency alone do not
prove that an upstream cache survived or that a stateless client had no cache.

The active-account regime and provider map come from one management snapshot;
this read performs no provider network requests. Unknown additive fields or
future regime strings must be tolerated by consumers.

## Per-session provider rebind

`haider session provider rebind --session <id> --provider <id>
[--base-url <url>] [--account <name>]` sends the negotiated
`session.provider.rebind` RPC (`session_provider_rebind_v1`). The CLI prints a
JSON receipt with schema `haider.session_provider_rebind.v1`, `session_id`,
`provider`, `base_url`, `account`, `selected_seq`, and `worker_generation`.
The RPC accepts those routing arguments plus `command_id` and
`worker_generation`; a Control capability and control attachment to that
session are required. Repeating the same command ID and coordinates returns
the original receipt; reusing it for different coordinates is a conflict.

A successful response means the `session_provider_rebound` event, the session
metadata projection, and the command receipt committed atomically. The event
is additive and replays through the ordinary journal cursor without provider
traffic. Restarted workers read the persisted binding. No daemon restart,
profile-registry rewrite, or change to any other session is required.

The request boundary is the point where the worker snapshots an adapter
before preparing provider-specific history, cache metadata, and its durable
request attempt. A request already past that boundary retains its original
adapter through response completion. A subsequent request, including a
continuation after a tool result or a retry through the core request loop,
picks up a rebind acknowledged before its boundary. Transport work already
owned by the earlier request stays with that request. A manual compaction
captures its route when the operation starts; any internal degraded fallback
retains that snapshot, and a later separate compaction reads the new binding.
The model, effort and speed are unchanged; the current model must be accepted by the target
provider's registry policy. Normal model selection retains its next-turn
contract. Selecting a different provider later clears an earlier rebind's
endpoint and account overrides.

Omitting `base_url` clears this session's URL override and uses the registry
or account endpoint. Omitting `account` clears the explicit account pin and
uses the selected provider's normal active-account resolution (or its
registered no-auth mode). An explicit alias must exist and belong to the
selected provider. Adapter construction uses session-local copies of both
endpoint sources, so an old pooled adapter cannot conceal the new URL.

URL overrides are allowed for registered custom providers and the
`openai-compatible` proxy adapter, subject to the registry's TrustedLan
endpoint validation (loopback and trusted LAN endpoints are supported;
metadata/link-local destinations and public plain HTTP are refused).
`bedrock` and `vertex` permit only their existing enterprise URL templates.
Fixed first-party, OAuth, and externally supervised agent endpoints cannot
be redirected by this verb. Unknown providers return `provider_unknown`,
unknown accounts `account_unknown`, wrong-provider aliases
`account_provider_mismatch`, disabled providers `provider_unavailable`, and
forbidden or malformed URL overrides `invalid_argument`. Stale generations
and command conflicts use the existing typed errors. A trust-class change,
or a provider change within lockdown, returns `busy` while a run is active;
retry after the session becomes idle. This preserves its frozen tool/effect
permissions.

A changed route resets the request loop's cache comparison state and marks a
configuration-change rewarm. The caching declaration above describes adapter
support; a rebind does not promise a hit at a different endpoint or account.
Harnesses should retain each rebind receipt, effective provider ID, caching
declaration, and observed proxy/provider usage on the corresponding row.

## Public agent and workflow commands (v0.0.970)

These commands are noninteractive. `spawn` and `run` create a coordinator
session and delegate one actual child through the daemon's tool engine:

The daemon must expose `spawn_subagent`, for example by starting it with
`HAIDER_TOOL_EXPOSURE=spawn_subagent`. Setting this variable on a later CLI
invocation does not reconfigure an already-running daemon. The default coding
catalog leaves delegation unexposed; spawn/run then return exit 70 and typed
`spawn_failed` with the native grant-ceiling refusal and parent coordinates,
without creating a child or making a provider request.

```sh
export HAIDER_TOOL_EXPOSURE=spawn_subagent
haider agent spawn "inspect the failing tests" --task investigation --json
haider agent list <parent-session-id> --json
haider agent message <parent-session-id> <agent-id> "inspect the new failure" --json
haider agent cancel <parent-session-id> <agent-id> --json
haider agent wait <parent-session-id> <agent-id> --timeout 30s --json
haider workflow list --json
haider workflow run child-implement-verify "implement and verify the fix" \
  --trigger mutation_with_independent_verification --json
haider workflow status <child-session-id> --json
```

All verbs accept `--json`, `--no-spawn`, and `--timeout <duration>`. Timeout
defaults to 30 seconds of command observation after connection; connection
startup and transport requests retain their standard finite budgets. Expiring
an observation does not cancel accepted work. A spawn timeout can contain
the accepted `session_id` and `run_id` in `result`; retain these coordinates
and inspect the session instead of blindly retrying. Explicit cancellation
uses `agent cancel`. Closing a successful spawn command leaves its child owned
by the daemon. The ordinary durable delegation lifetime budget still applies.

Spawn/run accept `--provider`, `--model`, `--agent-type`, `--task`, and
`--cwd`; `--prompt`/`-p` can replace the positional prompt. Omitted provider
and model are resolved by the daemon's session-create authority. `--` ends
option parsing, allowing literal prompts such as `--help` or `--json`. Agent spawn
also accepts `--workflow <id> --trigger <reason>`. Workflow execution requires
an explicit valid trigger and an admitted workflow, never a silent bare-task
fallback. Inspect `workflow list` for the native built-in/user catalog.
The catalog also includes workflows with human confirmation gates; those
cannot run as autonomous children and are rejected. `child-implement-verify`
uses `mutation_with_independent_verification`; `child-deeper` accepts
`dependent_phases`, `fan_out`, `distinct_review`, or `crash_recovery`.
Mutations accept `--command-id <id>` for the existing receipt identity rules;
an omitted ID is generated per invocation. Reusing an ID with different
request semantics is an error, not a new operation.

With `--json`, stdout contains exactly one JSON object followed by LF:

```json value.agent_spawn
{
  "error": null,
  "ok": true,
  "result": {
    "agent_id": "agent-investigation",
    "child_run_id": "run-child",
    "child_session_id": "session-child",
    "manifest": {
      "agent": "agent-investigation",
      "attempt": 0,
      "budget_tokens": 4096,
      "callsign": "gold-fox-000001",
      "coordinates": {
        "auto_hermetic": false,
        "call_id": "agent-cli-run-parent",
        "child_session_id": "session-child",
        "handoff_dir": "/workspace/.haider/handoff/parent",
        "lockdown": false,
        "parent_run_id": "run-parent",
        "parent_session_id": "session-parent",
        "provider": "fake",
        "public_headless": true,
        "public_operator_spawn": true,
        "tool_item_id": "item-parent-spawn"
      },
      "fencing_epoch": 1,
      "grant": {
        "effect_ceiling": [
          {
            "class": "fs_read"
          },
          {
            "class": "fs_write"
          },
          {
            "class": "process_exec"
          },
          {
            "class": "remote_execution"
          },
          {
            "class": "agent_spawn"
          },
          {
            "class": "network",
            "host": ""
          },
          {
            "class": "peer_message"
          }
        ],
        "tools": [
          "request_input",
          "fs_read",
          "fs_glob",
          "fs_search",
          "fs_write",
          "fs_edit",
          "write",
          "edit",
          "fs_path",
          "process_exec",
          "spawn_subagent",
          "message_subagent",
          "task_output",
          "task_kill",
          "web_fetch",
          "web_search",
          "monitor",
          "list_models",
          "peer_list",
          "peer_send",
          "ssh_list",
          "ssh_shell"
        ]
      },
      "lease": "lease-child",
      "model_profile": "fake-model",
      "placement": {
        "placement": "local"
      },
      "role": "subagent",
      "task": "investigation"
    },
    "run_id": "run-parent",
    "session_id": "session-parent"
  },
  "schema": "haider.agent.spawn.v1"
}
```

The five agent schemas are `haider.agent.spawn.v1`, `haider.agent.list.v1`,
`haider.agent.message.v1`, `haider.agent.cancel.v1`, and
`haider.agent.wait.v1`. Workflow schemas are `haider.workflow.run.v1`,
`haider.workflow.status.v1`, and `haider.workflow.list.v1`. All share `ok`,
`result`, and `error`. An error has `code`, `message`, and boolean `retryable`;
`result` is null or preserves available durable coordinates/evidence. Consumers
must tolerate additive fields. Human output is not a parsing contract.

| Verb | Successful `result` |
| --- | --- |
| agent spawn / workflow run | Parent `session_id`/`run_id`, actual `agent_id`, `child_session_id`/`child_run_id`, and durable `manifest`. |
| agent list | Native `SessionFleetSnapshot`, including `roots`, bounds, truncation and rollup. |
| agent message | Native `receipt`, including delivery kind and child run coordinates. Running children receive steer; idle children receive a queued fresh turn. |
| agent cancel | Native `agent.cancel` fields: `agent`, child session/run, status and optional terminal sequence. Acceptance is not terminal completion. |
| agent wait | Parent/agent/child coordinates, terminal `state`, child `terminal_seq`, `report`, `report_source`, and nullable parent `child_result_seq`. |
| workflow status | `session_id`, native `graph` status and nullable typed `activation`. A built-in graph need not have a typed Loom activation. |
| workflow list | `workflows`: native workflow catalog entries. |

Wait selects the latest child run visible during its initial journal replay
and keeps that target. For the initial delegation it requires both the child's
typed terminal and its parent's completed `ChildResult`, with
`report_source: "child_result"` and the exact `child_result_seq`. After an idle
child is messaged, its follow-up turn has no new parent collector: wait reads
the actual completed child message and terminal, reports
`report_source: "child_journal"`, `verified: "unverified"`, and a null
`child_result_seq`. It never substitutes the old parent's report. `agent.cancel`
retains the native RPC's original delegation-run targeting and recursive
cancellation semantics; its returned child-run coordinate is authoritative.

| Exit | Meaning |
| --- | --- |
| 0 | Operation succeeded, or wait observed successful completion. |
| 1 | Wait observed a failed child / red report (`child_failed`). |
| 2 | Invalid CLI syntax or locally invalid spawn arguments. |
| 69 | Profile/daemon connection unavailable. |
| 70 | Native RPC rejection, missing target, or rejected spawn. |
| 74 | Output could not be written, including a closed stdout pipe. |
| 76 | Required feature, profile identity, or response protocol mismatch. |
| 124 | Observation deadline expired; accepted work continues. |
| 130 | Wait observed child cancellation (`child_cancelled`). |

Public children are autonomous. A `request_input` without a default produces
the durable rejected tool result `no_human_available`, closes the menu, and
continues the provider loop. It is not a failed agent result. Wait observes
the eventual child terminal/report. The CLI never opens a TUI or waits for
stdin to answer a child menu.

## Additive changelog

### 2026-09-05 — v0.0.970 public agent/workflow CLI

- Added the eight documented verbs, singleton JSON envelopes and exit codes
  above. Existing command output and exit contracts are unchanged.
- Added negotiated `agent_cli_v1` and optional
  `HeadlessRunSpecV1.agent_spawn`. The existing headless start receipt pins
  operator-authored delegation before dispatch; the coordinator invokes no
  provider request. Child admission, result publication and collection use
  the native engine, with autonomous input handling for public children.
- Added the T0 spawn/result gate with sourced `BudgetSum` bounds and explicit
  status-owned daemon cleanup/no-orphan evidence.
- Added optional `caller_owner` to `session.surface_watch` responses. TUI
  clients learn their authoritative mirror identity at watch adoption, before
  publishing local edits; delayed self-echoes cannot overwrite newer typing.
  Omission preserves older response bytes and the legacy compatibility path.

### 2026-09-03 — v0.0.970

- Added `session.provider.rebind`, its CLI verb, negotiated feature, and
  durable `session_provider_rebound` event for per-session endpoint routing.

- Added `status.snapshot.caching`, projected as `daemon.caching` by
  `haider status --json`, declaring cache-aware prompt preparation,
  provider-view CAS, effective process reuse/idle TTL, and provider adapter
  cache regimes. It does not change cache, reuse, or eviction behavior.

- Added stable per-turn provider request correlation headers, matching
  request-attempt journal coordinates, and matching opt-in daemon trace
  fields. Existing strict provider JSON bodies remain unchanged by default;
  the metadata mirror requires both adapter support and explicit opt-in.
- Added default-compatible `ready_since` and `providers_loaded` fields to
  `status.snapshot`, projected as `daemon.ready_since` and
  `daemon.providers_loaded` by `haider status --json`.
- Defined `daemon.ready` as one positive predicate covering store open,
  completed startup recovery, loaded provider registry/factories, an
  accepting session hub, and the live `Ready` lifecycle phase. PID and socket
  publication are not readiness evidence.
- Aligned `haider --ready` and the spawn readiness channel with that predicate.
  No idle-TTL or warm-retention behavior changed, and provider-registry loading
  does not claim a provider connection.

### 2026-09-01 — v0.0.969

- Defined the existing `haider.session_recovery.v1` card, receipt, and error
  documents and their automation exit codes.
- Added the kill-9 provider-admission recovery contract: restart exposes a
  durable recovery card, `--probe` settles to `effect_unknown`, and only a
  clean terminal returns `no_recovery`.
- Bound headless SIGINT to one durable `turn.cancel`, one cancellation
  terminal, and exit 130; a second SIGINT takes the post-receipt fast exit.
- Made packaged auto-spawn finite and warm by default: absent
  `HAIDER_RUN_DAEMON_IDLE_TTL_MS` means a 30,000 ms idle TTL for every front
  door, including `haider run --start`; zero retains exact-child one-shot
  accounting and a positive value through 3,600,000 overrides the TTL.
- Added `daemon.idle_ttl_ms` and `daemon.warm` to `haider status --json`.
  Positive warm daemons report their effective TTL and `true`; TTL zero reports
  `0`/`false`; direct, unbounded, and older daemons report `null`/`false`.
  Automation must use the daemon-published values rather than infer policy from
  its own environment.
- Kept `haider daemon stop --json` as the explicit graceful operator exit for a
  warm resident daemon; a clean result continues to require
  `outcome: "stopped_cleanly"`, `daemon.process_exited: true`, and no surviving
  authenticated PID.
