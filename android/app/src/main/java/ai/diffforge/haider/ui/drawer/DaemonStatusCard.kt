package ai.diffforge.haider.ui.drawer

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.components.StateDot
import ai.diffforge.haider.ui.state.RelativeTime
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.requiredHeightIn
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.PlayArrow
import androidx.compose.material.icons.rounded.Stop
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow

/**
 * Daemon state, phrase and one trailing button, plus a resource line that
 * degrades segment by segment.
 *
 * `status.snapshot` carries no uptime and no memory number — verified absent
 * across haider-cli, haider-rpc and haider-client — so those two segments are
 * Android-owned (service start time, `Debug.MemoryInfo`), and any segment whose
 * source is unknown is *omitted* rather than printed as `0`
 * (UI-SPEC 3.3.2, traps 6.6.5 and 6.6.7).
 */
/**
 * One line: a dot, a word, and a single icon button (addition F, D2).
 *
 * The card used to carry the whole resource line — sessions, active turns,
 * megabytes, uptime — in a drawer whose job is to list sessions. Those numbers
 * are diagnostics, so they moved to Settings -> Daemon, where somebody looking
 * for them will actually look.
 */
@Composable
fun DaemonStatusRow(
    status: DaemonStatus,
    onStart: () -> Unit,
    onStop: () -> Unit,
    onOpenDetails: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = modifier
            .fillMaxWidth()
            .requiredHeightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.row)
            .clickable(onClick = onOpenDetails)
            .padding(horizontal = ForgeSpace.md)
            .semantics { liveRegion = LiveRegionMode.Polite },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        StateDot(
            when (status) {
                is DaemonStatus.Running -> colors.green
                DaemonStatus.Starting, DaemonStatus.Restarting, DaemonStatus.Stopping -> colors.amber
                DaemonStatus.Stopped -> colors.textMuted
                is DaemonStatus.Failed -> colors.red
            },
        )
        Text(
            phrase(status),
            style = type.chip,
            color = colors.text,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier
                .weight(1f)
                .padding(start = ForgeSpace.md),
        )
        when (status) {
            is DaemonStatus.Running -> ForgeIconButton(
                onClick = onStop,
                contentDescription = stringResource(R.string.daemon_action_stop),
                visual = ForgeSize.headerControl,
            ) {
                Icon(
                    Icons.Rounded.Stop,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
            DaemonStatus.Stopped, is DaemonStatus.Failed -> ForgeIconButton(
                onClick = onStart,
                contentDescription = stringResource(R.string.daemon_action_start),
                background = colors.accentWash,
                visual = ForgeSize.headerControl,
            ) {
                Icon(
                    Icons.Rounded.PlayArrow,
                    contentDescription = null,
                    tint = colors.accent,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
            DaemonStatus.Starting, DaemonStatus.Restarting, DaemonStatus.Stopping ->
                CircularProgressIndicator(
                    modifier = Modifier.size(ForgeSize.iconSm),
                    color = colors.amber,
                )
        }
    }
}

@Composable
private fun phrase(status: DaemonStatus): String = when (status) {
    is DaemonStatus.Running -> stringResource(R.string.daemon_running)
    DaemonStatus.Starting -> stringResource(R.string.daemon_starting)
    DaemonStatus.Restarting -> stringResource(R.string.daemon_restarting)
    DaemonStatus.Stopping -> stringResource(R.string.daemon_stopping)
    DaemonStatus.Stopped -> stringResource(R.string.daemon_stopped)
    is DaemonStatus.Failed -> stringResource(R.string.daemon_failed, status.reason)
}

/**
 * Each segment appears only when its source is known. Settings owns this line
 * now; the drawer shows the state word alone (addition F, D2/T1).
 */
@Composable
fun resourceLine(
    running: DaemonStatus.Running?,
    activeTurns: Int,
    elapsedRealtimeMs: Long,
): String {
    val info = running?.info ?: return ""
    val segments = buildList {
        info.sessionCount?.let {
            add(pluralStringResource(R.plurals.daemon_sessions_segment, it.toInt(), it.toInt()))
        }
        if (activeTurns > 0) {
            add(pluralStringResource(R.plurals.daemon_turns_segment, activeTurns, activeTurns))
        }
        info.pssBytes?.let { add(stringResource(R.string.daemon_memory_segment, (it / (1024 * 1024)).toString())) }
        RelativeTime.duration(info.startedAtElapsedRealtimeMs, elapsedRealtimeMs)
            .takeIf { it.isNotEmpty() }
            ?.let { add(stringResource(R.string.daemon_uptime_segment, it)) }
    }
    return segments.joinToString(" · ")
}
