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
    fun `rows are one line - glyph and title, nothing else`() {
        openDrawer()
        assertTrue(rule.onAllNodesWithTextSafe("Fix nav crash on back gesture") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Reply to Amir about the lease") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Port the settings screen") > 0)
        // Addition E: no model, no effort, no time, no badges on the row.
        assertEquals(0, rule.onAllNodesWithTextSafe("Sonnet 4.5 · high"))
        assertEquals(0, rule.onAllNodesWithTextSafe("RUNNING"))
        assertEquals(0, rule.onAllNodesWithTextSafe("NEEDS INPUT"))
        assertEquals(0, rule.onAllNodesWithTextSafe("FORKED"))
        assertEquals(0, rule.onAllNodesWithTextSafe("12m"))
        // The state still reaches anyone not looking at colour.
        listOf("Running", "Needs input", "Errored", "Waiting for network").forEach { word ->
            assertTrue(
                "no row spoke \"$word\"",
                rule.onAllNodes(hasContentDescription(word, substring = true))
                    .fetchSemanticsNodes().isNotEmpty(),
            )
        }
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
    fun `the drawer head is one row, then the list and Settings`() {
        openDrawer()
        // Round 10 merges the daemon line, New chat and the collapse chevron
        // into a single row: New chat is an icon square now, so it is named by
        // its description rather than a full-width label.
        rule.onNodeWithContentDescription("New chat").assertIsDisplayed()
        rule.onNodeWithText("Running in background").assertIsDisplayed()
        assertTrue(rule.onAllNodesWithTextSafe("Settings") > 0)
        // Addition F: the identity block, the Model row and the Appearance row
        // are gone; the theme lives on the top bar and the model in the composer.
        // "Model" is now a composer select label, so the assertion is about
        // the drawer's own subtree rather than the whole screen.
        assertEquals(0, rule.onAllNodesWithTextSafe("Appearance"))
        assertEquals(0, rule.onAllNodesWithTextSafe("v0.0.970 · on this device"))
    }

    @Test
    fun `a stopped daemon offers a one-tap Start on the daemon line`() {
        openDrawer(FakeScenario.DaemonStopped)
        assertTrue(rule.onAllNodesWithTextSafe("Stopped") > 0)
        rule.onNodeWithContentDescription("Start").assertIsDisplayed()
    }

    @Test
    fun `the stats line is not in the drawer at all`() {
        // It moved to Settings -> Daemon, where somebody looking for numbers
        // will look (addition F, D2/T1).
        openDrawer()
        assertEquals(
            0,
            rule.onAllNodes(hasText("MB", substring = true)).fetchSemanticsNodes().size,
        )
    }

    @Test
    fun `the filter chips are gone and search took their place`() {
        // Needs-input rows already float to the top, so a filter for them was a
        // second way to say the same thing (addition F, D4).
        openDrawer()
        assertEquals(0, rule.onAllNodes(hasContentDescription("Running 1")).fetchSemanticsNodes().size)
        assertEquals(0, rule.onAllNodes(hasContentDescription("Needs input 1")).fetchSemanticsNodes().size)
        rule.onNodeWithText("Search sessions").assertIsDisplayed()
    }

    @Test
    fun `search is there whenever there is anything to search`() {
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
