package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.settings.ACCOUNTS_KEY_FIELD_TAG
import ai.diffforge.haider.ui.settings.AccountsScreen
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
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Settings -> Accounts, driven by the fake repository. The security assertions
 * matter most: a key must be masked on screen, cleared after save, and absent
 * from every semantics node the platform can read back.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class AccountsScreenTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private val repository = FakeAccountsRepository()
    private val controller = OAuthAttemptController(repository, kotlinx.coroutines.MainScope())
    private val opened = mutableListOf<String>()

    private fun render() {
        rule.setContent {
            ForgeTheme(dark = true) {
                AccountsScreen(
                    repository = repository,
                    oauth = controller,
                    onBack = {},
                    onOpenUrl = { opened += it },
                )
            }
        }
    }

    @Test
    fun `an existing account shows its provider and masked identity`() {
        render()
        rule.onNodeWithText("Work").assertIsDisplayed()
        assertTrue(rule.onAllNodesWithTextSafe("anthropic · sign-in · you@anthropic") > 0)
    }

    @Test
    fun `adding an api key stages, validates and clears the field`() {
        render()
        rule.onNodeWithText("Add API key").performClick()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("sk-live-abcdefgh9999")
        rule.onNodeWithText("Save").performClick()
        rule.waitForIdle()

        assertTrue(repository.calls.contains("vault.stage"))
        assertTrue(repository.calls.contains("account.login_api"))
        rule.onNodeWithText("API key added and validated.").assertIsDisplayed()
        // The field is cleared, and the form is closed.
        assertEquals(0, rule.onAllNodesWithTextSafe("sk-live-abcdefgh9999"))
    }

    @Test
    fun `the key never appears in the semantics tree in clear text`() {
        render()
        rule.onNodeWithText("Add API key").performClick()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("sk-live-abcdefgh9999")
        rule.waitForIdle()

        // The field is masked: what a screen reader or a screenshot can see is
        // the transformed text, not the key.
        val visible = rule.onAllNodes(androidx.compose.ui.test.isRoot())
            .fetchSemanticsNodes()
            .flatMap { collectText(it) }
            .joinToString(" ")
        assertFalse("the raw key leaked into a rendered node", visible.contains("sk-live-abcdefgh9999"))
    }

    @Test
    fun `a short key is refused and no account is created`() {
        render()
        val before = repository.snapshot.value.accounts.size
        rule.onNodeWithText("Add API key").performClick()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("short")
        rule.onNodeWithText("Save").performClick()
        rule.waitForIdle()
        assertEquals(before, repository.snapshot.value.accounts.size)
        rule.onNodeWithText("invalid_api_key").assertIsDisplayed()
    }

    @Test
    fun `starting a sign-in opens the daemon-supplied url and waits`() {
        render()
        rule.onNodeWithText("Sign in with a provider").performClick()
        rule.onNodeWithContentDescription("Anthropic").performClick()
        rule.onNodeWithText("Start sign-in").performClick()
        rule.waitForIdle()

        assertEquals(1, opened.size)
        // The daemon composed the loopback URL; the UI never invents a redirect.
        assertTrue(opened.single().startsWith("http://127.0.0.1"))
        rule.onNodeWithText("Finish the sign-in in your browser, then come back.").assertIsDisplayed()
        rule.onNodeWithText("The return link only brings you back here. It carries no code and no token.")
            .assertIsDisplayed()
    }

    @Test
    fun `a device flow shows its user code instead of a redirect`() {
        render()
        rule.onNodeWithText("Sign in with a provider").performClick()
        rule.onNodeWithContentDescription("Kimi").performClick()
        rule.onNodeWithText("Start sign-in").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Code: HAID-971").assertIsDisplayed()
    }

    @Test
    fun `removing an account calls the remove door`() {
        render()
        rule.onNodeWithText("Delete").performClick()
        rule.waitForIdle()
        assertTrue(repository.calls.contains("account.remove"))
        rule.onNodeWithText("Account removed.").assertIsDisplayed()
    }

    private fun collectText(node: androidx.compose.ui.semantics.SemanticsNode): List<String> =
        buildList {
            node.config.getOrNull(SemanticsProperties.Text)?.forEach { add(it.text) }
            node.config.getOrNull(SemanticsProperties.EditableText)?.let { add(it.text) }
            node.config.getOrNull(SemanticsProperties.ContentDescription)?.let { addAll(it) }
            node.children.forEach { addAll(collectText(it)) }
        }
}
