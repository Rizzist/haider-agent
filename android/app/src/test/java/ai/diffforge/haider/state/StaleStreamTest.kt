package ai.diffforge.haider.state

import ai.diffforge.haider.ui.chat.Message
import ai.diffforge.haider.ui.chat.Role
import ai.diffforge.haider.ui.daemon.DaemonInfo
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.SendButtonMatrix
import ai.diffforge.haider.ui.state.SendButtonState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * A message still flagged `streaming` after a connection drop is not a live
 * run. Round 1 derived `turnRunning` from `messages.any { it.streaming }`, so a
 * stale tail rendered both Stop affordances for a session whose snapshot
 * carries no `run_id` — a button that cancels nothing, on a coordinate that
 * does not exist.
 */
class StaleStreamTest {
    private val running = DaemonStatus.Running(DaemonInfo(version = "0.0.970", generation = 1))

    private fun state(runId: String?, streaming: Boolean) = AppUiState(
        daemon = running,
        activeSessionId = "s",
        sessions = listOf(
            SessionRow(
                id = "s",
                runId = runId,
                runState = if (runId != null) "running" else "idle",
                workerGeneration = 4,
                state = if (runId != null) SessionVisualState.Running else SessionVisualState.Idle,
            ),
        ),
        messages = listOf(Message(1, Role.Agent, "half a sentence", streaming = streaming)),
    )

    @Test
    fun `a stale streaming message does not make a turn running`() {
        val stale = state(runId = null, streaming = true)
        assertFalse("run_id absent must suppress Stop", stale.turnRunning)
        assertTrue("and the stale tail is still worth knowing about", stale.staleStream)
    }

    @Test
    fun `a live run does`() {
        val live = state(runId = "run-1", streaming = true)
        assertTrue(live.turnRunning)
        assertFalse(live.staleStream)
    }

    @Test
    fun `the composer offers Send, not Stop, for a stale stream`() {
        val stale = state(runId = null, streaming = true)
        val composer = SendButtonMatrix.resolve(
            daemon = stale.daemon,
            turnRunning = stale.turnRunning,
            inputRequired = stale.needsInputHere,
            hasText = true,
        )
        assertEquals(SendButtonState.Send, composer.button)
    }

    @Test
    fun `a session with no rows at all offers nothing to stop`() {
        val empty = AppUiState(
            daemon = running,
            activeSessionId = "gone",
            messages = listOf(Message(1, Role.Agent, "", streaming = true)),
        )
        assertFalse(empty.turnRunning)
    }
}
