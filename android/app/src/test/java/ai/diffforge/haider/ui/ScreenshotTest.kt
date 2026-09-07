package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithText
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
 * Every row of the UI-SPEC 3.8 state matrix, in both themes, plus the surfaces
 * that are not matrix rows but are where people spend time: the drawer,
 * Settings, Accounts and the pickers.
 *
 * 6.3.9's wording now stands as written — every state, both themes — so the
 * matrix is a table here rather than a hand-picked four.
 *
 * Record with `-Proborazzi.test.record=true`. Verification in CI is a follow-up,
 * not a 971 gate: renderer drift would fail a release for a reason that has
 * nothing to do with the app (UI-SPEC 6.4).
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
        then: (() -> Unit)? = null,
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
        then?.invoke()
        rule.onRoot().captureRoboImage("src/test/screenshots/$name.png")
    }


    // ---------- the 3.8 state matrix, dark ----------

    @Test fun `first run dark`() = capture("first-run-dark", FakeScenario.FirstRun, true)
    @Test fun `first run light`() = capture("first-run-light", FakeScenario.FirstRun, false)

    @Test fun `daemon starting dark`() =
        capture("daemon-starting-dark", FakeScenario.DaemonStarting, true)
    @Test fun `daemon starting light`() =
        capture("daemon-starting-light", FakeScenario.DaemonStarting, false)

    @Test fun `daemon stopped dark`() =
        capture("daemon-stopped-dark", FakeScenario.DaemonStopped, true)
    @Test fun `daemon stopped light`() =
        capture("daemon-stopped-light", FakeScenario.DaemonStopped, false)

    @Test fun `daemon failed dark`() =
        capture("daemon-failed-dark", FakeScenario.DaemonFailed, true)
    @Test fun `daemon failed light`() =
        capture("daemon-failed-light", FakeScenario.DaemonFailed, false)

    @Test fun `permission denied dark`() =
        capture("permission-denied-dark", FakeScenario.NotificationsDenied, true)
    @Test fun `permission denied light`() =
        capture("permission-denied-light", FakeScenario.NotificationsDenied, false)

    @Test fun `permission blocked dark`() =
        capture("permission-blocked-dark", FakeScenario.NotificationsPermanentlyDenied, true)
    @Test fun `permission blocked light`() =
        capture("permission-blocked-light", FakeScenario.NotificationsPermanentlyDenied, false)

    @Test fun `no network dark`() = capture("no-network-dark", FakeScenario.NoNetwork, true)
    @Test fun `no network light`() = capture("no-network-light", FakeScenario.NoNetwork, false)

    @Test fun `turn running dark`() = capture("turn-running-dark", FakeScenario.TurnRunning, true)
    @Test fun `turn running light`() = capture("turn-running-light", FakeScenario.TurnRunning, false)

    @Test fun `input required dark`() =
        capture("input-required-dark", FakeScenario.InputRequiredHere, true)
    @Test fun `input required light`() =
        capture("input-required-light", FakeScenario.InputRequiredHere, false)

    @Test fun `input required elsewhere dark`() =
        capture("input-elsewhere-dark", FakeScenario.InputRequiredElsewhere, true)
    @Test fun `input required elsewhere light`() =
        capture("input-elsewhere-light", FakeScenario.InputRequiredElsewhere, false)

    @Test fun `errored turn dark`() = capture("errored-dark", FakeScenario.ErroredTurn, true)
    @Test fun `errored turn light`() = capture("errored-light", FakeScenario.ErroredTurn, false)

    @Test fun `empty roster ready dark`() =
        capture("empty-ready-dark", FakeScenario.EmptyRosterReady, true)
    @Test fun `empty roster ready light`() =
        capture("empty-ready-light", FakeScenario.EmptyRosterReady, false)

    // ---------- surfaces that are not matrix rows ----------

    @Test fun `main dark`() = capture("main-dark", FakeScenario.Populated, true)
    @Test fun `main light`() = capture("main-light", FakeScenario.Populated, false)

    @Test fun `drawer dark`() =
        capture("drawer-dark", FakeScenario.Populated, true, openDrawer = true)
    @Test fun `drawer light`() =
        capture("drawer-light", FakeScenario.Populated, false, openDrawer = true)

    @Test fun `settings dark`() =
        capture("settings-dark", FakeScenario.Populated, true, overlay = Overlay.Settings)
    @Test fun `settings light`() =
        capture("settings-light", FakeScenario.Populated, false, overlay = Overlay.Settings)

    @Test fun `accounts dark`() =
        capture("accounts-dark", FakeScenario.Populated, true, overlay = Overlay.Accounts)
    @Test fun `accounts light`() =
        capture("accounts-light", FakeScenario.Populated, false, overlay = Overlay.Accounts)

    // A ModalBottomSheet renders in its own window, so `onRoot()` cannot name
    // one node for it. The picker sheets are asserted behaviourally in
    // ComposerPickersTest instead of photographed here.
}

/**
 * The narrow viewport the verifier used on the device. The provider row
 * compressed DeepSeek's label to a four-pixel sliver here; it wraps now.
 */
@RunWith(AndroidJUnit4::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w360dp-h800dp-xhdpi")
class NarrowScreenshotTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun captureForm(name: String, dark: Boolean) {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(
            service = service,
            dark = dark,
            themeMode = if (dark) ThemeMode.Dark else ThemeMode.Light,
        )
        viewModel.openOverlay(Overlay.Accounts)
        rule.waitForIdle()
        rule.onNodeWithText("Add API key").performClick()
        rule.waitForIdle()
        rule.onRoot().captureRoboImage("src/test/screenshots/$name.png")
    }

    @Test fun `narrow api key form dark`() = captureForm("narrow-api-key-dark", dark = true)
    @Test fun `narrow api key form light`() = captureForm("narrow-api-key-light", dark = false)
}
