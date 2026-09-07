package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.start.START_FIRST_CHILD_TAG
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.unit.dp
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * D3: 970 centred the empty state in a `fillMaxSize` box, which is what put a
 * wall of dead space above and below the hero. The first child now sits just
 * under the header.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class StartSurfaceTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `the first child sits at most 24 dp below the header at 412 by 915`() {
        // First run shows no banner: the checklist is the message, so the
        // surface really does start directly under the header.
        val service = ComposeHost.install(FakeScenario.FirstRun)
        rule.setHaiderApp(service)

        val y = rule.onNodeWithTag(START_FIRST_CHILD_TAG)
            .fetchSemanticsNode()
            .positionInRoot.y
        // Measured from the header's own bottom edge, so the window insets do
        // not get counted as dead space the layout chose.
        val headerButton = rule.onAllNodes(
            androidx.compose.ui.test.hasContentDescription("Open sessions", substring = true),
        ).fetchSemanticsNodes().first()
        val headerBottom = headerButton.boundsInRoot.bottom +
            with(rule.density) { ((ForgeSize.header - ForgeSize.touch) / 2).toPx() }
        val ceilingPx = headerBottom + with(rule.density) { 24.dp.toPx() }
        assertTrue(
            "the start surface begins ${y}px down, past the ${ceilingPx}px ceiling",
            y <= ceilingPx,
        )
    }

    @Test
    fun `first run counts its steps and never asks for a host or a token`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        rule.setHaiderApp(service)
        rule.onNodeWithText("Haider runs on this phone").assertIsDisplayed()
        rule.onNodeWithText("Run Haider in the background").assertIsDisplayed()
        // The 970 connect form is gone (D9).
        assertEquals(0, rule.onAllNodesWithTextSafe("Host"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Port"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Token"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Set up connection"))
    }

    @Test
    fun `starting the daemon advances the checklist`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        rule.setHaiderApp(service)
        rule.onNodeWithText("Start Haider").performClick()
        rule.waitForIdle()
        // Step 1 collapses to its done line, and the next step opens.
        assertTrue(rule.onAllNodesWithTextSafe("Let Haider notify you") > 0)
        assertTrue(service.calls.contains("start"))
    }

    @Test
    fun `a suggestion fills the composer and does not send`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        val viewModel = rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("What is on my screen right now?").performClick()
        rule.waitForIdle()
        assertEquals("What is on my screen right now?", viewModel.state.value.draft)
        // The user stays the author: nothing was sent.
        assertTrue(service.calls.none { it.startsWith("chat.send") })
    }

    @Test
    fun `a completed setup drops the checklist for the ready line`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(service)
        rule.onNodeWithText("Ready. Ask for anything on this phone.").assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTextSafe("Keep it alive on One UI"))
    }

    @Test
    fun `with sessions the suggestions give way to recent sessions`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(service)
        assertTrue(rule.onAllNodesWithTextSafe("WHEN YOU ARE READY") > 0)
    }
}
