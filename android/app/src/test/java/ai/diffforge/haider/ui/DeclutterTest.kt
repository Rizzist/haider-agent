package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.scaffold.SESSION_VIEW_HEADER_TAG
import ai.diffforge.haider.ui.chat.SHELL_VIEW_TAG
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.ShellAvailability
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.SessionViewTab
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Additions E and F: the minimalist rail row, the single Stop, the Chat|Shell
 * switch, and the clutter that came off with them.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class DeclutterTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun openDrawer() {
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
    }

    // ---------- E1 / D6: the row is a glyph and a title ----------

    @Test
    fun `a drawer row renders exactly one piece of text - its title`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        openDrawer()
        // Scoped to the row itself: the composer legitimately still shows the
        // model and the effort, which is the point — one place each (G4).
        val row = rule.onAllNodes(
            hasContentDescription("Fix nav crash on back gesture", substring = true),
        ).fetchSemanticsNodes().first()
        val texts = row.config
            .getOrNull(androidx.compose.ui.semantics.SemanticsProperties.Text)
            .orEmpty()
            .map { it.text }
        assertEquals("the row renders more than its title: $texts", 1, texts.size)
        assertEquals("Fix nav crash on back gesture", texts.single())
    }

    @Test
    fun `the row still speaks its state and its time`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        openDrawer()
        // Colour is never the only signal: what the badge used to show, the
        // merged node still says.
        listOf("Running", "Needs input", "Errored").forEach { word ->
            assertTrue(
                "no row spoke \"$word\"",
                rule.onAllNodes(hasContentDescription(word, substring = true))
                    .fetchSemanticsNodes().isNotEmpty(),
            )
        }
    }

    // ---------- E2: exactly one Stop ----------

    @Test
    fun `there is exactly one Stop control in the whole running surface`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        assertEquals(
            "the sticky chip and the composer button were two Stops",
            1,
            rule.onAllNodes(hasContentDescription("Stop this turn")).fetchSemanticsNodes().size,
        )
    }

    @Test
    fun `no run means no Stop anywhere`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.EmptyRosterReady))
        rule.waitForIdle()
        assertEquals(
            0,
            rule.onAllNodes(hasContentDescription("Stop this turn")).fetchSemanticsNodes().size,
        )
    }

    // ---------- E3: Chat | Shell ----------

    @Test
    fun `the session surface carries a Chat and Shell switch`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        rule.onNodeWithTag(SESSION_VIEW_HEADER_TAG).assertIsDisplayed()
        rule.onNodeWithContentDescription("Chat").assertIsDisplayed()
        rule.onNodeWithContentDescription("Shell").assertIsDisplayed()
    }

    @Test
    fun `Shell is honest about being disabled, and names the daemon's reason`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("Shell").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(SHELL_VIEW_TAG).assertIsDisplayed()
        rule.onNodeWithText("No shell on this device").assertIsDisplayed()
        // The reason is the daemon's own code, not a sentence we invented.
        rule.onNodeWithText("process_exec_disabled").assertIsDisplayed()
    }

    @Test
    fun `Shell becomes usable the moment the daemon says it can`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        service.setShell(ShellAvailability(available = true))
        viewModel.selectViewTab(SessionViewTab.Shell)
        rule.waitForIdle()
        rule.onNodeWithText("Shell is ready.").assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTextSafe("No shell on this device"))
    }

    @Test
    fun `switching to Shell and back keeps the transcript`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.onNodeWithContentDescription("Shell").performClick()
        rule.waitForIdle()
        assertEquals(0, rule.onAllNodesWithTextSafe("The back gesture crashes on the settings screen."))
        rule.onNodeWithContentDescription("Chat").performClick()
        rule.waitForIdle()
        assertTrue(
            rule.onAllNodesWithTextSafe("The back gesture crashes on the settings screen.") > 0,
        )
    }

    // ---------- F: the clutter that came off ----------

    @Test
    fun `S3 - one tool call is one row, with no count header`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        assertEquals(0, rule.onAllNodesWithTextSafe("1 tool call"))
        assertEquals(0, rule.onAllNodesWithTextSafe("1 RUNNING"))
        // The row itself is there, and its state is a lowercase word.
        assertTrue(rule.onAllNodesWithTextSafe("shell") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("running") > 0)
        assertEquals(0, rule.onAllNodesWithTextSafe("RUNNING"))
    }

    @Test
    fun `G5 - the composer helper sentences are gone`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        assertEquals(
            0,
            rule.onAllNodesWithTextSafe("Haider is working — tap the stop button to end this turn."),
        )
    }

    @Test
    fun `S2 - the cross-session banner is one line with a chevron`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.InputRequiredElsewhere))
        rule.waitForIdle()
        // The title ellipsises on a narrow strip; "needs you" is its own node
        // so it cannot be the part that disappears (addition F, S2).
        assertTrue(rule.onAllNodesWithTextSafe("“Reply to Amir about the lease”") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("needs you") > 0)
        // No wide Open button and no three-line body.
        assertEquals(0, rule.onAllNodesWithTextSafe("Open"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Waiting 2m 14s — Send this reply to Amir (+1 604 555 0142)?"))
    }

    @Test
    fun `T2 - permissions are rows, and the explanation is behind them`() {
        val viewModel = rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        viewModel.openOverlay(Overlay.Settings)
        rule.waitForIdle()
        rule.onNodeWithText("Accessibility").assertIsDisplayed()
        // The paragraph is not on the list.
        assertEquals(
            0,
            rule.onAllNodesWithTextSafe(
                "Read the on-screen UI tree and inject taps and swipes that you authorise.",
            ),
        )
        rule.onNodeWithText("Accessibility").performClick()
        rule.waitForIdle()
        rule.onNodeWithText(
            "Read the on-screen UI tree and inject taps and swipes that you authorise.",
        ).assertIsDisplayed()
    }

    @Test
    fun `T1 - the stats line and the version live in Settings`() {
        val viewModel = rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        viewModel.openOverlay(Overlay.Settings)
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("Haider 0.0.971") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("5 sessions · 3 active · 58 MB · up 4h12m") > 0)
    }

    @Test
    fun `G2 - the only uppercase label left is the drawer section header`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        openDrawer()
        listOf("PROVIDER", "MODEL", "EFFORT", "WHEN YOU ARE READY", "HAIDER NEEDS YOU", "DAEMON")
            .forEach { assertEquals("\"$it\" is still shouting", 0, rule.onAllNodesWithTextSafe(it)) }
        assertTrue(rule.onAllNodesWithTextSafe("NEEDS YOU") > 0)
    }
}
