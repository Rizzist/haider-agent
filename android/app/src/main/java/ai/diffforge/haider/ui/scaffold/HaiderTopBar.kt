package ai.diffforge.haider.ui.scaffold

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.AttentionBadge
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.components.SessionGlyph
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.components.motionEnabled
import ai.diffforge.haider.ui.state.AttentionBadgeKind
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.SessionViewTab
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
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
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Chat
import androidx.compose.material.icons.rounded.DarkMode
import androidx.compose.material.icons.rounded.LightMode
import androidx.compose.material.icons.rounded.Menu
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material.icons.rounded.Terminal
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow

const val HAIDER_TOP_BAR_TAG = "haider_top_bar"
const val SESSION_VIEW_HEADER_TAG = "session_view_header"
const val HEADER_STATE_PILL_TAG = "header_state_pill"

/**
 * One slim control row (owner addition H1).
 *
 * The owner's note was "way too big header". It was: a 56 dp bar with a title
 * line, a conditional second line, and a separate slim row below it for the
 * Chat|Shell switch — three rows of chrome above a chat. The reference
 * (`dashboard.js:39296` TerminalChatHeaderBar) has no title at all, because the
 * title is data and the drawer already lists it in an outlined row.
 *
 * So: a hamburger square on the left, and on the right the Chat|Shell pill
 * (`:39389`/`:39490`), the state pill (`:18940`), a refresh circle and the
 * theme circle (`:39640`). Every visual is 40 dp inside a 48 dp target.
 *
 * There is no overflow menu any more either. Everything it held is reachable
 * where it belongs: Rename / Fork / Copy id on the drawer row's own sheet,
 * Settings in the drawer footer, daemon details on the drawer's daemon row.
 */
@Composable
fun HaiderTopBar(
    state: AppUiState,
    dark: Boolean,
    onOpenDrawer: () -> Unit,
    onToggleTheme: () -> Unit,
    onRefresh: () -> Unit,
    onSelectTab: (SessionViewTab) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    Column(modifier.testTag(HAIDER_TOP_BAR_TAG)) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.header)
                .padding(horizontal = ForgeSpace.md),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
        ) {
            Box {
                HeaderSquareButton(
                    icon = Icons.Rounded.Menu,
                    contentDescription = drawerDescription(state),
                    onClick = onOpenDrawer,
                )
                val badge = badgeColor(state)
                if (badge != null) {
                    AttentionBadge(
                        color = badge,
                        modifier = Modifier
                            .align(Alignment.TopEnd)
                            .padding(top = ForgeSpace.xs, end = ForgeSpace.xs),
                    )
                }
            }

            Box(Modifier.weight(1f))

            ViewToggle(
                tab = state.viewTab,
                onSelect = onSelectTab,
            )
            StatePill(state = state, session = state.activeSession)
            HeaderCircleButton(
                icon = Icons.Rounded.Refresh,
                contentDescription = stringResource(R.string.cd_refresh_session),
                onClick = onRefresh,
            )
            HeaderCircleButton(
                icon = if (dark) Icons.Rounded.LightMode else Icons.Rounded.DarkMode,
                contentDescription = stringResource(
                    if (dark) R.string.cd_use_light_theme else R.string.cd_use_dark_theme,
                ),
                onClick = onToggleTheme,
            )
        }
        Box(
            Modifier
                .fillMaxWidth()
                .height(ForgeSize.hairline)
                .background(colors.border),
        )
    }
}

/** 40 dp rounded-8 hairline square, in a 48 dp target. */
@Composable
private fun HeaderSquareButton(
    icon: ImageVector,
    contentDescription: String,
    onClick: () -> Unit,
) {
    val colors = Forge.colors
    Box(
        modifier = Modifier
            .size(ForgeSize.touch)
            .clickable(onClick = onClick)
            .semantics {
                this.contentDescription = contentDescription
                this.role = Role.Button
            },
        contentAlignment = Alignment.Center,
    ) {
        Box(
            Modifier
                .size(ForgeSize.headerControl)
                .clip(ForgeShapes.cardTight)
                .background(colors.surface)
                .border(ForgeSize.hairline, colors.borderStrong, ForgeShapes.cardTight),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                icon,
                contentDescription = null,
                tint = colors.textSoft,
                modifier = Modifier.size(ForgeSize.iconMd),
            )
        }
    }
}

/** The reference's 30 px header circle, at 40 dp in a 48 dp target. */
@Composable
private fun HeaderCircleButton(
    icon: ImageVector,
    contentDescription: String,
    onClick: () -> Unit,
) {
    val colors = Forge.colors
    ForgeIconButton(
        onClick = onClick,
        contentDescription = contentDescription,
        background = colors.surface,
        visual = ForgeSize.headerControl,
        border = colors.borderStrong,
    ) {
        Icon(
            icon,
            contentDescription = null,
            tint = colors.textSoft,
            modifier = Modifier.size(ForgeSize.iconSm),
        )
    }
}

/**
 * Chat | Shell, as the reference's pill (`:39389`): hairline strong border,
 * radius 999, surface fill, active segment on the accent wash with an inset
 * hairline.
 *
 * The painted pill is 40 dp and each segment's *target* is 48 dp, laid over it,
 * because the touch-target rule has no exemptions and a 26 px desktop button is
 * not a phone target.
 */
@Composable
private fun ViewToggle(tab: SessionViewTab, onSelect: (SessionViewTab) -> Unit) {
    val colors = Forge.colors
    val entries = SessionViewTab.entries
    Box(
        modifier = Modifier.testTag(SESSION_VIEW_HEADER_TAG),
        contentAlignment = Alignment.Center,
    ) {
        Row(
            Modifier
                .height(ForgeSize.headerControl)
                .clip(ForgeShapes.pill)
                .background(colors.surface)
                .border(ForgeSize.hairline, colors.borderStrong, ForgeShapes.pill)
                .padding(ForgeSpace.xxs),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            entries.forEach { candidate ->
                val active = candidate == tab
                Box(
                    Modifier
                        .width(ForgeSize.touch)
                        .height(ForgeSize.headerSegment)
                        .clip(ForgeShapes.pill)
                        .background(if (active) colors.accentWash else Color.Transparent)
                        .then(
                            if (active) {
                                Modifier.border(
                                    ForgeSize.hairline,
                                    colors.borderStrong,
                                    ForgeShapes.pill,
                                )
                            } else {
                                Modifier
                            },
                        ),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        when (candidate) {
                            SessionViewTab.Chat -> Icons.Rounded.Chat
                            SessionViewTab.Shell -> Icons.Rounded.Terminal
                        },
                        contentDescription = null,
                        tint = if (active) colors.text else colors.textMuted,
                        modifier = Modifier.size(ForgeSize.iconSm),
                    )
                }
            }
        }
        Row {
            entries.forEach { candidate ->
                val label = stringResource(
                    when (candidate) {
                        SessionViewTab.Chat -> R.string.tab_chat
                        SessionViewTab.Shell -> R.string.tab_shell
                    },
                )
                Box(
                    Modifier
                        .size(ForgeSize.touch)
                        .clickable { onSelect(candidate) }
                        .semantics {
                            contentDescription = label
                            role = Role.Tab
                            selected = candidate == tab
                        },
                )
            }
        }
    }
}

/**
 * The reference's selected-state pill (`:18940`): the session's brand mark and
 * one word, toned green / amber / red, breathing while a turn runs.
 *
 * This is the only place the header says anything about state, and it says it
 * in a word as well as a colour.
 */
@Composable
private fun StatePill(state: AppUiState, session: SessionRow?) {
    val colors = Forge.colors
    val type = Forge.type
    val (label, tone) = when {
        state.daemon is DaemonStatus.Failed -> stringResource(R.string.state_pill_error) to colors.red
        state.daemon !is DaemonStatus.Running ->
            stringResource(R.string.state_pill_offline) to colors.textMuted
        session == null -> stringResource(R.string.state_pill_idle) to colors.textMuted
        else -> when (session.state) {
            SessionVisualState.Running ->
                stringResource(R.string.state_pill_running) to colors.stateRunning
            SessionVisualState.NeedsInput ->
                stringResource(R.string.state_pill_needs_you) to colors.stateNeedsInput
            SessionVisualState.Errored ->
                stringResource(R.string.state_pill_error) to colors.stateErrored
            SessionVisualState.WaitingForNetwork ->
                stringResource(R.string.state_pill_waiting) to colors.amber
            else -> stringResource(R.string.state_pill_idle) to colors.textMuted
        }
    }
    Row(
        modifier = Modifier
            .height(ForgeSize.headerControl)
            .clip(ForgeShapes.pill)
            .background(colors.surface)
            .border(ForgeSize.hairline, colors.borderStrong, ForgeShapes.pill)
            .padding(horizontal = ForgeSpace.md)
            .testTag(HEADER_STATE_PILL_TAG)
            .semantics { contentDescription = label },
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
    ) {
        SessionGlyph(
            model = session?.model,
            provider = session?.provider,
            state = session?.state ?: SessionVisualState.Idle,
            animate = session?.state == SessionVisualState.Running && motionEnabled(),
            ringAgainst = colors.surface,
        )
        Text(
            label,
            style = type.chip,
            color = tone,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
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
private fun badgeColor(state: AppUiState): Color? {
    val colors = Forge.colors
    return when (state.attentionBadge) {
        AttentionBadgeKind.NeedsInput -> colors.stateNeedsInput
        AttentionBadgeKind.Errored -> colors.stateErrored
        AttentionBadgeKind.Running -> colors.stateRunning
        AttentionBadgeKind.None -> null
    }
}
