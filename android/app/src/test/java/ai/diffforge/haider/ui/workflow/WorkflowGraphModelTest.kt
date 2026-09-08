package ai.diffforge.haider.ui.workflow

import ai.diffforge.haider.ui.daemon.FakeWorkflowLoom
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The pins for the workflow view model.
 *
 * Each one replays a way the surface could have looked right and been wrong:
 * a fabricated phase, a graph invented out of `state: null`, a cursor rounded
 * through a Long, a node state folded out of a watch event, a back edge counted
 * as progress.
 */
class WorkflowGraphModelTest {

    // ---------- phases ----------

    @Test
    fun `the four wire phases map, and nothing else is guessed`() {
        assertEquals(WorkflowNodePhase.Waiting, WorkflowGraphModel.nodePhase("waiting"))
        assertEquals(WorkflowNodePhase.Activated, WorkflowGraphModel.nodePhase("activated"))
        assertEquals(WorkflowNodePhase.Completed, WorkflowGraphModel.nodePhase("completed"))
        assertEquals(WorkflowNodePhase.Rejected, WorkflowGraphModel.nodePhase("rejected"))
        // A phase this build does not know is Unknown, and is kept for display.
        assertEquals(WorkflowNodePhase.Unknown, WorkflowGraphModel.nodePhase("quarantined"))
        // Absence is NOT "waiting": a record that published no phase said
        // nothing, and waiting is a claim about the run.
        assertEquals(WorkflowNodePhase.Unpublished, WorkflowGraphModel.nodePhase(null))
        assertEquals(WorkflowNodePhase.Unpublished, WorkflowGraphModel.nodePhase(""))
        assertNotEquals(WorkflowNodePhase.Waiting, WorkflowGraphModel.nodePhase(null))
    }

    @Test
    fun `an unknown phase keeps the daemon's own word`() {
        val state = WorkflowRpcAdapter.parseNodeState(
            org.json.JSONObject("""{"node":"SHIP","phase":"quarantined","iteration":1}"""),
        )
        assertEquals(WorkflowNodePhase.Unknown, state.phase)
        assertEquals("quarantined", state.phaseRaw)
    }

    // ---------- cursors ----------

    @Test
    fun `a u64 cursor survives as a decimal string`() {
        val huge = "18446744073709551615"
        assertEquals(huge, WorkflowCursor.orNull(huge))
        // The defect this exists for: a signed Long cannot hold this cursor at
        // all, and `org.json` writes it as Long.MAX_VALUE. Either way the watch
        // would resume from a position the daemon never published.
        assertNull(huge.toLongOrNull())
        assertTrue(WorkflowCursor.advances("9007199254740992", "9007199254740993"))
        assertFalse(WorkflowCursor.advances("9007199254740993", "9007199254740992"))
    }

    @Test
    fun `zero is a real cursor and a non-decimal one is nothing`() {
        assertEquals("0", WorkflowCursor.orNull("0"))
        assertNull(WorkflowCursor.orNull("0x10"))
        assertNull(WorkflowCursor.orNull("-1"))
        assertNull(WorkflowCursor.orNull(null))
        // No held position: any real cursor is an advance.
        assertTrue(WorkflowCursor.advances(null, "0"))
    }

    // ---------- the watch signal ----------

    @Test
    fun `events, an advance, or a gap each mean re-read the state`() {
        val withEvents = WorkflowGraphModel.watchSignal(
            WorkflowWatchPage(
                requestedAfterCursor = "10",
                replayThroughCursor = "12",
                nextCursor = "12",
                events = listOf(
                    WorkflowWatchEvent("12", WorkflowWatchEventKind.NodeActivated, "x", "PLAN"),
                ),
            ),
        )
        assertTrue(withEvents.changed)
        assertFalse(withEvents.gap)
        assertEquals("12", withEvents.nextAfterCursor)

        // The scan moved past facts the page did not carry. What was elided has
        // to be re-read from state authority, not guessed.
        val gapped = WorkflowGraphModel.watchSignal(
            WorkflowWatchPage("10", "40", "40", emptyList()),
        )
        assertTrue(gapped.gap)
        assertTrue(gapped.changed)

        val quiet = WorkflowGraphModel.watchSignal(
            WorkflowWatchPage("10", "10", "10", emptyList()),
        )
        assertFalse(quiet.changed)
        assertFalse(quiet.gap)
    }

    @Test
    fun `a page with no next cursor drops the position rather than inventing one`() {
        val signal = WorkflowGraphModel.watchSignal(
            WorkflowWatchPage("10", "10", null, emptyList()),
        )
        // Null, not "0": resuming from zero would replay the whole journal, and
        // resuming from "10" would claim a position the page never published.
        assertNull(signal.nextAfterCursor)
    }

    @Test
    fun `the gap comparison is u64-exact`() {
        // 9007199254740993 and …992 are the same Double. Only BigInteger sees
        // the gap, and only a gap forces the re-read that keeps the graph true.
        val signal = WorkflowGraphModel.watchSignal(
            WorkflowWatchPage(
                requestedAfterCursor = "9007199254740992",
                replayThroughCursor = "9007199254740993",
                nextCursor = "9007199254740993",
                events = emptyList(),
            ),
        )
        assertTrue(signal.gap)
    }

    // ---------- the read, and its three absences ----------

    @Test
    fun `state null is no graph, and never an empty one`() {
        val read = WorkflowRpcAdapter.parseGraphState(org.json.JSONObject("""{"state":null}"""))
        assertEquals(WorkflowGraphRead.NoGraph, read)
        assertEquals(
            WorkflowGraphRead.NoGraph,
            WorkflowRpcAdapter.parseGraphState(org.json.JSONObject("{}")),
        )
        // And "no graph" is not the same value as "we never asked".
        assertNotEquals(WorkflowGraphRead.Unread, read)
    }

    @Test
    fun `an ast that published no edge list is not a zero-edge topology`() {
        val ast = WorkflowRpcAdapter.parseAst(
            org.json.JSONObject("""{"workflow_id":"w","nodes":[{"node":"A"}]}"""),
        )
        assertFalse(ast.edgesPublished)
        assertTrue(ast.edges.isEmpty())

        val published = WorkflowRpcAdapter.parseAst(
            org.json.JSONObject("""{"workflow_id":"w","nodes":[{"node":"A"}],"edges":[]}"""),
        )
        assertTrue(published.edgesPublished)
    }

    // ---------- layout ----------

    @Test
    fun `layering follows forward edges and back edges never deepen a node`() {
        val layout = WorkflowLayoutEngine.layout(FakeWorkflowLoom.AST)
        assertEquals(0, layout.placement("PLAN")!!.layer)
        assertEquals(1, layout.placement("IMPLEMENT")!!.layer)
        // The fan-out lands on one layer, and the join is below both.
        assertEquals(2, layout.placement("REVIEW")!!.layer)
        assertEquals(2, layout.placement("TEST")!!.layer)
        assertEquals(3, layout.placement("SHIP")!!.layer)
        assertEquals(4, layout.layers)
        assertEquals(2, layout.widestLayer)
    }

    @Test
    fun `every published edge is routed, and exactly one is a return path`() {
        val layout = WorkflowLayoutEngine.layout(FakeWorkflowLoom.AST)
        // The pin the brief names: the count of drawn edges is the count of
        // published edges, and the back edge is drawn as one.
        assertEquals(FakeWorkflowLoom.AST.edges.size, layout.routes.size)
        assertEquals(1, layout.backwardRoutes)
        val back = layout.routes.single { it.backward }
        assertEquals(WorkflowEdgeKind.Back, back.edge.kind)
        assertEquals("REVIEW", back.from!!.node)
        assertEquals("IMPLEMENT", back.to!!.node)
        // A graph-input edge has no source node, and none is invented.
        val input = layout.routes.single { it.edge.kind == WorkflowEdgeKind.GraphInput }
        assertNull(input.from)
        assertEquals("PLAN", input.to!!.node)
    }

    @Test
    fun `a forward cycle still lays out rather than hanging`() {
        val ast = WorkflowAst(
            nodes = listOf(WorkflowAstNode("A"), WorkflowAstNode("B")),
            edges = listOf(
                WorkflowEdge(1, WorkflowEdgeKind.Forward, "forward", "A", "B", "t"),
                WorkflowEdge(2, WorkflowEdgeKind.Forward, "forward", "B", "A", "t"),
            ),
            edgesPublished = true,
        )
        val layout = WorkflowLayoutEngine.layout(ast)
        assertEquals(2, layout.placements.size)
        // One of the two closes the loop and is drawn as a return path.
        assertEquals(1, layout.backwardRoutes)
    }

    // ---------- the AST tree ----------

    @Test
    fun `the tree walks forward edges, prints a rejoin once, and keeps every node`() {
        val lines = WorkflowAstTree.lines(FakeWorkflowLoom.AST)
        assertEquals("PLAN", lines.first().node)
        assertEquals(0, lines.first().depth)
        // SHIP is reached from both REVIEW and TEST: printed twice, descended
        // into once, and the second is flagged rather than silently dropped.
        val ships = lines.filter { it.node == "SHIP" }
        assertEquals(2, ships.size)
        assertEquals(1, ships.count { it.repeat })
        // Every declared node reaches the tree.
        assertEquals(
            FakeWorkflowLoom.AST.nodes.map { it.node }.toSet(),
            lines.map { it.node }.toSet(),
        )
    }

    @Test
    fun `a node no forward edge reaches is still listed`() {
        val ast = WorkflowAst(
            nodes = listOf(WorkflowAstNode("A"), WorkflowAstNode("ORPHAN")),
            edges = listOf(
                WorkflowEdge(1, WorkflowEdgeKind.GraphInput, "graph_input", null, "A", "t"),
            ),
            edgesPublished = true,
        )
        assertTrue(WorkflowAstTree.lines(ast).any { it.node == "ORPHAN" })
    }

    // ---------- the drill-in ----------

    @Test
    fun `drill-in matches the parent attempt, not the node name`() {
        val links = FakeWorkflowLoom.CHILD_LINKS
        // Same node name, different graph: it must not resolve here.
        val other = links.single { it.childSessionId == "s-child-other" }
        assertEquals("IMPLEMENT", other.parentAttempt.node)
        assertNull(
            ChildGraphIndex
                .linksFor(links, FakeWorkflowLoom.GRAPH_ID, "IMPLEMENT")
                .firstOrNull { it.childSessionId == "s-child-other" },
        )
        // And the newest attempt wins: after a back-edge retry, opening the
        // first attempt's session would open the work that was rejected.
        val latest = ChildGraphIndex.latestLink(links, FakeWorkflowLoom.GRAPH_ID, "IMPLEMENT")
        assertEquals("s-child-implement-2", latest!!.childSessionId)
        assertEquals(2, latest.parentAttempt.attempt)
    }

    @Test
    fun `a node with no attached child resolves to nothing`() {
        assertNull(
            ChildGraphIndex.latestLink(
                FakeWorkflowLoom.CHILD_LINKS,
                FakeWorkflowLoom.GRAPH_ID,
                "SHIP",
            ),
        )
    }

    // ---------- the chip ----------

    @Test
    fun `the chip distinguishes unread, none and a real workflow`() {
        assertEquals(
            WorkflowChipState.Unread,
            WorkflowChipModel.resolve(workflow = null, read = false),
        )
        assertEquals(WorkflowChipState.None, WorkflowChipModel.resolve(workflow = null))
        val active = WorkflowChipModel.resolve(FakeWorkflowLoom.SESSION_WORKFLOW)
            as WorkflowChipState.Active
        assertEquals("implement_verify", active.template)
        assertEquals("active", active.phase)
        // `ready_nodes` is the daemon's complete open-obligation list.
        assertEquals(2, active.activeNodes)
    }

    @Test
    fun `a legacy reduction with only a current node counts one`() {
        val legacy = SessionWorkflow(
            graphId = "g",
            template = "t",
            phase = "active",
            currentNode = "IMPLEMENT",
        )
        assertEquals(1, (WorkflowChipModel.resolve(legacy) as WorkflowChipState.Active).activeNodes)
    }

    @Test
    fun `a graph that is not active has no open obligation`() {
        val done = FakeWorkflowLoom.SESSION_WORKFLOW.copy(phase = "completed")
        assertEquals(0, (WorkflowChipModel.resolve(done) as WorkflowChipState.Active).activeNodes)
    }

    @Test
    fun `an unavailable daemon outranks every other chip answer`() {
        val state = WorkflowChipModel.resolve(
            workflow = FakeWorkflowLoom.SESSION_WORKFLOW,
            unavailable = WorkflowRpcAdapter.FEATURE_WORKFLOW_GRAPH_V1,
        )
        assertEquals(
            WorkflowChipState.Unavailable(WorkflowRpcAdapter.FEATURE_WORKFLOW_GRAPH_V1),
            state,
        )
    }

    // ---------- the fake's own transitions ----------

    @Test
    fun `the scripted daemon walks its recorded steps and settles completed`() {
        val fake = FakeWorkflowLoom()
        fake.rewind(0)
        val first = (fake.graphState() as WorkflowGraphRead.Graph).snapshot
        assertEquals(WorkflowGraphPhase.Active, first.phase)
        assertEquals(WorkflowNodePhase.Activated, first.nodeState("PLAN")!!.phase)
        while (fake.advance()) Unit
        val last = (fake.graphState() as WorkflowGraphRead.Graph).snapshot
        assertEquals(WorkflowGraphPhase.Completed, last.phase)
        // The retry really happened: IMPLEMENT ran twice and the graph counted
        // one back-edge activation.
        assertEquals(2, last.nodeState("IMPLEMENT")!!.iteration)
        assertEquals(1, last.backEdgeActivations)
    }

    @Test
    fun `a rejected node carries the daemon's code and message`() {
        val fake = FakeWorkflowLoom()
        fake.rewind(3)
        val snapshot = (fake.graphState() as WorkflowGraphRead.Graph).snapshot
        val review = snapshot.nodeState("REVIEW")!!
        assertEquals(WorkflowNodePhase.Rejected, review.phase)
        assertEquals("evidence_rejected", review.rejection!!.code)
    }
}
