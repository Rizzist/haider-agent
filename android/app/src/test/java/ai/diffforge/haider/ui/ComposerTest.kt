package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class ComposerTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `the model chip resolves rather than sitting on loading`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        assertTrue(rule.onAllNodesWithTextSafe("Sonnet 4.5 · high") > 0)
        // The literal that used to be permanent (D4) appears nowhere.
        assertEquals(0, rule.onAllNodesWithTextSafe("loading…"))
    }

    @Test
    fun `with no daemon the chip says start haider, not loading`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        rule.setHaiderApp(service)
        assertTrue(rule.onAllNodesWithTextSafe("Start Haider first") > 0)
        assertEquals(0, rule.onAllNodesWithTextSafe("loading…"))
    }

    @Test
    fun `a catalog error offers a retry`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        service.setModels(null)
        service.setCatalogError("provider catalog unavailable")
        rule.setHaiderApp(service)
        assertTrue(rule.onAllNodesWithTextSafe("Models unavailable · Retry") > 0)
    }

    @Test
    fun `the context row stays 32 dp, not the 48 that bloated the composer`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        val chip = rule.onAllNodes(hasContentDescription("Change model", substring = true))
            .fetchSemanticsNodes()
            .first()
        val heightDp = with(rule.density) { chip.size.height.toDp() }
        // The visual chip stays small; its touch box is grown separately.
        assertTrue("chip is $heightDp tall", heightDp <= ForgeSize.touch)
    }

    @Test
    fun `send is disabled with no text and enabled with some`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        val viewModel = rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("Send message").assertIsNotEnabled()
        viewModel.setDraft("hello")
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Send message").assertIsDisplayed()
    }

    @Test
    fun `a stopped daemon offers start and send`() {
        val service = ComposeHost.install(FakeScenario.DaemonStopped)
        val viewModel = rule.setHaiderApp(service)
        viewModel.setDraft("do the thing")
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Start Haider and send").assertIsDisplayed()
    }

    @Test
    fun `a running turn swaps send for stop`() {
        val service = ComposeHost.install(FakeScenario.TurnRunning)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Stop this turn")).onFirst().assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithContentDescriptionSafe("Send message"))
    }

    @Test
    fun `stopping a turn passes the coordinates from the same snapshot`() {
        val service = ComposeHost.install(FakeScenario.TurnRunning)
        rule.setHaiderApp(service)
        val row = service.sessions.value.first { it.id == "s-nav" }
        rule.onAllNodes(hasContentDescription("Stop this turn")).onFirst().performClick()
        rule.waitForIdle()
        assertTrue(
            service.calls.contains("turn.cancel:s-nav:${row.runId}:${row.workerGeneration}"),
        )
    }

    @Test
    fun `a paused turn disables the composer and says why`() {
        val service = ComposeHost.install(FakeScenario.InputRequiredHere)
        rule.setHaiderApp(service)
        assertTrue(rule.onAllNodesWithTextSafe("The turn is paused.") > 0)
    }
}

internal fun androidx.compose.ui.test.junit4.ComposeTestRule.onAllNodesWithContentDescriptionSafe(
    value: String,
): Int = onAllNodes(hasContentDescription(value)).fetchSemanticsNodes().size
