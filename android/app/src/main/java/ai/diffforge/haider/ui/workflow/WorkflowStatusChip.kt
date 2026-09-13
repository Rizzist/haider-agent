package ai.diffforge.haider.ui.workflow

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.AccountTree
import androidx.compose.material.icons.rounded.ChevronRight
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow

const val WORKFLOW_CHIP_TAG = "workflow_status_chip"

/**
 * The session's workflow, as one chip.
 *
 * Display-only about the *fact*: it renders what the session's own
 * `graph.status` said and nothing else. It is not display-only as a control —
 * tapping it opens the live graph — but nothing it shows is derived from that
 * screen's separate projection, so the chip can never disagree with the roster
 * it came from.
 *
 * [WorkflowChipState.None] draws nothing at all. Most sessions have no
 * workflow, and a row of "No workflow" above every chat is chrome that says
 * nothing; the absence is only worth words on the graph screen itself, where a
 * person went looking for it.
 */
@Composable
fun WorkflowStatusChip(
    state: WorkflowChipState,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    if (state is WorkflowChipState.None) return

    val label = when (state) {
        is WorkflowChipState.Unread -> stringResource(R.string.workflow_chip_unread)
        is WorkflowChipState.Unavailable ->
            stringResource(R.string.workflow_chip_unavailable, state.reason)
        is WorkflowChipState.Active -> when {
            state.agentType != null -> stringResource(
                R.string.workflow_chip_subagent,
                state.agentType,
                state.template,
                state.phase,
            )
            state.activeNodes > 0 -> stringResource(
                R.string.workflow_chip_active_open,
                state.template,
                state.phase,
                state.activeNodes,
            )
            else -> stringResource(R.string.workflow_chip_active, state.template, state.phase)
        }
        is WorkflowChipState.None -> ""
    }
    // Only a graph that exists can be opened. An unread or unavailable chip
    // still says what it knows, but it is not a door to a screen with nothing
    // behind it.
    val openable = state is WorkflowChipState.Active
    val tone = when (state) {
        is WorkflowChipState.Active -> when (state.phase) {
            "active" -> colors.accent
            "completed" -> colors.green
            "blocked" -> colors.amber
            "abandoned", "superseded" -> colors.textMuted
            else -> colors.textSoft
        }
        else -> colors.textMuted
    }

    ForgeChip(
        onClick = onClick,
        enabled = openable,
        // Deliberately not `selected` — in paint or in semantics: this chip
        // is a door, not an option in a group. An accented pill here stacked
        // a second blue outline directly under the needs-input banner and
        // competed with it; the phase-toned mark and the chevron already say
        // "door", and the banner keeps the one accent the screen has room for.
        contentDescription = if (openable) {
            stringResource(R.string.cd_open_workflow, label)
        } else {
            label
        },
        modifier = modifier.testTag(WORKFLOW_CHIP_TAG),
    ) {
        Icon(
            Icons.Rounded.AccountTree,
            contentDescription = null,
            tint = tone,
            modifier = Modifier.size(ForgeSize.iconSm),
        )
        Text(
            label,
            style = type.chip,
            color = if (openable) colors.text else colors.textMuted,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        if (openable) {
            Icon(
                Icons.Rounded.ChevronRight,
                contentDescription = null,
                tint = colors.textMuted,
                modifier = Modifier.size(ForgeSize.iconXs),
            )
        }
    }
}
