package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.settings.ACCOUNTS_KEY_FIELD_TAG
import ai.diffforge.haider.ui.settings.AccountsScreen
import ai.diffforge.haider.ui.state.PermissionClassifier
import ai.diffforge.haider.ui.theme.ForgeDark
import ai.diffforge.haider.ui.theme.ForgeTheme
import android.graphics.Bitmap
import android.graphics.Canvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertTextEquals
import androidx.compose.ui.test.junit4.AndroidComposeTestRule
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performImeAction
import androidx.compose.ui.test.performTextInput
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.MainScope
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The findings that only a real device could produce, pinned so they cannot
 * come back. Each of these passed the JVM suite before the app was ever run.
 */
@RunWith(AndroidJUnit4::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class DeviceRegressionTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

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
    fun `Save keeps its filled accent once the key is typed and the keyboard closes`() {
        // Round 5, P2: on the device the Save button went from a solid ember
        // pill to an invisible one the moment the IME came down. Nothing about
        // its state had changed — `enabled` was still true — so the assertion
        // has to be about the pixels, not the flag.
        accounts()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("fake-971-verify-only-1234")
        rule.waitForIdle()
        // Close the IME the way a person does: focus leaves the field.
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performImeAction()
        rule.runOnUiThread {
            WindowInsetsControllerCompat(rule.activity.window, rule.activity.window.decorView)
                .hide(WindowInsetsCompat.Type.ime())
        }
        rule.waitForIdle()

        rule.onNodeWithText("Save").assertIsEnabled()
        val bounds = rule.onNodeWithText("Save").fetchSemanticsNode().boundsInWindow
        val accent = ForgeDark.accent.toArgb()
        var filled = 0
        var total = 0
        val window = rule.captureWindow()
        for (y in bounds.top.toInt() until bounds.bottom.toInt()) {
            for (x in bounds.left.toInt() until bounds.right.toInt()) {
                if (x !in 0 until window.width || y !in 0 until window.height) continue
                total++
                if (window.getPixel(x, y) == accent) filled++
            }
        }
        assertTrue("the Save button was not on screen to sample", total > 0)
        // The pill is the visual height inside a 48 dp target, so a solid fill
        // is a third of the node, not all of it. Before the fix it was zero.
        assertTrue(
            "Save painted $filled accent pixels of $total: the fill is gone",
            filled > total / 3,
        )
    }

    @Test
    fun `validating consumes the key instead of leaving it in the field`() {
        accounts()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("fake-971-verify-only-1234")
        rule.onNodeWithText("Validate").performClick()
        rule.waitForIdle()
        // "Key validated" used to leave the masked key sitting there.
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).assertTextEquals("")
        rule.onNodeWithText("Key validated.").assertExists()
        // It was staged, so Save has something to commit without the plaintext.
        assertTrue(repository.calls.contains("vault.stage"))
    }

    @Test
    fun `a validated key saves from its staged reference alone`() {
        accounts()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("fake-971-verify-only-1234")
        rule.onNodeWithText("Validate").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Save").performClick()
        rule.waitForIdle()
        assertTrue(repository.calls.contains("account.login_api"))
        assertTrue(repository.snapshot.value.accounts.any { it.provider == "openai" })
        // Exactly one staging: the key was not re-read to commit it.
        assertEquals(1, repository.calls.count { it == "vault.stage" })
    }

    @Test
    fun `cancelling after validation discards the staged reference`() {
        accounts()
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("API key").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("OpenAI").performClick()
        rule.onNodeWithTag(ACCOUNTS_KEY_FIELD_TAG).performTextInput("fake-971-verify-only-1234")
        rule.onNodeWithText("Validate").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Cancel").performClick()
        rule.waitForIdle()
        assertFalse(repository.calls.contains("account.login_api"))
        assertTrue(repository.snapshot.value.accounts.none { it.provider == "openai" })
    }

    @Test
    fun `a stale click cannot answer the prompt that replaced it`() {
        val service = ComposeHost.install(FakeScenario.InputRequiredHere)
        rule.setHaiderApp(service)
        rule.waitForIdle()
        val original = service.sessions.value.first { it.id == "s-sms" }.needsInput!!
        // Capture the callback the card was drawn with...
        val staleClick = rule.onNodeWithText("Send it")
            .fetchSemanticsNode()
            .config[SemanticsActions.OnClick]
            .action!!
        // ...then let the daemon replace the prompt underneath it.
        rule.runOnIdle {
            service.setSessions(
                service.sessions.value.map { row ->
                    if (row.id == "s-sms") {
                        row.copy(
                            needsInput = original.copy(menuId = "replacement-menu", requestSeq = 999),
                        )
                    } else {
                        row
                    }
                },
            )
        }
        rule.waitForIdle()
        rule.runOnIdle { staleClick() }
        rule.waitForIdle()
        // Nothing was answered: not the old menu, and certainly not the new one.
        assertTrue(
            "a stale click answered a replacement prompt: ${service.calls}",
            service.calls.none { it.startsWith("menu.answer:") },
        )
    }

    @Test
    fun `granting the notification permission advances first run`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("Let Haider notify you") > 0)

        // What the Activity's permission callback does. An empty callback left
        // first run stuck on step 2 with the permission already granted.
        viewModel.onNotificationPermissionResult(granted = true, permanentlyDenied = false)
        rule.waitForIdle()

        assertTrue(service.calls.contains("permissions.notifications:true"))
        assertTrue(viewModel.state.value.environment.notificationsGranted)
        assertEquals(0, rule.onAllNodesWithTextSafe("Notifications are off"))
        assertTrue(rule.onAllNodesWithTextSafe("Notifications allowed") > 0)
    }

    @Test
    fun `a refusal with no rationale left becomes the settings path`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        val viewModel = rule.setHaiderApp(service)
        viewModel.onNotificationPermissionResult(granted = false, permanentlyDenied = true)
        rule.waitForIdle()
        assertTrue(viewModel.state.value.environment.notificationsPermanentlyDenied)
    }

    @Test
    fun `a fresh install is never-asked, not permanently denied`() {
        // `shouldShowRequestPermissionRationale` is false before the first
        // request and after the last refusal. Reading it alone made a fresh
        // install claim "Android will not ask again" — while the next tap still
        // opened the dialog.
        val service = ComposeHost.install(FakeScenario.FirstRun)
        val viewModel = rule.setHaiderApp(service)
        viewModel.onNotificationPermissionResult(
            granted = false,
            permanentlyDenied = PermissionClassifier.permanentlyDenied(
                granted = false,
                everRequested = false,
                shouldShowRationale = false,
            ),
        )
        rule.waitForIdle()
        assertFalse(viewModel.state.value.environment.notificationsPermanentlyDenied)
        assertEquals(
            0,
            rule.onAllNodes(
                androidx.compose.ui.test.hasContentDescription(
                    "Android will not ask again",
                    substring = true,
                ),
            ).fetchSemanticsNodes().size,
        )
        // The step is still the ordinary ask, not a trip to app settings.
        assertTrue(rule.onAllNodesWithTextSafe("Let Haider notify you") > 0)
    }

    @Test
    fun `a second refusal, after a real request, does read as permanent`() {
        val service = ComposeHost.install(FakeScenario.NotificationsDenied)
        val viewModel = rule.setHaiderApp(service)
        viewModel.onNotificationPermissionResult(
            granted = false,
            permanentlyDenied = PermissionClassifier.permanentlyDenied(
                granted = false,
                everRequested = true,
                shouldShowRationale = false,
            ),
        )
        rule.waitForIdle()
        assertTrue(viewModel.state.value.environment.notificationsPermanentlyDenied)
        // The strip is one line; the detail reaches the screen reader (F, S2).
        assertTrue(
            rule.onAllNodes(
                androidx.compose.ui.test.hasContentDescription(
                    "Android will not ask again",
                    substring = true,
                ),
            ).fetchSemanticsNodes().isNotEmpty(),
        )
    }

    /**
     * Robolectric has no window to PixelCopy from, so the pixels come from the
     * decor view drawn into a software bitmap — the same thing Roborazzi does,
     * without depending on a record flag being set.
     */
    private fun AndroidComposeTestRule<*, MainActivity>.captureWindow(): Bitmap {
        val root = activity.window.decorView
        val bitmap = Bitmap.createBitmap(root.width, root.height, Bitmap.Config.ARGB_8888)
        runOnUiThread { root.draw(Canvas(bitmap)) }
        return bitmap
    }

}
