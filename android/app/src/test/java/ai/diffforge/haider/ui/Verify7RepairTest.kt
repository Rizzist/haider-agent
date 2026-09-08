package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.chat.MODEL_REFUSAL_TAG
import ai.diffforge.haider.ui.chat.PickerKind
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.settings.ACCOUNTS_KEY_FIELD_TAG
import ai.diffforge.haider.ui.settings.AccountsScreen
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.theme.ForgeTheme
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.MainScope
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Round-7 device findings. Both P2s are the same species as round 6's: the
 * logic was right and the surface was wrong — a refusal delivered to a sheet
 * that had already closed, and a key the vault already had still sitting in a
 * field with a reveal button next to it.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class Verify7RepairTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    // ---------- P2: the composer picker shows the refusal ----------

    @Test
    fun `a refusal from the composer picker is shown, not swallowed by the dismiss`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.failNextSelection("confirm_new_epoch_required")
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Picker(PickerKind.Effort))
        rule.waitForIdle()

        // Tapping used to call onDismiss() in the same breath as the selection.
        rule.onNodeWithText("medium").performClick()
        rule.waitForIdle()

        rule.onNodeWithTag(MODEL_REFUSAL_TAG).assertIsDisplayed()
        rule.onNodeWithText("confirm_new_epoch_required").assertIsDisplayed()
        assertEquals("nothing may be confirmed on the user's behalf", 0, service.confirmedSelections)

        rule.onNodeWithText("Change it anyway").performClick()
        rule.waitForIdle()
        assertEquals(1, service.confirmedSelections)
    }

    @Test
    fun `a selection that the daemon accepts still closes the picker`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Picker(PickerKind.Effort))
        rule.waitForIdle()
        rule.onNodeWithText("medium").performClick()
        rule.waitForIdle()
        // Waiting for the answer must not leave the sheet stuck open.
        assertEquals(Overlay.None, viewModel.state.value.overlay)
        assertEquals(0, rule.onAllNodesWithTextSafe("The daemon refused that change"))
    }

    // ---------- P2: the key does not outlive its staging ----------

    private val repository = FakeAccountsRepository()

    private fun accounts() {
        rule.setContent {
            ForgeTheme(dark = true) {
                AccountsScreen(
                    repository = repository,
                    oauth = OAuthAttemptController(repository, MainScope()),
                    onBack = {},
                    onOpenUrl = {},
                )
            }
        }
    }

    @Test
    fun `the key is gone from the field the moment the vault has it`() {
        accounts()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        // The commit is held open, so this assertion runs in exactly the
        // window the finding is about: the vault has the key and login has not
        // come back yet.
        repository.holdCommit = true
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("fake-971-verify-only-1234")
        rule.onNodeWithText("Save").performClick()
        rule.waitForIdle()

        assertTrue(repository.calls.contains("vault.stage"))
        assertTrue("login has not returned yet", repository.calls.contains("account.login_api"))
        val field = rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).fetchSemanticsNode()
        val editable = field.config.getOrNull(SemanticsProperties.EditableText)?.text.orEmpty()
        assertEquals("the plaintext survived staging", "", editable)
        assertEquals(0, rule.onAllNodesWithTextSafe("fake-971-verify-only-1234"))
        repository.releaseCommit()
    }
}
