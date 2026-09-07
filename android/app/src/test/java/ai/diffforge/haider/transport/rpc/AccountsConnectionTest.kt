package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.*
import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import java.io.*

class AccountsConnectionTest {
    private class GoldenSocket : RpcSocket {
        private val received = PipedInputStream(65536)
        val serverOutput = PipedOutputStream(received)
        val serverInput = PipedInputStream(65536)
        private val sent = PipedOutputStream(serverInput)
        override val input: InputStream = received
        override val output: OutputStream = sent
        override fun connect(path: String) = Unit
        override fun readTimeout(milliseconds: Int) = Unit
        override fun close() { received.close(); serverOutput.close(); serverInput.close(); sent.close() }
    }

    @Test fun stagedApiKeyAndOAuthUseOneConnectionAndClearTransientInput() = runBlocking<Unit> {
        val fixtures = javaClass.classLoader!!.getResourceAsStream("wire_transcript.json")!!.bufferedReader().use { it.readText() }
        val frames = wireJson.parseToJsonElement(fixtures).jsonArray.map { RpcWire.parse(it.jsonObject.string("ws_body")) }
        val responses = frames.filter { it.string("kind") == "response" }.associateBy { it.string("request_id") }
        val one = GoldenSocket()
        val two = GoldenSocket()
        val sockets = java.util.concurrent.ConcurrentLinkedQueue(listOf(one, two))
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val client = RpcClient(scope, { sockets.remove() })
        val methods = java.util.concurrent.CopyOnWriteArrayList<String>()
        fun serve(socket: GoldenSocket) = scope.launch {
            try {
                RpcWire.read(socket.serverInput)!!
                val original = frames.first { it.string("kind") == "welcome" }
                RpcWire.write(socket.serverOutput, JsonObject(original + mapOf("profile_id" to JsonPrimitive("android-default"),
                    "capabilities_granted" to JsonArray(listOf("view", "control").map(::JsonPrimitive)))))
                while (isActive) {
                    val request = RpcWire.read(socket.serverInput) ?: break
                    val method = request.objectAt("body").string("method")
                    methods.add(method)
                    val body = when (method) {
                        "account.list_watch" -> obj("method" to method, "accepted" to true)
                        "account.list" -> responses.getValue("request-accounts-managed").objectAt("body")
                        "provider.list" -> responses.getValue("request-providers").objectAt("body")
                        "vault.stage" -> responses.getValue("request-stage").objectAt("body")
                        "account.login_api" -> responses.getValue("request-login").objectAt("body")
                        "account.oauth_start" -> responses.getValue("request-oauth-start").objectAt("body")
                        "account.oauth_status" -> responses.getValue("request-oauth-status").objectAt("body")
                        "account.add" -> responses.getValue("request-account-add").objectAt("body")
                        else -> error("Unexpected account method $method")
                    }
                    RpcWire.write(socket.serverOutput, obj("v" to 1, "kind" to "response", "request_id" to request.string("request_id"), "body" to body))
                }
            } catch (_: IOException) { }
        }
        val firstServer = serve(one)
        var repository: AccountsRepository? = null
        try {
            val target = RpcTarget("/private/h.sock", 1, 4, "0.0.8")
            client.connect(target)
            val accounts = AccountsRepository(client, scope)
            repository = accounts
            val key = "synthetic-key-only".toCharArray()
            val added = accounts.addApiKey("anthropic", "work", key, "durable-command")
            assertEquals("api_key", added.authKind)
            assertTrue(key.all { it == '\u0000' })
            assertTrue(methods.indexOf("vault.stage") < methods.indexOf("account.login_api"))
            val validate = "synthetic-validate".toCharArray()
            try { accounts.validateApiKey("anthropic", validate); fail() }
            catch (error: RpcRemoteException) { assertEquals("validate_only_unavailable", error.code) }
            assertTrue(validate.all { it == '\u0000' })
            val flow = accounts.startOAuth("fake-oauth", "work-oauth")
            assertFalse(flow.toString().contains("golden"))
            val status = accounts.pollOAuth(flow)
            assertEquals("ready", status.status)
            assertEquals("oauth", accounts.completeOAuth(flow, status.oauthReference!!, "oauth-command").authKind)
            accounts.close()
            client.close()
            val secondServer = serve(two)
            client.connect(target)
            assertEquals(RpcConnectionState.CONNECTED, client.state.value)
            val callsBefore = methods.size
            try { accounts.pollOAuth(flow); fail("an old flow must not move to the replacement connection") }
            catch (_: IOException) { }
            assertEquals(callsBefore, methods.size)
            secondServer.cancel()
        } finally { repository?.close(); client.close(); firstServer.cancel(); scope.cancel() }
    }
}
