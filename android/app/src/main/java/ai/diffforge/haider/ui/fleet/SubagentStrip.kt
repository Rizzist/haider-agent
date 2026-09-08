package ai.diffforge.haider.ui.fleet

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.BrandMarkOnly
import ai.diffforge.haider.ui.components.StateDot
import ai.diffforge.haider.ui.daemon.FleetLoad
import ai.diffforge.haider.ui.daemon.FleetModel
import ai.diffforge.haider.ui.daemon.Subagent
import ai.diffforge.haider.ui.daemon.SubagentLoad
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Hub
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow

/** The strip itself, so a pin can find it without matching on a label. */
const val SUBAGENT_STRIP_TAG = "subagent_strip"

/** One chip; the tag carries the agent id the chip was drawn for. */
fun subagentChipTag(agentId: String): String = "subagent_chip_$agentId"

/**
 * The session header's subagent chips — `session.observe`'s `subagents`
 * (frame.rs:2271), which is the daemon's own persisted chip state.
 *
 * A chip is tappable, and what it opens depends on what the daemon has
 * published: `ObserveSubagentWire` carries **no session id**, so the child's
 * own transcript is reachable only once a `session.fleet` snapshot has paired
 * the agent with a session. Until then the chip opens the fleet panel instead
 * of guessing a session id from the parent it is sitting on.
 *
 * The strip renders nothing at all when the daemon has published no subagents:
 * an empty rail above every ordinary conversation would be a permanent piece of
 * furniture for a feature most sessions never use.
 */
@Composable
fun SubagentStrip(
    load: SubagentLoad,
    fleet: FleetLoad,
    onOpenChild: (sessionId: String, agentId: String) -> Unit,
    onOpenFleet: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val subagents = load.subagents
    if (subagents.isEmpty()) return
    val roots = (fleet as? FleetLoad.Snapshot)?.snapshot?.roots.orEmpty()
    val openFleetLabel = stringResource(R.string.cd_open_fleet, subagents.size)

    Row(
        modifier = modifier
            .testTag(SUBAGENT_STRIP_TAG)
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .padding(horizontal = ForgeSpace.xl),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
    ) {
        // Fixed, and first. Round 1 put it at the end of the scrolling row and
        // two chips were enough to push the only way into the panel off a
        // 412 dp screen — a door nobody can reach is not a door.
        Box(
            modifier = Modifier
                .size(ForgeSize.touch)
                .clickable(onClick = onOpenFleet)
                .semantics {
                    contentDescription = openFleetLabel
                    role = Role.Button
                },
            contentAlignment = Alignment.Center,
        ) {
            Box(
                modifier = Modifier
                    .size(ForgeSize.subagentChip)
                    .clip(ForgeShapes.pill)
                    .background(colors.surfaceControl)
                    .border(ForgeSize.hairline, colors.border, ForgeShapes.pill),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    Icons.Rounded.Hub,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconXs),
                )
            }
        }
        Row(
            modifier = Modifier
                .weight(1f)
                .horizontalScroll(rememberScrollState()),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
        ) {
            subagents.forEach { subagent ->
                SubagentChip(
                    subagent = subagent,
                    // Null means the pairing has not been published; the chip
                    // then takes the honest route rather than a fabricated
                    // session id.
                    childSessionId = FleetModel.sessionOf(roots, subagent.agentId),
                    onOpenChild = onOpenChild,
                    onOpenFleet = onOpenFleet,
                )
            }
        }
    }
}

@Composable
private fun SubagentChip(
    subagent: Subagent,
    childSessionId: String?,
    onOpenChild: (String, String) -> Unit,
    onOpenFleet: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val label = FleetModel.label(subagent)
    val stateWord = FleetTone.shortLabel(subagent.state)
    val spoken = if (childSessionId != null) {
        stringResource(R.string.cd_open_subagent, label.text, subagent.task, stateWord)
    } else {
        stringResource(R.string.cd_subagent_no_session, label.text, stateWord)
    }
    Box(
        modifier = Modifier
            .testTag(subagentChipTag(subagent.agentId))
            .defaultMinSize(minWidth = ForgeSize.touch, minHeight = ForgeSize.touch)
            .clickable {
                if (childSessionId != null) onOpenChild(childSessionId, subagent.agentId)
                else onOpenFleet()
            }
            .semantics {
                contentDescription = spoken
                role = Role.Button
            },
        contentAlignment = Alignment.Center,
    ) {
        Row(
            modifier = Modifier
                .height(ForgeSize.subagentChip)
                .clip(ForgeShapes.pill)
                .background(colors.surfaceControl)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.pill)
                .padding(horizontal = ForgeSpace.md),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
        ) {
            StateDot(FleetTone.color(subagent.state))
            // The provider the *child* was resolved with, which is not always
            // the parent's: an absent one draws no mark rather than the
            // session's own (frame.rs:2226).
            subagent.provider?.let { BrandMarkOnly(model = null, provider = it) }
            if (label.fallback) {
                // mono: an agent id, shown only because the daemon assigned no
                // callsign — it is an identifier, and it is marked as one.
                Text(
                    stringResource(R.string.fleet_id_fallback, label.text),
                    style = type.numeric,
                    color = colors.textMuted,
                    maxLines = 1,
                )
            } else {
                Text(label.text, style = type.chip, color = colors.text, maxLines = 1)
            }
            Text(
                subagent.task,
                style = type.chip,
                color = colors.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.widthIn(max = ForgeSize.subagentTaskMax),
            )
        }
    }
}
