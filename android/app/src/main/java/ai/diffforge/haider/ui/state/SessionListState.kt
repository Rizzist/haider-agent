package ai.diffforge.haider.ui.state

import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState

/** The drawer's filter chips. */
enum class SessionFilter { All, Running, NeedsInput }

/** Attention groups, exactly as the TUI states the law (app.rs:11926-11972). */
enum class SessionGroupKind { NeedsYou, Active, Recent }

data class SessionGroup(val kind: SessionGroupKind, val rows: List<SessionRow>)

data class SessionListCounts(val all: Int, val running: Int, val needsInput: Int)

/**
 * Grouping and ordering for the session drawer. Pure, so the whole law is unit
 * tested before any UI consumes it.
 *
 * Order is *attention first*: tier 0 `needs_input`, tier 1 unseen/active, tier 2
 * the rest; then `last_activity_ms` desc; then title asc. A null
 * `last_activity_ms` sorts last — absent is unknown, never 0
 * (UI-SPEC 3.3, trap 6.6.5).
 */
object SessionListState {

    /**
     * The frozen order snapshot. Recomputing order under the user's thumb is
     * how a list steals a tap (UI-SPEC 3.3, trap 6.6.4), so the drawer captures
     * one of these on open / filter change / pull-to-refresh and reuses it
     * while it stays open.
     */
    data class OrderSnapshot(
        val ids: List<String>,
        /**
         * The group each row was rendered under. Freezing only the flat order
         * is not enough: a row that acquires `needs_input` while the drawer is
         * open would jump into NEEDS YOU and drag the tap target with it.
         */
        val groups: Map<String, SessionGroupKind> = emptyMap(),
    ) {
        private val index: Map<String, Int> = ids.withIndex().associate { (i, id) -> id to i }
        fun rank(id: String): Int? = index[id]
        fun group(id: String): SessionGroupKind? = groups[id]

        companion object {
            /** Captures the canonical order and the group each row sat in. */
            fun of(rows: List<SessionRow>, activeId: String? = null): OrderSnapshot = OrderSnapshot(
                ids = rows.map { it.id },
                groups = rows.associate { it.id to groupKind(it, activeId) },
            )

            fun capture(rows: List<SessionRow>, activeId: String?): OrderSnapshot =
                of(order(rows, activeId), activeId)
        }
    }

    fun counts(rows: List<SessionRow>): SessionListCounts = SessionListCounts(
        all = rows.size,
        running = rows.count { it.state == SessionVisualState.Running },
        needsInput = rows.count { it.needsInput != null || it.state == SessionVisualState.NeedsInput },
    )

    fun matches(row: SessionRow, filter: SessionFilter): Boolean = when (filter) {
        SessionFilter.All -> true
        SessionFilter.Running -> row.state == SessionVisualState.Running
        SessionFilter.NeedsInput ->
            row.needsInput != null || row.state == SessionVisualState.NeedsInput
    }

    /** Case-insensitive match over title, model, provider and id. */
    fun matches(row: SessionRow, query: String): Boolean {
        val needle = query.trim().lowercase()
        if (needle.isEmpty()) return true
        return listOfNotNull(row.title, row.model, row.provider, row.agentType, row.id)
            .any { it.lowercase().contains(needle) }
    }

    fun tier(row: SessionRow, activeId: String?): Int = when {
        row.needsInput != null || row.state == SessionVisualState.NeedsInput -> 0
        row.unseen && row.id != activeId -> 1
        row.runId != null -> 1
        else -> 2
    }

    /**
     * The group **is** the attention tier. Deriving it from `run_id` instead
     * put an unseen row (tier 1) in RECENT while a running row (also tier 1)
     * went to ACTIVE, so grouping re-sorted what [order] had already decided:
     * canonical `[a, b, c]` rendered as `[a, c, b]`.
     */
    fun groupKind(row: SessionRow, activeId: String? = null): SessionGroupKind =
        when (tier(row, activeId)) {
            0 -> SessionGroupKind.NeedsYou
            1 -> SessionGroupKind.Active
            else -> SessionGroupKind.Recent
        }

    /** The canonical comparator: tier, then activity desc, then title asc. */
    fun order(rows: List<SessionRow>, activeId: String?): List<SessionRow> =
        rows.sortedWith(
            compareBy<SessionRow> { tier(it, activeId) }
                // Long.MIN_VALUE for an unknown stamp: it sorts last, not as 0.
                .thenByDescending { it.lastActivityMs ?: Long.MIN_VALUE }
                .thenBy { (it.title ?: it.id).lowercase() },
        )

    /**
     * The drawer's list. When [snapshot] is present, rows it knows keep their
     * captured position and rows it has never seen are appended in canonical
     * order, so a roster delta never re-sorts under the user's thumb.
     */
    /**
     * The drawer's list.
     *
     * Group headers appear in the order their first member appears in the
     * canonical order, so grouping *presents* the attention order instead of
     * overriding it: flattening the returned groups reproduces [order] exactly.
     *
     * While [snapshot] is present, a row it knows keeps both its position and
     * the group it was rendered under, and rows it has never seen are appended
     * in canonical order — so nothing moves under the user's thumb.
     */
    fun groups(
        rows: List<SessionRow>,
        activeId: String?,
        filter: SessionFilter = SessionFilter.All,
        query: String = "",
        snapshot: OrderSnapshot? = null,
        extraIds: Set<String> = emptySet(),
    ): List<SessionGroup> {
        val visible = rows.filter {
            matches(it, filter) && (matches(it, query) || it.id in extraIds)
        }
        val ordered = if (snapshot == null) {
            order(visible, activeId)
        } else {
            val known = visible.filter { snapshot.rank(it.id) != null }
                .sortedBy { snapshot.rank(it.id) }
            val fresh = order(visible.filter { snapshot.rank(it.id) == null }, activeId)
            known + fresh
        }
        val kindOf: (SessionRow) -> SessionGroupKind = { row ->
            snapshot?.group(row.id) ?: groupKind(row, activeId)
        }
        // Sections are contiguous runs of the flat order, never buckets that
        // pull rows out of it. Flattening the result always reproduces
        // `ordered` exactly — asserted in SessionListStateTest — so a header
        // can appear twice rather than a row moving, which is the trade the
        // ordering law (rule 6) demands.
        val out = mutableListOf<SessionGroup>()
        var current = mutableListOf<SessionRow>()
        var currentKind: SessionGroupKind? = null
        ordered.forEach { row ->
            val kind = kindOf(row)
            if (kind != currentKind) {
                if (current.isNotEmpty()) out += SessionGroup(currentKind!!, current)
                current = mutableListOf()
                currentKind = kind
            }
            current += row
        }
        if (current.isNotEmpty() && currentKind != null) out += SessionGroup(currentKind!!, current)
        return out
    }

    /** The invariant the drawer depends on, exposed so tests can state it. */
    fun flatten(groups: List<SessionGroup>): List<String> =
        groups.flatMap { group -> group.rows.map { it.id } }

    /** The flat ordered list, for the start surface's "recent sessions" block. */
    fun recent(rows: List<SessionRow>, activeId: String?, limit: Int): List<SessionRow> =
        order(rows, activeId).take(limit)
}
