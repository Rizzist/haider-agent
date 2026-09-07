package ai.diffforge.haider.state

import ai.diffforge.haider.ui.daemon.NeedsInput
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.state.SessionFilter
import ai.diffforge.haider.ui.state.SessionGroupKind
import ai.diffforge.haider.ui.state.SessionListState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionListStateTest {
    private val now = 1_772_000_000_000L

    private fun row(
        id: String,
        title: String? = id,
        state: SessionVisualState = SessionVisualState.Idle,
        lastActivityMs: Long? = now,
        seenAtMs: Long? = now,
        runId: String? = null,
        needsInput: NeedsInput? = null,
    ) = SessionRow(
        id = id,
        title = title,
        state = state,
        lastActivityMs = lastActivityMs,
        seenAtMs = seenAtMs,
        runId = runId,
        needsInput = needsInput,
    )

    private val asking = NeedsInput(kind = "approval", title = "Send it?")

    @Test
    fun `needs input outranks everything else`() {
        val rows = listOf(
            row("idle", lastActivityMs = now),
            row("running", state = SessionVisualState.Running, runId = "r", lastActivityMs = now),
            row(
                "asking",
                state = SessionVisualState.NeedsInput,
                needsInput = asking,
                lastActivityMs = now - 10 * 60_000,
            ),
        )
        assertEquals("asking", SessionListState.order(rows, null).first().id)
    }

    @Test
    fun `groups follow attention and empty groups are hidden`() {
        val rows = listOf(
            row("a", state = SessionVisualState.NeedsInput, needsInput = asking),
            row("b", state = SessionVisualState.Running, runId = "r"),
            row("c"),
        )
        val kinds = SessionListState.groups(rows, null).map { it.kind }
        assertEquals(
            listOf(SessionGroupKind.NeedsYou, SessionGroupKind.Active, SessionGroupKind.Recent),
            kinds,
        )
        val onlyIdle = SessionListState.groups(listOf(row("c")), null).map { it.kind }
        assertEquals(listOf(SessionGroupKind.Recent), onlyIdle)
    }

    @Test
    fun `activity descends and ties break on title ascending`() {
        val rows = listOf(
            row("z", title = "zebra", lastActivityMs = now - 1000),
            row("a", title = "alpha", lastActivityMs = now - 1000),
            row("m", title = "middle", lastActivityMs = now),
        )
        assertEquals(listOf("m", "a", "z"), SessionListState.order(rows, null).map { it.id })
    }

    @Test
    fun `an unknown last activity sorts last, never as zero`() {
        val rows = listOf(
            row("unknown", lastActivityMs = null, seenAtMs = null),
            row("ancient", lastActivityMs = 1L),
            row("fresh", lastActivityMs = now),
        )
        assertEquals(
            listOf("fresh", "ancient", "unknown"),
            SessionListState.order(rows, null).map { it.id },
        )
    }

    @Test
    fun `order is frozen while the drawer is open`() {
        val before = listOf(
            row("a", lastActivityMs = now - 3000),
            row("b", lastActivityMs = now - 2000),
            row("c", lastActivityMs = now - 1000),
        )
        val snapshot = SessionListState.OrderSnapshot.of(SessionListState.order(before, null))
        // A roster delta makes `a` the most recent; the visible order must not move.
        val after = before.map { if (it.id == "a") it.copy(lastActivityMs = now) else it }
        val frozen = SessionListState.groups(after, null, snapshot = snapshot)
            .single().rows.map { it.id }
        assertEquals(listOf("c", "b", "a"), frozen)

        // Re-sorting only happens when a new snapshot is taken.
        val thawed = SessionListState.groups(after, null).single().rows.map { it.id }
        assertEquals(listOf("a", "c", "b"), thawed)
    }

    @Test
    fun `a row the snapshot has never seen is appended, not dropped`() {
        val before = listOf(row("a", lastActivityMs = now - 3000))
        val snapshot = SessionListState.OrderSnapshot.of(before)
        val after = before + row("new", lastActivityMs = now)
        val ids = SessionListState.groups(after, null, snapshot = snapshot).single().rows.map { it.id }
        assertEquals(listOf("a", "new"), ids)
    }

    @Test
    fun `filters and counts agree`() {
        val rows = listOf(
            row("a", state = SessionVisualState.NeedsInput, needsInput = asking),
            row("b", state = SessionVisualState.Running, runId = "r"),
            row("c"),
        )
        val counts = SessionListState.counts(rows)
        assertEquals(3, counts.all)
        assertEquals(1, counts.running)
        assertEquals(1, counts.needsInput)
        assertEquals(
            listOf("b"),
            SessionListState.groups(rows, null, SessionFilter.Running).flatMap { it.rows }.map { it.id },
        )
    }

    @Test
    fun `search matches title, model, provider and id`() {
        val rows = listOf(
            row("s-1", title = "Fix nav crash").copy(model = "claude-sonnet-4-5", provider = "anthropic"),
            row("s-2", title = "Summarise texts").copy(model = "claude-opus-4-1", provider = "anthropic"),
        )
        assertTrue(SessionListState.matches(rows[0], "NAV"))
        assertTrue(SessionListState.matches(rows[1], "opus"))
        assertTrue(SessionListState.matches(rows[0], "s-1"))
        assertTrue(SessionListState.matches(rows[0], ""))
        assertTrue(!SessionListState.matches(rows[0], "zzz"))
    }

    @Test
    fun `interleaved unseen and running rows keep the canonical order`() {
        // The device pass found canonical [a,b,c] rendering as [a,c,b] when an
        // unseen row sat between two running ones: the group was derived from
        // run_id rather than from the attention tier, so grouping re-sorted.
        val rows = listOf(
            row("a", title = "A", runId = "run-a", lastActivityMs = 300, seenAtMs = null),
            row("b", title = "B", lastActivityMs = 200, seenAtMs = 0),
            row("c", title = "C", runId = "run-c", lastActivityMs = 100, seenAtMs = null),
        )
        assertEquals(listOf("a", "b", "c"), SessionListState.order(rows, null).map { it.id })
        assertEquals(
            listOf("a", "b", "c"),
            SessionListState.flatten(SessionListState.groups(rows, null)),
        )
    }

    @Test
    fun `a new row is appended, never inserted above a frozen one`() {
        val rows = listOf(
            row("a", runId = "r", lastActivityMs = 300, seenAtMs = null),
            row("b", lastActivityMs = 200, seenAtMs = 200),
        )
        val snapshot = SessionListState.OrderSnapshot.capture(rows, null)
        // A brand-new Active row would sort first by activity; while the drawer
        // is open it goes to the end instead.
        val fresh = row("c", runId = "r2", lastActivityMs = 400, seenAtMs = null)
        assertEquals(
            listOf("a", "b", "c"),
            SessionListState.flatten(
                SessionListState.groups(rows + fresh, null, snapshot = snapshot),
            ),
        )
    }

    @Test
    fun `flattening the sections always reproduces the order, whatever the mix`() {
        // The invariant, stated once: sections are contiguous runs of the flat
        // order, so a header may repeat but a row never moves.
        val rows = (0 until 24).map { index ->
            row(
                id = "s-%02d".format(index),
                lastActivityMs = now - index * 1_000L,
                seenAtMs = if (index % 3 == 0) 0L else now,
                runId = if (index % 2 == 0) "run-$index" else null,
                needsInput = if (index % 7 == 0) asking else null,
            )
        }
        assertEquals(
            SessionListState.order(rows, null).map { it.id },
            SessionListState.flatten(SessionListState.groups(rows, null)),
        )
    }

    @Test
    fun `grouping presents the attention order instead of overriding it`() {
        // Round 1 emitted groups in enum order, so an ACTIVE row could be
        // drawn above a more recent RECENT row: the grouping silently re-sorted
        // what `order` had already decided.
        val unseen = row("unseen", lastActivityMs = 100, seenAtMs = 0)
        val active = row("active", lastActivityMs = 50, seenAtMs = 50, runId = "r")
        val rows = listOf(unseen, active)
        assertEquals(
            SessionListState.order(rows, null).map { it.id },
            SessionListState.groups(rows, null).flatMap { it.rows }.map { it.id },
        )
    }

    @Test
    fun `flattening the groups always reproduces the canonical order`() {
        val rows = listOf(
            row("a", state = SessionVisualState.NeedsInput, needsInput = asking, lastActivityMs = now - 9_000),
            row("b", state = SessionVisualState.Running, runId = "r", lastActivityMs = now),
            row("c", lastActivityMs = now - 1_000),
            row("d", lastActivityMs = null, seenAtMs = null),
        )
        assertEquals(
            SessionListState.order(rows, null).map { it.id },
            SessionListState.groups(rows, null).flatMap { it.rows }.map { it.id },
        )
    }

    @Test
    fun `a frozen row does not move when its group changes underneath it`() {
        val before = listOf(
            row("a", lastActivityMs = 20, seenAtMs = 20),
            row("b", lastActivityMs = 10, seenAtMs = 10),
        )
        val snapshot = SessionListState.OrderSnapshot.capture(before, null)
        // `b` starts asking for a human, which would promote it to NEEDS YOU.
        val after = before.map { if (it.id == "b") it.copy(needsInput = asking) else it }
        assertEquals(
            listOf("a", "b"),
            SessionListState.groups(after, null, snapshot = snapshot).flatMap { it.rows }.map { it.id },
        )
        // And it keeps the group header it was rendered under, so the row does
        // not jump out from beneath the user's thumb.
        assertEquals(
            listOf(SessionGroupKind.Recent),
            SessionListState.groups(after, null, snapshot = snapshot).map { it.kind },
        )
    }

    @Test
    fun `the freeze lifts when a new snapshot is captured`() {
        val before = listOf(
            row("a", lastActivityMs = 20, seenAtMs = 20),
            row("b", lastActivityMs = 10, seenAtMs = 10),
        )
        val after = before.map { if (it.id == "b") it.copy(needsInput = asking) else it }
        val reopened = SessionListState.OrderSnapshot.capture(after, null)
        assertEquals(
            listOf("b", "a"),
            SessionListState.groups(after, null, snapshot = reopened).flatMap { it.rows }.map { it.id },
        )
    }

    @Test
    fun `transcript hits survive the metadata filter`() {
        val rows = listOf(
            row("s-1", title = "Nothing matching"),
            row("s-2", title = "Also nothing"),
        )
        // The query matches no metadata, but the repository found it in a
        // transcript, so the row must still be listed.
        val listed = SessionListState.groups(
            rows = rows,
            activeId = null,
            query = "back stack invariants",
            extraIds = setOf("s-2"),
        ).flatMap { it.rows }.map { it.id }
        assertEquals(listOf("s-2"), listed)
    }

    @Test
    fun `hundreds of sessions stay ordered without collapsing groups`() {
        val rows = (0 until 300).map { index ->
            row(
                id = "s-%03d".format(index),
                lastActivityMs = now - index * 1000L,
                state = if (index % 7 == 0) SessionVisualState.Running else SessionVisualState.Idle,
                runId = if (index % 7 == 0) "run-$index" else null,
            )
        }
        val groups = SessionListState.groups(rows, null)
        assertEquals(listOf(SessionGroupKind.Active, SessionGroupKind.Recent), groups.map { it.kind })
        assertEquals(300, groups.sumOf { it.rows.size })
    }
}
