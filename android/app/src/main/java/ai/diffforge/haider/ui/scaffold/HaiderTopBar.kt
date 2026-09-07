package ai.diffforge.haider.ui.scaffold

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.components.AttentionBadge
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.components.StateDot
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.AttentionBadgeKind
import ai.diffforge.haider.ui.state.ModelNames
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Menu
import androidx.compose.material.icons.rounded.MoreVert
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace

const val HAIDER_TOP_BAR_TAG = "haider_top_bar"

/** What the header overflow can do. */
enum class TopBarAction {
    SessionDetails,
    Rename,
    Fork,
    StopTurn,
    ClearTranscript,
    Settings,
}

/**
 * 56 dp, one hairline, and **exactly two** interactive controls, both the same
 * shape (UI-SPEC 3.1).
 *
 * 970 crammed a bordered pill next to two bordered circles in 48 dp boxes, each
 * with different padding (D1), and printed `model ?: "Diff Forge AI"` on the
 * second line — a product name standing in for data that never arrived (D2).
 * Here the second line renders only when it has something true to say, and the
 * daemon's state lives in the drawer card, the banner and the badge instead.
 */
@Composable
fun HaiderTopBar(
    state: AppUiState,
    onOpenDrawer: () -> Unit,
    onOpenSessionSheet: () -> Unit,
    onAction: (TopBarAction) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    var menuOpen by remember { mutableStateOf(false) }
    val session = state.activeSession
    val subtitle = subtitle(state, session)

    Column(modifier.testTag(HAIDER_TOP_BAR_TAG)) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.header)
                .padding(horizontal = ForgeSpace.xs),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box {
                ForgeIconButton(
                    onClick = onOpenDrawer,
                    contentDescription = drawerDescription(state),
                ) {
                    Icon(
                        Icons.Rounded.Menu,
                        contentDescription = null,
                        tint = colors.textSoft,
                        modifier = Modifier.size(ForgeSize.icon),
                    )
                }
                val badge = badgeColor(state)
                if (badge != null) {
                    AttentionBadge(
                        color = badge,
                        modifier = Modifier
                            .align(Alignment.TopEnd)
                            .padding(top = ForgeSpace.md, end = ForgeSpace.md),
                    )
                }
            }

            Column(
                modifier = Modifier
                    .weight(1f)
                    .heightIn(min = ForgeSize.touch)
                    .clickable(onClick = onOpenSessionSheet)
                    .padding(horizontal = ForgeSpace.md),
                verticalArrangement = Arrangement.Center,
            ) {
                Text(
                    title(state, session),
                    style = type.sessionTitle,
                    color = colors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                // Never a placeholder string: absent means the line is absent
                // and line 1 vertically centres.
                if (subtitle != null) {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
                    ) {
                        subtitle.dot?.let { StateDot(it) }
                        Text(
                            subtitle.text,
                            style = type.sessionMeta,
                            color = colors.textMuted,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
            }

            Box {
                ForgeIconButton(
                    onClick = { menuOpen = true },
                    contentDescription = stringResource(R.string.cd_more_options),
                ) {
                    Icon(
                        Icons.Rounded.MoreVert,
                        contentDescription = null,
                        tint = colors.textSoft,
                        modifier = Modifier.size(ForgeSize.icon),
                    )
                }
                DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                    OverflowItem(R.string.action_session_details, session != null) {
                        menuOpen = false
                        onAction(TopBarAction.SessionDetails)
                    }
                    OverflowItem(R.string.action_rename, session != null) {
                        menuOpen = false
                        onAction(TopBarAction.Rename)
                    }
                    OverflowItem(R.string.action_fork, session != null) {
                        menuOpen = false
                        onAction(TopBarAction.Fork)
                    }
                    // Enabled only when the snapshot carries a run_id.
                    OverflowItem(R.string.action_stop_turn, session?.runId != null) {
                        menuOpen = false
                        onAction(TopBarAction.StopTurn)
                    }
                    OverflowItem(R.string.action_clear_transcript, session != null) {
                        menuOpen = false
                        onAction(TopBarAction.ClearTranscript)
                    }
                    OverflowItem(R.string.action_settings, true) {
                        menuOpen = false
                        onAction(TopBarAction.Settings)
                    }
                }
            }
        }
        Box(
            Modifier
                .fillMaxWidth()
                .height(ForgeSize.hairline)
                .background(colors.border),
        )
    }
}

@Composable
private fun OverflowItem(labelRes: Int, enabled: Boolean, onClick: () -> Unit) {
    val colors = Forge.colors
    val label = stringResource(labelRes)
    DropdownMenuItem(
        text = {
            Text(
                label,
                style = Forge.type.button,
                color = if (enabled) colors.text else colors.textMuted,
            )
        },
        enabled = enabled,
        onClick = onClick,
        modifier = Modifier.semantics { contentDescription = label },
    )
}

private data class Subtitle(val text: String, val dot: androidx.compose.ui.graphics.Color?)

@Composable
private fun title(state: AppUiState, session: SessionRow?): String = when {
    session == null -> stringResource(R.string.header_fallback_title)
    !session.title.isNullOrBlank() -> session.title
    else -> stringResource(R.string.header_session_fallback, session.id.take(6))
}

@Composable
private fun subtitle(state: AppUiState, session: SessionRow?): Subtitle? {
    val colors = Forge.colors
    if (!state.setup.complete && session == null) {
        return Subtitle(
            stringResource(
                R.string.app_subtitle_setup,
                state.setup.currentIndex + 1,
                state.setup.total,
            ),
            null,
        )
    }
    val daemonWord = when (state.daemon) {
        DaemonStatus.Starting, DaemonStatus.Restarting -> stringResource(R.string.daemon_starting)
        DaemonStatus.Stopping -> stringResource(R.string.daemon_stopping)
        DaemonStatus.Stopped -> stringResource(R.string.daemon_stopped)
        is DaemonStatus.Failed -> stringResource(R.string.daemon_stopped)
        is DaemonStatus.Running -> null
    }
    if (daemonWord != null) return Subtitle(daemonWord, colors.textMuted)
    if (session == null) return null

    val stateWord = when (session.state) {
        SessionVisualState.Running -> stringResource(R.string.state_running).lowercase()
        SessionVisualState.NeedsInput -> stringResource(R.string.state_needs_input).lowercase()
        SessionVisualState.Errored -> stringResource(R.string.state_errored).lowercase()
        SessionVisualState.WaitingForNetwork -> stringResource(R.string.state_waiting_network).lowercase()
        SessionVisualState.Idle, SessionVisualState.Unknown -> null
    }
    val model = ModelNames.short(session.model).ifBlank { null }
    val segments = listOfNotNull(stateWord, model, session.effort)
    if (segments.isEmpty()) return null
    val dot = when (session.state) {
        SessionVisualState.Running -> colors.stateRunning
        SessionVisualState.NeedsInput -> colors.stateNeedsInput
        SessionVisualState.Errored -> colors.stateErrored
        SessionVisualState.WaitingForNetwork -> colors.amber
        else -> null
    }
    return Subtitle(segments.joinToString(" · "), dot)
}

@Composable
private fun drawerDescription(state: AppUiState): String = when (state.attentionBadge) {
    AttentionBadgeKind.NeedsInput ->
        stringResource(R.string.cd_open_sessions_needs, state.attentionCount)
    AttentionBadgeKind.Running ->
        stringResource(R.string.cd_open_sessions_running, state.attentionCount)
    AttentionBadgeKind.Errored ->
        stringResource(R.string.cd_open_sessions_errored, state.attentionCount)
    AttentionBadgeKind.None -> stringResource(R.string.cd_open_sessions)
}

@Composable
private fun badgeColor(state: AppUiState): androidx.compose.ui.graphics.Color? {
    val colors = Forge.colors
    return when (state.attentionBadge) {
        AttentionBadgeKind.NeedsInput -> colors.stateNeedsInput
        AttentionBadgeKind.Running -> colors.stateRunning
        AttentionBadgeKind.Errored -> colors.stateErrored
        AttentionBadgeKind.None -> null
    }
}
