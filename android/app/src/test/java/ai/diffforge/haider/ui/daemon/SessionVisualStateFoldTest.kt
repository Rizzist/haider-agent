package ai.diffforge.haider.ui.daemon

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

/** The whole UI-SPEC 5.3 fold table, including the neutral Unknown. */
class SessionVisualStateFoldTest {

    private val asking = NeedsInput(kind = "permission", title = "Allow?")

    @Test
    fun `needs input outranks running`() {
        assertEquals(
            SessionVisualState.NeedsInput,
            SessionVisualStateFold.fold("running", asking),
        )
    }

    @Test
    fun `parked states are needs input even with no payload`() {
        assertEquals(SessionVisualState.NeedsInput, SessionVisualStateFold.fold("parked_input", null))
        assertEquals(
            SessionVisualState.NeedsInput,
            SessionVisualStateFold.fold("parked_permission", null),
        )
    }

    @Test
    fun `the coarse wire states map one for one`() {
        assertEquals(SessionVisualState.Running, SessionVisualStateFold.fold("running", null))
        assertEquals(
            SessionVisualState.WaitingForNetwork,
            SessionVisualStateFold.fold("waiting_for_route", null),
        )
        assertEquals(SessionVisualState.Idle, SessionVisualStateFold.fold("idle", null))
        assertEquals(SessionVisualState.Idle, SessionVisualStateFold.fold("cancelled", null))
        assertEquals(SessionVisualState.Errored, SessionVisualStateFold.fold("errored", null))
    }

    @Test
    fun `unknown and unrecognised stay neutral - no rail, no pill, no animation`() {
        listOf("effect_unknown", "unknown", "a_state_shipped_after_971", null).forEach { wire ->
            val state = SessionVisualStateFold.fold(wire, null)
            assertEquals(SessionVisualState.Unknown, state)
            assertFalse(SessionVisualStateFold.rendersRail(state))
            assertFalse(SessionVisualStateFold.animates(state))
        }
    }

    @Test
    fun `nothing pulses for a corpse`() {
        assertFalse(SessionVisualStateFold.animates(SessionVisualState.Errored))
        assertFalse(SessionVisualStateFold.animates(SessionVisualState.NeedsInput))
        assertFalse(SessionVisualStateFold.animates(SessionVisualState.Idle))
    }

    @Test
    fun `idle draws no rail`() {
        assertFalse(SessionVisualStateFold.rendersRail(SessionVisualState.Idle))
    }
}
