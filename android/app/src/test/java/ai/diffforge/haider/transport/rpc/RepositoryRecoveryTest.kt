package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.*
import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import java.io.*
import java.nio.file.Files
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.atomic.AtomicInteger

/** Reproduce real daemon watch semantics and recover through actual framed request deadlines. */
class RepositoryRecoveryTest {
    private class Peer : RpcSocket {
        private val inputPipe = PipedInputStream(65536)
        val send = PipedOutputStream(inputPipe)
        val read = PipedInputStream(65536)
        private val outputPipe = PipedOutputStream(read)
        override val input: InputStream = inputPipe
        override val output: OutputStream = outputPipe
        override fun connect(path: String) = Unit
        override fun readTimeout(milliseconds: Int) = Unit
        override fun close() { inputPipe.close(); outputPipe.close(); send.close(); read.close() }
        val calls = ConcurrentHashMap<String, AtomicInteger>()
        fun count(method: String) = calls[method]?.get() ?: 0
    }

    @Test fun accountsRefreshKeepsOneWatchAndSurvivesDeadline() = exercise(accounts = true)
    @Test fun rosterRefreshKeepsOneWatchAndSurvivesDeadline() = exercise(accounts = false)

    private fun exercise(accounts: Boolean) = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val first = Peer()
        val second = Peer()
        val sockets = ConcurrentLinkedQueue(listOf(first, second))
        val client = RpcClient(scope, { sockets.remove() }, requestTimeoutMs = 500)
        val dir = Files.createTempDirectory("repository-recovery").toFile()
        val frames = javaClass.classLoader!!.getResourceAsStream("wire_transcript.json")!!.bufferedReader().use {
            wireJson.parseToJsonElement(it.readText()).jsonArray.map { row -> RpcWire.parse(row.jsonObject.string("ws_body")) }
        }
        val responses = frames.filter { it.string("kind") == "response" }.associateBy { it.string("request_id") }
        val watchMethod = if (accounts) "account.list_watch" else "session.list_watch"
        val listMethod = if (accounts) "account.list" else "session.list"
        fun serve(peer: Peer, ignoreFirstWatch: Boolean) = scope.launch {
            try {
                RpcWire.read(peer.read)!!
                val welcome = frames.first { it.string("kind") == "welcome" }
                RpcWire.write(peer.send, JsonObject(welcome + mapOf("profile_id" to JsonPrimitive("android-default"),
                    "capabilities_granted" to JsonArray(listOf("view", "control").map(::JsonPrimitive)))))
                while (isActive) {
                    val request = RpcWire.read(peer.read) ?: break
                    val method = request.objectAt("body").string("method")
                    val count = peer.calls.computeIfAbsent(method) { AtomicInteger() }.incrementAndGet()
                    if (ignoreFirstWatch && method == watchMethod) continue
                    val body = when (method) {
                        watchMethod -> obj("method" to method, "accepted" to (count == 1))
                        "provider.list" -> responses.getValue("request-providers").objectAt("body")
                        "account.list" -> responses.getValue("request-accounts-managed").objectAt("body")
                        "session.list" -> obj("method" to method, "sessions" to JsonArray(listOf(
                            obj("session_id" to "s", "head_seq" to count, "worker_generation" to 1))))
                        else -> error("Unexpected method $method")
                    }
                    RpcWire.write(peer.send, obj("v" to 1, "kind" to "response", "request_id" to request.string("request_id"), "body" to body))
                }
            } catch (_: IOException) { }
        }
        var accountRepository: AccountsRepository? = null
        var rosterRepository: SessionRosterRepository? = null
        val servers = mutableListOf<Job>()
        try {
            servers.add(serve(first, ignoreFirstWatch = true))
            val target = RpcTarget("/private/h.sock", 1, 4, "0.0.8")
            client.connect(target)
            if (accounts) accountRepository = AccountsRepository(client, scope)
            else rosterRepository = SessionRosterRepository(client, scope, TranscriptCache(dir))
            fun error() = accountRepository?.loadError?.value ?: rosterRepository?.loadError?.value
            withTimeout(4000) { while (error() == null) delay(10) }
            assertEquals(RpcConnectionState.DISCONNECTED, client.state.value)
            assertTrue(scope.isActive)
            servers.add(serve(second, ignoreFirstWatch = false))
            client.connect(target)
            withTimeout(4000) { while (second.count(listMethod) == 0 || error() != null) delay(10) }
            repeat(2) {
                accountRepository?.refresh()
                rosterRepository?.refreshRoster()
                assertNull(error())
            }
            assertEquals(1, second.count(watchMethod))
            assertEquals(3, second.count(listMethod))
            if (accounts) {
                RpcWire.write(second.send, obj("v" to 1, "kind" to "accounts_changed", "revision" to 100))
                withTimeout(4000) { while (second.count(listMethod) < 4) delay(10) }
                assertEquals(1, second.count(watchMethod))
            } else assertEquals(3L, rosterRepository!!.sessions.value.single().headSeq)
        } finally {
            accountRepository?.close(); rosterRepository?.close(); client.close()
            servers.forEach { it.cancel() }; scope.cancel(); dir.deleteRecursively()
        }
    }
}
