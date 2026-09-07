package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.PermissionSnapshot
import ai.diffforge.haider.ui.state.PermissionStanding
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithContentDescription
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
 * The round-6 device findings, pinned.
 *
 * Every one of these passed the JVM suite before Astra ran the app: the pins
 * that existed asserted the thing they had been written against — one Stop in
 * the composer, a status string in Settings — and not the thing the person
 * saw, which was a second Stop one tap away in the overflow and a Settings row
 * that contradicted the system dialog.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class Verify6RepairTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun openOverflow() {
        rule.onAllNodes(hasContentDescription("More", substring = true)).onFirst().performClick()
        rule.waitForIdle()
    }

    // ---------- O1: no deceptive Clear ----------

    @Test
    fun `the overflow offers no Clear transcript, because nothing can clear it`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        openOverflow()
        assertEquals(0, rule.onAllNodesWithTextSafe("Clear transcript"))
    }

    // ---------- O2: exactly one Stop, everywhere ----------

    @Test
    fun `no node outside the composer carries the stop-turn action`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        // Overflow open — this is the surface the round-6 pin never opened.
        openOverflow()
        assertEquals(0, rule.onAllNodesWithTextSafe("Stop turn"))
        assertEquals(
            "the composer Stop must survive; anything else must not",
            1,
            rule.onAllNodes(hasContentDescription("Stop this turn")).fetchSemanticsNodes().size,
        )
    }

    @Test
    fun `a running row offers no Stop custom action to TalkBack`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst().performClick()
        rule.waitForIdle()
        val offered: List<String> = rule.onAllNodes(hasContentDescription("", substring = true))
            .fetchSemanticsNodes()
            .flatMap { node ->
                node.config.getOrNull(SemanticsActions.CustomActions).orEmpty().map { it.label }
            }
        assertTrue("a row still offered $offered", offered.none { it.contains("Stop") })
    }

    // ---------- O3: permission rows tell the truth ----------

    @Test
    fun `a granted SMS permission reads as granted`() {
        val viewModel = rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        viewModel.openOverlay(Overlay.Settings)
        viewModel.onPermissionsObserved(
            PermissionSnapshot(
                accessibility = PermissionStanding.NotGranted,
                sms = PermissionStanding.Granted,
            ),
        )
        rule.waitForIdle()
        rule.onNodeWithText("SMS").assertIsDisplayed()
        // Round 6 printed "Not granted" here no matter what Android said.
        assertTrue(rule.onAllNodesWithTextSafe("Granted") > 0)
    }

    @Test
    fun `an unobserved permission says so rather than guessing`() {
        val viewModel = rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        viewModel.openOverlay(Overlay.Settings)
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("Not checked yet") > 0)
    }

    // ---------- O5: every setup step expands ----------

    @Test
    fun `a pending setup step has a status and opens when tapped`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.FirstRun))
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("Not yet") > 0)
        // Tapping a pending step used to do nothing at all.
        rule.onNodeWithText("Let Haider notify you").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Allow notifications").assertIsDisplayed()
    }

    // ---------- O6: no raw id in the header ----------

    @Test
    fun `an untitled session is New session in the header, never its id`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.EmptyRosterReady))
        rule.waitForIdle()
        assertEquals(0, rule.onAllNodesWithTextSafe("Session s-new-"))
        assertTrue(rule.onAllNodesWithTextSafe("New session") > 0)
    }

    // ---------- O7: no transitional helper ----------

    @Test
    fun `a starting daemon draws no helper sentence under the composer`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.DaemonStarting))
        rule.waitForIdle()
        // Once, in the banner that owns the message — not again as a helper
        // line under the composer. Round 6 rendered it twice.
        assertEquals(1, rule.onAllNodesWithTextSafe("Starting Haider…"))
    }

    // ---------- O8: the finished call carries its duration ----------

    @Test
    fun `a completed tool call shows the duration the daemon reported`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.InputRequiredHere))
        rule.waitForIdle()
        rule.onNodeWithText("41s").assertIsDisplayed()
    }
}
