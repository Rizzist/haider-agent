package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.components.HAIDER_LOGO_DARK_TAG
import ai.diffforge.haider.ui.components.HAIDER_LOGO_LIGHT_TAG
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.scaffold.HaiderApp
import ai.diffforge.haider.ui.scaffold.InMemoryBannerDismissals
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.theme.LogoPreferences
import ai.diffforge.haider.ui.theme.LogoStyle
import ai.diffforge.haider.ui.theme.ThemeMode
import ai.diffforge.haider.ui.theme.ThemePreferences
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.assertIsNotSelected
import androidx.compose.ui.test.assertIsSelected
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * The wordmark's colourway contract (owner request 2026-09-13): Auto follows
 * the app theme, an explicit choice beats it, and a Settings tap re-renders
 * the mark in the same frame.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class LogoUiTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun renderedVariants(): Pair<Int, Int> = Pair(
        rule.onAllNodesWithTag(HAIDER_LOGO_DARK_TAG).fetchSemanticsNodes().size,
        rule.onAllNodesWithTag(HAIDER_LOGO_LIGHT_TAG).fetchSemanticsNodes().size,
    )

    private fun assertOnly(dark: Boolean) {
        val (darkCount, lightCount) = renderedVariants()
        if (dark) {
            assertTrue("no dark-variant logo rendered", darkCount > 0)
            assertEquals("a light-variant logo leaked in", 0, lightCount)
        } else {
            assertTrue("no light-variant logo rendered", lightCount > 0)
            assertEquals("a dark-variant logo leaked in", 0, darkCount)
        }
    }

    @Test
    fun `Auto renders the dark variant under the dark theme`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(service, dark = true, themeMode = ThemeMode.Dark)
        rule.waitForIdle()
        assertOnly(dark = true)
    }

    @Test
    fun `Auto renders the light variant under the light theme`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(service, dark = false, themeMode = ThemeMode.Light)
        rule.waitForIdle()
        assertOnly(dark = false)
    }

    @Test
    fun `an explicit Light choice wins on the dark theme`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(
            service,
            dark = true,
            themeMode = ThemeMode.Dark,
            logoStyle = LogoStyle.Light,
        )
        rule.waitForIdle()
        assertOnly(dark = false)
    }

    @Test
    fun `an explicit Dark choice wins on the light theme`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(
            service,
            dark = false,
            themeMode = ThemeMode.Light,
            logoStyle = LogoStyle.Dark,
        )
        rule.waitForIdle()
        assertOnly(dark = true)
    }

    /** The dark-themed app with a live, mutable style choice, opened on Settings. */
    private fun openSettingsWithLiveStyle(service: FakeDaemonService) {
        val viewModel = ChatViewModel(service, searchDebounceMs = 0)
        val accounts = FakeAccountsRepository()
        rule.setContent {
            var style by remember { mutableStateOf(LogoStyle.Auto) }
            HaiderApp(
                viewModel = viewModel,
                service = service,
                accounts = accounts,
                oauth = remember { OAuthAttemptController(accounts, kotlinx.coroutines.MainScope()) },
                appVersion = "0.0.970",
                themeMode = ThemeMode.Dark,
                onThemeMode = {},
                logoStyle = style,
                onLogoStyle = { style = it },
                dismissals = InMemoryBannerDismissals(),
                nowMsProvider = { FakeDaemonService.FIXED_NOW },
                elapsedRealtimeProvider = { FakeDaemonService.FIXED_UPTIME },
                darkOverride = true,
            )
        }
        viewModel.openOverlay(Overlay.Settings)
        rule.waitForIdle()
    }

    @Test
    fun `the Settings chips re-render the mark live`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        openSettingsWithLiveStyle(service)
        // Auto on the dark theme: the preview is the dark variant.
        assertOnly(dark = true)
        rule.onNodeWithText("Light").performClick()
        rule.waitForIdle()
        assertOnly(dark = false)
        rule.onNodeWithText("Auto").performClick()
        rule.waitForIdle()
        assertOnly(dark = true)
    }

    @Test
    fun `the Settings chips expose their selection to accessibility`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        openSettingsWithLiveStyle(service)
        // The accent wash is not just paint: the chip in force says selected
        // on its semantics node, and the other two say not selected.
        rule.onNodeWithText("Auto").assertIsSelected()
        rule.onNodeWithText("Light").assertIsNotSelected()
        rule.onNodeWithText("Dark").assertIsNotSelected()
        rule.onNodeWithText("Light").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Light").assertIsSelected()
        rule.onNodeWithText("Auto").assertIsNotSelected()
        rule.onNodeWithText("Dark").assertIsNotSelected()
    }

    @Test
    fun `the choice survives recreation through the shared preferences store`() {
        val context = RuntimeEnvironment.getApplication()
        LogoPreferences.save(ThemePreferences.store(context), LogoStyle.Light)
        // A recreated activity builds a fresh store over the same file.
        assertEquals(
            LogoStyle.Light,
            LogoPreferences.load(ThemePreferences.store(context)),
        )
    }
}
