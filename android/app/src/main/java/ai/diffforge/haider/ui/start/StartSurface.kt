package ai.diffforge.haider.ui.start

import ai.diffforge.haider.R
import ai.diffforge.haider.daemon.DaemonStatus
import ai.diffforge.haider.ui.drawer.SessionRowAction
import ai.diffforge.haider.ui.drawer.SessionRowItem
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.ModelNames
import ai.diffforge.haider.ui.state.RelativeTime
import ai.diffforge.haider.ui.state.SessionListState
import ai.diffforge.haider.ui.state.SetupStepId
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
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow

const val START_FIRST_CHILD_TAG = "start_first_child"

/**
 * First run and the steady-state empty session.
 *
 * **Top-aligned, never centred.** 970 put the hero in
 * `Box(fillMaxSize, contentAlignment = Center)`, which is what produced the
 * huge dead space above and below it (D3). Content starts 22 dp under the
 * header and flows down; the space left over is at the bottom, above the
 * composer, where empty space is normal.
 *
 * The copy no longer asks for something that will not exist in 971: there is no
 * host, no port and no token to type (D9).
 */
@Composable
fun StartSurface(
    state: AppUiState,
    appVersion: String,
    nowMs: Long,
    onStepAction: (SetupStepId) -> Unit,
    onSuggestion: (String) -> Unit,
    onSelectSession: (String) -> Unit,
    onSeeAllSessions: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    val setup = state.setup
    val running = state.daemon as? DaemonStatus.Running

    Column(
        modifier = modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = ForgeSpace.xl)
            .padding(top = ForgeSize.startFirstChildInset, bottom = ForgeSpace.xxl),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.lg),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .testTag(START_FIRST_CHILD_TAG),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier
                    .size(ForgeSize.markLg)
                    .clip(ForgeShapes.card)
                    .background(colors.accentWash)
                    .border(ForgeSize.hairline, colors.accent, ForgeShapes.card),
                contentAlignment = Alignment.Center,
            ) {
                Text("H", style = type.h4, color = colors.accent)
            }
            Column(
                Modifier
                    .weight(1f)
                    .padding(start = ForgeSpace.lg),
            ) {
                Text(
                    if (setup.complete) {
                        stringResource(R.string.app_name)
                    } else {
                        stringResource(R.string.start_title)
                    },
                    style = type.h1,
                    color = colors.text,
                )
                Text(
                    stringResource(R.string.start_subtitle, appVersion),
                    style = type.numeric,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }

        if (setup.complete) {
            Text(stringResource(R.string.ready_line), style = type.chatBody, color = colors.textMuted)
        } else {
            Text(
                if (setup.total >= 4) {
                    stringResource(R.string.start_lede)
                } else {
                    stringResource(R.string.start_lede_three)
                },
                style = type.chatBody,
                color = colors.textMuted,
            )
            Column(
                Modifier
                    .fillMaxWidth()
                    .clip(ForgeShapes.card)
                    .background(colors.surface)
                    .border(ForgeSize.hairline, colors.border, ForgeShapes.card)
                    .padding(horizontal = ForgeSpace.xl),
            ) {
                setup.steps.forEachIndexed { index, step ->
                    if (index > 0) {
                        Box(
                            Modifier
                                .fillMaxWidth()
                                .height(ForgeSize.hairline)
                                .background(colors.border),
                        )
                    }
                    SetupStepRow(
                        step = step,
                        index = index,
                        doneDetail = doneDetail(step.id, state, running, nowMs),
                        onAction = { onStepAction(step.id) },
                    )
                }
            }
        }

        if (state.sessions.isEmpty()) {
            SuggestionList(onSuggestion = onSuggestion)
        } else {
            RecentSessions(
                state = state,
                nowMs = nowMs,
                onSelectSession = onSelectSession,
                onSeeAllSessions = onSeeAllSessions,
            )
        }
    }
}

@Composable
private fun RecentSessions(
    state: AppUiState,
    nowMs: Long,
    onSelectSession: (String) -> Unit,
    onSeeAllSessions: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val recent = SessionListState.recent(state.sessions, state.activeSessionId, 3)
    Column(Modifier.fillMaxWidth()) {
        Text(
            stringResource(R.string.recent_sessions_header),
            style = type.drawerSection,
            color = colors.textMuted,
            modifier = Modifier.padding(top = ForgeSpace.xxl, bottom = ForgeSpace.md),
        )
        Column(
            Modifier
                .fillMaxWidth()
                .clip(ForgeShapes.card)
                .background(colors.surface)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.card),
        ) {
            recent.forEach { row ->
                SessionRowItem(
                    row = row,
                    selected = row.id == state.activeSessionId,
                    nowMs = nowMs,
                    onClick = { onSelectSession(row.id) },
                    onLongClick = { onSelectSession(row.id) },
                    onAction = { _: SessionRowAction -> onSelectSession(row.id) },
                )
            }
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = ForgeSize.touch)
                    .clickable(onClick = onSeeAllSessions)
                    .padding(horizontal = ForgeSpace.xl),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    stringResource(R.string.see_all_sessions, state.sessions.size),
                    style = type.button,
                    color = colors.accent,
                )
            }
        }
    }
}

@Composable
private fun doneDetail(
    id: SetupStepId,
    state: AppUiState,
    running: DaemonStatus.Running?,
    nowMs: Long,
): String? = when (id) {
    SetupStepId.RunService -> running?.info?.let { info ->
        stringResource(
            R.string.step_service_done,
            info.rssBytes?.let { "${it / (1024 * 1024)} MB" } ?: "",
            RelativeTime.clock(info.startedAtMs),
        )
    }
    SetupStepId.Notifications -> stringResource(R.string.step_notify_done)
    SetupStepId.Battery -> stringResource(R.string.step_battery_done)
    SetupStepId.Model -> state.models?.let {
        stringResource(
            R.string.step_model_done,
            it.current.provider,
            ModelNames.short(it.current.model),
        )
    }
}
