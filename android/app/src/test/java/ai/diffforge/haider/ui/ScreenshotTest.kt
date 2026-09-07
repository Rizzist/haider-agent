package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * Sixteen goldens at 412x915, fontScale 1.0, in both themes: main, drawer,
 * first run, input required, settings, accounts, daemon stopped and the
 * permanently denied notification permission — which is a distinct state from a
 * first refusal, because Android will not ask again.
 *
 * Record with `-Proborazzi.test.record=true`. Verification in CI is a follow-up,
 * not a 971 gate: renderer drift between a laptop and a CI runner would fail the
 * release for a reason that has nothing to do with the app (UI-SPEC 6.4).
 */
@RunWith(AndroidJUnit4::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class ScreenshotTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun capture(
        name: String,
        scenario: FakeScenario,
        dark: Boolean,
        openDrawer: Boolean = false,
        overlay: Overlay? = null,
    ) {
        val service = ComposeHost.install(scenario)
        val viewModel = rule.setHaiderApp(
            service = service,
            dark = dark,
            themeMode = if (dark) ThemeMode.Dark else ThemeMode.Light,
        )
        if (overlay != null) {
            viewModel.openOverlay(overlay)
            rule.waitForIdle()
        }
        if (openDrawer) {
            rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
                .onFirst()
                .performClick()
            rule.waitForIdle()
        }
        rule.onRoot().captureRoboImage("src/test/screenshots/$name.png")
    }

    @Test
    fun `main dark`() = capture("main-dark", FakeScenario.Populated, dark = true)

    @Test
    fun `main light`() = capture("main-light", FakeScenario.Populated, dark = false)

    @Test
    fun `drawer dark`() =
        capture("drawer-dark", FakeScenario.Populated, dark = true, openDrawer = true)

    @Test
    fun `drawer light`() =
        capture("drawer-light", FakeScenario.Populated, dark = false, openDrawer = true)

    @Test
    fun `first run dark`() = capture("first-run-dark", FakeScenario.FirstRun, dark = true)

    @Test
    fun `first run light`() = capture("first-run-light", FakeScenario.FirstRun, dark = false)

    @Test
    fun `input required dark`() =
        capture("input-required-dark", FakeScenario.InputRequiredHere, dark = true)

    @Test
    fun `input required light`() =
        capture("input-required-light", FakeScenario.InputRequiredHere, dark = false)

    @Test
    fun `settings dark`() =
        capture("settings-dark", FakeScenario.Populated, dark = true, overlay = Overlay.Settings)

    @Test
    fun `settings light`() =
        capture("settings-light", FakeScenario.Populated, dark = false, overlay = Overlay.Settings)

    @Test
    fun `accounts dark`() =
        capture("accounts-dark", FakeScenario.Populated, dark = true, overlay = Overlay.Accounts)

    @Test
    fun `accounts light`() =
        capture("accounts-light", FakeScenario.Populated, dark = false, overlay = Overlay.Accounts)

    @Test
    fun `daemon stopped dark`() = capture("daemon-stopped-dark", FakeScenario.DaemonStopped, dark = true)

    @Test
    fun `daemon stopped light`() = capture("daemon-stopped-light", FakeScenario.DaemonStopped, dark = false)

    @Test
    fun `permission blocked dark`() = capture(
        "permission-blocked-dark",
        FakeScenario.NotificationsPermanentlyDenied,
        dark = true,
    )

    @Test
    fun `permission blocked light`() = capture(
        "permission-blocked-light",
        FakeScenario.NotificationsPermanentlyDenied,
        dark = false,
    )
}
