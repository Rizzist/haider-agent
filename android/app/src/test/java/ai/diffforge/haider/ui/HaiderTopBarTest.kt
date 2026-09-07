package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.scaffold.HAIDER_TOP_BAR_TAG
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
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

@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class HaiderTopBarTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `the header carries three same-shaped 48 dp controls and nothing else`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)

        val minPx = with(rule.density) { ForgeSize.touch.toPx() }
        // Counted inside the bar itself: the closed drawer is composed at the
        // same coordinates and its collapse button is not on the header.
        val bar = rule.onNodeWithTag(HAIDER_TOP_BAR_TAG).fetchSemanticsNode()
        val buttons = buttonsUnder(bar)

        // Drawer, appearance, overflow. UI-SPEC 6.3.1 said two; the owner's
        // addition D put light/dark back on the bar, and that is the amendment.
        // What 6.3.1 protects still holds: one shape, one size, and no fourth
        // affordance smuggled in as a clickable label.
        val labels = buttons.map {
            it.config.getOrNull(SemanticsProperties.ContentDescription)?.first()
        }
        assertEquals(labels.toString(), 3, buttons.size)
        assertTrue(buttons.all { it.size.width >= minPx && it.size.height >= minPx })
        // The title is data, not a control: nothing else in the bar is tappable.
        assertEquals(labels.toString(), 3, clickableUnder(bar).size)
        // The 970 status pill is gone from the bar entirely.
        assertEquals(0, rule.onAllNodesWithTextSafe("connected"))
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
    fun `the overflow has no Stop at all, enabled or otherwise`() {
        // It used to carry a Stop that was merely disabled without a run_id.
        // E2 says the composer owns the only one, so the entry is gone rather
        // than greyed (verify-6 O2).
        val service = ComposeHost.install(FakeScenario.ErroredTurn)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("More options").performClick()
        assertEquals(0, rule.onAllNodesWithTextSafe("Stop turn"))
    }

    @Test
    fun `the overflow lists the session actions and settings`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("More options").performClick()
        // Matched by contentDescription: "Settings" is also a drawer footer row,
        // and the drawer is composed even while closed.
        // No Clear transcript (verify-6 O1) and no Stop (O2).
        listOf("Session details", "Rename", "Fork session", "Settings")
            .forEach { rule.onNodeWithContentDescription(it).assertIsDisplayed() }
    }

    @Test
    fun `the appearance toggle is on the bar`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("Use the light theme").assertIsDisplayed()
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

/** Every clickable node in one subtree, whatever its role. */
internal fun clickableUnder(
    node: androidx.compose.ui.semantics.SemanticsNode,
): List<androidx.compose.ui.semantics.SemanticsNode> = buildList {
    if (node.config.contains(androidx.compose.ui.semantics.SemanticsActions.OnClick)) add(node)
    node.children.forEach { addAll(clickableUnder(it)) }
}

/** Every Button-role node in one subtree. */
internal fun buttonsUnder(
    node: androidx.compose.ui.semantics.SemanticsNode,
): List<androidx.compose.ui.semantics.SemanticsNode> = buildList {
    if (node.config.getOrNull(SemanticsProperties.Role) == Role.Button) add(node)
    node.children.forEach { addAll(buttonsUnder(it)) }
}

/** Counts matching text nodes without failing when there are none. */
internal fun androidx.compose.ui.test.junit4.ComposeTestRule.onAllNodesWithTextSafe(
    text: String,
): Int = onAllNodes(androidx.compose.ui.test.hasText(text)).fetchSemanticsNodes().size


