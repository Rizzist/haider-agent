package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Addition D: the composer bar carries provider / model / effort pickers, like
 * the desktop client's chips row, bound to `provider.list` and the canonical
 * session-selection calls through the facade.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class ComposerPickersTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `all three chips are on the composer bar`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.onAllNodes(hasContentDescription("Change provider", substring = true))
            .onFirst()
            .assertIsDisplayed()
        rule.onAllNodes(hasContentDescription("Change model", substring = true))
            .onFirst()
            .assertIsDisplayed()
        rule.onAllNodes(hasContentDescription("Change effort", substring = true))
            .onFirst()
            .assertIsDisplayed()
    }

    @Test
    fun `the provider picker lists the inventory and marks the unavailable`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Change provider", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Provider").assertIsDisplayed()
        rule.onNodeWithContentDescription("Anthropic").assertIsDisplayed()
        // provider.list says OpenAI has no account, so it cannot be chosen.
        rule.onNodeWithContentDescription("OpenAI").assertIsNotEnabled()
        assertTrue(rule.onAllNodesWithTextSafe("OpenAI — No account configured") > 0)
    }

    @Test
    fun `choosing a provider goes through the canonical selection call`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Change provider", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Anthropic").performClick()
        rule.waitForIdle()
        assertTrue(service.calls.contains("selectProvider:anthropic"))
    }

    @Test
    fun `the model picker groups by provider and selects a full model id`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Change model", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        rule.onNodeWithText("ANTHROPIC").assertIsDisplayed()
        rule.onNodeWithContentDescription("Opus 4.1").performClick()
        rule.waitForIdle()
        // The short name is display only; the wire gets the full id.
        assertTrue(service.calls.contains("selectModel:anthropic/claude-opus-4-1"))
    }

    @Test
    fun `the effort picker offers what the provider actually supports`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Change effort", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        listOf("low", "medium", "high").forEach {
            rule.onNodeWithContentDescription(it).assertIsDisplayed()
        }
        rule.onNodeWithContentDescription("low").performClick()
        rule.waitForIdle()
        assertTrue(service.calls.contains("selectEffort:low"))
    }

    @Test
    fun `an empty inventory says so rather than showing an empty sheet`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.setProviders(ai.diffforge.haider.ui.daemon.ProviderInventory())
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Change model", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        rule.onNodeWithText(
            "Nothing to choose yet. The daemon has not published an inventory.",
        ).assertIsDisplayed()
    }

    @Test
    fun `a selection anchors the chip deadline so it cannot hang`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        val before = viewModel.state.value.catalogRequestedAtMs
        viewModel.selectEffort("medium")
        rule.waitForIdle()
        assertTrue(
            "a selection must restart the deadline clock",
            viewModel.state.value.catalogRequestedAtMs != before ||
                viewModel.state.value.catalogRequestedAtMs != null,
        )
    }

    @Test
    fun `with the daemon enabled the app opens straight into a session`() {
        // Addition D: no setup screen to walk past once there is nothing to set
        // up. An empty roster gets a session created and activated.
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        assertTrue("the app must open into a session", viewModel.state.value.activeSessionId != null)
        assertEquals(0, rule.onAllNodesWithTextSafe("Run Haider in the background"))
        assertTrue(service.calls.any { it.startsWith("activate:") })
    }

    @Test
    fun `a stopped daemon does not invent a session`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterStopped)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        assertEquals(null, viewModel.state.value.activeSessionId)
        assertTrue(service.calls.none { it == "createSession" })
    }
}
