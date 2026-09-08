package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.*
import java.io.Closeable

/** Canonical replay plus full-roster content indexing. One process-scoped instance per UI client. */
class TranscriptRepository(private val client: RpcClient, private val scope: CoroutineScope,
    private val cache: TranscriptCache) : Closeable {
    data class Coverage(val indexedSessions: Int = 0, val totalSessions: Int = 0, val complete: Boolean = false, val error: String? = null)
    data class SearchHit(val sessionId: String, val seq: Long?, val text: String)
    private data class Attachment(val session: String, val control: Boolean)
    private val lock = Any()
    private val attachments = mutableMapOf<String, Attachment>()
    private val wanted = mutableMapOf<String, Boolean>()
    private val attaching = Mutex()
    private val recovering = mutableSetOf<String>()
    private val _revision = MutableStateFlow(0L)
    val revision: StateFlow<Long> = _revision.asStateFlow()
    private val _coverage = MutableStateFlow(Coverage())
    val coverage: StateFlow<Coverage> = _coverage.asStateFlow()
    private val listener = client.observeFrames { frame ->
        when (frame.string("kind")) {
            "event" -> {
                val session = frame.string("session_id")
                // Registration runs on the reader before any following push is delivered.
                if (synchronized(lock) { attachments[frame.string("attachment_id")]?.session == session && wanted.containsKey(session) }) {
                    try { if (cache.apply(session, frame.objectAt("envelope"))) _revision.value++ }
                    catch (_: ReplayGap) { recover(session) }
                }
            }
            "lagged", "attach_caught_up" -> {
                val session = synchronized(lock) { attachments[frame.string("attachment_id")]?.session }
                if (session != null && (frame.string("kind") == "lagged" || frame.number("high_water_seq") > cache.lastApplied(session))) recover(session)
            }
        }
    }
    private val reconnect = scope.launch {
        client.state.collect { state ->
            if (state != RpcConnectionState.CONNECTED) synchronized(lock) { attachments.clear() }
            else synchronized(lock) { wanted.toMap() }.forEach { (session, control) ->
                try { attach(session, control) } catch (_: java.io.IOException) { _coverage.value = _coverage.value.copy(complete = false, error = "replay_unavailable") }
            }
        }
    }

    fun transcript(session: String): List<TranscriptCache.Entry> = cache.entries(session)

    suspend fun attach(session: String, control: Boolean = false) = attaching.withLock { attachLocked(session, control) }

    /** Hold the attachment lease through the protected request, including concurrent selection/replay changes. */
    suspend fun <T> withControlAttachment(session: String, operation: suspend (Long) -> T): T = attaching.withLock {
        val epoch = attachLocked(session, control = true)
        operation(epoch)
    }

    private suspend fun attachLocked(session: String, control: Boolean): Long {
        synchronized(lock) { wanted[session] = control }
        detachCurrent(session)
        val epoch = client.connectionEpoch
        val register: (JsonObject) -> Unit = { body ->
            val receipt = RpcResponses.attachment(body)
            if (receipt.sessionId != session) throw RpcProtocolException("attachment_session_mismatch")
            synchronized(lock) { attachments[receipt.id] = Attachment(session, control) }
        }
        try { client.request(RpcMethods.attach(session, cache.lastApplied(session), control), epoch, register) }
        catch (error: RpcRemoteException) {
            if (error.code != "cursor_ahead") throw error
            cache.reset(session)
            client.request(RpcMethods.attach(session, 0, control), epoch, register)
        }
        return epoch
    }

    suspend fun detach(session: String) = attaching.withLock {
        synchronized(lock) { wanted.remove(session) }
        detachCurrent(session)
    }
    private suspend fun detachCurrent(session: String) {
        val ids = synchronized(lock) { attachments.filterValues { it.session == session }.keys.toList().also { ids -> ids.forEach(attachments::remove) } }
        ids.forEach { RpcResponses.detached(client.request(RpcMethods.detach(it))) }
    }
    private fun recover(session: String) {
        if (!synchronized(lock) { recovering.add(session) }) return
        scope.launch {
            try { attach(session, synchronized(lock) { wanted[session] } ?: return@launch) }
            catch (_: java.io.IOException) { _coverage.value = _coverage.value.copy(complete = false, error = "replay_unavailable") }
            finally { synchronized(lock) { recovering.remove(session) } }
        }
    }

    /** Index every known session through its recorded head; a failed/oversized envelope stays partial. */
    suspend fun indexAll(rows: List<SessionSummary>) {
        _coverage.value = Coverage(totalSessions = rows.size)
        var done = 0
        try {
            val epoch = client.connectionEpoch
            for (row in rows) {
                if (cache.lastApplied(row.sessionId) > row.headSeq) cache.reset(row.sessionId)
                val pageSize = 1L
                while (cache.lastApplied(row.sessionId) < row.headSeq) {
                    val start = cache.lastApplied(row.sessionId) + 1
                    val end = minOf(row.headSeq, start + pageSize - 1)
                    // v1 has no typed oversized-page response. A single envelope is the minimum
                    // safe range; if even that exceeds the negotiated cap, expose partial history.
                    val result = RpcResponses.read(client.request(RpcMethods.read(row.sessionId, start, end), epoch))
                    if (result.sessionId != row.sessionId) throw RpcProtocolException("read_session_mismatch")
                    val events = result.envelopes
                    if (events.isEmpty()) throw RpcProtocolException("incomplete_history")
                    events.forEach { cache.apply(row.sessionId, it) }
                    _revision.value++
                }
                _coverage.value = Coverage(++done, rows.size)
            }
            val unsupported = rows.any { row -> cache.entries(row.sessionId).any { it.display.optionalString("type") == "unrendered" } }
            _coverage.value = Coverage(done, rows.size, complete = !unsupported,
                error = if (unsupported) "unsupported_display_events" else null)
        } catch (cancelled: CancellationException) { throw cancelled }
        catch (_: Exception) { _coverage.value = Coverage(done, rows.size, error = "history_unavailable") }
    }

    fun search(query: String, rows: List<SessionSummary>): List<SearchHit> {
        if (query.isBlank()) return emptyList()
        return rows.flatMap { row ->
            val hits = mutableListOf<SearchHit>()
            val metadata = listOfNotNull(row.title, row.sessionId, row.provider, row.model, row.workspaceCwd).joinToString(" · ")
            if (metadata.contains(query, ignoreCase = true)) hits.add(SearchHit(row.sessionId, null, metadata))
            // Fold completed items with replacement semantics, so deltas/final text do not duplicate hits.
            val text = linkedMapOf<String, Pair<Long, String>>()
            cache.entries(row.sessionId).forEach { entry ->
                val p = entry.display
                val itemId = p.optionalString("item_id")
                when (p.optionalString("type")) {
                    "user_message" -> text["user:${entry.seq}"] = entry.seq to (p.optionalString("text") ?: "")
                    "history_node" -> {
                        val value = p.string("text")
                        if (text.values.lastOrNull()?.second != value) text["node:${entry.seq}"] = entry.seq to value
                    }
                    "item" -> if (itemId != null) {
                        val item = p["item"] as? JsonObject
                        val delta = p["delta"] as? JsonObject
                        val replacement = ((item?.get("text") ?: item?.get("summary") ?: item?.get("reason")) as? JsonPrimitive)?.contentOrNull
                        val addition = (delta?.get("text") as? JsonPrimitive)?.contentOrNull
                        if (replacement != null) text[itemId] = entry.seq to replacement
                        else if (addition != null) text[itemId] = entry.seq to ((text[itemId]?.second ?: "") + addition)
                    }
                }
            }
            text.values.filter { it.second.contains(query, ignoreCase = true) }.forEach { hits.add(SearchHit(row.sessionId, it.first, it.second)) }
            hits
        }
    }
    override fun close() { listener.close(); reconnect.cancel() }
}
