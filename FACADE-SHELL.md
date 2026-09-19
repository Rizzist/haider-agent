# Android shell facade contract

This is the handoff contract for the separate Compose terminal stage. The backend in this lane is one-shot, non-PTY shell execution over the existing daemon RPC and durable transcript replay. The UI must not infer capability locally or submit commands through the model composer.

## Facade surface

`DaemonService` exposes:

```kotlin
val shell: StateFlow<ShellAvailability>
val shellExecutions: StateFlow<Map<String, List<ShellExecution>>>
suspend fun refreshShell()
suspend fun startShell(
    sessionId: String,
    submissionId: String,
    command: String,
    cwd: String? = null,
): ShellExecutionRef
suspend fun cancelShell(execution: ShellExecutionRef)
```

The production implementation is `RpcDaemonService`; `FakeDaemonService` implements the same surface for previews and UI tests.

## Availability

`shell` describes only the currently selected session:

```kotlin
data class ShellAvailability(
    val available: Boolean = false,
    val reason: String? = null,
    val sessionId: String? = null,
    val workerGeneration: Long? = null,
)
```

Treat `available` as a recent daemon observation, not an authorization credential. `startShell` performs the same checks again. The backend invalidates the observation on selected-session, roster/run, provider, connection, endpoint/Binder, and relevant replay lifecycle changes; it also repairs missed publications periodically. Call `refreshShell()` when the Shell tab becomes visible and after an explicit UI action that can change provider trust or session eligibility.

Known reason codes include `not_observed`, `checking`, `no_session`, `disconnected`, `capability_unknown`, `capability_unavailable`, `control_required`, `control_attachment_required`, `daemon_unavailable`, `session_unavailable`, `session_ineligible`, `read_only`, `lockdown`, `policy_unavailable`, `workspace_unavailable`, `system_shell_unavailable`, `session_busy`, and `process_exec_disabled`. Unknown additive reasons must render as unavailable rather than being mapped to success.

Availability requires a connected/ready daemon, an eligible idle root session, current Control capability and attachment, a valid Android workspace, a full-trust non-lockdown provider, the current worker generation, and the Android system shell. Child/delegated or typed-agent sessions are not eligible for this direct user door.

## Starting a command

The UI creates a fresh UUID-style `submissionId` before calling `startShell` and retains it until the call has definitely returned or the user abandons an uncertain submission. Reuse that ID only to retry the exact same `(sessionId, command, cwd)` after response loss. Never generate a new ID automatically after an uncertain response, and never resubmit merely because the connection or UI process restarted. A conflicting reuse fails with `submission_id_conflict`.

Input bounds enforced by the facade are:

- `submissionId`: nonblank, at most 128 characters.
- `command`: nonblank, at most 8,192 UTF-8 bytes.
- `cwd`: null or a nonempty relative path. The daemon canonicalizes it beneath the session workspace; absolute paths and the reserved lockdown subtree are refused.

Only one nonterminal run is accepted per session. The command is noninteractive and receives no stdin. It runs through `/system/bin/sh -c` with the fixed environment and 60-second/1-MiB daemon bounds defined by `docs/android/process-exec-c4.md`. This is app-UID execution, not filesystem confinement to the workspace.

On acceptance, `startShell` returns the exact durable coordinates:

```kotlin
data class ShellExecutionRef(
    val sessionId: String,
    val commandId: String,       // the caller's submissionId
    val runId: String,
    val itemId: String,
    val workerGeneration: Long,
)
```

Keep this value with the rendered command. Cancellation must pass the same reference; do not reconstruct it from the currently selected session or a newer roster generation.

## Execution projection and output

`shellExecutions` is a full-replacement map keyed by session ID. Collect it as state and render `shellExecutions[sessionId].orEmpty()`. The production adapter observes up to 32 recently used sessions and exposes at most 64 recent commands per observed session. These are UI-memory bounds, not deletion of the daemon journal.

```kotlin
enum class ShellExecutionStatus {
    Running, Completed, Cancelled, Error, Reconnecting
}

enum class ShellOutputStream { Stdout, Stderr }

data class ShellOutput(
    val seq: Long,
    val stream: ShellOutputStream,
    val chunkBase64: String,
)

data class ShellExecution(
    val ref: ShellExecutionRef,
    val command: String,
    val status: ShellExecutionStatus,
    val output: List<ShellOutput>,
    val outputTruncated: Boolean,
    val exitCode: Int?,
    val error: String?,
)
```

Output is already the daemon's redacted durable stream; the facade never fetches raw CAS captures. Redaction is pattern-based, so the UI must not label output as guaranteed secret-free. Preserve `output` order by `seq` for an interleaved terminal view. Base64 decode incrementally with one UTF-8 decoder per stream because a code point can cross chunk boundaries. `ShellExecution.text(stream)` is a safe convenience for a completed per-stream view, but it intentionally loses stdout/stderr interleaving.

The in-memory projection retains at most 256 KiB of decoded output per command. When more durable output exists, `outputTruncated` is true and the retained prefix stays visible. Do not interpret this UI bound as the daemon's 1-MiB execution limit or offer a raw-capture download through this facade.

Status meanings:

- `Running`: replay is caught up and the command is nonterminal.
- `Reconnecting`: a previously observed nonterminal command is being replayed after connection loss or a detected gap. It is not a request to resubmit.
- `Completed`: the command item completed; `exitCode` is authoritative and may be nonzero.
- `Cancelled`: durable run state reports cancellation.
- `Error`: the item or run failed; `error` is a bounded daemon error code when available.

Commands can keep running when the Shell tab closes, another session is selected, the RPC socket drops, or the UI process dies. Reattachment rebuilds the list from durable redacted replay and deduplicates by journal sequence. The UI should therefore show cached history while reconnecting and must not synthesize completion.

## Cancellation and errors

`cancelShell(ref)` sends `turn.cancel` with the accepted `runId` and `workerGeneration`. Repeating the same cancellation uses a deterministic command ID. Disable Stop after the projected command is terminal, but still handle an already-terminal or stale-generation refusal honestly.

Facade calls use ordinary Kotlin argument failures for local programmer/input errors and RPC/IO exceptions for daemon, connection, policy, conflict, busy, and protocol failures. Present a bounded user-facing message and then rely on refreshed `shell`/`shellExecutions` state; do not convert an exception into a successful or completed command.

## Fake service controls

For deterministic UI tests:

```kotlin
fake.setShell(ShellAvailability(available = true, sessionId = sessionId))
val ref = fake.startShell(sessionId, submissionId, command, cwd)
fake.appendShellOutput(ref, "text", ShellOutputStream.Stdout)
fake.setShellResult(ref, ShellExecutionStatus.Completed, exitCode = 0)
fake.cancelShell(ref)
```

The fake never launches a process and never auto-completes. Tests must drive output and the terminal status explicitly. Keep device/daemon integration claims separate from fake UI coverage.
