package ai.diffforge.haider.ui.drawer

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.StateDot
import ai.diffforge.haider.ui.fleet.FleetTone
import ai.diffforge.haider.ui.state.SessionTree
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ExpandLess
import androidx.compose.material.icons.rounded.ExpandMore
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription

/** The family expand/collapse control, so a pin can measure exactly it. */
const val FAMILY_TOGGLE_TAG = "family_toggle"

/** The indent rail a nested row draws to its parent. */
const val TREE_CONNECTOR_TAG = "tree_connector"

/**
 * One drawer line: the session row, its indent to the parent that spawned it,
 * and — for a row that spawned sessions of its own — the count and folded state
 * of what is under it.
 *
 * The whole row is still the session's own 48 dp target: the collapse control
 * is a second, adjacent 48 dp target at the end of the same line rather than a
 * row of its own, which is what round 10's R3 asked for ("the collapse/expand
 * button taking whole row, not making good use of space").
 */
@Composable
fun SessionTreeLineItem(
    line: SessionTree.Line,
    selected: Boolean,
    nowMs: Long,
    onClick: () -> Unit,
    onLongClick: () -> Unit,
    onAction: (SessionRowAction) -> Unit,
    onToggleFamily: () -> Unit,
    modifier: Modifier = Modifier,
    /** False while the drawer is shut: its rows must not animate (round 12). */
    visible: Boolean = true,
) {
    Row(
        modifier = modifier.fillMaxWidth().height(ForgeSize.touch),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (line.depth > 0) {
            TreeConnector(depth = line.depth, lastChild = line.lastChild)
        }
        SessionRowItem(
            row = line.row,
            selected = selected,
            nowMs = nowMs,
            onClick = onClick,
            onLongClick = onLongClick,
            onAction = onAction,
            modifier = Modifier.weight(1f),
            visible = visible,
        )
        line.summary?.let { summary ->
            FamilyToggle(
                row = line.row,
                summary = summary,
                expanded = line.expanded,
                onToggle = onToggleFamily,
            )
        }
    }
}

/**
 * The lineage rail. It is drawn, not typed: an ASCII elbow would land on the
 * monospace ramp and read as tool output.
 */
@Composable
private fun TreeConnector(depth: Int, lastChild: Boolean) {
    val colors = Forge.colors
    val density = LocalDensity.current
    val stroke = with(density) { ForgeSize.hairline.toPx() }
    val step = with(density) { ForgeSize.treeIndent.toPx() }
    val elbow = with(density) { ForgeSize.treeElbow.toPx() }
    Box(
        Modifier
            .testTag(TREE_CONNECTOR_TAG)
            .width(ForgeSize.treeIndent * depth)
            .fillMaxHeight()
            .drawBehind {
                val x = step * depth - step / 2f
                val midY = size.height / 2f
                drawLine(
                    color = colors.border,
                    start = Offset(x, 0f),
                    // A last child's rail stops at the elbow: nothing continues
                    // below it, and a rail that ran on would promise a sibling
                    // the daemon did not publish.
                    end = Offset(x, if (lastChild) midY else size.height),
                    strokeWidth = stroke,
                )
                drawLine(
                    color = colors.border,
                    start = Offset(x, midY),
                    end = Offset(x + elbow, midY),
                    strokeWidth = stroke,
                )
            },
    )
}

/**
 * The count and the aggregate dot, in one 48 dp target that folds the family.
 *
 * The dot is the *strongest* descendant state, so a collapsed family cannot
 * hide a child that is asking for a human; the number and the state word both
 * ride the content description, so the signal is never colour alone.
 */
@Composable
private fun FamilyToggle(
    row: ai.diffforge.haider.ui.daemon.SessionRow,
    summary: SessionTree.Summary,
    expanded: Boolean,
    onToggle: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val title = displayTitle(row)
    val stateWord = summary.aggregate?.let { FleetTone.word(it) }
    val label = if (expanded) {
        stringResource(R.string.cd_collapse_family, summary.descendants, title)
    } else {
        stringResource(R.string.cd_expand_family, summary.descendants, title)
    }
    Box(
        modifier = Modifier
            .testTag(FAMILY_TOGGLE_TAG)
            .size(ForgeSize.touch)
            .clickable(onClick = onToggle)
            .semantics {
                contentDescription = label
                this.role = Role.Button
                stateWord?.let { stateDescription = it }
            },
        contentAlignment = Alignment.Center,
    ) {
        Row(
            modifier = Modifier
                .height(ForgeSize.familyPill)
                .clip(ForgeShapes.pill)
                .background(colors.surfaceControl)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.pill)
                .padding(horizontal = ForgeSpace.sm),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xxs),
        ) {
            summary.aggregate?.let { StateDot(FleetTone.color(it)) }
            Text(
                summary.descendants.toString(),
                style = type.chip,
                color = colors.textSoft,
            )
            Icon(
                if (expanded) Icons.Rounded.ExpandLess else Icons.Rounded.ExpandMore,
                contentDescription = null,
                tint = colors.textMuted,
                modifier = Modifier.size(ForgeSize.iconXs),
            )
        }
    }
}
