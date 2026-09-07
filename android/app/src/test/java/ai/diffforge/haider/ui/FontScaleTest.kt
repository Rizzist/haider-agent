package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Layouts have to survive 200 % font scale: rows grow with `heightIn(min = …)`
 * rather than being pinned with `height(…)`, and the header title truncates to
 * one line instead of clipping (UI-SPEC 4.4).
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi", fontScale = 2.0f)
class FontScaleTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `first run composes at 200 percent without a crash`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        rule.setHaiderApp(service)
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("Haider runs on this phone") > 0)
    }

    @Test
    fun `the chat surface composes at 200 percent and the title still reads`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.waitForIdle()
        val title = rule.onAllNodesWithTextSafe("Fix nav crash on back gesture")
        assertTrue("the header title was clipped away entirely", title > 0)
    }

    @Test
    fun `the drawer composes at 200 percent and its rows grow rather than clip`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        // The session rows themselves, not the header's "Open sessions, n running".
        val rows = rule.onAllNodes(hasContentDescription("Fix nav crash", substring = true))
            .fetchSemanticsNodes()
        assertTrue(rows.isNotEmpty())
        val minPx = with(rule.density) { 64.dp.toPx() }
        assertTrue(
            "a session row shrank below its two-line minimum at 200 % font scale",
            rows.all { it.size.height >= minPx },
        )
    }

    @Test
    fun `the input-required card composes at 200 percent`() {
        val service = ComposeHost.install(FakeScenario.InputRequiredHere)
        rule.setHaiderApp(service)
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("HAIDER NEEDS YOU") > 0)
    }
}

private val Int.dp: androidx.compose.ui.unit.Dp get() = androidx.compose.ui.unit.Dp(this.toFloat())
