package ai.diffforge.haider.state

import ai.diffforge.haider.ui.loom.LoomAuthorKind
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.OverlayNavigation
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Where Back goes (971-V F8).
 *
 * The verifier could not get out of Looms: the on-screen control went to
 * Settings and the system gesture went straight to the session surface, so
 * neither of them went "back".
 */
class OverlayNavigationTest {

    @Test
    fun `a nested screen returns to the surface it was opened from`() {
        assertEquals(Overlay.Settings, OverlayNavigation.parent(Overlay.Looms))
        assertEquals(Overlay.Settings, OverlayNavigation.parent(Overlay.Accounts))
        assertEquals(
            Overlay.Looms,
            OverlayNavigation.parent(Overlay.LoomAuthoring(LoomAuthorKind.Workflow)),
        )
    }

    @Test
    fun `a top-level surface closes to the session deck`() {
        assertEquals(Overlay.None, OverlayNavigation.parent(Overlay.Settings))
        assertEquals(Overlay.None, OverlayNavigation.parent(Overlay.Attach))
        assertEquals(Overlay.None, OverlayNavigation.parent(Overlay.None))
        assertEquals(Overlay.None, OverlayNavigation.parent(Overlay.WorkflowGraph("s")))
    }

    @Test
    fun `walking back from authoring reaches the deck one screen at a time`() {
        // Three presses, three surfaces: authoring -> Looms -> Settings -> deck.
        var overlay: Overlay = Overlay.LoomAuthoring(LoomAuthorKind.AgentType)
        val visited = mutableListOf<Overlay>()
        repeat(3) {
            overlay = OverlayNavigation.parent(overlay)
            visited += overlay
        }
        assertEquals(listOf(Overlay.Looms, Overlay.Settings, Overlay.None), visited)
    }
}
