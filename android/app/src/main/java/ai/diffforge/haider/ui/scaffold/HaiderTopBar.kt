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
import androidx.compose.foundation.background
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
import androidx.compose.material.icons.rounded.DarkMode
import androidx.compose.material.icons.rounded.LightMode
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
    Settings,
}

/**
 * 56 dp, one hairline, and interactive controls that are all the same shape
 * (UI-SPEC 3.1): the drawer button, the appearance toggle and the overflow.
 * The title is not one of them — it is data.
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
    dark: Boolean,
    onOpenDrawer: () -> Unit,
    onToggleTheme: () -> Unit,
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

            // The title is data, not a control. Making it clickable made the
            // header a three-target bar, which is exactly what 6.3.1 forbids;
            // "Session details" lives in the overflow instead.
            Column(
                modifier = Modifier
                    .weight(1f)
                    .heightIn(min = ForgeSize.touch)
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

            // Addition D: light/dark stays on the bar, beside the overflow.
            ForgeIconButton(
                onClick = onToggleTheme,
                contentDescription = stringResource(
                    if (dark) R.string.cd_use_light_theme else R.string.cd_use_dark_theme,
                ),
            ) {
                Icon(
                    if (dark) Icons.Rounded.LightMode else Icons.Rounded.DarkMode,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.icon),
                )
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
                    // No Stop here. E2 puts the one turn Stop on the composer,
                    // and an overflow entry alongside it was two Stops on one
                    // screen (verify-6 O2). No Clear either: there is no
                    // session.clear RPC, so it could only ever have emptied the
                    // local view while the daemon kept every message —
                    // contracts-v1 forbids exactly that (verify-6 O1).
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
    // A blank title is a new session, not a raw id: "Session s-new-" was the
    // id leaking into the face S7 says it must never reach (verify-6 O6).
    else -> stringResource(R.string.header_new_session)
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
    // Same slot, same voice: the session states below are lowercase, so the
    // daemon word is too (addition F, G3).
    if (daemonWord != null) return Subtitle(daemonWord.lowercase(), colors.textMuted)
    if (session == null) return null

    // The state word and nothing else: the model and the effort are composer
    // chips, and repeating them here was the header saying what the composer
    // already says (addition F, S1/G4).
    val stateWord = when (session.state) {
        SessionVisualState.Running -> stringResource(R.string.state_running).lowercase()
        SessionVisualState.NeedsInput -> stringResource(R.string.state_needs_input).lowercase()
        SessionVisualState.Errored -> stringResource(R.string.state_errored).lowercase()
        SessionVisualState.WaitingForNetwork -> stringResource(R.string.state_waiting_network).lowercase()
        SessionVisualState.Idle, SessionVisualState.Unknown -> null
    } ?: return null
    val dot = when (session.state) {
        SessionVisualState.Running -> colors.stateRunning
        SessionVisualState.NeedsInput -> colors.stateNeedsInput
        SessionVisualState.Errored -> colors.stateErrored
        SessionVisualState.WaitingForNetwork -> colors.amber
        else -> null
    }
    return Subtitle(stateWord, dot)
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
