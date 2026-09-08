package ai.diffforge.haider.transport.rpc

import kotlinx.serialization.json.*
import java.io.File
import java.io.FileOutputStream
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest

/** Rebuildable UI cache. It never opens the daemon database or stores account/RPC responses. */
class TranscriptCache(private val directory: File) {
    data class Entry(val seq: Long, val display: JsonObject)
    private val records = mutableMapOf<String, MutableList<Entry>>()
    init { check(directory.mkdirs() || directory.isDirectory) }

    @Synchronized fun loadRoster(): List<JsonObject> = try {
        (wireJson.parseToJsonElement(File(directory, "roster-v1.json").readText()) as JsonArray).map { it.jsonObject }
    } catch (_: Exception) { emptyList() }
    @Synchronized fun saveRoster(rows: List<JsonObject>) = atomic(File(directory, "roster-v1.json"), JsonArray(rows).toString())

    @Synchronized fun entries(session: String): List<Entry> = load(session).toList()
    @Synchronized fun lastApplied(session: String): Long = load(session).lastOrNull()?.seq ?: 0

    /** Record+cursor commit atomically in one append. Failed writes never advance in-memory truth. */
    @Synchronized fun apply(session: String, envelope: JsonObject): Boolean {
        if (envelope.string("session_id") != session) throw RpcProtocolException("event_session_mismatch")
        val seq = envelope.number("seq")
        val rows = load(session)
        val last = rows.lastOrNull()?.seq ?: 0
        if (seq <= last) return false
        if (seq != last + 1) throw ReplayGap(session)
        val display = displayProjection(envelope)
        val line = obj("seq" to seq, "display" to display).toString() + "\n"
        val file = file(session)
        val before = if (file.exists()) file.length() else 0L
        try {
            FileOutputStream(file, true).use { it.write(line.toByteArray(Charsets.UTF_8)); it.fd.sync() }
        } catch (error: Exception) {
            java.io.RandomAccessFile(file, "rw").use { it.setLength(before) }
            throw error
        }
        rows.add(Entry(seq, display))
        return true
    }

    @Synchronized fun reset(session: String) {
        atomic(file(session), "")
        records.remove(session)
    }

    private fun load(session: String): MutableList<Entry> = records.getOrPut(session) {
        val source = file(session)
        val result = mutableListOf<Entry>()
        if (source.exists()) {
            val bytes = source.readBytes()
            var end = 0
            for (index in bytes.indices) if (bytes[index] == 10.toByte()) {
                try {
                    val value = wireJson.parseToJsonElement(String(bytes, end, index - end, Charsets.UTF_8)).jsonObject
                    val seq = value.number("seq")
                    if (seq != result.size.toLong() + 1) break
                    result.add(Entry(seq, value.objectAt("display")))
                    end = index + 1
                } catch (_: Exception) { break }
            }
            // Only the verified prefix is a cursor, even after power loss during append.
            if (end != bytes.size) java.io.RandomAccessFile(source, "rw").use { it.setLength(end.toLong()) }
        }
        result
    }
    private fun file(session: String): File {
        val hash = MessageDigest.getInstance("SHA-256").digest(session.toByteArray()).joinToString("") { "%02x".format(it) }
        // Projection changes replay from zero: v1 lost unknown items, and v2
        // classified ordinary lifecycle/metric records as missing chat content.
        return File(directory, "$hash.replay-v3.jsonl")
    }
    private fun atomic(file: File, contents: String) {
        val temp = File.createTempFile("cache-", ".tmp", directory)
        try {
            FileOutputStream(temp).use { it.write(contents.toByteArray(Charsets.UTF_8)); it.fd.sync() }
            Files.move(temp.toPath(), file.toPath(), StandardCopyOption.ATOMIC_MOVE, StandardCopyOption.REPLACE_EXISTING)
        } finally { temp.delete() }
    }

    companion object {
        /** Allowlist presentation fields; unknown, hidden and secret-input payloads store no content. */
        fun displayProjection(envelope: JsonObject): JsonObject {
            if ((envelope["render"] as? JsonObject)?.get("ui") != JsonPrimitive(true)) return obj("type" to "hidden")
            val payload = envelope.objectAt("payload")
            return when (payload.optionalString("type")) {
                "user_message" -> obj("type" to "user_message", "text" to payload.optionalString("text"))
                // These describe roster/lifecycle state, not transcript content.
                // Do not persist their arbitrary additive fields in the display cache.
                "session_state", "run_state", "session_seen", "session_renamed" -> obj("type" to "metadata")
                "node_committed" -> {
                    val node = payload["kind"] as? JsonObject
                    val kind = node?.optionalString("kind")
                    if (kind in setOf("user_turn", "assistant_commit") && node?.optionalString("text") != null)
                        obj("type" to "history_node", "kind" to kind, "text" to node.optionalString("text"))
                    else obj("type" to "unrendered")
                }
                "item" -> {
                    val item = payload["item"] as? JsonObject
                    val delta = payload["delta"] as? JsonObject
                    val kind = item?.optionalString("item")
                    // Known counters have no message body. Keep unknown extensions,
                    // budget interruptions and tool records visibly partial.
                    if (item != null && kind == "extension" && delta == null && payload.optionalString("event") in setOf("started", "completed")) {
                        val extension = item.optionalString("kind")
                        if (extension == "context_footprint_v1" ||
                            (extension == "provider_request_budget_v1" &&
                                (item["data"] as? JsonObject)?.optionalString("phase") == "progress"))
                            return obj("type" to "metadata")
                    }
                    val value = when (kind) {
                        "agent_message", "incomplete_agent_message" -> obj("item" to kind, "text" to item.optionalString("text"))
                        "reasoning" -> obj("item" to kind, "summary" to item.optionalString("summary"))
                        "refusal" -> obj("item" to kind, "reason" to item.optionalString("reason"))
                        // Tool arguments and other structured items need a separately reviewed
                        // display projection. Dropping their content must keep coverage partial.
                        else -> null
                    }
                    val safeDelta = delta?.takeIf { it.optionalString("delta") in setOf("text", "reasoning") }
                        ?.let { obj("delta" to it.string("delta"), "text" to it.optionalString("text")) }
                    if ((item != null && value == null) || (delta != null && safeDelta == null) || (value == null && safeDelta == null))
                        obj("type" to "unrendered")
                    else obj("type" to "item", "event" to payload.optionalString("event"), "item_id" to payload.optionalString("item_id"),
                        "item" to value, "delta" to safeDelta)
                }
                else -> obj("type" to "unrendered")
            }
        }
    }
}
class ReplayGap(val session: String) : java.io.IOException("replay_gap")
