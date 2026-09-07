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
