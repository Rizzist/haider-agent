package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.accounts.ModelInventoryAuthority
import ai.diffforge.haider.ui.chat.CUSTOM_MODEL_ENTRY_TAG
import ai.diffforge.haider.ui.chat.CUSTOM_MODEL_FIELD_TAG
import ai.diffforge.haider.ui.chat.CustomModelEntry
import ai.diffforge.haider.ui.chat.MODEL_PICKER_LIST_TAG
import ai.diffforge.haider.ui.chat.MODEL_REFUSAL_TAG
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.SelectionRefusalCodes
import ai.diffforge.haider.ui.theme.ForgeTheme
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollToNode
import androidx.compose.ui.test.performTextInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * A free-text model id, and what happens when the daemon refuses it.
 *
 * The entry exists only where the daemon says its inventory is ADVISORY, and
 * the refusal it can produce — `model_unknown` — is the one the panel used to
 * offer a confirmation for. Confirming that sends the identical request with
 * one more field; the panel now says so instead.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class CustomModelEntryTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private val chosen = mutableListOf<String>()

    private fun renderEntry(authority: ModelInventoryAuthority, enabled: Boolean = true) {
        rule.setContent {
            ForgeTheme(dark = true) {
                CustomModelEntry(
                    authority = authority,
                    enabled = enabled,
                    onSelect = { chosen += it },
                )
            }
        }
    }

    // ---------- the gate ----------

    @Test
    fun `an authoritative catalog offers no free-text id`() {
        // A miss in the daemon's own catalog is a real miss; `select_model`
        // refuses it, and offering the entry would be offering a refusal.
        renderEntry(ModelInventoryAuthority.Authoritative)
        assertEquals(0, rule.onAllNodesWithTagSafe(CUSTOM_MODEL_ENTRY_TAG))
    }

    @Test
    fun `an unstated authority is treated as authoritative`() {
        // An older summary said nothing. A guess in the permissive direction is
        // the one that hurts, so it stays closed.
        renderEntry(ModelInventoryAuthority.Unknown)
        assertEquals(0, rule.onAllNodesWithTagSafe(CUSTOM_MODEL_ENTRY_TAG))
    }

    @Test
    fun `an advisory catalog opens a field and refuses an empty id`() {
        renderEntry(ModelInventoryAuthority.Advisory)
        rule.onNodeWithContentDescription("Custom model…").performClick()
        rule.waitForIdle()
        rule.onNodeWithText(
            "This server accepts ids its list does not carry, so a passthrough id is worth trying.",
        ).assertIsDisplayed()
        rule.onNodeWithText("Use this model").assertIsNotEnabled()

        rule.onNodeWithTag(CUSTOM_MODEL_FIELD_TAG).performTextInput("  llama3.1:8b  ")
        rule.waitForIdle()
        rule.onNodeWithText("Use this model").performClick()
        rule.waitForIdle()
        // Trimmed, and otherwise handed over exactly as typed: the daemon owns
        // whether the id exists.
        assertEquals(listOf("llama3.1:8b"), chosen)
    }

    // ---------- through the picker ----------

    private fun openPicker(): Pair<ai.diffforge.haider.ui.daemon.FakeDaemonService, ai.diffforge.haider.ui.chat.ChatViewModel> {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.refreshModels()
        viewModel.openOverlay(Overlay.NewSessionWith)
        rule.waitForIdle()
        return service to viewModel
    }

    private fun scrollToEntry() {
        rule.onNodeWithTag(MODEL_PICKER_LIST_TAG)
            .performScrollToNode(hasContentDescription("Custom model…"))
        rule.waitForIdle()
    }

    @Test
    fun `a typed id goes through select_model, not a local catalog edit`() {
        val (service, _) = openPicker()
        scrollToEntry()
        rule.onNodeWithContentDescription("Custom model…").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(CUSTOM_MODEL_FIELD_TAG).performTextInput("router-experimental")
        rule.onNodeWithText("Use this model").performClick()
        rule.waitForIdle()
        // A typed id takes the SAME cache-epoch pre-warning every other model
        // row takes: it changes the model just as much.
        rule.onNodeWithText("Switch to local-lab / router-experimental?").assertIsDisplayed()
        rule.onNodeWithText("Confirm change").performClick()
        rule.waitForIdle()

        // The advisory provider is the one the entry belongs to, and the id is
        // sent to the daemon rather than added to a local list.
        assertTrue(
            "expected a select_model for the advisory provider: ${service.calls}",
            service.calls.contains("selectModel:local-lab/router-experimental"),
        )
    }

    @Test
    fun `a refused unknown model is not offered a confirmation`() {
        val (service, _) = openPicker()
        service.failNextSelection(SelectionRefusalCodes.MODEL_UNKNOWN)
        scrollToEntry()
        rule.onNodeWithContentDescription("Custom model…").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(CUSTOM_MODEL_FIELD_TAG).performTextInput("router-nope")
        rule.onNodeWithText("Use this model").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Confirm change").performClick()
        rule.waitForIdle()

        rule.onNodeWithTag(MODEL_REFUSAL_TAG).assertIsDisplayed()
        rule.onNodeWithText("router-nope is not in local-lab's known model list.").assertIsDisplayed()
        rule.onNodeWithText("Confirming will not change this: the daemon refused the row itself.")
            .assertIsDisplayed()
        // The one button in the app that may set `confirm_new_epoch` is absent,
        // because this refusal is not about consent.
        assertEquals(0, rule.onAllNodesWithTextSafe("Change it anyway"))
        rule.onNodeWithText("Close").assertIsDisplayed()
        assertEquals(0, service.confirmedSelections)
    }

    @Test
    fun `a cache-epoch refusal still gets its confirmation`() {
        val (service, _) = openPicker()
        service.failNextSelection(SelectionRefusalCodes.CACHE_EPOCH_CONFIRMATION_REQUIRED)
        scrollToEntry()
        rule.onNodeWithContentDescription("Custom model…").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(CUSTOM_MODEL_FIELD_TAG).performTextInput("router-experimental")
        rule.onNodeWithText("Use this model").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Confirm change").performClick()
        rule.waitForIdle()

        rule.onNodeWithText("Change it anyway").performClick()
        rule.waitForIdle()
        // Only a person's tap sets it, and it was set exactly once.
        assertEquals(1, service.confirmedSelections)
    }
}
