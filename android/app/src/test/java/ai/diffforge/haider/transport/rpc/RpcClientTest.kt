package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.*
import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import java.io.*
import java.nio.file.Files

/** Actual framed byte streams, including interleaved pushes and reordered correlated responses. */
class RpcClientTest {
    private class Peer : RpcSocket {
        private val inbound = PipedInputStream(65536)
        val send = PipedOutputStream(inbound)
        val read = PipedInputStream(65536)
        private val outbound = PipedOutputStream(read)
        override val input: InputStream = inbound
        override val output: OutputStream = outbound
        override fun connect(path: String) = Unit
        override fun readTimeout(milliseconds: Int) = Unit
        override fun close() { inbound.close(); outbound.close(); send.close(); read.close() }
        fun receive() = RpcWire.read(read)!!
        fun reply(request: JsonObject, body: JsonObject) = RpcWire.write(send, obj("v" to 1, "kind" to "response", "request_id" to request.string("request_id"), "body" to body))
        fun handshake() {
            val hello = receive()
            assertEquals("hello", hello.string("kind"))
            assertEquals(setOf("json"), hello.strings("encodings"))
            RpcWire.write(send, welcome())
        }
    }
    companion object {
        private val target = RpcTarget("/private/h.sock", 1, 4, "0.0.971")
        private fun welcome() = obj("v" to 1, "kind" to "welcome", "protocol" to 1, "instance_id" to "test-instance",
            "daemon_generation" to 4, "frame_limit" to 1048576, "profile_id" to "android-default", "daemon_version" to "0.0.971",
            "lifecycle_phase" to "ready", "capabilities_granted" to JsonArray(listOf("view", "control").map(::JsonPrimitive)))
    }

    @Test fun correlationPushesTopLevelMenuAndTimeout() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val peer = Peer()
        val client = RpcClient(scope, { peer }, 300)
        val push = CompletableDeferred<JsonObject>()
        val subscription = client.observeFrames { push.complete(it) }
        val server = scope.launch {
            peer.handshake()
            val first = peer.receive()
            val second = peer.receive()
            RpcWire.write(peer.send, obj("v" to 1, "kind" to "accounts_changed", "revision" to 8))
            peer.reply(second, obj("method" to second.objectAt("body").string("method"), "marker" to "second"))
            peer.reply(first, obj("method" to first.objectAt("body").string("method"), "marker" to "first"))
            val answer = peer.receive()
            assertEquals("menu_answer", answer.string("kind")); assertFalse(answer.containsKey("body"))
            peer.reply(answer, obj("method" to "error", "code" to "already_resolved", "message" to "sensitive ignored", "retryable" to false))
            peer.receive() // Deliberately leave one response unanswered.
        }
        try {
            client.connect(target)
            val first = async { client.request(RpcMethods.accounts()) }
            val second = async { client.request(RpcMethods.providers()) }
            assertEquals(setOf("first", "second"), setOf(first.await().string("marker"), second.await().string("marker")))
            assertEquals("accounts_changed", withTimeout(1000) { push.await() }.string("kind"))
            try { client.menuAnswer(RpcMethods.menu("command", MenuCoordinate(SessionCoordinate("s", 1), "m", 1, "ok", 0))); fail() }
            catch (error: RpcRemoteException) { assertEquals("already_resolved", error.code); assertFalse(error.toString().contains("sensitive")) }
            try { client.request(RpcMethods.accounts()); fail() } catch (_: IOException) { }
            server.join()
        } finally { subscription.close(); client.close(); scope.cancel() }
    }

    @Test fun blockedWritesAreInterruptedByDeadline() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val peer = Peer()
        val client = RpcClient(scope, { peer }, 300)
        val server = scope.launch { peer.handshake(); delay(10_000) }
        try {
            client.connect(target)
            val start = System.nanoTime()
            try {
                client.request(obj("method" to "session.rename", "title" to "x".repeat(500_000)))
                fail("unread pipe must block until closed")
            } catch (_: IOException) { }
            assertTrue("blocking stream deadline must close the socket", (System.nanoTime() - start) / 1_000_000 < 2500)
            assertEquals(RpcConnectionState.DISCONNECTED, client.state.value)
        } finally { client.close(); server.cancel(); scope.cancel() }
    }

    @Test fun callerDeadlineRemainsCancellation() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val peer = Peer()
        val client = RpcClient(scope, { peer }, 5000)
        val server = scope.launch { peer.handshake(); delay(10_000) }
        try {
            client.connect(target)
            try { withTimeout(100) { client.request(RpcMethods.accounts()) }; fail() }
            catch (_: TimeoutCancellationException) { }
            assertEquals(RpcConnectionState.DISCONNECTED, client.state.value)
        } finally { client.close(); server.cancel(); scope.cancel() }
    }

    @Test fun transcriptIgnoresStaleAttachmentBeforeImmediateCurrentEvent() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined)
        val peer = Peer()
        val client = RpcClient(scope, { peer })
        val dir = Files.createTempDirectory("attachment-correlation").toFile()
        val repository = TranscriptRepository(client, scope, TranscriptCache(dir))
        val frames = javaClass.classLoader!!.getResourceAsStream("wire_transcript.json")!!.bufferedReader().use {
            wireJson.parseToJsonElement(it.readText()).jsonArray.map { row -> RpcWire.parse(row.jsonObject.string("ws_body")) }
        }
        val event = frames.first { it.string("kind") == "event" }
        val server = scope.launch(Dispatchers.IO) {
            peer.handshake()
            val attach = peer.receive()
            peer.reply(attach, obj("method" to "session.attach", "attachment_id" to "current", "attach_state" to obj("session_id" to "session-1")))
            for (id in listOf("stale", "current")) RpcWire.write(peer.send, JsonObject(event + mapOf(
                "attachment_id" to JsonPrimitive(id), "envelope" to JsonObject(event.objectAt("envelope") + mapOf(
                    "seq" to JsonPrimitive(1), "payload" to obj("type" to "user_message", "text" to id))))))
            delay(10_000)
        }
        try {
            client.connect(target)
            repository.attach("session-1")
            withTimeout(3000) { while (repository.transcript("session-1").isEmpty()) delay(10) }
            assertEquals("current", repository.transcript("session-1").single().display.string("text"))
        } finally { repository.close(); client.close(); server.cancel(); scope.cancel(); dir.deleteRecursively() }
    }

    @Test fun responseHookRegistersAttachmentBeforeImmediatePush() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val peer = Peer()
        val client = RpcClient(scope, { peer })
        val registered = java.util.concurrent.atomic.AtomicBoolean(false)
        val push = CompletableDeferred<Boolean>()
        val listener = client.observeFrames { push.complete(registered.get()) }
        val server = scope.launch {
            peer.handshake()
            val request = peer.receive()
            peer.reply(request, obj("method" to "session.attach", "attachment_id" to "a"))
            RpcWire.write(peer.send, obj("v" to 1, "kind" to "attach_caught_up", "attachment_id" to "a", "high_water_seq" to 8))
            delay(1000)
        }
        try {
            client.connect(target)
            client.request(RpcMethods.attach("s"), onResponse = { registered.set(true) })
            assertTrue(withTimeout(2000) { push.await() })
        } finally { listener.close(); client.close(); server.cancel(); scope.cancel() }
    }

    @Test fun disconnectFencesConnectionOwnedReferencesAndWrongVersion() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val peer = Peer()
        val client = RpcClient(scope, { peer })
        val server = scope.launch { peer.handshake(); delay(10_000) }
        try {
            client.connect(target)
            val epoch = client.connectionEpoch
            client.close()
            assertEquals(RpcConnectionState.DISCONNECTED, client.state.value)
            assertNotEquals(epoch, client.connectionEpoch)
            try { client.request(RpcMethods.oauthStatus("synthetic-flow", "attempt"), epoch); fail() } catch (_: IOException) { }
            assertThrows(RpcProtocolException::class.java) {
                RpcClient.validateWelcome(JsonObject(welcome() + ("daemon_version" to JsonPrimitive("wrong"))), target, true)
            }
            assertThrows(RpcProtocolException::class.java) {
                RpcClient.validateWelcome(JsonObject(welcome() + ("encoding" to JsonPrimitive("msgpack"))), target, true)
            }
        } finally { client.close(); server.cancel(); scope.cancel() }
    }

    @Test fun rosterBuffersWatchBeforePaginatedBaselineAndKeepsNewerHead() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val peer = Peer()
        val client = RpcClient(scope, { peer })
        val dir = Files.createTempDirectory("roster").toFile()
        val cache = TranscriptCache(dir)
        fun summary(id: String, head: Int, provider: String? = null) = obj("session_id" to id, "head_seq" to head, "worker_generation" to 1, "provider" to provider)
        val server = scope.launch {
            peer.handshake()
            val watch = peer.receive(); assertEquals("session.list_watch", watch.objectAt("body").string("method"))
            peer.reply(watch, obj("method" to "session.list_watch", "accepted" to true))
            val first = peer.receive()
            RpcWire.write(peer.send, obj("v" to 1, "kind" to "session_roster_delta", "summaries" to JsonArray(listOf(summary("old", 8, "authoritative")))))
            peer.reply(first, obj("method" to "session.list", "sessions" to JsonArray(listOf(summary("old", 3))), "next_cursor" to "opaque-next"))
            val second = peer.receive(); assertEquals("opaque-next", second.objectAt("body").string("cursor"))
            peer.reply(second, obj("method" to "session.list", "sessions" to JsonArray(listOf(summary("older-idle", 2)))))
            delay(10_000)
        }
        var roster: SessionRosterRepository? = null
        try {
            client.connect(target)
            roster = SessionRosterRepository(client, scope, cache)
            withTimeout(3000) { while (roster.sessions.value.size != 2) delay(10) }
            assertEquals(8L, roster.sessions.value.first { it.sessionId == "old" }.headSeq)
            assertEquals("authoritative", roster.sessions.value.first { it.sessionId == "old" }.provider)
            assertNull(roster.sessions.value.first { it.sessionId == "older-idle" }.provider)
            client.close()
            assertEquals(2, TranscriptCache(dir).loadRoster().size)
        } finally { roster?.close(); client.close(); server.cancel(); scope.cancel(); dir.deleteRecursively() }
    }
}
