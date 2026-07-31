# W8 permissions/tools research

Read-only audit of branch `w8-perms` at `v0.0.35`. No Rust or JavaScript code was changed.

The central finding is that the proposed W8a daemon foundation is substantially present already. Production LIVE turns advertise eight tools, including a provider tool named `exec`; `FsWrite` and `ProcessExec` already stop at a durable, first-committed-wins approval menu; session grants are reconstructed from the journal; and dispatched brokered operations emit the four-phase effect lifecycle (blocked attempts stop after authorization). W8a should therefore consolidate and harden that path, expose it under the intended `process_exec` vocabulary, and add a daemon entrypoint for user-typed shell commands. It must not create a second permission authority.

A second important finding is that neither the simulator nor the Rust TUI has a literal `!` escape today. Both recognize six **bare** demo-only VFS commands. A LIVE `!cmd` should be parsed by the client but executed by the session's daemon through the existing `process_exec_user` backend; client-side process spawning would run on the wrong machine for remote sessions and bypass the effect journal, containment, cancellation, and receipts.

## Q1 current state

### Tool inventory and LIVE exposure

The executable public surface of `haider-tools` is visible in its re-exports ([lib.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/lib.rs:17)). Supporting types such as `EffectBroker`, `PermissionPolicy`, `ChangeLedger`, CAS sinks, and result bounds are infrastructure rather than independently callable tools.

| Crate capability | Normalized effect | LIVE provider name today | Current policy / route |
|---|---|---|---|
| `FsRead` | `FsRead` | `fs_read` | Advertised and dispatched; allow by default. The operation class and canonical path binding are at [filesystem.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:94), and daemon dispatch is at [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3155). |
| `FsList` | `FsRead` | `fs_list` | Advertised and dispatched; allow by default ([filesystem.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:124), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3167)). |
| `FsSearch` | `FsRead` | `fs_search` | Advertised and dispatched; allow by default ([filesystem.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:154), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3179)). |
| `FsWrite` | `FsWrite` | `fs_write` | Advertised and ask by default; applied writes are attributed to the turn's change ledger ([filesystem.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:194), [filesystem.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:411), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3211)). |
| `FsPatch` | `FsWrite` | `fs_patch` | Advertised and ask by default; exact-preimage patch with bounded diff preview ([filesystem.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:247), [filesystem.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:481), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3224)). |
| `ProcessExec` | `ProcessExec` | **`exec`**, not `process_exec` | Advertised and ask by default. `exec` is converted to `ProcessExec` and run through `EffectBroker::process_exec` ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3192)). |
| `ProcessControl` | `ProcessExec` | Not exposed | Library API for signal, stdin write, and kill. Each control is a second brokered effect bound to the original live process ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:397), [process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:535), [process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:793)). There is no provider definition or dispatcher arm. |
| `RequestInput` | None | `request_input` | Advertised, but deliberately outside the effect broker because asking is not a side effect. The actor owns the blocking menu round trip ([request_input.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/request_input.rs:1), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2191)). |
| `SpawnSubagent` | `AgentSpawn` | `spawn_subagent` | Advertised and allow by default; deferred after the brokered establishment effect ([spawn_subagent.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/spawn_subagent.rs:41), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3079)). |
| `ShellSession` / `UserProcessExec` | `ProcessExec` for non-builtins | Not exposed | Composer parser/state library. It recognizes `!`, handles `cd` and `env-view` without spawning, and returns an unforgeable user-origin `UserProcessExec` for everything else ([shell.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/shell.rs:15), [shell.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/shell.rs:72), [shell.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/shell.rs:111)). No production TUI, daemon RPC, or worker constructs it today; repository-wide uses outside the crate are tests only.

The production factory advertises exactly:

```text
request_input
fs_read
fs_list
fs_search
fs_write
fs_patch
exec
spawn_subagent
```

Evidence: [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2974). `DaemonDependencies::default` installs this `BrokerToolFactory` in production ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:425)). Each turn copies the factory definitions into `HarnessConfig` ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2487), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2528)), and core copies that list into every provider `TurnRequest` ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1059)). The factory contract already states that every advertised definition must be executable and that a missing dispatcher must advertise no general tools beyond actor-owned `request_input` ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:351)).

There are two naming mismatches W8 should close:

- The library, protocol effect class, simulator, demo scripts, and `/tools` copy say `process_exec`; LIVE providers see `exec` ([effect.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/effect.rs:9), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3554), [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1323), [app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:5924)).
- The frozen `ToolManifest` carries effect classes and dispatch mode, but production definitions are plain provider `ToolDefinition`s. Only `spawn_subagent` currently owns a crate-level manifest ([tool.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/tool.rs:8), [spawn_subagent.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/spawn_subagent.rs:57)). Consequently, `/tools` has no authoritative daemon inventory to render.

### Permission/approval seam: a gate already exists

There **is** an approval gate between a provider's mutating tool call and execution.

The daemon-side flow today is:

1. A provider streams `ToolCallStart`, argument deltas, and `ToolCallEnd`; core creates and updates a `TurnItem::ToolCall`, then invokes the dispatcher ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1394), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2180)).
2. The production dispatcher constructs the typed operation and calls only its brokered executor ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3053), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3154)).
3. `EffectBroker::normalize` canonicalizes arguments, BLAKE3-digests them, and durably appends `Intent` before authorization ([broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:883)). `authorize` then appends exactly one `Authorized` verdict ([broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:913)).
4. `EffectBroker::begin` appends `Dispatched` only for `Allow`/`PreAuthorized`; `Ask` becomes `AuthorizationRequired`, and `Deny` becomes `PermissionDenied` before the side effect ([broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1124)).
5. The dispatcher turns `AuthorizationRequired` into `ToolDispatchResult::ApprovalRequired(Menu)` ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3253)).
6. Core durably commits `MenuOpened`, parks, waits for an answer, asks the dispatcher to apply it, restores `RunningTool`, and retries the exact arguments as a fresh broker effect ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2275)). A blocked effect therefore never later grows a second `Authorized` phase.
7. The permission waiter rejects raw in-process `AnswerMenu` commands. Only the daemon's already-committed `MenuAnswered` envelope is accepted as the authorization credential ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2347), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2398), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2420)).
8. On the wire, answering requires Control capability and a live control attachment, plus the exact opening sequence and worker generation ([rpc.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1947), [rpc.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1982)). The hub serializes the store CAS with appends, publishes the committed envelope, then wakes the harness ([actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/actor.rs:345)).
9. The dispatcher applies that committed decision to the broker policy ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3300)). Denial is returned to the provider as a bounded typed tool result rather than silently stopping the turn ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3424)).

The production defaults are already effect-classed:

- `FsRead`: allow.
- `FsWrite`: ask.
- `ProcessExec`: ask.
- `AgentSpawn`: allow.
- Any unlisted future class defaults to ask.

Evidence: [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3007), [broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:205), [broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:323). Deny wins over always/session/class allows.

The generated menu already presents an effect preview and three server-enumerated decisions: `approve_once`, `approve_for_session`, and `deny` ([broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1270)). `RejectAlways` exists in the frozen decision enum and policy engine, but the production menu does not offer it ([menu.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/menu.rs:73), [broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1033)).

Session remembering is also implemented:

- For non-process effects, “approve for this session” is class-wide.
- For `ProcessExec`, class-wide grants are forbidden. The scope is the digest of exact shell program bytes, canonical cwd, and sorted environment-allowlist names; call id is deliberately excluded.
- Dispatcher creation scans durable `Effect::Intent`, `Effect::Authorized(Ask)`, `MenuOpened`, and `MenuAnswered` events to reconstruct grants and pending-menu bindings, so a remembered grant survives daemon restart but remains attached to that session.

Evidence: [broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:157), [broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:278), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2997), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3347).

One live-state mismatch remains: the protocol and TUI have a distinct `RunState::PermissionRequired`, but the general-tool approval loop currently parks in `RunState::InputRequired` ([state.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/state.rs:64), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2324)). Recovery is likewise pinned to `InputRequired` ([turn_recovery.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/turn_recovery.rs:163)). Thus a LIVE permission menu renders correctly as a menu, but its status badge says `INPUT_REQUIRED`, not the simulator's `PERMISSION_REQUIRED` vocabulary.

### `process_exec`: approval, containment, and residual authority

Current model-initiated process approval is stronger than a tool-name allow:

- The preview shows the exact JSON-escaped command and cwd; prepared limits are added before approval ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:150), [process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:235)).
- The approved command is passed unchanged as the one `/bin/sh -c` argument ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:649)).
- Cwd must canonicalize beneath the workspace, is opened component-by-component with `O_NOFOLLOW`, and is revalidated by inode identity immediately before spawn ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:84), [process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:168), [process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:1520)).
- The environment is cleared and only named allowlisted variables are restored ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:649)). LIVE provider wiring never supplies an env allowlist, so it is empty today ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3192)).
- Each child gets a process group. Default bounds are 8 KiB inline output, 1 MiB total output, 60 seconds wall time, and a 2-second TERM-to-KILL grace ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:246), [process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:662)). Dropping the handle requests supervised cancellation, while the broker-owned finalizer keeps terminal journaling alive ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:369)).
- Output is emitted as durable base64 byte deltas and larger transcripts can spill to CAS ([worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3635), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3685)).

This is **not a full operating-system sandbox**. It confines and revalidates the starting cwd, clears the environment, bounds time/output, and supervises a process group. It does not install a filesystem namespace, syscall filter, privilege boundary, or network policy. An approved shell command can still use absolute paths and network access available to the daemon OS user. The module explicitly records that descendants which create a new session/process group can escape `killpg`, with stronger containment deferred ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:9)). Process-created file changes also bypass the `FsWrite` change ledger, which is wired only around `fs_write`/`fs_patch` ([filesystem.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:411)). Approval is an authorization boundary, not containment.

Two presentation gaps matter for W8b:

- Provider process output arrives as `CommandOutput` deltas attached to the provider's `ToolCall` item. The TUI projection retains those bytes, but the `ToolCall` renderer does not display them; only `CommandExecution` renders output ([projection.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/projection.rs:487), [render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:4379), [render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:4422)).
- A nonzero child exit is encoded as `ProcessResult.status = Failed`, but the dispatcher returns a successful `BoundedResult`, so core completes the outer `ToolCall` with `ToolStatus::Completed` ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:1288), [worker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:3635), [actor.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:2246)). That may be defensible as “the tool returned a result,” but it disagrees with the simulator's red failed command row and hides the actionable status from the current TUI.

## Q2 sim UX contract

### There is no literal simulator `!` escape

The simulator's exact submit order is slash command, Aura input, numeric menu answer, bare shell builtin, subagent steering, new-session creation, then normal turn handling ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1966)). It never tests `startsWith("!")` and never strips `!`.

Therefore:

- `ls` is a simulator shell command.
- `!ls` is ordinary user/model text.
- `! cargo test` is ordinary user/model text.

The guide confirms the current UX by saying “the prompt doubles as a shell” with bare `ls`, `cd ..`, `mkdir experiments`, and `pwd` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3842)). The Rust TUI ports the same six bare names and refuses them in LIVE mode rather than painting the fake VFS as real state ([app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:74), [app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:3089)).

### Exact ShellRow/VFS behavior

The simulator shell is a local in-memory VFS, not a subprocess:

- Seed paths and the exact command allowlist are at [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:417): `ls`, `dir`, `pwd`, `cd`, `mkdir`, `touch`.
- `resolvePath` is lexical only. Empty/`~` maps to `~/dev`; `.` is discarded; `..` pops with a one-segment floor ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:429)).
- `runShell` whitespace-splits, uses only the first argument, gives unknown directories a fabricated `src/ README.md` listing, never validates `cd` existence, and returns a VFS append for `mkdir`/`touch` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:444)). There is no stdout/stderr distinction, exit status, streaming, timeout, approval, sandbox, or receipt.
- In a session, the result updates the session's display cwd for `cd` and appends `{kind:"shell", cmd, out}` to the transcript. At the launcher it updates launcher cwd and replaces the one launcher shell block ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1993), [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3302)). It is disabled on the subagent screen.
- `ShellRow` is exactly `$ <cmd>` plus one pre-wrapped output body ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3910), [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:4552)). The Rust TUI has the corresponding envelope-free demo transcript row ([projection.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/projection.rs:569), [render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:4087)).

Turning these bare fake commands into real host mutations would be an unsafe semantic jump. LIVE should require the explicit `!` cue, while demo parity may keep the six bare VFS commands.

### Permission/menu vocabulary

The simulator's menu primitive is `{id, kind, title, body, options, blocking}`. `askMenu` stores a resolver keyed by id; `answerMenu` removes the menu and resolves the selected index; a resolver lost after reload produces a stale-menu note ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:838)).

The scripted production example uses this exact vocabulary:

- Badge state: `PERMISSION` → `? PERMISSION_REQUIRED` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2813)).
- Title: `process_exec requests approval`.
- Body: exact command, `effect class: externally transactional · db writes`, and the exact rule created by “always.”
- Options: `allow once`, `allow for this session — adds the rule above`, and `deny — tell the agent why`.
- On denial, the sim emits a note and continues the model without running the tool. On allow-for-session it emits a session-rule note, then runs the tool ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1319)).

This is scripted UX, not a generic classifier. The simulator has no reusable `read`/`write`/`exec` effect table, no per-class defaults, no digest binding, and no durable receipt implementation. Its only explicit effect-class wording is “externally transactional.” Likewise, “deny — tell the agent why” has no reason-entry step; the reason is a fabricated note.

The interaction contract is nevertheless useful and already matched by the Rust TUI:

- A session menu replaces the composer; digits, arrows, Enter, and click answer it, while Esc is swallowed ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2431), [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3053)).
- The help copy says the same card can be answered by id over `menu.answer` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:587)).
- Menu glyphs are permission `?`, recovery `⌁`, exhausted `⟳`, voice `◉`, tools `⚒` ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3057)).
- Tool rows use `running|ok|err`, rendered as `◐|✓|✗` with `running…` metadata ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1243), [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3901)). This maps naturally to frozen `ToolStatus::{InProgress,Completed,Failed}`.

The simulator has two menu inconsistencies that should not become LIVE contract: `blocking:false` voice/tools menus still replace the composer because layout checks only whether any menu exists, and keyboard rendering uses the first menu while submit's digit fallback selects the last ([tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1880), [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1982), [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2434), [tui.js](/Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3056)). The Rust TUI's menu path is the more reliable contract.

### What the Rust TUI already has

The Rust TUI is already close to W8b's approval rendering needs:

- The generic menu renderer paints server-supplied title/body/options, windows options under height pressure, and never invents decisions ([render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:3203)). Patch previews receive diff-aware `+`/`-` coloring ([render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:3149)).
- `MenuOption.detail` and `DecisionKind` are not rendered; only `label` is visible ([render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:3264), [plain.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/plain.rs:87)). W8a must keep the exact grant rule in `Menu.body` and complete decision wording in `label`, as it does now.
- Blocking-menu key handling swallows Esc, wraps arrows, supports digits and Enter, and emits both stable option key and index with `AnswerVia::Tui`. It does not optimistically close the menu ([app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:4838), [app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:4876)).
- The TUI records committed opening coordinates and retries one durable answer command id until a committed `MenuAnswered`/`MenuClosed` retires it ([live.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/live.rs:1797), [live.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/live.rs:2203)).
- `ToolStatus` glyphs already cover pending, in-progress, completed, failed, and cancelled ([plain.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/plain.rs:10)). Rich rendering exists for both `ToolCall` and `CommandExecution` ([render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:4379)).

`/tools` is not live. It exists in the command registry, but help labels it demo-only ([commands.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/commands.rs:79), [commands.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/commands.rs:297)). LIVE explicitly refuses it because a locally minted card has no committed opening coordinates and can never be safely answered ([app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:4459)). Demo `/tools` claims “13, always on” and pretends to register three custom dispatch modes, but it is only a nonblocking local `Choice` plus display notes ([app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:5921), [app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:5060)). It is not inventory evidence and must not be copied into LIVE.

## Q3 live `!` design

### Decision

Map LIVE `!cmd` to **daemon-side execution through `EffectBroker::process_exec_user`**, not to a provider tool call and not to a subprocess spawned by the TUI.

The existing backend was designed for exactly this distinction:

- `ShellSession::submit` strips one leading `!`, rejects an empty command, handles `cd`/`env-view`, and creates `UserProcessExec` for other commands ([shell.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/shell.rs:111)).
- `UserProcessExec` cannot be publicly forged from a model-created `ProcessExec` because its operation and provenance fields are private ([shell.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/shell.rs:22)).
- `process_exec_user` bypasses the model-effect permission policy but still calls the same process backend ([process.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/process.rs:596)).
- The journal records `Authorized::PreAuthorized { source: UserTyped }`, then `Dispatched` and a terminal `Outcome` ([broker.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1067), [effect.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/effect.rs:39)). The explicit user command authorizes that one exact invocation; it must not create or consume a provider permission grant.

The split of responsibility should be:

```text
TUI composer
  └─ parse leading ! as a semantic user-shell action
       └─ receipt-backed daemon RPC at session + generation
            └─ session worker / effect broker
                 ├─ PreAuthorized(UserTyped)
                 ├─ process_exec_user with the same cwd checks and limits
                 ├─ CommandExecution started → CommandOutput* → completed
                 └─ terminal Effect::Outcome
```

“Local” must mean local to the **session daemon's workspace**, not local to the terminal process. This preserves remote-session meaning and keeps one authority for target cwd, effect receipts, CAS spill, cancellation, and recovery.

The minimum daemon entrypoint should be a durable, idempotent `shell.exec`-style command carrying `command_id`, `session_id`, `worker_generation`, exact command bytes, and an optional workspace-relative cwd. It should be serialized by the session worker. For the first W8 slice, reject or explicitly queue it while another run owns the session; do not create an unjournaled parallel side-effect lane. It should create no `UserMessage` and make zero provider requests.

`!cd` cannot be implemented by merely running `/bin/sh -c 'cd …'`, because child cwd changes do not persist. There are two honest choices:

1. W8 implements daemon-owned `ShellSession` state. `!cd` changes only the cwd of subsequent `!` commands after workspace-bound validation. It must not claim to retarget provider tools or the immutable session workspace root unless a separate durable metadata design is added.
2. W8 documents every `!cmd` as starting at the session workspace root and rejects `!cd` as unsupported.

The first choice best reuses the shipped library, but its restart/multi-client persistence must be specified. Client-only cwd state is not authoritative. The simulator's stronger wording—`cd` moves where the agent works—does not match today's immutable `SessionMetadataV1.cwd` ([session.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/session.rs:5)) and should not be promised by W8.

For presentation, direct shell execution should emit `TurnItem::CommandExecution`, not an envelope-free `TranscriptEntry::Shell`. The frozen item already carries command, `ToolStatus`, exit code, and byte-stream output deltas ([item.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:25), [item.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:92)). The existing rich/plain renderers then provide `$ command`, output, exit, truncation, and cancelled/failed glyphs without a new presentation protocol ([plain.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/plain.rs:134), [render.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/render.rs:4422)).

Modal ownership remains unchanged: a blocking menu replaces the composer, so `!` parsing is reachable only after the menu closes. The current TUI routes menu keys before composer handling and explicitly reports that the composer does not own input under a session menu ([app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:2790), [app.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/src/app.rs:3019)). `!` must never become a way around a pending provider approval.

## Proposed W8a/W8b split

### W8a — daemon authority, manifests, and receipts

Treat the current W4 broker/CAS implementation as the baseline and make these bounded changes:

1. **Canonical tool registry.** Define one daemon-owned registry whose entries include provider name, schema, normalized effect classes, and dispatch mode. It should be the source for both `TurnToolFactory::definitions()` and a read-only tool-inventory snapshot. Reuse frozen `ToolManifest`; do not let `/tools` maintain a second hardcoded inventory.
2. **Canonical process name.** Advertise `process_exec` to new provider turns. For recovery and in-flight history compatibility, continue accepting legacy `exec` at the dispatcher boundary for at least the migration window; do not advertise both unless provider duplicate-tool behavior is tested.
3. **One approval authority.** Retain `EffectBroker::begin → AuthorizationRequired → durable Menu CAS → resolve_permission → fresh exact retry`. Make every model-originated `FsWrite` and `ProcessExec` route pass it. Do not add TUI-side effect decisions.
4. **Explicit defaults.** Keep `FsRead=allow`, `FsWrite=ask`, `ProcessExec=ask`, `AgentSpawn=allow`, and explicitly default all future effect classes to ask. Defaults live in daemon policy, never client selection state.
5. **Permission state vocabulary.** Emit `RunState::PermissionRequired` for permission menus, while recovery accepts historical `InputRequired + MenuKind::Permission` checkpoints during migration.
6. **Remembered grants.** Reuse the current durable reconstruction. Keep process grants exact-command-shape; make the broader class scope of filesystem session grants conspicuous in the menu body. Do not let `!` modify this policy.
7. **Receipts, not a new audit DTO.** Continue emitting `Intent → Authorized → Dispatched → Outcome` plus the normal `ToolResult`/item lifecycle. Preserve `Unknown` on a dispatched-without-outcome crash. If `/tools` shows remembered grants, project them from these durable facts or a daemon snapshot derived from them.
8. **Direct user-shell backend.** Add the receipt-backed daemon command that invokes `process_exec_user` and emits `CommandExecution`. This is the backend half of W8b's `!`; it is daemon work even though the user-facing parser lands in W8b.
9. **Inventory read seam.** Expose the actual registered names/effects/defaults/session grants for W8b `/tools`. It is a read, not an answerable permission menu and not simulated custom-tool registration.

### W8b — TUI wiring

1. **Permission menus.** Reuse the existing generic menu renderer and answer outbox. Add only effect-class presentation tests and, if desired, render `MenuOption.detail`; never infer policy or invent options.
2. **Literal `!` escape.** In composer submit preprocessing, route a nonempty leading-`!` command to a semantic `AppRequest`, through `LiveDriver`, to the daemon command. Slash commands keep priority; ordinary bare text remains a model turn; the six bare VFS commands remain demo-only.
3. **Command projection.** Render committed `CommandExecution`/`CommandOutput` events and real `ToolStatus`; do not optimistically add a shell row. Cancellation, exit, truncation, and connection loss must come from committed daemon facts.
4. **`/tools` screen.** Replace the LIVE refusal and demo registration fiction with a read-only view of the daemon snapshot: provider-visible name, normalized effects, dispatch mode, default decision, and remembered session grants. A disconnected/stale snapshot is labeled, never fabricated. Custom registration is outside W8 unless the daemon actually implements it.
5. **Model process visibility.** Either teach the existing `ToolCall` row to show retained `CommandOutput` and inner process status for `process_exec`, or introduce a deliberate paired `CommandExecution` item daemon-side. Do not leave durable output invisible.

### Frozen protocol shapes to reuse

These are already serialized contracts; W8 should not create parallel permission or status vocabulary:

- **Envelope freeze:** protocol serialization is the artifact; changing existing fields requires a version bump, ADR, golden fixtures, and upcasting ([lib.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/lib.rs:1), [envelope.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/envelope.rs:34)).
- **Menus:** `Menu { id, kind, title, body, options, blocking, scope, origin, ttl_ms, timeout_option }`, `MenuKind::Permission`, server-enumerated `MenuOption`, and `DecisionKind::{AllowOnce,AllowAlways,RejectOnce,RejectAlways}` ([menu.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/menu.rs:11), [menu.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/menu.rs:32), [menu.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/menu.rs:60)).
- **Menu answers/events:** stable option key plus index, optional value, and `AnswerVia`; durable `MenuOpened`, `MenuAnswered`, `MenuClosed` events ([menu.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/menu.rs:94), [lib.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/lib.rs:50)).
- **Wire CAS receipt:** `MenuAnswer` carries `command_id`, session, menu, opening `request_seq`, opening `worker_generation`, option key/index, and optional input ([frame.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-rpc/src/frame.rs:1247)). The response's `resolution_seq` is convenience; the committed event is authority ([rpc.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:2056)).
- **Effect receipts:** normalized `EffectClass`, digest-bound `EffectIntent`, `AuthorizationVerdict::{Allow,PreAuthorized(UserTyped),Ask,Deny}`, `Dispatched`, and terminal `EffectOutcome` including `Unknown` ([effect.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/effect.rs:9), [effect.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/effect.rs:25), [effect.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/effect.rs:63)). There is no separate frozen `ToolReceipt` type; this event sequence plus item completion and `ToolResult` is the current durable receipt trail ([lib.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/lib.rs:64)).
- **Items/status/output:** `ToolCall`, `CommandExecution`, `ToolStatus::{Pending,InProgress,Completed,Failed,Cancelled}`, and byte-safe `CommandOutput { stream, chunk_b64 }` ([item.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:14), [item.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:70), [item.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/item.rs:92)).
- **Manifest/result:** `ToolManifest.effects`, `DispatchMode`, and bounded preview/artifact results ([tool.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/tool.rs:8), [tool.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/tool.rs:19), [tool.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-protocol/src/tool.rs:29)).

## Risks + laws

### Risks

1. **Second-authority risk.** W8a's headline duplicates shipped W4 behavior. A new approval service beside `EffectBroker` would split policy, grants, and crash recovery. Extend the existing path.
2. **“Sandbox” overclaim.** Cwd confinement and environment clearing do not stop absolute-path filesystem or network access. UI language should say “workspace cwd + bounded supervised process,” not imply kernel isolation.
3. **Tool-name migration.** Renaming `exec` to `process_exec` can break recovered checkpoints, provider tool history, cached prompts, and tests. Accept the legacy name at dispatch while advertising one canonical name.
4. **Grant breadth.** One non-process allow-for-session grant permits the whole effect class. In particular, one approved `FsWrite` permits later `fs_write` and `fs_patch` calls for the session. The current menu says so, but the risk is broader than the simulator's pattern-like rule.
5. **State migration.** Switching approval parking from `InputRequired` to `PermissionRequired` without dual-read recovery would terminalize old pending approvals on restart.
6. **Receipt secrecy.** Effect summaries, tool previews, `/tools`, and command receipts must not persist environment values, raw secrets, or unbounded command output. Process grant identity uses env names, not values.
7. **Direct-shell target ambiguity.** Client-side execution means the TUI host; daemon-side execution means the session host. Only the latter is coherent for remote sessions. UI must show target and cwd.
8. **User provenance confusion.** `!cmd` is directly preauthorized for one invocation; it must not look like a provider approval, satisfy a pending provider menu, or install a remembered grant.
9. **Cwd drift.** Simulator `cd` is client-persisted fake state; a real persistent cwd needs daemon authority and restart/multi-client semantics. Do not imply that a child-shell `cd` retargets the agent.
10. **Status/output loss.** Today model `exec` can show a green completed outer tool while its process result is failed, and streamed output is retained but not rendered. W8b needs an explicit semantic choice and tests.
11. **`/tools` fiction.** The demo card says “13, always on” and pretends registrations succeed. LIVE must show the daemon registry or clearly fail; it cannot mint a local answerable menu without committed coordinates.
12. **Process escape residual.** Descendants can escape the supervised process group, and process-created writes are not in the change ledger. Approval and receipts expose risk; they do not remove it.

### Minimum W8a laws

1. **Inventory equality:** the canonical advertised set equals the executable dispatcher set. `process_control`, shell builtins, and legacy aliases are not advertised unless deliberately supported.
2. **No pre-approval dispatch:** model `FsWrite`, `FsPatch`, and `ProcessExec` never cross their side-effect boundary before a committed, valid menu answer; raw actor answers and stale-generation answers fail closed.
3. **CAS uniqueness:** an N-way menu race commits exactly one answer; same-command retry returns the original resolution; losing commands cannot alter policy.
4. **Deny reaches the model:** deny executes nothing, produces a bounded permission-denied tool result, and allows the provider turn to continue.
5. **Allow-once exactness:** one answer authorizes exactly one fresh retry with the same effect class and canonical argument digest; a second call asks again.
6. **Session-grant scope:** non-process grants remain session/class scoped across daemon restart; process grants match only exact command bytes + canonical cwd + sorted env-name allowlist. Any difference asks again; class-wide process grants are rejected.
7. **Four-phase receipt:** every dispatched effect has exactly `Intent → Authorized(allow/preauthorized) → Dispatched → Outcome`; blocked effects have no `Dispatched`; crash after dispatch becomes `Unknown` and never auto-reruns.
8. **Process boundary:** exact approved bytes reach `/bin/sh -c`; cwd is revalidated beneath workspace immediately before spawn; environment is cleared; time/output bounds hold; cancellation supervises the group and terminalizes once.
9. **User-shell provenance:** direct `!` creates no provider request, emits `PreAuthorized(UserTyped)`, never changes provider grants, and cannot be forged through a provider tool call.
10. **Command receipt:** retrying the same direct-shell command id does not spawn twice; changing the body under the same id is rejected; response loss is recovered from committed effect/item facts.
11. **Historical recovery:** both old `InputRequired + Permission` and new `PermissionRequired` checkpoints resume the original menu CAS without redispatching the provider request or effect.

Strong existing coverage to preserve includes broker grant/fail-closed/order tests ([effect_broker_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/tests/effect_broker_tests.rs:217), [effect_broker_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/tests/effect_broker_tests.rs:372), [effect_broker_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/tests/effect_broker_tests.rs:553)), process digest/cwd/cancellation/user-preauthorization tests ([process_tools_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/tests/process_tools_tests.rs:599), [process_tools_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/tests/process_tools_tests.rs:756), [process_tools_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/tests/process_tools_tests.rs:974)), store menu race/replay tests ([menu_resolution_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-store/tests/menu_resolution_tests.rs:156), [menu_resolution_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-store/tests/menu_resolution_tests.rs:280)), and real UDS approval/restart tests ([live_turn_rpc_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:3704), [live_turn_rpc_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:4034), [live_turn_rpc_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:5159)). New W8a tests should target only the uncovered manifest/name, `PermissionRequired` migration, inventory read, and direct-shell RPC lanes.

### Minimum W8b laws

1. **Server-enumerated menu:** TUI renders exactly the committed title/body/labels and answers with the committed stable key/index. It never derives allow/deny options from an effect class.
2. **No optimistic approval:** a keypress does not close the menu, mark a grant, or mark the tool running. Only committed `MenuAnswered` and later item/effect events change projection.
3. **Modal precedence:** a blocking menu/login owns input before composer submission; `!` cannot bypass it. Stale menu clicks cannot answer a replacement card or another session.
4. **Escape routing:** `/...` remains slash command; `!` alone is a harmless validation error; exactly one leading `!` is stripped; `!!...` has explicitly tested literal semantics; bare non-demo text remains a model turn.
5. **Zero-provider shell:** `!cmd` produces one durable daemon command and zero `UserMessage`/provider turns. Command bytes are sent once without client-side shell interpolation or re-quoting.
6. **No client spawn:** the TUI never invokes a local OS subprocess for LIVE `!`; the rendered target/cwd identifies the session daemon.
7. **Committed command row:** one `CommandExecution` starts, zero or more ordered stdout/stderr byte deltas apply, and one terminal status lands. Exit code, failed/cancelled state, truncation, and decode error are honest under replay and reconnect.
8. **Grant isolation:** a user `!` neither resolves a provider permission card nor creates/consumes a session grant. `/tools` shows it as user-origin execution only if the daemon snapshot exposes active/audit data.
9. **Cwd contract:** either daemon-validated `!cd` persists for subsequent `!` commands with defined restart behavior, or W8 rejects it. A child process's transient `cd` is never painted as persistent.
10. **Real `/tools`:** LIVE opens no local `MenuOpened`; the screen lists the daemon's exact advertised tools/effects/defaults/grants. Disconnect/staleness is explicit, and selecting a row cannot fabricate custom registration.
11. **Effect/status rendering:** permission previews for read/write/process/network/credential classes remain readable at narrow heights; failed/cancelled process states use the frozen glyphs; model `process_exec` output is no longer silently retained off-screen.

Existing TUI laws to retain are the approval-card diff renderer ([w4a4_approval_card_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/tests/w4a4_approval_card_tests.rs:84)), exact live menu coordinates and same-command retry ([w3c3_live_driver_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/tests/w3c3_live_driver_tests.rs:912)), unseen-menu refusal ([w3c3_live_driver_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/tests/w3c3_live_driver_tests.rs:1018)), and the current rule that LIVE never paints the fake VFS ([w3c31_r2_tests.rs](/Users/rizzist/haider-run/haider-agent/crates/haider-tui/tests/w3c31_r2_tests.rs:561)). W8b should replace that last refusal only for explicit, daemon-backed `!`, while keeping the six bare demo commands isolated from LIVE.
