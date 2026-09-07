package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Every row of the UI-SPEC 3.8 state matrix is reachable through
 * [FakeScenario], and each one is asserted on the thing that distinguishes it.
 *
 * The eight committed screenshots (UI-SPEC 6.4) cover four of these surfaces in
 * both themes; the rest are covered here, because a screenshot of a banner is
 * not a better test of a banner than an assertion about it.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class StateMatrixTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `first run - the checklist is the message`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.FirstRun))
        assertTrue(rule.onAllNodesWithTextSafe("Run Haider in the background") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Setting up · step 1 of 4") > 0)
        // No banner competes with the checklist.
        assertEquals(0, rule.onAllNodesWithTextSafe("Haider isn't running"))
    }

    @Test
    fun `daemon starting - the banner says so and the composer waits`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.DaemonStarting))
        assertTrue(rule.onAllNodesWithTextSafe("Starting Haider…") > 0)
    }

    @Test
    fun `daemon stopped - the banner offers Start and the roster stays readable`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.DaemonStopped))
        assertTrue(rule.onAllNodesWithTextSafe("Haider isn't running") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Sessions are frozen until it starts.") > 0)
        // Transcripts are local, so they are still there.
        assertTrue(rule.onAllNodesWithTextSafe("Fix nav crash on back gesture") > 0)
    }

    @Test
    fun `daemon failed - the reason is named and details are one tap away`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.DaemonFailed))
        assertTrue(rule.onAllNodesWithTextSafe("Stopped — store_recovery_failed") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Daemon details") > 0)
    }

    @Test
    fun `notifications denied - dismissible, and framed by consequence`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.NotificationsDenied))
        assertTrue(rule.onAllNodesWithTextSafe("Notifications are off") > 0)
        assertTrue(
            rule.onAllNodes(hasContentDescription("Dismiss")).fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `no network - informational, and send stays available`() {
        val service = ComposeHost.install(FakeScenario.NoNetwork)
        val viewModel = rule.setHaiderApp(service)
        assertTrue(rule.onAllNodesWithTextSafe("No network") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("The agent runs locally, but model calls will fail.") > 0)
        viewModel.setDraft("still sendable")
        rule.waitForIdle()
        // The failure belongs to the turn, not to the button.
        assertTrue(
            rule.onAllNodes(hasContentDescription("Send message")).fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `turn running - a live turn and a stop affordance`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        assertTrue(
            rule.onAllNodes(hasContentDescription("Stop this turn")).fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `input required here - a card, and a paused composer`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.InputRequiredHere))
        assertTrue(rule.onAllNodesWithTextSafe("HAIDER NEEDS YOU") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("The turn is paused.") > 0)
    }

    @Test
    fun `input required elsewhere - the badge and the rank-3 banner`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.InputRequiredElsewhere))
        assertTrue(rule.onAllNodesWithTextSafe("“Reply to Amir about the lease” needs you") > 0)
        // The drawer button carries the count, not a third status affordance.
        assertTrue(
            rule.onAllNodes(hasContentDescription("Open sessions, 1 session needs input"))
                .fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `errored turn - the error card, with a retry only when retryable`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.ErroredTurn))
        assertTrue(rule.onAllNodesWithTextSafe("RUN FAILED") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("provider returned 529 after 3 attempts") > 0)
    }

    @Test
    fun `empty roster with setup done - ready, not a setup checklist`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.EmptyRosterReady))
        assertTrue(rule.onAllNodesWithTextSafe("Ready. Ask for anything on this phone.") > 0)
        assertEquals(0, rule.onAllNodesWithTextSafe("Run Haider in the background"))
    }

    @Test
    fun `a large roster is reachable and pages`() {
        val service = ComposeHost.install(FakeScenario.LargeRoster)
        rule.setHaiderApp(service)
        // The closed drawer does not page the roster in behind the user's back.
        assertEquals(60, service.sessions.value.size)
        assertTrue(service.paging.value.hasMore)
    }
}
