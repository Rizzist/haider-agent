package ai.diffforge.haider.daemon

/** Lane 3 implements one View connection on h.sock, not the mobile codec or a Binder data tunnel.
 * Subscribe/buffer list_watch before paging ALL session.list pages. Emit a reconciled Baseline
 * on each connection, then Changes in order; Reset revokes the old baseline on connection loss.
 * observe must return promptly. The source owns its IO and closes it when the subscription closes.
 */
interface SessionRosterSource {
    fun observe(endpoint: RpcEndpoint, observer: (RosterUpdate) -> Unit): AutoCloseable
}
sealed interface RosterUpdate {
    data object Reset : RosterUpdate
    data class Baseline(val sessions: List<NotificationSession>) : RosterUpdate
    data class Changes(val sessions: List<NotificationSession>) : RosterUpdate
    data object Unavailable : RosterUpdate
}

/** Deliberately excludes transcripts, arbitrary titles, credentials, and raw wire payloads. */
data class NotificationSession(
    val sessionId: String,
    val headSeq: Long,
    val workerGeneration: Long,
    val runId: String?,
    val runState: String?,
    val input: NotificationInput?,
)
data class NotificationInput(val menuId: String, val requestSeq: Long, val workerGeneration: Long)

/** Honest integration boundary. Production never uses fake session data. */
class UnavailableSessionRosterSource : SessionRosterSource {
    override fun observe(endpoint: RpcEndpoint, observer: (RosterUpdate) -> Unit): AutoCloseable {
        observer(RosterUpdate.Unavailable)
        return AutoCloseable { }
    }
}

internal interface SessionNotificationSink {
    fun attention(session: NotificationSession)
    fun clearAttention(sessionId: String)
    fun completion(session: NotificationSession)
}

/** Called serially; a fresh baseline cannot be mistaken for completed historical work. */
internal class SessionNotificationObserver(private val sink: SessionNotificationSink) {
    private val previous = mutableMapOf<String, NotificationSession>()
    private val attention = mutableMapOf<String, NotificationInput>()
    private var hasBaseline = false

    fun accept(update: RosterUpdate) {
        when (update) {
            RosterUpdate.Reset, RosterUpdate.Unavailable -> reset()
            is RosterUpdate.Baseline -> {
                reset()
                hasBaseline = true
                update.sessions.forEach { apply(it, baseline = true) }
            }
            is RosterUpdate.Changes -> if (hasBaseline) update.sessions.forEach { apply(it, baseline = false) }
        }
    }

    private fun reset() {
        attention.keys.forEach(sink::clearAttention)
        previous.clear()
        attention.clear()
        hasBaseline = false
    }

    private fun apply(row: NotificationSession, baseline: Boolean) {
        if (!validCoordinate(row.sessionId) || row.headSeq < 0 || row.workerGeneration < 0) return
        val before = previous[row.sessionId]
        if (before != null && (row.workerGeneration < before.workerGeneration ||
                (row.workerGeneration == before.workerGeneration && row.headSeq <= before.headSeq))) return
        previous[row.sessionId] = row
        val input = row.input?.takeIf { validCoordinate(it.menuId) && it.requestSeq > 0 &&
            it.workerGeneration == row.workerGeneration }
        if (input != null) {
            if (attention.put(row.sessionId, input) != input) sink.attention(row)
        } else if (attention.remove(row.sessionId) != null) {
            sink.clearAttention(row.sessionId)
        }
        if (!baseline && before?.runId != null && before.workerGeneration == row.workerGeneration &&
            before.runState in ACTIVE_STATES && row.runState in TERMINAL_STATES &&
            (row.runId == null || row.runId == before.runId)) sink.completion(row)
    }
    private fun validCoordinate(value: String) = value.length in 1..128 && value.all { it.isLetterOrDigit() || it in "-_:" }
    private companion object {
        val ACTIVE_STATES = setOf("running", "waiting_for_route", "effect_unknown", "parked_permission", "parked_input")
        val TERMINAL_STATES = setOf("idle", "errored", "cancelled")
    }
}
