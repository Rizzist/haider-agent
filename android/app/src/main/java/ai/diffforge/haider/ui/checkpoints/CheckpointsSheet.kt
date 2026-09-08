package ai.diffforge.haider.ui.checkpoints

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow

/** Test handles for the checkpoints sheet. */
const val CHECKPOINTS_SHEET_TAG = "checkpoints_sheet"
const val CHECKPOINTS_CONFIRM_TAG = "checkpoints_confirm"

/**
 * The session's durable workspace timeline: what the agent changed on disk,
 * newest first, with the three commands that put it back.
 *
 * Ported from the desktop `CheckpointPanel.jsx`, with its two load-bearing
 * rules kept intact:
 *
 *  - nothing here predicts a mutation. A tap sends a command, the daemon's
 *    receipt is the only thing that says what happened, and the list is then
 *    re-read from `checkpoint.list` rather than edited in place;
 *  - a fact the daemon did not publish is shown as not published. A row with no
 *    `checkpoint_id` cannot be undone and its buttons say why; a group with no
 *    `run_id` cannot be rolled back and says the same.
 *
 * What the phone changes is the gesture: undo, redo and rollback each take a
 * confirm step, because these restore bytes on disk and the desktop's single
 * click is a mis-tap away on a phone.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun CheckpointsSheet(
    state: CheckpointsUiState,
    branchName: String?,
    onDismiss: () -> Unit,
    onRefresh: () -> Unit,
    onLoadMore: () -> Unit,
    onConfirm: (CheckpointGesture?) -> Unit,
    onApply: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier
                .testTag(CHECKPOINTS_SHEET_TAG)
                .verticalScroll(rememberScrollState())
                .padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.lg),
        ) {
            Text(stringResource(R.string.checkpoints_title), style = type.h4, color = colors.text)
            Text(
                listOfNotNull(
                    stringResource(R.string.checkpoints_subtitle),
                    branchName,
                ).joinToString(" · "),
                style = type.sessionMeta,
                color = colors.textMuted,
            )

            state.unavailable?.let { reason ->
                Notice(stringResource(R.string.checkpoints_unavailable), colors.textMuted)
                // The daemon's own code, so a report can name it.
                Notice(reason, colors.textMuted)
                return@Column
            }

            state.confirming?.let { gesture ->
                ConfirmStrip(gesture = gesture, onCancel = { onConfirm(null) }, onApply = onApply)
            }

            state.conflict?.let { conflict ->
                Notice(stringResource(R.string.checkpoints_conflict), colors.red)
                DigestRow(stringResource(R.string.checkpoints_conflict_path), conflict.path)
                DigestRow(stringResource(R.string.checkpoints_conflict_expected), conflict.expectedDigest)
                DigestRow(stringResource(R.string.checkpoints_conflict_current), conflict.currentDigest)
            }
            state.rollbackConflict?.let { conflict ->
                Notice(
                    stringResource(
                        R.string.checkpoints_rollback_conflict,
                        conflict.verified.size,
                        conflict.conflicts.size,
                    ),
                    colors.red,
                )
                conflict.conflicts.forEach { row ->
                    DigestRow(stringResource(R.string.checkpoints_conflict_path), row.path)
                }
            }
            state.branchMismatch?.let {
                Notice(stringResource(R.string.checkpoints_branch_mismatch), colors.amber)
            }
            state.error?.let {
                Notice(stringResource(R.string.checkpoints_failed, it), colors.red)
            }
            state.receipt?.let { receipt ->
                Notice(
                    if (receipt.restoredCheckpointIds.isEmpty()) {
                        stringResource(R.string.checkpoints_receipt_none)
                    } else {
                        stringResource(
                            R.string.checkpoints_receipt,
                            receipt.restoredCheckpointIds.size,
                        )
                    },
                    colors.accent,
                )
            }

            when {
                state.rows == null ->
                    Notice(stringResource(R.string.checkpoints_loading), colors.textMuted)
                state.empty ->
                    Notice(stringResource(R.string.checkpoints_empty), colors.textMuted)
                else -> state.groups.forEach { group ->
                    TurnGroup(
                        group = group,
                        busy = state.busy,
                        onConfirm = onConfirm,
                    )
                }
            }

            when (state.cursorState) {
                CheckpointCursorState.More -> ForgeButton(
                    text = stringResource(R.string.checkpoints_more),
                    onClick = onLoadMore,
                    kind = ForgeButtonKind.Ghost,
                    enabled = !state.loading && !state.busy,
                )
                CheckpointCursorState.Invalid ->
                    Notice(stringResource(R.string.checkpoints_cursor_invalid), colors.amber)
                CheckpointCursorState.End ->
                    if (state.rows?.isNotEmpty() == true) {
                        Notice(stringResource(R.string.checkpoints_end), colors.textMuted)
                    }
            }

            ForgeButton(
                text = stringResource(R.string.checkpoints_refresh),
                onClick = onRefresh,
                kind = ForgeButtonKind.Ghost,
                enabled = !state.loading && !state.busy,
            )
        }
    }
}

@Composable
private fun ConfirmStrip(
    gesture: CheckpointGesture,
    onCancel: () -> Unit,
    onApply: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = Modifier
            .testTag(CHECKPOINTS_CONFIRM_TAG)
            .fillMaxWidth()
            .clip(ForgeShapes.cardTight)
            .background(colors.surfaceControl)
            .border(ForgeSize.hairline, colors.amber, ForgeShapes.cardTight)
            .padding(ForgeSpace.lg),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) {
        Text(
            stringResource(
                when (gesture) {
                    is CheckpointGesture.Undo -> R.string.checkpoints_confirm_undo
                    is CheckpointGesture.Redo -> R.string.checkpoints_confirm_redo
                    is CheckpointGesture.Rollback -> R.string.checkpoints_confirm_rollback
                },
            ),
            style = type.chatBody,
            color = colors.text,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
            ForgeButton(
                text = stringResource(R.string.checkpoints_confirm_yes),
                onClick = onApply,
            )
            ForgeButton(
                text = stringResource(R.string.action_cancel),
                onClick = onCancel,
                kind = ForgeButtonKind.Ghost,
            )
        }
    }
}

@Composable
private fun TurnGroup(
    group: CheckpointTurnGroup,
    busy: Boolean,
    onConfirm: (CheckpointGesture) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
        Text(stringResource(R.string.checkpoints_turn), style = type.sessionTitle, color = colors.textSoft)
        val runId = group.runId
        if (runId == null) {
            Notice(stringResource(R.string.checkpoints_turn_absent), colors.textMuted)
        } else {
            ForgeButton(
                text = stringResource(R.string.checkpoints_rollback),
                onClick = { onConfirm(CheckpointGesture.Rollback(runId)) },
                kind = ForgeButtonKind.Ghost,
                enabled = !busy,
            )
        }
        group.checkpoints.forEach { checkpoint ->
            CheckpointCard(checkpoint = checkpoint, busy = busy, onConfirm = onConfirm)
        }
    }
}

@Composable
private fun CheckpointCard(
    checkpoint: CheckpointView,
    busy: Boolean,
    onConfirm: (CheckpointGesture) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.cardTight)
            .background(colors.surfaceControl)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.cardTight)
            .padding(ForgeSpace.lg),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        Text(
            checkpoint.seq?.let { stringResource(R.string.checkpoints_seq, it) }
                ?: stringResource(R.string.checkpoints_seq_absent),
            style = type.sessionTitle,
            color = colors.text,
        )
        CategoryLine(stringResource(R.string.checkpoints_kind), checkpoint.kind)
        CategoryLine(stringResource(R.string.checkpoints_origin), checkpoint.origin)
        Text(
            stringResource(R.string.checkpoints_paths),
            style = type.sessionMeta,
            color = colors.textMuted,
        )
        if (checkpoint.paths.isEmpty()) {
            Notice(stringResource(R.string.checkpoints_paths_absent), colors.textMuted)
        } else {
            checkpoint.paths.forEach { path ->
                Text(
                    path.truncatedReason?.let {
                        stringResource(R.string.checkpoints_path_truncated, path.path, it)
                    } ?: path.path,
                    // mono: a workspace-relative file path, the same face the
                    // transcript gives a path in tool output.
                    style = type.toolRow,
                    color = if (path.truncated) colors.amber else colors.chatText,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        val target = checkpoint.checkpointId
        if (target == null) {
            Notice(stringResource(R.string.checkpoints_no_id), colors.textMuted)
        } else {
            Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                ForgeButton(
                    text = stringResource(R.string.checkpoints_undo),
                    onClick = { onConfirm(CheckpointGesture.Undo(target)) },
                    kind = ForgeButtonKind.Ghost,
                    enabled = !busy,
                )
                ForgeButton(
                    text = stringResource(R.string.checkpoints_redo),
                    onClick = { onConfirm(CheckpointGesture.Redo(target)) },
                    kind = ForgeButtonKind.Ghost,
                    enabled = !busy,
                )
            }
        }
    }
}

/**
 * A published category, and whether this client knows it.
 *
 * An unrecognised word is shown as the daemon wrote it, marked, rather than
 * hidden or mapped onto a neighbour that means something else.
 */
@Composable
private fun CategoryLine(label: String, category: CheckpointCategory) {
    val colors = Forge.colors
    val type = Forge.type
    val raw = category.raw
    Row(Modifier.fillMaxWidth()) {
        Text(
            label,
            style = type.sessionMeta,
            color = colors.textMuted,
            modifier = Modifier.padding(end = ForgeSpace.md),
        )
        Text(
            when {
                raw == null -> stringResource(R.string.checkpoints_absent)
                category.recognized -> raw
                else -> stringResource(R.string.checkpoints_unrecognised, raw, label.lowercase())
            },
            style = type.sessionMeta,
            color = if (raw != null && !category.recognized) colors.amber else colors.chatText,
        )
    }
}

/**
 * A digest or a path, on one horizontally scrollable line.
 *
 * A blake3 digest does not wrap into anything readable and must not stretch the
 * sheet, so it scrolls inside its own row (UI-SPEC: wide content scrolls, the
 * surface does not).
 */
@Composable
private fun DigestRow(label: String, value: String?) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.contextRow),
    ) {
        Text(
            label,
            style = type.sessionMeta,
            color = colors.textMuted,
            modifier = Modifier.padding(end = ForgeSpace.md),
        )
        Text(
            value ?: stringResource(R.string.checkpoints_absent),
            // mono: a content digest, read character by character when two are
            // being compared.
            style = type.numeric,
            color = colors.chatText,
            maxLines = 1,
            modifier = Modifier.horizontalScroll(rememberScrollState()),
        )
    }
}

@Composable
private fun Notice(text: String, color: Color) {
    Text(text, style = Forge.type.sessionMeta, color = color)
}
