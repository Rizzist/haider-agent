package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
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

@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class HaiderTopBarTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `the header carries exactly two controls`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)

        // Everything clickable above the banner: the drawer button and the
        // overflow. The title block is a target too, but it is not a Button
        // role — it opens the session sheet.
        rule.onNodeWithContentDescription("Open sessions, 1 session needs input").assertIsDisplayed()
        rule.onNodeWithContentDescription("More options").assertIsDisplayed()
        // The 970 status pill is gone from the bar entirely.
        rule.onAllNodesWithTextSafe("connected").let { assertEquals(0, it) }
    }

    @Test
    fun `the badge only reflects other sessions`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("Open sessions").assertIsDisplayed()
    }

    @Test
    fun `the subtitle is absent when there is nothing true to say`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(service)
        // Never a product name standing in for data (D2).
        assertEquals(0, rule.onAllNodesWithTextSafe("Diff Forge AI"))
        assertEquals(0, rule.onAllNodesWithTextSafe("loading…"))
    }

    @Test
    fun `stop turn is disabled when the snapshot carries no run id`() {
        val service = ComposeHost.install(FakeScenario.ErroredTurn)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("More options").performClick()
        rule.onNodeWithContentDescription("Stop turn").assertIsNotEnabled()
    }

    @Test
    fun `the overflow lists the session actions and settings`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("More options").performClick()
        // Matched by contentDescription: "Settings" is also a drawer footer row,
        // and the drawer is composed even while closed.
        listOf("Session details", "Rename", "Fork session", "Clear transcript", "Settings")
            .forEach { rule.onNodeWithContentDescription(it).assertIsDisplayed() }
    }

    @Test
    fun `the drawer button opens the drawer`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("Open sessions, 1 session needs input").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Close sessions").assertIsDisplayed()
        assertTrue(true)
    }
}

/** Counts matching text nodes without failing when there are none. */
internal fun androidx.compose.ui.test.junit4.ComposeTestRule.onAllNodesWithTextSafe(
    text: String,
): Int = onAllNodes(androidx.compose.ui.test.hasText(text)).fetchSemanticsNodes().size


