# Haider client contract — revision 1

Status: authoritative for wire protocol `v = 1`  
Source snapshot: package `0.0.943` plus the additive snapshot-availability field in this tree  
N-1 compatibility baseline: `0.0.942`  
Contract revision date: 2026-08-23

This document is the client-facing contract of `haider-rpc`,
`haider-client`, and the daemon producers behind them. Rust types and golden
fixtures remain the byte-level authority. This document decides which door a
client uses, what absence means, and which projection wins when the same fact
is visible in more than one place.

There is deliberately no aggregate `client.snapshot`. Sessions, accounts,
providers, usage, commands, workflows, binding, surfaces, and the journal keep
their existing authorities. A client must not create a second mega-document
and must not scrape a terminal, copy a daemon catalog, or manufacture a value
when a typed door is absent.

Normative words such as MUST and MUST NOT are intentional.

## 1. The honesty rule

A client has an answer only when one of these is true:

- the negotiated daemon advertised the feature that owns the answer and the
  typed response supplied it;
- a current response supplied an explicit availability state;
- a snapshot field whose absence semantics are defined below is present; or
- a durable raw envelope or a fully covered native-pipe row supplied the fact.

Everything else is unknown. In particular:

- an omitted optional is not zero, false, empty, the provider default, or a
  client default unless this document explicitly says so;
- an absent feature token means “do not call or render this feature,” not
  “derive it another way”;
- `Some(0)` and `Some(false)` are measured values; `None` is a different fact;
- an unavailable subsystem is not an empty subsystem;
- a lower-precedence projection may fill a value only when the higher door is
  unavailable and must remain visibly a fallback. An exact additive-field
  compatibility source explicitly named by this contract may fill only when
  the promoted field is absent. Neither may overwrite a present
  higher-precedence value.

### 1.1 Facts you must not derive

The following facts are already published. Clients MUST read the named source
and MUST NOT build a substitute, even when the substitute appears to work.

- **Prompt-cache rates.** The authority is
  `SessionSummary.cache_reread_hit_basis_points` and
  `SessionSummary.cache_lifetime_hit_basis_points` from `session.list` or
  `SessionRosterDelta`. For an older summary that lacks those promotions, the
  same-summary compatibility sources are respectively
  `agent_metrics.usage.cache_reread_hit_basis_points` and
  `agent_metrics.usage.cache_hit_basis_points`; use them only under the
  presence rule in §9.1. Never compute either rate from token counts. Even
  arithmetically correct division creates a second definition of the
  denominator and coverage. The observed symptom was a footer and a web
  client reporting 63.71% and 48% for a session whose published re-read rate
  was 90.58%.

- **Provider.** The authority for a roster row is the top-level
  `SessionSummary.provider`. Never infer it from `last_model`, never install a
  provider default, and never replace absence with a placeholder. Model names
  are not provider identities, especially with configured providers. Absent
  means unknown. The observed default was wrong for 209 of 209 rows.

- **Command catalog.** The authority is the current `command.list` result for
  the supplied query, session context, and dynamic slots. Never hard-code or
  mirror slash-command names. The catalog is context-dependent and also
  publishes ownership; a mirror can look complete while both its rows and
  behavior drift. The observed substitute hand-mirrored roughly 40 commands.

- **Resident binding.** The authority is the baseline and push
  `ResidentSessionBinding` frame. Never scrape a terminal or OSC escape for
  this fact. The frame is profile-global, last-writer-wins across N live
  publishers. Its optional client-minted `binding_token` correlates an
  individual publication to the launching surface without changing those
  authority semantics or exposing daemon connection identity. A client using
  that launch-token arrangement also does not scrape OSC for per-pane
  correlation; see §11.1.

- **Roster facts.** For facts rendered in a roster, the winning projection is
  the top-level field of `SessionSummary` from `session.list` or
  `SessionRosterDelta` at that row's `head_seq`; the field-specific precedence
  table is §8. A digest, raw-event fold, native-pipe row, or separate roster
  store MUST NOT overwrite that projection. When a winning field is absent,
  use only an exact additive compatibility source explicitly named by this
  contract. A lower projection is allowed only when the winning door is
  unavailable and must remain visibly a fallback; otherwise the fact is
  unknown. The observed substitute kept two roster sources without a
  precedence rule, let the poorer projection win, and destroyed a metric
  across every row.

Per-session account identity was considered for this list but is not a
published fact today: `SessionSummary.account_alias` is currently always
absent. There is therefore no authoritative value to tell a client to read.
Clients must render it unknown and must not substitute the profile-global
active account; see §16.

## 2. Profile identity and daemon discovery

Clients SHOULD use `haider_client::profile::resolve_profile`; the algorithm is
published here for independent implementations, not as permission to diverge.

1. The store directory is `HAIDER_PROFILE_DIR` when set. Otherwise it is
   `$HOME/.haider/dev-profile`. Failure to obtain either is fatal.
2. Make the path absolute, create it if absent, then canonicalize it. A
   non-UTF-8 canonical path is rejected.
3. Compute `profile_id` as lowercase BLAKE3 hex over the exact byte string
   `haider-profile-id-v1\n` followed by the canonical UTF-8 store path, with no
   separator added after the path.
4. Select the runtime directory. `HAIDER_RUNTIME_DIR` is intentionally ignored.
   On Linux, use `$XDG_RUNTIME_DIR/haider` only when `XDG_RUNTIME_DIR` is a real
   directory owned by the effective UID with mode `0700`; otherwise use
   `/tmp/haider-<effective-uid>`. macOS and other Unix targets always use that
   `/tmp` form. Windows uses the process temporary directory joined with
   `haider`, although its named-pipe address does not contain that path.
5. Compute lowercase BLAKE3 of the UTF-8 `profile_id`, take the first 32 hex
   characters, and form:

   - Unix: `<runtime_dir>/haider-<32hex>.sock`
   - Windows: `\\.\pipe\haider-<32hex>`

The endpoint name is the discovery mechanism. Do not scan the runtime
directory and do not parse lock files. Connect first. Only a missing or
connection-refused endpoint is spawnable; permission denial, framing failure,
protocol skew, profile mismatch, or feature skew is fatal and MUST NOT cause a
client to kill or replace the incumbent daemon. The packaged launcher starts
only the sibling `haiderd` next to its own executable and polls the same
endpoint. A nonempty `Welcome.profile_id` different from the resolved profile
is fatal. An empty `profile_id` is an old-daemon compatibility case, not proof
of equality.

## 3. Transport, framing, and encoding

### 3.1 Logical frame

Every JSON logical frame is one flat object with top-level `"v": 1` and a
`kind`. Unknown object fields are ignored. Unknown `kind` and unknown request
or response `method` decode to their `Unknown` variants. A top-level version
other than `1` is rejected; it is not treated as an unknown extension.

The WebSocket transport carries one encoded logical frame per message and has
no length prefix. The local IPC stream uses this framing for both JSON and
MessagePack:

```text
4-byte unsigned big-endian body length | exactly that many body bytes
```

Length zero, a prefix above the decoder limit, or a body above the negotiated
limit poisons the stream decoder. A client must not resynchronize by scanning
for JSON.

### 3.2 Hello, Welcome, and the switch boundary

`Hello` (`kind: "hello"`) is the first application frame sent by the client.
`Welcome` (`kind: "welcome"`) is the first successful daemon frame. Both are
always JSON and length-prefixed on the local stream. `Hello.encodings` is an
ordered client offer. Today
`"msgpack"` is the only alternative. `Welcome.encoding` selects the encoding;
omission means JSON.

The selected encoding begins immediately after the complete `Welcome` body.
If an OS read contains the end of `Welcome` and later frames, stop the JSON
decoder exactly at that frame boundary, switch the same length-framed decoder,
and decode only the unread suffix. Do not sniff a post-Welcome body and do not
try JSON after MessagePack failure.

`Hello.max_receive_frame` is the client receive ceiling. `Welcome.frame_limit`
is the daemon ceiling. Each peer enforces the smaller applicable value before
allocating or sending.

### 3.3 Versions, capabilities, and features

`Hello.protocol_min..=protocol_max` and `Welcome.protocol` are wire versions.
`Hello.client_version` and `Welcome.daemon_version` are package/build strings
for diagnostics and compatibility policy. Package equality does not establish
wire compatibility; wire overlap does. Protocol v1 negotiation selects the
highest exact overlap supported by both peers.

Capabilities and features answer different questions:

- `capabilities_granted` authorizes this connection. `view` permits reads,
  watches, replay, and view attachments. `control` permits mutations and
  control attachments. The grant is a subset of the requested set and never
  contains `Unknown`.
- `features` says what this daemon build implements. A granted `control`
  capability does not make an unadvertised method available. A feature token
  does not grant control.

Clients MUST gate optional methods and fields using the table below. The
daemon's dispatcher also checks capability and attachment policy; feature
advertisement is the skew/discoverability contract, not an authorization
credential carried on each request.

## 4. Feature-token map

An operation not listed here is part of the v1 base surface. “Field” means the
client may use that field only when it is present; the named token permits an
affordance before the response exists.

| Feature token | Methods, frames, or fields it publishes |
|---|---|
| `session_mutation_v1` | `session.create` and typed create metadata |
| `session_permission_overrides_v1` | `session.create.permission_overrides` and metadata permission overrides |
| `turn_control_v1` | `turn.submit`, `turn.cancel` |
| `run_retry_v1` | `run.retry` |
| `context_compaction_v1` | `session.compact` |
| `fallback_chain_v1` | durable fallback-lane events and next-lane continuation; no separate method |
| `compaction_guard_v1` | durable compaction-guard/promotion events; no separate method |
| `artifact_put_v1` | `artifact.put` |
| `branch_create_v1` | `branch.create`, branch-scoped submit/compact fields and responses |
| `session_fork_v1` | `session.fork`, `session.metafork` |
| `session_observe_v1` | `session.observe` |
| `session_observe_batch_v1` | `session.observe_batch` |
| `session_fleet_v1` | `session.fleet` |
| `resident_turn_submit_v1` | `turn.submit_from_cli` |
| `hooks_v1` | `hooks.list`, `hooks.trust`, `hooks.revoke`, `turn.submit_with_hook_trust`, hook events |
| `hooks_server_v1` | long-lived JSONL hook runtime facts; no new method |
| `agent_message_v1` | `agent.message` |
| `shell_exec_v1` | receipt-backed direct user `shell.exec` |
| `user_command_v1` | direct user shell-command provenance/output committed into later model context and the synthetic `shell.exec.run_id` cancellation coordinate; paired with `shell_exec_v1`, unrelated to catalog rows whose `kind` is `"custom"` |
| `tool_inventory_v1` | `tools.inventory` |
| `vault_stage_v1` | `vault.stage` |
| `account_login_api_v1` | `account.login_api` |
| `account_oauth_pkce_v1` | browser/loopback `account.oauth_start/status/cancel` |
| `account_oauth_device_v1` | device-code forms of the same OAuth methods and `user_code` |
| `account_oauth_import_v1` | `account.oauth_import` |
| `account_device_discovery_v1` | `account.device_candidates`, `account.import_device` |
| `account_management_v1` | `account.add/list/set_active/remove/set_default_model` and management revision fields |
| `account_rotation_v1` | live same-provider active-account rotation behavior |
| `account_list_watch_v1` | `account.list_watch`, `AccountsChanged` |
| `account_label_v1` | `account.set_label`, descriptor `label` |
| `provider_management_v1` | `provider.list` |
| `provider_configure_v1` | `provider.configure` |
| `provider_remove_v1` | `provider.remove` |
| `provider_models_v1` | `provider.models_refresh`, provider model inventories/details |
| `models_list_v1` | headless model enumeration composed from `provider.list` and `account.list`; it is not another RPC method |
| `session_model_select_v1` | `session.select_model` |
| `session_rename_v1` | `session.rename`, `SessionSummary.title` |
| `session_seen_v1` | `session.seen`, `seen_at_ms`, `last_activity_ms` |
| `session_needs_input_v1` | `SessionSummary.needs_input`, digest `needs_input` |
| `session_run_id_v1` | summary/digest `run_id` |
| `session_effort_select_v1` | `session.select_effort` |
| `session_fast_select_v1` | `session.select_fast` |
| `session_agent_type_select_v1` | `session.select_agent_type`, summary `agent_type` |
| `session_config_v1` | coherent headless read/write session provider/model/effort/fast configuration through existing fields and selection methods |
| `session_lineage_v1` | summary `kind` and `parent_session_id` |
| `session_list_watch_v1` | `session.list_watch`, `SessionRosterDelta` |
| `command_door_v1` | `command.list`, `command.invoke` |
| `input_mirror_v1` | `session.surface_publish/watch`, input portion, `session.input_inject`, `SessionInputInjected` |
| `input_mirror_attachments_v1` | input-surface `attachments` |
| `status_segment_v1` | status portion of surface publish/watch/delta |
| `status_segment_structured_v1` | status `state` and `detail` |
| `transcription_v1` | `transcription.secret_get/set` |
| `usage_report_v1` | `usage.report` |
| `haider_code_plan_status_v1` | unsolicited `HaiderCodePlanStatus` |
| `computer_permission_actions_v1` | `computer.permission_open_settings` and permission action fields |
| `effect_recovery_v1` | typed effect-unknown state and recovery-card coordinates in events/observation |
| `convergence_graph_v1` | `graph.pin/status/abandon` |
| `convergence_graph_v2` | `graph.switch` and retained graph-instance fields |
| `convergence_graph_v3` | `graph.inspect` |
| `convergence_graph_v4` | `graph.run_set.open` and todo child-graph telemetry |
| `loom_v1` | `loom.list/register_agent_type/register_workflow` |
| `loom_cli_presence_v1` | `loom.list.cli_present` |
| `store_health_v1` | unsolicited latched/replayed store-health `ProtocolError` transitions |
| `resident_session_binding_v1` | bidirectional `ResidentSessionBinding` baseline/push frame and its generation fence; it does not by itself guarantee publisher-token echo |
| `resident_session_binding_token_v1` | for every accepted publication carrying a valid client-originated `binding_token`, the daemon stores it with that publisher and echoes it verbatim on `ResidentSessionBinding` baselines/pushes; a publisher that supplies no token produces no field, never an empty string |
| `tui_attach_announce_v1` | OSC 7791 compatibility announcement by this release's TUI; it is not the RPC binding contract |
| `wire_msgpack_v1` | post-Welcome MessagePack selection |
| `session_attach_sealed_v1` | `session.attach.sealed_replay` |
| `export_seq_v1` | CLI export `seq`, `head_seq`, and exact `--since`; no RPC method |
| `pipe_native_v2` | `session.pipe_path` plus v2-or-newer native sidecar laws (current file version is 5) |

### 4.1 The one feature with an explicit withheld marker

Normally a feature token means “this daemon implements the named surface,” so
its absence reads as unimplemented. `FEATURE_USER_COMMAND_V1`
(`"user_command_v1"`) is the sole exception. In
`crates/haider-daemon/src/connection.rs:1829`, `encode_welcome_for_peer`
removes only this token and retries the otherwise unchanged `Welcome` when
advertising that token is exactly what pushes the frame past the peer's
receive-frame limit. Every other encoding failure remains fatal. The reason
is that one additive feature must not make the whole pre-existing connection
surface unavailable to a tightly bounded peer.

The additive Welcome field `uw` disambiguates that cause. It is absent by
default and when false; it is serialized only as `"uw":true` after this one
token was actually withheld. Therefore the three observable states are:

- absent `user_command_v1`, absent `uw`: an old or non-implementing daemon;
- present `user_command_v1`, absent `uw`: implemented and not withheld;
- absent `user_command_v1`, `uw=true`: implemented but withheld for this
  peer's receive-frame limit.

The marker does not change the safe action. Whenever `user_command_v1` is
absent, the direct-user-command semantics are unavailable to that connection
and a typed client rejects before sending a mutating `shell.exec`; `uw` only
reports why. The feature is paired with `shell_exec_v1`: it covers committing
a direct user shell command's provenance and output into later model context
and returning a synthetic-run cancellation coordinate. It has nothing to do
with catalog rows whose `CommandCatalogItemKindWire` value is `"custom"` or
with the `custom_commands` slot of `command.list`.

The additive `availability` field on `account.list`, `provider.list`, and
`usage.report` has no separate token. Field presence is its feature test.
Likewise, promoted roster fields without a token are usable only when present.

## 5. Door and delivery map

The Rust response type in this table is the named `ResponseBody` variant.
“Receipt” means a correlated durable command result; the request may be
retried with the same `command_id`. “Snapshot” never subscribes.

### 5.1 Client state doors

| Need | Method / frame | Success response | Delivery | Authority |
|---|---|---|---|---|
| Session roster | `session.list` | `SessionList` | paginated snapshot | sealed-journal daemon projection `SessionSummary` |
| Roster changes | `session.list_watch` | `SessionListWatch`, then `SessionRosterDelta` | watch: complete initial changed/new baseline followed by coalesced deltas | same summary producer as `session.list` |
| Exact event range | `session.read` | `SessionRead` / `SessionReadResult` | non-subscribing snapshot | raw committed envelopes |
| Current session state | `session.observe` | `SessionObserve` / `SessionObserveDigest` | bounded snapshot at one head | daemon journal reducer |
| Several current states | `session.observe_batch` | `SessionObserveBatch` | ordered bounded snapshots | same reducer, request order preserved |
| Descendant fleet | `session.fleet` | `SessionFleet` | bounded snapshot | durable delegation records plus child journals |
| Transcript replay/live tail | `session.attach` | `SessionAttach`, `Event*`, `AttachCaughtUp` | replay then live stream | raw journal envelopes |
| End replay/live tail | `session.detach` | `SessionDetach` | connection-local command | attachment registry |
| Native transcript path | `session.pipe_path` | `SessionPipePath` | snapshot | daemon-resolved absolute path; never derive it |
| Commands | `command.list` | `CommandList` | context snapshot | daemon `COMMANDS` catalog plus request slots |
| Command execution | `command.invoke` | `CommandInvoke` | correlated result: receipt, parked, client-owned, or unsupported | listed ownership and canonical nested receipt |
| Needs-input | summary/digest fields plus top-level `menu.answer` | `MenuAnswer` or typed error | snapshot coordinate plus durable CAS command | oldest answerable durable menu and its exact fence |
| OS permission action | `computer.permission_open_settings` | `ComputerPermissionOpenSettings` | control action, not a menu answer | durable permission event plus server allowlist |
| Accounts | `account.list` | `AccountList` | snapshot | account management snapshot |
| Account changes | `account.list_watch` | `AccountListWatch`, then `AccountsChanged` | watch invalidation, no baseline body and no descriptors in push | re-read `account.list` |
| Providers/models | `provider.list` | `ProviderList` | cached snapshot; no inline probe | provider registry publication |
| Usage/cache health | `usage.report` | `UsageReport` | snapshot | account meter readings plus journal-derived local ledger |
| Resident binding | top-level `ResidentSessionBinding` | same top-level frame fanned out; no response | required unsolicited baseline and pushes | profile-global most-recent live publisher |
| Volatile input/status | `session.surface_watch` | `SessionSurfaceWatching`, then `SessionSurfaceDelta` | complete baseline then complete latest snapshots | live publisher registry; not journaled |
| Volatile input action | `session.input_inject` | `SessionInputInjectAck`, then owner receives `SessionInputInjected` | routed action | current live input owner |
| Workflows | `loom.list` | `LoomList` | snapshot | persisted Loom registry; pipe source is workflow structure of record |
| Todos | raw `ItemEvent` envelopes (`TurnItem` with `item: "plan"`) | no independent snapshot response | attach replay/live lifecycle | durable item lifecycle; reducer in §12 |

### 5.2 Mutation and specialist doors

| Method | Success response | Kind |
|---|---|---|
| `artifact.put` | `ArtifactPut` | receipt-free content-addressed upload |
| `session.create` | `SessionCreate` | durable receipt |
| `branch.create` | `BranchCreate` | durable receipt |
| `session.fork` | `SessionFork` | durable receipt |
| `session.metafork` | `SessionMetafork` | write-free review, then durable receipt after digest acceptance |
| `turn.submit`, `turn.submit_from_cli`, `turn.submit_with_hook_trust` | `TurnSubmit` or `TurnSubmitOnBranch` | durable receipt |
| `turn.cancel` | `TurnCancel` | durable receipt |
| `run.retry` | `RunRetry` | durable receipt |
| `session.compact` | `SessionCompact` or `SessionCompactOnBranch` | durable receipt |
| `session.select_model` | `SessionSelectModel` | durable receipt |
| `session.rename` | `SessionRename` | durable receipt |
| `session.seen` | `SessionSeen` | durable receipt |
| `session.select_effort` | `SessionSelectEffort` | durable receipt |
| `session.select_agent_type` | `SessionSelectAgentType` | durable receipt |
| `session.select_fast` | `SessionSelectFast` | durable receipt |
| `session.diagnostic` | `SessionDiagnostic` | durable receipt |
| `agent.message` | `AgentMessage` | durable receipt |
| `shell.exec` | `ShellExec` | durable acceptance; terminal bytes/status are item events |
| `tools.inventory` | `ToolsInventory` | snapshot |
| `hooks.list` | `HooksList` | snapshot |
| `hooks.trust`, `hooks.revoke` | `HooksTrust`, `HooksRevoke` | durable receipts |
| `graph.pin`, `graph.run_set.open`, `graph.switch`, `graph.abandon` | same-named response | durable receipts |
| `graph.status`, `graph.inspect` | same-named response | snapshots |
| `loom.register_agent_type`, `loom.register_workflow` | `LoomRegistered` (`method: "loom.registered"`) | registry mutation/no-op receipt |
| `vault.stage` | `VaultStage` | connection-local ephemeral dedupe, deliberately not durable |
| `account.login_api`, `account.oauth_import`, `account.import_device`, `account.add`, `account.set_active`, `account.remove`, `account.set_default_model` | same-named response | durable account mutation |
| `account.oauth_start/status/cancel`, `account.device_candidates` | same-named response | connection-bound flow reads/actions |
| `account.set_label` | `AccountSetLabel` | control mutation; alias remains identity |
| `provider.models_refresh` | `ProviderModelsRefresh` | provider snapshot refresh |
| `provider.configure`, `provider.remove` | same-named response | durable provider mutation |
| `transcription.secret_get/set` | same-named response | same-UID UDS-only secret read/write, not a command receipt |

The golden matrix at
`crates/haider-rpc/tests/fixtures/client_contract_methods_v1.json`, combined
with the historical `wire_transcript.json`, pins a request and successful
response for every one of the 70 v1 request methods. `menu.answer` and resident
binding are top-level frames, not `RequestBody` methods.

## 6. Snapshot, watch, invalidation, push, and replay laws

- `session.list`, `session.read`, `session.observe`, `session.observe_batch`,
  `session.fleet`, `command.list`, `account.list`, `provider.list`,
  `usage.report`, `loom.list`, `tools.inventory`, `hooks.list`, graph reads,
  and `session.pipe_path` are snapshots. Calling them does not subscribe.
- `session.list_watch` subscribes before acknowledging. Its first
  `SessionRosterDelta` set is the current changed/new baseline, chunked at 64;
  later pushes are coalesced by session/head. Deltas deliberately omit
  removals. A client needing removals periodically replaces its membership
  set from a complete paginated `session.list`.
- `account.list_watch` subscribes before acknowledging, but does not send a
  descriptor baseline. `AccountsChanged { revision }` is an invalidation and
  contains no descriptors. Bursts may collapse to the newest revision. Re-read
  `account.list`; never maintain descriptors from the invalidation frame.
- `session.surface_watch` returns the complete current input/status baseline in
  its response. Every `SessionSurfaceDelta` is also a complete latest snapshot,
  not a patch. Either `None` clears that surface.
- every View or Control connection receives exactly one required
  `ResidentSessionBinding` baseline after `Welcome`, even when no publisher has
  ever bound. Later values are unsolicited required pushes.
- `HaiderCodePlanStatus`, store-health `ProtocolError` transitions, drain
  notices, and resident/surface frames are unsolicited pushes, not journal
  replay.
- only `session.attach` begins raw event delivery. It replays strictly after
  `after_seq`, through the captured `replay_through_seq`, emits
  `AttachCaughtUp`, then continues live. `session.read` never subscribes.

## 7. Sequence, gap, and command identity

`RawEnvelope.seq` is the sole durable replay cursor. Delivery is at least once.
A client maintains, per session/attachment, the greatest consecutive sequence
it has completely applied.

1. Drop an event whose `seq <= last_applied`.
2. Apply `seq == last_applied + 1`, then advance the cursor only after the
   complete projection transaction succeeds.
3. On a higher sequence, a `Lagged` frame, reconnect, or local event-queue
   loss, detach/reattach using the client's own `last_applied`. Do not use
   `Lagged.last_queued_seq`; queued is not applied.
4. `SessionAttach.attach_state.requested_after_seq` echoes the request.
   `replay_through_seq` is the sealed head for the initial replay. Replay is
   `(requested_after_seq, replay_through_seq]`.
5. `AttachCaughtUp.high_water_seq` means delivery is complete through that
   value. It may repeat on the same attachment with strictly increasing high
   waters after transparent gap repair. Treat every occurrence identically.
6. An attach cursor beyond the committed head returns typed `cursor_ahead`
   data. Do not clamp silently.

`request_id` and `command_id` are unrelated identities:

- `request_id` correlates one request and response on one connection. Reusing
  it after reconnect gives no durability.
- `command_id` is a client-generated durable idempotency key. Retry a mutation
  after response loss with the same semantic command and the same
  `command_id`; the daemon returns the original receipt. A different semantic
  command must get a new id.
- top-level `menu.answer.request_id` is optional response correlation only.
  Its `command_id` is the durable CAS identity.

## 8. Projection precedence

Precedence is field-specific. Never merge snapshots from different
`head_seq`, `worker_generation`, account/provider revision, native-pipe
generation, or surface owner/revision as if they were one observation.

| Fact being rendered | Winning client door | Lower projections and rule |
|---|---|---|
| Roster row, provider, last model, title, workspace, turn count, footprint, lineage, run badge/id, cache headline | `SessionSummary` from `session.list` or `SessionRosterDelta` at that head | A digest or raw-event fold is fallback only. An absent top-level field stays unknown unless this contract names an exact compatibility source, as it does for the promoted cache fields in §1.1; it must never be overwritten by a guessed/default value. |
| Detailed current run/menu/branch/subagent state | `SessionObserveDigest` at its `head_seq` | Raw events are durable facts but a client need not rebuild the daemon reducer. `metadata_only=true` authorizes only metadata/title/head/generation; projected defaults are not state. |
| Durable event fact and replay cursor | `RawEnvelope` from read/attach | Summary/digest are projections and cannot invent or reorder the event. Preserve raw payload. |
| Transcript display rows | a current-generation native pipe followed to full coverage | Raw item/node events are the durable fallback. At equal coverage, do not show both pipe and fallback rows. Pipe is not authority for run, account, roster, or permission state. |
| Current todo panel | latest open `TurnItem` lifecycle whose `item` is `"plan"`, at the applied raw-event cursor | There is no summary/digest todo projection. Use the exact reducer in §12; do not infer a plan from tool text. |
| Accounts/defaults/active aliases | `account.list` snapshot | `AccountsChanged` only invalidates. Provider rows do not replace descriptors. |
| Provider/model inventory | `provider.list` snapshot | Account rows and a client hardcoded model list do not override it. `provider.models_refresh` produces a newer snapshot. |
| Cross-account usage | `usage.report` | Session summaries contain only per-session promoted cache metrics, not the account report. |
| Per-session cache health | `SessionSummary.cache_reread_hit_basis_points`; use `cache_lifetime_hit_basis_points` only for the separately labeled lifetime/all-input share | The same-summary nested `agent_metrics.usage.cache_reread_hit_basis_points` and `cache_hit_basis_points` are compatibility sources only when their promoted field is absent. Never calculate a substitute from token counts. |
| Volatile composer/status | surface watch response/delta | Journal and terminal scraping are not fallback sources. Reconnect/watch again after owner loss. |
| Resident profile binding and client-minted surface correlation | `ResidentSessionBinding` baseline/push and optional `binding_token` | OSC 7791 is compatibility output, not an authority or required per-pane source; see §11. |
| Commands and ownership | `command.list` result | Never mirror `COMMANDS`. Ownership `"client_view"` is deliberately client-owned; `"unknown"` is non-executable. |
| Workflows/agent types | `loom.list` | Compiled graph material is derived; `LoomWorkflow.source` is the workflow structure of record. |

If two winning snapshots have different heads/revisions, select the newer
complete snapshot as a unit. Do not take a model from one and a cache rate from
another. If equal coordinates disagree, report a compatibility fault through
`session.diagnostic` where applicable; do not silently choose a client default.

## 9. Absence, empty, and zero semantics

### 9.1 Universal rules

- A serde-omitted optional has exactly the `None` meaning documented here.
  Unknown fields are ignored, but unknown enum variants follow the audit in
  [the enum appendix](client-contract-v1-enum-audit.md).
- Empty collections in an available snapshot are genuine emptiness. Empty
  collections in a legacy snapshot whose subsystem availability is omitted
  are not proof that the subsystem was available.
- Zero is not a universal absence sentinel. The two legacy exceptions are
  `provider.list.revision=0` and `usage.report.generated_at_ms=0` when the new
  availability field is omitted; those combinations are ambiguous and MUST
  render as unknown availability. Required zero coordinates elsewhere retain
  the field-specific meanings in §9.9.

Fallback chains must also distinguish presence from truthiness. This common
adoption pattern is wrong:

```javascript
value = promoted || nested; // WRONG
```

It prefers the older nested projection when `promoted` is the measured value
zero—exactly when the promoted cache fact says the cache served nothing. Test
whether the promoted field is present, not whether it is truthy:

```javascript
value = Object.hasOwn(summary, "cache_reread_hit_basis_points")
  ? summary.cache_reread_hit_basis_points
  : summary.agent_metrics?.usage?.cache_reread_hit_basis_points;
```

The same law applies to every promoted optional: present `0`, `false`, or an
empty collection wins over an older fallback.

### 9.2 Session summary optionals

| Field | `None` / empty meaning |
|---|---|
| `run_state` | old daemon; current producers populate it |
| `run_id` | no active run or old daemon; never render a stop action; read with `run_state` from the same summary |
| `seen_at_ms` | no acknowledgement has ever committed |
| `last_activity_ms` | no user-relevant committed activity |
| `waiting_why` | no legacy three-kind park reason; superseded by `needs_input` |
| `needs_input` | nothing currently requires human input; if present without all answer coordinates it is badgeable but not answerable |
| `metadata` | legacy/untyped row or old daemon; do not infer configuration |
| `provider` | unknown; never derive a default or look beside `last_model` in another object |
| `last_model` | unknown; latest durable selection otherwise wins over create metadata |
| `cache_lifetime_hit_basis_points` | no usage truth or old daemon; not 0% |
| `cache_reread_hit_basis_points` | no re-readable input yet, no usage truth, or old daemon; not 0%. `Some(0)` is a measured miss rate |
| `workspace_cwd` | old daemon/unknown; do not use the client's process cwd as session truth |
| `turn_count` | old daemon/unknown. `Some(0)` exclusively means no committed main-timeline user turn |
| `footprint_tokens` | old/no snapshot/unknown when content exists. `Some(0)` exclusively means a truly empty session |
| `footprint_truth` | absent exactly when `footprint_tokens` is absent; present value classifies the paired count |
| `title` | untitled or old daemon; these are intentionally indistinguishable |
| `agent_metrics` | old daemon or no reducible agent truth; not a zero snapshot |
| `parent_session_id` | root or old daemon; use `kind`, not id-shape, to distinguish when available |
| `kind` | old daemon; do not infer root |
| `agent_type` | plain session or old daemon; do not infer a Loom identity |
| `effort` | provider default or old daemon; do not invent a named effort |
| `fast` | old daemon. `Some(false)` is real normal mode |
| `account_alias` | no per-session account seam exists yet; it is currently always absent. Never substitute the global active account |

`head_seq = 0` is the actual empty journal head. `worker_generation = 0` is not
a general absence sentinel; generation comparisons use the supplied integer.

### 9.3 Observe/read/attach optionals

| Field | Absence meaning |
|---|---|
| `SessionReadResult.metadata` | same as summary metadata |
| `latest_context_footprint` | no durable footprint at or before the returned head |
| footprint `context_window` | provider did not declare a window; do not install a client table value |
| footprint `soft_threshold_tokens`, `estimated_turns_to_threshold` | no computed threshold or no honest turn estimate respectively; zero when present is a computed zero |
| digest `metadata` | legacy/untyped session; with `metadata_only`, it remains one of the authoritative fields |
| digest `run_id` | no active run or old daemon |
| `active_branch_id` | implicit main branch |
| `main_head_node_id` | no committed main head node |
| legacy waiting `pending_menu_id` | the park is badgeable but has no durable menu coordinate; do not answer it |
| menu `menu_id`, `request_seq`, `worker_generation` | old/recovery projection lacks an answer fence; render only, do not answer |
| menu `opened_at_ms` | old event; no exact parked-since time |
| menu `permission_description`, `presentation` | that menu has no typed copy/presentation or an older event omitted it |
| subagent `callsign` | daemon never persisted one; a UI fallback is display-only |
| digest `turn_count`, `agent_metrics` | old daemon or `metadata_only` response; not zero |
| digest `needs_input` | no current human park or old daemon without the field |
| attach `sealed_replay` omitted/false | replay ordinary durable deltas; true may omit superseded item deltas only during initial store replay |

`last_event_limit = 0` requests no trailing kind names; it does not make the
rest of the digest metadata-only. An empty branch list means only the implicit
main branch is known. An empty pending-menu/subagent list is genuine for a full
digest, but is merely a skipped projection when `metadata_only=true`.

### 9.4 Request optionals with semantic meaning

| Request field | Omitted / `None` meaning |
|---|---|
| `session.list.cursor` | first page; a returned cursor is opaque and passed verbatim |
| create `permission_overrides` | daemon defaults |
| create `cache_policy` | daemon/default policy |
| surface publish `input` or `status` | leave that surface unchanged; empty text/line is a value |
| branch-scoped `branch_id` | implicit main branch |
| branch/fork `source_branch_id` | source main branch |
| branch/fork/metafork `name` | daemon-generated/default name |
| metafork `accepted_proposal_digest` | write-free review only; presence is acceptance of the exact returned review manifest |
| model-select `provider` | select within current provider; presence selects the named provider/model row |
| rename `title` | clear the title after daemon normalization |
| effort-select `effort` | revert to provider default |
| agent-type-select `agent_type` | revert to plain session |
| shell `cwd` | use the session workspace |
| account login `alias` | daemon derives a globally unique alias |
| account login `validation_model` | release-owned full validation model |
| account remove `expected_revision` | legacy unfenced request; clients with revision truth should supply it |
| account label `label` | clear display label; alias identity does not change |
| account/provider list `provider` | all providers |
| provider configure `api_family`, `origin`, `auth_requirement` | on update, leave immutable identity unchanged; creation requires the identity fields |
| provider configure `default_model` | no declared default/clear according to mutation validation; never choose one client-side |
| menu answer `input` | option needs no free-form value |
| menu answer `request_id` | no correlated response; errors arrive as uncorrelated `ProtocolError` |

Boolean request defaults are false. In particular `confirm_new_epoch=false` is
not permission to cross a cache-epoch boundary; the daemon may return the
typed confirmation-required error.

### 9.5 Accounts, providers, and snapshot availability

All three responses now carry one of these wire shapes:

```json
{"state":"available"}
{"state":"unavailable","reason":"public explanation"}
{"state":"unknown"}
```

These are exact wire spellings, not the Rust identifiers.

The response field is optional and additive:

| `availability` | Meaning |
|---|---|
| omitted / `None` | old daemon; subsystem availability unknown. Preserve legacy data but do not use an old zero sentinel as proof of unavailability or availability |
| `available` | subsystem was read successfully; an empty list is genuinely empty and zero is a real value |
| `unavailable { reason }` | no snapshot truth was obtained; legacy empty/zero fields are compatibility placeholders only |
| `unknown` | newer state not understood; treat availability as unknown |

For `account.list`, `revision: None` means old/unrevisioned account view or
legacy unavailability. With `availability=available`, current management
producers supply a coherent revision and empty descriptors mean no accounts.
`provider_active` and `provider_defaults` are empty when no such coordinates
exist; neither authorizes deriving a per-session account. Descriptors contain
metadata only, never secrets. Descriptor `base_url=None` means a
provider-owned endpoint and `label=None` means no operator label. The required
`identity` string may be empty when no human identity was published; that is
not permission to invent one. Display code may fall back from label to
nonempty identity to alias, but alias remains the credential identity.

For `provider.list`, `revision=0` was the old unavailable placeholder. Only
explicit availability disambiguates it. Provider `endpoint`,
`availability_reason`, and `default_model` are absent when undeclared/unknown;
empty `models`, `model_details`, `auth_methods`, effort ladders, or speed lists
mean the provider declares none in an available snapshot. `context_window`,
`default_effort`, and `supports_thinking_type` absence means not declared;
clients hold no replacement capability tables.

### 9.6 Usage and cache optionals

`usage.report.availability` follows the table above. With omission,
`generated_at_ms=0` is the old ambiguous placeholder. With `available`, zero
is an actual assembly time value and an empty account list means no known
accounts.

For each account:

- `identity` and `plan` absent mean unknown, not a fallback alias/tier.
- meter `metered { windows: [] }` is a successful reading with no published
  windows. `local_only` means no server meter exists. `unavailable { reason }`
  means the reading failed. These are not interchangeable.
- window `utilization=0.0` is measured zero use; `resets_at_ms` and `label`
  absent mean the provider did not publish them.
- local integer zeros are measured journal-derived zeros.
- `est_cost_usd` absent means no priced metered model matched; it is not
  `$0`. `api_equivalent_est_cost_usd` absent means no complete equivalent
  estimate. Neither is a bill.
- the nested `cache` object is omitted only when every counter is zero, every
  cost is absent, and both breakdown/request lists are empty. Its absence means
  no cache detail, not a measured 0% rate.
- cache cost optionals are absent when pricing is incomplete. Breakdown
  `auth_method` absent is legacy/unknown and is not eligible for dollar
  rendering. Request `scope` absent means no safe attribution coordinates;
  scope `run` absent means no run coordinate.
- `CacheStatAvailability` and the request records decide whether cache splits
  exist. Never derive cache health by dividing cached tokens by some other
  denominator. If the published rates and their exact nested compatibility
  sources are absent, the rates are unknown.

For the per-session headline, basis points are integers from 0 through 10,000.
Use `cache_reread_hit_basis_points`. The lifetime/all-input rate includes first
and new input that could not hit a preceding prefix and is not the cache-health
headline.

For `AgentMetricsSnapshot`, `agent=None` names the root/head agent,
`terminal_at_ms=None` means a partial/nonterminal snapshot, and `usage=None`
means no durable usage truth—not zero usage. Within `AgentUsageMetrics`, either
cache rate is absent when its exact denominator/coverage is unavailable;
metered and API-equivalent costs are absent when their respective pricing
claim cannot be made. Breakdown `auth_method=None` is legacy/unknown and its
optional costs remain unavailable. Never add two snapshots for one agent;
replace only with a higher `head_seq`.

For unsolicited `HaiderCodePlanStatus`, every remote snapshot field remains
optional because the provider may omit it. `plan`, `plan_label`, weekly
allowance, credits, hold, live-model count, refresh cadence, and `cached` must
therefore stay unknown when absent; `Some(false)` and numeric zero are explicit
provider values. Inside weekly allowance, percent, state, reset, and grace are
independent facts. Inside hold, only `api_locked=Some(true)` or
`subscribe_banned=Some(true)` proves a halt; missing flags and a missing reason
prove neither health nor failure. The outer `state: "available"`,
`state: "indeterminate"`, and `state: "halted"` outcome is the health
classification—never derive it from the partial snapshot.

### 9.7 Other optional response fields

- `next_cursor` absent means the last page. A cursor is opaque.
- surface publish accepted revision absent means the field was omitted or its
  publisher-local revision was stale. Revisions are comparable only for the
  same owner and surface.
- surface watch/delta `input` or `status` absent means cleared. Input
  `attachments=[]` means none. Status `state`/`detail` absent means no
  structured value; do not parse `line` to invent it.
- `ShellExec.run_id` absent is an older response; cancellation is not safely
  available until the event stream supplies a run coordinate.
- `TurnCancel.terminal_seq` is present exactly for `already_terminal`.
- `GraphStatus.status=None` means no active graph. `GraphInspect.next_cursor`
  absent means last page. Other graph optionals retain the meanings named by
  their typed graph records; unknown graph payload additions remain raw in
  journal replay.
- `LoomList.cli_present[name]` absent means not probed. `false` means probed
  and missing. Empty `agent_types`/`workflows` in a successful snapshot means
  none registered.
- `OAuthStart` optionals are present only when that flow form supplies them.
  `availability.available=false` must carry the public reason; absence of a
  URL/flow id is not permission to synthesize one. `user_code` exists only for
  device flows. OAuth status `"ready"` carries the single-use reference; other
  states do not.
- transcription `secret=None` means no stored secret. The raw secret is
  same-UID UDS-only, zeroized, and must never be logged or converted through a
  loggable JSON value.
- error `data=None` means no typed recovery coordinates or an old daemon.
  `cursor_ahead` and `already_resolved` supply their typed data when emitted;
  never parse the human message.

### 9.8 Remaining direct-wire optional inventory

This table closes the optionals not owned by the session, provider, usage, or
surface tables above.

| Coordinate | Omitted / `None` / empty meaning |
|---|---|
| `Welcome.encoding` | JSON, as defined at the switch boundary in §3.2 |
| session metadata `system_prompt_version` | legacy or unspecified daemon policy version; do not name one client-side |
| session metadata `permission_overrides` | daemon registry defaults; the nested false booleans grant nothing |
| session metadata `title`, `effort`, `agent_type` | untitled, provider default, and plain session respectively; metadata `fast=false` and default cache policy are real defaults, not missing values |
| `command.invoke.session_id` | launcher/global command context; an operation that needs a session must refuse, not choose one |
| `command.list.items=[]` | no catalog rows match the supplied query/context; the advertised command-door feature establishes that the subsystem answered |
| catalog row `name`, `value`, `arg_hint`, `session_only` | not applicable to that row kind; never reverse-engineer the omitted coordinate from `label` |
| `CommandInvokeOutcomeWire` with `kind: "unsupported"`, `reason` omitted | unsupported with no public detail; do not parse another field for a reason |
| `graph.inspect.cursor` | first page; response `next_cursor=None` is the last page |
| todo child `depends_on_todo_id` | no predecessor todo; `opened_seq=None` means no child graph-open coordinate was returned and is not proof the child ran |
| needs-input `safe_body=[]` | no additional secret-free body; `since_ms=None` means no stable notification timestamp |
| needs-input/menu `options=[]` | no enumerated answer can be constructed from this projection; do not invent option keys or indexes |
| menu option `detail`, `decision` | no secondary copy and no typed permission decision respectively; never parse the label to manufacture either |
| fleet node `callsign` | no persisted callsign; a UI fallback is display-only |
| fleet node `parent_agent_id` | no parent-agent coordinate (the required `parent_session_id` still owns ancestry) |
| fleet node `metrics` | no direct metric snapshot; `FleetMetricsTotalsWire.usage=None` means at least one returned node lacks durable usage truth |
| fleet `children=[]`, `folded_children=0` | a real leaf. A positive `folded_children` says bounded children were omitted; the rollup covers only returned nodes |
| hook `trust_state` | old daemon; only the legacy `trusted` boolean may be used, with no invented revoked-by-edit state |
| branch/fork/metafork response `source_branch_id` | source was the implicit main branch |
| metafork review fields | with `committed=false`, `session_id`, `created_seq`, `worker_generation`, `metadata`, and `omission_count` are absent while `review_manifest` is present. With `committed=true`, those receipt fields are present and `review_manifest` is absent |
| rename/effort/agent-type receipt value | the committed result is cleared title, provider-default effort, or plain session respectively—the same semantics as the request |
| device candidate `account_label`, `expires_at_ms` | the external store supplied no label or expiry. `unsupported_reason=None` means no public reason, not that import is supported; use `import_supported` |
| OAuth availability `reason` | normally absent when available; an unavailable current producer supplies it. Old omission with `available=false` is an unspecified public reason |
| OAuth start fields | only the selected flow form supplies them, as detailed in §9.7; none may be reconstructed from the authorization URL |
| account activation `prior_alias` | no previously active account. Account removal `replacement_active_alias=None` means no replacement became active |
| protocol-error `presentation` | old/fact-only error has no structured presentation. `failed_write_ids=[]` means no uncommitted durable write ids were reported |
| cache-epoch error rewarm cost/token fields | pricing or the corresponding token basis is unavailable; zero, when present, is a computed zero |

`ObserveMenuWire.body=[]`, empty pending menus/subagents, empty hook lists, and
empty fleet roots are genuine emptiness for their successful full snapshots.
They are not subsystem-unavailable sentinels.

### 9.9 Zero, revision, generation, and timestamp inventory

- `head_seq`, `main_head_seq`, attach `after_seq`, pipe `starts_after`, and a
  replay cursor of zero mean “before/no committed envelope.” Coverage zero
  proves inspection through zero only.
- `worker_generation` and `daemon_generation` are supplied coordinates even
  when numerically zero. Never use zero to skip their equality fences and
  never compare the two generation domains with each other.
- Account/provider management revisions and hook revisions are monotonic only
  in their owning registry. Hook revision zero is the initial or old-omitted
  baseline—those cases are intentionally indistinguishable, but neither means
  the hook subsystem is unavailable. The provider legacy ambiguity is the
  explicit exception in §9.5.
- Surface revision zero is a publisher-local revision value. Whether a publish
  was accepted is expressed by the optional accepted-revision field, not by
  testing the number for zero.
- Required count/token/cost fields with zero are measured zero inside the
  snapshot that contains them. An absent optional outer metric remains
  unmeasured. Saturating rollups never turn missing component truth into zero.
- Required timestamps with zero carry exactly zero; a client must not replace
  them with its clock. Optional timestamps express absence with `None`.
- `Some(false)` is an explicit false. Serde-defaulted false fields on an old
  payload establish only that the additive flag was not asserted; they do not
  establish an unavailable subsystem or a positive permission.

## 10. Commands, needs-input, and permissions

### 10.1 Command catalog

`command.list` is the catalog door. Its `slots` object has exactly five
dynamic arrays, each encoded as JSON pairs `[value, description]`:

```text
providers
models
accounts
efforts
custom_commands
```

The requesting surface supplies these current-context values; the daemon
combines them with its single `COMMANDS` registry and returns rendered rows.
An omitted/empty slots object means no dynamic candidates. All discriminants
are exact, case-sensitive wire strings; Rust variant identifiers are not wire
values:

- catalog row `kind` is `"built_in"`, `"argument"`, `"custom"`, or
  `"unknown"`;
- `ownership` is `"daemon_operation"`, `"client_view"`, or `"unknown"`;
- `CommandInvokeOutcomeWire` uses the `kind` values `"receipt"`, `"parked"`,
  `"client_owned"`, `"unsupported"`, or `"unknown"`.

`ownership` is load-bearing:

- `daemon_operation`: invoke through `command.invoke` with a durable
  `command_id` when the operation mutates.
- `client_view`: perform only the client-local view behavior named by the row.
- `unknown`: display-only/non-executable.

Outcome `kind: "receipt"` nests the canonical operation response; there is no
second receipt vocabulary. `"parked"` carries the same `NeedsInputWire`, and
`"client_owned"` redirects to client view behavior. For `kind: "unsupported"`,
an omitted `reason` means no public reason. Outcome `"unknown"`, including an
unrecognized future value decoded by a typed client, is non-executable. A
client MUST NOT ship a hand-maintained slash-command list.

### 10.2 Needs-input answer fence

`needs_input` is the one secret-free human-attention card. The daemon chooses
the oldest answerable pending menu by request sequence. A parked run without a
visible menu still gets a badgeable kind/title, but it is not answerable.

A client may send `menu.answer` only when it has all three exact coordinates
from the same card: `menu_id`, `request_seq`, and `worker_generation`. It also
echoes the chosen option's stable `key` and display index. First committed
answer wins. Retrying the same semantic answer with the same `command_id`
returns the original `resolution_seq`; a different command loses with
`already_resolved { resolution_seq }`. A generation, menu, request-sequence,
or option mismatch is stale/invalid; never answer a newer card using cached
coordinates.

When `secret_answer=true`, stage the secret through `vault.stage` and send a
`MenuInput` object with `kind: "secret_vault_reference"`. Secret bytes never
enter the durable menu frame.

### 10.3 Separate operating-system permission action

`computer.permission_open_settings` is not `menu.answer` and does not grant a
Haider permission. It requires Control and a control attachment to the
session. It echoes the durable OS permission request's `session_id`, string
`request_id`, and typed `SystemPermission`. The daemon opens only its
server-known allowlisted settings pane; the caller cannot supply a URL.

The durable permission event enumerates allowed actions such as open settings,
retry, or restart. Opening settings does not resolve the menu. Retry/decision
still uses the separately fenced menu answer coordinates, and the daemon
rechecks the OS. Clients must not treat a successful open action as a grant.

## 11. Resident binding and volatile surfaces

### 11.1 Profile binding and client-minted surface correlation

`ResidentSessionBinding` answers the profile-level question: “something in
this profile is currently bound to session X.” The daemon registry holds N
publishers keyed by connection, and each accepted publication receives a
daemon-local monotonically increasing revision. `visible()` selects the live
publisher with the greatest revision and exposes its
`(session_id, worker_generation, binding_token)`. Thus N publisher records
still collapse to one profile-global most-recent value; the token does not
turn the daemon into a pane, window, or embedding registry.

`binding_token` is an optional, opaque correlator minted by the client that
launches a TUI surface. The supported TUI launch mechanism is the
`HAIDER_BINDING_TOKEN` environment variable. A consumer that spawns
`haider tui --session <id>` sets a different token on each child process. The
TUI copies that value into every resident-binding publication, and the daemon
stores and echoes it verbatim without parsing it or using it for identity,
routing, authorization, or any other behavior. Sanity validation is limited
to 1–128 UTF-8 bytes in the ASCII set `[A-Za-z0-9._:-]`.

Token echo has its own discovery bit,
`resident_session_binding_token_v1`; the older
`resident_session_binding_v1` bit advertises only the binding frame. Without
the token bit, an absent `binding_token` is ambiguous: the daemon may predate
client-originated binding tokens, or this publisher may simply have supplied
none. With the token bit, absence means only that this publisher supplied no
token; the daemon never substitutes an empty string.

A TUI process holds one daemon connection for its lifetime. Consequently an
in-TUI hop replaces that connection's publisher record and emits the same
`binding_token` with the new `session_id`. A separately launched TUI uses a
different connection and client-minted token, inserts a separate publisher
record, and emits that different token. A client can therefore distinguish a
hop from a second surface without observing or receiving either daemon
connection id.

The complete client arrangement is: after the daemon advertises
`resident_session_binding_token_v1`, open the View/Control binding stream,
mint and retain one token per child surface, set `HAIDER_BINDING_TOKEN` when
launching that child, and apply each echoed token/session pair to the matching
surface. The launch record supplies the initial mapping; later same-token
frames report in-TUI hops. A client may retire its terminal-based per-pane
read once the bit is advertised **and its own minted token round-trips**.
Until both conditions hold, it keeps its old-daemon compatibility fallback.
OSC 7791 remains only a compatibility announcement for clients that have not
migrated; it is not a second authority.

Consequences:

- it is not a daemon-minted pane, terminal, window, or connection identity;
- a multi-surface client maps a token-bearing publication only to the surface
  for which it minted that exact token. It MUST NOT assign a tokenless visible
  value to a pane by recency or guesswork;
- daemon connection ids and publisher revisions remain internal and are not
  promised client coordinates;
- when `resident_session_binding_token_v1` is advertised, absent
  `binding_token` means the publisher supplied none. Without the bit, absence
  is ambiguous with an old daemon. The daemon that advertises the bit never
  synthesizes, defaults, or derives a token from a connection id or process
  id;
- `session_id=None` is an explicit publisher unbind/launcher state, not
  missing data;
- a Control publisher sends the same top-level frame. The daemon validates
  the `worker_generation`. There is no correlated response;
- if the visible publisher disconnects, the next most-recent live publisher
  becomes visible. If none remains, the daemon retains/pushes an explicit
  unbound value using the removed/current store generation;
- every View or Control connection gets exactly one required baseline after
  `Welcome`. Later required-delivery failure closes the affected viewer rather
  than silently losing state;
- consumers apply a profile binding only against the authoritative current
  worker generation. A stale generation cannot bind a superseded session
  worker.

### 11.2 Volatile input and status

Surface values are per session and publisher connection, volatile, and
removed when the publisher disconnects. Publisher revisions are monotonic only
within `(owner, session, input-or-status)`; do not compare revisions across
owners or between input and status.

An omitted field in `session.surface_publish` means unchanged. An accepted
revision replaces the complete value. A stale revision is not accepted and is
reported by an absent accepted-revision field. The watch response and each
delta carry complete current values; `None` clears.

`session.input_inject` routes an operation to the current input owner. The
daemon does not edit the buffer. `delivered=true` means it entered the owner's
outbox; the owner applies it and republishes a later surface revision. The
published snapshot, not the inject acknowledgement, is rendering truth.

## 12. Native pipe, transcript, and todos

### 12.1 Native pipe generations and segments

Use `session.pipe_path`; never construct a filename from `session_id`. The
journal remains durable authority. The sidecar is best-effort and rebuildable;
a missing, corrupt, or torn sidecar is retried/rebuilt and must not fail the
journal append.

Start at the returned stable root. Its first JSONL line is:

```json
{"pipe":"haider.session.jsonl","version":5,"session_id":"…","generation":G,"segment":0,"starts_after":0}
```

Current producers write version 5. `pipe_native_v2` is the capability name
because v2 established coverage and `(seq, ordinal)` identity. Later versions
are additive/rebuild revisions. A v2-aware reader ignores unknown row keys and
unknown line kinds.

Validate every segment:

- header magic, session id, version, and generation must match the chain;
- root has segment 0 and starts after 0; successor segment numbers advance and
  `starts_after` equals the predecessor coverage;
- every transcript row has `(seq, ordinal)` identity and optional `branch_id`;
- a coverage record is `{ "coverage": N, "generation": G }` and proves every
  journal envelope through N was inspected, including envelopes that produced
  no row;
- compute `covered_through = max(all row seq values, all coverage values, all
  sealed-segment coverage values)`;
- a sealed segment ends with
  `{ "segment_end":"sealed", "coverage":N, "generation":G,
  "successor":"relative-filename" }`. The successor must be one direct-child
  relative filename. Follow that exact pointer; do not derive or glob it;
- EOF of a sealed segment never means caught up, even when its coverage equals
  the roster head. Open its successor;
- only EOF of the final unterminated segment can be at head, and only when
  `covered_through == SessionSummary.head_seq` (or another authoritative head
  for the same session);
- a missing/torn successor or torn final line means not caught up. Retain the
  last complete coverage and retry;
- an atomic rebuild increments `generation` and replaces the stable root.
  Reopen from the root on a generation change. Never mix successor files from
  two generations.

Rows are structured user/assistant text, incomplete assistant, error, tool, and
compaction-boundary records. Optional `reasoning` is the final sealed summary;
streaming reasoning deltas are not written. `compat=true` means the row is
genuinely redundant with the item stream and may be dropped by an
item-canonical client. A row carrying reasoning is producer-guaranteed not to
be marked compat. `args_preview`/`result_preview` absence means unavailable,
not empty output. `branch_id` absence means main. Ordinal distinguishes
multiple rows from the same sequence.

`reasoning` has no independent sequence position. It is a field on the
assistant row, not an item in the ordering. Therefore no client rendering
choice for its placement can violate append-only ordering: there is no wire
order to respect or break. A client MUST NOT infer that field order implies
temporal order, for this field or as a general habit. Reasoning always precedes
the response text it produced, so rendering it above the response is the
correct presentation. This is presentation guidance, not a wire requirement,
because the wire expresses no ordering here. More generally, when this
contract states a field's location but not its ordering semantics, a reader
must not supply the missing semantics by inference.

### 12.2 Raw item lifecycle to transcript

For an interactive transcript or when the native pipe is unavailable, reduce
raw `ItemEvent` values by `item_id`:

1. `event: "started"` with `{ item_id, item }` opens the item unless that id is
   already open or closed. Duplicate starts are no-ops.
2. `event: "delta"` with `{ item_id, delta }` applies only to the matching open
   item and the matching nested item/delta kind. Preserve streamed bytes
   exactly; command output byte fields are base64 where the type says so.
3. `event: "completed"` with `{ item_id, item }` is the authoritative final
   item and replaces the accumulated item. Do not append the final value to
   deltas.
4. A `"completed"` event without a previously observed `"started"` event still
   inserts one finished item; this is required for mid-stream attach and
   sealed replay.
5. A completed id never reopens; duplicate completion is a no-op.
6. Keep envelope payload as raw JSON even when a known nested event is decoded.
   Unknown item/event kinds must remain replayable.

For pipe-style transcript rows, the daemon projector also joins a completed
tool-call item to the immediately following committed tool-exchange node and
its independently committed result, emits bounded previews, joins final
reasoning to the assistant row, represents incomplete/failed runs explicitly,
and emits a compaction boundary. A client should consume the native result
rather than implement a competing projector when the pipe is available.

### 12.3 Current todo plan

There is no independent todo snapshot method. The durable `TurnItem` lifecycle
whose `item` is `"plan"` is the authority and is incrementally adoptable
through normal attach replay.

- The first nonempty `todo_write` in one open lifecycle commits an `ItemEvent`
  with `event: "started"`, a fresh `item_id`, and `item: "plan"`, then pins
  that full list as the current panel.
- Later writes for the same lifecycle commit
  `event: "completed"` with the same `item_id` and `item: "plan"`. The list
  has full replacement semantics; never merge individual rows with the old
  list.
- Keep the latest plan list pinned while at least one item does not have
  `TodoState` wire value `"completed"`.
- An all-completed list closes the id, unpins it, and adds the completed plan
  to the transcript/history (`NodeKind` with `kind: "todos"`). A later
  duplicate is ignored.
- An empty update closes/clears an open plan. An empty write before any plan
  commits no lifecycle event.
- A plan born all-completed emits `"started"` followed by `"completed"` and
  closes immediately.
- `TodoState` is exactly `listed`, `processing`, or `completed` and is frozen.
  `dep: None` means no dependency. A listed item whose referenced dependency
  is not completed is blocked. Do not derive dependency from ordering or text.

## 13. Workflows

`loom.list` returns registered `LoomAgentType` and compiled `LoomWorkflow`
records. An available empty list is genuinely empty. `LoomWorkflow.source` in
the `pipe/v1` DSL is structure of record; its graph template and node metadata
are compiled projections. `rev` is registry revision, not absence. Identical
content registration returns `updated=false` without advancing content.

`cli_present` is keyed by the declared CLI name verbatim. Missing map key means
not probed; `false` means probed and unavailable; `true` means present at list
time, not a permanent execution guarantee. Agent-type optional/empty
capability lists mean none declared, and empty color/glyph means no declared
accent. A client must not infer capabilities from the job prose.

## 14. Forward compatibility and raw preservation

Top-level unknown fields, frame kinds, request methods, and response methods
are tolerated. The top-level version remains strict. Raw envelopes carry
`payload: serde_json::Value`; clients MUST retain the original value and may
layer typed decoding on a clone/reference. This lets old clients store/replay
future event families without converting them to a lossy catch-all.

Some direct nested enums shipped without an unknown arm and are frozen; adding
a variant would break an old decoder, so expansion requires a new field/type or
a raw additive event family. Others explicitly absorb unknown variants. The
complete classification is normative in
[Client contract v1 — wire enum audit](client-contract-v1-enum-audit.md).

Never derive an enum's wire spelling from its Rust variant name. Read the
variant's `#[serde(rename = "...")]` first, then its enum-level serde rule, and
use the exact-spelling audit below. This is especially important for
initialisms: under serde `rename_all = "snake_case"`, Rust `OAuth` becomes
`"o_auth"`, while an explicit per-variant rename can make another `OAuth`
variant serialize as `"oauth"`.

## 15. Compatibility fixtures and change law

The machine-checkable contract lives in these fixtures/tests:

- `crates/haider-rpc/tests/fixtures/wire_transcript.json`: historical compact
  WebSocket bodies and four-byte length-prefixed UDS bytes, including
  Hello/Welcome, raw replay, menu CAS, accounts/providers/usage, and mutation
  receipts.
- `crates/haider-rpc/tests/fixtures/client_contract_methods_v1.json`: the
  methods added after the historical matrix, completing golden request and
  successful response coverage for all 70 request methods and all five
  command dynamic slots.
- `crates/haider-rpc/tests/fixtures/snapshot_availability_compat_v1.json`:
  old and new account/provider/usage response bytes.
- `snapshot_availability_is_compatible_in_both_n_minus_one_directions`:
  a v0.0.942-shaped reader ignores the new field while reading a new payload,
  and the current reader decodes an old payload with `availability=None`.

### 15.1 A green check says what ran

Test discovery is part of the receipt. In one crate, `cargo test` stopped at
the first failing test binary, so three later binaries never ran while the
gate printed `fail=0` and a crate containing 106 tests reported 47. In a
downstream client, the test script hand-listed 44 file paths, so a newly added
test file never ran; glob discovery immediately exposed three failures,
including one in a file its author had never executed. Neither instrument was
wrong; each answered a narrower question than the gate was asked to answer.
**A green check is a claim about what RAN, not about what EXISTS.**

Wire v1 evolves additively: new optional object fields with defaults, new
unknown-tolerant variants, new feature-gated methods, or raw event families.
Existing fields do not change meaning. A frozen enum does not grow. A required
field or incompatible meaning requires a new type/field or a wire-version
change. Months-old session payloads remain raw-decodable.

## 16. Known absences and limits of this revision

These are not guesses; they are explicit gaps in the current source contract.

**Two kinds of absence, and they warrant different client behaviour.** An absent
field looks identical from the client either way, so each entry below is
labelled:

- **[STRUCTURAL]** — the value does not exist in the daemon. "Unknown" is
  *permanently* correct. Render unknown and stop hoping: no future release fills
  it without the underlying capability being built first, which would be a
  contract change announced by a feature bit.
- **[UNPUBLISHED]** — the value exists internally but is not surfaced. "Unknown"
  is *temporary*. A client may reasonably expect it to appear and should not
  design around its permanent absence.

Neither kind licenses a substitute. This distinction governs what a client
should *expect*, never what it may *invent*; §1.1's prohibition on calculating
a replacement applies identically to both. The nearest available number is the
most dangerous thing in the room precisely because it resembles the missing one.

- **[STRUCTURAL]** roster deltas cannot announce removal; full `session.list`
  reconciliation is required;
- **[STRUCTURAL]** the RPC resident-binding baseline remains the single most-recent live
  publication, not an inventory of surfaces. `binding_token` lets a client
  correlate publications to tokens it minted and retire terminal reads, but
  does not make the daemon own or enumerate panes;
- **[STRUCTURAL]** there is no independent current-todo snapshot; normal raw replay supplies
  the durable lifecycle;
- **[STRUCTURAL]** `SessionSummary.account_alias` is not populated because
  **there is no per-session account binding to report**. `RequestBody` carries
  no account-selection request, session creation accepts only provider/model,
  and turn setup resolves the alias from the globally-resolved account. The
  field is empty because the fact does not exist, not because a publisher
  dropped it — so "unknown" is permanently correct until per-session binding is
  built. It must not be replaced with the global active account, which would be
  right for the current session by luck and silently wrong for every historical
  one. For the same reason `haider run --account` cannot work: it gated on
  `session_account_select_v1`, a feature string with zero daemon-side
  definitions, and now refuses honestly instead;
- **[STRUCTURAL]** old account/provider/usage payloads that omit `availability` retain their
  old ambiguous zero/empty sentinels. The new client can say “unknown,” but
  cannot reconstruct whether an old daemon measured an empty value;
- **[UNPUBLISHED]** a native sidecar is best-effort. The source defines validation and retry but
  no latency deadline by which it must catch the journal head;
- **[STRUCTURAL]** `title=None`, `effort=None`, and several lineage/config optionals deliberately
  combine an old-daemon case with a real domain absence. This document records
  the ambiguity instead of inventing a discriminator.

No additional precedence, latency guarantee, provider default, daemon-minted
pane identity, or per-session account binding could be established from
source, so none is claimed here.
