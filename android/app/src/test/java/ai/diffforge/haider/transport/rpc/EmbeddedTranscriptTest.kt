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
            obj("type" to "future_event", "text" to "must-not-cache") to "future_event",
            obj("type" to "node_committed", "kind" to obj("kind" to "future_node", "text" to "must-not-cache")) to "future_node",
            obj("type" to "item", "event" to "completed", "item" to obj("item" to "extension", "kind" to "future_extension", "data" to obj("text" to "must-not-cache"))) to "extension",
            obj("type" to "item", "event" to "completed", "item" to obj("item" to "extension", "kind" to "provider_request_budget_v1", "data" to obj("phase" to "exhausted", "text" to "must-not-cache"))) to "extension",
        )
        payloads.forEach { (payload, family) ->
            val display = TranscriptCache.displayProjection(obj("render" to obj("ui" to true), "payload" to payload))
            // The family is named so the coverage notice can summarise what it
            // could not draw; no arbitrary content comes with it (971-V F7).
            assertEquals(obj("type" to "unrendered", "family" to family), display)
            assertFalse(display.toString().contains("must-not-cache"))
        }
    }

    /**
     * The canonical terminal cause is rendered, and only its safe fields are
     * kept (971-V F7: the errored session showed an icon and no explanation).
     */
    @Test fun runFailedProjectsItsSafePresentationAndNeverTheFreeTextMessage() {
        val payload = obj("type" to "run_failed", "code" to "store_read_only", "retryable" to false,
            "message" to "must-not-cache",
            "presentation" to obj("subcode" to "cas_publish_denied", "title" to "Haider could not save the reply",
                "detail" to "The provider view store is read-only.", "scope" to "run",
                "allowed_actions" to JsonArray(emptyList())))
        val display = TranscriptCache.displayProjection(obj("render" to obj("ui" to true), "payload" to payload))
        assertEquals("run_failed", display.optionalString("type"))
        assertFalse(display.toString().contains("must-not-cache"))
        val message = RpcUiMapping.messages(listOf(TranscriptCache.Entry(1, display))).single()
        assertEquals(
            "Haider could not save the reply · The provider view store is read-only. · store_read_only",
            message.error,
        )
        assertFalse(message.errorRetryable)
        // A pre-E2 journal that stated no presentation still explains itself
        // with the code, rather than reading as an empty agent turn.
        val bare = TranscriptCache.displayProjection(obj("render" to obj("ui" to true),
            "payload" to obj("type" to "run_failed", "code" to "provider_error", "retryable" to true)))
        val fallback = RpcUiMapping.messages(listOf(TranscriptCache.Entry(1, bare))).single()
        assertEquals("provider_error", fallback.error)
        assertTrue(fallback.errorRetryable)
    }

    @Test fun providerFailureShowsOnlyTheAllowlistedDiagnosticFields() {
        val payload = obj("type" to "run_failed", "code" to "provider_error", "retryable" to false,
            "message" to "private raw body",
            "presentation" to obj("title" to "Provider denied the request",
                "detail" to "message withheld: may contain account data",
                "provider_error_type" to "permission_error",
                "provider_http_status" to 403,
                "provider_request_id" to "req_fixture-403",
                "unreviewed" to "private additive field"))
        val display = TranscriptCache.displayProjection(obj("render" to obj("ui" to true), "payload" to payload))
        assertFalse(display.toString().contains("private"))
        val message = RpcUiMapping.messages(listOf(TranscriptCache.Entry(1, display))).single()
        assertEquals("Provider denied the request · message withheld: may contain account data · Provider error: permission_error · HTTP 403 · Request ID: req_fixture-403 · provider_error", message.error)
    }

    @Test fun aHistoryNodeWithoutItsOriginalItemStillSuppliesText() {
        val payload = obj("type" to "node_committed", "kind" to obj("kind" to "assistant_commit", "text" to "Retained reply"))
        val display = TranscriptCache.displayProjection(obj("render" to obj("ui" to true), "payload" to payload))
        val message = RpcUiMapping.messages(listOf(TranscriptCache.Entry(1, display))).single()
        assertEquals(Role.Agent, message.role)
        assertEquals("Retained reply", message.text)
    }
}
