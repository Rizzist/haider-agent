package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.fleet.FleetPanelContent
import ai.diffforge.haider.ui.fleet.subagentChipTag
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSpace
import ai.diffforge.haider.ui.theme.ForgeTheme
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.test.ext.junit.runners.AndroidJUnit4
import com.github.takahirom.roborazzi.captureRoboImage
import kotlinx.coroutines.runBlocking
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The delegation surfaces, both themes.
 *
 * Record with `-Proborazzi.test.record=true`, as the rest of the suite does.
 * The panel is photographed through [FleetPanelContent] rather than the sheet:
 * a `ModalBottomSheet` renders in its own window and `onRoot()` cannot name one
 * node for it — the same reason the picker sheets are asserted behaviourally.
 */
@RunWith(AndroidJUnit4::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class FleetScreenshotTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun app(dark: Boolean): FakeDaemonService {
        val service = ComposeHost.install(FakeScenario.Fleet)
        rule.setHaiderApp(
            service = service,
            dark = dark,
            themeMode = if (dark) ThemeMode.Dark else ThemeMode.Light,
        )
        rule.waitForIdle()
        return service
    }

    private fun shoot(name: String) {
        rule.onRoot().captureRoboImage("src/test/screenshots/$name.png")
    }

    /** The session surface with its subagent chips. */
    private fun captureSession(name: String, dark: Boolean) {
        app(dark)
        shoot(name)
    }

    @Test fun `fleet session dark`() = captureSession("fleet-session-dark", true)
    @Test fun `fleet session light`() = captureSession("fleet-session-light", false)

    /** The drawer, with one family nested and one family folded. */
    private fun captureDrawer(name: String, dark: Boolean) {
        app(dark)
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        shoot(name)
    }

    @Test fun `fleet drawer dark`() = captureDrawer("fleet-drawer-dark", true)
    @Test fun `fleet drawer light`() = captureDrawer("fleet-drawer-light", false)

    /** One descendant's own transcript, read-only. */
    private fun captureChild(name: String, dark: Boolean) {
        app(dark)
        rule.onNodeWithTag(subagentChipTag("agent-auditor")).performScrollTo().performClick()
        rule.waitForIdle()
        shoot(name)
    }

    @Test fun `fleet child dark`() = captureChild("fleet-child-dark", true)
    @Test fun `fleet child light`() = captureChild("fleet-child-light", false)

    /** The cross-session panel: a bounded read, a refused read, and both. */
    private fun capturePanel(name: String, dark: Boolean) {
        val service = FakeDaemonService(FakeScenario.Fleet)
        val panel = runBlocking {
            listOf("s-fleet", "s-swarm", "s-broken").associateWith { service.fleet(it) }
        }
        val sessions = service.sessions.value
        rule.setContent {
            ForgeTheme(dark = dark) {
                Box(
                    Modifier
                        .fillMaxSize()
                        .background(Forge.colors.surfaceRaised)
                        .padding(vertical = ForgeSpace.xl),
                ) {
                    FleetPanelContent(
                        panel = panel,
                        sessions = sessions,
                        loading = false,
                        onJump = {},
                        onOpenChild = { _, _, _ -> },
                        onRefresh = {},
                    )
                }
            }
        }
        rule.waitForIdle()
        shoot(name)
    }

    @Test fun `fleet panel dark`() = capturePanel("fleet-panel-dark", true)
    @Test fun `fleet panel light`() = capturePanel("fleet-panel-light", false)

    /** The honest empty state: the daemon answered, and the answer is none. */
    private fun capturePanelEmpty(name: String, dark: Boolean) {
        val service = FakeDaemonService(FakeScenario.Populated)
        val panel = runBlocking { mapOf("s-nav" to service.fleet("s-nav")) }
        rule.setContent {
            ForgeTheme(dark = dark) {
                Box(
                    Modifier
                        .fillMaxSize()
                        .background(Forge.colors.surfaceRaised)
                        .padding(vertical = ForgeSpace.xl),
                ) {
                    FleetPanelContent(
                        panel = panel,
                        sessions = service.sessions.value,
                        loading = false,
                        onJump = {},
                        onOpenChild = { _, _, _ -> },
                        onRefresh = {},
                    )
                }
            }
        }
        rule.waitForIdle()
        shoot(name)
    }

    @Test fun `fleet panel empty dark`() = capturePanelEmpty("fleet-panel-empty-dark", true)
    @Test fun `fleet panel empty light`() = capturePanelEmpty("fleet-panel-empty-light", false)
}
