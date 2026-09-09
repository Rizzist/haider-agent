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
        for (name in listOf("wire_transcript.json", "peer_agent_injection_wire.json", "android_liveness_wire_v1.json")) {
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

    @Test fun rustPingPongGoldenUsesProductionEncodersIncludingUnsignedNonce() {
        json("android_liveness_wire_v1.json").jsonArray.forEach { entry ->
            val frame = RpcWire.parse(entry.jsonObject.string("ws_body"))
            val nonce = RpcWire.nonce(frame)
            assertEquals(frame, if (frame.string("kind") == "ping") RpcWire.ping(nonce) else RpcWire.pong(nonce))
            assertFalse(frame.containsKey("request_id"))
            assertFalse(frame.containsKey("body"))
        }
    }

    @Test fun everyCanonicalMobileSuccessUsesProductionProjection() {
        val bodies = mutableListOf<JsonObject>()
        for (name in listOf("wire_transcript.json", "peer_agent_injection_wire.json")) {
            json(name).jsonArray.map { RpcWire.parse(it.jsonObject.string("ws_body")) }
                .filter { it.string("kind") == "response" }.forEach { bodies += it.objectAt("body") }
        }
        for (name in listOf("provider_rebind_wire.json", "turn_retract_wire.json")) {
            json(name).jsonArray.map { it.jsonObject }.filter { it.string("kind") == "response" }
                .forEach { bodies += it.objectAt("body") }
        }
        bodies += json("client_contract_methods_v1.json").jsonObject.objects("methods").map { it.objectAt("response") }
        json("snapshot_availability_compat_v1.json").jsonObject.objects("pairs").forEach {
            bodies += it.objectAt("old"); bodies += it.objectAt("new")
        }
        val exercised = mutableSetOf<String>()
        for (body in bodies) {
            val method = body.string("method")
            when (method) {
                "session.list" -> RpcResponses.roster(body).also { page ->
                    assertEquals(body.objects("sessions").map { it.string("session_id") }, page.sessions.map { it.sessionId })
                    assertEquals(body.optionalString("next_cursor"), page.nextCursor)
                    page.sessions.forEach { row ->
                        assertEquals(row.sessionId, RpcUiMapping.row(row).id)
                    }
                }
                "session.list_watch", "account.list_watch" -> assertTrue(RpcResponses.watch(body))
                "session.observe" -> {
                    val digest = RpcResponses.observe(body)
                    val cut = RpcResponses.forkCut(digest)
                    assertEquals(body.objectAt("digest"), digest)
                    assertEquals(digest.string("session_id"), cut.session.sessionId)
                    assertEquals(digest.number("worker_generation"), cut.session.workerGeneration)
                    assertEquals(digest.optionalString("main_head_node_id"), cut.nodeId)
                    assertEquals(digest.number("main_head_seq"), cut.seq)
                }
                "session.attach" -> RpcResponses.attachment(body).also {
                    assertEquals(body.string("attachment_id"), it.id)
                    assertEquals(body.objectAt("attach_state").string("session_id"), it.sessionId)
                    assertEquals(body.objectAt("attach_state").number("replay_through_seq"), it.replayThrough)
                    assertEquals(body.objectAt("attach_state").number("worker_generation"), it.workerGeneration)
                }
                "session.detach" -> assertEquals(body.string("attachment_id"), RpcResponses.detached(body))
                "session.read" -> RpcResponses.read(body).also {
                    assertEquals(body.objectAt("result").string("session_id"), it.sessionId)
                    assertEquals(body.objectAt("result").number("head_seq"), it.headSeq)
                    assertEquals(body.objectAt("result").objects("envelopes"), it.envelopes)
                    it.envelopes.forEach { envelope -> TranscriptCache.displayProjection(envelope) }
                }
                "session.create", "session.fork", "session.rename", "session.seen", "session.select_model", "session.select_effort", "turn.submit", "turn.cancel" -> {
                    val receipt = RpcResponses.receipt(body)
                    assertEquals(body.string("session_id"), receipt.sessionId)
                    assertEquals(body.optionalNumber("worker_generation"), receipt.workerGeneration)
                    assertEquals(body.optionalString("run_id"), receipt.runId)
                    val sequence = listOf("created_seq", "renamed_seq", "seen_seq", "selected_seq", "accepted_seq").firstNotNullOfOrNull { body.optionalNumber(it) }
                    assertEquals(sequence, receipt.seq)
                }
                "menu.answer" -> assertEquals(body.number("resolution_seq"), RpcResponses.menuAnswer(body))
                "provider.list" -> {
                    if ((body["availability"] as? JsonObject)?.optionalString("state") == "unavailable") {
                        assertThrows(RpcRemoteException::class.java) { RpcResponses.providers(body) }
                    } else RpcResponses.providers(body).also { inventory ->
                        assertEquals(body.number("revision"), inventory.revision)
                        assertEquals(body.objects("providers").map { it.string("provider") }, inventory.providers.map { it.id })
                        body.objects("providers").zip(inventory.providers).forEach { (wire, row) ->
                            assertEquals(wire.strings("models").toList(), row.models)
                            assertEquals(wire.optionalString("default_model"), row.defaultModel)
                            assertEquals(wire.optionalString("api_family"), row.apiFamily)
                            assertEquals(wire.optionalString("availability_reason"), row.unavailableReason)
                            assertEquals("api_key" in wire.strings("auth_methods"), row.supportsApiKey)
                            assertEquals("oauth" in wire.strings("auth_methods"), row.supportsOAuth)
                            assertEquals(wire["enabled"] == JsonPrimitive(true) && wire.optionalString("availability") == "available", row.available)
                        }
                    }
                }
                "provider.models_refresh", "account.set_default_model" -> assertEquals(body.objectAt("provider").string("provider"), RpcResponses.refreshedProvider(body).id)
                "account.list" -> RpcResponses.accounts(body).also { snapshot ->
                    assertEquals(body.optionalNumber("revision"), snapshot.revision)
                    assertEquals(body.objects("descriptors").map { it.string("alias") }, snapshot.accounts.map { it.alias })
                }
                "account.login_api", "account.add", "account.set_active", "account.refresh", "account.set_label" -> RpcResponses.descriptor(body).also {
                    val descriptor = body.objectAt("descriptor")
                    assertEquals(descriptor.string("alias"), it.alias)
                    assertEquals(descriptor.string("provider"), it.provider)
                    assertEquals(descriptor.string("auth_method"), it.authKind)
                    assertEquals(descriptor.optionalString("identity"), it.identity)
                    assertEquals(descriptor.optionalString("label"), it.label)
                    assertEquals(descriptor.objectAt("status").string("status"), it.status)
                    assertEquals(descriptor["active"] == JsonPrimitive(true), it.active)
                }
                "account.remove" -> assertEquals(body.string("removed_alias"), RpcResponses.removed(body))
                "vault.stage" -> RpcResponses.stage(body).also {
                    assertEquals(body.string("vault_reference"), it.reference)
                    assertEquals(body.number("expires_at_ms"), it.expiresAtMs)
                    assertEquals("Stage(redacted)", it.toString())
                }
                "account.oauth_start" -> RpcResponses.oauthStart(body, "provider", "alias", "caller-owned-attempt", 42).also {
                    assertEquals(body.string("flow_id"), it.flowId)
                    assertEquals("caller-owned-attempt", it.attemptId)
                    assertEquals(42L, it.epoch)
                    assertEquals(body.optionalString("authorization_url"), it.authorizationUrl)
                    assertEquals(body.optionalNumber("expires_at_ms"), it.expiresAtMs)
                    assertEquals(body.optionalString("user_code"), it.userCode)
                }
                "account.oauth_status", "account.oauth_cancel" -> RpcResponses.oauthStatus(body.objectAt("status")).also {
                    assertEquals(body.objectAt("status").string("status"), it.status)
                    assertEquals(body.objectAt("status").optionalString("oauth_reference"), it.oauthReference)
                    assertEquals(body.objectAt("status").optionalString("identity"), it.identity)
                    RpcAccountsRepository.status(it)
                }
                else -> continue // Canonical methods outside the frozen mobile doors have no mobile projection.
            }
            exercised += method
        }
        assertEquals(setOf("session.list", "session.list_watch", "session.observe", "session.attach", "session.detach", "session.read",
            "session.create", "session.fork", "session.rename", "session.seen", "session.select_model", "session.select_effort", "turn.submit", "turn.cancel", "menu.answer",
            "provider.list", "provider.models_refresh", "account.list", "account.list_watch", "account.login_api", "account.add", "account.set_active", "account.refresh",
            "account.set_label", "account.set_default_model", "account.remove", "vault.stage", "account.oauth_start", "account.oauth_status", "account.oauth_cancel"), exercised)
    }

    @Test fun usedRequestEncodersMatchRustGoldens() {
        val requests = frames().filter { it.string("kind") == "request" }.associateBy { it.string("request_id") }
        fun check(id: String, actual: JsonObject) { assertEquals(id, requests.getValue(id).objectAt("body"), actual) }
        check("request-providers", RpcMethods.providers("openai"))
        check("request-create", RpcMethods.create("command-create", "/tmp/workspace", "anthropic", "claude-test", 4096))
        check("request-cancel", RpcMethods.cancel("command-cancel", SessionCoordinate("session-created", 7), "run-1"))
        check("request-session-fork", RpcMethods.fork("command-session-fork", SessionCoordinate("session-1", 7), "node-fork-2", 57, "Independent plan B", "branch-plan-b"))
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
            val messages = RpcUiMapping.messages(cache.entries("session-1"))
            assertEquals(listOf("Find the old conversation", "Found your conversation"), messages.map { it.text })
            assertEquals(listOf(ai.diffforge.haider.ui.chat.Role.User, ai.diffforge.haider.ui.chat.Role.Agent), messages.map { it.role })
            assertFalse(messages.last().streaming)
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
            // Any older projection version, whichever one is current: the point
            // is that a bump replays from zero, not which number it bumped to.
            assertTrue(file.renameTo(File(dir, file.name.replace(Regex("replay-v\\d+"), "replay-v1"))))
            assertEquals(0L, TranscriptCache(dir).lastApplied("session-1"))
            // A roster head rollback must not preserve/search a cursor from a lost future.
            repository.indexAll(listOf(row.copy(headSeq = 0)))
            assertEquals(0L, cache.lastApplied("session-1"))
            assertTrue(repository.transcript("session-1").isEmpty())
        } finally { repository.close(); client.close(); scope.coroutineContext[kotlinx.coroutines.Job]?.cancel(); dir.deleteRecursively() }
    }
}
