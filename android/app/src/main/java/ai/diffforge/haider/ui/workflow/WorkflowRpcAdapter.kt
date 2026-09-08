package ai.diffforge.haider.ui.workflow

import org.json.JSONArray
import org.json.JSONObject

/**
 * The one file that knows the workflow-graph wire shapes.
 *
 * Every builder and parser is derived from the frozen Rust declarations, not
 * from the desktop client's Tauri command names:
 *
 * | door | Rust | canonical shape |
 * |---|---|---|
 * | `workflow.graph.state` | `RequestBody::WorkflowGraphState` (frame.rs:4313) | `{session_id, graph_id?}` — `graph_id` is `skip_serializing_if`, so it is **omitted**, never sent as null; omission asks for the session's most recently changed graph |
 * | | resp (frame.rs:5399) | `{state?}` — an **absent or null** `state` is the daemon saying there is no live graph, which is not an empty graph |
 * | `workflow.graph.watch` | `RequestBody::WorkflowGraphWatch` (frame.rs:4320) | `{session_id, after_cursor, limit}` — `after_cursor` is a **u64 number**, not a string |
 * | | resp (frame.rs:5404) | `{page: WorkflowGraphWatchPage}` (graph.rs:453) |
 * | `graph.status` | frame.rs:4763 / resp :4766 | `{session_id}` → `{status?: GraphStatus}` (graph.rs:2260) |
 * | `graph.inspect` | frame.rs:4297 / resp :4814 | `{session_id, cursor?, limit}` → `{snapshot, next_cursor?}` |
 * | `workflow.instance` | frame.rs:4224 / resp :5340 | `{workflow_id, template_digest?}` → `{instance?}` |
 *
 * **Cursors.** `after_cursor`, `through_cursor`, `next_cursor` and a node's
 * `updated_cursor` are u64. `org.json` cannot carry one: `JSONObject.put` of a
 * `Number` routes through `numberToString`, which converts to `long` and turns
 * 18446744073709551615 into 9223372036854775807, and the tokener parses an
 * out-of-range literal into a `double`, turning 9007199254740993 into …992.
 *
 * So cursors are decimal **strings** everywhere in this app, [watchBody] writes
 * the number as a raw JSON literal rather than through `org.json`, and
 * [cursorOf] refuses a cursor that arrived as a floating-point value — the
 * caller then re-baselines from a fresh `workflow.graph.state` read instead of
 * replaying from a position that is already wrong.
 */
object WorkflowRpcAdapter {

    const val METHOD_GRAPH_STATE = "workflow.graph.state"
    const val METHOD_GRAPH_WATCH = "workflow.graph.watch"
    const val METHOD_GRAPH_STATUS = "graph.status"
    const val METHOD_GRAPH_INSPECT = "graph.inspect"
    const val METHOD_WORKFLOW_INSTANCE = "workflow.instance"

    /** `FEATURE_WORKFLOW_GRAPH_V1` (frame.rs:602). The honest unavailable reason. */
    const val FEATURE_WORKFLOW_GRAPH_V1 = "workflow_graph_v1"

    /** `FEATURE_SESSION_WORKFLOW_STATE_V1` (frame.rs:532) — the chip's fact. */
    const val FEATURE_SESSION_WORKFLOW_STATE_V1 = "session_workflow_state_v1"

    /** `EventPayload::ChildGraphAttached` (lib.rs:148), snake_case tagged. */
    const val EVENT_CHILD_GRAPH_ATTACHED = "child_graph_attached"

    /** The watch page cap; the daemon bounds it further. */
    const val WATCH_LIMIT = 64

    // ---------- requests ----------

    fun graphStateRequest(sessionId: String, graphId: String? = null): JSONObject =
        JSONObject()
            .put("method", METHOD_GRAPH_STATE)
            .put("session_id", sessionId)
            .also { if (!graphId.isNullOrEmpty()) it.put("graph_id", graphId) }

    /**
     * The watch body, rendered by hand.
     *
     * `after_cursor` is emitted as a bare decimal literal so a u64 above
     * `Long.MAX_VALUE` survives; a non-decimal cursor is refused outright
     * rather than silently baselined to zero, because "start from the
     * beginning" and "I lost my position" are different requests.
     */
    fun watchBody(sessionId: String, afterCursor: String, limit: Int = WATCH_LIMIT): String {
        val cursor = requireNotNull(WorkflowCursor.orNull(afterCursor)) {
            "after_cursor must be a decimal u64 string"
        }
        val session = JSONObject.quote(sessionId)
        return """{"method":"$METHOD_GRAPH_WATCH","session_id":$session,""" +
            """"after_cursor":$cursor,"limit":$limit}"""
    }

    fun graphStatusRequest(sessionId: String): JSONObject =
        JSONObject().put("method", METHOD_GRAPH_STATUS).put("session_id", sessionId)

    fun graphInspectRequest(sessionId: String, cursor: String? = null, limit: Int): JSONObject =
        JSONObject()
            .put("method", METHOD_GRAPH_INSPECT)
            .put("session_id", sessionId)
            .put("limit", limit)
            .also { if (!cursor.isNullOrEmpty()) it.put("cursor", cursor) }

    fun workflowInstanceRequest(workflowId: String, templateDigest: String? = null): JSONObject =
        JSONObject()
            .put("method", METHOD_WORKFLOW_INSTANCE)
            .put("workflow_id", workflowId)
            .also { if (!templateDigest.isNullOrEmpty()) it.put("template_digest", templateDigest) }

    // ---------- cursors ----------

    /**
     * A cursor read from a parsed frame.
     *
     * A `String` rides verbatim. An `Integer`/`Long` is exact and becomes its
     * decimal string. Anything else — a `Double`, which is what `org.json`
     * produces for an out-of-range integer literal — has already lost
     * precision and is refused.
     */
    fun cursorOf(value: Any?): String? = when (value) {
        is String -> WorkflowCursor.orNull(value)
        is Int -> if (value >= 0) value.toString() else null
        is Long -> if (value >= 0) value.toString() else null
        else -> null
    }

    private fun JSONObject.cursor(key: String): String? =
        if (has(key) && !isNull(key)) cursorOf(get(key)) else null

    private fun JSONObject.stringOrNull(key: String): String? =
        if (has(key) && !isNull(key)) optString(key).takeIf { it.isNotEmpty() } else null

    private fun JSONObject.longOrNull(key: String): Long? =
        if (has(key) && !isNull(key)) optLong(key) else null

    private fun JSONObject.intOrNull(key: String): Int? =
        if (has(key) && !isNull(key)) optInt(key) else null

    private fun JSONArray.objects(): List<JSONObject> =
        (0 until length()).mapNotNull { optJSONObject(it) }

    private fun JSONArray.ints(): List<Int> = (0 until length()).map { optInt(it) }

    // ---------- workflow.graph.state ----------

    /**
     * Parses a `workflow.graph.state` response.
     *
     * `{}` and `{"state":null}` are the same daemon statement — "no live
     * workflow graph for this session" — and both become [WorkflowGraphRead.NoGraph].
     * There is no path here that returns [WorkflowGraphRead.Unread]: not having
     * asked is a fact about the client, and a parser cannot observe it.
     */
    fun parseGraphState(response: JSONObject): WorkflowGraphRead {
        val state = if (response.has("state") && !response.isNull("state")) {
            response.optJSONObject("state")
        } else {
            null
        } ?: return WorkflowGraphRead.NoGraph
        return WorkflowGraphRead.Graph(parseSnapshot(state))
    }

    fun parseSnapshot(state: JSONObject): WorkflowGraphSnapshot {
        val phaseRaw = state.stringOrNull("phase")
        return WorkflowGraphSnapshot(
            graphId = state.optString("graph_id"),
            ast = parseAst(state.optJSONObject("ast")),
            astDigest = state.stringOrNull("ast_digest"),
            phase = WorkflowGraphModel.graphPhase(phaseRaw),
            phaseRaw = phaseRaw,
            throughCursor = state.cursor("through_cursor"),
            nextActivationOrder = state.longOrNull("next_activation_order"),
            backEdgeActivations = state.intOrNull("back_edge_activations"),
            nodes = state.optJSONArray("nodes")?.objects()?.map(::parseNodeState).orEmpty(),
        )
    }

    fun parseAst(ast: JSONObject?): WorkflowAst {
        if (ast == null) return WorkflowAst()
        val rawEdges = ast.optJSONArray("edges")
        return WorkflowAst(
            workflowId = ast.optString("workflow_id"),
            workflowDigest = ast.stringOrNull("workflow_digest"),
            inputType = ast.optString("input_type"),
            outputType = ast.optString("output_type"),
            nodes = ast.optJSONArray("nodes")?.objects()?.map(::parseAstNode).orEmpty(),
            // `skip_serializing_if = "Vec::is_empty"` never applies to the ast's
            // edges, so an absent key means an ast this client cannot draw —
            // said out loud rather than drawn as a zero-edge topology.
            edges = rawEdges?.objects()?.map(::parseEdge).orEmpty(),
            edgesPublished = rawEdges != null,
            maxBackEdgeActivations = ast.intOrNull("max_back_edge_activations"),
        )
    }

    private fun parseAstNode(node: JSONObject): WorkflowAstNode {
        val join = node.optJSONObject("join")
        return WorkflowAstNode(
            node = node.optString("node"),
            inputType = node.optString("input_type"),
            outputType = node.optString("output_type"),
            join = WorkflowJoin(
                initialAll = join?.optJSONArray("initial_all")?.ints().orEmpty(),
                reactivateAny = join?.optJSONArray("reactivate_any")?.ints().orEmpty(),
            ),
            convergenceGate = node.optBoolean("convergence_gate", false),
        )
    }

    private fun parseEdge(edge: JSONObject): WorkflowEdge {
        val kindRaw = edge.stringOrNull("kind")
        return WorkflowEdge(
            id = edge.optInt("id"),
            kind = WorkflowGraphModel.edgeKind(kindRaw),
            kindRaw = kindRaw,
            from = edge.stringOrNull("from"),
            to = edge.optString("to"),
            evidenceType = edge.optString("evidence_type"),
        )
    }

    fun parseNodeState(node: JSONObject): WorkflowNodeState {
        // `phase` is required on the wire, so an absent one is a daemon this
        // client does not understand — Unpublished, never a guessed "waiting".
        val phaseRaw = node.stringOrNull("phase")
        val rejection = node.optJSONObject("rejection")
        return WorkflowNodeState(
            node = node.optString("node"),
            phase = WorkflowGraphModel.nodePhase(phaseRaw),
            phaseRaw = phaseRaw,
            iteration = node.optInt("iteration"),
            activationOrder = node.longOrNull("activation_order"),
            inputEdgeIds = node.optJSONArray("inputs")?.objects()?.map { it.optInt("edge_id") }
                .orEmpty(),
            outputCount = node.optJSONArray("outputs")?.length() ?: 0,
            convergenceDigest = node.optJSONObject("convergence")?.stringOrNull("decision_digest"),
            rejection = rejection?.let {
                WorkflowRejection(
                    code = it.optString("code"),
                    message = it.optString("message"),
                    convergenceGate = it.optBoolean("convergence_gate", false),
                )
            },
            updatedCursor = node.cursor("updated_cursor"),
        )
    }

    // ---------- workflow.graph.watch ----------

    fun parseWatchPage(response: JSONObject): WorkflowWatchPage {
        val page = response.optJSONObject("page") ?: response
        return WorkflowWatchPage(
            requestedAfterCursor = page.cursor("requested_after_cursor"),
            replayThroughCursor = page.cursor("replay_through_cursor"),
            nextCursor = page.cursor("next_cursor"),
            events = page.optJSONArray("events")?.objects()?.map(::parseWatchEvent).orEmpty(),
        )
    }

    private fun parseWatchEvent(entry: JSONObject): WorkflowWatchEvent {
        val event = entry.optJSONObject("event")
        val typeRaw = event?.stringOrNull("type")
        return WorkflowWatchEvent(
            cursor = entry.cursor("cursor"),
            kind = WorkflowGraphModel.watchEventKind(typeRaw),
            typeRaw = typeRaw,
            node = event?.stringOrNull("node"),
        )
    }

    // ---------- graph.status (the session chip) ----------

    fun parseGraphStatus(response: JSONObject): SessionWorkflow? {
        val status = response.optJSONObject("status") ?: return null
        return parseSessionWorkflow(status)
    }

    /** `GraphStatus` (graph.rs:2260), as the session snapshot carries it. */
    fun parseSessionWorkflow(status: JSONObject): SessionWorkflow = SessionWorkflow(
        graphId = status.optString("graph_id"),
        template = status.optString("template"),
        digest = status.stringOrNull("digest"),
        phase = status.optString("phase"),
        currentNode = status.stringOrNull("current_node"),
        readyNodes = status.optJSONArray("ready_nodes")
            ?.let { array -> (0 until array.length()).map { array.optString(it) } }
            .orEmpty()
            .filter { it.isNotEmpty() },
    )

    // ---------- ChildGraphAttached (the drill-in) ----------

    /**
     * Reduces one session-journal envelope into a drill-in link, or null.
     *
     * The mapping is `parent_attempt` — graph id, node **and** attempt — plus
     * the `child_session_id` the daemon itself recorded. Nothing here matches a
     * child by its name, its title or its agent type: a sibling with the same
     * node name under another graph is a different session, and opening it
     * would be the worst kind of plausible answer.
     */
    fun parseChildGraphAttached(payload: JSONObject): ChildGraphLink? {
        if (payload.optString("type") != EVENT_CHILD_GRAPH_ATTACHED) return null
        val attempt = payload.optJSONObject("parent_attempt") ?: return null
        val childSessionId = payload.stringOrNull("child_session_id") ?: return null
        val graphId = attempt.stringOrNull("graph_id") ?: return null
        val node = attempt.stringOrNull("node") ?: return null
        return ChildGraphLink(
            parentAttempt = ParentGraphAttempt(
                graphId = graphId,
                node = node,
                attempt = attempt.optInt("attempt"),
            ),
            childSessionId = childSessionId,
            childGraphId = payload.optString("child_graph_id"),
            childRunId = payload.stringOrNull("child_run_id"),
            // `ChildWorkflowSelector` is one bounded string on the wire
            // (graph.rs:1858): plain | implement_verify | deeper |
            // workflow_ref(<name>). Kept verbatim; never re-spelled.
            workflow = payload.stringOrNull("workflow"),
            template = payload.stringOrNull("template"),
            digest = payload.stringOrNull("digest"),
            parentSlot = payload.stringOrNull("parent_slot"),
        )
    }

    fun parseChildGraphLinks(payloads: List<JSONObject>): List<ChildGraphLink> =
        payloads.mapNotNull(::parseChildGraphAttached)
}
