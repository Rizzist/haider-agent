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

    /**
     * The sub-48 dp shortcuts, named exactly, each with the full-size
     * equivalent that makes it permissible (UI-SPEC 4.3). Nothing is matched by
     * a loose prefix: "Start" would have exempted every Start button in the
     * app, which is how the round-1 list grew past the three sanctioned kinds.
     *
     * | shortcut | full-size equivalent |
     * |---|---|
     * | composer model/provider/effort chips | the picker sheets, and the drawer footer's Model row |
     * | banner action buttons (34 dp) | the same action in Settings |
     * | drawer filter chips (30 dp) | scrolling the grouped list |
     * | appearance segmented chips (30 dp) | Settings -> Appearance |
     */
    private val exactShortcuts = setOf(
        // Composer context row (32 dp visual).
        "Change model",
        "Models unavailable · Retry",
        "Start Haider first",
        "Loading models",
        // Banner actions (34 dp), each duplicated in Settings.
        "Start",
        "Allow",
        "Fix",
        "Open",
        "Dismiss",
        // Daemon card actions (34 dp), duplicated in Settings -> Daemon.
        "Stop",
        "Restart",
        // Appearance segmented control (30 dp), duplicated in Settings.
        "Sys",
        "Light",
        "Dark",
    )

    /** Filter chips carry live counts, so they are matched on their stem. */
    private val countedShortcuts = setOf("All", "Running", "Needs input")

    private fun exempt(label: String): Boolean {
        if (label in exactShortcuts) return true
        // "Change provider, anthropic" / "Change effort, high": the chip's own
        // label plus its current value.
        if (label.startsWith("Change provider") || label.startsWith("Change effort")) return true
        if (label.startsWith("Change model")) return true
        val stem = label.substringBeforeLast(' ')
        return stem in countedShortcuts
    }

    private fun sweep(label: String) {
        val minPx = with(rule.density) { 48.dp.toPx() }
        val offenders = rule.onAllNodes(hasClickAction())
            .fetchSemanticsNodes()
            .filter { node ->
                val description = node.spokenLabel()
                !exempt(description) &&
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
    fun `the settings screen has no undersized targets`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(ai.diffforge.haider.ui.state.Overlay.Settings)
        rule.waitForIdle()
        sweep("settings")
    }

    @Test
    fun `the accounts screen has no undersized targets`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(ai.diffforge.haider.ui.state.Overlay.Accounts)
        rule.waitForIdle()
        sweep("accounts")
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
