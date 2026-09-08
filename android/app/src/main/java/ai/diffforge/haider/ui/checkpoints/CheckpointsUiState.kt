package ai.diffforge.haider.ui.checkpoints

/**
 * Everything the checkpoints sheet renders, for the ONE session it is open on.
 *
 * The desktop keeps a map keyed by session because it shows several panes at
 * once; a phone shows one sheet, so the session id is a field and closing the
 * sheet drops the state with it — a page read for a session that is no longer
 * on screen must not be rendered under a different session's title.
 *
 * [rows] is never edited optimistically. A mutation's receipt is authority for
 * what happened, and the timeline is then re-read from `checkpoint.list`; the
 * client does not predict what undo produced.
 */
data class CheckpointsUiState(
    val sessionId: String? = null,
    /** Null until the first read returns. Empty rows after that means genuinely none. */
    val rows: List<CheckpointView>? = null,
    val nextCursor: Long? = null,
    val cursorState: CheckpointCursorState = CheckpointCursorState.End,
    val loading: Boolean = false,
    /** Which gesture is in flight, so exactly one control says so. */
    val pending: CheckpointGesture? = null,
    /** A gesture waiting for the confirm step. Undo, redo and rollback all have one. */
    val confirming: CheckpointGesture? = null,
    val unavailable: String? = null,
    val error: String? = null,
    val conflict: CheckpointConflictView? = null,
    val rollbackConflict: CheckpointRollbackConflictView? = null,
    val branchMismatch: CheckpointBranchMismatchView? = null,
    val receipt: CheckpointReceiptView? = null,
    /** A branch-create refusal or its typed unavailable reason. */
    val branchNotice: String? = null,
) {
    val open: Boolean get() = sessionId != null
    val busy: Boolean get() = pending != null

    /** Loaded and genuinely empty — distinct from "not read yet". */
    val empty: Boolean get() = rows?.isEmpty() == true

    val groups: List<CheckpointTurnGroup> get() = Checkpoints.turnGroups(rows.orEmpty())

    /** Clears everything a mutation could have left behind, before the next one. */
    fun clearedOutcome(): CheckpointsUiState = copy(
        error = null,
        conflict = null,
        rollbackConflict = null,
        branchMismatch = null,
        receipt = null,
        branchNotice = null,
    )
}

/** One addressed gesture: what to do, and the coordinate it is done to. */
sealed interface CheckpointGesture {
    /** [target] is a checkpoint id or [Checkpoints.TARGET_LAST]. */
    data class Undo(val target: String) : CheckpointGesture

    data class Redo(val target: String) : CheckpointGesture

    /** Addresses a whole run. All of its durable edits are restored, or none. */
    data class Rollback(val runId: String) : CheckpointGesture
}
