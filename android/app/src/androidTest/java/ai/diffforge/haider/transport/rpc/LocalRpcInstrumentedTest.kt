package ai.diffforge.haider.transport.rpc

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.net.LocalSocketAddress
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import ai.diffforge.haider.BuildConfig
import kotlinx.coroutines.*
import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class LocalRpcInstrumentedTest {
    @Test fun packagedNativeVersionMatchesApkAndContract() {
        // Reflection preserves lane ownership: lane 2 supplies the actual kept JNI class.
        System.loadLibrary("haider")
        val native = Class.forName("ai.diffforge.haider.daemon.NativeDaemon")
        val value = native.getDeclaredMethod("nativeVersion").invoke(null) as String
        val metadata = wireJson.parseToJsonElement(value).jsonObject
        assertEquals(BuildConfig.VERSION_NAME, metadata.string("daemon_version"))
        assertEquals(1L, metadata.number("jni_version"))
        assertEquals(1L, metadata.number("wire_protocol"))
        assertTrue(metadata.string("build_id").matches(Regex("[0-9a-f]{64}")))
        InstrumentationRegistry.getArguments().getString("expectedBuildId")?.let {
            assertEquals(it, metadata.string("build_id"))
        }
        assertEquals(android.os.Build.SUPPORTED_ABIS.first(), metadata.string("abi"))
        android.util.Log.i("HaiderNativeVerification", value)
    }

    @Test fun filesystemLocalSocketAuthenticatesPeerAndExchangesRustGolden() = runBlocking<Unit> {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val directory = File(context.cacheDir, "rpc-instrumentation").apply { mkdirs() }
        val path = File(directory, "h.sock")
        check(!path.exists()) { "test endpoint already exists" }
        val bound = LocalSocket()
        bound.bind(LocalSocketAddress(path.path, LocalSocketAddress.Namespace.FILESYSTEM))
        val server = LocalServerSocket(bound.fileDescriptor)
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val client = RpcClient(scope)
        val serving = scope.launch {
            server.accept().use { socket ->
                assertEquals(android.os.Process.myUid(), socket.peerCredentials.uid)
                val hello = RpcWire.read(socket.inputStream)!!
                assertEquals("hello", hello.string("kind"))
                RpcWire.write(socket.outputStream, obj("v" to 1, "kind" to "welcome", "protocol" to 1,
                    "daemon_generation" to 1, "instance_id" to "instrumentation", "profile_id" to "android-default",
                    "daemon_version" to BuildConfig.VERSION_NAME, "frame_limit" to RpcWire.MAX_BODY,
                    "lifecycle_phase" to "ready", "capabilities_granted" to JsonArray(listOf(JsonPrimitive("view")))))
                val request = RpcWire.read(socket.inputStream)!!
                val fixtures = contextForTests().assets.open("client_contract_methods_v1.json").bufferedReader().use { it.readText() }
                val response = wireJson.parseToJsonElement(fixtures).jsonObject.objects("methods")
                    .first { it.string("request_method") == "session.list_watch" }.objectAt("response")
                assertEquals("session.list_watch", request.objectAt("body").string("method"))
                RpcWire.write(socket.outputStream, obj("v" to 1, "kind" to "response", "request_id" to request.string("request_id"), "body" to response))
                delay(100)
            }
        }
        try {
            client.connect(RpcTarget(path.path, 1, 1, BuildConfig.VERSION_NAME), control = false)
            assertEquals(JsonPrimitive(true), client.request(RpcMethods.watchSessions())["accepted"])
            serving.join()
        } finally { client.close(); scope.cancel(); server.close(); bound.close(); path.delete(); directory.delete() }
    }
    private fun contextForTests() = InstrumentationRegistry.getInstrumentation().context
}
