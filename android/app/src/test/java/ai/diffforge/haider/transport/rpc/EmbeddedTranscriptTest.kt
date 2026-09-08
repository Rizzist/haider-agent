package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.ui.chat.Role
import kotlinx.coroutines.*
import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import java.nio.file.Files

class EmbeddedTranscriptTest {
    @Test fun actualFirstTurnProjectsCompleteTextWithoutDuplicateNodes() = runBlocking<Unit> {
        // API35 h.sock capture, with ephemeral identities and hidden payloads removed.
        val events = javaClass.getResourceAsStream("/integration-first-turn-v1.json")!!.use {
            wireJson.parseToJsonElement(it.reader().readText()).jsonArray.map(JsonElement::jsonObject)
        }
        val directory = Files.createTempDirectory("embedded-transcript").toFile()
        try {
            val cache = TranscriptCache(directory)
            events.forEach { cache.apply("integration-session", it) }
            val entries = TranscriptCache(directory).entries("integration-session")
            assertEquals(25L, entries.last().seq)
            assertFalse(entries.any { it.display.optionalString("type") == "unrendered" })
            val messages = RpcUiMapping.messages(entries)
            assertEquals(listOf(Role.User, Role.Agent), messages.map { it.role })
            assertEquals("Hello from API35 integration", messages[0].text)
            assertEquals("Haider integration fixture: the embedded daemon received your message over h.sock.", messages[1].text)
            assertFalse(messages.any { it.streaming })
            val scope = CoroutineScope(SupervisorJob())
            val client = RpcClient(scope)
            val repository = TranscriptRepository(client, scope, cache)
            try {
                val row = SessionSummary.parse(obj("session_id" to "integration-session", "head_seq" to 25, "worker_generation" to 1))
                repository.indexAll(listOf(row))
                assertTrue(repository.coverage.value.complete)
                assertEquals(1, repository.search("embedded daemon", listOf(row)).size)
                assertEquals(1, repository.search("Hello from API35", listOf(row)).size)
            } finally { repository.close(); client.close(); scope.cancel() }
        } finally { directory.deleteRecursively() }
    }

    @Test fun unknownAndAttentionRecordsRemainPartialAndDoNotPersistArbitraryContent() {
        val payloads = listOf(
            obj("type" to "future_event", "text" to "must-not-cache"),
            obj("type" to "run_failed", "message" to "must-not-cache"),
            obj("type" to "node_committed", "kind" to obj("kind" to "future_node", "text" to "must-not-cache")),
            obj("type" to "item", "event" to "completed", "item" to obj("item" to "extension", "kind" to "future_extension", "data" to obj("text" to "must-not-cache"))),
            obj("type" to "item", "event" to "completed", "item" to obj("item" to "extension", "kind" to "provider_request_budget_v1", "data" to obj("phase" to "exhausted", "text" to "must-not-cache"))),
        )
        payloads.forEach { payload ->
            val display = TranscriptCache.displayProjection(obj("render" to obj("ui" to true), "payload" to payload))
            assertEquals(obj("type" to "unrendered"), display)
        }
    }

    @Test fun aHistoryNodeWithoutItsOriginalItemStillSuppliesText() {
        val payload = obj("type" to "node_committed", "kind" to obj("kind" to "assistant_commit", "text" to "Retained reply"))
        val display = TranscriptCache.displayProjection(obj("render" to obj("ui" to true), "payload" to payload))
        val message = RpcUiMapping.messages(listOf(TranscriptCache.Entry(1, display))).single()
        assertEquals(Role.Agent, message.role)
        assertEquals("Retained reply", message.text)
    }
}
