package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.accounts.CustomModelsProbe
import ai.diffforge.haider.ui.accounts.CustomProbeFailure
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.settings.ACCOUNTS_KEY_FIELD_TAG
import ai.diffforge.haider.ui.settings.AccountsScreen
import ai.diffforge.haider.ui.settings.CUSTOM_SERVER_CARD_TAG
import ai.diffforge.haider.ui.settings.CUSTOM_SERVER_KEY_FIELD_TAG
import ai.diffforge.haider.ui.settings.CUSTOM_SERVER_MODEL_FIELD_TAG
import ai.diffforge.haider.ui.theme.ForgeTheme
import androidx.compose.ui.semantics.SemanticsNode
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.isRoot
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The custom-server card on screen: the form, the discovery step, the model
 * pick and the create — plus the two things a person can actually see going
 * wrong, a key that stays visible and a second form appearing beside the first.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class CustomServerScreenTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private val repository = FakeAccountsRepository()
    private val controller = OAuthAttemptController(repository, kotlinx.coroutines.MainScope())

    private fun render() {
        rule.setContent {
            ForgeTheme(dark = true) {
                AccountsScreen(
                    repository = repository,
                    oauth = controller,
                    onBack = {},
                    onOpenUrl = {},
                )
            }
        }
    }

    private fun openCard() {
        render()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Add a custom server").performClick()
        rule.waitForIdle()
    }

    @Test
    fun `the card mirrors the desktop form fields`() {
        openCard()
        rule.onNodeWithTag(CUSTOM_SERVER_CARD_TAG).assertIsDisplayed()
        rule.onNodeWithText("Name").assertIsDisplayed()
        rule.onNodeWithText("Base URL").assertIsDisplayed()
        rule.onNodeWithText("Authentication").assertIsDisplayed()
        rule.onNodeWithText("API flavour").assertIsDisplayed()
        // Both auth modes and both API families the TUI offers.
        rule.onNodeWithContentDescription("API key").assertIsDisplayed()
        rule.onNodeWithContentDescription("No auth").assertIsDisplayed()
        rule.onNodeWithContentDescription("OpenAI-compatible").assertIsDisplayed()
        rule.onNodeWithContentDescription("Anthropic messages").assertIsDisplayed()
        // Nothing can be discovered before a key is typed.
        rule.onNodeWithText("Discover models").assertIsNotEnabled()
    }

    @Test
    fun `discovery offers the returned models and the create carries the chosen one`() {
        openCard()
        rule.onNodeWithTag(CUSTOM_SERVER_KEY_FIELD_TAG).performTextInput("router-key-abcdefgh")
        rule.onNodeWithText("Discover models").performClick()
        rule.waitForIdle()

        rule.onNodeWithContentDescription("router-fast").assertIsDisplayed()
        rule.onNodeWithContentDescription("router-deep").assertIsDisplayed()
        rule.onNodeWithContentDescription("router-fast").performClick()
        rule.onNodeWithText("Add server").performClick()
        rule.waitForIdle()

        val configured = repository.configured.single()
        assertEquals("router-fast", configured.defaultModel)
        assertEquals(listOf("router-fast", "router-deep"), configured.models)
        // The form closed itself, and the credential was claimed for the new
        // provider under the same alias.
        assertEquals(0, rule.onAllNodesWithTagSafe(CUSTOM_SERVER_CARD_TAG))
        assertTrue(repository.snapshot.value.accounts.any { it.alias == "custom" })
    }

    @Test
    fun `the key is masked on screen and gone from the tree once discovery starts`() {
        openCard()
        rule.onNodeWithTag(CUSTOM_SERVER_KEY_FIELD_TAG).performTextInput("CUSTOMKEYSENTINEL4f21")
        rule.waitForIdle()
        assertFalse("the raw key rendered", visibleText().contains("CUSTOMKEYSENTINEL4f21"))

        rule.onNodeWithText("Discover models").performClick()
        rule.waitForIdle()
        // Submit hands the bytes to the vault and the card forgets them, which
        // is what closes the over-the-shoulder window the TUI pins shut.
        assertFalse("the key survived submit", visibleText().contains("CUSTOMKEYSENTINEL4f21"))
    }

    @Test
    fun `choosing no auth hides the key field and skips the credential`() {
        openCard()
        rule.onNodeWithContentDescription("No auth").performClick()
        rule.waitForIdle()
        assertEquals(0, rule.onAllNodesWithTagSafe(CUSTOM_SERVER_KEY_FIELD_TAG))
        rule.onNodeWithText("Discover models").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("router-deep").performClick()
        rule.onNodeWithText("Add server").performClick()
        rule.waitForIdle()

        assertEquals("none", repository.configured.single().authRequirement)
        assertFalse(repository.calls.contains("account.login_api"))
    }

    @Test
    fun `a failed probe offers a typed model id on the same card`() {
        repository.nextProbe = CustomModelsProbe.Failed(
            CustomProbeFailure.Unauthorized,
            "server returned 401 for /models",
        )
        openCard()
        rule.onNodeWithTag(CUSTOM_SERVER_KEY_FIELD_TAG).performTextInput("router-key-abcdefgh")
        rule.onNodeWithText("Discover models").performClick()
        rule.waitForIdle()

        // The error stays on the card — never a flash, never a toast.
        rule.onNodeWithTag(CUSTOM_SERVER_CARD_TAG).assertIsDisplayed()
        rule.onNodeWithText("Add server").assertIsNotEnabled()
        rule.onNodeWithTag(CUSTOM_SERVER_MODEL_FIELD_TAG).performTextInput("llama3.1:8b")
        rule.waitForIdle()
        rule.onNodeWithText("Add server").performClick()
        rule.waitForIdle()

        assertEquals(listOf("llama3.1:8b"), repository.configured.single().models)
    }

    @Test
    fun `opening the custom card replaces a pending API-key form`() {
        // 971-tui-fixes F1b: one pending add form, ever.
        render()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("sk-live-abcdefgh9999")
        rule.waitForIdle()

        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Add a custom server").performClick()
        rule.waitForIdle()

        rule.onNodeWithTag(CUSTOM_SERVER_CARD_TAG).assertIsDisplayed()
        // The replaced form is gone, and so is what was typed into it.
        assertEquals(0, rule.onAllNodesWithTagSafe(ACCOUNTS_KEY_FIELD_TAG))
        assertFalse(visibleText().contains("sk-live-abcdefgh9999"))
    }

    private fun visibleText(): String = rule.onAllNodes(isRoot())
        .fetchSemanticsNodes()
        .flatMap(::collectText)
        .joinToString(" ")

    private fun collectText(node: SemanticsNode): List<String> = buildList {
        node.config.getOrNull(SemanticsProperties.Text)?.forEach { add(it.text) }
        node.config.getOrNull(SemanticsProperties.EditableText)?.let { add(it.text) }
        node.config.getOrNull(SemanticsProperties.ContentDescription)?.let { addAll(it) }
        node.children.forEach { addAll(collectText(it)) }
    }
}
