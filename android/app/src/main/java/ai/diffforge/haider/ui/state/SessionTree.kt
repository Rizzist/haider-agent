package ai.diffforge.haider.ui.state

import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState

/**
 * The drawer's list once delegation is visible: a child session renders under
 * the parent that spawned it, not as a sibling of it.
 *
 * The lineage fact is `SessionSummary.parent_session_id` (frame.rs:1949) — the
 * daemon's own edge, reduced from the durable delegation record. Nothing here
 * infers a parent from a title, an id prefix or a timestamp: a row whose parent
 * is absent from the visible list stays at the top level, where it can be seen,
 * rather than being hidden under a row that is not there.
 *
 * Two laws from [SessionListState] survive intact:
 *
 * 1. **Attention first.** A family is ranked by its *strongest* member: if a
 *    descendant is parked on a human, the whole family sorts into NEEDS YOU.
 *    Nesting must never bury a child that is asking for something.
 * 2. **Nothing moves under a thumb.** With a frozen [SessionListState.OrderSnapshot]
 *    the rendered position of every known row is the captured one, and rows the
 *    freeze has never seen are appended in canonical order.
 *
 * With no `parent_session_id` anywhere in the roster this produces exactly what
 * [SessionListState.groups] produces — same rows, same order, same section
 * kinds. That equivalence is pinned in `SessionTreeTest`, because it is what
 * keeps every existing drawer golden honest.
 */
object SessionTree {

    /**
     * Above this many children a family arrives collapsed.
     *
     * A parent with two or three children is still a readable list; a fan-out
     * of ten pushes every other session off the screen, so the default flips
     * and the parent's count says what is hidden.
     */
    const val COLLAPSE_THRESHOLD = 3

    /** One session and the sessions it spawned, recursively. */
    data class Node(
        val row: SessionRow,
        val depth: Int,
        val children: List<Node> = emptyList(),
    ) {
        /** This node and everything under it, in render order. */
        val family: List<Node>
            get() = listOf(this) + children.flatMap { it.family }
    }

    /**
     * What a parent row says about the sessions under it.
     *
     * [aggregate] is null for a childless row: no children is not a state, and
     * a dot that means nothing is worse than no dot.
     */
    data class Summary(
        val directChildren: Int,
        val descendants: Int,
        val aggregate: SessionVisualState?,
        val needsInput: Int,
        val running: Int,
    )

    /** One contiguous run of the ordered roots under a single header. */
    data class Section(val kind: SessionGroupKind, val roots: List<Node>)

    /** One rendered drawer line, after collapse has been applied. */
    data class Line(
        val row: SessionRow,
        val depth: Int,
        /** Non-null exactly when this row has children. */
        val summary: Summary?,
        /** Meaningless without a [summary]; false for a leaf. */
        val expanded: Boolean,
        /** The last child of its parent — the connector's elbow, not a tee. */
        val lastChild: Boolean,
    )

    // ---------- nesting ----------

    /**
     * Nest [rows] by `parent_session_id`, preserving the order of [rows] both
     * among the roots and among each parent's children.
     *
     * A row whose parent is not in [rows] is a root — an orphan is shown, never
     * dropped. A cycle (a row that is its own ancestor, which durable truth
     * should never produce) is broken by promoting its first member to a root
     * rather than recursing forever.
     */
    fun nest(rows: List<SessionRow>): List<Node> {
        val byId = rows.associateBy { it.id }
        val childrenOf = rows
            .filter { it.parentSessionId != null && it.parentSessionId != it.id }
            .filter { byId.containsKey(it.parentSessionId) }
            .groupBy { it.parentSessionId!! }
        val attached = mutableSetOf<String>()

        fun build(row: SessionRow, depth: Int, ancestry: Set<String>): Node {
            val guarded = ancestry + row.id
            val children = childrenOf[row.id].orEmpty()
                .filterNot { it.id in ancestry }
                .map { child ->
                    attached += child.id
                    build(child, depth + 1, guarded)
                }
            return Node(row, depth, children)
        }

        val roots = mutableListOf<Node>()
        rows.forEach { row ->
            val parent = row.parentSessionId
            val isRoot = parent == null || parent == row.id || !byId.containsKey(parent)
            if (isRoot) roots += build(row, 0, emptySet())
        }
        // Anything a cycle kept out of the forest still has to be reachable.
        rows.forEach { row ->
            if (row.id !in attached && roots.none { it.row.id == row.id }) {
                roots += build(row, 0, emptySet())
            }
        }
        return roots
    }

    /** The parent row's count and aggregate dot, over every descendant. */
    fun summarise(node: Node): Summary? {
        val descendants = node.family.drop(1)
        if (descendants.isEmpty()) return null
        val states = descendants.map { it.row.state }
        return Summary(
            directChildren = node.children.size,
            descendants = descendants.size,
            aggregate = ATTENTION_ORDER.firstOrNull { it in states } ?: states.first(),
            needsInput = descendants.count {
                it.row.needsInput != null || it.row.state == SessionVisualState.NeedsInput
            },
            running = descendants.count { it.row.state == SessionVisualState.Running },
        )
    }

    /** Attention first, and it is the same order the drawer sorts by. */
    private val ATTENTION_ORDER = listOf(
        SessionVisualState.NeedsInput,
        SessionVisualState.Errored,
        SessionVisualState.Running,
        SessionVisualState.WaitingForNetwork,
        SessionVisualState.Idle,
        SessionVisualState.Unknown,
    )

    // ---------- ordering ----------

    /** A family's tier is its strongest member's: nesting never buries need. */
    fun familyTier(node: Node, activeId: String?): Int =
        node.family.minOf { SessionListState.tier(it.row, activeId) }

    fun familyGroupKind(node: Node, activeId: String?): SessionGroupKind =
        when (familyTier(node, activeId)) {
            0 -> SessionGroupKind.NeedsYou
            1 -> SessionGroupKind.Active
            else -> SessionGroupKind.Recent
        }

    /**
     * The canonical family comparator: tier, then the family's most recent
     * activity, then the root's title. A family whose every member has an
     * unknown stamp sorts last, exactly as an unknown single row does — absent
     * is unknown, never zero.
     */
    fun orderRoots(nodes: List<Node>, activeId: String?): List<Node> =
        nodes.sortedWith(
            compareBy<Node> { familyTier(it, activeId) }
                .thenByDescending { node ->
                    node.family.mapNotNull { it.row.lastActivityMs }.maxOrNull() ?: Long.MIN_VALUE
                }
                .thenBy { (it.row.title ?: it.row.id).lowercase() },
        )

    /** Children obey the same law as roots, one level down. */
    private fun orderTree(nodes: List<Node>, activeId: String?): List<Node> =
        orderRoots(nodes, activeId).map { node ->
            node.copy(children = orderTree(node.children, activeId))
        }

    /**
     * The freeze, family-aware.
     *
     * [SessionListState.OrderSnapshot.capture] ranks a flat list, so a family
     * captured through it would be pinned in the section its *root* alone
     * belongs to and would then jump the moment the drawer rendered it under
     * the family's tier. This captures what the drawer actually draws: the
     * flattened render order, with every member of a family carrying the
     * family's own group kind so a section stays contiguous.
     *
     * [SessionListState.OrderSnapshot.extend] stays flat, deliberately: a row
     * that appears *while* the drawer is open is ranked once, on arrival, and a
     * child arriving after its parent was frozen is appended rather than
     * inserted. It shows under its own header until the drawer closes and the
     * next open re-captures — which is the same trade the ordering law already
     * makes, and it is the one that does not move a row under a thumb.
     */
    fun captureOrder(
        rows: List<SessionRow>,
        activeId: String?,
    ): SessionListState.OrderSnapshot {
        val roots = orderTree(nest(rows), activeId)
        val ids = mutableListOf<String>()
        val groups = mutableMapOf<String, SessionGroupKind>()
        orderRoots(roots, activeId).forEach { root ->
            val kind = familyGroupKind(root, activeId)
            root.family.forEach { member ->
                ids += member.row.id
                groups[member.row.id] = kind
            }
        }
        return SessionListState.OrderSnapshot(ids = ids, groups = groups)
    }

    /**
     * The drawer's sections.
     *
     * Sections are contiguous runs of the ordered roots, never buckets that
     * pull a family out of the order — the same trade [SessionListState.groups]
     * makes, so a header may appear twice rather than a row moving.
     */
    fun sections(
        rows: List<SessionRow>,
        activeId: String?,
        filter: SessionFilter = SessionFilter.All,
        query: String = "",
        snapshot: SessionListState.OrderSnapshot? = null,
        extraIds: Set<String> = emptySet(),
    ): List<Section> {
        val visible = rows.filter {
            SessionListState.matches(it, filter) &&
                (SessionListState.matches(it, query) || it.id in extraIds)
        }
        val nested = nest(visible)
        val roots = if (snapshot == null) {
            orderTree(nested, activeId)
        } else {
            freeze(nested, snapshot, activeId)
        }
        val out = mutableListOf<Section>()
        var current = mutableListOf<Node>()
        var currentKind: SessionGroupKind? = null
        roots.forEach { root ->
            val kind = snapshot?.group(root.row.id) ?: familyGroupKind(root, activeId)
            if (kind != currentKind) {
                if (current.isNotEmpty()) out += Section(currentKind!!, current)
                current = mutableListOf()
                currentKind = kind
            }
            current += root
        }
        if (current.isNotEmpty() && currentKind != null) out += Section(currentKind!!, current)
        return out
    }

    /** Known rows keep their captured position; the rest append canonically. */
    private fun freeze(
        nodes: List<Node>,
        snapshot: SessionListState.OrderSnapshot,
        activeId: String?,
    ): List<Node> {
        fun apply(list: List<Node>): List<Node> {
            val known = list.filter { snapshot.rank(it.row.id) != null }
                .sortedBy { snapshot.rank(it.row.id) }
            val fresh = orderRoots(list.filter { snapshot.rank(it.row.id) == null }, activeId)
            return (known + fresh).map { it.copy(children = apply(it.children)) }
        }
        return apply(nodes)
    }

    // ---------- collapse ----------

    /** A small family arrives open; a large one arrives collapsed. */
    fun defaultExpanded(directChildren: Int): Boolean = directChildren <= COLLAPSE_THRESHOLD

    /**
     * [toggled] holds the ids the user has flipped *away from* the default, so
     * the default can change with the child count without stranding a choice on
     * a stale boolean.
     */
    fun expanded(node: Node, toggled: Set<String>): Boolean =
        defaultExpanded(node.children.size) != (node.row.id in toggled)

    /** The rendered lines for one section, after collapse. */
    fun lines(roots: List<Node>, toggled: Set<String>): List<Line> {
        val out = mutableListOf<Line>()
        fun walk(node: Node, lastChild: Boolean) {
            val summary = summarise(node)
            val open = summary != null && expanded(node, toggled)
            out += Line(
                row = node.row,
                depth = node.depth,
                summary = summary,
                expanded = open,
                lastChild = lastChild,
            )
            if (open) {
                node.children.forEachIndexed { index, child ->
                    walk(child, lastChild = index == node.children.lastIndex)
                }
            }
        }
        roots.forEach { walk(it, lastChild = false) }
        return out
    }

    /** The invariant the drawer depends on, exposed so tests can state it. */
    fun flatten(sections: List<Section>): List<String> =
        sections.flatMap { section -> section.roots.flatMap { root -> root.family.map { it.row.id } } }
}
