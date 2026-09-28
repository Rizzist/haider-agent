package ai.diffforge.haider.service.presence

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The phone's "Haider is controlling your phone · Stop" state machine: when the overlay appears,
 * follows, retires, and how Stop latches control off and reaches the daemon.
 */
class CuPresenceControllerTest {
    private var now = 1_000L
    private var stops = 0
    private val renderer = RecordingRenderer()
    private val controller = CuPresenceController(
        clock = { now },
        onStop = { stops += 1 },
        idleMs = 30_000L,
        stopLatchMs = 10_000L,
    ).also { it.attach(renderer) }

    @Test
    fun `first screen request shows the overlay once and every request moves the pointer`() {
        assertTrue(controller.onRequest("a11y.tap", 100, 200, mutating = true))
        assertTrue(controller.onRequest("a11y.text", null, null, mutating = true))
        assertEquals(
            listOf("show", "pointer 100,200 TAP", "pointer null,null TYPE"),
            renderer.calls,
        )
        assertTrue(controller.visible)
    }

    @Test
    fun `sms and unknown requests never raise the indicator`() {
        assertTrue(controller.onRequest("sms.list", null, null, mutating = false))
        assertTrue(controller.onRequest("capabilities", null, null, mutating = false))
        assertTrue(renderer.calls.isEmpty())
        assertFalse(controller.visible)
    }

    @Test
    fun `idle timeout retires the overlay and the next request reshows it`() {
        controller.onRequest("screen.capture", null, null, mutating = false)
        now += 29_999L
        assertTrue(controller.tick())
        now += 1L
        assertFalse(controller.tick())
        assertEquals("hide", renderer.calls.last())
        controller.onRequest("a11y.swipe", 5, 6, mutating = true)
        assertEquals(listOf("show", "pointer 5,6 SWIPE"), renderer.calls.takeLast(2))
    }

    @Test
    fun `presence end from the daemon hides immediately`() {
        controller.onRequest("app.open", null, null, mutating = true)
        controller.onPresenceEnd()
        assertFalse(controller.visible)
        assertEquals("hide", renderer.calls.last())
    }

    @Test
    fun `stop hides, notifies the daemon once and refuses control until the latch or presence end`() {
        controller.onRequest("a11y.tap", 1, 2, mutating = true)
        assertTrue(controller.stopPressed())
        assertEquals(1, stops)
        assertEquals(listOf("stopping", "hide"), renderer.calls.takeLast(2))
        // A tap already queued behind the Stop is refused; observation stays harmless and silent.
        assertFalse(controller.onRequest("a11y.tap", 1, 2, mutating = true))
        assertTrue(controller.onRequest("screen.capture", null, null, mutating = false))
        assertFalse("observation after Stop must not re-raise the chip", controller.visible)
        // A stale notification action cannot cancel anything once nothing is controlled.
        assertFalse(controller.stopPressed())
        assertEquals(1, stops)
        // The latch expires on its own...
        now += 10_000L
        assertTrue(controller.onRequest("a11y.tap", 1, 2, mutating = true))
        assertTrue(controller.visible)
        // ...or as soon as the daemon ends the cancelled run's presence.
        controller.stopPressed()
        controller.onPresenceEnd()
        assertTrue(controller.onRequest("a11y.tap", 3, 4, mutating = true))
    }

    @Test
    fun `detaching the renderer hides it and a later attach shows the live state`() {
        controller.onRequest("a11y.snapshot", null, null, mutating = false)
        controller.detach(renderer)
        assertEquals("hide", renderer.calls.last())
        val next = RecordingRenderer()
        controller.attach(next)
        assertEquals(listOf("show"), next.calls)
    }

    @Test
    fun `request types map to marks`() {
        assertEquals(PresenceMark.TAP, CuPresenceController.markFor("a11y.tap"))
        assertEquals(PresenceMark.OBSERVE, CuPresenceController.markFor("screen.capture"))
        assertEquals(PresenceMark.OPEN_APP, CuPresenceController.markFor("app.open"))
        assertNull(CuPresenceController.markFor("sms.list"))
    }

    @Test
    fun `the stop push is exactly the frame the daemon accepts`() {
        assertEquals("""{"type":"presence.stop"}""", CuPresence.stopPush().toString())
    }

    private class RecordingRenderer : PresenceRenderer {
        val calls = mutableListOf<String>()
        override fun show() {
            calls += "show"
        }
        override fun pointer(x: Int?, y: Int?, mark: PresenceMark) {
            calls += "pointer $x,$y $mark"
        }
        override fun stopping() {
            calls += "stopping"
        }
        override fun hide() {
            calls += "hide"
        }
    }
}
