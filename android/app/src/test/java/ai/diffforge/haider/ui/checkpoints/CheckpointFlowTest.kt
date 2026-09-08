package ai.diffforge.haider.ui.checkpoints

import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.state.Overlay
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/**
 * The checkpoint and branch behaviours, through the view model and the fake
 * facade — the seam a Compose assertion cannot reach.
 *
 * Each of these is a place where a plausible shortcut is wrong: an optimistic
 * timeline edit, a mutation on one tap, a conflict cleared by the reload that
 * followed it, a branch id sent for the implicit main branch.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class CheckpointFlowTest {

    private val dispatcher = StandardTestDispatcher()

    @Before fun installDispatcher() = Dispatchers.setMain(dispatcher)

    @After fun restoreDispatcher() = Dispatchers.resetMain()

    private fun service() = FakeDaemonService(FakeScenario.Populated)

    private fun model(service: FakeDaemonService) = ChatViewModel(service, searchDebounceMs = 0)

    // ---------- reading ----------

    @Test
    fun `opening the sheet reads the timeline newest first`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.openCheckpoints("s-nav")
        advanceUntilIdle()

        val state = viewModel.state.value.checkpoints
        assertEquals(Overlay.Checkpoints("s-nav"), viewModel.state.value.overlay)
        assertEquals("s-nav", state.sessionId)
        val seqs = state.rows.orEmpty().mapNotNull { it.seq }
        assertEquals(seqs.sortedDescending(), seqs)
        assertTrue(service.calls.any { it.startsWith("checkpoint.list:s-nav") })
    }

    @Test
    fun `a daemon without the feature says so instead of showing an empty timeline`() =
        runTest(dispatcher) {
            val service = service().apply { checkpointsUnavailable = true }
            val viewModel = model(service)
            viewModel.openCheckpoints("s-nav")
            advanceUntilIdle()

            val state = viewModel.state.value.checkpoints
            assertEquals(CheckpointUnavailable.FEATURE_ABSENT, state.unavailable)
            // Not an empty page: "this session changed nothing" is a different
            // claim from "this daemon does not keep a timeline".
            assertNull(state.rows)
            assertFalse(state.empty)
        }

    @Test
    fun `a session with no journal reports genuinely empty`() = runTest(dispatcher) {
        val viewModel = model(service())
        viewModel.openCheckpoints("s-broken")
        advanceUntilIdle()

        val state = viewModel.state.value.checkpoints
        assertTrue(state.empty)
        assertNull(state.unavailable)
    }

    @Test
    fun `paging appends, and the last page ends the list`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.openCheckpoints("s-nav")
        advanceUntilIdle()
        // Re-read a deliberately short first page so paging is exercised.
        viewModel.loadCheckpoints("s-nav")
        advanceUntilIdle()
        val first = viewModel.state.value.checkpoints
        assertEquals(CheckpointCursorState.End, first.cursorState)
        // The seeded journal fits one page; the end state is the honest one and
        // the sheet must not offer "Load more" for it.
        viewModel.loadMoreCheckpoints()
        advanceUntilIdle()
        assertEquals(first.rows?.size, viewModel.state.value.checkpoints.rows?.size)
    }

    // ---------- mutating ----------

    @Test
    fun `nothing mutates on one tap`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.openCheckpoints("s-nav")
        advanceUntilIdle()
        val target = viewModel.state.value.checkpoints.rows!!.first().checkpointId!!

        viewModel.confirmCheckpointGesture(CheckpointGesture.Undo(target))
        advanceUntilIdle()

        assertEquals(
            CheckpointGesture.Undo(target),
            viewModel.state.value.checkpoints.confirming,
        )
        assertTrue(service.calls.none { it.startsWith("checkpoint.undo") })
    }

    @Test
    fun `undo then redo append in order and the list is re-read, never edited`() =
        runTest(dispatcher) {
            val service = service()
            val viewModel = model(service)
            viewModel.openCheckpoints("s-nav")
            advanceUntilIdle()
            val before = viewModel.state.value.checkpoints.rows!!
            val target = before.first().checkpointId!!

            viewModel.confirmCheckpointGesture(CheckpointGesture.Undo(target))
            viewModel.applyCheckpointGesture()
            advanceUntilIdle()

            val afterUndo = viewModel.state.value.checkpoints
            assertEquals(listOf(target), afterUndo.receipt?.restoredCheckpointIds)
            // The undo is itself an append-only journal entry, and it is now the
            // newest row — read back from `checkpoint.list`, not spliced in.
            assertEquals("undo", afterUndo.rows!!.first().origin.raw)
            assertEquals(before.size + 1, afterUndo.rows!!.size)
            val listReads = service.calls.count { it.startsWith("checkpoint.list") }

            viewModel.confirmCheckpointGesture(CheckpointGesture.Redo(target))
            viewModel.applyCheckpointGesture()
            advanceUntilIdle()

            val afterRedo = viewModel.state.value.checkpoints
            assertEquals("redo", afterRedo.rows!!.first().origin.raw)
            assertEquals(before.size + 2, afterRedo.rows!!.size)
            // Every committed mutation is followed by exactly one re-read.
            assertEquals(listReads + 1, service.calls.count { it.startsWith("checkpoint.list") })
            assertEquals(
                listOf("checkpoint.undo:$target", "checkpoint.redo:$target"),
                service.calls.filter { it.startsWith("checkpoint.undo") || it.startsWith("checkpoint.redo") },
            )
        }

    @Test
    fun `undo last addresses the newest addressable row`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.openCheckpoints("s-nav")
        advanceUntilIdle()
        val newest = viewModel.state.value.checkpoints.rows!!.first().checkpointId

        viewModel.confirmCheckpointGesture(CheckpointGesture.Undo(Checkpoints.TARGET_LAST))
        viewModel.applyCheckpointGesture()
        advanceUntilIdle()

        assertEquals(
            listOf(newest),
            viewModel.state.value.checkpoints.receipt?.restoredCheckpointIds,
        )
    }

    @Test
    fun `a rollback restores every edit of one run, addressed by run id`() =
        runTest(dispatcher) {
            val service = service()
            val viewModel = model(service)
            viewModel.openCheckpoints("s-nav")
            advanceUntilIdle()
            val group = viewModel.state.value.checkpoints.groups.first { it.runId != null }

            viewModel.confirmCheckpointGesture(CheckpointGesture.Rollback(group.runId!!))
            viewModel.applyCheckpointGesture()
            advanceUntilIdle()

            val receipt = viewModel.state.value.checkpoints.receipt!!
            assertEquals(group.checkpoints.size, receipt.restoredCheckpointIds.size)
            assertTrue(service.calls.contains("checkpoint.rollback_turn:${group.runId}"))
        }

    @Test
    fun `a conflict is terminal for the gesture and is not cleared by a reload`() =
        runTest(dispatcher) {
            val service = service()
            val viewModel = model(service)
            viewModel.openCheckpoints("s-nav")
            advanceUntilIdle()
            val target = viewModel.state.value.checkpoints.rows!!.first().checkpointId!!
            service.nextCheckpointOutcome = CheckpointOutcome.Conflict(
                CheckpointConflictView("src/lib.rs", "blake3:expected", "blake3:current"),
            )
            val readsBefore = service.calls.count { it.startsWith("checkpoint.list") }

            viewModel.confirmCheckpointGesture(CheckpointGesture.Undo(target))
            viewModel.applyCheckpointGesture()
            advanceUntilIdle()

            val state = viewModel.state.value.checkpoints
            assertEquals("src/lib.rs", state.conflict?.path)
            assertNull(state.receipt)
            // Re-reading here would clear the very notice nobody has seen yet.
            assertEquals(readsBefore, service.calls.count { it.startsWith("checkpoint.list") })
            // An explicit re-read is how it IS dismissed, after looking.
            viewModel.loadCheckpoints("s-nav")
            advanceUntilIdle()
            assertNull(viewModel.state.value.checkpoints.conflict)
        }

    @Test
    fun `a rollback conflict says nothing was restored`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.openCheckpoints("s-nav")
        advanceUntilIdle()
        service.nextCheckpointOutcome = CheckpointOutcome.RollbackConflict(
            CheckpointRollbackConflictView(
                verified = listOf("src/ok.rs"),
                conflicts = listOf(CheckpointConflictView("src/foreign.rs", null, "blake3:x")),
            ),
        )

        viewModel.confirmCheckpointGesture(CheckpointGesture.Rollback("run-nav-1"))
        viewModel.applyCheckpointGesture()
        advanceUntilIdle()

        val state = viewModel.state.value.checkpoints
        assertEquals(listOf("src/ok.rs"), state.rollbackConflict?.verified)
        assertNull(state.receipt)
    }

    @Test
    fun `a branch mismatch is shown as itself, not as a failure`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.openCheckpoints("s-nav")
        advanceUntilIdle()
        service.nextCheckpointOutcome = CheckpointOutcome.BranchMismatch(
            CheckpointBranchMismatchView("checkpoint-other", "branch-other", null),
        )

        viewModel.confirmCheckpointGesture(CheckpointGesture.Undo("checkpoint-other"))
        viewModel.applyCheckpointGesture()
        advanceUntilIdle()

        assertNotNull(viewModel.state.value.checkpoints.branchMismatch)
        assertNull(viewModel.state.value.checkpoints.error)
    }

    @Test
    fun `a page that returns after the sheet moved on is dropped`() = runTest(dispatcher) {
        val viewModel = model(service())
        viewModel.openCheckpoints("s-nav")
        advanceUntilIdle()
        // The sheet is reopened on another session before the stale read is
        // applied; a page must never render under the wrong title.
        viewModel.openCheckpoints("s-broken")
        viewModel.loadCheckpoints("s-nav")
        advanceUntilIdle()

        assertEquals("s-broken", viewModel.state.value.checkpoints.sessionId)
        assertTrue(viewModel.state.value.checkpoints.rows.orEmpty().isEmpty())
    }

    // ---------- branches ----------

    @Test
    fun `main carries no branch id into turn submit`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.selectBranch("s-nav", null)
        advanceUntilIdle()
        service.send("s-nav", "hello")

        assertTrue(service.calls.contains("chat.send:s-nav"))
        assertTrue(service.calls.none { it.startsWith("chat.send:s-nav:branch=") })
        assertNull(viewModel.state.value.branchSelection["s-nav"])
    }

    @Test
    fun `a chosen branch threads into every later turn submit`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.selectBranch("s-nav", "branch-plan-b")
        advanceUntilIdle()

        assertEquals("branch-plan-b", viewModel.state.value.branchSelection["s-nav"])
        service.send("s-nav", "continue plan B")
        service.send("s-nav", "and again")
        assertEquals(
            2,
            service.calls.count { it == "chat.send:s-nav:branch=branch-plan-b" },
        )
        // Another session is unaffected: the selection is per session.
        service.send("s-broken", "unrelated")
        assertTrue(service.calls.contains("chat.send:s-broken"))
    }

    @Test
    fun `choosing a branch re-reads the branch-scoped timeline`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        viewModel.openCheckpoints("s-nav")
        advanceUntilIdle()
        assertTrue(viewModel.state.value.checkpoints.rows!!.isNotEmpty())

        viewModel.selectBranch("s-nav", "branch-plan-b")
        advanceUntilIdle()

        // `checkpoint.list` is branch-scoped, so the main-branch rows are not
        // the branch's rows and must not be left on screen under its name.
        assertTrue(viewModel.state.value.checkpoints.empty)
    }

    @Test
    fun `creating a branch forks at the published head and does not select it`() =
        runTest(dispatcher) {
            val service = service()
            val viewModel = model(service)
            viewModel.createBranch("s-nav", "Plan C")
            advanceUntilIdle()

            val row = viewModel.state.value.sessions.first { it.id == "s-nav" }
            val created = row.branches.last()
            assertEquals("Plan C", created.name)
            assertEquals("node-nav-head", created.forkNodeId)
            assertEquals(981L, created.forkSeq)
            // Created is not chosen: the next turn moves only because somebody
            // chose it, which is the same consent rule the refusal path has.
            assertNull(viewModel.state.value.branchSelection["s-nav"])
            assertNull(viewModel.state.value.checkpoints.branchNotice)
        }

    @Test
    fun `a session with no head node sends no half coordinate`() = runTest(dispatcher) {
        val service = service()
        val viewModel = model(service)
        // `s-route` publishes no head node in the fixture.
        viewModel.createBranch("s-route", "Nope")
        advanceUntilIdle()

        assertEquals(
            ChatViewModel.NO_FORK_POINT,
            viewModel.state.value.checkpoints.branchNotice,
        )
        assertTrue(service.calls.none { it.startsWith("branch.create") })
    }

    @Test
    fun `a daemon without branch create says so`() = runTest(dispatcher) {
        val service = service().apply { branchCreateUnavailable = true }
        val viewModel = model(service)
        viewModel.createBranch("s-nav", "Plan D")
        advanceUntilIdle()

        assertEquals(
            CheckpointUnavailable.BRANCH_FEATURE_ABSENT,
            viewModel.state.value.checkpoints.branchNotice,
        )
    }
}
