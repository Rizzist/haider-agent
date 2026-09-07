package ai.diffforge.haider.daemon

import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.ui.chat.Message
import kotlinx.coroutines.flow.StateFlow

/**
 * The only interface the UI lane consumes (UI-SPEC 5.3).
 *
 * The daemon-embedding lane (971-2/971-3) implements it over the Binder control
 * plane plus the RPC data plane; this lane ships [FakeDaemonService] so every
 * screen and every state-matrix row is reachable without a daemon.
 */

// ---------- daemon lifecycle (Android-owned) ----------

sealed interface DaemonStatus {
    data object Stopped : DaemonStatus
    data object Starting : DaemonStatus
    data object Restarting : DaemonStatus
    data class Failed(val reason: String, val code: String?) : DaemonStatus
    data class Running(val info: DaemonInfo) : DaemonStatus
}

data class DaemonInfo(
    // from `daemon.status` / status.snapshot — daemon-owned
    val version: String,
    val generation: Long,
    val pid: Int? = null,
    val socketPath: String? = null,
    val ready: Boolean = true,
    val sessionCount: Long? = null,
    val waitingForRouteCount: Long? = null,
    val profilePath: String? = null,
    val runtimeDir: String? = null,
    // ANDROID-OWNED — the daemon has no uptime or memory field. Verified absent
    // from status.snapshot / StatusDocument / haider-client. Do NOT wait for
    // them on the wire (UI-SPEC 5.3 non-negotiable 6, trap 6.6.7).
    val startedAtMs: Long? = null,
    val rssBytes: Long? = null,
)

// ---------- session roster ----------

enum class SessionVisualState { Running, NeedsInput, WaitingForNetwork, Idle, Errored, Unknown }

data class ForkProvenance(val sessionId: String, val seq: Long)

data class MenuOption(
    val key: String,
    val label: String,
    val detail: String? = null,
    /** allow_once | allow_always | reject_once | reject_always — style from this, never the label. */
    val decision: String? = null,
)

data class NeedsInput(
    /** permission|question|approval|recovery|secret|update|trust_hook|choice|conflict|file|exhausted|unknown — MAY GROW. */
    val kind: String,
    val title: String,
    /** Render verbatim; never rewrite into prose. */
    val safeBody: List<String> = emptyList(),
    val menuId: String? = null,
    val requestSeq: Long? = null,
    val workerGeneration: Long? = null,
    val sinceMs: Long? = null,
    val options: List<MenuOption> = emptyList(),
    /** true ⇒ NEVER a plain text field (UI-SPEC 3.7). */
    val secretAnswer: Boolean = false,
) {
    /** The daemon's own title, falling back to the first safe-body line. */
    val displayTitle: String
        get() = title.ifBlank { safeBody.firstOrNull().orEmpty() }
}

data class SessionRow(
    val id: String,
    val title: String? = null,
    val state: SessionVisualState = SessionVisualState.Unknown,
    /** Raw ObserveRunStateWire string, kept for diagnostics. */
    val runState: String = "unknown",
    val provider: String? = null,
    val model: String? = null,
    val effort: String? = null,
    val fast: Boolean? = null,
    val agentType: String? = null,
    /** null = unknown, NOT 0. */
    val lastActivityMs: Long? = null,
    val createdAtMs: Long? = null,
    val seenAtMs: Long? = null,
    val turnCount: Long? = null,
    /** Some(0) = truly empty; null = unknown. */
    val footprintTokens: Long? = null,
    val footprintExact: Boolean? = null,
    val workspaceCwd: String? = null,
    val forkedFrom: ForkProvenance? = null,
    val parentSessionId: String? = null,
    val kind: String? = null,
    val needsInput: NeedsInput? = null,
    /** null ⇒ no active run ⇒ render no Stop button. */
    val runId: String? = null,
    val workerGeneration: Long = 0L,
    val headSeq: Long = 0L,
) {
    val unseen: Boolean
        get() = lastActivityMs != null && seenAtMs != null && lastActivityMs > seenAtMs
}

/** Roster page state, so the drawer can scroll hundreds of sessions honestly. */
data class RosterPaging(
    val loading: Boolean = false,
    val hasMore: Boolean = false,
    val cursor: String? = null,
)

/**
 * The `run_state` -> visual-state fold (UI-SPEC 5.3). The only sanctioned one.
 *
 * `Unknown` is neutral on purpose (sessionActivity.js:91-94): it never renders
 * green, never animates, never claims the session is fine.
 */
object SessionVisualStateFold {
    fun fold(runState: String?, needsInput: NeedsInput?): SessionVisualState {
        if (needsInput != null) return SessionVisualState.NeedsInput
        return when (runState?.lowercase()) {
            "parked_input", "parked_permission" -> SessionVisualState.NeedsInput
            "running" -> SessionVisualState.Running
            "waiting_for_route" -> SessionVisualState.WaitingForNetwork
            "idle", "cancelled" -> SessionVisualState.Idle
            "errored" -> SessionVisualState.Errored
            else -> SessionVisualState.Unknown
        }
    }

    /** Neither a rail nor a pill is drawn for these; nothing animates either. */
    fun rendersRail(state: SessionVisualState): Boolean = when (state) {
        SessionVisualState.Running,
        SessionVisualState.NeedsInput,
        SessionVisualState.WaitingForNetwork,
        SessionVisualState.Errored,
        -> true
        SessionVisualState.Idle, SessionVisualState.Unknown -> false
    }

    /** Errored never animates — nothing pulses for a corpse (render.rs:1129-1144). */
    fun animates(state: SessionVisualState): Boolean = state == SessionVisualState.Running
}

/** The compare-and-set coordinates a cancel needs, read from one snapshot. */
data class CancelCoordinates(
    val sessionId: String,
    val runId: String,
    val workerGeneration: Long,
)

object TurnCancel {
    /**
     * `run_id` and `worker_generation` must come from the same message as
     * `run_state`; pairing an id from one poll with a state from another can
     * cancel a run that already ended (frame.rs:1716-1730).
     */
    fun coordinates(row: SessionRow?): CancelCoordinates? {
        val runId = row?.runId ?: return null
        return CancelCoordinates(row.id, runId, row.workerGeneration)
    }
}

// ---------- the service the UI talks to ----------

interface DaemonService {
    val status: StateFlow<DaemonStatus>

    /** Network / notification / battery signals, from the C2 snapshot. */
    val environment: StateFlow<DaemonEnvironment>
    val sessions: StateFlow<List<SessionRow>>
    val paging: StateFlow<RosterPaging>
    val activeSessionId: StateFlow<String?>
    val models: StateFlow<SessionConfig?>
    val catalogError: StateFlow<String?>

    /** When the catalog request went out; drives the model chip's 6 s deadline. */
    val catalogRequestedAtMs: StateFlow<Long?>

    suspend fun start()
    suspend fun stop()
    suspend fun restart()
    suspend fun refreshRoster()

    /** Paginated `session.list`; appends the next page to [sessions]. */
    suspend fun loadMoreSessions()
    suspend fun createSession(model: String? = null, effort: String? = null): String
    suspend fun activate(sessionId: String)
    suspend fun markSeen(sessionId: String)
    suspend fun rename(sessionId: String, title: String)
    suspend fun fork(sessionId: String): String
    suspend fun delete(sessionId: String)

    /** Refuses unless [TurnCancel.coordinates] resolves from the current snapshot. */
    suspend fun stopTurn(sessionId: String)
    suspend fun answer(
        sessionId: String,
        menuId: String,
        optionKey: String,
        optionIndex: Int,
        text: String? = null,
    )
    suspend fun selectModel(provider: String, model: String)
    suspend fun selectEffort(effort: String?)
    suspend fun refreshModels()
    suspend fun send(sessionId: String, text: String)

    /** `session.attach`: the replayed transcript of a past session. */
    suspend fun transcript(sessionId: String): List<Message>
}

/** Raised when a cancel was asked for without live coordinates. */
class MissingRunCoordinates(sessionId: String) :
    IllegalStateException("No active run for session $sessionId")
