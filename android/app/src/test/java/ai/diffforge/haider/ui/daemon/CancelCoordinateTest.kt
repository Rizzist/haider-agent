package ai.diffforge.haider.ui.daemon

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * `run_id` and `worker_generation` must be read from the same snapshot as
 * `run_state`; pairing an id from one poll with a state from another can cancel
 * a run that already ended (frame.rs:1716-1730).
 */
class CancelCoordinateTest {

    @Test
    fun `no run id means no coordinates and no stop button`() {
        val idle = SessionRow(id = "s", runId = null, workerGeneration = 4)
        assertNull(TurnCancel.coordinates(idle))
        assertNull(TurnCancel.coordinates(null))
    }

    @Test
    fun `both coordinates pass through unchanged`() {
        val running = SessionRow(id = "s", runId = "run-9", workerGeneration = 6)
        val coordinates = TurnCancel.coordinates(running)!!
        assertEquals("s", coordinates.sessionId)
        assertEquals("run-9", coordinates.runId)
        assertEquals(6L, coordinates.workerGeneration)
    }

    @Test
    fun `the service refuses a cancel without live coordinates`() = runTest {
        val service = FakeDaemonService(FakeScenario.Populated)
        try {
            // s-broken is errored: it carries no run_id.
            service.stopTurn("s-broken")
            fail("expected the cancel to be refused")
        } catch (expected: MissingRunCoordinates) {
            assertTrue(expected.message!!.contains("s-broken"))
        }
        assertTrue(service.calls.none { it.startsWith("turn.cancel") })
    }

    @Test
    fun `a live cancel carries the run id and generation from the same row`() = runTest {
        val service = FakeDaemonService(FakeScenario.Populated)
        val row = service.sessions.value.first { it.id == "s-nav" }
        service.stopTurn("s-nav")
        assertTrue(
            service.calls.contains("turn.cancel:s-nav:${row.runId}:${row.workerGeneration}"),
        )
        // After the cancel the row no longer offers a stop button.
        assertNull(TurnCancel.coordinates(service.sessions.value.first { it.id == "s-nav" }))
    }
}
