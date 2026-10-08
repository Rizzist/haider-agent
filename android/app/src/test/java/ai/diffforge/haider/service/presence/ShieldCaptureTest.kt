package ai.diffforge.haider.service.presence

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Verifier finding (191a8d60): cancelling a capture while the overlay was hidden could leave the
 * "controlling your phone" chip invisible for good. The overlay must be restored on every exit.
 * MUTATION CHECK: move the restore out of `finally`, or drop `NonCancellable`. Expected failure:
 * `visible` stays false in the cancellation tests below.
 */
class ShieldCaptureTest {
    private var visible = true
    private var restores = 0
    private val hide: suspend () -> Boolean = {
        visible = false
        true
    }
    private val restore: suspend () -> Unit = {
        // Suspends like the real main-dispatcher hop, to prove NonCancellable is in effect.
        kotlinx.coroutines.yield()
        visible = true
        restores += 1
    }

    @Test
    fun `cancel while waiting for the hidden frame restores the overlay`() = runTest {
        val job = async(start = CoroutineStart.UNDISPATCHED) {
            shieldCapture(hide, awaitHiddenFrame = { awaitCancellation() }, restore = restore, capture = { "png" })
        }
        assertEquals(false, visible)
        job.cancel()
        runCurrent()
        assertTrue("overlay restored after cancellation mid frame wait", visible)
        assertEquals(1, restores)
    }

    @Test
    fun `cancel during the capture itself restores the overlay`() = runTest {
        val entered = CompletableDeferred<Unit>()
        val job = async {
            shieldCapture(
                hide,
                awaitHiddenFrame = {},
                restore = restore,
                capture = {
                    entered.complete(Unit)
                    awaitCancellation()
                },
            )
        }
        entered.await()
        assertEquals(false, visible)
        job.cancel()
        runCurrent()
        assertTrue(visible)
        assertEquals(1, restores)
    }

    @Test
    fun `a frame that never commits is bounded and the capture still runs hidden`() = runTest {
        var hiddenDuringCapture = true
        val result = async {
            shieldCapture(
                hide,
                awaitHiddenFrame = { awaitCancellation() },
                restore = restore,
                capture = {
                    hiddenDuringCapture = !visible
                    "png"
                },
            )
        }
        advanceTimeBy(CAPTURE_SHIELD_FRAME_WAIT_MS + 1)
        assertEquals("png", result.await())
        assertTrue("the model-facing capture ran while hidden", hiddenDuringCapture)
        assertTrue(visible)
    }

    @Test
    fun `a failing capture restores the overlay and propagates the error`() = runTest {
        val outcome = runCatching {
            shieldCapture(hide, awaitHiddenFrame = {}, restore = restore, capture = { error("boom") })
        }
        assertTrue(outcome.isFailure)
        assertTrue(visible)
    }
}
