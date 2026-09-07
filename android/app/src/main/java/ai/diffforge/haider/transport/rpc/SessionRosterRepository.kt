package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.*
import java.io.Closeable

/** A full replacement projection of SessionSummary. Absent optionals remain unknown. */
data class SessionSummary(val sessionId: String, val headSeq: Long, val workerGeneration: Long,
    val provider: String?, val model: String?, val runState: String?, val runId: String?, val title: String?,
    val lastActivityMs: Long?, val seenAtMs: Long?, val workspaceCwd: String?, val needsInput: JsonObject?,
    val canonical: JsonObject) {
    companion object {
        fun parse(value: JsonObject) = SessionSummary(value.string("session_id"), value.number("head_seq"),
            value.number("worker_generation"), value.optionalString("provider"), value.optionalString("last_model"),
            value.optionalString("run_state"), value.optionalString("run_id"), value.optionalString("title"),
            value.optionalNumber("last_activity_ms"), value.optionalNumber("seen_at_ms"), value.optionalString("workspace_cwd"),
            value["needs_input"] as? JsonObject, value)
    }
}

/** Shared by UI and notifier (a notifier connects with View only). Own one per connection. */
class SessionRosterRepository(private val client: RpcClient, private val scope: CoroutineScope,
    private val cache: TranscriptCache) : SessionRoster, Closeable {
    private val lock = Any()
    private val refreshMutex = Mutex()
    private var watchedEpoch: Long? = null
    private val _sessions = MutableStateFlow(cache.loadRoster().map(SessionSummary::parse))
    private val _loading = MutableStateFlow(false)
    private val _ready = MutableStateFlow(false)
    @Volatile private var hydratedEpoch: Long? = null
    fun isReady(): Boolean = hydratedEpoch == client.connectionEpoch && client.state.value == RpcConnectionState.CONNECTED
    /** A successful baseline in the current connection epoch, never inferred from an empty cache. */
    val ready: StateFlow<Boolean> = _ready.asStateFlow()
    private val _error = MutableStateFlow<String?>(null)
    override val sessions: StateFlow<List<SessionSummary>> = _sessions.asStateFlow()
    override val loading: StateFlow<Boolean> = _loading.asStateFlow()
    override val loadError: StateFlow<String?> = _error.asStateFlow()
    private var baseline: MutableMap<String, SessionSummary>? = null
    private val buffered = mutableMapOf<String, SessionSummary>()
    private val watch = client.observeFrames { frame ->
        if (frame.string("kind") == "session_roster_delta") synchronized(lock) {
            val incoming = frame.objects("summaries").map(SessionSummary::parse)
            if (baseline != null) incoming.forEach { merge(buffered, it) }
            else {
                val rows = _sessions.value.associateBy { it.sessionId }.toMutableMap()
                incoming.forEach { merge(rows, it) }
                publish(rows)
            }
        }
    }
    private val reconnect = scope.launch {
        client.state.collect { if (it == RpcConnectionState.CONNECTED) refreshRoster() else _ready.value = false }
    }

    /** Subscribe first, buffer deltas, follow every opaque page, then replace the roster. */
    override suspend fun refreshRoster() = refreshMutex.withLock {
        val epoch = client.connectionEpoch
        if (watchedEpoch != epoch) _ready.value = false
        _loading.value = true
        _error.value = null
        synchronized(lock) { baseline = mutableMapOf(); buffered.clear() }
        try {
            if (watchedEpoch != epoch) {
                val accepted = client.request(RpcMethods.watchSessions(), epoch)
                RpcResponses.watch(accepted)
                watchedEpoch = epoch
            }
            val cursors = mutableSetOf<String>()
            var cursor: String? = null
            do {
                val page = client.request(RpcMethods.list(cursor, recency = client.welcome.value?.features?.contains("session_list_recency_v1") == true), epoch)
                val parsed = RpcResponses.roster(page)
                synchronized(lock) { parsed.sessions.forEach { merge(baseline!!, it) } }
                cursor = parsed.nextCursor
                if (cursor != null && !cursors.add(cursor)) throw RpcProtocolException("repeated_roster_cursor")
            } while (cursor != null)
            synchronized(lock) {
                if (epoch != client.connectionEpoch) throw java.io.IOException("connection_lost")
                val rows = baseline!!
                buffered.values.forEach { merge(rows, it) }
                publish(rows)
                hydratedEpoch = epoch
                _ready.value = true
                baseline = null
                buffered.clear()
            }
        } catch (cancelled: CancellationException) { throw cancelled }
        catch (_: Exception) { _error.value = "roster_unavailable" }
        finally { synchronized(lock) { baseline = null; buffered.clear() }; _loading.value = false }
    }

    override suspend fun observe(session: String): JsonObject = RpcResponses.observe(client.request(RpcMethods.observe(session)))

    private fun publish(rows: Map<String, SessionSummary>) {
        val sorted = rows.values.sortedWith(compareByDescending<SessionSummary> { it.lastActivityMs ?: Long.MIN_VALUE }.thenBy { it.sessionId })
        cache.saveRoster(sorted.map { it.canonical })
        _sessions.value = sorted
    }
    override fun close() { watch.close(); reconnect.cancel() }
    companion object {
        internal fun merge(rows: MutableMap<String, SessionSummary>, next: SessionSummary) {
            val old = rows[next.sessionId]
            if (old == null || next.workerGeneration > old.workerGeneration ||
                (next.workerGeneration == old.workerGeneration && next.headSeq >= old.headSeq)) rows[next.sessionId] = next
        }
    }
}
