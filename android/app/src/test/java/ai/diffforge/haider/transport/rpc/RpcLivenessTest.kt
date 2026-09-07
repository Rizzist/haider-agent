package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import java.io.IOException
import java.net.ServerSocket
import java.net.Socket
import java.net.SocketTimeoutException
import java.util.concurrent.atomic.AtomicInteger

/** A real framed stream fake daemon with the production daemon's 45 s read-idle retirement. */
class RpcLivenessTest {
    private class NetworkSocket(private val port: Int) : RpcSocket {
        private val socket = Socket()
        override val input get() = socket.getInputStream()
        override val output get() = socket.getOutputStream()
        override fun connect(path: String) = socket.connect(java.net.InetSocketAddress("127.0.0.1", port))
        override fun readTimeout(milliseconds: Int) { socket.soTimeout = milliseconds }
        override fun close() = socket.close()
    }
    private val target = RpcTarget("/test/h.sock", 1, 1, "0.0.971")
    private fun welcome() = obj("v" to 1, "kind" to "welcome", "protocol" to 1, "instance_id" to "idle-test",
        "daemon_generation" to 1, "daemon_version" to target.appVersion, "profile_id" to "android-default",
        "frame_limit" to RpcWire.MAX_BODY, "capabilities_granted" to JsonArray(listOf("view", "control").map(::JsonPrimitive)))
    private fun handshake(peer: Socket) {
        assertEquals("hello", RpcWire.read(peer.getInputStream())!!.string("kind"))
        RpcWire.write(peer.getOutputStream(), welcome())
    }

    @Test(timeout = 65_000) fun idleWatchAndOAuthConnectionSurvivesDaemon45SecondRetirement() = runBlocking<Unit> {
        val listener = ServerSocket(0, 1, java.net.InetAddress.getLoopbackAddress())
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val client = RpcClient(owner, { NetworkSocket(listener.localPort) })
        val probes = AtomicInteger()
        val retired = CompletableDeferred<Unit>()
        val server = owner.async {
            listener.accept().use { peer ->
                peer.soTimeout = 45_000 // connection.rs READ_IDLE_DEADLINE, not the daemon's profile idle TTL.
                handshake(peer)
                try {
                    while (true) {
                        val frame = RpcWire.read(peer.getInputStream()) ?: break
                        when (frame.string("kind")) {
                            "ping" -> { probes.incrementAndGet(); RpcWire.write(peer.getOutputStream(), RpcWire.pong(RpcWire.nonce(frame))) }
                            "request" -> RpcWire.write(peer.getOutputStream(), obj("v" to 1, "kind" to "response",
                                "request_id" to frame.string("request_id"), "body" to obj("method" to "account.oauth_status", "status" to obj("status" to "waiting_browser"))))
                            else -> error("unexpected client liveness")
                        }
                    }
                } catch (_: SocketTimeoutException) { retired.complete(Unit) }
            }
        }
        try {
            client.connect(target)
            val epoch = client.connectionEpoch
            delay(46_000) // No UI calls; a watch/browser consent keeps this original connection.
            assertFalse("daemon retired the idle connection", retired.isCompleted)
            assertTrue("client sent no periodic top-level Ping", probes.get() >= 2)
            assertEquals(epoch, client.connectionEpoch)
            assertEquals("waiting_browser", client.request(RpcMethods.oauthStatus("synthetic-flow", "attempt"), epoch).objectAt("status").string("status"))
        } finally { client.close(); listener.close(); server.cancel(); owner.cancel() }
    }

    @Test fun pingsAndPongsDoNotRenewOneRequestDeadline() = runBlocking<Unit> {
        val listener = ServerSocket(0)
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val client = RpcClient(owner, { NetworkSocket(listener.localPort) }, requestTimeoutMs = 500,
            heartbeatIntervalMs = 40, heartbeatTimeoutMs = 2000)
        val echoed = CompletableDeferred<Unit>()
        val server = owner.launch {
            listener.accept().use { peer ->
                handshake(peer)
                val nonce = wireJson.parseToJsonElement("18446744073709551615").jsonPrimitive
                RpcWire.write(peer.getOutputStream(), RpcWire.ping(nonce))
                try {
                    while (isActive) {
                        val frame = RpcWire.read(peer.getInputStream()) ?: break
                        if (frame.string("kind") == "pong") { assertEquals(nonce, RpcWire.nonce(frame)); echoed.complete(Unit) }
                        if (frame.string("kind") == "ping") RpcWire.write(peer.getOutputStream(), RpcWire.pong(RpcWire.nonce(frame)))
                        // The business request deliberately never completes; Ping/Pong must not restart its deadline.
                    }
                } catch (_: IOException) { }
            }
        }
        try {
            client.connect(target)
            withTimeout(2000) { echoed.await() }
            val started = System.nanoTime()
            try { withTimeout(1500) { client.request(RpcMethods.accounts()) }; fail("deadline missing") }
            catch (error: IOException) { assertEquals("request_timeout", error.message) }
            assertTrue((System.nanoTime() - started) / 1_000_000 < 1500)
            assertEquals(RpcConnectionState.DISCONNECTED, client.state.value)
        } finally { client.close(); listener.close(); server.cancel(); owner.cancel() }
    }

    @Test fun wrongPongRetiresItsEpochAndCannotStopTheReplacement() = runBlocking<Unit> {
        val listener = ServerSocket(0)
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val client = RpcClient(owner, { NetworkSocket(listener.localPort) }, heartbeatIntervalMs = 40, heartbeatTimeoutMs = 150)
        val server = owner.launch {
            repeat(2) { index -> listener.accept().use { peer ->
                handshake(peer)
                try {
                    while (isActive) {
                        val frame = RpcWire.read(peer.getInputStream()) ?: break
                        if (frame.string("kind") == "ping") RpcWire.write(peer.getOutputStream(),
                            RpcWire.pong(if (index == 0) JsonPrimitive(999) else RpcWire.nonce(frame)))
                    }
                } catch (_: IOException) { }
            } }
        }
        try {
            client.connect(target)
            val oldEpoch = client.connectionEpoch
            withTimeout(2000) { client.state.first { it == RpcConnectionState.DISCONNECTED } }
            assertNotEquals(oldEpoch, client.connectionEpoch)
            client.connect(target)
            val newEpoch = client.connectionEpoch
            delay(400)
            assertEquals(RpcConnectionState.CONNECTED, client.state.value)
            assertEquals(newEpoch, client.connectionEpoch)
        } finally { client.close(); listener.close(); server.cancel(); owner.cancel() }
    }

    @Test fun clampsValidUnsignedDaemonCeilingsAndRejectsMalformedLimits() {
        for (limit in listOf(1L, RpcWire.MAX_BODY.toLong(), 48L * 1024 * 1024, 0xffff_ffffL)) {
            val granted = RpcClient.validateWelcome(JsonObject(welcome() + ("frame_limit" to JsonPrimitive(limit))), target, true)
            assertEquals(minOf(limit, RpcWire.MAX_BODY.toLong()).toInt(), granted.frameLimit)
        }
        for (limit in listOf(JsonPrimitive(0), JsonPrimitive(-1), JsonPrimitive(0x1_0000_0000L), JsonPrimitive(1.5), JsonPrimitive("8388608"))) {
            assertThrows(RpcProtocolException::class.java) { RpcClient.validateWelcome(JsonObject(welcome() + ("frame_limit" to limit)), target, true) }
        }
    }
}
