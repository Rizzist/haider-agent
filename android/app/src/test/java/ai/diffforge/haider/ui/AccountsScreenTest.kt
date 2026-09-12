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
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
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

    /**
     * With the daemon stopped, the initial provider read throws
     * `IOException("connection_lost")` straight out of the load effect, and
     * unguarded it killed the process on entry (971-V round 2,
     * `accounts-crash.log`). The throw must become the unavailable surface.
     */
    @Test
    fun `a stopped daemon yields the unavailable surface, not a crash`() {
        repository.providersFailure = "connection_lost"
        render()
        rule.waitForIdle()
        rule.onNodeWithText("Accounts is unavailable — connection_lost").assertIsDisplayed()
        // Withheld, not emptied: no rows to misread as "you have none", and no
        // Add door whose every save could only fail.
        assertEquals(0, rule.onAllNodesWithTextSafe("Work"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Add account"))
    }

    @Test
    fun `an existing account shows its provider and masked identity`() {
        render()
        rule.onNodeWithText("Work").assertIsDisplayed()
        // Glyph + label + identity line; the ACTIVE badge is a check now (A1).
        assertTrue(rule.onAllNodesWithTextSafe("you@anthropic · sign-in") > 0)
        assertEquals(0, rule.onAllNodesWithTextSafe("ACTIVE"))
    }

    @Test
    fun `adding an api key stages, validates and clears the field`() {
        render()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
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
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
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
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("short")
        rule.onNodeWithText("Save").performClick()
        rule.waitForIdle()
        assertEquals(before, repository.snapshot.value.accounts.size)
        // Save is what validates now, so a short key is refused there.
        rule.onNodeWithText("invalid_api_key").assertIsDisplayed()
    }

    /**
     * A fresh profile: DeepSeek is the fixture's `available = false` provider,
     * which is exactly the state every built-in provider is in before it has a
     * credential. It must still be selectable, and Save must still enable
     * (971-V F5 — every choice was greyed out and Save never enabled).
     */
    @Test
    fun `an unavailable built-in provider can still be given a key`() {
        render()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("DeepSeek").assertIsEnabled()
        rule.onNodeWithContentDescription("DeepSeek").performClick()
        // Save is disabled until there is a key, and enabled once there is one.
        rule.onNodeWithText("Save").assertIsNotEnabled()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("sk-live-abcdefgh9999")
        rule.waitForIdle()
        rule.onNodeWithText("Save").assertIsEnabled()
        rule.onNodeWithText("Save").performClick()
        rule.waitForIdle()
        assertTrue(repository.calls.contains("account.login_api"))
        assertTrue(repository.snapshot.value.accounts.any { it.provider == "deepseek" })
    }

    /** The same rule on the sign-in form: Start cannot be unreachable (971-V F5). */
    @Test
    fun `sign-in offers every provider that declares oauth`() {
        render()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Sign in with a provider").performClick()
        rule.waitForIdle()
        // Google declares no OAuth at all, so it is not on this form; every row
        // that IS on it can be chosen.
        assertEquals(0, rule.onAllNodesWithContentDescriptionSafe("Google"))
        rule.onNodeWithContentDescription("Anthropic").assertIsEnabled()
        rule.onNodeWithText("Start sign-in").assertIsNotEnabled()
        rule.onNodeWithContentDescription("Anthropic").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Start sign-in").assertIsEnabled()
    }

    @Test
    fun `starting a sign-in opens the daemon-supplied url and waits`() {
        render()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Sign in with a provider").performClick()
        rule.waitForIdle()
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
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Sign in with a provider").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Kimi").performClick()
        rule.onNodeWithText("Start sign-in").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Code: HAID-971").assertIsDisplayed()
    }

    @Test
    fun `removing an account is behind its detail sheet and a confirmation`() {
        render()
        // No destructive button on a list row (A1).
        assertEquals(0, rule.onAllNodesWithTextSafe("Delete"))
        rule.onNodeWithText("Work").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Delete").performClick()
        rule.waitForIdle()
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
