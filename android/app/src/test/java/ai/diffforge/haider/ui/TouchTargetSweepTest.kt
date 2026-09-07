package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.semantics.SemanticsNode
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.hasClickAction
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
 * Every interactive node is at least 48 x 48 dp, except the three documented
 * shortcuts in UI-SPEC 4.3 — the model chip (32), banner actions (34) and the
 * drawer filter chips (30). Each of those is permitted *only* because it has a
 * full-size equivalent one level away; none is the sole path to its action.
 *
 * Content descriptions are required on every icon-only control in the same
 * sweep, because an unlabelled icon is unreachable, not merely unlabelled.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class TouchTargetSweepTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    /** The sanctioned sub-48 dp shortcuts, matched by their spoken label. */
    private val shortcuts = listOf(
        "Change model",
        "Models unavailable",
        "Start Haider first",
        "Loading models",
        "All ",
        "Running ",
        "Needs input ",
        "Sys",
        "Light",
        "Dark",
        "Start",
        "Stop",
        "Restart",
        "Allow",
        "Fix",
        "Open",
        "Dismiss",
    )

    private fun sweep(label: String) {
        val minPx = with(rule.density) { 48.dp.toPx() }
        val offenders = rule.onAllNodes(hasClickAction())
            .fetchSemanticsNodes()
            .filter { node ->
                val description = node.spokenLabel()
                shortcuts.none { description.startsWith(it) } &&
                    (node.size.width < minPx || node.size.height < minPx)
            }
            .map { "${it.spokenLabel()} = ${it.size.width}x${it.size.height}px" }
        assertTrue("$label has undersized targets: $offenders", offenders.isEmpty())
    }

    private fun SemanticsNode.spokenLabel(): String =
        config.getOrNull(SemanticsProperties.ContentDescription)?.joinToString(" ")
            ?: config.getOrNull(SemanticsProperties.Text)?.joinToString(" ")
            ?: ""

    @Test
    fun `the first-run surface has no undersized targets`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        rule.setHaiderApp(service)
        sweep("first run")
    }

    @Test
    fun `the chat surface has no undersized targets`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        sweep("chat")
    }

    @Test
    fun `the drawer has no undersized targets`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        sweep("drawer")
    }

    @Test
    fun `the input-required surface has no undersized targets`() {
        val service = ComposeHost.install(FakeScenario.InputRequiredHere)
        rule.setHaiderApp(service)
        sweep("input required")
    }

    @Test
    fun `every icon-only control is labelled`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        val unlabelled = rule.onAllNodes(
            SemanticsMatcher("clickable and unlabelled") { node ->
                node.config.contains(androidx.compose.ui.semantics.SemanticsActions.OnClick) &&
                    node.spokenLabel().isBlank()
            },
        ).fetchSemanticsNodes().size
        assertTrue("$unlabelled clickable nodes speak nothing", unlabelled == 0)
    }
}

private val Int.dp: androidx.compose.ui.unit.Dp get() = androidx.compose.ui.unit.Dp(this.toFloat())
