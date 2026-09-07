package ai.diffforge.haider.daemon

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.Bundle
import android.os.IBinder
import android.os.SystemClock
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Read-only probes for the external reboot/SIGKILL/replacement matrix. These do NOT start native. */
@RunWith(AndroidJUnit4::class)
class DaemonRecoveryProbeTest {
    @Test fun assertAlreadyRunning() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val context = instrumentation.targetContext
        val connected = CountDownLatch(1)
        var daemon: IHaiderDaemonService? = null
        val connection = object : ServiceConnection {
            override fun onServiceConnected(name: ComponentName, binder: IBinder) {
                daemon = IHaiderDaemonService.Stub.asInterface(binder); connected.countDown()
            }
            override fun onServiceDisconnected(name: ComponentName) { }
        }
        // Flags=0 is essential: a passing test must not create the service it is supposed to observe.
        val bound = context.bindService(Intent(context, HaiderDaemonService::class.java), connection, 0)
        assertTrue("Daemon should already be started by Android", bound)
        try {
            assertTrue(connected.await(10, TimeUnit.SECONDS))
            val deadline = SystemClock.elapsedRealtime() + 130_000
            while (daemon!!.snapshot.phase != "READY" && SystemClock.elapsedRealtime() < deadline) {
                assertNotEquals("ERROR", daemon!!.snapshot.phase)
                SystemClock.sleep(100)
            }
            val snapshot = daemon!!.snapshot
            assertEquals("READY", snapshot.phase)
            assertTrue(snapshot.enabled)
            assertNotNull(snapshot.rpcEndpoint)
            instrumentation.sendStatus(0, Bundle().apply {
                putString("stream", "Ready generation=${snapshot.daemonGeneration} seq=${snapshot.snapshotSeq}\n")
            })
        } finally { context.unbindService(connection) }
    }
    @Test fun assertDisabledState() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val state = FileDaemonLifecycleStore(context).load()
        assertFalse("User Stop must survive reboot/replacement", state.enabled)
        assertFalse(state.active)
        assertNull(state.updateUntilUnixMs)
    }
}
