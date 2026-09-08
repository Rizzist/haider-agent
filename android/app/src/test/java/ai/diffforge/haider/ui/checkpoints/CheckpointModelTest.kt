package ai.diffforge.haider.ui.checkpoints

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The pure checkpoint views: ordering, turn grouping, paging merge and the
 * places where an unpublished fact must stay unpublished.
 *
 * These are the rules the sheet cannot express in a rendered assertion. Two
 * rows in the wrong order still both render; a merge that drops a row still
 * renders a list.
 */
class CheckpointModelTest {

    private fun row(
        id: String?,
        seq: Long?,
        run: String? = "run-1",
        kind: String? = "edit",
        origin: String? = "tool",
    ) = CheckpointView(
        checkpointId = id,
        sessionId = "s",
        branchId = null,
        runId = run,
        effectId = null,
        callId = null,
        seq = seq,
        workspaceRevision = null,
        kind = CheckpointCategory.of(kind, CHECKPOINT_KINDS),
        origin = CheckpointCategory.of(origin, CHECKPOINT_ORIGINS),
        sourceCheckpointId = null,
        paths = emptyList(),
        postDigest = null,
        recordedAtMs = null,
    )

    @Test
    fun `rows render newest first by journal sequence`() {
        val ordered = Checkpoints.newestFirst(
            listOf(row("a", 1), row("c", 9), row("b", 4)),
        )
        assertEquals(listOf("c", "b", "a"), ordered.map { it.checkpointId })
    }

    @Test
    fun `a row without a sequence sorts last, never first`() {
        // An unknown position must not be presented as the most recent thing
        // that happened — that is the one direction the mistake is dangerous in,
        // because the newest row is what `undo last` addresses.
        val ordered = Checkpoints.newestFirst(listOf(row("none", null), row("a", 1)))
        assertEquals(listOf("a", "none"), ordered.map { it.checkpointId })
    }

    @Test
    fun `ordering is exact at u64 scale`() {
        // Above 2^53 a double-backed comparison starts calling two different
        // positions equal. These two differ by one.
        val big = 9_007_199_254_740_993L
        val ordered = Checkpoints.newestFirst(listOf(row("low", big), row("high", big + 1)))
        assertEquals(listOf("high", "low"), ordered.map { it.checkpointId })
    }

    @Test
    fun `consecutive rows of one run are one turn group`() {
        val groups = Checkpoints.turnGroups(
            listOf(
                row("d", 4, run = "run-2"),
                row("c", 3, run = "run-2"),
                row("b", 2, run = "run-1"),
                row("a", 1, run = "run-1"),
            ),
        )
        assertEquals(listOf("run-2", "run-1"), groups.map { it.runId })
        assertEquals(2, groups[0].checkpoints.size)
        assertEquals(2, groups[1].checkpoints.size)
    }

    @Test
    fun `two separated runs of one id stay two groups`() {
        // Grouping is positional, as the desktop panel does it: a page boundary
        // between them means the sheet does not know they are one turn.
        val groups = Checkpoints.turnGroups(
            listOf(row("c", 3, run = "run-1"), row("b", 2, run = "run-2"), row("a", 1, run = "run-1")),
        )
        assertEquals(3, groups.size)
    }

    @Test
    fun `a group with no run id is still grouped and still rendered`() {
        val groups = Checkpoints.turnGroups(listOf(row("a", 1, run = null)))
        assertEquals(1, groups.size)
        assertEquals(null, groups.single().runId)
    }

    @Test
    fun `merging an appended page drops repeats and keeps idless rows`() {
        val merged = Checkpoints.merge(
            previous = listOf(row("c", 3), row("b", 2)),
            incoming = listOf(row("b", 2), row("a", 1), row(null, 0)),
        )
        assertEquals(listOf("c", "b", "a", null), merged.map { it.checkpointId })
    }

    @Test
    fun `a row without an id cannot be addressed`() {
        assertFalse(row(null, 1).addressable)
        assertTrue(row("a", 1).addressable)
    }

    @Test
    fun `an unrecognised kind survives verbatim and is marked`() {
        // Both Rust enums are closed today and may grow; a word this client
        // does not know is shown as the daemon wrote it.
        val unknown = row("a", 1, kind = "rename").kind
        assertEquals("rename", unknown.raw)
        assertFalse(unknown.recognized)
        assertFalse(unknown.absent)
    }

    @Test
    fun `an omitted origin is absent rather than asserted as tool`() {
        // `origin` is #[serde(default)] on the Rust side, so an old daemon can
        // omit it. Rendering the Rust default as if the daemon had said it
        // would put a fact on screen that never crossed the wire.
        val absent = row("a", 1, origin = null).origin
        assertTrue(absent.absent)
        assertFalse(absent.recognized)
    }

    @Test
    fun `the sheet distinguishes not read yet from genuinely empty`() {
        val unread = CheckpointsUiState(sessionId = "s")
        assertFalse(unread.empty)
        val loaded = CheckpointsUiState(sessionId = "s", rows = emptyList())
        assertTrue(loaded.empty)
    }

    @Test
    fun `clearing an outcome leaves the rows alone`() {
        val state = CheckpointsUiState(
            sessionId = "s",
            rows = listOf(row("a", 1)),
            conflict = CheckpointConflictView("p", null, null),
            receipt = CheckpointReceiptView(null, emptyList(), 1),
            error = "boom",
        )
        val cleared = state.clearedOutcome()
        assertEquals(1, cleared.rows?.size)
        assertEquals(null, cleared.conflict)
        assertEquals(null, cleared.receipt)
        assertEquals(null, cleared.error)
    }
}
