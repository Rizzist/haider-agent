package ai.diffforge.haider.ui.workflow

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Golden frames for the workflow doors, written from the frozen Rust
 * declarations rather than from the desktop client's Tauri command names.
 *
 * The bodies below are what the daemon serialises: `graph_id` omitted when the
 * caller names none (`skip_serializing_if`), `from` absent on a `graph_input`
 * edge, `convergence_gate` absent when false, cursors as bare u64 **numbers**,
 * and the activation facts tagged `type` in snake_case.
 */
class WorkflowWireShapeTest {

    // ---------- requests ----------

    @Test
    fun `graph state omits graph_id rather than sending null`() {
        val plain = WorkflowRpcAdapter.graphStateRequest("s-1")
        assertEquals("workflow.graph.state", plain.getString("method"))
        assertEquals("s-1", plain.getString("session_id"))
        // Omission is a request: "give me the most recently changed graph".
        // A null would be a different, unsupported statement.
        assertFalse(plain.has("graph_id"))
        assertEquals("g-9", WorkflowRpcAdapter.graphStateRequest("s-1", "g-9").getString("graph_id"))
    }

    @Test
    fun `the watch body writes after_cursor as a bare u64 literal`() {
        val body = WorkflowRpcAdapter.watchBody("s-1", "18446744073709551615", limit = 64)
        // The pin: unquoted, and not clamped. `JSONObject.put` of a Number goes
        // through `numberToString`, which converts to long and would emit
        // 9223372036854775807 — a position the daemon never published.
        assertTrue(body.contains("\"after_cursor\":18446744073709551615"))
        assertFalse(body.contains("\"after_cursor\":\"18446744073709551615\""))
        assertFalse(body.contains("9223372036854775807"))
        // It is still valid JSON, and the other fields ride normally.
        val parsed = JSONObject(body)
        assertEquals("workflow.graph.watch", parsed.getString("method"))
        assertEquals("s-1", parsed.getString("session_id"))
        assertEquals(64, parsed.getInt("limit"))
    }

    @Test(expected = IllegalArgumentException::class)
    fun `a watch cannot be built from a cursor that is not a decimal u64`() {
        // "I lost my position" and "start from the beginning" are different
        // requests, and only the caller can decide which one it means.
        WorkflowRpcAdapter.watchBody("s-1", "", limit = 8)
    }

    // ---------- workflow.graph.state ----------

    private val stateFrame = """
        {"state":{
          "graph_id":"g-1",
          "ast":{
            "workflow_id":"implement_verify",
            "workflow_digest":"9f31c0a7c4e21b55",
            "input_type":"Task",
            "output_type":"Ship",
            "nodes":[
              {"node":"PLAN","input_type":"Task","output_type":"Plan","join":{"initial_all":[1]}},
              {"node":"IMPLEMENT","input_type":"Plan","output_type":"Patch",
               "join":{"initial_all":[2],"reactivate_any":[7]}},
              {"node":"SHIP","input_type":"Verdict","output_type":"Ship",
               "join":{"initial_all":[5,6]},"convergence_gate":true}
            ],
            "edges":[
              {"id":1,"kind":"graph_input","to":"PLAN","evidence_type":"Task"},
              {"id":2,"kind":"forward","from":"PLAN","to":"IMPLEMENT","evidence_type":"Plan"},
              {"id":7,"kind":"back","from":"IMPLEMENT","to":"PLAN","evidence_type":"Verdict"}
            ],
            "max_back_edge_activations":3
          },
          "ast_digest":"b7d41f0c9a2e5648",
          "phase":"active",
          "through_cursor":42,
          "next_activation_order":9,
          "back_edge_activations":1,
          "nodes":[
            {"node":"PLAN","phase":"completed","iteration":1,"activation_order":1,
             "inputs":[{"edge_id":1,"evidence":{"kind":"instruct","id":"e1"}}],
             "outputs":[{"kind":"instruct","id":"e2"}],
             "convergence":{"decision_digest":"d31e77aa"},
             "updated_cursor":12},
            {"node":"IMPLEMENT","phase":"activated","iteration":2,"activation_order":5,
             "updated_cursor":26}
          ],
          "activation_order":[{"activation_order":1,"node":"PLAN","iteration":1,"cursor":12}]
        }}
    """.trimIndent()

    @Test
    fun `the state frame parses into the projection the screen draws`() {
        val read = WorkflowRpcAdapter.parseGraphState(JSONObject(stateFrame))
        val snapshot = (read as WorkflowGraphRead.Graph).snapshot
        assertEquals("g-1", snapshot.graphId)
        // The topology fence rides verbatim; nothing recomputes it.
        assertEquals("b7d41f0c9a2e5648", snapshot.astDigest)
        assertEquals(WorkflowGraphPhase.Active, snapshot.phase)
        // A JSON number cursor becomes its exact decimal string.
        assertEquals("42", snapshot.throughCursor)
        assertEquals(1, snapshot.backEdgeActivations)

        val plan = snapshot.nodeState("PLAN")!!
        assertEquals(WorkflowNodePhase.Completed, plan.phase)
        assertEquals(listOf(1), plan.inputEdgeIds)
        assertEquals(1, plan.outputCount)
        assertEquals("d31e77aa", plan.convergenceDigest)
        assertEquals("12", plan.updatedCursor)

        val implement = snapshot.nodeState("IMPLEMENT")!!
        assertEquals(WorkflowNodePhase.Activated, implement.phase)
        assertEquals(2, implement.iteration)
        // `outputs` is skip-empty on the wire: absent means none, and none is
        // zero — not "unknown".
        assertEquals(0, implement.outputCount)
        assertNull(implement.convergenceDigest)
    }

    @Test
    fun `the ast keeps join semantics, the convergence gate and every edge kind`() {
        val ast = (WorkflowRpcAdapter.parseGraphState(JSONObject(stateFrame))
            as WorkflowGraphRead.Graph).snapshot.ast
        assertTrue(ast.edgesPublished)
        assertEquals(3, ast.edges.size)

        val input = ast.edgeById(1)!!
        assertEquals(WorkflowEdgeKind.GraphInput, input.kind)
        // A graph-input edge legitimately has no source, and none is invented.
        assertNull(input.from)

        assertEquals(WorkflowEdgeKind.Forward, ast.edgeById(2)!!.kind)
        assertEquals(WorkflowEdgeKind.Back, ast.edgeById(7)!!.kind)
        assertEquals(1, ast.backEdges.size)

        val implement = ast.nodes.single { it.node == "IMPLEMENT" }
        assertEquals(listOf(2), implement.join.initialAll)
        assertEquals(listOf(7), implement.join.reactivateAny)
        assertTrue(implement.join.reactivates)
        assertFalse(implement.join.isJoin)

        val ship = ast.nodes.single { it.node == "SHIP" }
        assertTrue(ship.join.isJoin)
        assertTrue(ship.convergenceGate)
        // `convergence_gate` is skip-if-false: absent is false, not unknown.
        assertFalse(implement.convergenceGate)
    }

    @Test
    fun `an unrecognised edge kind keeps the daemon's spelling`() {
        val ast = WorkflowRpcAdapter.parseAst(
            JSONObject("""{"nodes":[],"edges":[{"id":9,"kind":"sideways","to":"A"}]}"""),
        )
        val edge = ast.edgeById(9)!!
        assertEquals(WorkflowEdgeKind.Unknown, edge.kind)
        assertEquals("sideways", edge.kindRaw)
    }

    // ---------- workflow.graph.watch ----------

    @Test
    fun `the watch page keeps unknown journal facts and their cursors`() {
        val page = WorkflowRpcAdapter.parseWatchPage(
            JSONObject(
                """
                {"page":{
                  "requested_after_cursor":10,
                  "replay_through_cursor":26,
                  "next_cursor":26,
                  "events":[
                    {"cursor":18,"event":{"type":"workflow_node_activated","node":"REVIEW"}},
                    {"cursor":23,"event":{"type":"workflow_node_rejected","node":"REVIEW"}},
                    {"cursor":26,"event":{"type":"workflow_node_quarantined","node":"REVIEW"}}
                  ]}}
                """.trimIndent(),
            ),
        )
        assertEquals("10", page.requestedAfterCursor)
        assertEquals("26", page.nextCursor)
        assertEquals(3, page.events.size)
        assertEquals(WorkflowWatchEventKind.NodeActivated, page.events[0].kind)
        assertEquals(WorkflowWatchEventKind.NodeRejected, page.events[1].kind)
        // An unrecognised fact is preserved with its raw type — never dropped
        // and never coerced onto a kind this build happens to know.
        assertEquals(WorkflowWatchEventKind.Unknown, page.events[2].kind)
        assertEquals("workflow_node_quarantined", page.events[2].typeRaw)
    }

    @Test
    fun `a cursor that arrived as a floating point value is refused`() {
        // `org.json` parses an out-of-range integer literal into a Double, and
        // 9007199254740993 is already 9007199254740992 by then. Refusing it
        // makes the caller re-baseline instead of replaying from the wrong place.
        assertNull(WorkflowRpcAdapter.cursorOf(9.007199254740993E15))
        assertEquals("42", WorkflowRpcAdapter.cursorOf(42))
        assertEquals("42", WorkflowRpcAdapter.cursorOf(42L))
        assertEquals("42", WorkflowRpcAdapter.cursorOf("42"))
        assertNull(WorkflowRpcAdapter.cursorOf(null))
    }

    // ---------- graph.status ----------

    @Test
    fun `the session workflow fact parses from a graph status`() {
        val workflow = WorkflowRpcAdapter.parseGraphStatus(
            JSONObject(
                """
                {"status":{"graph_id":"g-1","template":"implement_verify","digest":"9f31",
                 "template_version":2,"phase":"active","current_node":"IMPLEMENT",
                 "ready_nodes":["REVIEW","TEST"],"attempt":1,"nodes":[]}}
                """.trimIndent(),
            ),
        )!!
        assertEquals("implement_verify", workflow.template)
        assertEquals("active", workflow.phase)
        assertEquals(listOf("REVIEW", "TEST"), workflow.readyNodes)
        // A status the daemon did not send is null, not an empty workflow.
        assertNull(WorkflowRpcAdapter.parseGraphStatus(JSONObject("{}")))
    }

    // ---------- ChildGraphAttached ----------

    @Test
    fun `a child graph attachment parses into its exact parent attempt`() {
        val link = WorkflowRpcAdapter.parseChildGraphAttached(
            JSONObject(
                """
                {"type":"child_graph_attached",
                 "parent_run_id":"run-1","parent_call_id":"call-1","parent_tool_item_id":"item-1",
                 "parent_attempt":{"graph_id":"g-1","node":"IMPLEMENT","attempt":2},
                 "parent_slot":"patch","parent_authority":"daemon_verified",
                 "child_session_id":"s-child-2","child_run_id":"run-child-2",
                 "child_graph_id":"g-child-2","workflow":"workflow_ref(reviewer_pass)",
                 "template":"reviewer_pass","digest":"c2b0","gate_reason":"fan_out",
                 "cache_key":{"task_shape":"t","effective_grant_digest":"d","gate_structure":"s"}}
                """.trimIndent(),
            ),
        )!!
        assertEquals(ParentGraphAttempt("g-1", "IMPLEMENT", 2), link.parentAttempt)
        assertEquals("s-child-2", link.childSessionId)
        assertEquals("g-child-2", link.childGraphId)
        // `ChildWorkflowSelector` is one bounded string; it is kept verbatim
        // rather than re-spelled into a shape this client prefers.
        assertEquals("workflow_ref(reviewer_pass)", link.workflow)
    }

    @Test
    fun `a differently typed journal event is not a child attachment`() {
        assertNull(
            WorkflowRpcAdapter.parseChildGraphAttached(
                JSONObject("""{"type":"todo_graph_attached","child_graph_id":"g"}"""),
            ),
        )
        // And a malformed one is refused rather than half-built: a link without
        // a parent attempt could only ever be matched by name.
        assertNull(
            WorkflowRpcAdapter.parseChildGraphAttached(
                JSONObject("""{"type":"child_graph_attached","child_session_id":"s"}"""),
            ),
        )
    }
}
