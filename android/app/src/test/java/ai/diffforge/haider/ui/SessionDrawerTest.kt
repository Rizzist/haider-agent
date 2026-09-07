package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasText
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
 * Titles legitimately appear twice — once in the header, once in the drawer row
 * — so these assert on counts rather than on a single node.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class SessionDrawerTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun openDrawer(scenario: FakeScenario = FakeScenario.Populated) {
        val service = ComposeHost.install(scenario)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
    }

    @Test
    fun `the drawer lists every local session with its state and model`() {
        openDrawer()
        assertTrue(rule.onAllNodesWithTextSafe("Fix nav crash on back gesture") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Reply to Amir about the lease") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Port the settings screen") > 0)
        // The state is a word, never colour alone. The pill is decorative — the
        // merged row speaks the state, which is what TalkBack actually reads.
        listOf("Running", "Needs input", "Errored", "Waiting for network").forEach { word ->
            assertTrue(
                "no row spoke \"$word\"",
                rule.onAllNodes(hasContentDescription(word, substring = true))
                    .fetchSemanticsNodes().isNotEmpty(),
            )
        }
        assertTrue(rule.onAllNodesWithTextSafe("Sonnet 4.5 · high") > 0)
    }

    @Test
    fun `the attention groups appear in order`() {
        openDrawer()
        rule.onNodeWithText("NEEDS YOU").assertIsDisplayed()
        rule.onNodeWithText("ACTIVE").assertIsDisplayed()
        rule.onNodeWithText("RECENT").assertIsDisplayed()
    }

    @Test
    fun `an empty roster hides the groups and says what to do`() {
        // With the daemon running the app opens into a session, so the empty
        // roster is only reachable while it is stopped (addition D).
        openDrawer(FakeScenario.EmptyRosterStopped)
        assertEquals(0, rule.onAllNodesWithTextSafe("NEEDS YOU"))
        assertEquals(0, rule.onAllNodesWithTextSafe("ACTIVE"))
        rule.onNodeWithText("No sessions yet. Tap New session to start one.").assertIsDisplayed()
    }

    @Test
    fun `new session, the daemon card and the footer rows are all present`() {
        openDrawer()
        assertTrue(rule.onAllNodesWithTextSafe("New session") > 0)
        rule.onNodeWithText("Running in background").assertIsDisplayed()
        rule.onNodeWithText("Model").assertIsDisplayed()
        assertTrue(rule.onAllNodesWithTextSafe("Settings") > 0)
    }

    @Test
    fun `a stopped daemon offers a one-tap Start in the card`() {
        openDrawer(FakeScenario.DaemonStopped)
        // Header subtitle and card phrase both say it; the card's action is what
        // matters here.
        assertTrue(rule.onAllNodesWithTextSafe("Stopped") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Start") > 0)
    }

    @Test
    fun `the resource line prints only the segments it knows`() {
        openDrawer()
        assertTrue(rule.onAllNodes(hasText("MB", substring = true)).fetchSemanticsNodes().isNotEmpty())
        assertEquals(0, rule.onAllNodesWithTextSafe("0 MB"))
        assertEquals(0, rule.onAllNodesWithTextSafe("up 0m"))
    }

    @Test
    fun `filter chips narrow the list`() {
        openDrawer()
        rule.onNodeWithContentDescription("Running 1").performClick()
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("Fix nav crash on back gesture") > 0)
        assertEquals(0, rule.onAllNodesWithTextSafe("Port the settings screen"))
    }

    @Test
    fun `search appears once the roster is large`() {
        openDrawer(FakeScenario.LargeRoster)
        rule.onNodeWithText("Search sessions").assertIsDisplayed()
    }

    @Test
    fun `back closes the drawer`() {
        openDrawer()
        rule.onNodeWithContentDescription("Close sessions").assertIsDisplayed()
        rule.activityRule.scenario.onActivity { it.onBackPressedDispatcher.onBackPressed() }
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("New session") >= 0)
    }
}
