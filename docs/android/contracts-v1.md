# Android 971 contracts v1 — FROZEN

Frozen 2026-09-07 by lane 971-0, technical validation of the orchestrator's DRAFT v0. Source candidate: `6dd86d338b93a4212f58ad2ba114133d166028f6`, Git tree `1cca7219a280c1e71a5910a0e96293ca80875cb5`. This is an implementation contract, **not evidence that the Android implementation has shipped or passed runtime tests**. No build or mobile execution was performed in this read-mostly lane.

Authority: approved `handoff/android971/ARCH.md`; `UI-SPEC.md` where consistent with ARCH; the owner's 2026-09-07 additions requiring full session history/search/replay, Settings account entry and on-device OAuth, and real third-party-app/order/payment-popup/return verification. These additions supersede UI-SPEC §6.5's history/search exclusions. ARCH supersedes its proposed private chat extension, `dataSync`, token bootstrap, and deferred emulator verification.

All “must”, signatures, layouts, and policy choices below are **new frozen requirements**, unless explicitly described as existing. Source references identify verified existing seams or constraints, not implemented Android functionality. Source anchors refer to the candidate above. To keep tables readable, `runtime.rs`, `config.rs`, `lifecycle.rs`, `worker.rs`, `accounts.rs`, `oauth.rs`, `mobile_transport.rs`, `connection.rs`, `endpoint.rs`, and `session_hub/rpc.rs` mean files under `crates/haider-daemon/src/`; `frame.rs` means `crates/haider-rpc/src/frame.rs`. Other paths are written in full.

## C1 — JNI v1: lifecycle only

Artifact: workspace package `haider-android`, library name `haider`, crate type `cdylib`, producing `libhaider.so`; arm64-v8a production and x86_64 test builds, API 26. This crate and native packaging do not yet exist in this candidate; the current APK configuration has minSdk 26 and no native configuration (`android/app/build.gradle.kts:28`, `android/app/build.gradle.kts:79`).

The JVM class is `ai.diffforge.haider.daemon.NativeDaemon`. Implement a final Java class with private constructor and static native methods, or bytecode-equivalent Kotlin with genuine static entry points. JNI symbols are `Java_ai_diffforge_haider_daemon_NativeDaemon_<method>`. Keep this name across shrinking. One Kotlin native-owner thread serializes calls; Rust additionally guards its process singleton against overlapping starts. Do not hold a global mutex while awaiting shutdown or run native startup on Android's main/Binder threads.

| Method | Java signature / JNI descriptor | Frozen behavior |
|---|---|---|
| `nativeVersion` | `static native String nativeVersion();` / `()Ljava/lang/String;` | JSON object: `jni_version:1`, `daemon_version:String`, `wire_protocol:1`, `build_id:String`, `abi:String`. No store, context, or runtime side effects. `build_id` identifies packaged native build; ABI is `arm64-v8a` or `x86_64`. |
| `nativeInit` | `static native int nativeInit(android.content.Context context, String pathsJson);` / `(Landroid/content/Context;Ljava/lang/String;)I` | Retain an application-context global reference, initialize ndk-context/logging, validate C3. Same valid context/path set is idempotent; changing the profile/path set during this process lifetime is rejected. |
| `nativeStart` | `static native int nativeStart(byte[] vaultDek, String policyJson);` / `([BLjava/lang/String;)I` | Validate/copy 32-byte DEK into zeroizing ownership, erase the passed array on every reachable return path, construct explicit config/dependencies and start asynchronously. `OK` means startup accepted, **not Ready**. Startup/recovery errors are subsequently observable. Kotlin also clears its original buffer in `finally`. |
| `nativeObserve` | `static native String nativeObserve();` / `()Ljava/lang/String;` | Nonblocking full JSON snapshot described below; safe before init and after shutdown. |
| `nativeShutdown` | `static native int nativeShutdown(boolean forced, long deadlineMs);` / `(ZJ)I` | `deadlineMs` is an elapsed-duration budget, not a wall-clock timestamp; zero selects 7,000 ms, negative is invalid. Idempotent graceful request; if forced, explicitly advance to forced state. Wait for the retained completion within the budget. |
| `nativeRelease` | `static native void nativeRelease();` / `()V` | Idempotent release after proven completion. If still active, perform the same bounded stop first. On timeout retain live runtime/context ownership, publish `SHUTDOWN_TIMEOUT`, and let the service terminate only its own `:daemon` process; never free context under running tasks or start a second runtime. |

`NativeStatus` is a single integer domain: `0 OK` (also graceful shutdown/already stopped), `1 ALREADY_RUNNING`, `2 NOT_INITIALIZED`, `3 BAD_PATHS`, `4 BAD_POLICY`, `5 VAULT_KEY_INVALID`, `6 STORE_RECOVERY_FAILED`, `7 INTERNAL`, `8 SHUTDOWN_TIMEOUT`, `9 SHUTDOWN_FORCED`, `10 BAD_ARGUMENT`. No separate undefined `GRACEFUL/FORCED/TIMEOUT` integer domain. `ALREADY_RUNNING` does not authorize adoption of an unknown incumbent. Errors carry stable codes, never panic payloads, secrets, aliases, or arbitrary native error strings.

Observe JSON keys: `jni_version:1`, `phase` (`Starting|Recovering|Ready|Draining|Failed|Stopped`), `daemon_generation` (nonnegative integer; zero means not yet known), optional `ready_since_unix_ms`, optional `endpoint_path`, optional `error_code`, optional `retryable`. Initial phase is `Stopped`. Optional keys are omitted when unknown. Endpoint and ready time occur only when the positive readiness predicate is true. On completion retain a terminal observation until the next start. Generation is the durable store generation, not a Kotlin retry counter or worker generation.

**Existing seam and required adapter work:** `spawn_with_dependencies` calls `tokio::spawn` in the entered caller runtime (`runtime.rs:157`, `runtime.rs:167`, `runtime.rs:481`). It does not require `run_with_signals` (`runtime.rs:492`). The current readiness snapshot supplies readiness/time (`lifecycle.rs:125`); diagnostics supplies state/finished/textual outcome/connection admissions, **not endpoint or generation** (`runtime.rs:204`). Lane 971-1 must expose immutable bootstrap metadata from the actual generation/path used at `runtime.rs:796` and `runtime.rs:1372`; do not fabricate these from Kotlin. No RPC addition is required. Real Ready is published after the prerequisite gate (`runtime.rs:1394`, `lifecycle.rs:143`).

`DaemonTask::join(self)` consumes its task and returns `Result<ShutdownOutcome, DaemonError>` (`runtime.rs:456`). Keep one pinned join future, or an owned completion task/receipt, across timeout attempts; do not drop `timeout(task.join())` and lose the join owner. `request("android-forced")` alone starts a **graceful** drain when it is the first request. For immediate force, call `request_graceful()` then `request("android-forced")`; subsequent graceful calls never force (`lifecycle.rs:392`, `lifecycle.rs:419`). Report actual `ShutdownOutcome`, including forced barrier expiry, not the requested mode (`lifecycle.rs:329`, `config.rs:55`). Dropping a Tokio JoinHandle detaches its task, and runtime shutdown does not cancel already-running blocking work; a timed-out native stop is not proof of quiescence ([Tokio JoinHandle](https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html), [Tokio Runtime](https://docs.rs/tokio/latest/tokio/runtime/struct.Runtime.html)).

Use unwind protection at each JNI boundary and at asynchronous owner boundaries. Never unwind into ART; map catchable Rust panics to `INTERNAL`. Do not promise recovery from abort, native crash, stack overflow, failed JNI allocation, or library linkage. Kotlin handles those failures as native/service failure; Java VM exceptions such as OOM cannot honestly be promised impossible. Do not publish `DaemonTaskDiagnosticSnapshot.outcome` directly—it is formatted native result text (`runtime.rs:181`, `runtime.rs:228`).

Runtime configuration is fixed as follows:

| Setting | Value | Evidence / consequence |
|---|---|---|
| Tokio | multi-thread, `enable_all`, 2 workers, 8 MiB thread stack, name `haider-android`; explicit 8 MiB native owner-thread stack | Binary precedent is four workers and 8 MiB (`crates/haider-daemond/src/main.rs:64`); wrapper must enter its own runtime. Blocking-pool bound is lane-decided and measured. |
| Frame body limit | 8,388,608 bytes | Current generic default is **48 MiB**, not 8 MiB (`frame.rs:57`). |
| Connections / ordinary queue | 4 / 8 frames per connection | Current defaults are 64 / 32 (`config.rs:113`). |
| Ordinary queued bytes | **8,388,612** (`frame_limit + 4`) | The draft's 8 MiB is invalid (`config.rs:156`). Encoded payload ceiling is `B + 2*(F+4)` per connection, hence 100,663,344 bytes across four connections; this is not total daemon RSS (`config.rs:29`). |
| Handshake / drain | 10 s / 5 s | Existing defaults and positive-bound validation (`config.rs:117`, `config.rs:175`). |
| Idle / discovery | `idle_ttl=None`, `discovery_disabled=true` | Fields/defaults at `config.rs:72`, `config.rs:119`. Discovery disable alone is not the complete C4 security policy. |
| SQLite | WAL; typed `store_synchronous=Normal` initially; allow typed `Full` for validation | The existing private enum/environment override must be threaded through daemon/core/store open paths, preserving desktop behavior (`crates/haider-store/src/event_store.rs:196`, `runtime.rs:757`). Adding only a DaemonConfig field is insufficient. |

`policyJson` v1 is exactly `{ "policy_version":1, "name":"android-standalone", "default_model":"<nonempty full model id>", "store_synchronous":"normal" }`, with `store_synchronous` also accepting `full`. Reject unknown fields/versions and an empty model; no caller-provided allowlist or process/environment override can widen C4. The service obtains the default from packaged configuration; account/model changes continue through RPC (`config.rs:67`, `frame.rs:4075`).

## C2 — Binder v1: service control and safe status

Package `ai.diffforge.haider.daemon`. The following are separate AIDL files, with the named types imported from that package. `RpcEndpoint` and `DaemonServiceSnapshot` are custom Parcelable types with separate `parcelable <Name>;` AIDL declarations. Parcel encoding is fixed in listed field order, including presence bits for nullable values; no Bundle of arbitrary data.

```aidl
package ai.diffforge.haider.daemon;
import ai.diffforge.haider.daemon.DaemonServiceSnapshot;
import ai.diffforge.haider.daemon.RpcEndpoint;
import ai.diffforge.haider.daemon.IDaemonSnapshotListener;
interface IHaiderDaemonService {
    DaemonServiceSnapshot getSnapshot();
    void registerListener(IDaemonSnapshotListener listener);
    void unregisterListener(IDaemonSnapshotListener listener);
    void startUserInitiated();
    void stopAndDisable();
    void restart();
    void prepareForUpdate();
    @nullable RpcEndpoint getRpcEndpoint();
}
```

```aidl
package ai.diffforge.haider.daemon;
import ai.diffforge.haider.daemon.DaemonServiceSnapshot;
oneway interface IDaemonSnapshotListener {
    void onSnapshot(in DaemonServiceSnapshot snapshot);
}
```

`RpcEndpoint` ordered fields: `path:String`, `wireProtocol:Int`, `daemonGeneration:Long`. It always describes **h.sock**, never mobile.sock or an OAuth HTTP listener.

`DaemonServiceSnapshot` ordered fields (all fields exist in every Parcelable):

| Field | Kotlin type / values |
|---|---|
| `enabled` | `Boolean` |
| `phase` | `String`: `DISABLED|STARTING|RECOVERING|READY|RESTARTING|STOPPING|ERROR` |
| `appVersion` | `String`, packaged app version |
| `nativeVersion` | `String`, empty until successfully queried |
| `wireProtocol` | `Int`, zero until known, otherwise 1 |
| `daemonGeneration` | `Long`, zero until known, otherwise actual durable generation |
| `rpcEndpoint` | `RpcEndpoint?`, non-null only in READY |
| `restartAttempt` | `Int`, nonnegative |
| `nextRetryUnixMs` | `Long?` |
| `network` | `String`: `AVAILABLE|UNAVAILABLE|UNKNOWN` |
| `notificationsGranted` | `Boolean` |
| `batteryRestricted` | `Boolean`, coarse battery restriction/optimization warning |
| `errorCode` | `String?`, stable redacted code |
| `errorRetryable` | `Boolean` |
| `snapshotSeq` | `Long`, increasing during one Binder service instance |
| `startedAtElapsedRealtimeMs` | `Long?`, appended to draft; service-owned monotonic start time |
| `pssBytes` | `Long?`, appended to draft; service-owned memory sample, not labelled RSS |

Snapshot fields preceding the two appended metrics retain the draft names/order. Source wire already separates daemon generation and worker generation (`frame.rs:828`, `frame.rs:1815`); Kotlin must not conflate them. Wire status has no service uptime or PSS; the UI-spec's `rssBytes = totalPss*1024` measures **PSS**. The service supplies its own measurement; unknown metrics stay null (`frame.rs:4609` lists the status response; see also UI-SPEC §5.3).

Every service method checks the calling UID against the package UID **before clearing Binder identity**, else throws `SecurityException`. Registering a listener immediately schedules one full snapshot, then full replacement snapshots; no callback under the state lock. Remove dead listeners. Getters are fast; mutators enqueue owner-thread work and return, with outcome delivered through snapshots. On Binder death clear endpoint/connection authority, reconnect, and reset the per-instance sequence baseline. Never compare `snapshotSeq` across different Binder instances.

`startUserInitiated` is invoked only from a visible user action. The facade arranges `startForegroundService` before/with binding, so a disabled unbound service can actually start. `stopAndDisable` persists disabled **before** drain. `restart` is an explicit user start/restart and clears the crash latch. `prepareForUpdate` drains while preserving enabled intent and records one bounded restart-after-replacement marker; it must not enable an intentionally disabled app. Mutators never acknowledge readiness merely because a command was enqueued.

The started-and-bound `HaiderDaemonService` must run in `android:process=":daemon"`, `exported=false`, `stopWithTask=false`, `START_STICKY`, foreground type `specialUse`, with `FOREGROUND_SERVICE_SPECIAL_USE` and a declared subtype: “User-enabled on-device AI daemon that maintains local sessions, responds to user automation events, and notifies the user when a running session requires attention.” Promote immediately before native load/recovery. The candidate has only accessibility and media-projection services (`android/app/src/main/AndroidManifest.xml:50`, `android/app/src/main/AndroidManifest.xml:62`); this is new service work. `specialUse` is the approved type and has an explicit manifest permission/subtype requirement ([Android service types](https://developer.android.com/develop/background-work/services/fgs/service-types)).

Boot after unlock and `MY_PACKAGE_REPLACED` honor the opt-in. Three unexpected exits within ten minutes latch ERROR; explicit restart clears it. A forced native timeout must not destroy the entire app UID or the UI process. Normal unbinding/UI death must not stop the daemon. Place capability components and their singleton buses together in `:daemon`; bridge the UI's screen-consent result explicitly. Current components use process-local singleton references (`android/app/src/main/java/ai/diffforge/haider/service/AndroidCapabilityHandler.kt:68`, `android/app/src/main/java/ai/diffforge/haider/service/AndroidCapabilityHandler.kt:82`).

Binder never carries transcript, credential material, staged/ready references, authorization URLs, vault data, or RPC frames. Data plane is full RPC over h.sock, with independent UI state `DISCONNECTED|CONNECTING|CONNECTED|PROTOCOL_ERROR`. JNI Ready and RPC Connected remain distinct. New metrics are safe status, not an RPC tunnel.

Notification channels are `haider_daemon_status` LOW, `haider_attention` HIGH, `haider_completion` DEFAULT. Explicit package-scoped immutable notification intents are `OPEN_SESSION{session_id,head_seq}`, `OPEN_INPUT{session_id,menu_id,request_seq,worker_generation}`, service actions `STOP_DAEMON` and `RESTART_DAEMON`, and `OPEN_DAEMON_STATUS`. Coordinates are navigation hints; re-observe before answering. Use only safe display fields from `NeedsInputWire` (`frame.rs:2154`). These rules do not rewrite the PackageInstaller's separate mutable status PendingIntent (`android/app/src/main/java/ai/diffforge/haider/update/PackageInstallerLauncher.kt:43`).

**UI facade compatibility:** retain the UI-SPEC §5.3 `DaemonService`/`DaemonStatus` facade locally. It composes Binder lifecycle with the lane-3 RPC repository; it is not the AIDL service. Map DISABLED→Stopped, STARTING/RECOVERING→Starting, RESTARTING→Restarting, ERROR→Failed, READY→Running only with truthful data-plane facts; show STOPPING as a local stopping/starting transition rather than accepting new turns. Preserve nullable SessionRow fields and same-snapshot cancellation/menu coordinates. History and account repositories are additive local interfaces owned jointly by lane 3/UI; no new Binder methods. The facade's old `models`/`SessionConfig` shape may be adapted locally from provider inventory and session selection—the private `session.config.get` is not a full-RPC method (`mobile_transport.rs:120`, `frame.rs:3833`, `frame.rs:4106`).

## C3 — explicit runtime paths and encrypted vault

`pathsJson` is an object with exactly six required String fields: `profile_id`, `store_dir`, `runtime_dir`, `logs_dir`, `workspace_dir`, `tmp_dir`. All directory fields are absolute app-private paths sourced from Android Context, never user text or ambient HOME. Version is fixed by JNI v1. Reject unknown keys, embedded NUL, traversal/symlink escape, wrong ownership and socket-budget overflow before opening the store.

| Field / artifact | Frozen location and ownership |
|---|---|
| `profile_id` | `android-default`; stable across app updates |
| `store_dir` | `filesDir/haider/profiles/default` |
| `runtime_dir` | `filesDir/haider/runtime/android-default` (profile scope first 20 Unicode scalar characters; this id is already shorter) |
| `logs_dir` | `filesDir/haider/logs`; five redacted rotations of approximately 1 MiB |
| `workspace_dir` | `store_dir/workspace`; immutable Android filesystem ceiling |
| `tmp_dir` | `runtime_dir/tmp`; native temporary storage only |
| Primary endpoints | `runtime_dir/h.sock`, `runtime_dir/mobile.sock`; directories 0700, sockets 0600, same-UID peers |
| Runtime records | `haiderd.pid` (existing name), owned temporary/staging entries; native/Welcome metadata is version authority, no separate `daemon-version` file required |
| Store artifacts | `store.sqlite` and SQLite sidecars, `accounts.json`, `cas/`, provider views, `vault/`; authoritative durable daemon state |
| Provider-lockdown state | `store_dir/lockdown` (separate from workspace); platform-trust bookkeeping, not an Android workspace-boundary substitute |
| Kotlin-only key wrapping | `noBackupFilesDir/haider/keys/wrapped-vault-dek` |
| Kotlin-only lifecycle | `noBackupFilesDir/haider/enabled-state/` containing enabled/retry/update state |
| Optional UI read cache/search index | `filesDir/haider/ui-cache/`, rebuildable from RPC, separate from daemon database; no credentials |
| Temporary diagnostic export | `cacheDir/haider/diagnostics/`; user-triggered, redacted, removed after export |

The direct `DaemonConfig::new(profile_id, store_dir, runtime_dir)` seam bypasses profile resolution, but calls `owner_scoped_runtime_directory`; verify that the resulting path remains the approved private path (`config.rs:101`). `endpoint_path_for` just delegates to `Endpoint::new`; it **does not** shorten a path or apply the 20-character scope (`crates/haider-client/src/profile.rs:417`). Scope/fallback live in the higher-level resolver (`crates/haider-client/src/profile.rs:421`, `crates/haider-client/src/profile.rs:462`), whose HOME fallback must not run (`crates/haider-client/src/profile.rs:324`). UI takes the final h.sock path from Binder, never derives it.

Validate **both** endpoints with `Endpoint::validate_for_bind`; use `Endpoint::from_address` for mobile.sock (`crates/haider-platform/src/ipc/mod.rs:372`). The bind bound is **107 path bytes on Linux/Android** (103 on Apple), including the longest temporary staging socket path, not just h.sock (`crates/haider-platform/src/ipc/unix.rs:27`, `crates/haider-platform/src/ipc/unix.rs:1380`). The separately exported portable/peer constant remains 103 (`crates/haider-platform/src/ipc/mod.rs:195`); do not confuse it with this platform bind check. No `/tmp` fallback on Android. Use platform endpoint binding/identity cleanup, not unlink-and-bind shortcuts. The opportunistic sweep currently recognizes h.sock and peer families, **not mobile.sock**; extend exact owned cleanup or rely on mobile's own BoundEndpoint cleanup, and test stale mobile recovery (`crates/haider-platform/src/ipc/unix.rs:1225`). The existing PID basename is `haiderd.pid`, not the draft's `daemon.pid` (`endpoint.rs:39`); there is no current daemon-version publisher in the surveyed runtime.

`tmp_dir` is an explicit embedding dependency. It does not automatically take effect through current DaemonConfig. The current runtime helper reads `std::env::temp_dir()` and recognizes a `.haiderd-tmp-*` child (`endpoint.rs:127`). Lane 1 must thread the private temp root through embedded consumers/cleanup, without racing process-global environment mutation in ART; do not claim that setting a path JSON key already controls all temporary files. The same applies to explicit logs and any retained subsystem with desktop HOME defaults. Store/CAS open already derive from store root (`crates/haider-store/src/event_store.rs:2135`).

The store database/transcripts remain sandbox-protected plaintext; only vault contents have application-layer encryption. Preserve `android:allowBackup=false` (`android/app/src/main/AndroidManifest.xml:22`). Do not restore ciphertext without its wrapping key, replace a lost key silently, or erase sessions to repair credentials.

### Vault file format v1

Inject `VaultProvision::Available(Arc<EncryptedFileVault>)` through `AccountsDependencies`; never let Android fall back to PlatformDefault's plaintext FileVault (`accounts.rs:609`, `accounts.rs:11973`). The vault implements all five operations, including nonblocking refresh-rotation locks (`crates/haider-accounts/src/vault.rs:99`). Retain profile scoping once: the daemon wraps the injected vault with ProfileVault and SourceLinkedVault (`accounts.rs:11984`).

The `alias` below is the **physical alias received by EncryptedFileVault**, normally `{first16hex(blake3("haider-vault-profile-v1\n" + profile_id))}::{logical_alias}`. It is not the UI label or bare account alias (`crates/haider-daemon/src/profile_vault.rs:17`, `crates/haider-daemon/src/profile_vault.rs:34`).

```text
filename = lowercase_hex(UTF8(physical_alias)) + ".vault"
file     = 48 41 56 31                # ASCII HAV1, 4 bytes
           || nonce                  # 12 fresh cryptographically random bytes
           || ciphertext             # exactly plaintext_length bytes
           || authentication_tag     # 16 bytes
algorithm = AES-256-GCM, 32-byte DEK, no compression or padding
AAD       = ASCII("HAV1") || u32be(alias_utf8_length) || UTF8(physical_alias)
```

This fixes the draft's ambiguous `{format:1,alias}` AAD serialization. Plaintext limit is 524,288 bytes; minimum encoded entry is 32 bytes and maximum 524,320. Preserve FileVault semantics for empty/missing/delete/list/refresh locking, but reject malformed magic, oversized/truncated records, failed authentication, wrong type/ownership, and symlink substitution before releasing any plaintext. Authentication/corruption is a typed non-`CredentialMissing` failure (for example `StoreCorrupt`), so ProfileVault's legacy missing-item fallback cannot conceal it (`crates/haider-daemon/src/profile_vault.rs:67`). No plaintext legacy fallback is permitted in this Android implementation.

All temporary **file** contents are ciphertext. Create a 0600 exclusive temp file beside the target, write/sync, atomically replace, sync the directory, and remove owned failed temporaries; preserve OS refresh-lock semantics. Existing FileVault uses this ordering and a 512 KiB plaintext bound (`crates/haider-accounts/src/file_vault.rs:30`, `crates/haider-accounts/src/file_vault.rs:88`, `crates/haider-accounts/src/file_vault.rs:184`). Normal absence is not corruption. A wrong key cannot be detected from length alone or from an empty vault; do not report successful credential recovery until ciphertext authentication has actually succeeded.

Kotlin in `:daemon` generates the random DEK and wraps it under a non-exportable Android Keystore AES-GCM key with `setUserAuthenticationRequired(false)`. Only the unwrapped DEK crosses Kotlin→Rust JNI at start. Keystore alias, wrapping-blob encoding and key-generation bookkeeping are lane-2-private and versioned there; they never cross Binder or appear in pathsJson. Lost/invalidated wrapping key or damaged entries fail closed and expose credential-reentry state, preserving the database and unaffected accounts. Any explicit reset must archive/remove only affected credential ciphertext under the account recovery flow; no automatic key regeneration over an existing vault.

## C4 — android-standalone tool and capability policy

The policy is a hard platform ceiling; ordinary permissions, provider trust, tool discovery, stored grants, subagents, recovered turns and remote routes cannot widen it. Retained tools still obey existing permission, provider, model, grant and availability gates. “Retained” is not a promise of unconditional advertisement on every session.

### Tool allow/deny tables

| Retained route | Public tool name(s) | Restrictions / source |
|---|---|---|
| RequestInput, ListTools, Plan, LoomRegister | `request_input`, `list_tools`, `plan`, `loom_register` | Actor-owned paths must use Android catalog too; Loom is in-process/provider workflow only, no CLI install or shell hooks (`worker.rs:13524`). |
| TodoWrite, GraphEvidence | `todo_write`, `graph_evidence` | Keep existing root/child and provenance restrictions (`worker.rs:13568`, `worker.rs:13848`). |
| FsRead, FsGlob, FsSearch | `fs_read`, `fs_glob`, `fs_search` | Constrained workspace reads (`worker.rs:13593`). |
| FsWrite, FsEdit, FsPath | `fs_write`, `fs_edit`, `write`, `edit`, `fs_path` | All aliases and rename/source/destination/parent operations obey the same ceiling (`worker.rs:13608`). |
| WebFetch, WebSearch | `web_fetch`, `web_search` | Preserve network policy and conditional web-search availability; rustls dependencies exist (`worker.rs:13677`, `Cargo.toml:56`). |
| Mobile | `mobile` | C4 mobile.sock, OS permissions, separate observation/control, user ControlGate, existing activation/grant rules (`worker.rs:13712`, `worker.rs:20874`). |
| Monitor, ListModels | `monitor`, `list_models` | Monitors use canonical report RPC and safe native/in-process sources; no shell-backed source (`worker.rs:13722`, `frame.rs:5997`). |
| SpawnSubagent, MessageSubagent | `spawn_subagent`, `message_subagent` | In-process descendants only; inherited hard ceiling; no typed-agent executable or CLI installation (`worker.rs:13640`, `worker.rs:13848`). |

| Excluded route | Public/legacy names that must not dispatch | Source |
|---|---|---|
| ProcessExec | `process_exec`, legacy `exec`, including any SSH/process selection inside arguments | `worker.rs:13633`, `worker.rs:14536` |
| TaskOutput, TaskKill | `task_output`, `task_kill` | `worker.rs:13656` |
| WorkflowAuthor | `workflow_author` | `worker.rs:13585`; exclusion is a conservative Android policy, not a claim that its entire implementation is a shell hook |
| Computer | `computer` | Android currently reaches UnavailableComputerBackend (`crates/haider-tools/src/computer.rs:475`) |
| PeerList, PeerSend | `peer_list`, `peer_send` | **Present** in this desktop catalog; draft's “already removed” is false (`worker.rs:13737`) |
| SshList, SshShell | `ssh_list`, `ssh_shell` | `worker.rs:13747` |

A dedicated **AndroidToolFactory backed by a policy-aware registered catalog** is required. `ConfiguredToolExposureFactory` simply returns the inner definitions/dispatcher and changes initial exposure (`worker.rs:2661`, `worker.rs:2680`). It removes neither routes nor the authorized registry. `list_tools` can promote authorized names; exposure is not enforcement (`worker.rs:13529`, `worker.rs:13778`).

Lane 1 must derive definitions, actor-owned dispatch, broker dispatch/aliases, tool inventory, permission defaults, discovery and child grants from the same Android-filtered catalog. Keep the desktop catalog unchanged. The current process-wide catalog and global inventory are direct callers that a wrapper alone would miss (`worker.rs:13792`, `worker.rs:14536`, `worker.rs:20870`, `session_hub/rpc.rs:15140`). Intersect allowed routes **before** backend construction/dispatch and repeat the platform check at effect execution. Reuse broker semantics; do not duplicate its permission/journal engine. Existing unknown-route behavior is `unsupported tool`, via the normal typed invalid-argument result—not literally `unknown tool` (`worker.rs:17710`). Denied routes/aliases must follow that path without executing an effect.

Generate `crates/haider-daemon/src/fixtures/android_standalone_tool_inventory.json` during lane-1 implementation from the actual Android catalog, with a documented deterministic session (root, mobile-use activated, no remembered grants). It does not exist yet. Also pin inactive-mobile and restricted-child projections; the current inventory conditionally excludes mobile and always excludes workflow_author for root inventory (`worker.rs:20874`). Attempt every excluded name, especially `exec`, through real dispatch and alternate ingress. A filtered JSON snapshot alone is not a route-denial test.

### Workspace and non-tool entry points

`lockdown_root_override` is **not** the Android filesystem ceiling. It selects storage for a process-global provider LockdownManager (`config.rs:80`, `runtime.rs:714`, `crates/haider-daemon/src/lockdown/mod.rs:799`); full-trust providers have no lockdown turn (`worker.rs:15070`). Existing broker roots come from **session metadata.cwd**, which a control RPC can change (`worker.rs:15485`, `frame.rs:3857`). The manager also remembers the first root for the lifetime of the process. Therefore initialize its bookkeeping at C3 `store_dir/lockdown`, and introduce a separate immutable Android workspace policy.

All root/child session creation, workspace selection, recovery, fork and turn setup must validate canonical cwd within `workspace_dir`, and every filesystem operation must enforce a handle-anchored boundary there, including traversal, absolute path, symlink/rename races and any secondary operands. Recovered out-of-scope sessions may be read but cannot run tools until their workspace is explicitly repaired. A provider-lockdown write sandbox must additionally be placed within the Android workspace (under a reserved private workspace subtree); do not let that special path escape into `store_dir/lockdown`. Ordinary Android workspace tools must not mutate lockdown ledgers. Existing lockdown's separate read/write sandbox semantics demonstrate why merely reusing its storage root is insufficient (`crates/haider-daemon/src/lockdown/mod.rs:845`, `worker.rs:15097`).

Disable shell hook execution/replay, typed-agent CLI installs/runners, SSH-agent discovery and SSH shell services, peer registration/network services, local microphone/STT, desktop credential-source scanning/import and gcloud. `discovery_disabled=true` only documents first-party store probing (`config.rs:72`); explicit RPCs and startup actors need policy checks too. Gcloud's injectable trait otherwise starts a process (`crates/haider-daemon/src/gcloud.rs:25`); local capture already returns `MicUnavailable` on Android (`crates/haider-stt/src/capture.rs:610`).

Keep the **136-method schema** but answer existing `capability_denied`/typed unavailable responses before unsupported side effects. At minimum cover `shell.*`, `ssh.*`, `peer.*`, `session.set_ssh_scope`, `computer.permission_open_settings`, credential-source enrollment/scan/import, OAuth CLI imports, shell/CLI Loom installation or execution, and hook-trust execution. Resolve `command.invoke`, `headless.run.start`, `turn.submit_from_cli`, `turn.submit_with_hook_trust`, and recovered work through the same ceiling; these must not be escape hatches. Read-only status surfaces can remain truthful and empty/unavailable. `provider.set_trust` cannot lift Android's ceiling. Do not expose the unused `transcription.secret_get` secret-export surface in standalone. These doors already exist in the pinned method set (`crates/haider-rpc/tests/wire_golden_tests.rs:682`); this requirement changes Android availability, not wire method names.

### mobile.sock v1

Separate filesystem socket for reverse device capabilities only. No session/account/OAuth RPC or private chat mirror. The canonical daemon hub must own monitor reports; do not forward `chat.*` over this socket in standalone (`frame.rs:5997`, `mobile_transport.rs:1023`). Retain the existing actor's pending-request correlation, connection replacement, cancellation and push handling, refactoring the TCP-specific stream/split into generic `AsyncRead/AsyncWrite` or `IpcStream` plumbing. Existing codecs are already generic, while acceptor/serve/actor take TcpListener/TcpStream (`mobile_transport.rs:429`, `mobile_transport.rs:711`, `mobile_transport.rs:752`, `mobile_transport.rs:835`). This is feasible reuse, not an already available UDS overload.

Bind through the platform endpoint helper; reject an unknown/different peer UID before reading a frame, and verify the server UID on the Kotlin LocalSocket. Keep owner-checked stale/inode cleanup under the profile lease (`crates/haider-platform/src/ipc/unix.rs:119`, `crates/haider-platform/src/ipc/unix.rs:1442`, `connection.rs:1266`). There is no bearer token. **Retain a bounded protocol hello**, removing only its authentication token; otherwise capability negotiation has no defined first response:

```json
{"id":1,"body":{"type":"hello","apkVersion":"<app-version>"}}
{"id":1,"body":{"type":"authOk","capabilities":["a11y.snapshot","a11y.tap","a11y.swipe","a11y.text","screen.capture","sms.list","app.open"]}}
{"id":0,"body":{"type":"capabilities.changed","granted":["accessibility"]}}
```

`authOk` is the retained envelope label for the completed peer-authenticated hello, not token success. Unexpected token/bootstrap/chat fields in standalone hello are rejected. Reply `authReject` with a redacted reason for invalid protocol hello after UID authentication, then close. Hello deadline 10 seconds. The existing hello requires id 1 and nonempty apkVersion and returns capabilities (`mobile_transport.rs:760`, `mobile_transport.rs:813`). Full package/native compatibility is also checked through C1/C2 and Welcome.

Framing: unsigned four-byte **big-endian** JSON-body length, 1..8,388,608 inclusive; UTF-8 JSON envelope with exactly `id` (signed 64-bit integer) and `body` (typed object). Cap lengths before allocation, including base64 expansion. ID 0 is an unsolicited push; positive daemon request IDs correlate replies on the current connection, never across replacement. Recent capability/SMS state must be reset/revalidated on reconnect. Most recently authenticated capability connection wins (`mobile_transport.rs:68`, `mobile_transport.rs:429`, `mobile_transport.rs:1143`, `mobile_transport.rs:1333`).

| Daemon request body | APK reply body | Required gate |
|---|---|---|
| `{"type":"a11y.snapshot"}` | `{"type":"a11yTree","nodes":[...]}` | Accessibility available |
| `a11y.tap`: `x,y,control:true` | `ack` with Boolean `ok` | Accessibility + control |
| `a11y.swipe`: `x1,y1,x2,y2,ms,control:true` | `ack` | Accessibility + control |
| `a11y.text`: `text,control:true` | `ack` | Focused editable node + control |
| `{"type":"screen.capture"}` | `png` with standard-base64 `base64` | Current MediaProjection consent |
| `sms.list`: optional `sinceMs,limit` | `smsList` with `messages:[{address,body,ts,read}]` | SMS grant |
| `app.open`: `pkg,control:true` | `ack` | Launchable package + control |

Use existing spelling/units and node fields `text,contentDesc,className,resourceId,bounds:[left,top,right,bottom],clickable`; replies can instead be `rejected{reason}` or `error{reason}`. Protocol truth comes from `mobile_transport.rs:1679`, `mobile_transport.rs:1830` and `android/app/src/main/java/ai/diffforge/haider/service/AndroidCapabilityHandler.kt:26`, `android/app/src/main/java/ai/diffforge/haider/service/HaiderAccessibilityService.kt:133`. `capabilities.changed.granted` uses existing grant names such as `accessibility`, not the seven server method names; preserve the mapping at `mobile_transport.rs:1608`. SMS push is `{id:0,body:{type:"sms.incoming",address,body,ts}}` with existing bounded validation (`mobile_transport.rs:1166`).

Observe/control remain independently brokered, and Kotlin checks both `control==true` and `ControlGate.enabled`; do not silently enable control when a peer authenticates (`android/app/src/main/java/ai/diffforge/haider/service/AndroidCapabilityHandler.kt:14`). Unsupported long-press, key events and app enumeration remain typed unavailable, not inferred successes (`mobile_transport.rs:1718`, `mobile_transport.rs:1747`, `mobile_transport.rs:1766`).

Standalone must never invoke the legacy mobile TCP/token bootstrap, which currently prints its generated token to stderr (`mobile_transport.rs:632`). A `legacy-termux` feature may retain it for separate builds, but enabling standalone must reject a conflicting legacy bootstrap configuration. This restriction concerns the **mobile capability listener**, not OAuth's short-lived browser callback listener.

**Third-party behavior acceptance:** leave capability ownership active when Haider UI backgrounds, open an actual other app, traverse its order flow, observe its payment WebView/new-window popup, return, and confirm the result from fresh observations. Current a11y snapshot reads only `rootInActiveWindow` despite requesting interactive-window retrieval (`android/app/src/main/java/ai/diffforge/haider/service/HaiderAccessibilityService.kt:39`, `android/app/src/main/res/xml/accessibility_service_config.xml:5`). Lane 2 must validate popup/window selection and improve traversal if required without changing the v1 node envelope. A gesture `ack` is not proof of an order/payment result. Use fresh screenshot/layout inspection; protected payment surfaces may require the user's interaction. No real charge is authorized by this contract. Physical-device/OEM/SMS/secure-window limitations must be recorded, never replaced by a host test.

## Full RPC, history and accounts/OAuth

### h.sock wire and history

Use full `WireFrame` JSON with top-level `v:1`/`kind`, request_id/body correlation, four-byte UDS framing, and Hello/Welcome. Kotlin requests JSON encoding and an 8 MiB receive body cap; validate the granted version/capabilities, native version and durable generation. Unknown additive fields/kinds are tolerated, wrong `v` is not (`frame.rs:55`, `frame.rs:771`, `frame.rs:823`, `frame.rs:5825`). UI needs View and Control as appropriate; notifier uses View. Multiplex Settings/OAuth on the UI connection instead of exceeding the four-connection budget.

| Need | Existing door and required use |
|---|---|
| Full roster | `session.list{cursor?,limit,order?}`; follow opaque `next_cursor` through **all** pages, including old/idle sessions; use recent-activity ordering where negotiated (`frame.rs:3415`, `frame.rs:4598`). |
| Live roster | `session.list_watch{}` and `session_roster_delta`; subscribe/buffer while fetching baseline, reconcile with authoritative head/generation, refresh on reconnect and drawer open. Deltas do not report removals (`frame.rs:3437`, `frame.rs:5874`). |
| Persistent replay | `session.attach{session_id,after_seq:0,mode:"view" or "control",sealed_replay?}` for full history; detach by attachment_id. Persist last **applied** seq only after projection/cache update. Dedupe by `(session_id,seq)` and reattach gaps; caught-up may repeat (`frame.rs:3602`, `frame.rs:5853`). |
| Search | Local title/metadata and transcript-content search index built from canonical RPC projections across the full roster, not just currently opened conversations. `session.read{session_id,range:{start_seq,end_seq}}` supports bounded indexing; ranges start at 1 and contain at most 1,024 envelopes; adapt page size to byte cap (`frame.rs:3463`, `session_hub/rpc.rs:17242`). Expose indexing progress/partial results; search completeness is known only after coverage through each recorded head. |
| Stopped/offline history | Preserve a rebuildable app-private read cache so already synchronized history/search stays readable while daemon is stopped; network loss alone does not prevent local RPC. Never write or read the daemon's SQLite directly. Full history after UI process death comes from this cache and/or RPC replay. |
| Detail/actions | `session.observe`, `session.create`, `session.rename`, `session.seen`, `session.fork`, `session.select_model`, `session.select_effort`, `turn.submit`, `turn.cancel`; use actual command/worker/run/fork coordinates, not the obsolete mobile proxy payloads (`frame.rs:3357`, `frame.rs:3470`, `frame.rs:3631`, `frame.rs:3697`, `frame.rs:3833`). |
| Human answers | `WireFrame::MenuAnswer` is a **top-level frame**, not a RequestBody method. Include command_id and optional request_id for a correlated reply; keep menu/request/worker coordinates from the rendering snapshot. `secret_answer` uses `vault.stage` with `purpose:"menu_secret"` and `MenuInput::SecretVaultReference`, never text (`frame.rs:1552`, `frame.rs:5805`, `frame.rs:5926`). |

SessionSummary is authoritative for optional provider/model/activity/run fields; preserve null/absence, title/lineage/workspace and head/generation (`frame.rs:1822`). Do not synthesize fine-grained “thinking” from coarse run_state. Index only permitted display transcript content; staged values, OAuth material and secret input must never enter the journal, search index, saved-state or logs. Very large historical envelopes exceeding the mobile negotiated limit require a truthful unavailable/error path, not silent truncation presented as complete history.

There is **no** `session.search`, `session.delete`, or `session.clear` in the 136-method set (`crates/haider-rpc/tests/wire_golden_tests.rs:763`). Search/replay require no additions with the above client-side design. Do not implement UI-SPEC's Delete/Clear as a fictitious RPC, file deletion or deceptive local-only deletion. A durable delete requirement needs a separately reviewed contract amendment. “Full history” here covers the owner's explicit roster/search/replay requirement.

### Accounts RPC surface (same-UID Control where required)

All the following remain reachable on **h.sock**, using the existing account actor/vault, not Binder or mobile.sock. `vault.stage` requires connection-level Control and LocalSameUid; the transport gate is already present (`session_hub/rpc.rs:5419`, `session_hub/rpc.rs:8613`). Account reads return descriptors, not credential bytes (`frame.rs:4098`).

| Settings operation | Existing RPC(s) |
|---|---|
| Load/refresh accounts | `account.list`, `account.list_watch` (re-read list on `accounts_changed`), `account.refresh` (`frame.rs:4092`, `frame.rs:4385`, `frame.rs:5882`) |
| Provider/model catalog | `provider.list`, `provider.models_refresh`; custom-provider setup may use `provider.models_probe`, `provider.configure`, `provider.remove` (`frame.rs:4106`, `frame.rs:4133`, `frame.rs:4175`) |
| API key add/replace | `vault.stage` purpose `api_key` → `account.login_api{command_id,provider,alias?,vault_reference,validation_model?,replace_existing?}` on the **same connection** (`frame.rs:3951`, `frame.rs:3967`) |
| OAuth start/progress/cancel | `account.oauth_start{provider,desired_alias,attempt_id}`, `account.oauth_status{flow_id,attempt_id}`, `account.oauth_cancel{flow_id,attempt_id}` (`frame.rs:3982`) |
| Commit OAuth account | `account.add{command_id,provider,alias,auth_method:"oauth",flow_id,attempt_id,oauth_reference}` after Ready (`frame.rs:4047`) |
| Manage account | `account.set_active`, `account.remove`, `account.set_default_model`, `account.set_label` with actual revision/confirmation fields (`frame.rs:4059`) |
| Provider trust | `provider.set_trust`, when shown, affects provider trust only and cannot lift C4 (`frame.rs:4514`) |

There is **no top-level `oauth.*` namespace**. Existing `account.oauth_import_sources` / `account.oauth_import` are **daemon-local CLI credential import**, not a browser callback API; standalone reports imports unavailable without scanning desktop paths (`frame.rs:4000`). Likewise source/device-discovery RPCs are not the phone's add-account path (`frame.rs:4012`). The stale AccountsDependencies comment saying the default OAuth catalog is empty is false; default construction reads the sanctioned registration table (`accounts.rs:637`, `oauth.rs:892`).

API-key entry uses a masked transient Settings field. Send the UTF-8 secret once in `vault.stage`; clear UI buffers after staging/submission/cancellation, do not put it in Compose saved-state, clipboard exports, debug frames or Binder. Complete login and durable descriptor commit before displaying “Added”. A lost login response is retried using the same semantic command ID, with freshly staged input only if required by the existing recovery response (`frame.rs:3957`, `session_hub/rpc.rs:8931`). The actual immutable Java String copies cannot be promised cryptographically zeroized; minimize their lifetime and never persist them.

### OAuth browser round trip: frozen zero-new-method choice

**Provider → daemon loopback capture → safe return Intent → UI → existing status RPC.** Do not replace a provider's registered redirect with an invented App Link/custom scheme. The OAuth redirect policy is loopback; `compose_redirect` has provider-specific behavior: OpenAI uses `http://localhost:1455/auth/callback` (registered fallback port 1457), Anthropic uses `http://localhost:<port>/callback`, and other authorization-code registrations use numeric IPv4 plus a random callback path. The listener itself binds 127.0.0.1. If both OpenAI ports are occupied, fail with `oauth_listener_unavailable`; do not cancel another application's listener or advertise an unregistered port. Android browser resolution of OpenAI/Anthropic localhost is a required live test, not a source-proven pass. Clients continue to use the daemon-returned URL and loopback_port without rewriting them; no wire method or custom-scheme callback ingress is added.

1. Settings creates a fresh attempt_id and calls `account.oauth_start` on the process-scoped full-RPC client. Rust generates state, PKCE verifier/nonce, binds the temporary listener **inside :daemon**, and returns its transient authorization URL/flow id/expiry, or typed unavailable (`oauth.rs:3051`, `oauth.rs:3105`, `frame.rs:5109`). Keep that RPC connection alive while the Activity backgrounds for consent. Activity rotation must not close it.
2. Open the daemon-supplied HTTPS authorization/verification URL in a Custom Tab or external browser, without rewriting query/redirect/provider metadata. URL state is sensitive and memory-only. Use a package-scoped immutable **Return to Haider** action in the Custom Tab, pointing directly to the Settings destination with no copied current browser URL; return action/Activity intent never forwards browser extras to Binder/RPC. External-browser fallback supports the callback page's return link and normal Back/Close. Custom Tabs support application PendingIntent actions ([Chrome Custom Tabs interactivity](https://developer.chrome.com/docs/android/custom-tabs/guide-interactivity)).
3. For authorization-code providers the browser follows the provider's exact loopback redirect directly into Rust capture. Keep existing host/path/state validation, bounded HTTP read, cancellation/expiry, and daemon-side token exchange (`oauth.rs:3808`, `oauth.rs:4318`). The OAuth listener is not UID-authenticated: a different-UID browser must reach it; state/PKCE/host validation protect it. This is separate from h.sock/mobile.sock owner authentication.
4. Lane 1 adds an **Android-only callback landing page** with a user-tappable `haider://oauth/return` link (and return instructions). No query, fragment, authorization code, state, token, flow ID, or ready reference belongs in that link. Preserve no-store/no-referrer headers (`oauth.rs:4930`). Lane 2 adds an exact BROWSABLE/DEFAULT/ACTION_VIEW intent filter for scheme `haider`, host `oauth`, path `/return` to the UI process; handle cold launch and `onNewIntent`. The current callback page is static and has no Android return path (`oauth.rs:4909`); the current manifest has no deep-link filter (`android/app/src/main/AndroidManifest.xml:27`). Custom schemes can be claimed by other apps, so this return Intent is only a navigation hint ([Android deep links](https://developer.android.com/training/app-links/create-deeplinks)). No authorization result is trusted from it. Without a live local attempt it only opens Settings; it never starts the disabled daemon, creates a flow or commits an account on its own.
5. On return (or foreground polling), the UI calls `account.oauth_status` over the **original h.sock connection**, using its in-memory flow_id/attempt_id. It may see Exchanging—the existing success page is sent before exchange completes (`oauth.rs:3905`). Only Ready yields the opaque ready reference, which is sent straight back in `account.add`. Tokens never leave Rust; commit completion is confirmed by the account descriptor/list (`oauth.rs:3942`, `oauth.rs:2853`, `accounts.rs:6340`).
6. Device flows (currently Kimi/Grok registrations) return user_code/verification URL while Rust retains device_code and polls the provider. They require no redirect listener and no callback forwarding. The Custom Tab Return action or close/back restores Settings, which polls the same status and commits identically (`oauth.rs:376`, `oauth.rs:406`, `oauth.rs:3272`, `frame.rs:5122`). Preserve server interval/slow-down/cancel/expiry handling.
7. Connection loss or UI-process death cancels that flow; restart sign-in with a new attempt, after first checking whether a durable account commit already succeeded. Do not persist a ready reference or pretend it can move to a new connection. Existing coordinator binds flow ownership to daemon instance, connection, and attempt and removes it on disconnect (`oauth.rs:2736`, `oauth.rs:2768`, `oauth.rs:2807`, `oauth.rs:2853`). A pending OAuth flow surviving UI-process death would require a separately designed resumability contract; ordinary on-phone consent completion does not.

**Boundary accounting:** API keys go UI→h.sock→daemon staging/vault. Authorization URL and public device user_code go daemon→UI→browser transiently; the authorization URL is intentionally classified secret-bearing (`frame.rs:947`). Browser authorization codes/state go browser→daemon loopback only. PKCE verifier, device_code, refresh/access/ID tokens stay in daemon memory/vault and daemon→provider HTTPS. An opaque staged/ready reference crosses h.sock back to its owning UI connection and is returned once; it is a sensitive capability, not a reusable credential. None of those values crosses Binder, notifications, diagnostics, transcript, search cache, or mobile.sock. Keystore wrapping keys never leave Keystore; unwrapped DEK crosses only service-process JNI at startup.

**Method decision:** zero new RequestBody methods and no new WireFrame kind are needed for the frozen roster/search/replay/API-key/OAuth design. The 136 pin and wire protocol version stay unchanged. A direct provider→App-Link callback containing a code **cannot** be forwarded using any existing account method; implementing that alternative would require a new reviewed callback-ingress API, approved provider redirect registrations and new goldens. It is not part of v1 and must not be smuggled through `account.oauth_import`, `vault.stage`, or Binder.

## Frozen versus lane-decided

| Frozen across lanes | Lane-decided implementation details / owner |
|---|---|
| Library/JVM names, signatures, status codes, JSON keys, start/stop ownership and no-signal lifecycle | JNI crate internals, bounded owner queue, Tokio blocking-pool sizing and diagnostic metadata seam — 971-1 |
| Two-process architecture, Binder methods/ordered fields, C2 metrics, service lifecycle/security/notifications | Parcelable code, backoff schedule below the latch, Keystore wrapping-file representation, receiver structure — 971-2 |
| Explicit path layout, profile identity, socket permissions/path budget, immutable workspace ceiling, HAV1 bytes/AAD | Secure file primitives, temp dependency plumbing, workspace-policy implementation, ciphertext recovery UI — 971-1/2 |
| Android allow/deny rules apply to definitions, dispatch and alternate ingress; no desktop-shell escape | Policy-aware registry factoring, feature organization, exact generated inventory fixtures — 971-1 |
| mobile.sock codec/hello/capability bodies and separate data planes | Generic actor refactor — 971-1; LocalSocket adapter, component/window lifecycle — 971-2/3 |
| Full RPC JSON v1, current 136 methods, canonical replay/coordinates; full searchable history | Kotlin parser, UI cache/index schema, repository names/paging UI — 971-3/UI |
| Account staging/commit surface, daemon-only token capture, safe OAuth return URI and same-connection lifetime | Custom Tab integration and Settings UX — UI/971-2; callback landing-page variant — 971-1; RPC repository — 971-3 |
| API 26, arm64 production/x86_64 test, libhaider.so, matching versions, NDK r28+ and 16 KiB verification | Cargo-ndk/Gradle task wiring, exact version pins and artifact checks — 971-3 |
| Real Android/third-party-app verification and honest unresolved physical-device/provider limits | Test package/merchant sandbox, screenshots, device selection and evidence harness — 971 integration verifier |

Lane 1 may need `haider-core` and `haider-store` changes for typed SQLite/temp/workspace dependencies in addition to ARCH's original directories; this is a real lane-cut expansion, not work performed by lane 0. Lane 2 and UI must agree ownership of `daemon/` models and resources before edits; lane 3 owns `transport/rpc/` and the full-RPC facade integration. Do not add a second incompatible `DaemonService` in parallel.

## Change log against DRAFT v0

1. Validate against actual wave-971 candidate, not the draft header's 71c4f45e; FROZEN is this uncommitted document's status, not an assertion of a commit/build.
2. Correct ordinary outbound budget to 8 MiB **+4**; source's old memory example is stale under the actual 48 MiB generic default (`config.rs:38`, `frame.rs:57`).
3. Define concrete shutdown status values, duration units, consuming-join ownership and forced escalation. Start acceptance is not Ready; release after timeout cannot free active context (`runtime.rs:456`, `lifecycle.rs:392`).
4. Specify missing generation/path metadata seam; existing diagnostics cannot produce the proposed observe object alone (`runtime.rs:204`). Bound panic claims to catchable unwinding.
5. Preserve Binder draft surface; append service timing/PSS fields and define snapshot sequencing/reconnect. Correct PSS/RSS naming and control/data-plane facade ownership.
6. Define exact HAV1 AAD/byte lengths, physical profile-scoped alias, refresh locks and corruption behavior. Clarify wrapped-blob format is Kotlin-private (`crates/haider-daemon/src/profile_vault.rs:34`).
7. Replace misuse of `lockdown_root_override` with a separate Android workspace policy; keep lockdown bookkeeping separate and address alternate RPC/recovery paths (`worker.rs:15070`, `worker.rs:15485`).
8. Require a policy-aware Android factory/catalog, including actor paths, inventory, aliases and child grants. Exposure is not route removal; peer tools still exist; unknown tool wording is `unsupported tool` (`worker.rs:2680`, `worker.rs:13737`, `worker.rs:17710`).
9. Specify reusable mobile actor refactor, retained non-token hello, explicit socket cleanup and grant-name mapping. Exclude legacy bootstrap/token logging; retain separate OAuth loopback listeners (`mobile_transport.rs:632`, `mobile_transport.rs:835`).
10. **Do not remove the update manifest entries**: `UpdateInstallReceiver` and `UpdateConfirmationActivity` are top-level classes in `android/app/src/main/java/ai/diffforge/haider/update/PackageInstallerLauncher.kt:69` and `:219`. Filename-only survey was wrong.
11. Add canonical full history/search/read-cache and on-phone Settings/API-key/OAuth contracts. No `oauth.*` namespace or callback-ingress RPC exists; choose existing loopback capture plus safe return navigation to preserve 136 methods (`frame.rs:3982`, `oauth.rs:4487`).
12. Correct outdated UI anchors: MenuAnswer and event/watch variants moved later in frame.rs; no `session.delete` exists. Full RPC, specialUse and real Android verification replace UI-SPEC's superseded private-chat/dataSync/no-emulator recommendations.
13. Correct runtime record assumptions: reuse `haiderd.pid`; do not invent an existing daemon-version file. Distinguish the portable 103-byte peer constant from Android's 107-byte bind validator (`endpoint.rs:39`, `crates/haider-platform/src/ipc/mod.rs:195`, `crates/haider-platform/src/ipc/unix.rs:1380`).

## Validation handoff (requirements, not executed passes)

The source method-set pin is 136 and compares the actual RequestBody declarations to an exhaustive list (`crates/haider-rpc/tests/wire_golden_tests.rs:579`, `crates/haider-rpc/tests/wire_golden_tests.rs:825`). Historical byte fixtures pin both compact JSON and length-prefixed UDS bytes (`crates/haider-rpc/tests/wire_golden_tests.rs:1353`, `crates/haider-rpc/tests/wire_golden_tests.rs:1406`); supplementary fixtures cover newer method shapes. These are distinct from future Kotlin consumption tests. Lane 3 must consume versioned fixtures for Hello/Welcome, every used account/OAuth/history door, replay/menu/unknown enum cases, and malformed/oversize framing without hand-reinterpreting the schema. Existing fixtures/pins are not regenerated by this lane.

Before publication, run exact-candidate local platform gates, Rust/Gradle checks, both ABI/API-26 cross-links, native load/UDS on supported API levels, 16 KiB runtime checks, vault failure/restart, real browser sign-in (including Anthropic localhost), real third-party order/payment-popup/return, lifecycle/permission/cancellation and source-policy escape tests. Astra must inspect actual screenshots/layout and preserve commands/exit codes and artifacts. Fake-provider/browser tests are necessary reproducible coverage but do not establish live-provider acceptance. Build or device limitations remain explicit NO_SHIP findings for the affected implementation, never a passing contract checkbox.
