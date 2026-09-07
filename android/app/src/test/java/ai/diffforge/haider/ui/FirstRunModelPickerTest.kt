package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
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
 * First run's "Pick a model" opened the legacy sheet, which never requested the
 * missing catalog and had no deadline: two device captures 12.5 seconds apart
 * were identical, both reading "Asking the daemon for its model catalog…".
 *
 * The step now opens the canonical picker, which asks on entry and obeys the
 * same 6 s rule as the composer chip.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class FirstRunModelPickerTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `opening the picker asks the daemon for what it is missing`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        // First run has no catalog at all.
        assertEquals(null, service.models.value)

        viewModel.openOverlay(
            ai.diffforge.haider.ui.state.Overlay.Picker(ai.diffforge.haider.ui.chat.PickerKind.Model),
        )
        rule.waitForIdle()

        assertTrue(
            "opening the picker must request what it is missing: ${service.calls}",
            service.calls.contains("provider.list") && service.calls.contains("refreshModels"),
        )
    }

    @Test
    fun `an empty catalog resolves to a retry rather than a spinner`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        // The daemon has nothing to publish, so the request resolves to an
        // error rather than staying in flight. The sheet must say so.
        service.catalogUnavailable = true
        service.setProviders(ai.diffforge.haider.ui.daemon.ProviderInventory())
        service.setModels(null)
        viewModel.openOverlay(
            ai.diffforge.haider.ui.state.Overlay.Picker(ai.diffforge.haider.ui.chat.PickerKind.Model),
        )
        rule.waitForIdle()

        rule.onNodeWithText(
            "Nothing to choose yet. The daemon has not published an inventory.",
        ).assertIsDisplayed()
        rule.onNodeWithText("Retry").assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTextSafe("Asking the daemon for its catalog…"))
    }

    @Test
    fun `retry asks again`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        val viewModel = rule.setHaiderApp(service)
        service.catalogUnavailable = true
        service.setProviders(ai.diffforge.haider.ui.daemon.ProviderInventory())
        service.setModels(null)
        viewModel.openOverlay(
            ai.diffforge.haider.ui.state.Overlay.Picker(ai.diffforge.haider.ui.chat.PickerKind.Model),
        )
        rule.waitForIdle()
        val before = service.calls.count { it == "refreshModels" }
        rule.onNodeWithText("Retry").performClick()
        rule.waitForIdle()
        assertTrue(service.calls.count { it == "refreshModels" } > before)
    }

    @Test
    fun `a resolved catalog lists models instead of any notice`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(
            ai.diffforge.haider.ui.state.Overlay.Picker(ai.diffforge.haider.ui.chat.PickerKind.Model),
        )
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("Sonnet 4.5") > 0)
        assertEquals(0, rule.onAllNodesWithTextSafe("Asking the daemon for its catalog…"))
        assertEquals(
            0,
            rule.onAllNodesWithTextSafe(
                "Nothing to choose yet. The daemon has not published an inventory.",
            ),
        )
    }
}
