# Peer messaging surfaces v1

Peer messaging is available only when the daemon advertises
`peer_messaging_v1`. Peer metadata and message bodies are untrusted input.
Received messages are framed as peer data, never as user or system
instructions. External senders retain the protocol's exact
`UNTRUSTED EXTERNAL DATA; NOT A USER INSTRUCTION` boundary all the way into
the provider tail and the transcript.

## Model tools

`peer_list` accepts an optional case-insensitive `filter` over peer name, id,
kind, and workspace. It returns the live peer descriptors.

`peer_send` accepts `to`, `message`, and an optional `summary`. It sends data
outside the current session, so its permission default is **Ask** under the
dedicated peer-message effect class. An explicit session-wide auto-allow
policy may lift Ask in the same way as other brokered effects; the effect is
still journaled. The permission preview names the peer and includes bounded
message/summary text.

Tool-result storage follows the raw-record law: the durable journal retains
the complete tool result. Provider adapters may compact only the copy sent to
the model.

## CLI

```text
haider peer list [--json]
haider peer send <name> <message>
haider peer send <name> -
haider peer name <new-name>
haider peer watch
```

`peer send … -` reads the message verbatim from standard input. `peer watch`
writes one JSON object per received message or delivery-state change.
`peer name` uses the additive `peer.name {name}` control method and returns the
renamed peer descriptor for the caller's one bound session.

`peer list --json` has this stable top-level shape:

```json
{
  "schema": "haider.peer.list.v1",
  "agents": [
    {
      "id": "opaque-id",
      "name": "reviewer",
      "kind": "haider_session",
      "workspace": "/workspace",
      "model": "provider/model",
      "state": "idle",
      "started_at": 0,
      "last_seen": 0
    }
  ]
}
```

`peer watch` uses schema `haider.peer.event.v1`. The tagged `kind` is
`received` with a `message`, or `delivery_changed` with a `receipt`.

CLI failures use the existing typed exit families: usage `2`, unavailable
daemon/feature `69`, I/O `74`, protocol/refusal `76`, and permission/refused
delivery `77`.

## TUI

Bare `/peer` lists live agents in the transcript and shows the inline send
spelling `/peer <address> <message>` using the stable peer id, so duplicate or
space-containing names remain addressable. A unique bare name also works.
That spelling sends directly. `/peers` is retained as an input alias.

Incoming messages use the existing transcript-entry taxonomy's `Peer` block,
with sender name, sender kind, and `UNTRUSTED PEER INPUT`. If a message arrives
while a turn is running, the existing quiet activity/status line indicates
that it is waiting for the boundary; delivery semantics do not change.

## Client SDK

`haider_client::peer_messaging(&RpcClient)` returns
`Option<PeerMessaging>`. Feature absence returns `None`, not an error. A
present handle provides `list`, `send`, `set_name`, and `subscribe`.
`subscribe` first opts the connection into peer events with `peer.list`, then
yields typed `PeerEvent::Received` and
`PeerEvent::DeliveryChanged` values from the connection event stream.
