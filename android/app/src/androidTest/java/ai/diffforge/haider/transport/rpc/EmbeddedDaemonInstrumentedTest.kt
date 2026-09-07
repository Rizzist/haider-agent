package ai.diffforge.haider.transport.rpc

import android.content.*
import android.os.IBinder
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import ai.diffforge.haider.BuildConfig
import kotlinx.coroutines.*
import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.UUID

/** Requires the integrated lane-1/2 app and its deterministic fake-provider catalog. */
@RunWith(AndroidJUnit4::class)
class EmbeddedDaemonInstrumentedTest {
    @Test fun realBinderReadyUdsRosterAndFakeProviderTurn() = runBlocking<Unit> {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val context = instrumentation.targetContext
        val serviceType = Class.forName("ai.diffforge.haider.daemon.HaiderDaemonService")
        val aidl = Class.forName("ai.diffforge.haider.daemon.IHaiderDaemonService")
        val stub = Class.forName("ai.diffforge.haider.daemon.IHaiderDaemonService\$Stub")
        val connected = CompletableDeferred<Any>()
        val binding = object : ServiceConnection {
            override fun onServiceConnected(name: ComponentName, binder: IBinder) {
                connected.complete(requireNotNull(stub.getMethod("asInterface", IBinder::class.java).invoke(null, binder)))
            }
            override fun onServiceDisconnected(name: ComponentName) = Unit
        }
        val intent = Intent(context, serviceType).setPackage(context.packageName)
        assertTrue(context.bindService(intent, binding, Context.BIND_AUTO_CREATE))
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val client = RpcClient(scope)
        var control: Any? = null
        try {
            control = withTimeout(10_000) { connected.await() }
            // The harness has launched the real Activity; this is an explicit disposable-device user-start test.
            context.startForegroundService(intent)
            aidl.getMethod("startUserInitiated").invoke(control)
            var endpoint: Any? = null
            withTimeout(30_000) {
                while (endpoint == null) {
                    endpoint = aidl.getMethod("getRpcEndpoint").invoke(control)
                    if (endpoint == null) delay(100)
                }
            }
            val value = endpoint!!
            val type = value.javaClass
            val path = type.getMethod("getPath").invoke(value) as String
            val protocol = type.getMethod("getWireProtocol").invoke(value) as Int
            val generation = type.getMethod("getDaemonGeneration").invoke(value) as Long
            client.connect(RpcTarget(path, protocol, generation, BuildConfig.VERSION_NAME))
            assertEquals(JsonPrimitive(true), client.request(RpcMethods.watchSessions())["accepted"])
            val inventory = client.request(RpcMethods.providers()).objects("providers")
            val requestedProvider = InstrumentationRegistry.getArguments().getString("fakeProvider", "fake")!!
            val fake = inventory.firstOrNull { it.string("provider") == requestedProvider }
                ?: error("Deterministic fake-provider fixture is not installed; this is not live-provider authorization")
            val model = fake.strings("models").firstOrNull() ?: error("Fake provider has no model")
            val created = client.request(RpcMethods.create(UUID.randomUUID().toString(),
                File(context.filesDir, "haider/profiles/default/workspace").path, requestedProvider, model, 512))
            val session = created.string("session_id")
            val digest = client.request(RpcMethods.observe(session)).objectAt("digest")
            val cache = TranscriptCache(File(context.cacheDir, "integration-rpc-cache"))
            val replay = TranscriptRepository(client, scope, cache)
            try {
                replay.attach(session, control = true)
                val accepted = client.request(RpcMethods.submit(UUID.randomUUID().toString(),
                    SessionCoordinate(session, digest.number("worker_generation")), "Reply with the deterministic test completion."))
                assertEquals(session, accepted.string("session_id"))
                withTimeout(30_000) {
                    while (client.request(RpcMethods.observe(session)).objectAt("digest").optionalString("run_state") != "idle") delay(100)
                }
                assertTrue(client.request(RpcMethods.list()).objects("sessions").any { it.string("session_id") == session })
                withTimeout(10_000) { while (cache.lastApplied(session) < accepted.number("accepted_seq")) delay(100) }
                replay.detach(session)
            } finally { replay.close() }
        } finally {
            client.close(); scope.cancel()
            // Nightly harness keeps enabled intent across a real reboot only when explicitly requested.
            if (control != null && InstrumentationRegistry.getArguments().getString("keepEnabled") != "true")
                aidl.getMethod("stopAndDisable").invoke(control)
            context.unbindService(binding)
        }
    }
}
