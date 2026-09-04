# runperm evidence — autonomous runs can build

Date: 2026-09-05  
Lane: `lane-970-runperm` from `origin/wave-970`  
Scope: autonomous permission resolution, explicit read-only denial, and
non-interactive entrypoint audit

## Claim audit

The reported root cause was substantively correct, with drifted line numbers
and one overstatement:

- The three old defaults were present at `crates/haider-cli/src/run.rs:109-111`
  in this worktree, not the cited `:100-108`; parsing was at `:226-233`, not
  `:221-229`.
- `haider-client` creates headless sessions with durable
  `SessionInteractionModeV1::Autonomous` at the then-current
  `crates/haider-client/src/headless.rs:2708`.
- Filesystem mutation, process execution, screen/mobile, network, task-kill,
  remote execution, and peer-send registry defaults were `Ask`. The worker
  installed a residual-Ask denial at the then-current
  `crates/haider-daemon/src/worker.rs:14265-14271`. The reusable headless
  reducer independently answered a leaked permission menu with `RejectOnce`.
  Together these made unflagged autonomous writes and process execution
  impossible.
- “Success ceiling was 0” is true for benchmark cases requiring a write or
  test execution, not for every possible read-only/chat workload. The observed
  external score was nevertheless 0/20.
- The supplied external log compared the installed OpenCode 1.17.20 behavior;
  its cited upstream source was already 1.18.9. The relevant comparison—its
  build agent's wildcard permission default was allow—remained correct.

## Implemented rule

`Autonomous` is now the authority. `InteractionGate::EffectBrokerAsk` and
`MobileOrDeviceGrant` resolve to `AutoApprove`. Worker policy promotes all
registered Ask defaults to ordinary `Allow`, and the broker also has a
residual-Ask Allow fallback so a future/unlisted effect cannot park or deny.
Hard deny and explicit deny are evaluated before that fallback.

The old residual-Ask denial field and `deny_unresolved_asks` API were removed.
The reusable headless reducer, CLI legacy replay reducer, and store recovery
reducer now select the enumerated `AllowOnce` option if an older producer still
publishes a Haider permission menu. None synthesizes a denial from an Ask.

The CLI's public compatibility projection initializes
`allow_writes=true`, `allow_exec=true`, and `auto_allow=true`, so its durable
pin also states the shipped default. Those booleans are not the implementation
authority: the daemon's interaction-mode rule independently promotes every
Ask class, including classes not named by the legacy flags. The flags remain
accepted no-ops for automation compatibility.

## What can still refuse

There are two permission-policy refusal authorities:

1. An explicit user deny. `--read-only` durably sets `read_only=true`, which
   wins over all allow fields for filesystem mutation and local/remote
   process, Git, desktop-control, and peer-message effects that could write the workspace.
   It also blocks Loom registry mutation before it can enqueue an unbrokered
   typed-agent installer process, and suppresses matching automatic hooks.
   A direct filesystem write uses the exact reason `write denied: run is
   --read-only`; indirect routes use equally explicit class-specific reasons.
   A read-only client requires both `session_permission_overrides_v1` and the
   dedicated `session_read_only_v1` feature before session creation, so an
   older daemon cannot silently ignore the restriction while accepting the
   legacy allow fields.
   Broker Deny is journaled; all generic
   and specialized dispatch paths convert ordinary permission denies to typed
   rejected tool results preserving the rule reason. The headless denial
   ledger exposes that same reason. Read-only additionally latches a terminal
   `PermissionDenied`. The latch becomes terminal-eligible only after the next
   provider request carrying the durable tool result completes its stream-open
   attempt. On the normal response path the model therefore consumes the typed
   denial before any non-cancel terminal becomes Errored with the same reason;
   on a transport-open error the request was issued/open-attempted, without
   claiming remote receipt. Cancellation retains priority. The CLI end-to-end
   regression requires the second provider response, including the text
   `write was refused`, before checking the named terminal cause.
2. Provider lockdown. Its hard-deny list remains above user/session allows and
   the autonomous fallback. Existing `RefusedByLockdown` journaling and typed
   results remain intact.

Workspace containment is not a permission-policy default and was not relaxed:
canonical-root traversal, absolute escape, and symlink escape remain typed
`WorkspaceBoundary` refusals. Likewise an unavailable OS TCC grant is an
external prerequisite, not a Haider Ask decision; autonomous mode does not
fabricate an operating-system grant.

## Ask-shaped route inventory

All current model-effect Ask routes pass through the same effective policy.
`tools.inventory` continues to expose the canonical interactive registry
defaults, so an `Ask` in inventory is not an unresolved autonomous decision:

| Route/family | Autonomous behavior |
|---|---|
| `fs_write`, `fs_edit`, mutating `fs_path` | ordinary `Authorized(Allow)`, then workspace-contained dispatch |
| local `process_exec`, background process start, `task_kill` | ordinary `Authorized(Allow)`; process supervision unchanged |
| `web_fetch` / `Network { host }` | ordinary `Authorized(Allow)`; URL/redirect and child API-scope fences unchanged |
| `ssh_shell` / `RemoteExecution` | ordinary `Authorized(Allow)`; saved-profile scope and vault prerequisites unchanged |
| `computer` screen observation/control | Haider Ask becomes `Allow`; external OS permission preflight remains honest |
| mobile observation/control/SMS | Haider Ask becomes `Allow`; activation/transport availability remains required |
| command-backed monitor registration | ordinary `Authorized(Allow)` before runner installation |
| `spawn_subagent` and `peer_send` | ordinary `Allow`; grant/peer existence and lockdown ceilings remain separate |

Direct `AuthorizationVerdict::Deny` branches in SSH, lockdown-sandbox write,
monitor command, subagent spawn, peer send, computer/mobile, web, and the
general filesystem/process dispatcher were audited. Ordinary explicit denies
are model-readable rather than escaping as an untyped provider failure.

## Other non-interactive entrypoints

- `haider run`, `haider run --start`, the reusable headless API, detached
  recovery, status, and replay all use or reconstruct the same durable
  Autonomous metadata. Store and CLI recovery no longer expect RejectOnce.
- Delegated children remain `Interactive` so an explicit `request_input` can be
  projected to and answered by the parent. An Autonomous parent instead
  projects permission-only `auto_allow=true` into the child, and its explicit
  read-only restriction is inherited. Thus a child cannot introduce an
  unanswerable permission menu, while the child's declared grant remains the
  hard tool/effect ceiling. Workflow execution uses these same child/session
  and worker-policy paths.
- Background task and monitor effects are brokered before process start; the
  autonomous monitor regression test now requires `Authorized(Allow)` and
  `Dispatched` before its marker appears.
- Hook trust is a digest-pinned supply-chain integrity policy, not an
  effect-broker Ask. An untrusted or edited hook does not open a headless
  question and emits a durable `HookNotice`; `--trust-hooks` remains an
  explicit trust choice. Under `--read-only`, matching automatic hooks are not
  dispatched even if otherwise trusted, and a typed `HookNotice` names
  `hook execution denied: run is --read-only`. Decision hooks can answer an
  already committed interactive permission menu; Autonomous policy emits no
  such menu, and recovery uses first-committed-wins CAS so an explicit
  `RejectOnce` cannot be overwritten.
- `loom_register` is plan-gated rather than broker-gated, but agent-type
  registration can enqueue a package-manager installer. Read-only checks the
  route before either registry CAS or job creation and returns the typed reason
  `registry mutation denied: run is --read-only`.
- `request_input` without a declared default, missing credentials, unknown
  post-crash effect outcome, repeated unfinished-workflow confirmation, and OS
  permission absence are typed inability/recovery states rather than Haider
  permission Ask decisions. They were not converted into fabricated approvals.

## Verification

Required environment for builds and tests:

```text
RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
```

Daemon-backed tests additionally use `HAIDER_TEST_SIBLINGS_PREBUILT=1` after
building `haider` and `haiderd`. Final debug sibling sizes were 108,837,824
bytes and 197,348,800 bytes respectively, both above the 10 MiB daemon-spawn
sentinel. `df -m /` was checked before every build/test invocation and never
approached the 700 MiB stop threshold.

Completed while implementing:

- `cargo check --workspace --all-targets` — passed.
- `cargo test -p haider-cli --test cli_tests run_write_and_exec_permission_flags_journal_ordinary_allow -- --nocapture` — passed; unflagged write and exec both dispatched with ordinary Allow.
- `cargo test -p haider-cli --test cli_tests run_read_only_denial_is_typed_and_terminal -- --nocapture` — passed; the second provider response consumed the typed denial, then the process exited 77 with the exact `permission_denied` terminal.
- The dedicated old-daemon compatibility regression passed: a read-only
  client requires `session_read_only_v1` before any session mutation even when
  the daemon advertises the older permission-overrides feature.
- `cargo test -p haider-client --test headless_run_tests permission_ -- --nocapture` — passed, 4 tests; leaked menus select AllowOnce, reconnect retry preserves the decision, and a competing RejectOnce is a typed conflict.
- Named daemon/tools tests passed for autonomous registry-wide Ask promotion,
  exact one-shot rejection priority, read-only class overrides, recovery-menu
  auto-resolution, durable read-only terminal rehydration, hooks, monitor
  dispatch, delegation, outside-workspace traversal/symlink refusal, and both
  lockdown pins.
- Final `cargo test --workspace --locked` — passed after the compatibility and
  terminal-delivery fixes, including all unit,
  integration, golden, and doc-test targets; existing explicitly ignored
  live/manual tests remained ignored and no test was weakened or newly
  ignored.
- Final scoped all-target Clippy for protocol, tools, core, store, client,
  daemon, CLI, daemond, RPC, and TUI with `--locked -- -D warnings` — passed.
- `cargo fmt --all -- --check` and `git diff --check` — passed.
- `cargo run --locked -p xtask -- test-count` — `4757 tests (baseline 4757)
  — ok`; the tracked baseline increased from 4748 to 4757.
- The exact post-wording `run_help_states_autonomous_default_and_explicit_denies`
  regression passed with prebuilt siblings.

macOS behavior was executed. Linux and Windows cfg-specific behavior is by
source inspection; the permission rule is cfg-neutral and platform boundary,
process, and wire tests compiled in the workspace all-target gates available
on this host.

## CI error registry walk

| Registry class | Result |
| --- | --- |
| #1-#19 | Checked. The protocol field is additive with serde defaults, exhaustive interaction-policy matches were updated, no dependency or lint allowance was added, and scoped all-target deny-warnings Clippy is clean. |
| #20/#21/#48/#54 | Checked. Nine named regressions were added (4748 to 4757), no existing test was removed, weakened, newly ignored, or platform-gated, and every Cargo test used the required 8 MiB stack. |
| #22-#44 | Checked. Store recovery now chooses the enumerated `AllowOnce` option with CAS conflict preservation; provider, release, dependency, process-supervision, and socket contracts are otherwise unchanged. |
| #45/#77 | Checked. No unsafe code or lint suppression was added. |
| #46-#63 | Checked. Workspace containment, path canonicalization, exact effect arguments, grant ceilings, and journal settlement remain in force; named traversal and symlink tests pass. |
| #64/#67/#71/#72/#74 | Checked. Prebuilt siblings were used for daemon-backed tests, final `haiderd` is 197,348,800 bytes, discovery stayed disabled, and the mandated build environment was applied. |
| #65/#68-#78 | Fixed/checked. Permission and lockdown refusals are typed tool results; read-only additionally becomes the durable exact-run `permission_denied` terminal cause only after a provider request carrying the result completes its open attempt. Normal-path coverage proves the second model response. Replay and reconnect restore the first committed decision/cause, and `session_read_only_v1` prevents an older daemon from ignoring the deny. |
| #79-#93 | Checked. Process ownership, output draining, PID identity, publication ordering, sparse-file, line-ending, and sampling behavior are unchanged. Hook read-only suppression emits a durable notice instead of silently dispatching or skipping. |
| #94 | Checked. No product deadline, timeout, or arbitrary observation sleep was added. The hook absence regression awaits the existing outbox-drain barrier rather than sleeping; existing harness bounds are unchanged. |
| #95 | Checked. No external-state wait was introduced while a negotiated connection is open; keepalive behavior is unchanged. |
| #96-#98 | Checked. Provider terminal reserve and route attribution are unchanged. The read-only cause is pending after durable tool settlement, becomes terminal-eligible only after the next provider stream-open attempt, and keeps cancellation priority. |

No new CI error class was discovered.

## Independent verification

Two independent completed-diff reviews returned **SHIP** with no P0/P1/P2
blockers:

- The entrypoint audit traced run/start/recovery/replay, delegated workflows,
  background routes, hooks, Loom registration, and OS/credential boundaries.
  It confirmed autonomous registered and residual Ask resolution, explicit
  deny/lockdown precedence, workspace containment, typed terminal delivery,
  compatibility fencing, and evidence reconciliation.
- The permission verifier independently traced the CLI defaults through the
  durable session pin and worker/broker authorization, checked every
  read-only class and inherited child restriction, verified that an older
  daemon is rejected before session mutation, and confirmed that terminal
  eligibility follows the provider request carrying the durable denial.
