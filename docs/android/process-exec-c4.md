# C4 addendum — Android shell execution (972)

This addendum supersedes only the ProcessExec and local shell exclusions in
`contracts-v1.md` C4. It is a backend contract; terminal UI and independent
Android device verification are separate delivery stages.

## Execution and filesystem authority

The standalone daemon invokes the immutable OS interpreter `/system/bin/sh -c`
with the exact submitted command bytes. It never selects an interpreter from
`SHELL`, PATH, or writable app storage. The supported executable set is the
device's accessible system executables and shell builtins. No additional native
executable is packaged in this lane; a future packaged executable must be shipped
in an OS-managed executable install location and separately validated. Workspace,
cache, filesDir and downloaded files are **not native executable install
locations**. Android's W^X restrictions apply; chmod is not an installation method.
[Android 10 execution restrictions](https://developer.android.com/about/versions/10/behavior-changes-10#execute-permission).

Interpreting a workspace script using the system shell is permitted and is
ProcessExec, including scripts assembled by an agent. Interpretation does not
turn writable storage into a native executable location. There is no parser-based
allowlist for commands inside shell text and no claim to intercept subsequent
execs, filesystem calls, sockets, or interpreter behavior.

The daemon validates every session root against its retained immutable Android
workspace handle. The broker pins/revalidates the command's starting directory
under that root; cwd is optional and workspace-relative. The reserved lockdown
subtree cannot be selected as cwd. **Cwd containment is not filesystem
confinement.** Approved commands can access other same-UID app files, including
daemon state outside the workspace, and OS paths/network permitted by Android's
UID, SELinux and permissions. Brokered filesystem tools retain their stronger
handle-anchored boundary; that boundary does not apply to shell syscalls. Shell
permission must therefore be presented as app-UID execution, never workspace-only
execution. No extra Android permissions or root/su capability are granted.

The child environment is cleared. Android supplies fixed PATH `/system/bin`,
LANG `C`, HOME equal to the pinned workspace root, and TMPDIR equal to command
cwd. Ambient environment-name allowlists are rejected in standalone ProcessExec;
no account or daemon environment is inherited. Explicit values written in shell
text have the authority of that command. Commands are noninteractive, pipe-based,
one-shot executions, with a 60 s wall limit and 1 MiB combined-output threshold.
There is no PTY, persistent cwd/environment, package manager, or stdin facade.

## Authorization and availability

Model `process_exec` (and legacy `exec`) uses the existing EffectBroker Ask
default, durable menu answers and exact command/cwd/environment-name shape
grants. Explicit daemon session permission overrides/autonomous policy retain
their existing meaning; read-only and lockdown denial cannot be lifted by a
grant. Android UI Ask/Auto is client-side StandingConsent for device capabilities
only: **ProcessExec remains excluded**. Choosing Android Auto grants no shell
authority and answers no process permission menu.

The local terminal invokes existing `shell.exec` only for explicitly user-submitted
commands. That submission is the broker's `UserTyped` authorization for those
exact bytes, not a standing grant or a model-tool approval. It requires a Control
connection and Control attachment, an idle eligible root session, current worker
generation, available pinned workspace, and a full-trust provider. Delegated and
typed-agent sessions are ineligible for this direct door; model child execution
continues to obey its tool/effect grant. Scoped direct shell RPC stays excluded
in standalone. Remote/profile and background ProcessExec arguments are refused;
SSH, task routes, shell monitors, hooks, credential imports and all other C4
exclusions remain excluded.

`tools.inventory` has an additive optional `shell` response field, computed from
the same eligibility checks used at shell admission and again before spawn.
It carries `available`, `reason` and `worker_generation`. Missing means unknown,
never available. Inventory is an observation, not authorization: the accept
transaction checks idle/generation again and the worker rechecks policy/workspace.
The Kotlin facade invalidates on selection, roster/journal, provider, connection
and Binder changes and refreshes while observed. Every start re-queries capability.
Lockdown refusals retain their existing typed refusal and journal path.

## Output and lifecycle

Both entrances use the existing broker intent/authorization/dispatch/outcome
journal, output cap, timeout and artifact capture. Streaming journal/UI output
uses the existing line redactor. Raw bounded captures remain in the existing
owner-authorized CAS; the facade never implicitly fetches or exposes raw captures.
Redaction is pattern-based, not proof that arbitrary sensitive output is removed.
Command text is journaled as user input: users must not type secrets into it.

Each command is owned by its session supervisor within the `:daemon` foreground
service. Closing a screen, switching sessions, losing RPC, or UI-process death
does not cancel it. Reattachment replays committed redacted output and results;
it never automatically resubmits a command. Cancel uses the accepted run ID and
worker generation with `turn.cancel`. Session close and daemon drain cancel
owned executions. Cancellation/time/output limits send TERM, wait the existing
2 s grace, then KILL to the process group, retaining the unreaped leader until
the sweep to prevent recycled-PGID signals.

On Android, **normal leader exit also initiates this group sweep** before reap;
ordinary background descendants are not deliberately detached. Desktop normal
completion keeps its existing detach behavior. Dropping the Android supervisor
also performs a best-effort synchronous group KILL while the leader is still
pinned, covering forced runtime/task teardown. The existing bounded pipe drain
and cleanup errors remain observable; sending KILL is not proof of descendant
exit in uninterruptible kernel sleep.

Residual risks: a descendant can escape killpg with a new session/group; SIGKILL,
native crash, power loss, or OS removal of the daemon process can bypass Rust
cleanup, and survivors may remain until Android stops the app UID. No persisted
numeric PID is killed after restart (PID reuse would be unsafe). Android force-stop
is the OS cleanup boundary, not a daemon guarantee. This lane does not claim a
kernel cgroup/sandbox or abrupt-app-death descendant guarantee. A parent-death
signal alone would not solve it: it is not inherited across fork and follows the
creating thread's death, so this lane does not present it as tree containment.
[Linux parent-death signal contract](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html).
