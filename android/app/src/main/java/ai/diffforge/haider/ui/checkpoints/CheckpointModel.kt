package ai.diffforge.haider.ui.checkpoints

/**
 * Pure checkpoint views, ported from the desktop `checkpointModel.js`.
 *
 * Durable workspace state belongs to the daemon: this file labels published
 * facts and never invents a branch, revision, cursor, path or mutation result.
 *
 * Two deliberate divergences from the desktop port, because the wire truth is
 * `crates/haider-protocol/src/checkpoint.rs`, not the Tauri SDK the desktop
 * client sits behind:
 *
 *  - `seq`, `workspace_revision` and the cursor are **numbers** on this wire
 *    (`wire_golden_tests.rs::checkpoint_list_and_record_optional_fields_are_pinned`
 *    pins `"seq": 42` and `"next_cursor": 42`). The desktop refuses anything but
 *    a decimal string because its SDK hands it strings; refusing a number here
 *    would refuse every page the daemon actually sends;
 *  - `CheckpointListPage.next_cursor` carries `skip_serializing_if`, so an
 *    ABSENT cursor is the end of the list. The desktop demands an explicit
 *    `null` and calls absence invalid — on this wire that would call every
 *    final page malformed.
 *
 * `workspace_revision` is a *string* id (`WorkspaceRevision::new("…")`), not a
 * number; it is carried verbatim and never parsed.
 */

/** `CheckpointKind` (checkpoint.rs:22). */
val CHECKPOINT_KINDS: Set<String> = setOf("edit", "write", "create", "delete", "move")

/** `CheckpointOrigin` (checkpoint.rs:46). Undo/redo/rollback are ordinary history. */
val CHECKPOINT_ORIGINS: Set<String> = setOf("tool", "undo", "redo", "rollback_turn")

/**
 * A published category the client may or may not recognise.
 *
 * Both Rust enums are closed today and may grow. An unrecognised word survives
 * verbatim with [recognized] false rather than being folded into a neighbour or
 * dropped — the row then says the daemon's own word and admits it does not know
 * it, which is what the desktop's `categoryView` does.
 */
data class CheckpointCategory(
    val raw: String?,
    val recognized: Boolean,
) {
    val absent: Boolean get() = raw == null

    companion object {
        fun of(raw: String?, known: Set<String>): CheckpointCategory =
            CheckpointCategory(raw = raw?.takeIf { it.isNotEmpty() }, recognized = raw in known)
    }
}

/** One `CheckpointPath` (checkpoint.rs:68). */
data class CheckpointPathView(
    val path: String,
    val preDigest: String? = null,
    val postDigest: String? = null,
    /**
     * `pre_artifact == null` with no reason is the explicit absent marker; a
     * reason means the pre-image was omitted, so undo cannot restore this path
     * from the journal. The two are never collapsed.
     */
    val preArtifact: String? = null,
    val truncatedReason: String? = null,
) {
    /** The mutation left this path absent. */
    val removed: Boolean get() = postDigest == null

    /** No pre-image was frozen, and the daemon said why. */
    val truncated: Boolean get() = truncatedReason != null
}

/** One `CheckpointRecorded` row (checkpoint.rs:84), as the sheet needs it. */
data class CheckpointView(
    val checkpointId: String?,
    val sessionId: String?,
    val branchId: String?,
    val runId: String?,
    val effectId: String?,
    val callId: String?,
    /** Zero is a producer placeholder the store replaces; absent stays null. */
    val seq: Long?,
    val workspaceRevision: String?,
    val kind: CheckpointCategory,
    val origin: CheckpointCategory,
    val sourceCheckpointId: String?,
    val paths: List<CheckpointPathView>,
    val postDigest: String?,
    val recordedAtMs: Long?,
) {
    /** A row with no id cannot be an undo/redo target; the affordance is refused. */
    val addressable: Boolean get() = !checkpointId.isNullOrEmpty()

    val touchedPaths: List<String> get() = paths.map { it.path }
}

/** One `BranchDescriptor` (branch.rs:11). Main is implicit and carries no row. */
data class BranchView(
    val branchId: String,
    val name: String,
    val sourceBranchId: String? = null,
    val forkNodeId: String? = null,
    val forkSeq: Long? = null,
    val createdSeq: Long? = null,
    val createdAtMs: Long? = null,
    val headNodeId: String? = null,
    val headSeq: Long? = null,
)

/**
 * How a page's pagination stands.
 *
 * [End] is an absent cursor, which is what the daemon sends for a final page.
 * [Invalid] is a cursor that was present but not a number: pagination stops and
 * says so rather than guessing a position that may already name a different row.
 */
enum class CheckpointCursorState { More, End, Invalid }

data class CheckpointPage(
    val checkpoints: List<CheckpointView> = emptyList(),
    val nextCursor: Long? = null,
    val cursorState: CheckpointCursorState = CheckpointCursorState.End,
) {
    val empty: Boolean get() = checkpoints.isEmpty()
}

/** One turn's worth of checkpoints, in list order. `run_id` is the rollback coordinate. */
data class CheckpointTurnGroup(val runId: String?, val checkpoints: List<CheckpointView>)

/** A typed `checkpoint_conflict` (frame.rs:5701, checkpoint.rs:119). */
data class CheckpointConflictView(
    val path: String?,
    val expectedDigest: String?,
    val currentDigest: String?,
)

/**
 * A typed `checkpoint_rollback_conflict` (checkpoint.rs:129).
 *
 * All-or-nothing: [verified] names every path whose freshness matched and
 * **nothing was restored**. The sheet says that out loud, because a list of
 * verified paths reads like a list of restored ones otherwise.
 */
data class CheckpointRollbackConflictView(
    val verified: List<String>,
    val conflicts: List<CheckpointConflictView>,
)

/** A `checkpoint_branch_mismatch` (frame.rs:5707). */
data class CheckpointBranchMismatchView(
    val checkpointId: String?,
    val checkpointBranchId: String?,
    val requestedBranchId: String?,
)

/** `CheckpointMutationReceipt` (checkpoint.rs:139). Post-mutation authority. */
data class CheckpointReceiptView(
    val checkpoint: CheckpointView?,
    /**
     * Carried verbatim, in the daemon's order. It is never inferred from the
     * target or from the checkpoint — an empty list means "restored nothing",
     * which is a different fact from "the daemon did not say".
     */
    val restoredCheckpointIds: List<String>,
    val workerGeneration: Long?,
)

/** What a `checkpoint.list` read returned. */
sealed interface CheckpointListResult {
    data class Page(val page: CheckpointPage) : CheckpointListResult

    /** The daemon does not serve `checkpoint_v1`, or refused the read. */
    data class Unavailable(val reason: String) : CheckpointListResult

    data class Failed(val code: String) : CheckpointListResult
}

/** What one undo / redo / rollback returned. */
sealed interface CheckpointOutcome {
    data class Committed(val receipt: CheckpointReceiptView) : CheckpointOutcome

    /** Freshness refused to overwrite bytes the target did not produce. */
    data class Conflict(val conflict: CheckpointConflictView) : CheckpointOutcome

    /** The all-or-nothing rollback preflight failed; no path was restored. */
    data class RollbackConflict(val conflict: CheckpointRollbackConflictView) : CheckpointOutcome

    /** The command addressed history owned by another branch. */
    data class BranchMismatch(val mismatch: CheckpointBranchMismatchView) : CheckpointOutcome

    data class Unavailable(val reason: String) : CheckpointOutcome
    data class Failed(val code: String) : CheckpointOutcome
}

/** What one `branch.create` returned. */
sealed interface BranchOutcome {
    data class Created(val branch: BranchView) : BranchOutcome
    data class Unavailable(val reason: String) : BranchOutcome
    data class Failed(val code: String) : BranchOutcome
}

/** Stable reasons this client states when a door is not served. */
object CheckpointUnavailable {
    /** `FEATURE_CHECKPOINT_V1` (frame.rs:617) was not in the welcome features. */
    const val FEATURE_ABSENT: String = "checkpoint_v1_unavailable"

    /** `FEATURE_BRANCH_CREATE_V1` (frame.rs:403) was not in the welcome features. */
    const val BRANCH_FEATURE_ABSENT: String = "branch_create_v1_unavailable"
}

object Checkpoints {

    /** The literal `checkpoint.undo`/`redo` accept in place of an id (frame.rs:4405). */
    const val TARGET_LAST: String = "last"

    /** `CHECKPOINT_LIST_MAX_PAGE` (checkpoint.rs:16). A larger page is refused. */
    const val MAX_PAGE: Int = 100

    /** What this client asks for. Halved from the cap: a phone renders a page. */
    const val PAGE_LIMIT: Int = 50

    /**
     * Newest first, by journal sequence.
     *
     * A row without a seq sorts last rather than first: an unknown position
     * must not be presented as the most recent thing that happened.
     */
    fun newestFirst(rows: List<CheckpointView>): List<CheckpointView> =
        rows.sortedWith(
            compareBy<CheckpointView> { it.seq == null }
                .thenByDescending { it.seq ?: Long.MIN_VALUE },
        )

    /**
     * Consecutive rows that share a `run_id`, in the order they are rendered.
     *
     * Grouping is positional, exactly as the desktop panel does it: two
     * separated runs of the same id stay two groups, because rolling back "this
     * turn" addresses the run, and merging distant rows would imply the sheet
     * knows they are one turn when the list may be paged between them.
     */
    fun turnGroups(rows: List<CheckpointView>): List<CheckpointTurnGroup> {
        val groups = mutableListOf<CheckpointTurnGroup>()
        var current = mutableListOf<CheckpointView>()
        var currentRun: String? = null
        rows.forEach { row ->
            if (current.isNotEmpty() && row.runId == currentRun) {
                current += row
            } else {
                if (current.isNotEmpty()) groups += CheckpointTurnGroup(currentRun, current.toList())
                current = mutableListOf(row)
                currentRun = row.runId
            }
        }
        if (current.isNotEmpty()) groups += CheckpointTurnGroup(currentRun, current.toList())
        return groups
    }

    /**
     * Merge an appended page into the rows already held.
     *
     * Ids seen before are dropped so a re-read that overlaps does not double a
     * row; a row without an id is kept, because dropping it would silently lose
     * history the daemon did publish.
     */
    fun merge(previous: List<CheckpointView>, incoming: List<CheckpointView>): List<CheckpointView> {
        val seen = mutableSetOf<String>()
        val rows = (previous + incoming).filter { row ->
            val id = row.checkpointId ?: return@filter true
            seen.add(id)
        }
        return newestFirst(rows)
    }
}
