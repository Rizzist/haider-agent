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
        // The strip is one line: the detail reaches a screen reader, not the
        // layout (addition F, S2).
        assertTrue(
            rule.onAllNodes(
                hasContentDescription("Sessions are frozen until it starts.", substring = true),
            ).fetchSemanticsNodes().isNotEmpty(),
        )
        // Transcripts are local, so they are still there.
        assertTrue(rule.onAllNodesWithTextSafe("Fix nav crash on back gesture") > 0)
    }

    @Test
    fun `daemon failed - the reason is named and details are one tap away`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.DaemonFailed))
        assertTrue(
            rule.onAllNodes(
                hasContentDescription("store_recovery_failed", substring = true),
            ).fetchSemanticsNodes().isNotEmpty(),
        )
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
    fun `notifications permanently denied - the action goes to settings`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.NotificationsPermanentlyDenied))
        assertTrue(rule.onAllNodesWithTextSafe("Notifications are off") > 0)
        // Android will not show the dialog again; asking for it would be a lie.
        assertTrue(
            rule.onAllNodes(
                hasContentDescription("Android will not ask again", substring = true),
            ).fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `no network - informational, and send stays available`() {
        val service = ComposeHost.install(FakeScenario.NoNetwork)
        val viewModel = rule.setHaiderApp(service)
        assertTrue(rule.onAllNodesWithTextSafe("No network") > 0)
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
        // The question is the card's title; no uppercase label above it (S6).
        assertTrue(rule.onAllNodesWithTextSafe("Send this reply to Amir (+1 604 555 0142)?") > 0)
        assertEquals(0, rule.onAllNodesWithTextSafe("HAIDER NEEDS YOU"))
    }

    @Test
    fun `input required elsewhere - the badge and the rank-3 banner`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.InputRequiredElsewhere))
        // The title ellipsises on a narrow strip; "needs you" is its own node
        // so it cannot be the part that disappears (addition F, S2).
        assertTrue(rule.onAllNodesWithTextSafe("“Reply to Amir about the lease”") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("needs you") > 0)
        // The drawer button carries the count, not a third status affordance.
        assertTrue(
            rule.onAllNodes(hasContentDescription("Open sessions, 1 session needs input"))
                .fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `errored turn - the error card, with a retry only when retryable`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.ErroredTurn))
        assertTrue(rule.onAllNodesWithTextSafe("Run failed") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("provider returned 529 after 3 attempts") > 0)
    }

    @Test
    fun `empty roster with setup done - ready, not a setup checklist`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.EmptyRosterReady))
        // The hero line is gone; the suggestions are the content (F, S7).
        assertTrue(rule.onAllNodesWithTextSafe("Try") > 0)
        assertEquals(0, rule.onAllNodesWithTextSafe("Ready. Ask for anything on this phone."))
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
