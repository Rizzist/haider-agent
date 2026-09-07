package ai.diffforge.haider.state

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.DaemonInfo
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.state.SendButtonMatrix
import ai.diffforge.haider.ui.state.SendButtonState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The whole UI-SPEC 3.6 send-button matrix. */
class SendButtonStateTest {
    private val running = DaemonStatus.Running(DaemonInfo(version = "0.0.971", generation = 1))

    @Test
    fun `running and idle with no text is disabled`() {
        val state = SendButtonMatrix.resolve(running, turnRunning = false, inputRequired = false, hasText = false)
        assertEquals(SendButtonState.Disabled, state.button)
        assertFalse(state.button.enabled)
        assertNull(state.helperRes)
    }

    @Test
    fun `running and idle with text sends`() {
        val state = SendButtonMatrix.resolve(running, turnRunning = false, inputRequired = false, hasText = true)
        assertEquals(SendButtonState.Send, state.button)
        assertEquals(R.string.cd_send, state.contentDescriptionRes)
    }

    @Test
    fun `a running turn offers stop, whatever the text says`() {
        listOf(true, false).forEach { hasText ->
            val state = SendButtonMatrix.resolve(running, turnRunning = true, inputRequired = false, hasText = hasText)
            assertEquals(SendButtonState.Stop, state.button)
            assertEquals(R.string.cd_stop_turn, state.contentDescriptionRes)
            assertEquals(R.string.composer_helper_running, state.helperRes)
        }
    }

    @Test
    fun `input required outranks a running turn and pauses the composer`() {
        val state = SendButtonMatrix.resolve(running, turnRunning = true, inputRequired = true, hasText = true)
        assertEquals(SendButtonState.Paused, state.button)
        assertFalse(state.inputEnabled)
        assertEquals(R.string.composer_helper_paused, state.helperRes)
        assertEquals(R.string.composer_placeholder_paused, state.placeholderRes)
    }

    @Test
    fun `a stopped daemon with text says start and send`() {
        val state = SendButtonMatrix.resolve(
            DaemonStatus.Stopped,
            turnRunning = false,
            inputRequired = false,
            hasText = true,
        )
        assertEquals(SendButtonState.StartAndSend, state.button)
        assertEquals(R.string.cd_start_and_send, state.contentDescriptionRes)
        assertTrue(state.button.enabled)
    }

    @Test
    fun `a stopped daemon with no text is still disabled`() {
        assertEquals(
            SendButtonState.Disabled,
            SendButtonMatrix.resolve(
                DaemonStatus.Stopped,
                turnRunning = false,
                inputRequired = false,
                hasText = false,
            ).button,
        )
    }

    @Test
    fun `starting, restarting and stopping all refuse new turns`() {
        listOf(DaemonStatus.Starting, DaemonStatus.Restarting, DaemonStatus.Stopping).forEach { daemon ->
            val state = SendButtonMatrix.resolve(daemon, turnRunning = false, inputRequired = false, hasText = true)
            assertEquals(SendButtonState.Starting, state.button)
            assertFalse(state.inputEnabled)
            assertFalse(state.button.enabled)
        }
    }

    @Test
    fun `unfinished setup points at the step instead of the button`() {
        val state = SendButtonMatrix.resolve(
            DaemonStatus.Stopped,
            turnRunning = false,
            inputRequired = false,
            hasText = true,
            setupComplete = false,
        )
        assertEquals(SendButtonState.Disabled, state.button)
        assertEquals(R.string.composer_helper_setup, state.helperRes)
    }
}
