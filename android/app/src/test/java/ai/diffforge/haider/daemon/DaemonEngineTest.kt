package ai.diffforge.haider.daemon

import org.junit.Assert.*
import org.junit.Test

class DaemonEngineTest {
    private val host = FakeNativeDaemonHost()
    private val store = FakeLifecycleStore()
    private val clock = FakeDaemonClock()
    private val snapshots = mutableListOf<DaemonServiceSnapshot>()
    private var terminated = 0
    private var vault = VaultDekProvider { use -> ByteArray(32) { 7 }.let { try { use(it) } finally { it.fill(0) } } }
    private fun engine() = DaemonEngine(host, vault, store, clock, "0.0.970", "{}", "/private/runtime/h.sock", "{}",
        snapshots::add, { terminated++ }).also { it.restore(null) }

    @Test fun disabledBindingDoesNotLoadNative() {
        val engine = engine()
        engine.resumeEnabled(); engine.tick()
        assertEquals("DISABLED", engine.snapshot.phase)
        assertTrue(host.calls.isEmpty())
    }
    @Test fun startAcceptanceIsNotReadyAndEndpointUsesActualGeneration() {
        val engine = engine()
        engine.startUserInitiated()
        assertTrue(store.state.enabled && store.state.active)
        assertEquals("STARTING", engine.snapshot.phase)
        assertNull(engine.snapshot.rpcEndpoint)
        assertTrue(host.passedDek!!.all { it == 0.toByte() })
        engine.tick()
        assertEquals("RECOVERING", engine.snapshot.phase)
        host.ready(); engine.tick()
        assertEquals(RpcEndpoint("/private/runtime/h.sock", 1, 41), engine.snapshot.rpcEndpoint)
        assertEquals(41L, engine.snapshot.daemonGeneration)
        assertEquals(clock.elapsed, engine.snapshot.startedAtElapsedRealtimeMs)
        assertTrue(snapshots.zipWithNext().all { (a, b) -> b.snapshotSeq > a.snapshotSeq })
        engine.startUserInitiated()
        assertEquals(1, host.calls.count { it == "start" })
    }
    @Test fun stopPersistsDisabledBeforeDrainingAndCancelsAutostart() {
        val engine = engine()
        engine.startUserInitiated(); host.ready(); engine.tick()
        host.onShutdown = { assertFalse(store.state.enabled); assertNull(engine.snapshot.rpcEndpoint) }
        engine.stopAndDisable()
        assertEquals("DISABLED", engine.snapshot.phase)
        assertFalse(store.state.active)
        assertEquals(listOf("shutdown", "release"), host.calls.takeLast(2))
        clock.advance(1_000_000); engine.tick(); engine.resumeEnabled()
        assertEquals(1, host.calls.count { it == "start" })
    }
    @Test fun threeExitsInTenMinutesLatchAcrossServiceInstances() {
        var engine = engine()
        engine.startUserInitiated()
        host.crash(); engine.tick()
        assertEquals("RESTARTING", engine.snapshot.phase)
        engine.tick()
        assertEquals(1, host.calls.count { it == "start" })
        clock.advance(1_000); engine.tick()
        // Simulate SIGKILL: do not call destroy; reconstruct from the active marker.
        engine = engine()
        assertEquals(2, engine.snapshot.restartAttempt)
        clock.advance(4_999); engine.tick()
        assertEquals(2, host.calls.count { it == "start" })
        clock.advance(1); engine.tick()
        engine = engine()
        assertEquals("ERROR", engine.snapshot.phase)
        assertEquals("CRASH_LOOP", engine.snapshot.errorCode)
        assertTrue(engine.snapshot.errorRetryable)
        assertNull(engine.snapshot.nextRetryUnixMs)
        engine = engine() // Restore an already-latched state, rather than recording another crash.
        assertEquals("ERROR", engine.snapshot.phase)
        assertEquals("CRASH_LOOP", engine.snapshot.errorCode)
        assertTrue(engine.snapshot.errorRetryable)
        assertNull(engine.snapshot.nextRetryUnixMs)
        clock.advance(1_000_000); engine.tick(); engine.resumeEnabled()
        assertEquals(3, host.calls.count { it == "start" })
        engine.restart()
        assertEquals("STARTING", engine.snapshot.phase)
        assertFalse(engine.snapshot.errorRetryable)
        assertNull(engine.snapshot.errorCode)
        assertTrue(store.state.crashes.isEmpty())
    }
    @Test fun oldCrashesExpireButBackwardWallClockDoesNotClearLatch() {
        store.state = PersistedLifecycle(enabled = true, active = true,
            crashes = listOf(clock.wall - 700_000, clock.wall - 650_000))
        assertEquals(1, engine().snapshot.restartAttempt)
        store.state = PersistedLifecycle(enabled = true, active = true,
            crashes = listOf(clock.wall + 100, clock.wall + 200))
        assertEquals("CRASH_LOOP", engine().snapshot.errorCode)
    }
    @Test fun monotonicBackoffSurvivesWallClockJump() {
        val engine = engine(); engine.startUserInitiated(); host.crash(); engine.tick()
        clock.wall += 10_000_000; engine.tick()
        assertEquals(1, host.calls.count { it == "start" })
        clock.elapsed += 1_000; engine.tick()
        assertEquals(2, host.calls.count { it == "start" })
    }
    @Test fun stopDuringBackoffPreventsAnyScheduledRestart() {
        val engine = engine(); engine.startUserInitiated(); host.crash(); engine.tick()
        engine.stopAndDisable(); clock.advance(10_000); engine.tick()
        assertEquals(1, host.calls.count { it == "start" })
        assertFalse(store.state.enabled)
    }
    @Test fun shutdownTimeoutNeverReleasesOrStartsAnotherRuntime() {
        val engine = engine(); engine.startUserInitiated()
        host.shutdownStatus = NativeStatus.SHUTDOWN_TIMEOUT
        engine.restart()
        assertEquals(1, terminated)
        assertFalse("release" in host.calls)
        assertEquals("SHUTDOWN_TIMEOUT", engine.snapshot.errorCode)
        assertNull(engine.snapshot.rpcEndpoint)
        assertEquals(1, host.calls.count { it == "start" })
        assertEquals("SHUTDOWN_TIMEOUT", engine().snapshot.errorCode)
    }
    @Test fun updatePreservesOptInAndMarkerIsBoundedAndConsumed() {
        var engine = engine(); engine.startUserInitiated(); engine.prepareForUpdate()
        assertTrue(store.state.enabled)
        assertFalse(store.state.active)
        assertEquals(clock.wall + 120_000, store.state.updateUntilUnixMs)
        engine = engine(); engine.resumeEnabled()
        assertEquals(1, host.calls.count { it == "start" })
        engine.resumeEnabled(afterReplacement = true)
        assertEquals(2, host.calls.count { it == "start" })
        assertNull(store.state.updateUntilUnixMs)
        engine.stopAndDisable(); engine.prepareForUpdate(); engine.resumeEnabled(true)
        assertFalse(store.state.enabled)
        assertNull(store.state.updateUntilUnixMs)
    }
    @Test fun abandonedUpdateResumesAfterItsBoundedWindow() {
        val engine = engine(); engine.startUserInitiated(); engine.prepareForUpdate()
        clock.advance(120_000); engine.tick()
        assertEquals(2, host.calls.count { it == "start" })
    }
    @Test fun osUserStopRevokesOptInButOldExitCannotUndoNewUserStart() {
        store.state = PersistedLifecycle(enabled = true, active = true, lastUserStartUnixMs = clock.wall - 100)
        var engine = engine()
        engine.restore(ProcessExit(clock.wall, true))
        engine.resumeEnabled()
        assertFalse(store.state.enabled)
        assertEquals("DISABLED", engine.snapshot.phase)
        engine.startUserInitiated(); clock.advance(100)
        engine = engine()
        engine.restore(ProcessExit(clock.wall - 200, true))
        assertTrue(store.state.enabled)
    }
    @Test fun vaultInvalidationIsTypedNonRetryingAndPreservesEnabledIntent() {
        vault = VaultDekProvider { throw VaultKeyFailure("VAULT_KEY_INVALID") }
        val engine = engine(); engine.startUserInitiated()
        assertEquals("ERROR", engine.snapshot.phase)
        assertEquals("VAULT_KEY_INVALID", engine.snapshot.errorCode)
        assertFalse(engine.snapshot.errorRetryable)
        assertTrue(store.state.enabled)
        assertFalse("start" in host.calls)
        assertEquals(listOf("shutdown", "release"), host.calls.takeLast(2))
    }
    @Test fun versionMismatchNeverStartsOrUnwraps() {
        vault = VaultDekProvider { error("must not unwrap") }
        host.versionJson = host.versionJson.replace("0.0.970", "0.0.969")
        val engine = engine(); engine.startUserInitiated()
        assertEquals("NATIVE_VERSION_MISMATCH", engine.snapshot.errorCode)
        assertEquals(listOf("version"), host.calls)
    }
    @Test fun malformedReadyAndNativeErrorTextNeverEnterBinder() {
        val engine = engine(); engine.startUserInitiated()
        host.observation = """{"jni_version":1,"phase":"Ready","daemon_generation":7,"endpoint_path":"/secret/h.sock","ready_since_unix_ms":1}"""
        engine.tick()
        assertEquals("NATIVE_PROTOCOL_INVALID", engine.snapshot.errorCode)
        assertNull(engine.snapshot.rpcEndpoint)
        assertFalse(engine.snapshot.toString().contains("/secret"))
        engine.restart()
        host.observation = """{"jni_version":1,"phase":"Failed","daemon_generation":7,"error_code":"token=synthetic-canary","retryable":false}"""
        engine.tick()
        assertEquals("INTERNAL", engine.snapshot.errorCode)
        assertFalse(snapshots.any { it.toString().contains("synthetic-canary") })
    }
    @Test fun alreadyRunningIsNotAdopted() {
        host.startStatus = NativeStatus.ALREADY_RUNNING
        val engine = engine(); engine.startUserInitiated()
        assertEquals("ALREADY_RUNNING", engine.snapshot.errorCode)
        assertNull(engine.snapshot.rpcEndpoint)
        assertEquals(listOf("shutdown", "release"), host.calls.takeLast(2))
    }
    @Test fun hungRecoveryIsBounded() {
        val engine = engine(); engine.startUserInitiated()
        clock.advance(DaemonEngine.STARTUP_TIMEOUT_MS); engine.tick()
        assertEquals("RESTARTING", engine.snapshot.phase)
        assertTrue("shutdown" in host.calls)
    }
    @Test fun packageUpdateIsNotUserStopOrUnexpectedCrashOnLegacyAndModernAndroid() {
        store.state = PersistedLifecycle(enabled = true, active = true, updateUntilUnixMs = clock.wall + 120_000)
        val legacy = DaemonEngine(host, vault, store, clock, "0.0.970", "{}", "/private/runtime/h.sock", "{}", snapshots::add, {})
        legacy.restore(ProcessExit(clock.wall, true, legacyUserRequested = true))
        assertTrue(store.state.enabled)
        assertTrue(store.state.crashes.isEmpty())
        legacy.resumeEnabled(afterReplacement = true)
        assertEquals("STARTING", legacy.snapshot.phase)
        assertNull(store.state.updateUntilUnixMs)

        store.state = PersistedLifecycle(enabled = true, active = true)
        val modern = DaemonEngine(host, vault, store, clock, "0.0.970", "{}", "/private/runtime/h.sock", "{}", snapshots::add, {})
        modern.restore(ProcessExit(clock.wall, false, packageUpdated = true))
        assertTrue(store.state.enabled)
        assertTrue(store.state.crashes.isEmpty())
    }
    @Test fun unambiguousUserStopOverridesPendingUpdate() {
        store.state = PersistedLifecycle(enabled = true, active = true, updateUntilUnixMs = clock.wall + 120_000)
        val engine = DaemonEngine(host, vault, store, clock, "0.0.970", "{}", "/private/runtime/h.sock", "{}", snapshots::add, {})
        engine.restore(ProcessExit(clock.wall, true))
        assertFalse(store.state.enabled)
        assertNull(store.state.updateUntilUnixMs)
        assertEquals("DISABLED", engine.snapshot.phase)
    }
    @Test fun spontaneousNativeDrainIsBounded() {
        val engine = engine(); engine.startUserInitiated()
        host.observation = """{"jni_version":1,"phase":"Draining","daemon_generation":41}"""
        engine.tick()
        assertEquals("STOPPING", engine.snapshot.phase)
        clock.advance(7_000); engine.tick()
        assertEquals("RESTARTING", engine.snapshot.phase)
    }
    @Test fun fractionalGenerationCannotBePublishedAsAnInteger() {
        val engine = engine(); engine.startUserInitiated(); host.ready()
        host.observation = host.observation.replace("41", "41.5")
        engine.tick()
        assertEquals("NATIVE_PROTOCOL_INVALID", engine.snapshot.errorCode)
        assertNull(engine.snapshot.rpcEndpoint)
    }
    @Test fun serviceDestructionDrainsWithoutDisablingUserIntent() {
        val engine = engine(); engine.startUserInitiated(); engine.destroy()
        assertTrue(store.state.enabled)
        assertFalse(store.state.active)
        assertEquals(listOf("shutdown", "release"), host.calls.takeLast(2))
    }
}
