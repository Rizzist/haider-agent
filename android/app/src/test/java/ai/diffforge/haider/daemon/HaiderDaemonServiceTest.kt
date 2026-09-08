package ai.diffforge.haider.daemon

import android.content.Intent
import android.os.Looper
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowBinder
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

internal class TestHaiderDaemonService : HaiderDaemonService() {
    val fake = FakeNativeDaemonHost()
    val state = FakeLifecycleStore()
    var promotedBeforeVersion = false
    var beforePaths: () -> Unit = {}
    var beforeSave: () -> Unit = {}
    override fun createPaths(): DaemonPaths {
        beforePaths()
        return DaemonPaths("{}", "/private/runtime/h.sock", File("/unused-vault"))
    }
    override fun createLifecycleStore(): DaemonLifecycleStore = object : DaemonLifecycleStore {
        override fun load() = state.load()
        override fun save(next: PersistedLifecycle) { beforeSave(); state.save(next) }
    }
    override fun createVault(paths: DaemonPaths) = VaultDekProvider { use ->
        val dek = ByteArray(32) { 7 }
        try { use(dek) } finally { dek.fill(0) }
    }
    override fun createNativeHost(): NativeDaemonHost = object : NativeDaemonHost by fake {
        override fun version(): String {
            promotedBeforeVersion = shadowOf(this@TestHaiderDaemonService).lastForegroundNotification != null
            val appVersion = packageManager.getPackageInfo(packageName, 0).versionName!!
            return fake.version().replace("0.0.970", appVersion)
        }
    }
}

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class HaiderDaemonServiceTest {
    @Test fun coldStartCannotBeStoppedByInitializationWhileItsOwnerActionIsPending() {
        val controller = Robolectric.buildService(TestHaiderDaemonService::class.java)
        val service = controller.get()
        val initialize = CountDownLatch(1)
        val savingStart = CountDownLatch(1)
        val completeStart = CountDownLatch(1)
        service.beforePaths = { check(initialize.await(5, TimeUnit.SECONDS)) }
        service.beforeSave = {
            savingStart.countDown()
            check(completeStart.await(5, TimeUnit.SECONDS))
        }
        controller.create()
        try {
            service.onStartCommand(HaiderDaemonService.userStartIntent(service), 0, 1)
            initialize.countDown()
            assertTrue(savingStart.await(5, TimeUnit.SECONDS))
            // Restore has published DISABLED and posted teardown; STARTING cannot publish yet.
            shadowOf(Looper.getMainLooper()).idle()
            assertFalse("Initialization stopped a pending cold start", shadowOf(service).isStoppedBySelf)
            completeStart.countDown()
            await { service.fake.passedDek != null }
            service.fake.ready()
            val binder = IHaiderDaemonService.Stub.asInterface(service.onBind(Intent()))
            await { binder.snapshot.phase == "READY" }
            assertTrue(service.promotedBeforeVersion)
            assertNotNull(binder.rpcEndpoint)
            assertFalse(shadowOf(service).isStoppedBySelf)
            service.onStartCommand(Intent().setAction(DaemonIntents.STOP_DAEMON), 0, 2)
            await { shadowOf(service).isStoppedBySelf }
            assertEquals("DISABLED", binder.snapshot.phase)
        } finally {
            initialize.countDown(); completeStart.countDown()
            controller.destroy()
        }
    }
    @Test fun foregroundBeforeNativeReadyBindingSurvivesUnbindAndStopDisables() {
        val controller = Robolectric.buildService(TestHaiderDaemonService::class.java).create()
        val service = controller.get()
        val binder = IHaiderDaemonService.Stub.asInterface(service.onBind(Intent()))
        try {
            val intent = HaiderDaemonService.userStartIntent(service)
            assertEquals(android.app.Service.START_STICKY, service.onStartCommand(intent, 0, 1))
            await { service.fake.passedDek != null }
            assertTrue(service.promotedBeforeVersion)
            assertTrue(service.state.state.enabled)
            assertNull(binder.rpcEndpoint)
            service.fake.ready()
            await { binder.snapshot.phase == "READY" }
            assertEquals(41L, binder.rpcEndpoint!!.daemonGeneration)
            service.onUnbind(Intent())
            assertEquals("READY", binder.snapshot.phase)
            val initial = CountDownLatch(1)
            val listener = object : IDaemonSnapshotListener.Stub() {
                override fun onSnapshot(snapshot: DaemonServiceSnapshot) { initial.countDown() }
            }
            binder.registerListener(listener)
            assertTrue(initial.await(3, TimeUnit.SECONDS))
            binder.unregisterListener(listener)
            service.onStartCommand(Intent().setAction(DaemonIntents.STOP_DAEMON), 0, 2)
            await { binder.snapshot.phase == "DISABLED" }
            assertFalse(service.state.state.enabled)
            assertNull(binder.rpcEndpoint)
            assertEquals(listOf("shutdown", "release"), service.fake.calls.takeLast(2))
        } finally { controller.destroy() }
    }
    @Test fun everyBinderMethodRejectsOtherUidBeforeAnyEffect() {
        val controller = Robolectric.buildService(TestHaiderDaemonService::class.java).create()
        val service = controller.get()
        val binder = IHaiderDaemonService.Stub.asInterface(service.onBind(Intent()))
        val listener = object : IDaemonSnapshotListener.Stub() { override fun onSnapshot(snapshot: DaemonServiceSnapshot) {} }
        try {
            ShadowBinder.setCallingUid(service.applicationInfo.uid + 1)
            val methods: List<() -> Unit> = listOf(
                { binder.snapshot; Unit }, { binder.rpcEndpoint; Unit },
                { binder.registerListener(listener) }, { binder.unregisterListener(listener) },
                { binder.startUserInitiated() }, { binder.stopAndDisable() }, { binder.restart() }, { binder.prepareForUpdate() },
            )
            for (method in methods) {
                try { method(); fail("Cross-UID method accepted") } catch (_: SecurityException) { }
            }
            assertTrue(service.fake.calls.isEmpty())
            assertFalse(service.state.state.enabled)
        } finally { ShadowBinder.setCallingUid(android.os.Process.myUid()); controller.destroy() }
    }
    private fun await(condition: () -> Boolean) {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
        while (!condition() && System.nanoTime() < deadline) {
            shadowOf(Looper.getMainLooper()).idle()
            Thread.sleep(10)
        }
        assertTrue("Timed out waiting for service state", condition())
    }
}
