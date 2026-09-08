package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.scaffold.HAIDER_TOP_BAR_TAG
import ai.diffforge.haider.ui.scaffold.HEADER_STATE_PILL_TAG
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
    fun `the header is five controls and no title`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)

        val minPx = with(rule.density) { ForgeSize.touch.toPx() }
        // Counted inside the bar itself: the closed drawer is composed at the
        // same coordinates and its collapse button is not on the header.
        val bar = rule.onNodeWithTag(HAIDER_TOP_BAR_TAG).fetchSemanticsNode()
        val buttons = buttonsUnder(bar)

        // Hamburger, Chat, Shell, refresh, theme (addition H1). The overflow
        // is gone: everything it held lives on the drawer row's own sheet, the
        // drawer footer, or the drawer's daemon row.
        val labels = buttons.map {
            it.config.getOrNull(SemanticsProperties.ContentDescription)?.first()
        }
        // Three buttons — hamburger, refresh, theme — plus the two Chat|Shell
        // segments, which are tabs and say so.
        assertEquals(labels.toString(), 3, buttons.size)
        assertTrue(buttons.all { it.size.width >= minPx && it.size.height >= minPx })
        assertEquals(labels.toString(), 5, clickableUnder(bar).size)
        assertTrue(clickableUnder(bar).all { it.size.width >= minPx && it.size.height >= minPx })
        // No title block at all: the session title is drawer data now. Scoped
        // to the bar, because the closed drawer is composed at the same
        // coordinates and legitimately shows the title in its own row.
        val barText = textUnder(bar)
        assertTrue(barText.toString(), barText.none { it.contains("Fix nav crash") })
        assertTrue(barText.toString(), barText.none { it == "connected" })
    }

    @Test
    fun `the badge only reflects other sessions`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("Open sessions").assertIsDisplayed()
    }

    @Test
    fun `the header prints no product name and no placeholder`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        rule.setHaiderApp(service)
        // Never a product name standing in for data (D2).
        assertEquals(0, rule.onAllNodesWithTextSafe("Diff Forge AI"))
        assertEquals(0, rule.onAllNodesWithTextSafe("loading…"))
    }

    @Test
    fun `the appearance toggle is on the bar`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onNodeWithContentDescription("Use the light theme").assertIsDisplayed()
    }

    @Test
    fun `the state pill says the state in a word, not only a colour`() {
        val service = ComposeHost.install(FakeScenario.TurnRunning)
        rule.setHaiderApp(service)
        rule.onNodeWithTag(HEADER_STATE_PILL_TAG).assertIsDisplayed()
        assertTrue(rule.onAllNodesWithTextSafe("Running") > 0)
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

/** Every rendered string in one subtree. */
internal fun textUnder(
    node: androidx.compose.ui.semantics.SemanticsNode,
): List<String> = buildList {
    node.config.getOrNull(SemanticsProperties.Text)?.forEach { add(it.text) }
    node.children.forEach { addAll(textUnder(it)) }
}

/** Counts matching text nodes without failing when there are none. */
internal fun androidx.compose.ui.test.junit4.ComposeTestRule.onAllNodesWithTextSafe(
    text: String,
): Int = onAllNodes(androidx.compose.ui.test.hasText(text)).fetchSemanticsNodes().size
