# W9 updates + headless research

Research-only audit of branch `w9-headless` at workspace version `0.0.36`. No Rust code was changed. The two headline findings are:

- `haider run` already exists, but it is an in-process SQLite/`HarnessActor` path, not a daemon-backed client. W9 should migrate that command, preserving its JSONL and exit-code laws, rather than retain two execution authorities.
- A safe self-update is a staged two-binary transaction. Each path can be replaced atomically on macOS, but the `haider`/`haiderd` pair cannot be committed atomically with two renames; rollback metadata and restart ordering are therefore part of correctness.

## Q1

### Current CLI surface and version embedding

`haider-cli` does **not** use Clap: its dependency list has no `clap` ([crates/haider-cli/Cargo.toml:13](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/Cargo.toml:13)), and `main` manually matches `std::env::args()` ([crates/haider-cli/src/main.rs:55](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:55)). Consequently there is no Clap-derived version metadata to extend. The current surface is:

- `haider --version`, `haider -V`, and `haider version`; `haider self-test`; and noninteractive `haider --ready` ([main.rs:58](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:58)).
- `haider run ...`, `haider tui ...`, and `haider import ...`; unknown/incomplete invocations print the enumerated surface and exit 2 ([main.rs:68](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:68)). `tui` accepts live/demo/plain/theme variants ([main.rs:301](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:301)); `import` accepts `codex` or `claude-code`, or lists sources when omitted ([main.rs:123](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:123)).
- Bare `haider` connects to or spawns the profile daemon and enters the live TUI ([main.rs:81](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:81)).
- The existing `run` requires `--jsonl`, exactly one prompt, and either fake-provider defaults or `--provider anthropic --model <id>` ([main.rs:710](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:710)). It opens the SQLite profile directly and constructs an in-process actor/provider ([main.rs:480](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:480), [main.rs:538](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:538)). This is the path W9 must replace with daemon RPC.

The workspace version and repository are compile-time package metadata (`0.0.36`, `https://github.com/Rizzist/haider-agent`) ([Cargo.toml:20](/Users/rizzist/haider-run/haider-agent/Cargo.toml:20)). The CLI binds `VERSION` to `env!("CARGO_PKG_VERSION")` and prints `haider <version>` ([main.rs:31](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:31)); a CLI test pins that output ([cli_tests.rs:60](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/tests/cli_tests.rs:60)). The client puts the same package version in `Hello.client_version` ([crates/haider-client/src/client.rs:59](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:59)), and the daemon puts its package version in `Welcome.daemon_version` ([crates/haider-daemon/src/connection.rs:1358](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/connection.rs:1358)); Welcome has no git SHA, target triple, or build timestamp ([crates/haider-rpc/src/frame.rs:254](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:254)). `haiderd` currently has no `--version` or build-info command; its parser only accepts daemon/profile/store/runtime settings ([crates/haider-daemond/src/main.rs:133](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/src/main.rs:133)). W9 update preflight should add `haiderd --version` or boot the staged daemon against throwaway directories and verify its `Welcome` before live replacement.

### Profile, UDS, connect, and spawn seams

`ProfileInput::capture` reads the profile-related environment ([crates/haider-client/src/profile.rs:46](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/profile.rs:46)). Resolution defaults to `$HOME/.haider/dev-profile`, makes the path absolute, creates/canonicalizes it, and derives a stable profile ID ([profile.rs:149](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/profile.rs:149)). On macOS the runtime root is `/tmp/haider-<uid>` ([profile.rs:210](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/profile.rs:210)); the endpoint is `<runtime>/haider-<first-32-hex-of-BLAKE3(profile_id)>.sock` ([profile.rs:197](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/profile.rs:197)). The daemon delegates to the same endpoint helper, so client and server do not independently reinterpret the path ([crates/haider-daemon/src/config.rs:102](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/config.rs:102)).

`connect` opens that UDS, writes `Hello`, and accepts only `Welcome` or a typed `ProtocolError`, under the handshake timeout ([crates/haider-client/src/client.rs:194](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:194)). The default timeout is ten seconds ([client.rs:33](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:33)). Only `NotFound` and `ConnectionRefused` are spawnable failures; permission, protocol, profile, and other I/O errors are not converted into “start another daemon” ([client.rs:253](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:253), [crates/haider-client/src/spawn.rs:242](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:242)).

`ensure_daemon` first connects, verifies the profile identity and required features, and requires the daemon to be `Ready`; only a missing/refused endpoint enters the bounded spawn race ([spawn.rs:181](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:181), [spawn.rs:242](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:242)). Live mutation requires the context/session-mutation/turn-control feature set ([spawn.rs:34](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:34)). Packaging authority is the sibling `haiderd` next to `current_exe()`; a daemon elsewhere on `PATH` is diagnostic only and is never silently executed ([spawn.rs:330](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:330)). Spawn passes the exact resolved profile/store/runtime directories, nulls stdin, writes an owner-only log, and puts the daemon in a detached process group so it survives CLI exit ([spawn.rs:356](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:356)).

### Hello/capabilities and version-skew behavior

The default client identifies itself with a fresh instance ID, `ClientKind::Cli`, and View+Control capabilities ([client.rs:44](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:44)). `Hello` carries protocol min/max, client name/version/instance/kind, requested capabilities, and frame limit; `Welcome` returns selected protocol, daemon instance/generation/version/state, profile identity, granted capabilities, frame limit, and features ([crates/haider-rpc/src/frame.rs:205](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:205), [frame.rs:254](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:254)). Negotiation checks protocol overlap and capability intersection, not semantic package-version equality ([crates/haider-rpc/src/negotiation.rs:28](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/negotiation.rs:28)).

Therefore there is **no client/daemon version handshake today**. Different `client_version` and `daemon_version` strings connect normally when wire v1, profile identity, state, and required features agree. A real-daemon lifecycle test connects using client version `test` ([crates/haider-daemond/tests/lifecycle_tests.rs:198](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/lifecycle_tests.rs:198)); the client suite accepts a fake daemon version `0.0.1-fake` ([crates/haider-client/tests/client_tests.rs:164](/Users/rizzist/haider-run/haider-agent/crates/haider-client/tests/client_tests.rs:164)).

What fails is protocol or feature skew. A non-overlapping wire range is fatal and `ensure_daemon` does not spawn/kill a replacement ([lifecycle_tests.rs:616](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/lifecycle_tests.rs:616), [spawn.rs:228](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:228)). A live daemon missing required features yields an “old/incompatible daemon” diagnostic but is likewise never replaced ([spawn.rs:202](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:202)); the CLI maps protocol/profile/feature front-door skew to exit 76 ([main.rs:286](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:286)). For update health checking, W9 must explicitly require `Welcome.daemon_version == target`; the existing handshake will not do that for it.

## Q2

### Daemon-backed one-shot flow

`RpcClient` is a generic correlated-request plus uncorrelated-frame client, not a typed session facade. Callers take the event receiver, issue `RequestBody` values through `request`/`begin_request`, and send top-level menu answers through `send_frame` ([crates/haider-client/src/client.rs:406](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:406), [client.rs:422](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:422)). The exact one-shot transaction is:

1. Resolve the profile and `ensure_daemon` with Headless, View+Control, and the live feature set. `ClientKind::Headless` already exists ([crates/haider-rpc/src/frame.rs:171](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:171)).
2. Take the event receiver **before** submitting work.
3. Send `SessionCreate { command_id, cwd, provider, model, max_tokens }`; retain the returned `session_id`, `worker_generation`, and metadata.
4. Send `SessionAttach { session_id, after_seq: 0, mode: Control }`, then wait for the attach response/barrier. Submit requires both Control capability and a live Control attachment ([crates/haider-daemon/src/session_hub/rpc.rs:173](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:173)); the daemon gates replay so the attach response is visible before its first replay event ([rpc.rs:2100](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:2100)).
5. Send `TurnSubmit { command_id, session_id, worker_generation, text, attachments, mode }` and retain `run_id`, `accepted_seq`, generation, and disposition ([crates/haider-rpc/src/frame.rs:693](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:693), [frame.rs:923](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:923)).
6. Reduce only envelopes for that session/run until a terminal run state. Events can arrive before the `TurnSubmit` response; the wire explicitly disclaims socket-order causality ([frame.rs:952](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:952)). Buffer events by `run_id` until the response identifies the accepted run.

The TUI live driver is the production precedent. It queues the first prompt across create → attach and emits submit only once attached ([crates/haider-tui/src/live.rs:987](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/live.rs:987), [live.rs:1028](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/live.rs:1028)); its link maps commands to wire requests ([crates/haider-tui/src/link.rs:497](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/link.rs:497)) and concurrently forwards frames while awaiting responses ([link.rs:329](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/link.rs:329)). W9 should extract/share the attach barrier, cursor reducer, and reconnect law below the TUI rather than make `haider-cli` depend on a presentation reducer.

The closest daemon-backed headless precedent is the real-UDS test client: it handshakes as Headless with View+Control ([crates/haider-daemond/tests/support/mod.rs:91](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/support/mod.rs:91)), implements create → Control attach → `CaughtUp` ([crates/haider-daemond/tests/live_turn_rpc_tests.rs:276](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:276)), submits a turn ([live_turn_rpc_tests.rs:341](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:341)), and filters by run ID until Done/Errored/Cancelled ([live_turn_rpc_tests.rs:505](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:505)). `haider-verify` is only a scaffold ([crates/haider-verify/src/lib.rs:1](/Users/rizzist/haider-run/haider-agent/crates/haider-verify/src/lib.rs:1)); the PTY probes drive the TUI and are not a no-TTY precedent ([scripts/tui-probes/pty-probe-live.py:2](/Users/rizzist/haider-run/haider-agent/scripts/tui-probes/pty-probe-live.py:2)).

### Stream reduction and completion

The headless consumer needs these laws:

- Envelope delivery is at-least-once. Drop `seq <= last_applied`; if `seq > last_applied + 1`, apply nothing across the gap and reattach from the last fully applied sequence ([crates/haider-rpc/src/frame.rs:1263](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:1263)). The TUI reducer implements this exact rule ([crates/haider-tui/src/projection.rs:250](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/projection.rs:250)). `RpcClient` may drop events under pressure and exposes `lost_events()` specifically so the caller can cursor-reattach ([crates/haider-client/src/client.rs:621](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:621)).
- Only `RunState::{Done, Errored, Cancelled}` are terminal. `Waiting`, `InputRequired`, `PermissionRequired`, `Compacting`, `Verifying`, and `EffectOutcomeUnknown` are parked/nonterminal; neither session Idle nor an item completion ends the command ([crates/haider-protocol/src/state.rs:52](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:52), [state.rs:121](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:121)).
- An accepted provider/runtime failure is a durable `RunFailed { code, message, retryable }` immediately followed by `RunState::Errored` ([crates/haider-protocol/src/lib.rs:40](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/lib.rs:40), [crates/haider-daemon/src/worker.rs:3162](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3162)). A `ResponseBody::Error` before acceptance is an immediate RPC failure, not a run terminal ([crates/haider-rpc/src/frame.rs:1100](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:1100)).
- For final text, reduce item lifecycle by `item_id`: Started, zero or more Delta events, then Completed. Completed is authoritative replacement, not text to append again ([crates/haider-protocol/src/item.rs:82](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:82), [crates/haider-tui/src/projection.rs:438](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/projection.rs:438)). Final assistant output comes from `TurnItem::AgentMessage` ([item.rs:18](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:18)).
- `--timeout` should be a wall-clock deadline. On expiry, send durable `turn.cancel`, continue consuming through a short bounded grace until a correlated terminal, and return the timeout outcome even if the resulting terminal is Cancelled. Simply dropping the socket can leave the daemon run active.

The old in-process `run` remains useful as a migration oracle: it writes every committed `RawEnvelope` as LF-delimited JSONL, catches lag by reading the durable store, and performs a final drain ([crates/haider-cli/src/main.rs:538](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:538), [main.rs:648](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:648)). Tests pin LF framing and terminal Done, provider failure, Cancelled=130, slow-consumer completeness, and non-hanging store failure ([crates/haider-cli/tests/cli_tests.rs:123](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/tests/cli_tests.rs:123), [cli_tests.rs:269](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/tests/cli_tests.rs:269), [cli_tests.rs:307](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/tests/cli_tests.rs:307), [cli_tests.rs:377](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/tests/cli_tests.rs:377)). Preserve these laws while moving authority to `haiderd`.

### Approval behavior and the additive policy seam

W8a production defaults are `FsRead=Allow`, `FsWrite` (including patch)=Ask, `ProcessExec=Ask`, and `AgentSpawn=Allow` ([crates/haider-daemon/src/worker.rs:3419](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3419)). An ask commits a permission `MenuOpened`, parks the run, and waits for a durable menu CAS ([crates/haider-core/src/actor.rs:2316](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2316)); permission menus have no TTL, so an ignored ask hangs indefinitely ([crates/haider-tools/src/broker.rs:1290](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1290)).

Default headless policy should fail closed but not mislabel the ask as terminal:

1. On `MenuOpened(MenuKind::Permission)`, select the **server-enumerated** option whose typed decision is `RejectOnce`; never assume an index/key or parse a display label ([crates/haider-protocol/src/menu.rs:60](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/menu.rs:60)).
2. Print/emit a clear `permission_denied_by_headless_default` message, then send `WireFrame::MenuAnswer` with the opening `seq` and current `worker_generation`; the daemon records this wire path as `AnswerVia::Rpc` ([crates/haider-rpc/src/frame.rs:1284](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:1284), [crates/haider-daemon/src/session_hub/rpc.rs:2166](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:2166)).
3. Continue until a terminal run state. A denied tool becomes a typed tool result and the model may still finish Done; real write and exec denial tests demonstrate that behavior ([crates/haider-daemond/tests/live_turn_rpc_tests.rs:4613](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:4613), [live_turn_rpc_tests.rs:4498](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:4498)).

For non-permission `InputRequired` menus (question, secret, file, recovery), there is no honest one-shot answer: emit `input_required`, request cancellation, and exit nonzero rather than hang. Never auto-resolve `EffectOutcomeUnknown`; it requires interactive recovery.

There is no per-session policy override today. `SessionCreate` has only command/cwd/provider/model/max-tokens ([crates/haider-rpc/src/frame.rs:693](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:693)); durable session metadata has no policy field ([crates/haider-protocol/src/session.rs:5](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/session.rs:5)); and the worker constructs policy only from registry defaults plus durable session grants ([crates/haider-daemon/src/worker.rs:3511](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3511)).

The minimal additive seam for `--allow-writes`/`--allow-exec` is an optional/defaulted `SessionPermissionOverridesV1` on `SessionCreate` and `SessionMetadataV1`, preferably typed booleans or validated entries limited initially to `FsWrite=Allow` and `ProcessExec=Allow`. Include it in create-command idempotency/digest, persist it, and apply it after W8a registry defaults when constructing the session dispatcher. Advertise `session_permission_overrides_v1` in `Welcome`; require that feature when either flag is present so an older tolerant daemon cannot silently ignore requested preauthorization. These model effects must journal ordinary policy `Allow`, not forged `PreAuthorized(UserTyped)`, which is reserved for direct user-typed effects ([crates/haider-protocol/src/effect.rs:41](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/effect.rs:41), [crates/haider-tools/src/broker.rs:1067](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1067)). Auto-answering `AllowOnce` menus would be post-hoc approval, not the requested preauthorization.

### Output contract proposal

- `--output print` (default): stdout contains only the final assistant text plus one trailing LF; progress, permission-denial notices, and errors go to stderr; never emit ANSI or require a TTY.
- `--output json`: one LF-terminated, versioned CLI object, for example `{"schema":"haider.run.v1","session_id":...,"run_id":...,"outcome":"done|errored|cancelled|timeout|input_required","response":...,"usage":...,"permission_denials":[...],"error":...}`. Fields should be additive within v1; breaking changes require `haider.run.v2`.
- `--output jsonl`: retain the existing frozen `RawEnvelope` JSONL contract, one compact envelope per LF. Protocol/envelope schema v1 and sequence coordinates are already explicit ([crates/haider-protocol/src/lib.rs:1](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/lib.rs:1), [crates/haider-protocol/src/envelope.rs:34](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/envelope.rs:34)). If W9 needs CLI-only metadata, use a separately versioned wrapper rather than silently changing `RawEnvelope` lines.

## Q3

### Release discovery and staged verification

The release workflow builds only `aarch64-apple-darwin` and `x86_64-apple-darwin` ([.github/workflows/release.yml:15](/Users/rizzist/haider-run/haider-agent/.github/workflows/release.yml:15)). It checks tag == workspace version, builds both binaries, ad-hoc signs both, and smoke-checks `haider --version` ([release.yml:28](/Users/rizzist/haider-run/haider-agent/.github/workflows/release.yml:28)). It packages one top-level `haider-vX.Y.Z-<target>/` containing sibling `haider` and `haiderd`, creates `haider-vX.Y.Z-<target>.tar.xz`, then writes the `.sha256` asset ([release.yml:45](/Users/rizzist/haider-run/haider-agent/.github/workflows/release.yml:45)). The checksum line currently names `dist/<asset>`, so parse exactly one 64-hex digest and require the referenced **basename** to equal the expected asset; do not require the path prefix to match the downloaded location.

Do not use GitHub's `/releases/latest`: GitHub defines it as the most recent non-prerelease/non-draft release ([GitHub Releases API](https://docs.github.com/en/rest/releases/releases#get-the-latest-release)), while this repository publishes every current release with `--prerelease` until v0.1.0 ([release.yml:71](/Users/rizzist/haider-run/haider-agent/.github/workflows/release.yml:71)). List published non-draft releases, explicitly include the repository's prerelease channel, parse `v<semver>`, and choose the highest admissible release with exactly the expected archive/checksum pair. Choose target from the running binary's compiled architecture, not physical host hardware, so an x86_64/Rosetta installation updates to another x86_64 binary.

Before touching the install directory:

1. Download the archive and checksum into an owner-only private staging area as `.part` files. Enforce response/status, bounded size, content-length/EOF when supplied, and close the completed files before verification.
2. Verify the archive SHA-256. A partial download, malformed/ambiguous checksum, name mismatch, or hash mismatch stops here with zero backup, replacement, or daemon actions.
3. Strictly extract only the expected top directory and the two regular files. Reject absolute/traversal paths, symlinks, hardlinks, devices, duplicate or extra members, missing binaries, wrong target, and decompression beyond fixed count/size limits.
4. Stage on the **same filesystem as the install directory**, fsync each staged file and its directory, and do all executable/signature smoke checks there.

### macOS replacement, xattrs/signing, and rollback

Use rename-then-replace, never unlink-first and never truncate/write a live executable. Apple's `rename(2)` requires the source and destination to be on the same filesystem, replaces an existing destination, and guarantees that an instance of the destination exists through a crash ([Apple `rename(2)`](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/rename.2.html)). The updater process can rename its own installed path: the running process retains the old mapped inode while the canonical path begins naming the new inode. The same applies to a running old daemon. This is why staged same-filesystem renames are safer than unlink+copy.

The project calls the downloads “unsigned,” meaning no Developer ID/notarization, even though the workflow already embeds ad-hoc signatures ([release.yml:34](/Users/rizzist/haider-run/haider-agent/.github/workflows/release.yml:34), [README.md:11](/Users/rizzist/haider-run/haider-agent/README.md:11)). Follow the stated distribution memory on **staged files only**:

- remove only `com.apple.quarantine` when present, not every extended attribute;
- run `/usr/bin/codesign --force --sign - --timestamp=none` on both staged binaries;
- verify both with `codesign --verify --strict`, then run staged version/self-test smoke checks.

Apple documents that `-s -` applies an ad-hoc “Sign to Run Locally” signature and that Apple silicon requires signed code ([Apple helper-tool guidance](https://developer.apple.com/documentation/xcode/embedding-a-helper-tool-in-a-sandboxed-app)); Apple also documents `codesign --verify`/`--strict` verification ([TN3161](https://developer.apple.com/documentation/technotes/tn3161-inside-code-signing-certificates)). Local re-signing changes bytes after archive verification, so retain the source digest as transport evidence and separately verify the re-signed staged outputs. Never clear xattrs or re-sign the live/running paths.

A two-file pair is not atomically committable with two renames. The minimum recoverable transaction is:

1. Acquire an owner-only update lock in the install directory. Validate that `current_exe()` is the expected writable `haider` installation and that the sibling `haiderd` is a regular file; refuse managed, read-only, or ambiguous symlink layouts.
2. Create same-filesystem, fsynced backups of **both** installed binaries and a transaction marker naming old/target versions and phase.
3. Rename the staged `haiderd` into place first, then staged `haider`; fsync the install directory, re-read both canonical paths, and verify both new binaries.
4. Only after both canonical paths are the verified target pair may restart begin. If a daemon was running before the update, keep backups until the new daemon passes its exact-version health check; if none was running, leave it stopped and use the staged daemon preflight as the proof.
5. On any failure before restart, restore every touched canonical path by rename while the old daemon is still running; do not signal it. On restart/health failure, safely stop any new daemon, restore both backups, restart the old sibling, and report rollback. Delete backups/marker only after success.

Installing daemon first is the less dangerous transient skew because current clients tolerate additive newer daemons, but it does not make a two-rename transaction crash-atomic. The marker must let the next updater recover a power loss after only one swap; versioned directories plus one atomic “current” indirection would be a stronger future design.

### Graceful daemon drain and restart

Auto-spawn deliberately never kills or replaces a live daemon ([crates/haider-client/src/spawn.rs:6](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:6)). There is no daemon-shutdown RPC, `Welcome` has no PID ([crates/haider-rpc/src/frame.rs:254](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:254)), and the PID text in the profile lock is explicitly diagnostic and must never drive a decision ([crates/haider-store/src/profile_lock.rs:1](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/profile_lock.rs:1)).

The existing safe shutdown primitive is a first SIGTERM/SIGINT: it requests graceful drain; any later signal forces shutdown ([crates/haider-daemon/src/runtime.rs:140](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/runtime.rs:140), [crates/haider-daemon/src/lifecycle.rs:194](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/lifecycle.rs:194)). The default whole-barrier deadline is five seconds ([crates/haider-daemon/src/config.rs:54](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/config.rs:54)). Drain order is load-bearing: close listener and gate new work; settle/cancel or park workers; drain the session hub; send `ServerDraining`; drain writers; flush; remove the exact socket; close the store/profile lock last ([crates/haider-daemon/src/runtime.rs:517](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/runtime.rs:517), [runtime.rs:936](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/runtime.rs:936)). Graceful outcome promises connections closed, store flushed, socket removed, and lock released last ([crates/haider-daemon/src/lifecycle.rs:167](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/lifecycle.rs:167)).

For a bootstrap-compatible W9 restart, add a small `haider-client` seam that captures the UDS peer PID from kernel peer credentials before `UnixStream::into_split`; Tokio exposes the peer PID on macOS through [`UnixStream::peer_cred`](https://docs.rs/tokio/1.53.1/tokio/net/struct.UnixStream.html#method.peer_cred). `connect` currently consumes the stream without retaining it ([crates/haider-client/src/client.rs:194](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:194), [client.rs:323](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/client.rs:323)), while the server already treats same-UID peer credentials as its local trust boundary ([crates/haider-daemon/src/connection.rs:872](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/connection.rs:872)). The update flow should:

1. Direct-connect without auto-spawn and retain the matching Welcome identity plus kernel-authenticated peer PID. If no daemon is running, update must not start one merely to stop it.
2. After—and only after—the two-binary commit, send exactly one SIGTERM while that authenticated connection still proves PID identity. Never use the lock-file PID and never send a second signal on timeout.
3. Observe matching `ServerDraining`, wait for disconnect, then prove finalization by acquiring and releasing the actual OS profile lock—never by reading its PID payload—before spawning the new sibling. Do not spawn in the socket-removed/lock-still-held gap; current `ensure_daemon` can lose that race after one candidate exits 75 ([crates/haider-store/src/profile_lock.rs:21](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/profile_lock.rs:21), [crates/haider-client/src/spawn.rs:288](/Users/rizzist/haider-run/haider-agent/crates/haider-client/src/spawn.rs:288)).
4. If a daemon was running before the update, require `Ready`, the expected profile/feature set, and `Welcome.daemon_version == target` before deleting backups. If none was running, do not spawn one. The daemon's Welcome version comes from its own package build ([crates/haider-daemon/src/connection.rs:1358](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/connection.rs:1358)).

An additive authenticated `daemon.shutdown_v1` request could replace peer-PID signaling later, but the signal seam works with the already-shipped graceful-drain implementation. The update-specific spawn path should retain the spawned child until health succeeds so a never-ready new daemon can be stopped before rollback.

Drain is not behaviorally free: ordinary active turns are cancelled, while input/permission/local-child checkpoints can be parked for next-generation recovery ([crates/haider-daemon/src/worker.rs:1612](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:1612), [worker.rs:2236](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2236)). Update UX must state that current-profile active work may be cancelled, or W9 needs an occupancy/refusal seam.

### Version gate

Use monotonic SemVer by default:

- `target > current`: admit the transaction.
- `target == current`: successful no-op, with no download, replacement, signing, or restart. Do not silently reinterpret it as repair/reinstall.
- `target < current`: refuse before download or any local mutation.
- malformed tags, architecture mismatch, missing/duplicate assets, and release/tag/checksum name disagreement: refuse.

If repair or downgrade is added later, make the authorities distinct (`--reinstall` for same-version and `--allow-downgrade --version <exact>` for lower), not one ambiguous `--force` flag.

## Q4

Use two feature tracks and four independently reviewable slices.

### W9a1 — update discovery and immutable staging

Implement release listing/channel policy, target selection, SemVer gate, bounded download, checksum parser, strict archive extraction, quarantine handling, local ad-hoc signing, and staged smoke checks. It must have no API that can mutate canonical installed paths.

Minimum laws:

- Newer version and exact target asset pair are selected for both macOS architectures; same version is a zero-call no-op; downgrade/malformed/asset mismatch refuses before download.
- Current GitHub-prerelease releases are discoverable; SemVer ordering is independent of API list order.
- Injected HTTP disconnect/truncated length leaves installed inode, bytes, mode, and daemon state unchanged.
- Wrong, ambiguous, or name-mismatched SHA aborts before extraction, backup, replacement, or restart; the fixture includes the workflow's current `dist/NAME` checksum spelling.
- Traversal, absolute path, link/device, duplicate, extra, oversized, and missing-binary archives all refuse.
- Any xattr, signing, signature-verify, executable-bit, or staged smoke failure leaves the installed pair byte-for-byte unchanged.

### W9a2 — transactional pair commit, drain, health, rollback

Wire the manual `haider update` arm and implement update lock/marker/recovery, two backups, daemon-then-CLI rename, installed-pair verification, authenticated one-signal drain, child-retaining restart, exact Welcome-version health check, rollback, and old-daemon restart.

Minimum laws:

- Fault-inject every boundary—backup 1/2, daemon rename, CLI rename, directory fsync, post-swap verify—and require the exact old pair to be restored for every pre-restart failure.
- A restart spy first reads both live paths as the verified new pair. It is never invoked after only one swap or on failed post-swap verification.
- SHA mismatch and partial download cannot reach the commit interface by construction.
- Running `haider update` survives replacement of its own path; a running old daemon continues on its old inode until the one graceful signal.
- Real-daemon test observes matching drain, lock release, a newly spawned sibling, increased generation, and `Ready` Welcome at the target daemon version. The no-daemon case does not spawn one.
- Failed new-daemon health stops that child, restores both old binaries, and restarts the old version. Restart timeout sends no second signal and retains recoverable backups/marker.
- Crash-recovery fixtures for each marker phase either complete the target pair or restore the old pair; never accept a mixed pair.

### W9b1 — reusable daemon-backed headless transaction and permissions

Move create → Control attach → submit → cursor stream/reconnect → terminal reduction into `haider-client`-level reusable code, migrate the existing `run`, add timeout/cancellation, and add the optional durable per-session policy override plus feature bit.

Minimum laws:

- Submit cannot precede successful Control attach; events arriving before submit response are buffered/correlated correctly.
- Duplicates are ignored, a gap applies nothing and reattaches from the last complete sequence, and a deliberately saturated event channel loses no durable output.
- Done, Errored+adjacent `RunFailed`, and Cancelled map to distinct typed outcomes; no nonterminal parked state ends the runner.
- Timeout sends cancel, drains through a bounded terminal grace, and exits as timeout; disconnect without cancellation cannot be reported as success.
- A default write/exec ask selects typed `RejectOnce`, emits the denial notice/result, and continues to the eventual terminal. It never hangs and never selects by option position.
- `--allow-writes` and `--allow-exec` are present in durable session metadata, require the advertised feature, journal ordinary policy Allow for the relevant classes, and suppress those asks. Without a flag the W8a Ask default remains intact.
- Non-permission InputRequired and EffectOutcomeUnknown are never guessed; the command cancels/fails with the typed blocking reason.

### W9b2 — CLI surface, output laws, and exit codes

Extend the current manual parser to `haider run <prompt> [--output print|json|jsonl] [--timeout <duration>] [--allow-writes] [--allow-exec]`, defaulting to print. Keep provider/model selection if still part of the public contract, but route all modes through the same daemon-backed runner. Do not retain the old SQLite/`HarnessActor` alternate authority.

Proposed stable exit mapping:

| Exit | Meaning |
|---:|---|
| 0 | Correlated terminal Done, including a turn that handled a denied tool and still completed. |
| 2 | CLI usage/flag error. |
| 65 | Terminal Errored with `ProviderError` or `ProviderTimeout`; preserve the existing provider-error precedent ([crates/haider-cli/src/main.rs:588](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:588)). |
| 69 | Daemon unavailable/startup timeout; consistent with current front-door classification ([main.rs:286](/Users/rizzist/haider-run/haider-agent/crates/haider-cli/src/main.rs:286)). |
| 70 | Internal/unclassifiable software failure or impossible nonterminal end. |
| 74 | Output/transport I/O failure. Treat stdout BrokenPipe deliberately rather than panic. |
| 76 | Protocol, profile, required-feature, or update health/version mismatch. |
| 77 | Terminal permission/input-required class failure when the run cannot continue; a merely denied tool that later reaches Done remains 0. |
| 124 | W9 wall-clock timeout, even when its cancellation produces terminal Cancelled. |
| 130 | User/signal cancellation. Preserve the existing cancellation law. |

Minimum laws:

- Table-driven tests cover every terminal/outcome/error-code mapping, including denied-tool-then-Done, `RunFailed.code`, user cancellation, timeout-induced cancellation, and pre-acceptance RPC error.
- `print` has exact stdout/stderr separation and one trailing LF. It is byte-identical under redirected stdin/stdout/stderr and with no TTY/`TERM`; no PTY is required.
- `json` has a golden `haider.run.v1` schema for success/error/cancel/timeout/input-required. Additive-field compatibility and absence/null rules are pinned.
- `jsonl` has one compact valid v1 `RawEnvelope` per LF, monotonically reduced sequences, a correlated terminal line, and no loss under a slow pipe. Existing JSONL fixtures remain migration laws.
- Timeout parsing has finite/nonzero bounds; timeout cancels exactly once and never prints a success object.
- Permission-policy flags are rejected if the daemon lacks `session_permission_overrides_v1`, rather than silently degrading to prompt-and-deny.

## Risks

- **Release discovery is currently self-contradictory with `/latest`.** All v0.0.x releases are GitHub prereleases, so the latest-full-release endpoint cannot implement this updater.
- **Repository access is unspecified.** The README calls the repository private ([README.md:7](/Users/rizzist/haider-run/haider-agent/README.md:7)). Unauthenticated Releases API/downloads will fail unless it becomes public or W9 defines a non-leaking token source, scopes, redirects, and redaction policy.
- **Checksum is not publisher authentication.** The archive and `.sha256` share one GitHub release/account trust domain. It detects corruption but not compromise of the publisher/release path. Signed release manifests or an embedded public key are a later hardening step.
- **Pair commit is not power-fail atomic.** Without a durable marker/recovery or versioned-directory indirection, power loss can leave new `haiderd` beside old `haider`.
- **Install layout may not be self-owned.** Homebrew/Nix/package-manager, symlinked, read-only, or differently owned installs must be refused or delegated; overwriting them would fight their authority.
- **Only the resolved profile daemon is visible.** Other `HAIDER_PROFILE_DIR` daemons may continue executing old mapped code after installed files change. W9 should explicitly promise current-profile-only restart unless it adds a trusted daemon registry.
- **Graceful update can cancel live work.** Drain parks recoverable checkpoints but cancels ordinary active turns; update should say so, refuse when busy, or add a daemon occupancy summary.
- **A semantic version is informational today.** Normal connections permit client/daemon package skew. Update health must add its own exact target check and must not infer it from successful protocol negotiation.
- **The old `run` is a second authority.** Leaving `--jsonl` on the in-process SQLite actor while adding print/json over RPC creates divergent session, policy, reconnect, and error behavior.
- **Headless approval is observable behavior.** Default denial may change the model's continuation and a denied tool may still end Done. Machine outputs must expose the denial instead of equating “exit 0” with “every requested effect ran.”
- **Timeout/disconnect can orphan work.** Cancellation must be durable and correlated; dropping the client is not cancellation.
- **Raw JSONL can contain sensitive content.** Prompts, tool arguments, command output, and error details may appear in envelopes. Document that JSONL is an audit stream and avoid duplicating it into diagnostic logs.
- **Durable one-shot sessions accumulate.** W9 should state that each invocation creates a durable session and return its ID in machine output, or add an explicit retention/ephemeral-session design; silent cleanup would violate journal expectations.
