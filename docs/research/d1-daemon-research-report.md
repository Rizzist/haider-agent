# Daemon + attach architecture research for Haider Code W3

Date: 2026-07-26

## Executive summary

Haider should not copy any one of the surveyed products wholesale.

OpenCode demonstrates the convenience of an ordinary HTTP server and Server-Sent Events, but its remote attach is a reconstructed client view, not a durable cursor-based attachment. Events are fanned out live to every SSE connection, but the stream has no event IDs, acknowledgement, or replay; reconnection can miss events. Its default TUI also embeds the server in a worker rather than attaching to a durable daemon. This is useful as a UI hydration precedent, not as Haider's reliability model.

Codex app-server is the strongest protocol and multi-client precedent. It has an explicit connection handshake, typed request/response/notification frames, thread start/resume/list/read/unsubscribe methods, bounded transport queues, one live listener per thread, multi-subscriber fan-out, server-to-client approval requests, first-response-wins arbitration, and replay of unresolved approvals to a newly resumed client. Its active-thread resume is carefully serialized so the response snapshot and following live events have no race. However, its reconnect model is state reconstruction rather than durable event-cursor replay, and its WebSocket listener is explicitly experimental. Loopback WebSocket can also be unauthenticated and rejects every request bearing an `Origin`, which is unsuitable for Haider's GUI webview.

Claude Code's CLI implementation is not present in its public repository, so its wire protocol cannot be audited. The public changelog nevertheless confirms a background daemon, `claude attach`, multiple attached windows, control sockets, socket authentication tokens, worker/session restart, stale-lock bugs, version-skew handling, and a particularly important fixed bug where an old daemon deleted its successor's socket. The public Agent SDK is inspectable, but it is a different surface: one SDK client owns one Claude CLI subprocess over newline-delimited `stream-json` stdio. Permission callbacks use correlated `control_request` / `control_response` frames. SDK resume materializes a stored JSONL transcript and starts a subprocess; it is not live multi-client attachment.

The recommended Haider design is:

- one runtime owner per profile, enforced by the existing store lock held for the daemon's entire lifetime;
- filesystem UDS in an owner-private directory as the primary endpoint, with liveness probing only after acquiring the startup/singleton lock;
- a localhost-only WebSocket endpoint that always requires a high-entropy token and an exact webview-origin policy;
- one versioned, transport-neutral logical frame schema, encoded as one JSON object per WebSocket text message and as length-prefixed UTF-8 JSON on UDS;
- a per-session actor that serializes append, durable high-water observation, and live publication;
- attachment from `after_seq`, with an atomic replay/live barrier, bounded connection queues, and store fallback whenever a subscriber lags;
- durable, compare-and-set arbitration for `MenuAnswer`, so any control-capable attachment may answer but exactly one answer becomes authoritative;
- startup recovery, including `worker_generation` advance and C4a unknown-effect reconciliation, completed before the daemon advertises ready;
- shutdown that stops new mutations first, announces draining, bounds the drain, persists honest terminal/unknown outcomes, closes attachments, removes the exact socket it created, and releases the singleton lock last.

The architectural difference that matters most is that Haider already has the primitive the other systems lack: `RawEnvelope.seq` is a durable cursor. It should remain the sole resume truth. Do not add a separate ephemeral notification offset or reconstruct session state as the normal attach mechanism.

## Scope and method

The research used shallow local clones and source inspection only. No web pages were consulted. Findings are pinned to:

| Repository | Commit inspected | What is public at that commit |
|---|---|---|
| `sst/opencode` | `7534d23551f665e65080809975b4ca5c7d63807b` | Server, CLI/TUI, generated client, event and session source |
| `openai/codex` | `61a44880a85d2fd0d8770908dea5733495e571c8` | Rust app-server, protocol, transports, tests |
| `anthropics/claude-code` | `7ef6eec9d9ba84ea6f233f26c45f1df5c5991843` | README, changelog, plugins/examples; not the CLI implementation |
| `anthropics/claude-agent-sdk-python` | `f8b9ec923982082a02c485924e0f60367949c3a1` | SDK client, subprocess transport, control protocol, session store |
| `anthropics/claude-agent-sdk-typescript` | `71c804dc8f4a61c1dca6fe10d4b95a6b65d1396b` | Published SDK documentation/examples; little core implementation source |

Repository-relative file paths below refer to those pins. Product behavior inferred only from a changelog is labelled as such.

## 1. OpenCode server mode

### 1.1 Process and transport model

`packages/opencode/src/cli/cmd/serve.ts` defines `ServeCommand`. It calls `Server.listen(opts)` and keeps the command alive. The actual server construction is in `packages/opencode/src/server/server.ts`:

- `listen()` runs `listenEffect()`;
- `listenerLayer()` and `startListener()` create a Node/Bun HTTP listener;
- `makeStop()` implements normal shutdown;
- `forceClose()` also closes tracked WebSockets;
- `serverLayer()` builds the HTTP service.

The product server is therefore an HTTP API. The main event channels are SSE, not WebSocket or stdio. A WebSocket route exists for PTY functionality, but it is not the general session-event transport.

The default local TUI is an important qualification. `TuiThreadCommand` in `packages/opencode/src/cli/cmd/tui.ts` starts a JavaScript worker running `packages/opencode/src/cli/tui/worker.ts`. The parent creates:

- `createWorkerFetch()`, which turns TUI fetches into worker RPC calls; and
- `createEventSource()`, which forwards the worker's global events.

The worker's exported `rpc` object serves HTTP requests by calling the embedded app's `fetch`, forwards `GlobalBus` events over its RPC channel, and can optionally call `Server.listen()` when a real network endpoint is requested. Teardown calls the worker's `shutdown` RPC and terminates it. Thus the normal local TUI is an embedded server in a child worker, not a thin client of a durable, independently managed daemon.

Remote attachment is separate. `AttachCommand` in `packages/opencode/src/cli/cmd/attach.ts` accepts a server URL, optional directory, `--continue`, `--session`, `--fork`, and Basic-auth options. It validates an explicit session through `validateSession()` in `packages/opencode/src/cli/tui/validate-session.ts`, then runs the ordinary TUI against the remote HTTP URL.

There is no transport-level “claim” or attachment lease. Attachment means “hydrate this UI from HTTP and consume the instance event stream.”

### 1.2 Session listing, attach, continue, fork, and resume

Session HTTP handlers are defined in:

- `packages/opencode/src/server/routes/instance/httpapi/groups/session.ts`; and
- `packages/opencode/src/server/routes/instance/httpapi/handlers/session.ts`.

`GET /session` lists sessions, with server-side filters for the current instance/directory/scope/path and results ordered by recent update. `GET /session/:id` fetches one session. Messages, todos, diffs, forks, and other session resources have separate routes.

The TUI's client-side reconstruction lives in `packages/tui/src/context/sync.tsx`:

- `listSessions()` fetches recent sessions and establishes local ordering;
- `bootstrap()` hydrates global resources and session summaries;
- `session.sync()` fetches the session, its recent messages, todos, and diff.

The provider starts the event stream before or alongside hydration. While `session.sync()` is running it records that the session is hydrating and prevents older HTTP results from overwriting newer events already observed from the stream. That race avoidance is worth retaining in Haider's client reducers even though Haider will use replay rather than HTTP snapshots.

Selection behavior is in `packages/tui/src/app.tsx`:

- an explicit `--session` opens that ID;
- `--continue` chooses the most recently updated root session;
- `--fork` invokes the session fork API and opens the new session.

The headless `RunCommand` in `packages/opencode/src/cli/cmd/run.ts` follows the same broad model. Its `session()`, `current()`, and `execute()` helpers get/fork/create the session, subscribe to the event stream, and locally filter events by session ID.

OpenCode's “resume” is therefore persistence plus resource hydration. It is not replay from a durable event offset, and a client cannot say “send every committed event after event N.”

### 1.3 Event streaming and multiple clients

The instance event route is declared in `packages/opencode/src/server/routes/instance/httpapi/groups/event.ts`. `eventResponse()` in `packages/opencode/src/server/routes/instance/httpapi/handlers/event.ts`:

1. allocates a queue for the HTTP request;
2. registers an `EventV2` listener immediately;
3. filters events by directory/workspace;
4. emits `server.connected`;
5. emits `server.heartbeat` every ten seconds; and
6. serializes each item as an SSE `message`.

The global stream is analogous: `eventResponse()` in `packages/opencode/src/server/routes/instance/httpapi/handlers/global.ts` registers one `GlobalBus` callback per connection and adds heartbeats.

This naturally supports multiple simultaneous clients. Each SSE request has its own listener and queue, and a published event is offered to all matching listeners. There is no exclusive session ownership.

The reliability limit is visible in the same code:

- SSE events have no durable `id`;
- there is no `Last-Event-ID` handling;
- there is no acknowledgement;
- there is no per-client cursor;
- the queue is not a durable replay buffer.

`startSSE()` in `packages/tui/src/context/sdk.tsx` retries the global stream with exponential backoff from roughly one to thirty seconds and batches UI delivery, but it supplies no cursor and performs no guaranteed resynchronization. The browser app's `packages/app/src/context/server-sdk.tsx` similarly reconnects without a replay position. Any event committed while a client is disconnected can therefore be absent from that client's live event history until some later resource refetch happens to reconstruct its effects.

For Haider, the lesson is to preserve the one-listener-per-client fan-out semantics but replace the ephemeral SSE queue with `RawEnvelope.seq` replay and explicit lag recovery.

### 1.4 Authentication and localhost assumptions

`packages/opencode/src/server/auth.ts` contains:

- `ServerAuth.Config`, which reads `OPENCODE_SERVER_PASSWORD` and an optional username (default `"opencode"`);
- `required()`;
- `authorized()`; and
- helpers for constructing Basic-auth headers.

The authorization middleware in `packages/opencode/src/server/routes/instance/httpapi/middleware/authorization.ts` parses HTTP Basic credentials (and also recognizes a query token for a special path), returning `401` with `WWW-Authenticate` on failure. If no password is configured, authentication is disabled. `ServeCommand` warns about the missing password but still starts.

This is reasonable for manually operated development servers but is not an adequate security boundary for a daemon with tool-control authority. “Bound to localhost” does not authenticate the OS user, another local process, or a hostile page capable of reaching a loopback WebSocket. Haider should not reproduce the optional-password behavior.

### 1.5 What Haider should take and reject

Take:

- a simple session list/read surface;
- multiple independent subscribers;
- start-live-before-hydrate thinking;
- client-side protection against stale hydration overwriting a newer event;
- clean separation between session resource operations and live events.

Reject:

- an embedded TUI worker as the default runtime owner;
- a broad global event stream that every client must filter;
- reconnect without a durable cursor;
- unbounded per-connection event accumulation;
- unauthenticated localhost operation;
- “continue” chosen from an eventually hydrated list without a durable attach contract.

## 2. Codex app-server

### 2.1 Transport choices and connection lifecycle

`codex-rs/app-server/README.md` documents three relevant modes:

- stdio (`--stdio` or `--listen stdio://`): one JSON object per line;
- TCP WebSocket (`--listen ws://IP:PORT`): one RPC object per WebSocket text frame;
- Unix control socket (`--listen unix://PATH`): WebSocket framing, including HTTP Upgrade, carried over UDS. With `unix://` and no path, the documented default is `$CODEX_HOME/app-server-control/app-server-control.sock`.

The README labels WebSocket support experimental/unsupported. The WS server exposes `/readyz` and `/healthz`. The transport rejects any request containing an `Origin` header, a defensible cross-site safeguard for native clients but one that prevents a normal browser/webview WebSocket client.

Every transport connection must initialize exactly once:

```json
{"method":"initialize","id":1,"params":{"clientInfo":{"name":"example","version":"1"}}}
{"id":1,"result":{"userAgent":"..."}}
{"method":"initialized"}
```

Requests before initialization and duplicate initialization are errors. Capabilities and notification opt-outs are connection-scoped.

The wire types in `codex-rs/app-server-protocol/src/rpc.rs` are an untagged `JSONRPCMessage` union of `JSONRPCRequest`, `JSONRPCNotification`, `JSONRPCResponse`, and `JSONRPCError`. It deliberately resembles JSON-RPC 2.0 while omitting the `"jsonrpc":"2.0"` member. IDs may be strings or signed integers. `codex-rs/app-server-protocol/src/protocol/common.rs` maps method names to strongly typed request and notification variants. Server notifications are flattened into a `ServerNotificationEnvelope` with an optional emission timestamp.

This is a sound application protocol shape: correlated bidirectional requests plus uncorrelated notifications over a full-duplex connection. Haider can adopt the shape without inheriting the JSON-RPC near-compatibility ambiguity.

A typical WS exchange is ordinary JSON, with each object below occupying one text frame:

```json
{"method":"thread/resume","id":11,"params":{"threadId":"thr_123"}}
{"id":11,"result":{"thread":{"id":"thr_123","turns":[]}}}
{"method":"thread/status/changed","params":{"threadId":"thr_123","status":{"type":"active","activeFlags":[]}}}
```

An approval uses the same channel in the opposite direction:

```json
{"method":"item/commandExecution/requestApproval","id":50,"params":{"threadId":"thr_123","turnId":"turn_456","itemId":"item_789","command":"cargo test","cwd":"/workspace"}}
{"id":50,"result":{"decision":"accept"}}
{"method":"serverRequest/resolved","params":{"threadId":"thr_123","requestId":50}}
```

The final command truth arrives later as an `item/completed` notification. The examples are reduced to the stable fields relevant here; the protocol types and README document additional optional approval and environment fields.

### 2.2 Thread list, start, resume, read, fork, and unsubscribe

The v2 thread types are in `codex-rs/app-server-protocol/src/protocol/v2/thread.rs`.

- `thread/start` creates a thread and automatically subscribes the requesting connection.
- `thread/resume` joins an already loaded thread or reconstructs a stored thread, then subscribes the requesting connection.
- `thread/fork` creates a new history-derived thread and subscribes.
- `thread/list` is cursor-paginated and supports sorting and filtering.
- `thread/read` retrieves state without loading/subscribing the thread.
- `thread/unsubscribe` removes only that connection's subscription and reports `notLoaded`, `notSubscribed`, or `unsubscribed`.

Subscription bookkeeping is explicit in `codex-rs/app-server/src/thread_state.rs`. `ThreadStateManagerInner` holds:

- the live connection registry;
- each loaded thread and its `HashSet<ConnectionId>` subscribers; and
- the reverse connection-to-thread mapping.

Functions such as `try_ensure_connection_subscribed()`, `try_add_connection_to_thread()`, `unsubscribe_connection_from_thread()`, and `remove_connection()` keep both indexes consistent.

`ensure_listener_task_running()` in `codex-rs/app-server/src/request_processors/thread_lifecycle.rs` creates one core event listener for each loaded `CodexThread`. For every core event it obtains the current subscriber set and emits through `ThreadScopedOutgoingMessageSender`. When the last subscriber leaves, the thread remains loaded while it is active; when it is both inactive and unsubscribed it is eligible to unload after `THREAD_UNLOADING_DELAY`, currently thirty minutes. Unload eventually produces the appropriate session/thread-end notifications.

This is a better fit than OpenCode's broad instance stream: subscription is a first-class server operation, and fan-out is session-scoped.

### 2.3 The resume/live ordering seam

Codex contains a subtle design worth copying in a different form.

For an already running thread, `resume_running_thread()` in `codex-rs/app-server/src/request_processors/thread_processor.rs` does not independently read state and then subscribe. It sends a `ThreadListenerCommand::SendThreadResumeResponse` to the thread's existing listener. `handle_pending_thread_resume_request()` in `request_processors/thread_lifecycle.rs` then:

1. builds persisted turn history;
2. merges a snapshot of an active in-memory turn if present;
3. adds the connection to the subscriber set;
4. sends the resume response and optional usage/goal snapshots; and
5. calls `replay_requests_to_connection_for_thread()` for unresolved server requests.

Because that command and core events are serialized through the same listener loop, the resume snapshot cannot race with a live notification and leave a hole between them.

Haider needs the same ordering guarantee but should establish it around a durable high-water mark:

```text
subscribe/buffer live
capture committed head H
replay (client_seq, H]
announce caught-up-through H
drain buffered events > H
continue live
```

The operation that observes `H` and registers the live receiver must be serialized with journal append/publication by the per-session actor. A separate “read store, then subscribe” sequence is incorrect.

Codex resume is not a durable event cursor. After a socket loss, the client reconnects, initializes, and calls `thread/resume`; the server reconstructs turns and active state. There is no event acknowledgement or “after notification N.” That makes Codex's barrier implementation a valuable precedent, but not its state snapshot the right Haider API.

### 2.4 Approval round-trips and any-client answers

The app-server makes approvals server-initiated RPC requests. The README shows methods including:

- `item/commandExecution/requestApproval`;
- `item/fileChange/requestApproval`; and
- `item/tool/requestUserInput`.

The request has the ordinary RPC `id` plus thread, turn, and item identifiers. The client replies with the same `id` and a typed result. Command decisions in `codex-rs/app-server-protocol/src/protocol/v2/item.rs` include acceptance, session-wide acceptance, amendments, decline, and cancel.

The important multi-client behavior is in `codex-rs/app-server/src/outgoing_message.rs`:

- `send_request_to_connections()` allocates one server request ID and sends the same request to all current subscribers;
- one pending callback represents the request;
- `notify_client_response()` removes that callback when the first valid reply arrives;
- later replies find no pending callback and cannot win;
- `replay_requests_to_connection_for_thread()` sends still-pending requests to a client that resumes later.

Resolution is also routed back through the thread listener, preserving ordering with thread events, and clients receive a `serverRequest/resolved` notification.

This closely matches Haider's “answer from any attached client” requirement. Haider should improve on it by making arbitration durable rather than only an in-memory callback:

- the menu request is a committed event with stable `menu_id`, request sequence, and worker generation;
- every `MenuAnswer` supplies that identity;
- one store transaction compares “still pending” and appends the answer/resolution;
- the first committed answer wins;
- every attachment observes the same resolution envelope;
- losing clients receive `AlreadyResolved { answer_seq }`.

That makes an answer safe across daemon restart and prevents two transports from independently causing the protected effect.

### 2.5 Transport backpressure and overload

`codex-rs/app-server-transport/src/transport/mod.rs` uses a bounded ingress channel (`CHANNEL_CAPACITY` is 128 at the inspected commit). On saturation, requests can receive a structured `-32001` “Server overloaded; retry later” error rather than accumulating without bound.

`codex-rs/app-server-transport/src/transport/websocket.rs` gives each connection a bounded writer queue. `codex-rs/app-server/src/transport.rs` disconnects a slow WebSocket connection when its outbound queue is full; stdio, which represents one directly owned client, can await writes. This is the right general policy for a shared daemon: one slow viewport must not stall the session actor or other clients.

Haider's lag error should additionally carry the last sequence successfully queued or delivered. The client then reconnects/reattaches from its last applied `RawEnvelope.seq`; the daemon should not preserve an unbounded private backlog for that client.

### 2.6 WebSocket and UDS authentication

The WebSocket implementation is in:

- `codex-rs/app-server-transport/src/transport/websocket.rs`; and
- `codex-rs/app-server-transport/src/transport/auth.rs`.

`authorize_upgrade()` accepts either a capability token or a signed bearer JWT according to configuration. Capability-token configuration can use a secret file or a SHA-256 digest, and comparison is performed through digests rather than plain string equality. JWT mode validates HMAC signature and time/issuer/audience claims. An unauthenticated non-loopback listener is refused.

Loopback listeners may be unauthenticated. Combined with the unconditional `Origin` rejection, this assumes a native/non-browser client and uses “no browser origin” as a major part of the threat reduction.

Haider's GUI requirement changes the answer:

- the GUI webview will normally send an `Origin`;
- the browser WebSocket API cannot set an arbitrary `Authorization` header;
- rejecting all origins is incompatible with the required client;
- allowing the expected origin without authentication is unsafe.

Haider should both validate an exact origin allowlist and require a token at the HTTP Upgrade. The practical options are:

1. carry an opaque token in a `Sec-WebSocket-Protocol` offer alongside `haider.rpc.v1`, with a carefully constrained base64url representation and redaction; or
2. mint a short-lived, single-use WS ticket over UDS/native IPC and present it in the upgrade URL, with query logging disabled and the ticket bound to daemon instance, origin, expiry, and capability.

For a packaged webview, option 2 is cleaner if the native shell can bootstrap the ticket. For a simpler v0.1, subprotocol token transport is acceptable. Sending a token only in the first application message is too late: it accepts and allocates an unauthenticated WebSocket first.

### 2.7 UDS singleton precedent

`codex-rs/app-server-transport/src/transport/unix_socket.rs` provides a strong reference:

- `acquire_app_server_startup_lock()` takes a blocking file lock in `spawn_blocking`;
- `prepare_control_socket_path()` first attempts to connect;
- a successful connect means `AddrInUse`;
- `NotFound` is safe;
- after `ConnectionRefused`, only a path verified by `codex_uds::is_stale_socket_path()` is removed;
- non-socket occupants are refused;
- the parent directory is made private;
- the bound socket is changed to mode `0600`;
- a guard removes the socket on shutdown.

`codex-rs/uds/src/lib.rs` implements `prepare_private_socket_directory()` with Unix mode `0700` and verifies the stale path is actually a socket. It also abstracts a Windows UDS implementation.

The conformance tests in `codex-rs/app-server-transport/src/transport/unix_socket_tests.rs` cover HTTP Upgrade plus WebSocket text and ping/pong, startup-lock serialization, and socket permissions.

One improvement is mandatory for Haider: a path-only drop guard can delete a successor's socket if ownership changes during handover. The current Claude Code changelog explicitly records having fixed that class of bug. Haider's cleanup guard must record the bound socket's identity where the platform permits (device/inode on Unix), re-`lstat` without following symlinks, and unlink only if the path still denotes the socket this process created. At minimum, handover must rename or generation-scope rendezvous files so the old guard cannot target the new endpoint.

### 2.8 Shutdown behavior

`ShutdownState` and its transition logic are in `codex-rs/app-server/src/lib.rs`. The first supported signal requests a graceful drain; a later forceable signal forces exit. The server waits for active turns to reach zero, then cancels listeners, disconnects transports, waits on connection RPC gates, drains cleanup/background tasks, and asks loaded threads to shut down. `wait_for_thread_shutdown()` in `request_processors/thread_lifecycle.rs` has a bounded timeout.

`codex-rs/app-server/src/connection_rpc_gate.rs` tracks in-flight RPC handlers per connection. Closing the gate prevents queued work from beginning while permitting already running handlers to finish. `connection_cleanup.rs` owns cancellable cleanup tasks.

This is good shutdown plumbing. The policy is less suitable for Haider because the Codex drain can continue accepting requests while it waits for turns. Haider should announce and enforce a `Draining` state immediately: no new sessions, turns, tool dispatches, or other mutations after the drain barrier, while read/attach may remain briefly available to receive final committed events.

## 3. Claude Code: what is and is not public

### 3.1 Public implementation boundary

The `anthropics/claude-code` repository's `README.md` describes the product and links external documentation, but the installed CLI implementation is not in the repository. There is consequently no auditable source for its daemon transport, wire framing, attachment algorithm, or authorization checks.

This report does not infer those internals. It uses two narrower bodies of local evidence:

1. the public repository's `CHANGELOG.md`, which names user-visible behavior and fixed failure modes; and
2. the open-source Python Agent SDK, which exposes a documented headless subprocess protocol but not the background daemon.

### 3.2 Changelog evidence about the background daemon

At the inspected commit, `claude-code/CHANGELOG.md` establishes the following product surface:

- `claude --bg` and `/background` move work into background sessions;
- `claude agents` lists/opens those sessions;
- `claude attach <id>` attaches a terminal;
- more than one window may attach to a session;
- leaving one attached window must not detach other windows;
- a control socket and socket auth tokens exist;
- `claude daemon status` and `claude daemon stop --any` exist;
- the daemon maintains a roster and spawns/resumes workers;
- workers can be restored after daemon restart;
- permissions/input can be parked while no client is attached;
- session and daemon versions can skew during auto-update.

Several bug fixes are directly relevant to Haider:

- a stale legacy lockfile plus PID reuse caused a daemon command to target an unrelated process;
- a displaced old daemon deleted its successor's control socket;
- a crash left a stale `daemon.lock` that prevented agents from starting;
- cold-start races produced “socket missing”;
- failure to start the control socket left an unreachable daemon blocking restart;
- socket auth tokens were lost during daemon restart;
- old and new binaries disagreed about worker restart and handover;
- attaching during a worker/daemon transition returned transient “job not found” or “agent is still starting” instead of waiting;
- session tokens went stale and made attach/reply/stop permanently unresponsive;
- one window's Back action detached other windows;
- attach could fail after sleep/wake, upgrade, or binary replacement;
- background workers crash-looped when a client reset a connection;
- idle detached sessions and workers could keep the daemon alive indefinitely.

These entries do not reveal how Claude solves each issue, but they are a valuable negative test inventory. In particular, they reinforce four decisions:

- never use PID identity as singleton proof;
- make socket cleanup ownership-aware;
- expose transitional startup/recovery state instead of returning false “not found”;
- treat version/generation as protocol-visible data and fence stale workers.

The changelog also reports that streaming control requests once became “complete” before their handler finished and could be lost on session restart. This is exactly why Haider's menu resolution and protected-effect transition must be committed to the journal, not represented only by a socket callback.

### 3.3 The public Agent SDK transport

The Python SDK's `ClaudeSDKClient.connect()` in `claude-agent-sdk-python/src/claude_agent_sdk/client.py` creates one `SubprocessCLITransport`, starts one internal `Query`, and initializes it. `SubprocessCLITransport._build_command()` in `_internal/transport/subprocess_cli.py` launches the bundled CLI with:

```text
--output-format stream-json --verbose
--input-format stream-json
```

Depending on options it also supplies:

```text
--continue
--resume=<session>
--session-id=<uuid>
--fork-session
--permission-prompt-tool stdio
```

The transport is newline-delimited JSON over subprocess stdin/stdout. `_LineFramer` and `_parse_stdout_line()` handle streamed output. Closing the SDK client closes stdin, waits for a graceful flush, then escalates through terminate and kill with bounded waits; an `atexit` hook also cleans up children. This is good foreground child-process hygiene, but it has none of the discovery or sharing semantics of a daemon.

The bidirectional control protocol is visible in `_internal/query.py`. `_send_control_request()` writes:

```json
{
  "type": "control_request",
  "request_id": "req_1_ab12cd34",
  "request": {"subtype": "initialize"}
}
```

The CLI answers with:

```json
{
  "type": "control_response",
  "response": {
    "subtype": "success",
    "request_id": "req_1_ab12cd34",
    "response": {}
  }
}
```

The CLI can send its own `control_request`; `_handle_control_request()` handles `can_use_tool`, `hook_callback`, and `mcp_message`. A tool permission answer is returned as `behavior: "allow"` with optional updated input/permissions, or `behavior: "deny"` with a message. `control_cancel_request` cancels an in-flight SDK handler.

This validates correlated full-duplex requests as a practical shape for permission round-trips. It does not support “any attached client”: the callback lives in the single Python process that owns the subprocess.

### 3.4 SDK persistence and resume

The newer Python SDK has a `SessionStore` protocol in `src/claude_agent_sdk/types.py`. A store appends and loads opaque JSONL transcript entries, may list sessions, and may provide subagent keys. Entries with stable UUIDs are recommended as idempotency keys.

`materialize_resume_session()` in `_internal/session_resume.py`:

1. resolves an explicit resume ID or the most recently modified session for continue;
2. loads the full session from `SessionStore`;
3. writes a temporary `CLAUDE_CONFIG_DIR` laid out like the CLI's normal state;
4. materializes subagent transcript data where supported; and
5. starts a new CLI subprocess with `--resume`.

Transcript-mirror frames are copied back to the store during execution. This is session restoration, not live process attachment. There is no client-held event sequence, subscription registry, multi-client fan-out, or reconnect-to-running-subprocess contract in the SDK source.

### 3.5 Claude lessons for Haider

The inspectable SDK suggests:

- newline JSON is easy for a private subprocess protocol;
- correlated control requests make permission callbacks straightforward;
- graceful EOF/terminate/kill escalation matters;
- resume data needs stable idempotency identities.

The changelog suggests a much broader daemon lesson:

- daemon handover, upgrade, stale endpoint cleanup, no-client input, multi-window detach, and worker generation are core correctness paths, not operational polish.

Haider should test those paths from the beginning rather than relying on a later production bug history to discover them.

## 4. Comparative conclusions

| Concern | OpenCode | Codex app-server | Claude public evidence | Haider implication |
|---|---|---|---|---|
| Default runtime owner | TUI worker embeds server | Independent app-server | Background daemon exists, source closed | Independent per-profile daemon |
| Primary client transport | HTTP + SSE | JSONL stdio, WS, WS-over-UDS | SDK NDJSON stdio; daemon unknown | Framed UDS + localhost WS |
| Session subscription | Broad instance stream, local filter | Explicit per-thread subscriber set | Multi-window attach visible | Explicit attachment ID per session/client |
| Resume | HTTP hydration | Reconstructed history/active snapshot | Transcript resume / background restore | Replay `RawEnvelope` strictly after client seq |
| Replay cursor | None | None for notifications | None visible | `RawEnvelope.seq`; no parallel offset |
| Replay/live race | Hydration guards stale overwrite | Serialized listener command closes gap | Unknown | Per-session actor barrier |
| Multi-client fan-out | Listener/queue per SSE request | One listener, subscriber set | Multiple windows | One session hub, bounded client queues |
| Approval/input | REST/events; client-specific policies | Server request to all, first reply wins, replay pending | SDK control callback; daemon parks requests | Durable `MenuAnswer` CAS, any control attachment |
| Slow client | Potential queue growth | Bounded queue; disconnect WS | Client reset bugs noted | Disconnect/lag frame; resume from seq |
| Local auth | Optional Basic password | Optional on loopback; bearer/JWT available | Socket auth tokens exist | Mandatory token for WS; UDS OS boundary + private perms |
| Singleton/socket | Server command, not central model | startup lock + liveness + stale socket check | Many stale-lock/handover fixes | lifetime store lock + probe + identity-safe cleanup |
| Restart truth | Resource state | Thread history/state | Worker restore mentioned | bump generation; reconcile effects unknown before ready |

The systems converge on one point: multiple viewports should not own the agent. They diverge on recovery. Haider's event-sourced journal allows a stronger contract than any surveyed implementation: exact, client-held, persistent resume position.

## 5. Proposed Haider v0.1 protocol

### 5.1 Separate logical frames from transport framing

“Same protocol frames on UDS and WS” should mean the same serialized application-frame body and semantics, not identical lower-level bytes:

- WebSocket: one UTF-8 JSON object in one WS text message.
- UDS: four-byte unsigned big-endian length followed by the same UTF-8 JSON bytes.

UDS is a byte stream and does not preserve write boundaries. A length prefix gives deterministic maximum-size enforcement, tolerates arbitrary read fragmentation, and avoids relying on newline scanning. JSON remains inspectable and makes v0.1 tooling easy. A future binary codec can be capability-negotiated without altering RPC semantics.

Running a WebSocket HTTP Upgrade over UDS, as Codex does, maximizes transport-code reuse but adds an HTTP/WebSocket stack to the trusted local native path. Haider has only two transports and should prefer a small `tokio_util::codec` length-delimited UDS adapter plus a WS adapter around the same `WireFrame`.

The length limit must be checked before allocation. A reasonable starting cap is 8 MiB for ordinary frames, with large artifacts referenced through the CAS rather than embedded. The chosen value belongs in `Welcome`, and oversized input closes the connection with a protocol error.

### 5.2 Use an explicit envelope, not almost-JSON-RPC

Use a visibly versioned tagged union. For example:

```json
{
  "v": 1,
  "kind": "request",
  "id": "01J...",
  "method": "session.attach",
  "params": {
    "sessionId": "ses_...",
    "afterSeq": 418,
    "mode": "control"
  }
}
```

```json
{
  "v": 1,
  "kind": "event",
  "attachmentId": "att_...",
  "sessionId": "ses_...",
  "seq": 419,
  "rawEnvelope": {}
}
```

Recommended top-level variants are:

- `Hello`;
- `Welcome`;
- `Request`;
- `Response`;
- `Event`;
- `AttachCaughtUp`;
- `MenuAnswer`;
- `Lagged`;
- `ServerDraining`;
- `Ping` / `Pong`;
- `ProtocolError`.

`Request`/`Response` support symmetric correlated operations. `Event` remains special because its sequence is part of the durability contract. `MenuAnswer` should be a named frame, as required, even if the daemon internally converts it to a command. Stable textual discriminants and additive optional fields make golden fixtures readable.

Do not call this JSON-RPC unless it is actually compliant. Codex's deliberate omission works in its generated ecosystem, but near-compatibility invites off-the-shelf clients to make incorrect assumptions about error codes, batch requests, notifications, and version fields.

### 5.3 Handshake and connection identity

The first application frame must be:

```text
Hello {
  protocol_min,
  protocol_max,
  client_name,
  client_version,
  client_instance_id,
  requested_capabilities,
  max_receive_frame
}
```

The daemon responds:

```text
Welcome {
  selected_protocol,
  daemon_instance_id,
  daemon_generation,
  profile_id,
  daemon_version,
  granted_capabilities,
  max_frame,
  lifecycle_phase
}
```

Authentication occurs before this handshake for WS and through endpoint access/optional peer credentials for UDS. The handshake selects protocol features; it does not carry the primary WS secret.

`daemon_instance_id` is random per process start. It lets clients distinguish a transient connection loss from a daemon restart; `daemon_generation` is the durable per-profile restart/recovery epoch if Haider maintains one. `worker_generation` remains session/execution-scoped and is exposed by attach state and `RawEnvelope`, not falsely flattened into one connection-wide value. Do not conflate process instance, daemon generation, authority epoch, worker generation, and event sequence.

Request IDs are scoped to a connection and should be opaque client-generated strings. Mutation requests also need a durable `command_id`/idempotency key because a response can be lost after the command commits. Retrying a socket request ID on a new connection is not sufficient deduplication.

### 5.4 Session list and metadata

Keep listing separate from attachment:

```text
SessionList { cursor?, limit, filter?, sort? }
SessionRead { session_id }
SessionAttach { session_id, after_seq, mode }
SessionDetach { attachment_id }
```

`SessionList` should be cursor-paginated like Codex, with cursor meaning defined by a stable ordering key rather than an array index. `SessionRead` can return cheap metadata and the committed head sequence without loading a live worker. Neither operation subscribes the connection.

`SessionAttach` is the only operation that begins event delivery. The response should include:

```text
attachment_id
session_id
requested_after_seq
replay_through_seq
current_worker_generation
authority identity
```

One connection may attach to more than one session if the GUI needs a dashboard, but the server must schedule outgoing frames fairly so a hot session cannot starve the others.

### 5.5 Snapshot-free attach and reconnect

The required flow is:

1. The client connects and completes `Hello`/`Welcome`.
2. It sends `SessionAttach { session_id, after_seq }`, where `after_seq` is the greatest sequence it has fully applied, or zero for the complete history.
3. The session actor atomically registers a live receiver and captures committed head `H` in its serialized command loop.
4. A replay task reads store pages for `(after_seq, H]` and sends them in ascending sequence.
5. Events committed after `H` are buffered in a bounded catch-up buffer or remain available through a broadcast receiver.
6. The server sends `AttachCaughtUp { through_seq: H }`.
7. It drains buffered events with `seq > H`, dropping duplicates by sequence, then switches to live delivery.
8. The client advances its durable cursor only after applying each event.

Two invariants make this correct:

```text
persist event before publish
register receiver + observe H in the same session-actor order as append/publish
```

If the catch-up receiver reports lag, or its bounded buffer fills, the daemon discards that receiver and resumes the same attachment from the store after `last_sent_seq`. It does not fail the session and does not allocate unbounded memory.

The client must treat `seq <= last_applied` as a duplicate and ignore it. A received `seq > last_applied + 1` is a gap: stop applying that attachment and request replay after `last_applied`. This yields at-least-once delivery with an exactly-once reducer effect by sequence, which is the achievable and appropriate contract.

Expected edge responses include:

- `SessionNotFound`;
- `CursorAhead { requested, head }`;
- `AttachmentNotFound`;
- `CapabilityDenied`;
- `DaemonDraining`;
- a retention error in the future if full history is ever compacted away.

Because v0.1 is explicitly snapshot-free, replay may be long. Use store paging and emit catch-up progress, but do not invent a hidden state snapshot that changes the semantic contract. CAS payloads should stay out of envelopes and load on demand.

### 5.6 Multiple clients and backpressure

Each connection should have:

- a bounded inbound command queue;
- a bounded outbound writer queue;
- a cancellation token;
- a task tracker for in-flight handlers;
- an authorization/capability context;
- a map of its attachment IDs.

Each session actor should have a map of attachment metadata, but not an unbounded event queue per client. Publication enqueues into bounded connection writers. A writer that cannot keep up receives `Lagged { attachment_id, last_queued_seq }` if that frame itself can be queued, then is detached or the whole connection is closed. Reattachment from the client's applied cursor is the recovery path.

Do not await a slow socket write from the session actor. Do not let one connection's queue combine unlimited output from many sessions. Apply per-session quotas or fair round-robin scheduling at the connection writer.

Presence is connection/attachment metadata, not a durable session event unless product semantics specifically need an audit trail. Attaching and detaching must not change session authority or worker ownership.

### 5.7 `MenuAnswer` from any attachment

A pending menu must be durable:

```text
menu_id
session_id
request_seq
kind
options/version
blocking_scope
worker_generation
status = pending | resolved | expired | cancelled
```

The incoming frame should include at least:

```text
MenuAnswer {
  command_id,
  session_id,
  menu_id,
  request_seq,
  worker_generation,
  selected_option,
  optional_input
}
```

The daemon validates that:

- the connection has `control`;
- the connection is attached to the session, unless policy explicitly permits a controller without a viewport;
- the menu is still pending;
- the option matches the committed menu version;
- the generation/authority is not stale.

It then performs one transactional compare-and-set and appends the answer/resolution envelope. The event, not the socket response, wakes the harness. Concurrent answers have exactly one committed winner. Losers receive an ordinary correlated response such as `AlreadyResolved { resolution_seq }` and then observe that same resolution through replay/live delivery.

Pending menus survive with the session. A client that attaches after the prompt was raised learns about it through `RawEnvelope` replay; no separate best-effort pending-request cache is needed. A daemon restart must not resend a protected effect merely because the in-memory waiter disappeared.

This design generalizes Codex's first-callback-wins mechanism into an event-sourced, crash-safe rule.

## 6. Rust daemon plumbing

### 6.1 Tokio UDS server

On Unix, the basic accept loop should use `tokio::net::UnixListener`:

```text
prepare private runtime directory
acquire singleton/store lock
probe and validate rendezvous path
bind listener
chmod socket 0600
publish discovery metadata
loop select(listener.accept(), shutdown.cancelled())
spawn one supervised connection task per accepted stream
```

The framed stream can use a small `tokio_util::codec::Decoder`/`Encoder` around the four-byte length prefix. `Framed` is convenient, but cancellation and partial writes still need tests. JSON decoding belongs after frame-size validation.

Use an owner-private directory (`0700`) and a socket path with mode `0600`. Resolve the profile directory from an explicit, normalized profile identity. Never accept an arbitrary socket cleanup target from an environment variable without verifying it lies under that private directory.

Unix socket paths have short platform-specific limits, notably around a hundred bytes on common systems. Put the runtime directory close to the filesystem root supplied by the OS and derive a fixed-length profile hash for the filename. Preserve the full profile ID in discovery metadata rather than in a long socket basename.

Peer credentials are useful defense in depth. On Linux, inspect `SO_PEERCRED`; on BSD/macOS use the available peer-UID facility. Refuse a different UID where supported. They do not replace file permissions or protocol authentication on WS and should sit behind a small platform abstraction.

If Windows is a v0.1 target, do not pretend POSIX UDS semantics and ACLs are portable. Hide local IPC behind `LocalListener`/`LocalStream` and use a Windows implementation with explicit user-only ACLs (named pipes or a proven Windows UDS adapter). Keep the application framing identical and run the same transport conformance suite.

### 6.2 Filesystem sockets versus abstract sockets

Linux abstract namespace sockets avoid stale filesystem nodes, but they are the wrong primary endpoint for Haider:

- they are Linux-specific and unavailable on macOS;
- they have no filesystem ownership/mode boundary;
- discovery and inspection are less transparent;
- profile rendezvous is not represented in the user's private runtime directory;
- test and support behavior would diverge by OS.

Use a filesystem UDS. Its stale-node cost is manageable with a lock, liveness probe, safe `lstat`, and identity-aware cleanup. Abstract sockets may be a later Linux optimization for a private subordinate channel, not the public per-profile endpoint.

### 6.3 Singleton and CLI auto-start algorithm

The existing `haider-store` singleton is the source of truth and should be held for the whole writer lifetime, not merely during bind. Socket liveness is a discovery check, not the authority lock.

The lock file itself may persist forever. Its existence does not mean a daemon is alive; the kernel-held advisory lock does, and it is released when the owning process/file description dies. Never “clean up” or replace the lock pathname while a process may hold it, because two processes can then lock different inodes under the same apparent name.

Daemon startup:

1. Resolve and normalize the profile, store path, runtime directory, socket path, lock path, and discovery path.
2. Try to acquire the existing exclusive store/singleton lock without waiting indefinitely. Keep a successful guard in the top-level runtime owner until shutdown is complete.
3. With the lock held, probe the expected socket.
4. If the connection succeeds and the peer completes a short `Hello`/profile check, treat this as an invariant violation or a race winner already serving the profile; do not unlink anything.
5. If the path is absent, bind.
6. If connection returns `ECONNREFUSED`, use `symlink_metadata`/`lstat`; require an actual socket owned by the expected user inside the expected private runtime directory. Refuse symlinks, regular files, directories, and unknown ownership. Only then unlink.
7. Bind, capture the socket identity, set permissions, start acceptors, recover the store, and only then atomically publish ready discovery metadata.

There are two common race patterns:

- two CLIs both decide to auto-start;
- an old socket survives a crash while a new daemon starts.

The lifetime store lock decides the daemon winner. The losing `haiderd` should return a distinct “already running” exit status, after which the launching CLI retries UDS attachment. No PID kill or stale-file guess is necessary.

If the try-lock reports busy before the winner has published its socket, the loser must not inspect or remove the rendezvous path. It should briefly poll the expected handshake for diagnostics, then exit “already running/recovering.” Only the lock holder may decide that a socket node is stale.

CLI startup:

1. Attempt UDS connect first.
2. On a successful handshake, attach.
3. On absent/refused endpoint, spawn `haiderd --profile <canonical-profile>` without forwarding secrets on argv.
4. Poll the UDS/health handshake with a bounded deadline and short backoff.
5. If another process won, attach to it.
6. If startup reports recovery failure, surface the daemon's durable diagnostic rather than looping spawns.

The discovery file can contain non-secret facts such as:

```text
profile_id
daemon_instance_id
pid (diagnostic only)
protocol range
UDS path
WS port
startup phase
start timestamp
```

Write it atomically after bind and update it on phase transitions. PID is useful for diagnostics but never proves ownership or liveness. WS tokens should live in a separate `0600` secret file or native credential handoff and must not be copied into broadly readable metadata.

`--standalone` is required to run in-process. It must nevertheless acquire the same store/singleton lock and writer authority before opening the journal. If a daemon is live for the profile, strict standalone should fail with an explicit “profile already served; stop daemon or choose another profile” message. Silently attaching would violate the stated in-process meaning; opening a second writer would violate the more important journal invariant.

### 6.4 Socket cleanup and handover

The cleanup guard should own:

- canonical socket path;
- daemon instance/generation;
- on Unix, the device/inode identity observed after bind;
- a boolean recording that this process successfully bound it.

At cleanup, `lstat` the path and remove it only if its identity still matches. A successor may have rebound the path during a controlled handover; an old process must never delete that socket. Discovery/secret files need the same instance-aware replacement/removal rule.

For a future zero-downtime upgrade, prefer a protocol-level handover:

1. old daemon enters `DrainingForUpgrade`;
2. it stops mutations and workers at a durable barrier;
3. it releases the listener/store in an ordered exchange;
4. the new daemon acquires the lifetime lock, bumps instance/generation, recovers, and binds/publishes;
5. old attachments reconnect by cursor.

Do not design v0.1 around inheriting listener file descriptors unless uninterrupted sockets are a firm requirement. Event-cursor reconnect makes a short endpoint gap cheap and far easier to reason about.

### 6.5 Localhost WebSocket security

Bind numeric loopback addresses only:

- `127.0.0.1`;
- optionally `[::1]` as a separate listener.

Do not bind `0.0.0.0`, `[::]`, or a hostname that may resolve unexpectedly. Generate at least 256 random bits per daemon instance for the initial control token. Store it owner-only, never print it, never put it on daemon argv, redact it and tickets from tracing, and rotate it on daemon restart.

Authentication must be required even on loopback. Threats include another local user/process, a compromised local application, browser-origin attacks, and accidental port exposure through containers/proxies. The eventual `view|control` capability split should be represented in the authentication context now:

```text
Principal {
  token_id,
  profile_id,
  capabilities: { view | control },
  expires_at?,
  origin?,
}
```

v0.1 may issue one control-scoped token, but authorization checks should still call `require(View)` or `require(Control)` at method boundaries. This avoids retrofitting every handler later.

For the webview:

- validate the exact expected `Origin` values for the chosen GUI framework;
- reject absent/`null` origins unless that is an explicitly documented platform case;
- regard Origin as a CSRF-style signal, not identity;
- validate the token/ticket during Upgrade;
- cap handshake headers and application frame sizes;
- rate-limit failed handshakes;
- use ping/pong and an idle timeout;
- serve no general HTTP content on the RPC port beyond a minimal non-secret health result, if any.

If `Sec-WebSocket-Protocol` carries the token, the server should select only the public protocol name (`haider.rpc.v1`) and avoid echoing a secret-bearing subprotocol. Confirm framework behavior because some libraries require selecting one of the offered strings. A one-time UDS-minted ticket is preferable where the native GUI shell can provide it.

### 6.6 Graceful shutdown

Use an explicit state machine:

```text
Running -> Draining -> Finalizing -> Stopped
                    \-> Forced
```

On the first termination request:

1. Atomically enter `Draining`.
2. Stop auto-start discovery from advertising the daemon as available for new work, or mark it draining.
3. Stop accepting new WS/UDS connections, or accept only long enough to return `DaemonDraining`.
4. Reject new sessions, turns, effect dispatches, worker starts, and other mutations.
5. Send `ServerDraining { reason, deadline, daemon_instance_id, daemon_generation }` to all attachments; each session's final envelopes retain their own worker generation.
6. Allow already active work a bounded grace period to reach a semantic checkpoint.
7. Continue persisting and broadcasting terminal envelopes during that period.
8. Resolve not-yet-dispatched work as cancelled where that is semantically true.
9. For dispatched effects whose outcome cannot be established, append `EffectOutcomeUnknown`; never label them cancelled merely because shutdown timed out.
10. Flush/checkpoint the store, stop workers, close the session broadcasters after their final committed sequence, and close transports.
11. Close WebSockets with `1012 Service Restart` for an upgrade/restart or `1001 Going Away` for a normal stop.
12. Wait for connection task trackers with a bound, cancel the remainder, remove only owned rendezvous files, then release the store/singleton lock last.

Pending menus normally remain pending durable state. Shutdown should not invent a denial. If the pending question guards work that is being explicitly cancelled, append the normal cancellation events so replay explains why the menu is no longer actionable.

A second signal or expired global deadline enters `Forced`: kill worker process trees, cancel tasks, flush what can be flushed, and exit. Startup recovery must assume the forced path could have interrupted any operation.

The accept loop, connection tasks, session actors, worker supervisors, and store flusher should all be owned by explicit task trackers or `JoinSet`s. Detached tasks make it impossible to know when releasing the lock is safe.

## 7. Crash recovery and the C4a seam

### 7.1 Startup readiness phases

The daemon should expose:

```text
starting
opening_store
reconciling
starting_workers
ready
draining
failed
```

It may bind early so auto-start clients can observe progress, but it must reject control operations until recovery is complete. Attach/list can either wait server-side or return `NotReady { phase, retry_after }`; the client should not translate a recovering session into “not found.”

### 7.2 Required recovery ordering

With the singleton held:

1. Open and validate the store.
2. Establish a new `daemon_instance_id`.
3. Durably increment `worker_generation` for every recovered execution/worker lineage before starting its replacement worker.
4. Scan effects in a dispatched/started state without a terminal outcome.
5. For each effect belonging to a prior/incomplete generation, reconcile according to effect class and durable evidence.
6. Where outcome cannot be proven, append `EffectOutcomeUnknown`.
7. Make reconciliation idempotent, keyed by effect identity and prior generation or enforced by a unique terminal-outcome rule.
8. Restore/reduce session actors from the event journal.
9. Only then mark ready and allow new mutation/worker execution.

An automatic retry of an ambiguous external side effect is prohibited. Read-only or provably idempotent operations may have a separate policy, but the durable record must state why replay is safe. A new worker receives the incremented generation; facts or `MenuAnswer`s tied to an old worker generation are fenced.

Clients reconnecting during or after recovery use the same last-applied sequence. They will observe the appended unknown-outcome envelopes in order. No special recovery snapshot or out-of-band warning is needed, although the GUI may project those envelopes into a prominent recovery menu.

### 7.3 Publication rule

The store commit is authoritative:

```text
validate command
append transaction
commit
advance in-memory reducer/head
publish RawEnvelope
```

Never publish and then persist. If publication fails after commit, subscribers recover from the store. If the process fails before commit, there is no event to replay. This simple asymmetry is what makes the cursor contract possible.

If a command appends multiple envelopes atomically, publish them in their committed sequence order only after the transaction succeeds. An attaching client may read the transaction from the store while a live client receives publication, but both converge by sequence.

## 8. Proposed crate and file decomposition

Keep wire definitions and codecs separate from daemon ownership. A practical workspace layout is:

```text
crates/
  haider-rpc/
    Cargo.toml
    src/
      lib.rs
      version.rs
      frame.rs
      method.rs
      error.rs
      capability.rs
      handshake.rs
      codec.rs
      client.rs
      transport/
        mod.rs
        uds.rs
        websocket.rs
    tests/
      golden_frames.rs
      codec_limits.rs
      transport_conformance.rs
      uds_conformance.rs
      websocket_conformance.rs

  haider-daemon/
    Cargo.toml
    src/
      lib.rs
      config.rs
      profile.rs
      runtime.rs
      singleton.rs
      discovery.rs
      readiness.rs
      listener.rs
      connection.rs
      authorization.rs
      session_hub.rs
      attachment.rs
      menu_router.rs
      recovery.rs
      shutdown.rs
      worker_supervisor.rs
    tests/
      autostart_race.rs
      attach_replay.rs
      menu_race.rs
      crash_recovery.rs
      graceful_shutdown.rs

  haider/
    src/
      bin/
        haider.rs
        haiderd.rs
      daemon_client.rs
      daemon_autostart.rs
      standalone.rs
```

If the product mandate is literally one distributed executable, `haiderd` can still be a Cargo binary target backed by `haider-daemon`; packaging may expose it through the main executable's `daemon` role or install a hardlink/shim. The important part is that `src/bin/haiderd.rs` is thin: parse arguments, initialize tracing, resolve the profile, call `haider_daemon::run()`, and map exit status.

Responsibilities:

- `haider-rpc/frame.rs`: only stable serialized frame types.
- `version.rs` and `handshake.rs`: negotiation and feature advertisement.
- `codec.rs`: maximum-sized length-delimited UDS body and JSON encode/decode.
- `capability.rs`: `View`/`Control` vocabulary shared by transports and handlers.
- `client.rs`: reconnect, request correlation, attach cursor, and orderly detach; no TUI state.
- `transport/*`: stream/message adaptation only, not session policy.
- `haider-daemon/singleton.rs`: lifetime guard and safe socket preflight/cleanup.
- `discovery.rs`: atomic public metadata and secret/ticket location.
- `connection.rs`: handshake, request dispatch, bounded writer, task gate.
- `session_hub.rs`: actor registry and committed-event publication.
- `attachment.rs`: replay/live barrier and lag recovery.
- `menu_router.rs`: `MenuAnswer` authorization and durable compare-and-set.
- `recovery.rs`: generation advance and C4a effect reconciliation.
- `shutdown.rs`: drain state machine and ordered teardown.

`haider-rpc` should not depend on `haider-store`, the provider loop, or daemon runtime. It may depend on the crate that owns stable `RawEnvelope` wire types. `haider-daemon` depends inward on RPC, store, domain reducers, harness, and worker supervision. Store locking remains in `haider-store`; daemon singleton orchestration wraps and holds that guard rather than duplicating it.

The in-process standalone path should invoke the same `haider-daemon` host core without network listeners, using an in-memory command/event adapter that satisfies the same internal interface. This is how “identical runtime” remains testable without pretending standalone is a socket client.

## 9. Testing strategy

### 9.1 Protocol and codec

- Golden JSON fixtures for every frame variant and error.
- Round-trip tests for current protocol and accepted older fixtures.
- Unknown optional fields are ignored; unknown required versions/kinds fail clearly.
- Property/fuzz tests for fragmented length prefixes, truncated bodies, invalid UTF-8/JSON, declared length overflow, and maximum frame size.
- Request-response correlation with out-of-order responses and duplicate/unknown IDs.
- Durable `command_id` deduplication across two connections.
- Capability tests proving view can list/read/attach but cannot submit `MenuAnswer` or mutations.

### 9.2 Shared transport conformance

Define one black-box suite parameterized by a connector:

```text
connect -> Hello/Welcome
request -> response
attach -> ordered replay -> caught-up -> live event
server request/notification interleaving
client detach
server draining
malformed/oversized frame
slow reader
disconnect and resume
```

Run the same transcript over UDS and WS and compare decoded logical frames, not handshake bytes. For UDS, deliberately split every possible length-prefix byte and body at irregular boundaries and combine multiple frames in one write. For WS, test one object per text message, reject binary/continuation policy violations as chosen, and exercise ping/pong/close codes.

### 9.3 UDS conformance and singleton

At minimum:

- runtime directory becomes `0700`;
- socket becomes `0600`;
- peer UID is accepted/refused as applicable;
- live socket is never unlinked;
- refused stale socket is removed only after it is verified as a socket;
- regular file, directory, and symlink at the path are refused;
- overlong paths fail with a clear diagnostic or use the fixed hashed name;
- two simultaneous daemon starts produce exactly one owner;
- two simultaneous CLI auto-starts both attach to that winner;
- killed daemon leaves a node that the next guarded startup safely replaces;
- a cleanup guard does not delete a replacement socket with a different inode/generation;
- failure between lock, bind, chmod, metadata publish, and ready leaves recoverable state;
- standalone and daemon cannot both acquire the profile writer lock;
- Windows local-IPC behavior runs the same suite behind its platform adapter if supported.

Run permission tests with a controlled umask and do not assume the default environment is secure.

### 9.4 Attach/replay concurrency

Use a deterministic fake store and controllable session actor to inject events:

- entirely before attach;
- between receiver registration and high-water capture;
- during every replay page;
- immediately before/after `AttachCaughtUp`;
- during buffered-to-live transition.

For every schedule, the client must apply exactly the contiguous sequence `(after_seq, final_head]`, with no omissions and harmless duplicates only. Also test:

- `after_seq = 0`;
- `after_seq = head`;
- cursor ahead of head;
- nonexistent session;
- empty session;
- disconnect during replay;
- reconnect after the last applied seq;
- broadcast lag during replay and live delivery;
- store paging boundaries;
- two and many simultaneous clients with different cursors;
- one slow client while others remain current;
- one connection attached to several hot sessions.

Property-test the client reducer: arbitrary duplicate delivery preserves state, while a gap is detected before applying later events.

### 9.5 Menus and approvals

- N control clients answer the same menu concurrently; exactly one answer event commits.
- Losing clients receive `AlreadyResolved` with the winning sequence.
- View-only token cannot answer.
- A client attaching after menu creation sees it in replay and may answer.
- Disconnect after answer commit but before response; retry with same `command_id` returns the committed result.
- Restart with a pending menu preserves it.
- Restart after answer commit but before worker wake does not execute the protected transition twice.
- An answer naming stale `request_seq`, worker generation, option version, or authority is rejected.
- Detaching one client never resolves/cancels a session-wide menu for the others.

### 9.6 Crash recovery

Inject process failure at every C4a boundary:

```text
before dispatch intent commit
after dispatch intent commit / before external call
during external call
after external return / before terminal commit
after terminal commit / before response/publication
```

After restart, assert:

- each recovered execution's `worker_generation` increased durably;
- every dispatched-without-terminal effect is reconciled exactly once;
- an unknowable outcome becomes exactly one `EffectOutcomeUnknown`;
- terminal effects are not reclassified;
- ambiguous non-idempotent effects are never automatically repeated;
- reconnecting clients observe the recovery envelopes through normal sequence replay;
- stale worker output and stale `MenuAnswer` are fenced.

Use both unit-level fault injection and subprocess kill tests against a real store.

### 9.7 Shutdown and WS security

Shutdown cases:

- idle daemon;
- active inference;
- before and after effect dispatch;
- pending menu;
- slow client;
- connection handler in flight;
- first signal graceful, second signal forced;
- drain deadline expiry;
- final event delivered before connection close;
- socket removed and lock released only after store/worker finalization.

WebSocket cases:

- only `127.0.0.1`/`::1` listeners are created;
- no, malformed, wrong, expired, replayed, and correct tokens;
- correct/wrong/absent/`null` Origin;
- ticket bound to the wrong daemon instance or capability;
- query/header/log redaction;
- oversized handshake and frame;
- repeated authentication failure rate limiting;
- view token denied on control methods;
- token rotation on restart and clean GUI re-bootstrap.

Use `tokio::time::pause()` for deadlines/heartbeats, model concurrency invariants with `loom` where practical, and keep the transport suite independent of wall-clock sleeps.

## 10. Risks and trade-offs

### Replay cost

Snapshot-free replay is the stated contract and the cleanest correctness model, but an old client may read a large journal. Paging, CAS references, and progress reporting mitigate memory and latency. If snapshots are added later, make them optional verified accelerators whose base sequence is explicit; never make them a second source of truth.

### JSON and high-volume deltas

JSON is appropriate for v0.1 observability. Provider token deltas may make it CPU/bandwidth-heavy. Measure before adding compression or a binary codec. A negotiated binary encoding must preserve the exact logical `RawEnvelope` and sequence semantics.

### One token in a webview

A long-lived control token exposed to JavaScript raises the consequence of a webview content injection. Prefer a native shell that mints a short-lived, origin-bound ticket and keep remote navigation disabled. Capability checks and a future view-only token limit damage but do not replace webview hardening.

### Store lock scope

Holding the existing singleton lock for the whole daemon lifetime is intentionally conservative. It prevents daemon/standalone split-brain. It may require maintenance commands to run through RPC instead of opening the store directly; that is the correct direction for a daemon-owned profile.

### Drain versus availability

Rejecting new mutations as soon as drain starts may interrupt an auto-update workflow more visibly than Codex's permissive drain. It produces a much stronger boundary: everything accepted before the barrier is accounted for, and nothing accepted after it can prolong shutdown indefinitely.

## RECOMMENDATIONS

1. **Make `haider-store`'s lifetime lock the singleton authority.** Acquire it before stale-socket cleanup or store open, retain it through worker and store shutdown, and release it last. Treat socket liveness and PID only as discovery/diagnostic signals.

2. **Use filesystem UDS as the primary v0.1 endpoint.** Place a fixed-length profile-derived socket name under a `0700` per-user runtime directory, set the socket to `0600`, check peer UID where available, and do not use Linux abstract sockets for the public endpoint.

3. **Make stale endpoint cleanup conservative and ownership-aware.** Probe first; after `ECONNREFUSED`, unlink only an `lstat`-verified socket owned by the expected user under the expected directory. Record device/inode or an equivalent generation identity after bind, and let a cleanup guard remove only that exact socket. Add the “old daemon must not delete successor socket” case before handover work begins.

4. **Implement CLI auto-start as connect, spawn, then connect again.** Concurrent launchers may all spawn, but the store lock chooses one daemon; losers exit “already running,” and every CLI polls the same readiness handshake. Surface `recovering` and `failed` phases rather than converting transient startup into session-not-found errors.

5. **Keep `--standalone` genuinely in-process but under the same exclusion rule.** Reuse the daemon host core through an in-memory adapter. If the profile daemon holds the store lock, fail standalone explicitly; never open a second journal writer and never silently turn strict standalone into a daemon attachment.

6. **Define one versioned logical `WireFrame` union.** Use explicit `Hello`, `Welcome`, `Request`, `Response`, `Event`, `AttachCaughtUp`, `MenuAnswer`, `Lagged`, `ServerDraining`, and protocol-error variants. Do not claim JSON-RPC compatibility. Negotiate a version/capabilities range and expose daemon instance/generation, frame limit, and lifecycle phase in `Welcome`; expose session/execution-scoped worker generation in attach state and `RawEnvelope`.

7. **Encode the same JSON frame body on both transports.** Send one object per WS text message. On UDS, prefix the UTF-8 JSON bytes with a four-byte big-endian length and enforce the limit before allocating. Keep large artifacts in CAS and reference them from envelopes.

8. **Make attachment explicit and session-scoped.** Provide paginated `SessionList`, non-subscribing `SessionRead`, `SessionAttach { session_id, after_seq, mode }`, and `SessionDetach`. Return a unique attachment ID. Do not broadcast all profile events and require every client to filter them.

9. **Use `RawEnvelope.seq` as the only replay cursor.** The client sends its greatest fully applied sequence. The daemon sends committed envelopes strictly after it. Do not add an SSE-style ephemeral event ID, notification counter, or snapshot generation as a competing resume position.

10. **Close the replay/live race inside the per-session actor.** Register the live receiver and capture durable high-water `H` in the same serialized order as append/publication; replay `(after_seq, H]`, send `AttachCaughtUp(H)`, drain buffered `> H`, then go live. Persist before publish. Add deterministic tests for an append at every boundary.

11. **Promise at-least-once delivery with contiguous, idempotent application.** Clients ignore sequences already applied, stop on a gap, and reattach after the last applied sequence. Advance/persist a client cursor only after reducing an event. This is simpler and more honest than claiming exactly-once network delivery.

12. **Bound all connection queues and make the store the lag buffer.** Never block a session actor on a socket. On slow-client overflow, emit `Lagged` if possible and detach/close that client; let it resume by sequence. Fairly multiplex several session attachments on one GUI connection.

13. **Make `MenuAnswer` a durable compare-and-set command.** Include `command_id`, `session_id`, `menu_id`, original request sequence, worker generation, and selected option. Authorize `control`, validate freshness, and atomically append one resolution. First committed answer wins; losers get `AlreadyResolved` and all clients learn the result through the event stream.

14. **Model `view|control` capabilities in v0.1 even if one control token ships first.** Centralize method authorization now. UDS local identity may be granted according to profile policy; every localhost WS connection must authenticate explicitly.

15. **Secure WS for a browser/webview at Upgrade time.** Bind only numeric loopback, require a 256-bit token or short-lived UDS-minted ticket, validate exact expected Origin, redact secrets, and cap/rate-limit handshakes. Prefer single-use, origin/instance/capability-bound tickets; use a constrained `Sec-WebSocket-Protocol` token only as the simpler fallback. Never rely on loopback or Origin alone.

16. **Complete C4a recovery before ready.** Open the store under the singleton, durably bump `worker_generation` for every recovered execution, reconcile every dispatched-without-terminal effect, append `EffectOutcomeUnknown` exactly once where truth is unknowable, restore reducers, then enable mutations/workers. Never automatically retry an ambiguous non-idempotent effect.

17. **Adopt an explicit drain barrier.** On first shutdown signal enter `Draining`, reject new mutations/effect dispatch, notify attachments, and allow bounded completion to checkpoints. Append true terminal/cancel/unknown outcomes, flush the store, close workers and attachments, remove owned rendezvous files, and release the lock last. A second signal forces termination and leaves recovery to the next generation.

18. **Split the implementation into wire, runtime library, and thin binaries.** Put stable frames/codecs/transports/client behavior in `haider-rpc`; singleton, connection, session hub, attachment barrier, menu routing, recovery, and shutdown in `haider-daemon`; keep `haiderd` as a thin entry point. Let standalone reuse `haider-daemon` internally without listeners. Do not put session policy in transport code or daemon orchestration in `haider-store`.

19. **Ship a parameterized UDS/WS conformance suite with the crate.** Run the same decoded transcript over both transports, then add UDS-specific fragmentation, permissions, stale-path, simultaneous-start, abrupt-death, path-length, peer-identity, and replacement-socket tests. Treat these as protocol release gates, not platform smoke tests.

20. **Add concurrency and failure testing at the semantic seams.** Cover replay/live interleavings, many cursors, slow clients, N-way menu-answer races, lost responses after commit, and process death at every effect-dispatch boundary. Assert contiguous event histories, one durable menu winner, generation fencing, and exactly-once unknown-outcome reconciliation.

21. **Use OpenCode and Codex selectively as regression references.** Preserve OpenCode's stale-hydration guard and simple multiple-listener UX; preserve Codex's explicit subscriptions, typed duplex RPC, serialized resume barrier, bounded queues, and pending-approval replay. Deliberately reject both products' absence of a durable event cursor and reject optional localhost authentication.

22. **Turn Claude's public daemon bug history into named tests.** Include stale PID reuse, cold-start “socket missing,” old-daemon/successor cleanup, failed listener startup, token loss on restart, version-skew attach, one-window detach affecting another, client reset, sleep/wake, and no-client pending-input cases in W3's acceptance matrix.
