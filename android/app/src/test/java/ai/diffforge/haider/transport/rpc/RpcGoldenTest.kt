package ai.diffforge.haider.transport.rpc

import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import java.io.*
import java.nio.ByteBuffer
import java.nio.file.Files

class RpcGoldenTest {
    private fun resource(name: String) = javaClass.classLoader!!.getResourceAsStream(name)!!.bufferedReader().use { it.readText() }
    private fun json(name: String) = wireJson.parseToJsonElement(resource(name))
    private fun frames() = json("wire_transcript.json").jsonArray.map { RpcWire.parse(it.jsonObject.string("ws_body")) }

    @Test fun consumesEveryVersionedWireGoldenAsExactUdsBytes() {
        for (name in listOf("wire_transcript.json", "peer_agent_injection_wire.json")) {
            json(name).jsonArray.forEach { entry ->
                val row = entry.jsonObject
                val hex = row.string("uds_stream_hex")
                val bytes = hex.chunked(2).map { it.toInt(16).toByte() }.toByteArray()
                val expected = RpcWire.parse(row.string("ws_body"))
                assertEquals(expected, RpcWire.read(ByteArrayInputStream(bytes)))
                val output = ByteArrayOutputStream()
                RpcWire.write(output, expected)
                assertArrayEquals(bytes, output.toByteArray())
            }
        }
        for (name in listOf("provider_rebind_wire.json", "turn_retract_wire.json"))
            json(name).jsonArray.forEach { frame -> assertEquals(frame, RpcWire.parse(frame.toString())) }
    }

    @Test fun supplementaryMethodAndAvailabilityGoldensAreConsumedWithoutSchemaCopies() {
        val fixture = json("client_contract_methods_v1.json").jsonObject
        assertEquals("haider-client-wire/v1", fixture.string("contract"))
        fixture.objects("methods").forEachIndexed { index, item ->
            val request = RpcWire.request("golden-$index", item.objectAt("request"))
            assertEquals(item.string("request_method"), request.objectAt("body").string("method"))
            assertEquals(item.string("response_method"), item.objectAt("response").string("method"))
            assertEquals(request, RpcWire.parse(request.toString()))
        }
        val pairs = json("snapshot_availability_compat_v1.json").jsonObject.objects("pairs")
        pairs.forEach { pair ->
            for (key in listOf("old", "new")) {
                val body = pair.objectAt(key)
                assertEquals(pair.string("method"), body.string("method"))
                assertEquals(body, RpcWire.parse(obj("v" to 1, "kind" to "response", "request_id" to "a", "body" to body).toString()).objectAt("body"))
            }
        }
    }

    @Test fun usedRequestEncodersMatchRustGoldens() {
        val requests = frames().filter { it.string("kind") == "request" }.associateBy { it.string("request_id") }
        fun check(id: String, actual: JsonObject) { assertEquals(id, requests.getValue(id).objectAt("body"), actual) }
        check("request-list", RpcMethods.list("cursor-after-session-0", 50))
        check("request-read", RpcMethods.read("session-1", 5, 9))
        check("request-attach", RpcMethods.attach("session-1", 4))
        check("request-submit", RpcMethods.submit("command-submit", SessionCoordinate("session-created", 7), "hello"))
        check("request-observe", RpcMethods.observe("session-1"))
        check("request-stage", RpcMethods.stage("stage-1", "api_key", "golden-placeholder-key"))
        check("request-login", RpcMethods.loginApi("command-login", "anthropic", "work", "vaultref-0123456789abcdef"))
        check("request-oauth-start", RpcMethods.oauthStart("fake-oauth", "work-oauth", "attempt-1"))
        check("request-oauth-status", RpcMethods.oauthStatus("oauth-flow-golden", "attempt-1"))
        check("request-oauth-cancel", RpcMethods.oauthCancel("oauth-flow-golden", "attempt-1"))
        check("request-account-add", RpcMethods.addOAuth("command-account-add", "fake-oauth", "work-oauth", "oauth-flow-golden", "attempt-1", "oauth-ready-golden"))
        check("request-remove", RpcMethods.remove("command-remove", "work", 8))
        check("request-set-active", RpcMethods.setActive("command-set-active", "work"))
        check("request-default-model", RpcMethods.setDefaultModel("command-default-model", "openai", "frontier-a", 9))
        check("request-provider-models-refresh", RpcMethods.refreshModels("openai-oauth"))
        check("request-session-rename", RpcMethods.rename("command-session-rename", SessionCoordinate("session-1", 7), "Parser rewrite"))
        val menu = frames().first { it.string("kind") == "menu_answer" }
        val answer = RpcMethods.menu("command-1", MenuCoordinate(SessionCoordinate("session-1", 7), "menu-1", 8, "other", 2), RpcMethods.textInput("custom answer"))
        assertEquals(JsonObject(menu - setOf("v", "kind", "request_id")), answer)
        val extra = json("client_contract_methods_v1.json").jsonObject.objects("methods").associateBy { it.string("request_method") }
        for (body in listOf(RpcMethods.watchSessions(), RpcMethods.watchAccounts(), RpcMethods.refreshAccount("openai-oauth"))) {
            assertEquals(extra.getValue(body.string("method")).objectAt("request"), body)
        }
    }

    @Test fun rustRosterAuthorityPreservesUnknownOptionalsAndUnknownEnums() {
        val rows = frames().filter { it["body"] is JsonObject && it.objectAt("body").optionalString("method") == "session.list" && it.string("kind") == "response" }
        val old = SessionSummary.parse(rows.first().objectAt("body").objects("sessions").single())
        assertNull(old.provider); assertNull(old.model); assertNull(old.runState); assertNull(old.lastActivityMs)
        val future = SessionSummary.parse(JsonObject(old.canonical + mapOf("run_state" to JsonPrimitive("future_state"), "metadata" to obj("provider" to "must-not-infer"))))
        assertNull(future.provider); assertEquals("future_state", future.runState)
        val map = mutableMapOf(old.sessionId to future)
        SessionRosterRepository.merge(map, old.copy(headSeq = old.headSeq - 1))
        assertEquals(future, map[old.sessionId])
        SessionRosterRepository.merge(map, old)
        assertNull(map[old.sessionId]!!.runState)
    }

    @Test fun malformedOversizeWrongVersionAndUnknownKinds() {
        for (bytes in listOf(byteArrayOf(0), byteArrayOf(0, 0, 0, 0), ByteBuffer.allocate(4).putInt(-1).array(),
            ByteBuffer.allocate(4).putInt(RpcWire.MAX_BODY + 1).array(), byteArrayOf(0, 0, 0, 2, 123), byteArrayOf(0, 0, 0, 1, -1))) {
            assertThrows(RpcProtocolException::class.java) { RpcWire.read(ByteArrayInputStream(bytes)) }
        }
        for (text in listOf("{'v':1,'kind':'future'}", "{\"v\":1,\"kind\":\"future\"}x", "{\"v\":2,\"kind\":\"future\"}", "{\"v\":\"1\",\"kind\":\"future\"}"))
            assertThrows(RpcProtocolException::class.java) { RpcWire.parse(text) }
        assertEquals("future", RpcWire.parse("{\"v\":1,\"kind\":\"future\",\"extra\":{}}").string("kind"))
    }

    @Test fun cacheUsesRustPayloadProjectionAndDurableAppliedCursor() {
        val dir = Files.createTempDirectory("haider-replay-test").toFile()
        try {
            val cache = TranscriptCache(dir)
            val envelope = frames().first { it.string("kind") == "event" }.objectAt("envelope")
            json("android_display_payloads_v1.json").jsonArray.take(4).forEachIndexed { index, payload ->
                val next = JsonObject(envelope + mapOf("seq" to JsonPrimitive(index + 1), "payload" to payload))
                assertTrue(cache.apply("session-1", next))
                assertFalse(cache.apply("session-1", next))
            }
            assertEquals(4L, TranscriptCache(dir).lastApplied("session-1"))
            assertEquals("Found your conversation", cache.entries("session-1").last().display.objectAt("item").string("text"))
            assertThrows(ReplayGap::class.java) { cache.apply("session-1", envelope) }
            val secret = JsonObject(envelope + mapOf("seq" to JsonPrimitive(5), "payload" to obj("type" to "menu_answered", "secret" to "DO-NOT-CACHE")))
            cache.apply("session-1", secret)
            assertFalse(dir.listFiles()!!.joinToString { it.readText() }.contains("DO-NOT-CACHE"))
            val file = dir.listFiles()!!.first { it.name.endsWith("jsonl") }
            file.appendText("{\"seq\":6")
            assertEquals(5L, TranscriptCache(dir).lastApplied("session-1"))
            assertTrue(file.readText().endsWith("\n"))
        } finally { dir.deleteRecursively() }
    }

    @Test fun unsupportedRustItemsStayPartialAndOldProjectionCursorsAreRebuilt() = kotlinx.coroutines.runBlocking<Unit> {
        val dir = Files.createTempDirectory("haider-partial-test").toFile()
        val scope = kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.SupervisorJob())
        val client = RpcClient(scope)
        val cache = TranscriptCache(dir)
        val repository = TranscriptRepository(client, scope, cache)
        try {
            val template = frames().first { it.string("kind") == "event" }.objectAt("envelope")
            val plan = json("android_display_payloads_v1.json").jsonArray.last()
            assertEquals("plan", plan.jsonObject.objectAt("item").string("item"))
            cache.apply("session-1", JsonObject(template + mapOf("seq" to JsonPrimitive(1), "payload" to plan)))
            val row = SessionSummary.parse(obj("session_id" to "session-1", "head_seq" to 1, "worker_generation" to 1))
            repository.indexAll(listOf(row))
            assertFalse(repository.coverage.value.complete)
            assertEquals("unsupported_display_events", repository.coverage.value.error)
            val file = dir.listFiles()!!.single()
            assertTrue(file.renameTo(File(dir, file.name.replace("replay-v2", "replay-v1"))))
            assertEquals(0L, TranscriptCache(dir).lastApplied("session-1"))
            // A roster head rollback must not preserve/search a cursor from a lost future.
            repository.indexAll(listOf(row.copy(headSeq = 0)))
            assertEquals(0L, cache.lastApplied("session-1"))
            assertTrue(repository.transcript("session-1").isEmpty())
        } finally { repository.close(); client.close(); scope.coroutineContext[kotlinx.coroutines.Job]?.cancel(); dir.deleteRecursively() }
    }
}
