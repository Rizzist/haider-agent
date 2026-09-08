package ai.diffforge.haider.ui.workflow

/**
 * Where the DAG's nodes go, in grid coordinates.
 *
 * Deliberately free of Compose: a layout is arithmetic over the frozen ast, and
 * arithmetic is the part that can be wrong in a way a screenshot will not show.
 * The screen multiplies these integers by the [ai.diffforge.haider.ui.theme]
 * tokens; nothing here knows a dp.
 */
data class GraphPlacement(
    val node: String,
    /** Depth from the graph input, by longest forward path. */
    val layer: Int,
    /** Position within the layer, in ast declaration order. */
    val column: Int,
    val columnsInLayer: Int,
)

data class GraphRoute(
    val edge: WorkflowEdge,
    /** Null for a `graph_input` edge: its source is the graph, not a node. */
    val from: GraphPlacement?,
    /** Null when the ast names a target that is not in its own node list. */
    val to: GraphPlacement?,
    /**
     * Drawn as a return path. True for every `back` edge, and also for a
     * forward edge whose target does not sit strictly deeper — the daemon's
     * kind is authority for semantics, the geometry only decides the curve.
     */
    val backward: Boolean,
)

data class GraphLayout(
    val placements: List<GraphPlacement>,
    val routes: List<GraphRoute>,
    val layers: Int,
    val widestLayer: Int,
) {
    fun placement(node: String): GraphPlacement? = placements.firstOrNull { it.node == node }

    /** The pin: how many edges are drawn as return paths. */
    val backwardRoutes: Int get() = routes.count { it.backward }
}

object WorkflowLayoutEngine {

    /**
     * Longest-path layering over `forward` edges only.
     *
     * Back edges are excluded from layering by construction: they are the
     * cycles, and including them would either not terminate or push a retry
     * target below the node that retries it. The in-progress guard is not
     * defensive decoration — a `forward` edge set that happens to contain a
     * cycle is a daemon fact this screen still has to draw.
     */
    fun layout(ast: WorkflowAst): GraphLayout {
        val order = ast.nodes.map { it.node }
        val declared = order.toSet()
        val incoming = mutableMapOf<String, MutableList<String>>()
        for (edge in ast.edges) {
            if (edge.kind != WorkflowEdgeKind.Forward) continue
            val from = edge.from ?: continue
            if (from !in declared || edge.to !in declared) continue
            incoming.getOrPut(edge.to) { mutableListOf() } += from
        }

        val depth = mutableMapOf<String, Int>()
        val inProgress = mutableSetOf<String>()

        fun layerOf(node: String): Int {
            depth[node]?.let { return it }
            if (!inProgress.add(node)) return 0
            val sources = incoming[node].orEmpty()
            val value = if (sources.isEmpty()) 0 else sources.maxOf { layerOf(it) + 1 }
            inProgress.remove(node)
            depth[node] = value
            return value
        }
        order.forEach { layerOf(it) }

        val byLayer = order.groupBy { depth[it] ?: 0 }
        val placements = order.map { node ->
            val layer = depth[node] ?: 0
            val siblings = byLayer[layer].orEmpty()
            GraphPlacement(
                node = node,
                layer = layer,
                column = siblings.indexOf(node).coerceAtLeast(0),
                columnsInLayer = siblings.size.coerceAtLeast(1),
            )
        }
        val index = placements.associateBy { it.node }

        val routes = ast.edges.map { edge ->
            val from = edge.from?.let { index[it] }
            val to = index[edge.to]
            val backward = edge.kind == WorkflowEdgeKind.Back ||
                (from != null && to != null && to.layer <= from.layer)
            GraphRoute(edge = edge, from = from, to = to, backward = backward)
        }

        return GraphLayout(
            placements = placements,
            routes = routes,
            layers = (depth.values.maxOrNull() ?: 0) + 1,
            widestLayer = byLayer.values.maxOfOrNull { it.size } ?: 0,
        )
    }
}

// ---------- the AST toggle ----------

/** One line of the indented activation-AST tree. */
data class AstLine(
    val depth: Int,
    val node: String,
    val inputType: String,
    val outputType: String,
    val join: WorkflowJoin,
    val convergenceGate: Boolean,
    /** Edge ids that reach this line's node, so the tree keeps the join facts. */
    val viaEdgeIds: List<Int>,
    /** True when this line repeats a node already printed higher up. */
    val repeat: Boolean,
)

object WorkflowAstTree {

    /**
     * The activation ast as an indented tree, walked along `forward` edges from
     * every root (a node with a `graph_input` edge, or with no forward parent).
     *
     * A node reached twice is printed again, flagged [AstLine.repeat], and not
     * descended into: a DAG is not a tree, and pretending otherwise would
     * either hide a join or loop forever. Back edges never expand the tree —
     * they are stated on the node they leave from.
     */
    fun lines(ast: WorkflowAst): List<AstLine> {
        val byName = ast.nodes.associateBy { it.node }
        val children = mutableMapOf<String, MutableList<WorkflowEdge>>()
        val hasForwardParent = mutableSetOf<String>()
        for (edge in ast.edges) {
            if (edge.kind != WorkflowEdgeKind.Forward) continue
            val from = edge.from ?: continue
            children.getOrPut(from) { mutableListOf() } += edge
            hasForwardParent += edge.to
        }
        val seeded = ast.edges
            .filter { it.kind == WorkflowEdgeKind.GraphInput }
            .map { it.to }
        val roots = ast.nodes
            .map { it.node }
            .filter { it in seeded || it !in hasForwardParent }
            .distinct()

        val out = mutableListOf<AstLine>()
        val printed = mutableSetOf<String>()

        fun emit(name: String, depth: Int, via: List<Int>) {
            val spec = byName[name] ?: return
            val repeat = !printed.add(name)
            out += AstLine(
                depth = depth,
                node = name,
                inputType = spec.inputType,
                outputType = spec.outputType,
                join = spec.join,
                convergenceGate = spec.convergenceGate,
                viaEdgeIds = via,
                repeat = repeat,
            )
            if (repeat) return
            children[name].orEmpty().forEach { edge -> emit(edge.to, depth + 1, listOf(edge.id)) }
        }

        roots.forEach { root ->
            val via = ast.edges.filter { it.kind == WorkflowEdgeKind.GraphInput && it.to == root }
            emit(root, 0, via.map { it.id })
        }
        // A node the walk never reached is still a declared node. It is listed
        // at the root depth rather than dropped, because an unreachable node is
        // a fact about the workflow, not noise.
        ast.nodes.map { it.node }.filterNot { it in printed }.forEach { emit(it, 0, emptyList()) }
        return out
    }
}
