package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.checkpoints.BRANCHES_SHEET_TAG
import ai.diffforge.haider.ui.checkpoints.CHECKPOINTS_SHEET_TAG
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.settings.AccountsScreen
import ai.diffforge.haider.ui.settings.CUSTOM_SERVER_KEY_FIELD_TAG
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.theme.ForgeTheme
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * Goldens for this lane's three surfaces, in both themes.
 *
 * The Accounts card is photographed through `onRoot()` like every other
 * full-screen surface. The two sheets are captured by their own node, because
 * a `ModalBottomSheet` renders in its own window and `onRoot()` cannot name one
 * node for it — the same reason the picker sheets are absent from
 * [ScreenshotTest].
 *
 * Record with `-Proborazzi.test.record=true`.
 */
@RunWith(AndroidJUnit4::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class ExtrasScreenshotTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    // ---------- the custom-server card ----------

    private fun captureCustomServer(name: String, dark: Boolean, discovered: Boolean) {
        val repository = FakeAccountsRepository()
        rule.setContent {
            ForgeTheme(dark = dark) {
                AccountsScreen(
                    repository = repository,
                    oauth = OAuthAttemptController(repository, kotlinx.coroutines.MainScope()),
                    onBack = {},
                    onOpenUrl = {},
                )
            }
        }
        rule.onNodeWithText("Add account").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Add a custom server").performClick()
        rule.waitForIdle()
        if (discovered) {
            rule.onNodeWithTag(CUSTOM_SERVER_KEY_FIELD_TAG).performTextInput("router-key-abcdefgh")
            rule.onNodeWithText("Discover models").performClick()
            rule.waitForIdle()
        }
        rule.onRoot().captureRoboImage("src/test/screenshots/$name.png")
    }

    @Test fun `custom server form dark`() =
        captureCustomServer("custom-server-form-dark", dark = true, discovered = false)

    @Test fun `custom server form light`() =
        captureCustomServer("custom-server-form-light", dark = false, discovered = false)

    @Test fun `custom server models dark`() =
        captureCustomServer("custom-server-models-dark", dark = true, discovered = true)

    @Test fun `custom server models light`() =
        captureCustomServer("custom-server-models-light", dark = false, discovered = true)

    // ---------- the sheets ----------

    private fun captureSheet(name: String, dark: Boolean, tag: String, overlay: Overlay) {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(
            service = service,
            dark = dark,
            themeMode = if (dark) ThemeMode.Dark else ThemeMode.Light,
        )
        when (overlay) {
            is Overlay.Checkpoints -> viewModel.openCheckpoints(overlay.sessionId)
            else -> viewModel.openOverlay(overlay)
        }
        rule.waitForIdle()
        rule.onNodeWithTag(tag).captureRoboImage("src/test/screenshots/$name.png")
    }

    @Test fun `checkpoints sheet dark`() = captureSheet(
        "checkpoints-dark",
        dark = true,
        tag = CHECKPOINTS_SHEET_TAG,
        overlay = Overlay.Checkpoints("s-nav"),
    )

    @Test fun `checkpoints sheet light`() = captureSheet(
        "checkpoints-light",
        dark = false,
        tag = CHECKPOINTS_SHEET_TAG,
        overlay = Overlay.Checkpoints("s-nav"),
    )

    @Test fun `branches sheet dark`() = captureSheet(
        "branches-dark",
        dark = true,
        tag = BRANCHES_SHEET_TAG,
        overlay = Overlay.Branches("s-nav"),
    )

    @Test fun `branches sheet light`() = captureSheet(
        "branches-light",
        dark = false,
        tag = BRANCHES_SHEET_TAG,
        overlay = Overlay.Branches("s-nav"),
    )
}
