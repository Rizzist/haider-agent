package ai.diffforge.haider.ui.workflow

import java.math.BigInteger

/**
 * Typed view models for `workflow_graph_v1`, ported from the desktop
 * `workflowGraphModel.js` house law and checked against the Rust declarations
 * (`haider-protocol/src/graph.rs:216-460`, `haider-rpc/src/frame.rs:4311-4326`,
 * `:5399-5407`).
 *
 * The four laws this file exists to keep:
 *
 *  1. `state: null` is the daemon saying **there is no live workflow graph for
 *     this session**. It is a distinct answer from *never asked*, and neither is
 *     a fabricated empty graph. [WorkflowGraphRead] has a case for each.
 *  2. The daemon's `nodes` + `phase` are the authority. A watch page is a
 *     **change signal** — "re-read `workflow.graph.state`" — never a reduction
 *     input. Nothing here folds an event into a node phase.
 *  3. Cursors are u64. They travel and compare as decimal **strings**:
 *     9007199254740993 must never become …992. [cursorAdvances] and
 *     [watchSignal] compare with [BigInteger]; nothing parses a cursor to Long.
 *  4. Absence says so. A node whose record published no phase is
 *     [WorkflowNodePhase.Unpublished] — never "waiting", and never inferred
 *     from the node's name. An ast that published no edge list is
 *     `edgesPublished = false` — never a zero-edge topology stated as fact.
 */

// ---------- topology ----------

/** `WorkflowEdgeKind` (graph.rs:216). A kind this build does not know is kept. */
enum class WorkflowEdgeKind { GraphInput, Forward, Back, Unknown }

/**
 * One typed runtime edge (`WorkflowActivationEdge`, graph.rs:223).
 *
 * `from` is legitimately absent on a `graph_input` edge; that absence is
 * rendered as the graph input, never as a node called "null".
 */
data class WorkflowEdge(
    val id: Int,
    val kind: WorkflowEdgeKind,
    /** The daemon's own spelling, kept so an unknown kind can still be shown. */
    val kindRaw: String?,
    val from: String?,
    val to: String,
    val evidenceType: String = "",
)

/** `WorkflowJoinSemantics` (graph.rs:237): the exact edge sets that activate. */
data class WorkflowJoin(
    val initialAll: List<Int> = emptyList(),
    val reactivateAny: List<Int> = emptyList(),
) {
    /** A fork/join node: first activation waits for more than one edge. */
    val isJoin: Boolean get() = initialAll.size > 1

    /** The node can be re-entered by an explicit back edge. */
    val reactivates: Boolean get() = reactivateAny.isNotEmpty()
}

/** `WorkflowActivationNode` (graph.rs:244). */
data class WorkflowAstNode(
    val node: String,
    val inputType: String = "",
    val outputType: String = "",
    val join: WorkflowJoin = WorkflowJoin(),
    val convergenceGate: Boolean = false,
)

/**
 * `WorkflowActivationAst` (graph.rs:257) — the frozen executable topology.
 *
 * [edgesPublished] is false when the ast carried no `edges` key at all. The
 * screen then says the topology was not published rather than drawing a graph
 * with no edges as though that were the daemon's answer.
 */
data class WorkflowAst(
    val workflowId: String = "",
    val workflowDigest: String? = null,
    val inputType: String = "",
    val outputType: String = "",
    val nodes: List<WorkflowAstNode> = emptyList(),
    val edges: List<WorkflowEdge> = emptyList(),
    val edgesPublished: Boolean = false,
    val maxBackEdgeActivations: Int? = null,
) {
    fun edgeById(id: Int): WorkflowEdge? = edges.firstOrNull { it.id == id }

    val backEdges: List<WorkflowEdge> get() = edges.filter { it.kind == WorkflowEdgeKind.Back }
    val forwardEdges: List<WorkflowEdge> get() = edges.filter { it.kind == WorkflowEdgeKind.Forward }
    val inputEdges: List<WorkflowEdge> get() = edges.filter { it.kind == WorkflowEdgeKind.GraphInput }
}

// ---------- live state ----------

/**
 * `WorkflowNodePhase` (graph.rs:348) plus the two honest absences the wire
 * itself cannot spell: a record that published no phase, and a phase this
 * build does not recognise.
 */
enum class WorkflowNodePhase { Waiting, Activated, Completed, Rejected, Unknown, Unpublished }

/** `WorkflowNodeRejectCode` (graph.rs:323), kept verbatim when unrecognised. */
data class WorkflowRejection(
    val code: String,
    val message: String,
    val convergenceGate: Boolean = false,
)

/** `WorkflowNodeState` (graph.rs:356). */
data class WorkflowNodeState(
    val node: String,
    val phase: WorkflowNodePhase,
    /** The daemon's own word, so an unknown phase is still displayable. */
    val phaseRaw: String?,
    val iteration: Int = 0,
    /** Absent until the node has been activated at least once. */
    val activationOrder: Long? = null,
    val inputEdgeIds: List<Int> = emptyList(),
    val outputCount: Int = 0,
    val convergenceDigest: String? = null,
    val rejection: WorkflowRejection? = null,
    val updatedCursor: String? = null,
)

/** `WorkflowGraphPhase` (graph.rs:375). */
enum class WorkflowGraphPhase { Active, Completed, Rejected, Unknown }

/** `WorkflowGraphState` (graph.rs:391). */
data class WorkflowGraphSnapshot(
    val graphId: String,
    val ast: WorkflowAst,
    /**
     * THE topology fence. Carried verbatim; never recomputed from the ast and
     * never fabricated. Absence is null, and says "not published".
     */
    val astDigest: String?,
    val phase: WorkflowGraphPhase,
    val phaseRaw: String?,
    /** Decimal string. "0" is a real cursor; absence is null, never "0". */
    val throughCursor: String?,
    val nextActivationOrder: Long? = null,
    val backEdgeActivations: Int? = null,
    val nodes: List<WorkflowNodeState> = emptyList(),
) {
    fun nodeState(name: String): WorkflowNodeState? = nodes.firstOrNull { it.node == name }

    val activeNodes: List<WorkflowNodeState>
        get() = nodes.filter { it.phase == WorkflowNodePhase.Activated }
}

/**
 * What a `workflow.graph.state` read actually said.
 *
 * [Unread] and [NoGraph] are deliberately different: round-tripping "we have
 * not asked" into "there is no graph" is the exact fabrication the desktop
 * house law forbids, and on a phone — where the screen is opened before the
 * first read lands — it is the difference the user sees first.
 */
sealed interface WorkflowGraphRead {
    data object Unread : WorkflowGraphRead

    /** The daemon answered, and its answer was `state: null`. */
    data object NoGraph : WorkflowGraphRead

    data class Graph(val snapshot: WorkflowGraphSnapshot) : WorkflowGraphRead

    /**
     * The daemon does not advertise the feature, or refused. The reason is the
     * daemon's own typed code (a feature name or an error code), shown as-is:
     * nothing on this screen may look like it is working when it is not.
     */
    data class Unavailable(val reason: String) : WorkflowGraphRead
}

// ---------- watch ----------

/** One journal fact from a watch page. An unknown `type` is kept, never dropped. */
data class WorkflowWatchEvent(
    val cursor: String?,
    /** One of the four v1 fact types, or null when unrecognised. */
    val kind: WorkflowWatchEventKind,
    val typeRaw: String?,
    val node: String? = null,
)

enum class WorkflowWatchEventKind { GraphStarted, NodeActivated, NodeCompleted, NodeRejected, Unknown }

/** `WorkflowGraphWatchPage` (graph.rs:453). Every cursor rides verbatim. */
data class WorkflowWatchPage(
    val requestedAfterCursor: String?,
    val replayThroughCursor: String?,
    val nextCursor: String?,
    val events: List<WorkflowWatchEvent> = emptyList(),
)

/**
 * What a page means. [changed] is exactly "re-read `workflow.graph.state`
 * now"; [gap] means the replay scan moved past facts the page did not carry,
 * so the elided change must be re-read rather than guessed.
 */
data class WorkflowWatchSignal(
    val changed: Boolean,
    val gap: Boolean,
    /** The verbatim `next_cursor` to resume from; null re-baselines from state. */
    val nextAfterCursor: String?,
)

object WorkflowCursor {
    private val DECIMAL = Regex("""^\d+$""")

    /** A cursor is a decimal string or nothing. A Long round-trip would lose u64. */
    fun orNull(value: String?): String? =
        if (value != null && DECIMAL.matches(value)) value else null

    fun advances(current: String?, candidate: String?): Boolean {
        val next = orNull(candidate) ?: return false
        val held = orNull(current) ?: return true
        return BigInteger(next) > BigInteger(held)
    }

    const val BASELINE: String = "0"
}

object WorkflowGraphModel {

    fun nodePhase(raw: String?): WorkflowNodePhase = when {
        raw == null || raw.isEmpty() -> WorkflowNodePhase.Unpublished
        raw == "waiting" -> WorkflowNodePhase.Waiting
        raw == "activated" -> WorkflowNodePhase.Activated
        raw == "completed" -> WorkflowNodePhase.Completed
        raw == "rejected" -> WorkflowNodePhase.Rejected
        else -> WorkflowNodePhase.Unknown
    }

    fun graphPhase(raw: String?): WorkflowGraphPhase = when (raw) {
        "active" -> WorkflowGraphPhase.Active
        "completed" -> WorkflowGraphPhase.Completed
        "rejected" -> WorkflowGraphPhase.Rejected
        else -> WorkflowGraphPhase.Unknown
    }

    fun edgeKind(raw: String?): WorkflowEdgeKind = when (raw) {
        "graph_input" -> WorkflowEdgeKind.GraphInput
        "forward" -> WorkflowEdgeKind.Forward
        "back" -> WorkflowEdgeKind.Back
        else -> WorkflowEdgeKind.Unknown
    }

    fun watchEventKind(raw: String?): WorkflowWatchEventKind = when (raw) {
        "workflow_graph_started" -> WorkflowWatchEventKind.GraphStarted
        "workflow_node_activated" -> WorkflowWatchEventKind.NodeActivated
        "workflow_node_completed" -> WorkflowWatchEventKind.NodeCompleted
        "workflow_node_rejected" -> WorkflowWatchEventKind.NodeRejected
        else -> WorkflowWatchEventKind.Unknown
    }

    /**
     * The page is a change signal, never a reduction input.
     *
     * Every comparison is [BigInteger] over the decimal strings: Long math
     * would collapse u64-scale cursors and misjudge both the gap and the
     * advance.
     */
    fun watchSignal(page: WorkflowWatchPage): WorkflowWatchSignal {
        val requested = WorkflowCursor.orNull(page.requestedAfterCursor)
        var explained: BigInteger? = requested?.let { BigInteger(it) }
        for (event in page.events) {
            val at = WorkflowCursor.orNull(event.cursor)?.let { BigInteger(it) } ?: continue
            val held = explained
            if (held == null || at > held) explained = at
        }
        val replay = WorkflowCursor.orNull(page.replayThroughCursor)?.let { BigInteger(it) }
        val settled = explained
        val gap = replay != null && settled != null && replay > settled
        val next = WorkflowCursor.orNull(page.nextCursor)
        val advanced = next != null &&
            (requested == null || BigInteger(next) != BigInteger(requested))
        return WorkflowWatchSignal(
            changed = page.events.isNotEmpty() || gap || advanced,
            gap = gap,
            nextAfterCursor = next,
        )
    }
}

// ---------- the session chip ----------

/**
 * The per-session workflow fact the roster carries.
 *
 * `SessionSnapshot.workflow` (frame.rs:2299) is a `GraphStatus`
 * (graph.rs:2260) — the convergence-graph projection, which is what the
 * session listing publishes. It is *not* the activation projection the DAG
 * screen reads, so the chip shows only what this fact actually states.
 */
data class SessionWorkflow(
    val graphId: String,
    val template: String,
    val digest: String? = null,
    /** `GraphPhase` (graph.rs:2190): active|blocked|completed|abandoned|superseded. */
    val phase: String,
    val currentNode: String? = null,
    /** Every open obligation, in template order. Empty on legacy reductions. */
    val readyNodes: List<String> = emptyList(),
)

/** What the chip may say. Four honest values, exactly as the desktop chip has. */
sealed interface WorkflowChipState {
    /** No `graph.status` fact for this session yet — never "no workflow". */
    data object Unread : WorkflowChipState

    /** The roster carried a session with no workflow. Nothing is drawn. */
    data object None : WorkflowChipState

    data class Unavailable(val reason: String) : WorkflowChipState

    data class Active(
        val template: String,
        val phase: String,
        /**
         * Open obligations from `ready_nodes`, or the single `current_node`
         * when the daemon published only that. Never counted from the DAG
         * screen's separate projection.
         */
        val activeNodes: Int,
        val currentNode: String?,
        /** Non-null when this session is itself a subagent (delivery 3). */
        val agentType: String? = null,
    ) : WorkflowChipState
}

object WorkflowChipModel {

    /**
     * House law, ported verbatim: workflow state comes only from the session's
     * own `workflow` fact. It is never derived from the loom list, from a
     * selected agent type, or from session lineage.
     *
     * `activeNodes` prefers `ready_nodes` because that is the daemon's complete
     * open-obligation list; a legacy reduction publishes only `current_node`,
     * which counts as one. A graph that is not active has no open obligation,
     * so the count is zero rather than a stale one.
     */
    fun resolve(
        workflow: SessionWorkflow?,
        agentType: String? = null,
        unavailable: String? = null,
        read: Boolean = true,
    ): WorkflowChipState = when {
        unavailable != null -> WorkflowChipState.Unavailable(unavailable)
        !read -> WorkflowChipState.Unread
        workflow == null -> WorkflowChipState.None
        else -> WorkflowChipState.Active(
            template = workflow.template,
            phase = workflow.phase,
            activeNodes = when {
                workflow.phase != "active" -> 0
                workflow.readyNodes.isNotEmpty() -> workflow.readyNodes.size
                workflow.currentNode != null -> 1
                else -> 0
            },
            currentNode = workflow.currentNode,
            agentType = agentType,
        )
    }
}

// ---------- drill-in ----------

/** `ParentGraphAttempt` (graph.rs:2076): the exact attempt a child hangs off. */
data class ParentGraphAttempt(val graphId: String, val node: String, val attempt: Int)

/**
 * One `ChildGraphAttached` journal fact (graph.rs:2102), reduced to what a
 * drill-in needs.
 *
 * The mapping is the **coordinate**, never the name: two sibling children can
 * carry the same node name under different graphs and different attempts, and
 * matching on the name alone opens somebody else's session.
 */
data class ChildGraphLink(
    val parentAttempt: ParentGraphAttempt,
    val childSessionId: String,
    val childGraphId: String,
    val childRunId: String? = null,
    /** `ChildWorkflowSelector` (graph.rs:1862), verbatim. */
    val workflow: String? = null,
    val template: String? = null,
    val digest: String? = null,
    val parentSlot: String? = null,
)

object ChildGraphIndex {

    /** Every child attached to this exact (graph, node), oldest attempt first. */
    fun linksFor(links: List<ChildGraphLink>, graphId: String, node: String): List<ChildGraphLink> =
        links
            .filter { it.parentAttempt.graphId == graphId && it.parentAttempt.node == node }
            .sortedBy { it.parentAttempt.attempt }

    /**
     * The child a tap on this node opens: the newest attempt at that exact
     * coordinate, or nothing. A node with no attached child drills into its
     * evidence instead — it does not open a plausible-looking neighbour.
     */
    fun latestLink(links: List<ChildGraphLink>, graphId: String, node: String): ChildGraphLink? =
        linksFor(links, graphId, node).maxByOrNull { it.parentAttempt.attempt }
}
