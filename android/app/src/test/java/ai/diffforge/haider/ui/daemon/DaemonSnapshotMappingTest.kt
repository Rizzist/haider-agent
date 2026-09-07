package ai.diffforge.haider.ui.daemon

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The UI's whole dependency on the frozen Binder contract (C2). */
class DaemonSnapshotMappingTest {

    private fun snapshot(
        phase: DaemonPhase,
        endpoint: RpcEndpoint? = null,
        errorCode: String? = null,
        startedAt: Long? = null,
        pss: Long? = null,
        seq: Long = 1,
    ) = DaemonServiceSnapshot(
        enabled = true,
        phase = phase,
        appVersion = "0.0.971",
        nativeVersion = "0.0.971",
        wireProtocol = 1,
        daemonGeneration = 12,
        rpcEndpoint = endpoint,
        restartAttempt = 0,
        nextRetryUnixMs = null,
        network = NetworkState.Available,
        notificationsGranted = true,
        batteryRestricted = false,
        errorCode = errorCode,
        errorRetryable = false,
        snapshotSeq = seq,
        startedAtElapsedRealtimeMs = startedAt,
        pssBytes = pss,
    )

    @Test
    fun `every phase folds the way the contract says`() {
        assertEquals(DaemonStatus.Stopped, DaemonSnapshotMapping.toStatus(snapshot(DaemonPhase.Disabled)))
        assertEquals(DaemonStatus.Starting, DaemonSnapshotMapping.toStatus(snapshot(DaemonPhase.Starting)))
        assertEquals(DaemonStatus.Starting, DaemonSnapshotMapping.toStatus(snapshot(DaemonPhase.Recovering)))
        assertEquals(DaemonStatus.Restarting, DaemonSnapshotMapping.toStatus(snapshot(DaemonPhase.Restarting)))
        // STOPPING is a transition, not a stopped daemon you can queue against.
        assertEquals(DaemonStatus.Stopping, DaemonSnapshotMapping.toStatus(snapshot(DaemonPhase.Stopping)))
    }

    @Test
    fun `an error carries the redacted code, not a stack trace`() {
        val status = DaemonSnapshotMapping.toStatus(
            snapshot(DaemonPhase.Error, errorCode = "STORE_RECOVERY_FAILED"),
        ) as DaemonStatus.Failed
        assertEquals("STORE_RECOVERY_FAILED", status.reason)
        assertEquals("STORE_RECOVERY_FAILED", status.code)
    }

    @Test
    fun `READY is not Running until the data plane is actually connected`() {
        val ready = snapshot(DaemonPhase.Ready, endpoint = RpcEndpoint("/x/h.sock", 1, 12))
        assertEquals(
            DaemonStatus.Starting,
            DaemonSnapshotMapping.toStatus(ready, dataPlane = DataPlaneState.Connecting),
        )
        assertTrue(
            DaemonSnapshotMapping.toStatus(ready, dataPlane = DataPlaneState.Connected)
                is DaemonStatus.Running,
        )
    }

    @Test
    fun `the two appended metrics are service-owned and pass through`() {
        val status = DaemonSnapshotMapping.toStatus(
            snapshot(
                DaemonPhase.Ready,
                endpoint = RpcEndpoint("/x/h.sock", 1, 12),
                startedAt = 5_000,
                pss = 61 * 1024 * 1024,
            ),
        ) as DaemonStatus.Running
        assertEquals(5_000L, status.info.startedAtElapsedRealtimeMs)
        assertEquals(61L * 1024 * 1024, status.info.pssBytes)
        // Unknown metrics stay null; they are never invented as zero.
        val unknown = DaemonSnapshotMapping.toStatus(
            snapshot(DaemonPhase.Ready, endpoint = RpcEndpoint("/x/h.sock", 1, 12)),
        ) as DaemonStatus.Running
        assertNull(unknown.info.startedAtElapsedRealtimeMs)
        assertNull(unknown.info.pssBytes)
    }

    @Test
    fun `the endpoint is only trusted in READY`() {
        val starting = DaemonSnapshotMapping.toStatus(
            snapshot(DaemonPhase.Starting, endpoint = RpcEndpoint("/x/h.sock", 1, 12)),
        )
        assertEquals(DaemonStatus.Starting, starting)
    }

    @Test
    fun `daemon generation is not conflated with a worker generation`() {
        val status = DaemonSnapshotMapping.toStatus(
            snapshot(DaemonPhase.Ready, endpoint = RpcEndpoint("/x/h.sock", 1, 12)),
        ) as DaemonStatus.Running
        assertEquals(12L, status.info.generation)
    }

    @Test
    fun `snapshot sequence is per Binder instance and re-baselines on death`() {
        val sequencer = DaemonSnapshotSequencer()
        assertTrue(sequencer.accept(snapshot(DaemonPhase.Ready, seq = 4)))
        assertFalse(sequencer.accept(snapshot(DaemonPhase.Ready, seq = 4)))
        assertFalse(sequencer.accept(snapshot(DaemonPhase.Ready, seq = 3)))
        assertTrue(sequencer.accept(snapshot(DaemonPhase.Ready, seq = 5)))
        // A new Binder instance restarts its own sequence: comparing across
        // instances would silently drop the first snapshots of the new one.
        sequencer.resetBaseline()
        assertTrue(sequencer.accept(snapshot(DaemonPhase.Ready, seq = 1)))
    }

    @Test
    fun `an unknown phase string is treated as an error, never as ready`() {
        assertEquals(DaemonPhase.Error, DaemonPhase.fromWire("SOMETHING_NEW"))
        assertEquals(DaemonPhase.Error, DaemonPhase.fromWire(null))
        assertEquals(DaemonPhase.Ready, DaemonPhase.fromWire("READY"))
    }

    @Test
    fun `environment signals come straight off the snapshot`() {
        val environment = DaemonSnapshotMapping.toEnvironment(snapshot(DaemonPhase.Ready))
        assertEquals(NetworkState.Available, environment.network)
        assertTrue(environment.notificationsGranted)
        assertFalse(environment.batteryRestricted)
    }
}
