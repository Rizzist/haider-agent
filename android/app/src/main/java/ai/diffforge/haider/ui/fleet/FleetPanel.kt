package ai.diffforge.haider.ui.fleet

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.components.StateDot
import ai.diffforge.haider.ui.daemon.FleetCompleteness
import ai.diffforge.haider.ui.daemon.FleetActivity
import android.os.SystemClock
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.delay
import ai.diffforge.haider.ui.daemon.FleetLoad
import ai.diffforge.haider.ui.daemon.FleetModel
import ai.diffforge.haider.ui.daemon.FleetNode
import ai.diffforge.haider.ui.daemon.FleetSnapshot
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ChevronRight
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material.icons.rounded.Subject
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
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

/** One panel row; the tag carries the agent id it was drawn for. */
fun fleetRowTag(agentId: String): String = "fleet_row_$agentId"

/** 973-activity-icons: the activity glyph beside a fleet row's name. */
fun fleetActivityTag(agentId: String): String = "fleet_activity_$agentId"

const val FLEET_PANEL_TAG = "fleet_panel"

/**
 * One listed descendant, with the parent session it belongs to.
 *
 * The pair is inseparable: a `session.fleet` snapshot is only ever true of the
 * session it was requested for, so a row that lost its parent coordinate could
 * not be addressed, jumped to, or attributed (frame.rs:2395).
 */
data class FleetEntry(
    val parentSessionId: String,
    val parentTitle: String?,
    val node: FleetNode,
    /** The snapshot's daemon-clock instant (activity decay reference). */
    val generatedAtMs: Long? = null,
    val receivedAtElapsedMs: Long,
)

/** Pure selection over the panel's reads, so the list itself can be pinned. */
object FleetPanelModel {

    /**
     * The active fleet: queued, live and waiting, in the daemon's own tree
     * order, across every session that was read.
     *
     * Terminal children are counted, not listed — [finished] reports them —
     * because a panel meant for "what is running right now" that fills with
     * yesterday's finished agents stops answering that question.
     */
    fun active(
        panel: Map<String, FleetLoad>,
        titles: Map<String, String?> = emptyMap(),
    ): List<FleetEntry> = panel.entries
        .mapNotNull { (sessionId, load) ->
            (load as? FleetLoad.Snapshot)?.let { sessionId to it }
        }
        .flatMap { (sessionId, load) ->
            val snapshot = load.snapshot
            FleetModel.flatten(snapshot.roots)
                .filter { it.state.active }
                .map {
                    FleetEntry(sessionId, titles[sessionId], it, snapshot.generatedAtMs, load.receivedAtElapsedMs)
                }
        }

    fun finished(panel: Map<String, FleetLoad>): Int = panel.values
        .mapNotNull { (it as? FleetLoad.Snapshot)?.snapshot }
        .sumOf { snapshot -> FleetModel.flatten(snapshot.roots).count { it.state.terminal } }

    /** Children the daemon said it did not return, summed over the reads. */
    fun folded(panel: Map<String, FleetLoad>): Int = panel.values
        .mapNotNull { (it as? FleetLoad.Snapshot)?.snapshot }
        .sumOf { snapshot -> FleetModel.flatten(snapshot.roots).sumOf { it.foldedChildren } }

    /** Every read that came back bounded, so the panel can say so once. */
    fun bounded(panel: Map<String, FleetLoad>): List<FleetSnapshot> = panel.values
        .mapNotNull { (it as? FleetLoad.Snapshot)?.snapshot }
        .filter { FleetModel.completeness(it) == FleetCompleteness.Bounded }

    /** Reads the daemon refused, with its own reason, one line each. */
    fun unavailable(panel: Map<String, FleetLoad>): List<Pair<String, String>> =
        panel.entries.mapNotNull { (sessionId, load) ->
            when (load) {
                is FleetLoad.Unavailable -> sessionId to load.reason
                is FleetLoad.Failed -> sessionId to load.reason
                else -> null
            }
        }
}

/** The sheet. Its content is a separate composable so a golden can host it. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun FleetSheet(
    panel: Map<String, FleetLoad>,
    sessions: List<SessionRow>,
    loading: Boolean,
    onDismiss: () -> Unit,
    onJump: (String) -> Unit,
    onOpenChild: (sessionId: String, agentId: String, parentSessionId: String) -> Unit,
    onRefresh: () -> Unit,
) {
    val colors = Forge.colors
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        FleetPanelContent(
            panel = panel,
            sessions = sessions,
            loading = loading,
            onJump = onJump,
            onOpenChild = onOpenChild,
            onRefresh = onRefresh,
            modifier = Modifier.padding(bottom = ForgeSpace.xxxl),
        )
    }
}

/**
 * The cross-session subagent list.
 *
 * Every honesty rule the desktop panel states renders here: an unread session
 * and a session the daemon says has no subagents are different lines; a bounded
 * snapshot is labelled bounded and never presented as the complete tree; the
 * exact number of children the daemon folded away is shown rather than implied;
 * and a session whose fleet read was refused names the daemon's own reason
 * instead of quietly contributing nothing.
 */
@Composable
fun FleetPanelContent(
    panel: Map<String, FleetLoad>,
    sessions: List<SessionRow>,
    loading: Boolean,
    onJump: (String) -> Unit,
    onOpenChild: (sessionId: String, agentId: String, parentSessionId: String) -> Unit,
    onRefresh: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    val titles = remember(sessions) { sessions.associate { it.id to it.title } }
    val entries = remember(panel, titles) { FleetPanelModel.active(panel, titles) }
    val finished = remember(panel) { FleetPanelModel.finished(panel) }
    val folded = remember(panel) { FleetPanelModel.folded(panel) }
    val bounded = remember(panel) { FleetPanelModel.bounded(panel) }
    val refused = remember(panel) { FleetPanelModel.unavailable(panel) }
    // 973-activity-icons: map the phone's clock onto the daemon's via the
    // snapshot instant, and recompose only at the next instant an icon can
    // change (hot-window / linger boundary) — no polling loop.
    var tick by remember { mutableLongStateOf(0L) }
    val daemonNow: (FleetEntry) -> Long? = { entry ->
        entry.generatedAtMs?.let {
            it + (SystemClock.elapsedRealtime() - entry.receivedAtElapsedMs).coerceAtLeast(0L)
        }
    }
    val nextChange = entries.mapNotNull { entry ->
        val now = daemonNow(entry) ?: return@mapNotNull null
        entry.node.metrics?.activity?.nextChangeAt(now)?.let { it - now }
    }.minOrNull()
    LaunchedEffect(panel, tick, nextChange) {
        if (nextChange != null) {
            delay(nextChange.coerceAtLeast(1L))
            tick++
        }
    }
    val iconsOn = ActivityIconPreference.current()
    // Re-evaluated on every recomposition `tick` triggers.
    val activities: Map<String, FleetActivity?> = entries.associate { entry ->
        "${entry.parentSessionId}:${entry.node.agentId}" to
            if (iconsOn && tick >= 0L) entry.node.activityAt(daemonNow(entry)) else null
    }

    Column(
        modifier = modifier
            .testTag(FLEET_PANEL_TAG)
            .fillMaxWidth()
            .padding(horizontal = ForgeSpace.xl),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().heightIn(min = ForgeSize.touch),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                stringResource(R.string.fleet_title),
                style = type.h4,
                color = colors.text,
                modifier = Modifier.weight(1f),
            )
            ForgeIconButton(
                onClick = onRefresh,
                contentDescription = stringResource(R.string.cd_fleet_refresh),
                visual = ForgeSize.headerControl,
            ) {
                Icon(
                    Icons.Rounded.Refresh,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
        }

        when {
            loading -> Muted(stringResource(R.string.fleet_reading))
            panel.isEmpty() -> Muted(stringResource(R.string.fleet_unread))
            entries.isEmpty() && refused.isEmpty() -> Muted(stringResource(R.string.fleet_empty))
        }

        if (entries.isNotEmpty()) {
            LazyColumn(
                modifier = Modifier.fillMaxWidth().heightIn(max = ForgeSize.fleetListMax),
            ) {
                items(entries, key = { "${it.parentSessionId}:${it.node.agentId}" }) { entry ->
                    FleetEntryRow(
                        entry = entry,
                        activity = activities["${entry.parentSessionId}:${entry.node.agentId}"],
                        onJump = onJump,
                        onOpenChild = onOpenChild,
                    )
                }
            }
        }

        bounded.forEach { snapshot ->
            Warned(
                stringResource(
                    R.string.fleet_bounded,
                    snapshot.nodeLimit?.toString() ?: stringResource(R.string.fleet_no_data),
                    snapshot.depthLimit?.toString() ?: stringResource(R.string.fleet_no_data),
                ),
            )
        }
        if (folded > 0) Warned(stringResource(R.string.fleet_folded, folded))
        if (finished > 0) Muted(stringResource(R.string.fleet_finished_hidden, finished))
        refused.forEach { (sessionId, reason) ->
            Warned(stringResource(R.string.fleet_session_unavailable, sessionId, reason))
        }
    }
}

/**
 * One child.
 *
 * The row itself jumps to the child's own session — the panel's whole point is
 * getting to the agent that needs you — and the trailing control opens the
 * read-only transcript beside its parent instead.
 */
@Composable
private fun FleetEntryRow(
    entry: FleetEntry,
    activity: FleetActivity?,
    onJump: (String) -> Unit,
    onOpenChild: (String, String, String) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val node = entry.node
    val label = FleetModel.label(node)
    val stateWord = FleetTone.label(node.state)
    val parent = entry.parentTitle ?: entry.parentSessionId
    val jumpable = node.sessionId.isNotEmpty()
    val base = stringResource(
        R.string.cd_fleet_jump,
        label.text,
        node.task,
        stateWord,
        parent,
    )
    val spoken = if (activity == null) {
        base
    } else {
        stringResource(R.string.cd_fleet_activity, base, stringResource(activity.description))
    }
    Row(
        modifier = Modifier.fillMaxWidth().heightIn(min = ForgeSize.touch),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(
            modifier = Modifier
                .testTag(fleetRowTag(node.agentId))
                .weight(1f)
                .heightIn(min = ForgeSize.touch)
                .clip(ForgeShapes.cardTight)
                .clickable(enabled = jumpable) { onJump(node.sessionId) }
                .semantics {
                    contentDescription = spoken
                    role = Role.Button
                }
                .padding(horizontal = ForgeSpace.sm),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            StateDot(FleetTone.color(node.state))
            Column(Modifier.weight(1f)) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
                ) {
                    // 973-activity-icons: the activity emoji LEFT of the name
                    // (activity, not state — the StateDot keeps state).
                    if (activity != null) {
                        Text(
                            activity.emoji,
                            style = type.sessionTitle,
                            maxLines = 1,
                            modifier = Modifier.testTag(fleetActivityTag(node.agentId)),
                        )
                    }
                    if (label.fallback) {
                        // mono: the agent id, standing in for a callsign the
                        // daemon has not assigned — an identifier, marked as one.
                        Text(
                            stringResource(R.string.fleet_id_fallback, label.text),
                            style = type.numeric,
                            color = colors.textMuted,
                            maxLines = 1,
                        )
                    } else {
                        Text(
                            label.text,
                            style = type.sessionTitle,
                            color = colors.text,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    Text(stateWord, style = type.chip, color = colors.textMuted, maxLines = 1)
                }
                Text(
                    node.task,
                    style = type.sessionMeta,
                    color = colors.textSoft,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    stringResource(R.string.fleet_under_parent, parent),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (jumpable) {
                Icon(
                    Icons.Rounded.ChevronRight,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconXs),
                )
            }
        }
        if (jumpable) {
            ForgeIconButton(
                // The parent is the session this snapshot was read for, never
                // the session the panel happens to be open over.
                onClick = { onOpenChild(node.sessionId, node.agentId, entry.parentSessionId) },
                contentDescription = stringResource(
                    R.string.cd_fleet_open_transcript,
                    label.text,
                ),
                visual = ForgeSize.headerControl,
            ) {
                Icon(
                    Icons.Rounded.Subject,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
        }
    }
}

@Composable
private fun Muted(text: String) {
    Text(
        text,
        style = Forge.type.sessionMeta,
        color = Forge.colors.textMuted,
        modifier = Modifier.padding(vertical = ForgeSpace.md),
    )
}

@Composable
private fun Warned(text: String) {
    val colors = Forge.colors
    Box(
        Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.cardTight)
            .background(colors.surfaceControl)
            .padding(horizontal = ForgeSpace.md, vertical = ForgeSpace.sm),
    ) {
        Text(text, style = Forge.type.sessionMeta, color = colors.amber)
    }
}
