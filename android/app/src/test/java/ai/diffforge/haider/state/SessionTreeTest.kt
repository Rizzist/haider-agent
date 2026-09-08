package ai.diffforge.haider.state

import ai.diffforge.haider.ui.daemon.NeedsInput
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.state.SessionGroupKind
import ai.diffforge.haider.ui.state.SessionListState
import ai.diffforge.haider.ui.state.SessionTree
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The drawer's delegation law, pinned before any composable reads it.
 *
 * The pin that matters most is the first one: with no `parent_session_id` in
 * the roster this must be [SessionListState.groups] exactly — same rows, same
 * order, same section kinds. Every existing drawer golden and every existing
 * ordering test depends on that, and a nesting pass that quietly re-sorted an
 * undelegated roster would be a regression nothing else would catch.
 */
class SessionTreeTest {

    private fun row(
        id: String,
        parent: String? = null,
        state: SessionVisualState = SessionVisualState.Idle,
        activityMs: Long? = 1_000,
        runId: String? = null,
        needsInput: Boolean = false,
        title: String? = id,
    ) = SessionRow(
        id = id,
        title = title,
        state = state,
        lastActivityMs = activityMs,
        seenAtMs = activityMs,
        runId = runId,
        parentSessionId = parent,
        needsInput = if (needsInput) {
            NeedsInput(kind = "question", title = "?", menuId = "m-$id", requestSeq = 1, workerGeneration = 1)
        } else {
            null
        },
    )

    // ---------- the equivalence that protects everything already shipped ----------

    @Test
    fun `a roster with no delegation is exactly the flat grouping`() {
        val rows = listOf(
            row("a", state = SessionVisualState.Running, runId = "r", activityMs = 9_000),
            row("b", needsInput = true, state = SessionVisualState.NeedsInput, activityMs = 5_000),
            row("c", activityMs = 1_000),
            row("d", activityMs = null),
        )
        val flat = SessionListState.groups(rows, activeId = "a")
        val sections = SessionTree.sections(rows, activeId = "a")
        assertEquals(SessionListState.flatten(flat), SessionTree.flatten(sections))
        assertEquals(flat.map { it.kind }, sections.map { it.kind })
    }

    @Test
    fun `an undelegated capture matches the flat capture`() {
        val rows = listOf(
            row("a", state = SessionVisualState.Running, runId = "r", activityMs = 9_000),
            row("b", activityMs = 5_000),
        )
        assertEquals(
            SessionListState.OrderSnapshot.capture(rows, "a").ids,
            SessionTree.captureOrder(rows, "a").ids,
        )
    }

    // ---------- nesting ----------

    @Test
    fun `a child renders under its parent, in tree order`() {
        val rows = listOf(
            row("p", state = SessionVisualState.Running, runId = "r", activityMs = 9_000),
            row("c1", parent = "p", activityMs = 8_000),
            row("c2", parent = "p", activityMs = 7_000),
            row("g", parent = "c1", activityMs = 6_000),
        )
        val sections = SessionTree.sections(rows, activeId = null)
        assertEquals(listOf("p", "c1", "g", "c2"), SessionTree.flatten(sections))
    }

    @Test
    fun `an orphan child stays a root rather than disappearing`() {
        // The parent is not in the visible roster — paged out, filtered out, or
        // simply never listed. Hiding the child under a row that is not there
        // would remove it from the drawer entirely.
        val rows = listOf(row("c", parent = "missing"))
        val sections = SessionTree.sections(rows, activeId = null)
        assertEquals(listOf("c"), SessionTree.flatten(sections))
        assertEquals(0, sections.single().roots.single().depth)
    }

    @Test
    fun `a self-parented row is a root, not an infinite tree`() {
        val rows = listOf(row("a", parent = "a"))
        assertEquals(listOf("a"), SessionTree.flatten(SessionTree.sections(rows, null)))
    }

    @Test
    fun `a cycle is broken instead of recursing forever`() {
        val rows = listOf(row("a", parent = "b"), row("b", parent = "a"))
        val flattened = SessionTree.flatten(SessionTree.sections(rows, null))
        assertEquals(setOf("a", "b"), flattened.toSet())
        assertEquals(2, flattened.size)
    }

    // ---------- attention ----------

    @Test
    fun `a family whose child needs a human sorts into needs-you as one run`() {
        val rows = listOf(
            row("old", activityMs = 100),
            row("p", state = SessionVisualState.Running, runId = "r", activityMs = 9_000),
            row("c", parent = "p", needsInput = true, state = SessionVisualState.NeedsInput, activityMs = 8_000),
        )
        val sections = SessionTree.sections(rows, activeId = null)
        assertEquals(SessionGroupKind.NeedsYou, sections.first().kind)
        assertEquals(listOf("p", "c"), SessionTree.flatten(sections).take(2))
    }

    @Test
    fun `the aggregate dot is the strongest descendant state`() {
        val node = SessionTree.nest(
            listOf(
                row("p", state = SessionVisualState.Running),
                row("c1", parent = "p", state = SessionVisualState.Idle),
                row("c2", parent = "p", state = SessionVisualState.NeedsInput, needsInput = true),
                row("c3", parent = "p", state = SessionVisualState.Errored),
            ),
        ).single()
        val summary = SessionTree.summarise(node)!!
        assertEquals(SessionVisualState.NeedsInput, summary.aggregate)
        assertEquals(3, summary.directChildren)
        assertEquals(3, summary.descendants)
        assertEquals(1, summary.needsInput)
    }

    @Test
    fun `the count covers every descendant, not just the direct children`() {
        val node = SessionTree.nest(
            listOf(
                row("p"),
                row("c", parent = "p"),
                row("g", parent = "c"),
            ),
        ).single()
        val summary = SessionTree.summarise(node)!!
        assertEquals(1, summary.directChildren)
        assertEquals(2, summary.descendants)
    }

    @Test
    fun `a childless row has no summary and therefore no dot`() {
        val node = SessionTree.nest(listOf(row("a"))).single()
        assertNull(SessionTree.summarise(node))
    }

    // ---------- collapse ----------

    @Test
    fun `a small family arrives open and a large one arrives folded`() {
        assertTrue(SessionTree.defaultExpanded(SessionTree.COLLAPSE_THRESHOLD))
        assertFalse(SessionTree.defaultExpanded(SessionTree.COLLAPSE_THRESHOLD + 1))
    }

    @Test
    fun `a folded family hides its children and keeps its own row`() {
        val rows = listOf(row("p")) + (1..4).map { row("c$it", parent = "p") }
        val roots = SessionTree.sections(rows, activeId = null).single().roots
        val lines = SessionTree.lines(roots, toggled = emptySet())
        assertEquals(listOf("p"), lines.map { it.row.id })
        assertEquals(4, lines.single().summary!!.descendants)
        assertFalse(lines.single().expanded)
        // The toggle is a flip away from the default, so unfolding shows them.
        val opened = SessionTree.lines(roots, toggled = setOf("p"))
        assertEquals(5, opened.size)
        assertTrue(opened.first().expanded)
    }

    @Test
    fun `the last child is marked so its connector can stop`() {
        val rows = listOf(row("p"), row("c1", parent = "p"), row("c2", parent = "p"))
        val roots = SessionTree.sections(rows, activeId = null).single().roots
        val lines = SessionTree.lines(roots, toggled = emptySet())
        assertEquals(listOf(false, false, true), lines.map { it.lastChild })
        assertEquals(listOf(0, 1, 1), lines.map { it.depth })
    }

    // ---------- the freeze ----------

    @Test
    fun `a frozen family keeps its captured position when a child changes state`() {
        val before = listOf(
            row("p", state = SessionVisualState.Running, runId = "r", activityMs = 9_000),
            row("c", parent = "p", activityMs = 8_000),
            row("other", needsInput = true, state = SessionVisualState.NeedsInput, activityMs = 1_000),
        )
        val snapshot = SessionTree.captureOrder(before, activeId = null)
        assertEquals(listOf("other", "p", "c"), snapshot.ids)

        // The child then parks on a human. Its family is now tier 0 too, and
        // its 9 s activity beats `other`'s — so an unfrozen list would swap the
        // two runs under the user's thumb.
        val after = before.map {
            if (it.id == "c") {
                it.copy(
                    state = SessionVisualState.NeedsInput,
                    needsInput = NeedsInput(
                        kind = "question",
                        title = "?",
                        menuId = "m",
                        requestSeq = 1,
                        workerGeneration = 1,
                    ),
                )
            } else {
                it
            }
        }
        assertEquals(
            listOf("p", "c", "other"),
            SessionTree.flatten(SessionTree.sections(after, activeId = null)),
        )
        assertEquals(
            listOf("other", "p", "c"),
            SessionTree.flatten(SessionTree.sections(after, activeId = null, snapshot = snapshot)),
        )
    }

    @Test
    fun `a family the freeze has never seen is appended in canonical order`() {
        val known = listOf(row("p", activityMs = 5_000))
        val snapshot = SessionTree.captureOrder(known, activeId = null)
        val grown = known + listOf(row("q", activityMs = 9_000), row("qc", parent = "q", activityMs = 8_000))
        val sections = SessionTree.sections(grown, activeId = null, snapshot = snapshot)
        assertEquals(listOf("p", "q", "qc"), SessionTree.flatten(sections))
    }

    @Test
    fun `a captured family carries one group kind for every member`() {
        val rows = listOf(
            row("p", state = SessionVisualState.Running, runId = "r", activityMs = 9_000),
            row("c", parent = "p", needsInput = true, state = SessionVisualState.NeedsInput, activityMs = 8_000),
        )
        val snapshot = SessionTree.captureOrder(rows, activeId = null)
        assertEquals(SessionGroupKind.NeedsYou, snapshot.group("p"))
        assertEquals(SessionGroupKind.NeedsYou, snapshot.group("c"))
    }

    // ---------- filtering ----------

    @Test
    fun `a search that matches only the child promotes the child to a root`() {
        // The parent is filtered out, so the child cannot nest under it; it has
        // to remain visible rather than vanish with its parent.
        val rows = listOf(row("parent", title = "unrelated"), row("kid", parent = "parent", title = "needle"))
        val sections = SessionTree.sections(rows, activeId = null, query = "needle")
        assertEquals(listOf("kid"), SessionTree.flatten(sections))
    }
}
