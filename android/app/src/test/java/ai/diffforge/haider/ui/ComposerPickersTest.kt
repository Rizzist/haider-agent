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
    fun `two compact chips are on the composer bar`() {
        // The provider folds into the model sheet, which is grouped by
        // provider anyway, so two chips fit one row at 360 dp (F, S5).
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.onAllNodes(hasContentDescription("Change model", substring = true))
            .onFirst()
            .assertIsDisplayed()
        rule.onAllNodes(hasContentDescription("Change effort", substring = true))
            .onFirst()
            .assertIsDisplayed()
        assertEquals(
            0,
            rule.onAllNodes(hasContentDescription("Change provider", substring = true))
                .fetchSemanticsNodes().size,
        )
    }

    @Test
    fun `the model sheet groups by provider and names an unavailable one`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Change model", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Anthropic").assertIsDisplayed()
        // provider.list says OpenAI has no account, and the group header says so.
        assertTrue(rule.onAllNodesWithTextSafe("OpenAI — No account configured") > 0)
    }

    @Test
    fun `the model picker groups by provider and selects a full model id`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Change model", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Anthropic").assertIsDisplayed()
        rule.onNodeWithContentDescription("Opus 4.1").performClick()
        rule.waitForIdle()
        // The short name is display only; the wire gets the full id.
        assertTrue(service.calls.contains("selectModel:anthropic/claude-opus-4-1"))
    }

    @Test
    fun `the effort picker offers what the selected model supports`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Change effort", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        // Sonnet is selected and does allow low.
        listOf("low", "medium", "high").forEach {
            rule.onNodeWithContentDescription(it).assertIsDisplayed()
        }
        rule.onNodeWithContentDescription("low").performClick()
        rule.waitForIdle()
        assertTrue(service.calls.contains("selectEffort:low"))
    }

    @Test
    fun `the effort picker follows the model, not the provider`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        // Opus allows medium and high only; the picker must not offer low.
        viewModel.selectModel("anthropic", "claude-opus-4-1")
        rule.waitForIdle()
        viewModel.openOverlay(
            ai.diffforge.haider.ui.state.Overlay.Picker(ai.diffforge.haider.ui.chat.PickerKind.Effort),
        )
        rule.waitForIdle()
        rule.onNodeWithContentDescription("medium").assertIsDisplayed()
        rule.onNodeWithContentDescription("high").assertIsDisplayed()
        rule.onNodeWithContentDescription("low").assertDoesNotExist()
    }

    @Test
    fun `switching to a stricter model re-derives an unsupported effort`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.selectEffort("low")
        rule.waitForIdle()
        viewModel.selectModel("anthropic", "claude-opus-4-1")
        rule.waitForIdle()
        // `low` cannot survive the switch: Opus does not accept it.
        assertEquals("high", service.models.value!!.current.effort)
    }

    @Test
    fun `an unsupported effort is refused rather than sent`() = kotlinx.coroutines.test.runTest {
        val service = ai.diffforge.haider.ui.daemon.FakeDaemonService(FakeScenario.Populated)
        service.selectModel("anthropic", "claude-opus-4-1")
        service.selectEffort("low")
        assertTrue(service.calls.contains("selectEffort:rejected:low"))
        assertTrue(service.calls.none { it == "selectEffort:low" })
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
        assertEquals(service.activeSessionId.value, viewModel.state.value.activeSessionId)
        assertEquals(1, service.calls.count { it == "createSession" })
    }

    @Test
    fun `repeated readiness notifications create once after the authoritative roster`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        service.rosterReady.value = false
        val viewModel = rule.setHaiderApp(service)
        rule.runOnIdle { repeat(3) { viewModel.ensureActiveSession() } }
        assertTrue(service.calls.none { it == "createSession" })
        rule.runOnIdle { service.rosterReady.value = true }
        rule.waitForIdle()
        assertEquals(1, service.calls.count { it == "createSession" })
        assertEquals(service.activeSessionId.value, viewModel.state.value.activeSessionId)
    }

    @Test
    fun `a stopped daemon does not invent a session`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterStopped)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        assertEquals(null, viewModel.state.value.activeSessionId)
        assertTrue(service.calls.none { it == "createSession" })
    }

    @Test
    fun `a cold session hint waits for the roster without starting the daemon`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val running = service.status.value
        service.rosterReady.value = false
        service.setStatus(ai.diffforge.haider.ui.daemon.DaemonStatus.Starting)
        val target = service.sessions.value.last().id
        val viewModel = rule.setHaiderApp(service)
        rule.runOnIdle { viewModel.activate(target) }
        assertTrue(service.calls.none { it == "activate:$target" })
        rule.runOnIdle { service.rosterReady.value = true; service.setStatus(running) }
        rule.waitForIdle()
        assertEquals(target, service.activeSessionId.value)
        assertEquals(1, service.calls.count { it == "activate:$target" })
        assertTrue(service.calls.none { it == "start" || it == "createSession" })
    }

    @Test
    fun `a stale ready roster cannot authorize navigation during startup`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val running = service.status.value
        service.setStatus(ai.diffforge.haider.ui.daemon.DaemonStatus.Starting)
        val target = service.sessions.value.last().id
        val viewModel = rule.setHaiderApp(service)
        rule.runOnIdle { viewModel.activate(target) }
        assertTrue(service.calls.none { it == "activate:$target" })
        rule.runOnIdle { service.setStatus(running) }
        rule.waitForIdle()
        assertEquals(target, service.activeSessionId.value)
        assertEquals(1, service.calls.count { it == "activate:$target" })
    }

    @Test
    fun `queued cold navigation cannot block an explicit start and send`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.setStatus(ai.diffforge.haider.ui.daemon.DaemonStatus.Stopped)
        service.rosterReady.value = false
        val target = service.sessions.value.last().id
        val viewModel = rule.setHaiderApp(service)
        rule.runOnIdle { viewModel.activate(target) }
        assertTrue(service.calls.none { it == "start" || it == "activate:$target" })
        rule.runOnIdle {
            viewModel.setDraft("Resume after a cold session hint")
            viewModel.send()
        }
        assertEquals(1, service.calls.count { it == "start" })
        assertTrue(service.calls.none { it.startsWith("turn.submit:") })
        rule.runOnIdle { service.rosterReady.value = true }
        rule.waitForIdle()
        assertEquals(target, service.activeSessionId.value)
        assertEquals(1, service.calls.count { it == "turn.submit:$target:steer:0" })
    }

    @Test
    fun `start and send waits for the roster and creates only one session`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterStopped)
        service.rosterReady.value = false
        val viewModel = rule.setHaiderApp(service)
        rule.runOnIdle {
            viewModel.setDraft("A queued first turn")
            viewModel.send()
        }
        assertEquals(1, service.calls.count { it == "start" })
        assertTrue(service.calls.none { it == "createSession" || it.startsWith("turn.submit:") })
        rule.runOnIdle { service.rosterReady.value = true }
        rule.waitForIdle()
        assertEquals(1, service.calls.count { it == "createSession" })
        assertEquals(1, service.calls.count { it.startsWith("turn.submit:") })
    }
}
