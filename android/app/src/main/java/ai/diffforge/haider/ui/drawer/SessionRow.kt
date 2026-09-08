package ai.diffforge.haider.ui.drawer

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.SessionGlyph
import ai.diffforge.haider.ui.components.motionEnabled
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.daemon.SessionVisualStateFold
import ai.diffforge.haider.ui.state.ModelNames
import ai.diffforge.haider.ui.state.RelativeTime
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
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

enum class SessionRowAction { Rename, Fork, CopyId }

/**
 * One line: the provider glyph and the title. That is the whole row.
 *
 * Addition E amends UI-SPEC 6.3.5: the model, the effort, the state pill, the
 * relative time and the third line are gone, because every one of them is
 * repeated on the surface the row opens. What is left is the two things a rail
 * is for — which agent, and which conversation — and the desktop rail proves
 * the point (`SessionsRail.jsx`: "a brand-dot icon strip").
 *
 * State is carried by the glyph's accent and, for anyone not looking at
 * colour, by the merged `contentDescription`, which still speaks the state
 * word and the expanded time. Selection is an outline rather than a fill, so
 * the row that is open reads as a container rather than a highlight.
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
    val stateWord = stateWord(row.state)
    val spoken = buildList {
        add(displayTitle(row))
        stateWord?.let(::add)
        ModelNames.short(row.model).takeIf { it.isNotBlank() }?.let(::add)
        RelativeTime.spoken(row.lastActivityMs, nowMs).takeIf { it.isNotBlank() }?.let(::add)
        if (row.unseen && !selected) add(stringResource(R.string.cd_new_activity))
    }.joinToString(", ")

    val actions = buildList {
        add(CustomAccessibilityAction(stringResource(R.string.action_rename)) {
            onAction(SessionRowAction.Rename); true
        })
        add(CustomAccessibilityAction(stringResource(R.string.action_fork)) {
            onAction(SessionRowAction.Fork); true
        })
        // No Stop custom action: TalkBack must not offer a second turn Stop
        // that the screen does not have (E2, verify-6 O2).
        add(CustomAccessibilityAction(stringResource(R.string.action_copy_session_id)) {
            onAction(SessionRowAction.CopyId); true
        })
    }

    Row(
        modifier = modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.row)
            .border(
                ForgeSize.hairline,
                if (selected) colors.accent else Color.Transparent,
                ForgeShapes.row,
            )
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .semantics(mergeDescendants = true) {
                contentDescription = spoken
                stateWord?.let { stateDescription = it }
                this.selected = selected
                customActions = actions
            }
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.sm),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        SessionGlyph(
            provider = row.provider,
            state = row.state,
            animate = SessionVisualStateFold.animates(row.state) && motionEnabled(),
        )
        Text(
            displayTitle(row),
            style = type.sessionTitle,
            color = if (selected) colors.text else colors.chatText,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(start = ForgeSpace.lg),
        )
    }
}

/** An untitled session is "New session", never a raw id in the face. */
@Composable
fun displayTitle(row: SessionRow): String = when {
    !row.title.isNullOrBlank() -> row.title
    else -> stringResource(R.string.header_new_session)
}

@Composable
private fun stateWord(state: SessionVisualState): String? = when (state) {
    SessionVisualState.Running -> stringResource(R.string.state_running)
    SessionVisualState.NeedsInput -> stringResource(R.string.state_needs_input)
    SessionVisualState.Errored -> stringResource(R.string.state_errored)
    SessionVisualState.WaitingForNetwork -> stringResource(R.string.state_waiting_network)
    SessionVisualState.Idle -> stringResource(R.string.state_idle)
    SessionVisualState.Unknown -> stringResource(R.string.state_unknown)
}
