package ai.diffforge.haider.ui.drawer

import ai.diffforge.haider.R
import ai.diffforge.haider.daemon.SessionRow
import ai.diffforge.haider.daemon.SessionVisualState
import ai.diffforge.haider.daemon.SessionVisualStateFold
import ai.diffforge.haider.ui.components.ForkedPill
import ai.diffforge.haider.ui.components.RunningDots
import ai.diffforge.haider.ui.components.StateDot
import ai.diffforge.haider.ui.components.StatePill
import ai.diffforge.haider.ui.components.motionEnabled
import ai.diffforge.haider.ui.state.ModelNames
import ai.diffforge.haider.ui.state.RelativeTime
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.CustomAccessibilityAction
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.customActions
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.style.TextOverflow

enum class SessionRowAction { Rename, Fork, StopTurn, CopyId, Delete }

/**
 * The drawer's centrepiece: a three-part row (mark · title · meta), as the
 * desktop rail states its own contract (SessionsRail.jsx:821-822).
 *
 * Every state signal is redundant across three channels — motion, colour and a
 * word — because colour is never the only signal (SessionsRail.jsx:684-685).
 * An errored row carries no motion at all: nothing pulses for a corpse
 * (haider-tui/src/render.rs:1129-1144).
 *
 * The whole row is one merged accessibility node, and the long-press actions are
 * also exposed as `customActions`, so nothing needs a long-press gesture to be
 * reachable.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun SessionRowItem(
    row: SessionRow,
    selected: Boolean,
    nowMs: Long,
    onClick: () -> Unit,
    onLongClick: () -> Unit,
    onAction: (SessionRowAction) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    val rail = railColor(row.state, selected)
    val stateWord = stateWord(row.state)
    val time = RelativeTime.format(row.lastActivityMs, nowMs)
    val spokenTime = RelativeTime.spoken(row.lastActivityMs, nowMs)
    val newActivity = stringResource(R.string.cd_new_activity)
    val third = thirdLine(row)

    val spoken = buildList {
        add(displayTitle(row))
        stateWord?.let(::add)
        ModelNames.short(row.model).takeIf { it.isNotBlank() }?.let(::add)
        spokenTime.takeIf { it.isNotBlank() }?.let(::add)
        if (row.unseen && !selected) add(newActivity)
    }.joinToString(", ")

    val actions = buildList {
        add(CustomAccessibilityAction(stringResource(R.string.action_rename)) {
            onAction(SessionRowAction.Rename); true
        })
        add(CustomAccessibilityAction(stringResource(R.string.action_fork)) {
            onAction(SessionRowAction.Fork); true
        })
        if (row.runId != null) {
            add(CustomAccessibilityAction(stringResource(R.string.action_stop_turn)) {
                onAction(SessionRowAction.StopTurn); true
            })
        }
        add(CustomAccessibilityAction(stringResource(R.string.action_delete)) {
            onAction(SessionRowAction.Delete); true
        })
    }

    Row(
        modifier = modifier
            .fillMaxWidth()
            .heightIn(min = if (third == null) ForgeSize.rowMin else ForgeSize.rowMinThreeLine)
            .clip(ForgeShapes.row)
            .background(if (selected) colors.surfaceSelected else Color.Transparent)
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .semantics(mergeDescendants = true) {
                contentDescription = spoken
                stateWord?.let { stateDescription = it }
                this.selected = selected
                customActions = actions
            }
            .padding(end = ForgeSpace.lg),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        // 3 dp rail: absent for idle and for a state we cannot vouch for.
        Box(
            Modifier
                .width(ForgeSize.rail)
                .heightIn(min = ForgeSize.rowMin - ForgeSpace.xl)
                .clip(ForgeShapes.pill)
                .background(rail ?: Color.Transparent),
        )
        Column(
            modifier = Modifier
                .weight(1f)
                .padding(start = ForgeSpace.lg, top = ForgeSpace.md, bottom = ForgeSpace.md),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    displayTitle(row),
                    style = type.sessionTitle,
                    color = colors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (row.unseen && !selected) {
                    StateDot(colors.accent, Modifier.padding(start = ForgeSpace.md))
                }
                if (time.isNotEmpty()) {
                    Text(
                        time,
                        style = type.sessionMeta,
                        color = colors.textMuted,
                        modifier = Modifier.padding(start = ForgeSpace.md),
                    )
                }
            }
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
            ) {
                if (stateWord != null && rail != null) {
                    StatePill(stateWord.uppercase(), rail)
                } else {
                    StateDot(colors.stateIdle)
                    Text(
                        stringResource(R.string.state_idle),
                        style = type.sessionMeta,
                        color = colors.textMuted,
                    )
                }
                val meta = listOfNotNull(
                    ModelNames.short(row.model).takeIf { it.isNotBlank() },
                    row.effort,
                ).joinToString(" · ")
                if (meta.isNotEmpty()) {
                    Text(
                        meta,
                        style = type.sessionMeta,
                        color = colors.textMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f, fill = false),
                    )
                }
                if (row.forkedFrom != null) ForkedPill()
            }
            if (third != null) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
                ) {
                    if (row.state == SessionVisualState.Running) {
                        RunningDots(
                            color = colors.stateRunning,
                            animate = SessionVisualStateFold.animates(row.state) && motionEnabled(),
                            modifier = Modifier.size(width = ForgeSpace.xl, height = ForgeSpace.xs),
                        )
                    }
                    Text(
                        third,
                        style = type.sessionMeta,
                        color = colors.textMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
        }
    }
}

@Composable
fun displayTitle(row: SessionRow): String = when {
    !row.title.isNullOrBlank() -> row.title
    else -> stringResource(R.string.header_session_fallback, row.id.take(6))
}

@Composable
private fun stateWord(state: SessionVisualState): String? = when (state) {
    SessionVisualState.Running -> stringResource(R.string.state_running)
    SessionVisualState.NeedsInput -> stringResource(R.string.state_needs_input)
    SessionVisualState.Errored -> stringResource(R.string.state_errored)
    SessionVisualState.WaitingForNetwork -> stringResource(R.string.state_waiting_network)
    SessionVisualState.Idle, SessionVisualState.Unknown -> null
}

@Composable
private fun railColor(state: SessionVisualState, selected: Boolean): Color? {
    val colors = Forge.colors
    if (selected) return colors.accent
    if (!SessionVisualStateFold.rendersRail(state)) return null
    return when (state) {
        SessionVisualState.Running -> colors.stateRunning
        SessionVisualState.NeedsInput -> colors.stateNeedsInput
        SessionVisualState.Errored -> colors.stateErrored
        SessionVisualState.WaitingForNetwork -> colors.amber
        else -> null
    }
}

/** Present only while running or needing input. */
private fun thirdLine(row: SessionRow): String? = when {
    row.needsInput != null -> row.needsInput.displayTitle.ifBlank { null }
    row.state == SessionVisualState.Running -> row.workspaceCwd ?: row.agentType ?: "working"
    else -> null
}
