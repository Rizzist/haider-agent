package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
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
 * Eight goldens at 412x915, fontScale 1.0: main / drawer / first-run /
 * input-required, in both themes.
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

    private fun capture(name: String, scenario: FakeScenario, dark: Boolean, openDrawer: Boolean = false) {
        val service = ComposeHost.install(scenario)
        rule.setHaiderApp(
            service = service,
            dark = dark,
            themeMode = if (dark) ThemeMode.Dark else ThemeMode.Light,
        )
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
}
