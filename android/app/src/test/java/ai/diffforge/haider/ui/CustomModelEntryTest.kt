package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.accounts.ModelInventoryAuthority
import ai.diffforge.haider.ui.chat.CUSTOM_MODEL_ENTRY_TAG
import ai.diffforge.haider.ui.chat.CUSTOM_MODEL_FIELD_TAG
import ai.diffforge.haider.ui.chat.CustomModelEntry
import ai.diffforge.haider.ui.chat.MODEL_PICKER_LIST_TAG
import ai.diffforge.haider.ui.chat.MODEL_REFUSAL_TAG
import ai.diffforge.haider.ui.chat.SelectionRefusalPanel
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.SelectionRefusal
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
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * A free-text model id, and what the refusal panel does with the code it can
 * now produce.
 *
 * The entry exists only where the daemon says its inventory is ADVISORY. The
 * refusal it makes reachable — `model_unknown` — is the one the panel used to
 * offer a confirmation for; confirming that sends the identical request with
 * one more field, so the panel says so instead of offering the button.
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
    private var confirmed = 0
    private var kept = 0

    private fun renderEntry(authority: ModelInventoryAuthority) {
        rule.setContent {
            ForgeTheme(dark = true) {
                CustomModelEntry(
                    authority = authority,
                    enabled = true,
                    onSelect = { chosen += it },
                )
            }
        }
    }

    private fun renderRefusal(refusal: SelectionRefusal) {
        rule.setContent {
            ForgeTheme(dark = true) {
                SelectionRefusalPanel(
                    refusal = refusal,
                    onConfirm = { confirmed += 1 },
                    onKeep = { kept += 1 },
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

    @Test
    fun `the picker carries one entry, under the advisory provider`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.refreshModels()
        viewModel.openOverlay(Overlay.NewSessionWith)
        rule.waitForIdle()
        rule.onNodeWithTag(MODEL_PICKER_LIST_TAG)
            .performScrollToNode(hasContentDescription("Custom model…"))
        rule.waitForIdle()

        // The seeded catalog has exactly one advisory provider; the two
        // authoritative ones get no entry at all.
        assertEquals(1, rule.onAllNodesWithTagSafe(CUSTOM_MODEL_ENTRY_TAG))
    }

    // ---------- the refusal the entry makes reachable ----------

    @Test
    fun `an unknown model is refused without a confirmation button`() {
        renderRefusal(
            SelectionRefusal(
                code = SelectionRefusalCodes.MODEL_UNKNOWN,
                provider = "local-lab",
                model = "router-nope",
            ),
        )
        rule.onNodeWithTag(MODEL_REFUSAL_TAG).assertIsDisplayed()
        rule.onNodeWithText("router-nope is not in local-lab's known model list.")
            .assertIsDisplayed()
        rule.onNodeWithText("Confirming will not change this: the daemon refused the row itself.")
            .assertIsDisplayed()
        // The one button in the app that may set `confirm_new_epoch` is absent,
        // because this refusal is not about consent.
        assertEquals(0, rule.onAllNodesWithTextSafe("Change it anyway"))
        rule.onNodeWithText("Close").performClick()
        assertEquals(0, confirmed)
        assertEquals(1, kept)
    }

    @Test
    fun `an uncreatable provider says so, and offers no confirmation either`() {
        renderRefusal(
            SelectionRefusal(
                code = SelectionRefusalCodes.PROVIDER_UNAVAILABLE,
                provider = "openai",
                model = "gpt-5",
            ),
        )
        rule.onNodeWithText("openai cannot be created on this daemon.").assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTextSafe("Change it anyway"))
    }

    @Test
    fun `a refusal this client does not recognise is never promoted to confirmable`() {
        renderRefusal(SelectionRefusal(code = "selection_refused"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Change it anyway"))
        rule.onNodeWithText("Close").assertIsDisplayed()
    }

    @Test
    fun `a cache-epoch refusal keeps its confirmation`() {
        renderRefusal(
            SelectionRefusal(
                code = SelectionRefusalCodes.CACHE_EPOCH_CONFIRMATION_REQUIRED,
                provider = "local-lab",
                model = "router-experimental",
            ),
        )
        // This is the one refusal a second step actually fixes.
        assertEquals(
            0,
            rule.onAllNodesWithTextSafe(
                "Confirming will not change this: the daemon refused the row itself.",
            ),
        )
        rule.onNodeWithText("Change it anyway").performClick()
        assertEquals(1, confirmed)
        rule.onNodeWithText("Keep the current one").assertIsDisplayed()
    }
}
