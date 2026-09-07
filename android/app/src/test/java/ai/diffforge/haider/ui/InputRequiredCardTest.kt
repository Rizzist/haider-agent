package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.MenuCoordinates
import ai.diffforge.haider.ui.daemon.MenuOption
import ai.diffforge.haider.ui.daemon.NeedsInput
import ai.diffforge.haider.ui.chat.ASK_SECRET_FIELD_TAG
import ai.diffforge.haider.ui.chat.InputRequiredCard
import ai.diffforge.haider.ui.theme.ForgeTheme
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performTextInput
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
class InputRequiredCardTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private val answers = mutableListOf<Triple<String, Int, String?>>()

    private val secrets = mutableListOf<Triple<String, Int, String>>()

    private fun render(
        needsInput: NeedsInput,
        answeredElsewhere: Boolean = false,
        answerable: Boolean = true,
    ) {
        val coordinates = if (answerable) {
            MenuCoordinates.of("s", needsInput, "c")
                ?: MenuCoordinates("s", "menu-fixture", 1, 1, "c")
        } else {
            null
        }
        rule.setContent {
            ForgeTheme(dark = true) {
                InputRequiredCard(
                    needsInput = needsInput,
                    nowMs = FakeDaemonService.FIXED_NOW,
                    answeredElsewhere = answeredElsewhere,
                    coordinates = coordinates,
                    onAnswer = { _, key, index, text -> answers += Triple(key, index, text) },
                    onAnswerSecret = { _, key, index, secret ->
                        secrets += Triple(key, index, String(secret))
                        secret.fill(' ')
                    },
                )
            }
        }
    }

    @Test
    fun `the body is rendered verbatim, never rewritten into prose`() {
        render(
            NeedsInput(
                kind = "approval",
                title = "Send this reply to Amir?",
                safeBody = listOf("“Yes — 4 pm still works.”", "second line"),
                options = listOf(MenuOption("send", "Send it", decision = "allow_once")),
            ),
        )
        rule.onNodeWithText("“Yes — 4 pm still works.”").assertIsDisplayed()
        rule.onNodeWithText("second line").assertIsDisplayed()
    }

    @Test
    fun `answering sends the option key and its index`() {
        render(
            NeedsInput(
                kind = "approval",
                title = "Send it?",
                menuId = "menu-9f2",
                options = listOf(
                    MenuOption("send", "Send it", decision = "allow_once"),
                    MenuOption("skip", "Don't send", decision = "reject_once"),
                ),
            ),
        )
        rule.onNodeWithText("Don't send").performClick()
        assertEquals(Triple("skip", 1, null), answers.last())
    }

    @Test
    fun `three or more options stack instead of crowding a row`() {
        render(
            NeedsInput(
                kind = "choice",
                title = "Which branch?",
                options = listOf(
                    MenuOption("a", "main"),
                    MenuOption("b", "develop"),
                    MenuOption("c", "release"),
                ),
            ),
        )
        listOf("main", "develop", "release").forEach {
            rule.onNodeWithText(it).assertIsDisplayed()
        }
    }

    @Test
    fun `a secret prompt gets a masked field, never a plain one`() {
        render(
            NeedsInput(
                kind = "secret",
                title = "Passphrase for the signing key?",
                menuId = "menu-1",
                requestSeq = 3,
                workerGeneration = 1,
                secretAnswer = true,
            ),
        )
        rule.onNodeWithTag(ASK_SECRET_FIELD_TAG).performTextInput("hunter2-hunter2")
        rule.waitForIdle()
        // Masked: the typed value is not rendered back anywhere.
        assertEquals(0, rule.onAllNodesWithTextSafe("hunter2-hunter2"))
        rule.onNodeWithText("Send").performClick()
        rule.waitForIdle()
        // It leaves through the secret path, not the text path.
        assertEquals("hunter2-hunter2", secrets.single().third)
        assertTrue(answers.isEmpty())
    }

    @Test
    fun `a prompt missing its coordinates offers no answer at all`() {
        render(
            NeedsInput(
                kind = "approval",
                title = "Send it?",
                options = listOf(MenuOption("send", "Send it", decision = "allow_once")),
            ),
            answerable = false,
        )
        rule.onNodeWithText("Send it").assertIsNotEnabled()
    }

    @Test
    fun `a free-text question does get a field`() {
        render(NeedsInput(kind = "question", title = "Which branch should I push to?"))
        assertTrue(rule.onAllNodes(hasSetTextAction()).fetchSemanticsNodes().isNotEmpty())
    }

    @Test
    fun `an unknown kind still renders the daemon's own copy`() {
        render(
            NeedsInput(
                kind = "a_kind_shipped_after_971",
                title = "Something new",
                safeBody = listOf("body"),
            ),
        )
        rule.onNodeWithText("Something new").assertIsDisplayed()
    }

    @Test
    fun `an empty title falls back to the first safe body line`() {
        render(NeedsInput(kind = "question", title = "", safeBody = listOf("Pick one")))
        rule.onNodeWithText("Pick one").assertIsDisplayed()
    }

    @Test
    fun `a race the user did not cause replaces the card, it does not error`() {
        render(
            NeedsInput(
                kind = "approval",
                title = "Send it?",
                options = listOf(MenuOption("send", "Send it", decision = "allow_once")),
            ),
            answeredElsewhere = true,
        )
        rule.onNodeWithText("Answered elsewhere.").assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTextSafe("Send it"))
    }

    @Test
    fun `allow_always carries its consequence in words`() {
        render(
            NeedsInput(
                kind = "permission",
                title = "Allow shell?",
                options = listOf(
                    MenuOption("once", "Allow once", decision = "allow_once"),
                    MenuOption("always", "Always allow", decision = "allow_always"),
                ),
            ),
        )
        rule.onNodeWithText("Won't ask again this session").assertIsDisplayed()
    }
}
