package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.FakeWorkflowLoom
import ai.diffforge.haider.ui.loom.AUTHORING_PROSE_TAG
import ai.diffforge.haider.ui.loom.AUTHORING_SCREEN_TAG
import ai.diffforge.haider.ui.loom.LOOMS_SCREEN_TAG
import ai.diffforge.haider.ui.loom.LoomAuthorKind
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.workflow.WORKFLOW_AST_TAG
import ai.diffforge.haider.ui.workflow.WORKFLOW_CHIP_TAG
import ai.diffforge.haider.ui.workflow.WORKFLOW_GRAPH_TAG
import ai.diffforge.haider.ui.workflow.WORKFLOW_NODE_TAG_PREFIX
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextReplacement
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The rendered pins for lane 971-UI-workflows.
 *
 * The model tests prove the arithmetic; these prove the screen is wired to it —
 * that the chip only appears for a session that has a workflow, that every
 * declared node reaches the canvas, that the drill-in opens the session the
 * daemon named, and that an unavailable daemon produces words rather than an
 * empty graph.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class WorkflowSurfacesTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private data class Harness(val service: FakeDaemonService, val viewModel: ChatViewModel)

    private fun open(
        scenario: FakeScenario = FakeScenario.WorkflowRunning,
        overlay: Overlay? = null,
    ): Harness {
        val service = ComposeHost.install(scenario)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        if (overlay != null) {
            viewModel.openOverlay(overlay)
            rule.waitForIdle()
        }
        return Harness(service, viewModel)
    }

    // ---------- the chip ----------

    @Test
    fun `the chip appears only on the session that has a workflow`() {
        val harness = open()
        rule.onNodeWithTag(WORKFLOW_CHIP_TAG).assertIsDisplayed()
        // Every other fixture session has no workflow, and the chip draws
        // nothing at all rather than a permanent "No workflow" line.
        harness.viewModel.activate("s-route")
        rule.waitForIdle()
        rule.onAllNodesWithTag(WORKFLOW_CHIP_TAG).assertCountEquals(0)
    }

    @Test
    fun `the chip opens the graph for its own session`() {
        val harness = open()
        rule.onNodeWithTag(WORKFLOW_CHIP_TAG).performClick()
        rule.waitForIdle()
        assertEquals(Overlay.WorkflowGraph("s-nav"), harness.viewModel.state.value.overlay)
    }

    // ---------- the canvas ----------

    @Test
    fun `every declared node reaches the canvas`() {
        open(overlay = Overlay.WorkflowGraph("s-nav"))
        rule.onNodeWithTag(WORKFLOW_GRAPH_TAG).assertIsDisplayed()
        FakeWorkflowLoom.AST.nodes.forEach { node ->
            rule.onNodeWithTag("$WORKFLOW_NODE_TAG_PREFIX${node.node}").assertExists()
        }
    }

    @Test
    fun `a node speaks its phase, its iteration and its join semantics`() {
        open(overlay = Overlay.WorkflowGraph("s-nav"))
        rule.onAllNodes(hasContentDescription("IMPLEMENT", substring = true))
            .onFirst()
            .assertExists()
        rule.onAllNodes(hasContentDescription("Completed", substring = true))
            .onFirst()
            .assertExists()
        // SHIP joins two inputs and is a convergence gate; both are spoken, not
        // left to a glyph a screen reader cannot see.
        rule.onAllNodes(hasContentDescription("joins 2 inputs", substring = true))
            .onFirst()
            .assertExists()
        rule.onAllNodes(hasContentDescription("convergence gate", substring = true))
            .onFirst()
            .assertExists()
    }

    @Test
    fun `the AST toggle swaps the canvas for the indented tree`() {
        open(overlay = Overlay.WorkflowGraph("s-nav"))
        rule.onNodeWithText("AST").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(WORKFLOW_AST_TAG).assertIsDisplayed()
        rule.onAllNodesWithTag(WORKFLOW_GRAPH_TAG).assertCountEquals(0)
        rule.onNodeWithText("Graph").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(WORKFLOW_GRAPH_TAG).assertIsDisplayed()
    }

    @Test
    fun `tapping a node with an attached child opens that exact session`() {
        val harness = open(overlay = Overlay.WorkflowGraph("s-nav"))
        rule.onNodeWithTag("${WORKFLOW_NODE_TAG_PREFIX}IMPLEMENT").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("Open child session").performClick()
        rule.waitForIdle()
        // The newest attempt's session, from the daemon's own parent_attempt
        // mapping — not the first attempt, and not a name match.
        assertEquals(
            Overlay.WorkflowGraph("s-child-implement-2", "g-child-impl-2"),
            harness.viewModel.state.value.overlay,
        )
        assertEquals("s-child-implement-2", harness.viewModel.state.value.activeSessionId)
    }

    @Test
    fun `a node with no attached child says so instead of offering a door`() {
        open(overlay = Overlay.WorkflowGraph("s-nav"))
        rule.onNodeWithTag("${WORKFLOW_NODE_TAG_PREFIX}SHIP").performClick()
        rule.waitForIdle()
        rule.onNodeWithText("No child session was attached to this attempt.").assertExists()
        rule.onAllNodesWithText("Open child session").assertCountEquals(0)
    }

    @Test
    fun `an unavailable daemon says so and draws no graph`() {
        open(
            scenario = FakeScenario.WorkflowUnavailable,
            overlay = Overlay.WorkflowGraph("s-nav"),
        )
        rule.onNodeWithText("workflow_graph_v1", substring = true).assertIsDisplayed()
        rule.onAllNodesWithTag(WORKFLOW_GRAPH_TAG).assertCountEquals(0)
    }

    @Test
    fun `a session the daemon says has no graph is not drawn as an empty one`() {
        open(overlay = Overlay.WorkflowGraph("s-route"))
        rule.onNodeWithText("No live workflow graph for this session.").assertIsDisplayed()
        rule.onAllNodesWithTag(WORKFLOW_GRAPH_TAG).assertCountEquals(0)
    }

    // ---------- looms ----------

    @Test
    fun `the registry lists agent types and admits an unprobed program`() {
        open(overlay = Overlay.Looms)
        rule.onNodeWithTag(LOOMS_SCREEN_TAG).assertIsDisplayed()
        rule.onNodeWithText("Implementer").assertExists()
        rule.onNodeWithText("Reviewer").assertExists()
        // "cargo" is declared by Reviewer and absent from `cli_present`, so it
        // is NOT PROBED — never rendered as missing from the device.
        rule.onNodeWithText("not probed").assertExists()
        rule.onAllNodesWithText("on this device").onFirst().assertExists()
    }

    @Test
    fun `an unavailable registry offers no list at all`() {
        open(scenario = FakeScenario.LoomUnavailable, overlay = Overlay.Looms)
        rule.onNodeWithText("loom_v1", substring = true).assertIsDisplayed()
        rule.onAllNodesWithText("Implementer").assertCountEquals(0)
    }

    @Test
    fun `archiving records the revision the list published`() {
        val harness = open(overlay = Overlay.Looms)
        rule.onAllNodesWithText("Archive").onFirst().performClick()
        rule.waitForIdle()
        // The fence travels with the call; an archive without it is not a
        // compare-and-set. Implementer is rev 4 in the fixture.
        assertTrue(harness.service.calls.any { it == "loom.archive:implementer:4" })
    }

    // ---------- authoring ----------

    @Test
    fun `the authoring cycle drafts, shows the daemon's errors and will not confirm`() {
        val harness = open(overlay = Overlay.LoomAuthoring(LoomAuthorKind.AgentType))
        rule.onNodeWithTag(AUTHORING_SCREEN_TAG).assertIsDisplayed()
        rule.onNodeWithTag(AUTHORING_PROSE_TAG).performTextReplacement("a planner")
        rule.waitForIdle()
        rule.onNodeWithText("Draft it").performClick()
        rule.waitForIdle()
        // The scripted draft has a real defect, and the daemon's own typed
        // location is shown.
        rule.onNodeWithText("2 problems to fix").assertExists()
        // The daemon's typed code and its exact one-based location, not just
        // the prose: `code` is what a caller branches on.
        rule.onAllNodesWithText("unknown_agent_type", substring = true)
            .onFirst()
            .assertExists()
        rule.onAllNodesWithText("line 7, column 38", substring = true)
            .onFirst()
            .assertExists()
        // Pressing Register does nothing: a confirm of a document carrying
        // errors would only ask the daemon for a refusal.
        rule.onNodeWithText("Register").performClick()
        rule.waitForIdle()
        assertTrue(harness.service.calls.none { it.startsWith("loom.author.confirm") })
    }

    @Test
    fun `a daemon with no model to draft with says so`() {
        open(
            scenario = FakeScenario.LoomUnavailable,
            overlay = Overlay.LoomAuthoring(LoomAuthorKind.Workflow),
        )
        rule.onNodeWithTag(AUTHORING_PROSE_TAG).performTextReplacement("just plan")
        rule.waitForIdle()
        rule.onNodeWithText("Draft it").performClick()
        rule.waitForIdle()
        // The daemon's typed reason, shown rather than a spinner that never ends.
        rule.onNodeWithText("no_model_selected", substring = true).assertIsDisplayed()
    }
}
