# Haider automation contract v1

Status: orchestrator-facing guide for wire protocol `v = 1`  
Byte authority: `crates/haider-rpc/tests/fixtures/wire_transcript.json` and
`client_contract_methods_v1.json`

This guide is for a controller that has never used the Rust SDK. It summarizes
the existing protocol; it does not add a method, field, event, or compatibility
promise. Every JSON block below is copied either as a complete `ws_body` frame
from `wire_transcript.json` or as an exact request/response body from
`client_contract_methods_v1.json`. The fence tag says which real
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
`:46-79`). The named fixtures contain no CLI recovery output, so the required
wire request/response examples for this catalog entry are the independent
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
{"method":"status.snapshot","session_count":365,"adoption_available":[{"source":"codex","email":"person@example.invalid"}],"daemon_pid":4242,"socket_path":"/tmp/haider-golden/h.sock","pid_file_path":"/tmp/haider-golden/haiderd.pid","ready":true}
```

Sources: `client_contract_methods_v1.json:389-392`; types
`crates/haider-rpc/src/frame.rs:3095-3099`, `:4176-4202`.

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

SIGINT semantics are currently undefined for `haider run` and have no binding
to `turn.cancel`; orchestrators must use `turn.cancel` or the `--timeout`
budget, and exit 130 is produced only by the product's own cancellation path.

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
  directly; on the one terminal envelope only, the adapter adds
  `payload.terminal_kind` and optional `payload.error_code` while preserving
  the same `seq` (`docs/jsonl-run-contract-v1.md:7-22`, `:78-102`;
  `crates/haider-cli/src/run.rs:1322-1389`).

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

## 9. Compatibility and no silent degradation

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

The 968 runtime-directory lane is adding
[`runtime_dir_resolution`](https://github.com/Rizzist/haider-agent/blob/lane-968-rtdir/crates/haider-cli/src/observe.rs#L67-L78) to
`haider status --json`. It reports which resolution source won and why prior
candidates were rejected; the field is landing in 968 and is not yet in this
branch's wire or CLI types. Until it merges, use the daemon-published
`.daemon.socket_path` and `.daemon.pid_file_path` from status, and report
resolution provenance as unavailable rather than reverse-engineering it. It
remains a CLI status field, not `ResponseBody::StatusSnapshot`. The visible
`lane-968-rtdir` branch defines and projects it at
`crates/haider-cli/src/observe.rs:67-78`, `:240-272`, `:562-608`; its typed
meaning is at `crates/haider-client/src/profile.rs:187-200` on that branch.
