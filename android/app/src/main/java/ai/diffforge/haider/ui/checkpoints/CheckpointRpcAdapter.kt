package ai.diffforge.haider.ui.checkpoints

import org.json.JSONArray
import org.json.JSONObject

/**
 * The one file that knows the checkpoint and branch wire shapes.
 *
 * Every builder and parser is derived from the frozen Rust declarations and
 * checked against the Rust wire goldens, not from the desktop client's Tauri
 * command names (`checkpoint_list`, `checkpoint_undo`, … are Tauri commands;
 * they are not methods on this wire):
 *
 * | door | Rust | canonical shape |
 * |---|---|---|
 * | `checkpoint.list` | `RequestBody::CheckpointList` (frame.rs:4387) | `{session_id, branch_id?, cursor?, limit}` — `cursor` and `limit` are NUMBERS; `limit` is a `u16` capped at `CHECKPOINT_LIST_MAX_PAGE` |
 * | `checkpoint.undo` | frame.rs:4396 | `{command_id, session_id, branch_id?, worker_generation, target}` — `target` is a checkpoint id or the literal `last` |
 * | `checkpoint.redo` | frame.rs:4406 | the same shape |
 * | `checkpoint.rollback_turn` | frame.rs:4416 | `{command_id, session_id, branch_id?, worker_generation, run_id}` |
 * | `branch.create` | frame.rs:3617 | `{command_id, session_id, worker_generation, source_branch_id?, fork_node_id, fork_seq, name?}` |
 *
 * Responses: `checkpoint.list` answers `{page: CheckpointListPage}`; the three
 * mutations answer `{receipt: CheckpointMutationReceipt}`; `branch.create`
 * answers the flat coordinates `{session_id, branch_id, source_branch_id?,
 * fork_node_id, fork_seq, created_seq, worker_generation, name}` (frame.rs:4844).
 *
 * Golden bodies live in
 * `crates/haider-rpc/tests/wire_golden_tests.rs::checkpoint_list_and_record_optional_fields_are_pinned`
 * and `crates/haider-rpc/tests/fixtures/wire_transcript.json` entries 73/74
 * (`branch.create`) and 82/83 (`turn.submit` carrying `branch_id`).
 *
 * Optionals declared `skip_serializing_if` are OMITTED, never sent as an
 * explicit null — a null `branch_id` would decode as main, which is a different
 * branch from the one that was not stated.
 */
object CheckpointRpcAdapter {
    const val METHOD_CHECKPOINT_LIST = "checkpoint.list"
    const val METHOD_CHECKPOINT_UNDO = "checkpoint.undo"
    const val METHOD_CHECKPOINT_REDO = "checkpoint.redo"
    const val METHOD_CHECKPOINT_ROLLBACK_TURN = "checkpoint.rollback_turn"
    const val METHOD_BRANCH_CREATE = "branch.create"

    /** `FEATURE_CHECKPOINT_V1` (frame.rs:617). */
    const val FEATURE_CHECKPOINT_V1 = "checkpoint_v1"

    /** `FEATURE_BRANCH_CREATE_V1` (frame.rs:403). */
    const val FEATURE_BRANCH_CREATE_V1 = "branch_create_v1"

    /** `ERROR_CODE_CHECKPOINT_CONFLICT` (frame.rs:276). */
    const val ERROR_CHECKPOINT_CONFLICT = "checkpoint_conflict"

    /** The rollback preflight's own typed kind (`ErrorData::CheckpointRollbackConflict`). */
    const val ERROR_CHECKPOINT_ROLLBACK_CONFLICT = "checkpoint_rollback_conflict"

    /** `ERROR_CODE_CHECKPOINT_BRANCH_MISMATCH` (frame.rs:278). */
    const val ERROR_CHECKPOINT_BRANCH_MISMATCH = "checkpoint_branch_mismatch"

    // ---------- requests ----------

    fun listRequest(
        sessionId: String,
        branchId: String? = null,
        cursor: Long? = null,
        limit: Int = Checkpoints.PAGE_LIMIT,
    ): JSONObject = JSONObject()
        .put("method", METHOD_CHECKPOINT_LIST)
        .put("session_id", sessionId)
        .putIfPresent("branch_id", branchId)
        .apply { if (cursor != null) put("cursor", cursor) }
        // u16, and the daemon refuses a page above CHECKPOINT_LIST_MAX_PAGE.
        .put("limit", limit.coerceIn(1, Checkpoints.MAX_PAGE))

    fun undoRequest(
        commandId: String,
        sessionId: String,
        workerGeneration: Long,
        target: String,
        branchId: String? = null,
    ): JSONObject = mutationRequest(
        METHOD_CHECKPOINT_UNDO,
        commandId,
        sessionId,
        workerGeneration,
        branchId,
    ).put("target", target)

    fun redoRequest(
        commandId: String,
        sessionId: String,
        workerGeneration: Long,
        target: String,
        branchId: String? = null,
    ): JSONObject = mutationRequest(
        METHOD_CHECKPOINT_REDO,
        commandId,
        sessionId,
        workerGeneration,
        branchId,
    ).put("target", target)

    fun rollbackTurnRequest(
        commandId: String,
        sessionId: String,
        workerGeneration: Long,
        runId: String,
        branchId: String? = null,
    ): JSONObject = mutationRequest(
        METHOD_CHECKPOINT_ROLLBACK_TURN,
        commandId,
        sessionId,
        workerGeneration,
        branchId,
    ).put("run_id", runId)

    /**
     * `branch.create` names an EXACT committed node: both `fork_node_id` and
     * `fork_seq` are required and must come from the same published fact. A
     * checkpoint carries neither, so the caller reads them from the row's own
     * head coordinates — this builder cannot invent them.
     */
    fun branchCreateRequest(
        commandId: String,
        sessionId: String,
        workerGeneration: Long,
        forkNodeId: String,
        forkSeq: Long,
        name: String? = null,
        sourceBranchId: String? = null,
    ): JSONObject = JSONObject()
        .put("method", METHOD_BRANCH_CREATE)
        .put("command_id", commandId)
        .put("session_id", sessionId)
        .put("worker_generation", workerGeneration)
        .putIfPresent("source_branch_id", sourceBranchId)
        .put("fork_node_id", forkNodeId)
        .put("fork_seq", forkSeq)
        .putIfPresent("name", name)

    private fun mutationRequest(
        method: String,
        commandId: String,
        sessionId: String,
        workerGeneration: Long,
        branchId: String?,
    ): JSONObject = JSONObject()
        .put("method", method)
        .put("command_id", commandId)
        .put("session_id", sessionId)
        .putIfPresent("branch_id", branchId)
        .put("worker_generation", workerGeneration)

    // ---------- parsers ----------

    fun parseListPage(body: JSONObject): CheckpointPage {
        val page = body.optJSONObject("page") ?: body
        val array = page.optJSONArray("checkpoints") ?: JSONArray()
        val rows = (0 until array.length()).mapNotNull { index ->
            array.optJSONObject(index)?.let(::parseCheckpoint)
        }
        // `next_cursor` carries skip_serializing_if: ABSENT is the end of the
        // list, which is what the daemon sends for a final page. A present but
        // non-numeric cursor is invalid — pagination stops rather than casting.
        val hasCursor = page.has("next_cursor") && !page.isNull("next_cursor")
        val cursor = if (hasCursor) page.optLongOrNull("next_cursor") else null
        return CheckpointPage(
            checkpoints = Checkpoints.newestFirst(rows),
            nextCursor = cursor,
            cursorState = when {
                !hasCursor -> CheckpointCursorState.End
                cursor == null -> CheckpointCursorState.Invalid
                else -> CheckpointCursorState.More
            },
        )
    }

    fun parseCheckpoint(item: JSONObject): CheckpointView = CheckpointView(
        checkpointId = item.optStringOrNull("checkpoint_id"),
        sessionId = item.optStringOrNull("session_id"),
        branchId = item.optStringOrNull("branch_id"),
        runId = item.optStringOrNull("run_id"),
        effectId = item.optStringOrNull("effect_id"),
        callId = item.optStringOrNull("call_id"),
        seq = item.optLongOrNull("seq"),
        // WorkspaceRevision is a string id, not a number; carried verbatim.
        workspaceRevision = item.optStringOrNull("workspace_revision"),
        kind = CheckpointCategory.of(item.optStringOrNull("kind"), CHECKPOINT_KINDS),
        // `origin` is `#[serde(default)]` on the Rust side and defaults to
        // `tool`; an omitted origin is still reported as absent here rather
        // than asserted as tool, because the daemon did not say it.
        origin = CheckpointCategory.of(item.optStringOrNull("origin"), CHECKPOINT_ORIGINS),
        sourceCheckpointId = item.optStringOrNull("source_checkpoint_id"),
        paths = item.optJSONArray("paths").let { paths ->
            if (paths == null) {
                emptyList()
            } else {
                (0 until paths.length()).mapNotNull { index ->
                    paths.optJSONObject(index)?.let { path ->
                        val name = path.optStringOrNull("path") ?: return@mapNotNull null
                        CheckpointPathView(
                            path = name,
                            preDigest = path.optStringOrNull("pre_digest"),
                            postDigest = path.optStringOrNull("post_digest"),
                            preArtifact = path.optStringOrNull("pre_artifact"),
                            truncatedReason = path.optStringOrNull("truncated_reason"),
                        )
                    }
                }
            }
        },
        postDigest = item.optStringOrNull("post_digest"),
        recordedAtMs = item.optLongOrNull("recorded_at_ms"),
    )

    fun parseReceipt(body: JSONObject): CheckpointReceiptView {
        val receipt = body.optJSONObject("receipt") ?: body
        val ids = receipt.optJSONArray("restored_checkpoint_ids") ?: JSONArray()
        return CheckpointReceiptView(
            checkpoint = receipt.optJSONObject("checkpoint")?.let(::parseCheckpoint),
            restoredCheckpointIds = (0 until ids.length()).mapNotNull { ids.optString(it).ifBlank { null } },
            workerGeneration = receipt.optLongOrNull("worker_generation"),
        )
    }

    fun parseConflict(data: JSONObject): CheckpointConflictView {
        val conflict = data.optJSONObject("conflict") ?: data
        return CheckpointConflictView(
            path = conflict.optStringOrNull("path"),
            expectedDigest = conflict.optStringOrNull("expected_digest"),
            currentDigest = conflict.optStringOrNull("current_digest"),
        )
    }

    fun parseRollbackConflict(data: JSONObject): CheckpointRollbackConflictView {
        val conflict = data.optJSONObject("conflict") ?: data
        val verified = conflict.optJSONArray("verified") ?: JSONArray()
        val conflicts = conflict.optJSONArray("conflicts") ?: JSONArray()
        return CheckpointRollbackConflictView(
            verified = (0 until verified.length()).mapNotNull { verified.optString(it).ifBlank { null } },
            conflicts = (0 until conflicts.length()).mapNotNull { index ->
                conflicts.optJSONObject(index)?.let(::parseConflict)
            },
        )
    }

    fun parseBranchMismatch(data: JSONObject): CheckpointBranchMismatchView =
        CheckpointBranchMismatchView(
            checkpointId = data.optStringOrNull("checkpoint_id"),
            checkpointBranchId = data.optStringOrNull("checkpoint_branch_id"),
            requestedBranchId = data.optStringOrNull("requested_branch_id"),
        )

    /** The flat `branch.create` receipt coordinates (frame.rs:4844). */
    fun parseBranchCreated(body: JSONObject): BranchView? {
        val id = body.optStringOrNull("branch_id") ?: return null
        return BranchView(
            branchId = id,
            // `name` is required on the response; the daemon normalizes it.
            name = body.optStringOrNull("name") ?: id,
            sourceBranchId = body.optStringOrNull("source_branch_id"),
            forkNodeId = body.optStringOrNull("fork_node_id"),
            forkSeq = body.optLongOrNull("fork_seq"),
            createdSeq = body.optLongOrNull("created_seq"),
            headNodeId = body.optStringOrNull("fork_node_id"),
            headSeq = body.optLongOrNull("created_seq"),
        )
    }

    /** One `BranchDescriptor` from `ObserveSessionWire.branches` (branch.rs:11). */
    fun parseBranchDescriptor(item: JSONObject): BranchView? {
        val id = item.optStringOrNull("branch_id") ?: return null
        return BranchView(
            branchId = id,
            name = item.optStringOrNull("name") ?: id,
            sourceBranchId = item.optStringOrNull("source_branch_id"),
            forkNodeId = item.optStringOrNull("fork_node_id"),
            forkSeq = item.optLongOrNull("fork_seq"),
            createdSeq = item.optLongOrNull("created_seq"),
            createdAtMs = item.optLongOrNull("created_at_ms"),
            headNodeId = item.optStringOrNull("head_node_id"),
            headSeq = item.optLongOrNull("head_seq"),
        )
    }

    /**
     * Which typed outcome an error body names.
     *
     * `ErrorData` is internally tagged on `kind` (`wire_golden_tests.rs` pins
     * `{"kind":"checkpoint_conflict", "conflict":{…}}`), so the tag is read
     * rather than the prose message.
     */
    fun outcomeOfError(code: String?, data: JSONObject?): CheckpointOutcome {
        val kind = data?.optStringOrNull("kind") ?: code
        return when (kind) {
            ERROR_CHECKPOINT_CONFLICT ->
                CheckpointOutcome.Conflict(parseConflict(data ?: JSONObject()))
            ERROR_CHECKPOINT_ROLLBACK_CONFLICT ->
                CheckpointOutcome.RollbackConflict(parseRollbackConflict(data ?: JSONObject()))
            ERROR_CHECKPOINT_BRANCH_MISMATCH ->
                CheckpointOutcome.BranchMismatch(parseBranchMismatch(data ?: JSONObject()))
            else -> CheckpointOutcome.Failed(code ?: "checkpoint_failed")
        }
    }

    private fun JSONObject.optStringOrNull(key: String): String? =
        if (isNull(key)) null else optString(key).ifBlank { null }

    /** Null for absent AND for a value that is not an integer. Zero is a value. */
    private fun JSONObject.optLongOrNull(key: String): Long? {
        if (!has(key) || isNull(key)) return null
        return when (val value = opt(key)) {
            is Number -> value.toLong()
            // A daemon that sends a numeric coordinate as a string is honoured,
            // but a non-numeric string is refused rather than coerced to zero.
            is String -> value.toLongOrNull()
            else -> null
        }
    }

    private fun JSONObject.putIfPresent(key: String, value: String?): JSONObject =
        if (value.isNullOrBlank()) this else put(key, value)
}
