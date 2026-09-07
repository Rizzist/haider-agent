package ai.diffforge.haider.daemon

import android.app.NotificationManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.IBinder
import android.os.Parcel
import android.os.SystemClock
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Real service/JNI tests for a DISPOSABLE installed fixture APK. Missing .so is a failure, never skip.
 * Run on API 35 x86_64 CI and API 29/34/35 ARM64 + API 35 16 KiB on the mini.
 * SIGKILL, reboot and in-place replacement require the external shell matrix described in the report.
 */
@RunWith(AndroidJUnit4::class)
class HaiderDaemonInstrumentedTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context get() = instrumentation.targetContext
    private lateinit var remote: IHaiderDaemonService
    private var bound = false
    private val connected = CountDownLatch(1)
    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            remote = IHaiderDaemonService.Stub.asInterface(binder)
            connected.countDown()
        }
        override fun onServiceDisconnected(name: ComponentName) { }
    }
    @Before fun visibleUserStart() {
        instrumentation.startActivitySync(Intent().setClassName(context.packageName, "ai.diffforge.haider.MainActivity")
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        context.startForegroundService(HaiderDaemonService.userStartIntent(context))
        bound = context.bindService(Intent(context, HaiderDaemonService::class.java), connection, Context.BIND_AUTO_CREATE)
        assertTrue(bound)
        assertTrue("Binder connection", connected.await(10, TimeUnit.SECONDS))
    }
    @After fun stopFixture() {
        if (::remote.isInitialized) {
            remote.stopAndDisable()
            awaitPhase("DISABLED")
        }
        if (bound) context.unbindService(connection)
    }
    @Test fun realNativeReadyForegroundSnapshotAndParcelable() {
        val ready = awaitPhase("READY")
        assertTrue(ready.enabled)
        assertTrue(ready.daemonGeneration > 0)
        assertEquals(ready.appVersion, ready.nativeVersion)
        assertEquals(1, ready.wireProtocol)
        assertTrue(ready.rpcEndpoint!!.path.endsWith("/h.sock"))
        assertTrue(File(ready.rpcEndpoint!!.path).exists())
        assertNotNull(ready.startedAtElapsedRealtimeMs)
        val parcel = Parcel.obtain()
        try {
            ready.writeToParcel(parcel, 0); parcel.setDataPosition(0)
            assertEquals(ready, DaemonServiceSnapshot.CREATOR.createFromParcel(parcel))
        } finally { parcel.recycle() }
        val manager = context.getSystemService(NotificationManager::class.java)
        assertNotNull(manager.getNotificationChannel(DaemonNotifications.STATUS_CHANNEL))
        // With POST_NOTIFICATIONS granted in the fixture, the actual FGS notification is inspectable.
        if (manager.areNotificationsEnabled()) {
            assertTrue(manager.activeNotifications.any { it.id == DaemonNotifications.STATUS_ID })
        }
    }
    @Test fun fullSnapshotListenerAndNotificationStopDrain() {
        val ready = awaitPhase("READY")
        val initial = CountDownLatch(1)
        val listener = object : IDaemonSnapshotListener.Stub() {
            override fun onSnapshot(snapshot: DaemonServiceSnapshot) { initial.countDown() }
        }
        remote.registerListener(listener)
        assertTrue(initial.await(5, TimeUnit.SECONDS))
        remote.unregisterListener(listener)
        DaemonIntents.service(context, DaemonIntents.STOP_DAEMON).send()
        val stopped = awaitPhase("DISABLED")
        assertFalse(stopped.enabled)
        assertNull(stopped.rpcEndpoint)
        assertNull(remote.rpcEndpoint)
        assertTrue(stopped.snapshotSeq > ready.snapshotSeq)
        assertFalse(File(ready.rpcEndpoint!!.path).exists())
    }
    @Test fun unbindingDoesNotOwnDaemonLifetime() {
        val ready = awaitPhase("READY")
        context.unbindService(connection); bound = false
        SystemClock.sleep(1_000)
        val latch = CountDownLatch(1)
        var rebound: IHaiderDaemonService? = null
        val other = object : ServiceConnection {
            override fun onServiceConnected(name: ComponentName, binder: IBinder) {
                rebound = IHaiderDaemonService.Stub.asInterface(binder); latch.countDown()
            }
            override fun onServiceDisconnected(name: ComponentName) { }
        }
        assertTrue(context.bindService(Intent(context, HaiderDaemonService::class.java), other, 0))
        try {
            assertTrue(latch.await(5, TimeUnit.SECONDS))
            assertEquals("READY", rebound!!.snapshot.phase)
            assertEquals(ready.daemonGeneration, rebound!!.snapshot.daemonGeneration)
        } finally { context.unbindService(other) }
    }
    private fun awaitPhase(phase: String): DaemonServiceSnapshot {
        val deadline = SystemClock.elapsedRealtime() + 130_000
        while (SystemClock.elapsedRealtime() < deadline) {
            val snapshot = remote.snapshot
            if (snapshot.phase == phase) return snapshot
            if (snapshot.phase == "ERROR" && phase == "READY") fail("Native startup failed: ${snapshot.errorCode}")
            SystemClock.sleep(100)
        }
        throw AssertionError("Expected $phase; actual ${remote.snapshot.phase}")
    }
}
