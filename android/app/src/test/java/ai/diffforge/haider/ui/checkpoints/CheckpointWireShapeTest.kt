package ai.diffforge.haider.ui.checkpoints

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pins every checkpoint and branch door to the Rust wire goldens.
 *
 * The request bodies below are
 * `crates/haider-rpc/tests/wire_golden_tests.rs::checkpoint_list_and_record_optional_fields_are_pinned`
 * verbatim, and the `branch.create` pair is
 * `crates/haider-rpc/tests/fixtures/wire_transcript.json` entries 73/74. The
 * desktop client reaches these through Tauri commands (`checkpoint_list`,
 * `checkpoint_undo`, …), which are not method names on this wire; deriving the
 * Android shapes from those would have shipped four doors that do not exist.
 */
class CheckpointWireShapeTest {

    // ---------- requests ----------

    @Test
    fun `checkpoint list matches the golden body exactly`() {
        val body = CheckpointRpcAdapter.listRequest(
            sessionId = "session-checkpoint-wire",
            branchId = "branch-checkpoint-wire",
            cursor = 41,
            limit = 25,
        )
        assertEquals("checkpoint.list", body.getString("method"))
        assertEquals("session-checkpoint-wire", body.getString("session_id"))
        assertEquals("branch-checkpoint-wire", body.getString("branch_id"))
        // A NUMBER, not a decimal string: the desktop model refuses numbers
        // because its SDK hands it strings, and this wire sends `"cursor": 41`.
        assertEquals(41L, body.getLong("cursor"))
        assertEquals(25, body.getInt("limit"))
    }

    @Test
    fun `an omitted branch is omitted, never sent as null`() {
        val body = CheckpointRpcAdapter.listRequest("s")
        // `branch_id: null` decodes as MAIN, which is a different branch from
        // the one that was not stated.
        assertFalse(body.has("branch_id"))
        assertFalse(body.has("cursor"))
    }

    @Test
    fun `the page limit is capped at the protocol maximum`() {
        // CHECKPOINT_LIST_MAX_PAGE (checkpoint.rs:16). The daemon refuses more.
        assertEquals(100, Checkpoints.MAX_PAGE)
        assertEquals(100, CheckpointRpcAdapter.listRequest("s", limit = 4_000).getInt("limit"))
        assertEquals(1, CheckpointRpcAdapter.listRequest("s", limit = 0).getInt("limit"))
    }

    @Test
    fun `undo redo and rollback match their golden bodies`() {
        val undo = CheckpointRpcAdapter.undoRequest(
            commandId = "checkpoint-undo-wire",
            sessionId = "session-checkpoint-wire",
            workerGeneration = 7,
            target = "last",
            branchId = "branch-checkpoint-wire",
        )
        assertEquals("checkpoint.undo", undo.getString("method"))
        assertEquals("checkpoint-undo-wire", undo.getString("command_id"))
        assertEquals(7L, undo.getLong("worker_generation"))
        assertEquals("last", undo.getString("target"))
        assertEquals(Checkpoints.TARGET_LAST, undo.getString("target"))

        val redo = CheckpointRpcAdapter.redoRequest(
            commandId = "checkpoint-redo-wire",
            sessionId = "session-checkpoint-wire",
            workerGeneration = 8,
            target = "checkpoint-source-wire",
            branchId = "branch-checkpoint-wire",
        )
        assertEquals("checkpoint.redo", redo.getString("method"))
        assertEquals("checkpoint-source-wire", redo.getString("target"))

        val rollback = CheckpointRpcAdapter.rollbackTurnRequest(
            commandId = "checkpoint-rollback-wire",
            sessionId = "session-checkpoint-wire",
            workerGeneration = 9,
            runId = "run-checkpoint-wire",
            branchId = "branch-checkpoint-wire",
        )
        assertEquals("checkpoint.rollback_turn", rollback.getString("method"))
        assertEquals("run-checkpoint-wire", rollback.getString("run_id"))
        // A rollback addresses a RUN, never a checkpoint id.
        assertFalse(rollback.has("target"))
    }

    @Test
    fun `branch create matches the transcript body exactly`() {
        val body = CheckpointRpcAdapter.branchCreateRequest(
            commandId = "command-branch-create",
            sessionId = "session-1",
            workerGeneration = 7,
            forkNodeId = "node-fork-1",
            forkSeq = 41,
            name = "Plan B",
        )
        assertEquals("branch.create", body.getString("method"))
        assertEquals("command-branch-create", body.getString("command_id"))
        assertEquals("session-1", body.getString("session_id"))
        assertEquals(7L, body.getLong("worker_generation"))
        assertEquals("node-fork-1", body.getString("fork_node_id"))
        assertEquals(41L, body.getLong("fork_seq"))
        assertEquals("Plan B", body.getString("name"))
        // Transcript entry 73 carries no `source_branch_id` for a main fork.
        assertFalse(body.has("source_branch_id"))
    }

    // ---------- responses ----------

    private val goldenPage = """
        {"method":"checkpoint.list","page":{"checkpoints":[{
          "checkpoint_id":"checkpoint-wire",
          "session_id":"session-checkpoint-wire",
          "branch_id":"branch-checkpoint-wire",
          "run_id":"run-checkpoint-wire",
          "effect_id":"effect-checkpoint-wire",
          "call_id":"call-checkpoint-wire",
          "seq":42,
          "workspace_revision":"workspace-revision-wire",
          "kind":"move",
          "origin":"undo",
          "source_checkpoint_id":"checkpoint-source-wire",
          "paths":[
            {"path":"src/from.rs","pre_artifact":"blake3:artifact","pre_digest":"blake3:pre"},
            {"path":"src/large.rs","pre_digest":"blake3:large","post_digest":"blake3:large-post",
             "truncated_reason":"pre-image exceeds 8388608 bytes"}
          ],
          "post_digest":"blake3:aggregate",
          "recorded_at_ms":1720000000000
        }],"next_cursor":42}}
    """.trimIndent()

    @Test
    fun `the golden page parses every published coordinate`() {
        val page = CheckpointRpcAdapter.parseListPage(JSONObject(goldenPage))
        val row = page.checkpoints.single()
        assertEquals("checkpoint-wire", row.checkpointId)
        assertEquals("branch-checkpoint-wire", row.branchId)
        assertEquals("run-checkpoint-wire", row.runId)
        assertEquals(42L, row.seq)
        // A workspace revision is a STRING id, not a number, and is carried
        // verbatim rather than parsed.
        assertEquals("workspace-revision-wire", row.workspaceRevision)
        assertEquals("move", row.kind.raw)
        assertTrue(row.kind.recognized)
        assertEquals("undo", row.origin.raw)
        assertTrue(row.origin.recognized)
        assertEquals("checkpoint-source-wire", row.sourceCheckpointId)
        assertEquals(listOf("src/from.rs", "src/large.rs"), row.touchedPaths)
        assertEquals(42L, page.nextCursor)
        assertEquals(CheckpointCursorState.More, page.cursorState)
    }

    @Test
    fun `an omitted pre-image says why, and an absent one does not`() {
        val page = CheckpointRpcAdapter.parseListPage(JSONObject(goldenPage))
        val (present, truncated) = page.checkpoints.single().paths
        // `pre_artifact` present, no reason: the pre-image was frozen.
        assertEquals("blake3:artifact", present.preArtifact)
        assertFalse(present.truncated)
        // The mutation left this path absent — `post_digest` was omitted.
        assertTrue(present.removed)
        // No artifact WITH a reason: capture could not fail open silently.
        assertNull(truncated.preArtifact)
        assertTrue(truncated.truncated)
        assertEquals("pre-image exceeds 8388608 bytes", truncated.truncatedReason)
        assertFalse(truncated.removed)
    }

    @Test
    fun `an absent next cursor is the end of the list, not a malformed page`() {
        // CheckpointListPage.next_cursor carries skip_serializing_if, so the
        // daemon OMITS it on a final page. The desktop model demands an
        // explicit null and would call every final page invalid.
        val page = CheckpointRpcAdapter.parseListPage(
            JSONObject("""{"page":{"checkpoints":[]}}"""),
        )
        assertEquals(CheckpointCursorState.End, page.cursorState)
        assertNull(page.nextCursor)
        assertTrue(page.empty)
    }

    @Test
    fun `an explicit null cursor is also the end`() {
        val page = CheckpointRpcAdapter.parseListPage(
            JSONObject("""{"page":{"checkpoints":[],"next_cursor":null}}"""),
        )
        assertEquals(CheckpointCursorState.End, page.cursorState)
    }

    @Test
    fun `a cursor that is not a position stops paging instead of being cast`() {
        val page = CheckpointRpcAdapter.parseListPage(
            JSONObject("""{"page":{"checkpoints":[],"next_cursor":"soon"}}"""),
        )
        assertEquals(CheckpointCursorState.Invalid, page.cursorState)
        assertNull(page.nextCursor)
    }

    @Test
    fun `a receipt carries the restored ids verbatim`() {
        val receipt = CheckpointRpcAdapter.parseReceipt(
            JSONObject(
                """{"method":"checkpoint.undo","receipt":{
                     "checkpoint":{"checkpoint_id":"checkpoint-undo-1","seq":43,"kind":"write",
                                   "origin":"undo","paths":[],"post_digest":"blake3:x"},
                     "restored_checkpoint_ids":["checkpoint-b","checkpoint-a"],
                     "worker_generation":7}}""",
            ),
        )
        // Same order, same values — never re-derived from the target.
        assertEquals(listOf("checkpoint-b", "checkpoint-a"), receipt.restoredCheckpointIds)
        assertEquals(7L, receipt.workerGeneration)
        assertEquals("checkpoint-undo-1", receipt.checkpoint?.checkpointId)
    }

    @Test
    fun `restoring nothing is a different fact from not saying`() {
        val none = CheckpointRpcAdapter.parseReceipt(
            JSONObject("""{"receipt":{"restored_checkpoint_ids":[],"worker_generation":1}}"""),
        )
        assertEquals(emptyList<String>(), none.restoredCheckpointIds)
        assertEquals(1L, none.workerGeneration)
        val silent = CheckpointRpcAdapter.parseReceipt(JSONObject("""{"receipt":{}}"""))
        assertEquals(emptyList<String>(), silent.restoredCheckpointIds)
        // Absent is null, never zero: zero is a generation a daemon can send.
        assertNull(silent.workerGeneration)
    }

    // ---------- typed refusals ----------

    @Test
    fun `a checkpoint conflict is read from its tag, not its prose`() {
        val outcome = CheckpointRpcAdapter.outcomeOfError(
            code = "checkpoint_conflict",
            data = JSONObject(
                """{"kind":"checkpoint_conflict","conflict":{"path":"src/lib.rs",
                     "expected_digest":"blake3:expected","current_digest":"blake3:current"}}""",
            ),
        )
        val conflict = (outcome as CheckpointOutcome.Conflict).conflict
        assertEquals("src/lib.rs", conflict.path)
        assertEquals("blake3:expected", conflict.expectedDigest)
        assertEquals("blake3:current", conflict.currentDigest)
    }

    @Test
    fun `a rollback conflict names what matched and restores nothing`() {
        val outcome = CheckpointRpcAdapter.outcomeOfError(
            code = "provider_error",
            data = JSONObject(
                """{"kind":"checkpoint_rollback_conflict","conflict":{
                     "verified":["src/ok.rs"],
                     "conflicts":[{"path":"src/foreign.rs","current_digest":"blake3:foreign"}]}}""",
            ),
        )
        val conflict = (outcome as CheckpointOutcome.RollbackConflict).conflict
        assertEquals(listOf("src/ok.rs"), conflict.verified)
        assertEquals("src/foreign.rs", conflict.conflicts.single().path)
        // Absent digests stay null rather than manufacturing either side.
        assertNull(conflict.conflicts.single().expectedDigest)
    }

    @Test
    fun `a branch mismatch carries both branch coordinates`() {
        val outcome = CheckpointRpcAdapter.outcomeOfError(
            code = "checkpoint_branch_mismatch",
            data = JSONObject(
                """{"kind":"checkpoint_branch_mismatch","checkpoint_id":"checkpoint-other",
                     "checkpoint_branch_id":"branch-other"}""",
            ),
        )
        val mismatch = (outcome as CheckpointOutcome.BranchMismatch).mismatch
        assertEquals("checkpoint-other", mismatch.checkpointId)
        assertEquals("branch-other", mismatch.checkpointBranchId)
        // The request named main, so the daemon omitted the requested branch.
        assertNull(mismatch.requestedBranchId)
    }

    @Test
    fun `an unrecognised error is a failure, not a conflict`() {
        val outcome = CheckpointRpcAdapter.outcomeOfError("worker_generation_stale", null)
        assertEquals(CheckpointOutcome.Failed("worker_generation_stale"), outcome)
    }

    @Test
    fun `the branch create receipt parses its flat coordinates`() {
        val branch = CheckpointRpcAdapter.parseBranchCreated(
            JSONObject(
                """{"method":"branch.create","session_id":"session-1","branch_id":"branch-plan-b",
                     "fork_node_id":"node-fork-1","fork_seq":41,"created_seq":52,
                     "worker_generation":7,"name":"Plan B"}""",
            ),
        )
        assertEquals("branch-plan-b", branch?.branchId)
        assertEquals("Plan B", branch?.name)
        assertEquals(41L, branch?.forkSeq)
        assertEquals(52L, branch?.createdSeq)
    }

    @Test
    fun `the stable feature and error names are the Rust ones`() {
        assertEquals("checkpoint_v1", CheckpointRpcAdapter.FEATURE_CHECKPOINT_V1)
        assertEquals("branch_create_v1", CheckpointRpcAdapter.FEATURE_BRANCH_CREATE_V1)
        assertEquals("checkpoint_conflict", CheckpointRpcAdapter.ERROR_CHECKPOINT_CONFLICT)
        assertEquals(
            "checkpoint_branch_mismatch",
            CheckpointRpcAdapter.ERROR_CHECKPOINT_BRANCH_MISMATCH,
        )
    }
}
