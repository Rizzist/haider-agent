# RPC facade integration (971-3 round 2)

`RpcDaemonService : ui.daemon.DaemonService` and `RpcAccountsRepository :
ui.accounts.AccountsRepository` implement the existing UI names. The shared
interface declarations were reconciled against UI round 4 commit
`d4e8a1e56d84dfd03aa24e600a5cd9a702dbe638`; no UI worktree or composable was edited.
The corrections below are intentional contract changes to those shared types,
not new RPC methods.

Lane 2 supplies `RpcControlPlane`: a process-scoped stream of sequenced Binder
snapshots, with null on Binder death, and its start/stop/restart methods. Its
`reportNotificationPermission` port forwards Activity permission observations
to the Android adapter; this is not an additional frozen C2 Binder or RPC method.
Lane 2 must publish the actual permission state through its existing snapshot path. Construct
one `RpcDaemonService` with that port, the app-private display-cache directory,
the C3 workspace and packaged provider/model/token defaults. Inject that service
and its `accounts` property into the UI. Only process shutdown disposes it.
`RpcEndpoint` and appVersion are read from the same snapshot. READY and RPC
CONNECTED are independently mapped. The service's monotonic timing/PSS fields
stay in Binder; no metrics or endpoint are inferred from the wire.

The facade loads the full roster eagerly, so `loadMoreSessions` has no pending
page (`hasMore=false`, `cursor=null`). Session rows preserve absent facts and
map needs-input/menu coordinates together. Created time comes from typed
metadata; provider/model never fall back to metadata when roster truth is absent.
Control attachments precede turn submission. Fork uses the actual observed
main-head node/sequence and refuses a missing node. Search includes all roster
heads and does not claim unsupported display items as complete. Display messages
fold final item replacement over streamed deltas. Account/provider inventories
remain memory-only; only safe roster/display projections are cached.

The round 4 `ModelOption`/`ModelDetail` interfaces are supported with efforts,
default effort, and context window projected per model. Unknown model details
remain empty/null. `stageApiKey` wipes the supplied array and returns only an
epoch-owned reference; `commitStagedApiKey` validates and commits via
`account.login_api`. Lost-epoch and consumed references are refused locally.
Retryable failures retain the semantic command ID across explicit restaging.
No plaintext is retained by the adapter. References are swept at the wire
`expires_at_ms`, retired on terminal/consumed outcomes, and superseded by a
fresh stage for the same command. Command receipts survive reference expiry.
Staging alone does not validate a key.
The same expiry and terminal-outcome cleanup applies to menu-secret and OAuth
ownership. Retryable OAuth completion retains its original completion command.

## Required UI-side changes at integration

- Collect `transcriptUpdates(activeSessionId)` with `collectLatest` when the active
  session changes. It performs one initial load and emits folded live pushes from
  the cache. Do not call the one-shot `transcript()` after each delta. The existing
  `ChatViewModel.loadTranscript` only reloads after explicit actions and otherwise
  misses subsequent assistant output. The fake inherits the one-shot stream
  default; its live scenarios should override it when testing streaming.
- Implement `rosterReady` on the fake (true only for its installed authoritative
  fixture). The real facade keeps it false until baseline hydration in the current
  epoch, withholds initial Running until its exposed rows agree with that baseline,
  and rejects create before hydration. Serialize `ChatViewModel.ensureActiveSession`
  with a Mutex, wait for readiness, and re-read `activeSessionId`/sessions inside
  that operation before choosing or creating. Multiple Running emissions must not
  schedule concurrent automatic creation.
- Compare the stable error code `already_resolved` in `ChatViewModel`; its current
  `ERROR_CODE_ALREADY_RESOLVED` literal is a Rust constant name, not a wire code.
- Handle `AccountResult` from `setActive` and `remove` in `AccountsScreen`.
  Announce removal only for `Ok`; render the returned failure for either action.
  The round 4 consumer currently discards both results and announces removal
  even after a revision, connection, or confirmation refusal.

- Merge the shared interface corrections: `AccountsSnapshot.revision: Long?`,
  `SessionRow.runState: String?`, `ProviderInventory.revision: Long?`, and
  `AuthKind.Unknown` / `OAuthStyle.Unknown`, plus redacted `toString()` for
  transient OAuth/menu capability types. Initial or absent revision is unknown,
  not revision zero. `FakeAccountsRepository` must keep its own known fixture
  revision when incrementing its five mutation paths; do not assign zero to a
  missing production revision. Unknown auth kinds need a neutral label.
- `validateApiKey` must surface `Failed("validate_only_unavailable")`; the fake's
  validate-only success and interface's old promise are wrong. The frozen wire
  validates as part of `account.login_api`, which also commits. No new validation
  door or implicit account commit was added.
- Treat a null `stageApiKey` result as staging unavailable and offer re-entry or
  retry. `AccountsScreen` currently labels it `invalid_api_key`, but staging can
  fail because of connection loss, expiry, or capacity and does not validate.
- `ProviderSummaryWire` has no label or `oauth_style`. Public provider ID is the
  label fallback. OAuth style remains Unknown until the UI can render a flow
  without inventing provider facts; the returned authorization URL is already
  the correct browser destination for authorization-code and device flows.
  Preserve the additive optional `user_code` returned by Rust OAuthStart and
  render it only when supplied. Do not fabricate a code when absent. Open the
  returned authorization URL unchanged; it may also carry the public device code.
- Replace the UI's duplicate permissive `AccountsRpcAdapter` parsers at the
  production composition root with `RpcAccountsRepository`. Its old parser maps
  absent account revision to zero, unknown auth to API key, missing auth methods
  to supported API key, and unknown OAuth status to Waiting. Those defaults
  conflict with the frozen contract. The production adapter reports unknown
  status as a terminal unsupported status and preserves revision absence.
- The existing `setActive`, `selectModel`, and `selectEffort` UI calls have no
  confirmation argument. The facade sends no `confirm_new_epoch=true`. If Rust
  requires confirmation, render the returned refusal and add an explicit
  confirmation interaction/API before allowing that retry; never infer consent.
- The UI's existing transcript interface supports Complete/Partial/Unavailable;
  keep those states visible. Unsupported tools/structured display payloads are
  deliberately Partial pending a reviewed renderer, rather than hidden behind a
  Complete result. The fake's view-only transcript description must not downgrade
  the production connection's Control attachment before a send.

No method rename is required. Native/Binder construction, notification permission
history, live APK/native verification and the composition-root replacement of
fakes remain integration work in lanes 971-2/UI/971-V. The data-plane facade does
not implement a second Android service or open the daemon database.
