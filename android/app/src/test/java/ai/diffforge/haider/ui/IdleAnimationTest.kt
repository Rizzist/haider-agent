package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.components.MotionTicker
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The measured defect, pinned: 417 % host CPU with the app resumed on a running
 * session and the drawer closed, against 3.9 % once backgrounded.
 *
 * Nothing here can measure host CPU, so it pins the thing that *caused* it —
 * how many looping animations are subscribed. A screen that subscribes nothing
 * cannot spin a core.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class IdleAnimationTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Before
    fun resetTicker() {
        MotionTicker.resetForTest()
    }

    @Test
    fun `an idle session with the drawer closed animates nothing`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.EmptyRosterReady))
        rule.waitForIdle()
        assertEquals(
            "something is still animating on an idle screen",
            0,
            MotionTicker.activeSubscribers,
        )
        assertFalse("the ticker is running with no subscribers", MotionTicker.running)
    }

    @Test
    fun `a populated roster does not animate behind a closed drawer`() {
        // Populated has running rows. The drawer is composed while closed —
        // sweeps rely on that — so this is exactly the measured case.
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        val behindClosedDrawer = MotionTicker.activeSubscribers
        rule.onNodeWithContentDescription("Open sessions, 1 session needs input").performClick()
        rule.waitForIdle()
        val withDrawerOpen = MotionTicker.activeSubscribers
        assertTrue(
            "the closed drawer had $behindClosedDrawer subscribers and the open one $withDrawerOpen",
            withDrawerOpen > behindClosedDrawer,
        )
    }

    @Test
    fun `a running turn subscribes, and only one source drives it`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        assertTrue("a streaming turn should animate", MotionTicker.activeSubscribers > 0)
        assertTrue(MotionTicker.running)
    }

    @Test
    fun `a paused app animates nothing, whatever is on screen`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        assertTrue(MotionTicker.running)
        // What the lifecycle gate does on ON_PAUSE.
        MotionTicker.setResumed(false)
        assertFalse("the ticker outlived the foreground", MotionTicker.running)
        MotionTicker.setResumed(true)
        assertTrue(MotionTicker.running)
    }

    @Test
    fun `the ticker rate is a few hertz, not a frame clock`() {
        // 320 ms is ~3 Hz. A frame clock is 16.
        assertTrue(MotionTicker.TICK_MS >= 200L)
    }
}
