package ai.diffforge.haider.ui.settings

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.drawer.resourceLine
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowBack
import androidx.compose.material.icons.rounded.ChevronRight
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow

/**
 * The only full screen in the app: it hosts flows that leave it (Accessibility,
 * SMS, notifications, battery), which a sheet cannot survive.
 *
 * The 970 onboarding screen's permission cards moved here; its host/port/token
 * `ConnectCard` is gone, because in 971 there is nothing to connect to — the
 * daemon is in the app.
 */
@Composable
fun SettingsScreen(
    state: AppUiState,
    themeMode: ThemeMode,
    appVersion: String,
    elapsedRealtimeMs: Long,
    onBack: () -> Unit,
    onThemeMode: (ThemeMode) -> Unit,
    onOpenAccounts: () -> Unit,
    onStartDaemon: () -> Unit,
    onStopDaemon: () -> Unit,
    onRestartDaemon: () -> Unit,
    onOpenAccessibility: () -> Unit,
    onGrantSms: () -> Unit,
    onRequestNotifications: () -> Unit,
    onOpenBattery: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = modifier
            .fillMaxSize()
            .background(colors.bg)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.header)
                .padding(horizontal = ForgeSpace.xs),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ForgeIconButton(onClick = onBack, contentDescription = stringResource(R.string.action_back)) {
                Icon(
                    Icons.Rounded.ArrowBack,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.icon),
                )
            }
            Text(
                stringResource(R.string.settings_title),
                style = type.sessionTitle,
                color = colors.text,
                modifier = Modifier.padding(start = ForgeSpace.md),
            )
        }

        Column(
            Modifier
                .weight(1f)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.lg),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.lg),
        ) {
            SectionLabel(R.string.settings_section_daemon)
            Card {
                Text(
                    when (val daemon = state.daemon) {
                        is DaemonStatus.Running -> stringResource(R.string.daemon_running)
                        DaemonStatus.Starting -> stringResource(R.string.daemon_starting)
                        DaemonStatus.Restarting -> stringResource(R.string.daemon_restarting)
                        DaemonStatus.Stopping -> stringResource(R.string.daemon_stopping)
                        DaemonStatus.Stopped -> stringResource(R.string.daemon_stopped)
                        is DaemonStatus.Failed -> stringResource(R.string.daemon_failed, daemon.reason)
                    },
                    style = type.sessionTitle,
                    color = colors.text,
                )
                val line = resourceLine(
                    state.daemon as? DaemonStatus.Running,
                    state.sessions.count { it.runId != null },
                    elapsedRealtimeMs,
                )
                if (line.isNotEmpty()) {
                    Text(line, style = type.numeric, color = colors.textMuted)
                }
                Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                    if (state.daemon is DaemonStatus.Running) {
                        ForgeButton(
                            text = stringResource(R.string.daemon_action_stop),
                            onClick = onStopDaemon,
                            kind = ForgeButtonKind.Ghost,
                        )
                        ForgeButton(
                            text = stringResource(R.string.daemon_action_restart),
                            onClick = onRestartDaemon,
                            kind = ForgeButtonKind.Ghost,
                        )
                    } else {
                        ForgeButton(
                            text = stringResource(R.string.daemon_action_start),
                            onClick = onStartDaemon,
                        )
                    }
                }
            }

            SectionLabel(R.string.settings_section_accounts)
            NavigationRow(
                title = stringResource(R.string.accounts_title),
                subtitle = stringResource(R.string.accounts_add_api_key),
                onClick = onOpenAccounts,
            )

            SectionLabel(R.string.settings_section_permissions)
            PermissionCard(
                title = stringResource(R.string.settings_permission_accessibility),
                body = stringResource(R.string.settings_permission_accessibility_body),
                action = stringResource(R.string.settings_permission_accessibility_action),
                onClick = onOpenAccessibility,
            )
            PermissionCard(
                title = stringResource(R.string.settings_permission_screen),
                body = stringResource(R.string.settings_permission_screen_body),
                action = null,
                onClick = {},
            )
            PermissionCard(
                title = stringResource(R.string.settings_permission_sms),
                body = stringResource(R.string.settings_permission_sms_body),
                action = stringResource(R.string.settings_permission_sms_action),
                onClick = onGrantSms,
            )
            PermissionCard(
                title = stringResource(R.string.settings_permission_notifications),
                body = stringResource(R.string.settings_permission_notifications_body),
                action = if (state.environment.notificationsGranted) {
                    null
                } else {
                    stringResource(R.string.step_notify_action)
                },
                onClick = onRequestNotifications,
            )
            PermissionCard(
                title = stringResource(R.string.settings_permission_battery),
                body = stringResource(R.string.settings_permission_battery_body),
                action = stringResource(R.string.step_battery_action),
                onClick = onOpenBattery,
            )

            SectionLabel(R.string.settings_section_appearance)
            Card {
                Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                    listOf(
                        ThemeMode.System to R.string.appearance_system,
                        ThemeMode.Light to R.string.appearance_light,
                        ThemeMode.Dark to R.string.appearance_dark,
                    ).forEach { (mode, labelRes) ->
                        val label = stringResource(labelRes)
                        ForgeChip(
                            onClick = { onThemeMode(mode) },
                            selected = themeMode == mode,
                            contentDescription = label,
                        ) {
                            Text(
                                label,
                                style = type.button,
                                color = if (themeMode == mode) colors.accent else colors.textMuted,
                            )
                        }
                    }
                }
            }

            SectionLabel(R.string.settings_section_about)
            Card {
                Text(
                    stringResource(R.string.settings_about_version, appVersion),
                    style = type.sessionTitle,
                    color = colors.text,
                )
                Text(
                    stringResource(R.string.settings_about_body),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
            }
        }
    }
}

@Composable
internal fun SectionLabel(labelRes: Int) {
    Text(
        stringResource(labelRes),
        style = Forge.type.drawerSection,
        color = Forge.colors.textMuted,
        modifier = Modifier.padding(top = ForgeSpace.md),
    )
}

@Composable
internal fun Card(content: @Composable () -> Unit) {
    val colors = Forge.colors
    Column(
        Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.card)
            .background(colors.surface)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.card)
            .padding(ForgeSpace.xl),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) { content() }
}

@Composable
private fun PermissionCard(title: String, body: String, action: String?, onClick: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Card {
        Text(title, style = type.sessionTitle, color = colors.text)
        Text(body, style = type.sessionMeta, color = colors.textMuted)
        if (action != null) {
            ForgeButton(text = action, onClick = onClick, kind = ForgeButtonKind.Ghost)
        }
    }
}

@Composable
internal fun NavigationRow(title: String, subtitle: String?, onClick: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.footerRow)
            .clip(ForgeShapes.card)
            .background(colors.surface)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.card)
            .clickable(onClick = onClick)
            .padding(horizontal = ForgeSpace.xl),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = type.sessionTitle, color = colors.text)
            subtitle?.let {
                Text(it, style = type.sessionMeta, color = colors.textMuted, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
        }
        Box {
            Icon(
                Icons.Rounded.ChevronRight,
                contentDescription = null,
                tint = colors.textMuted,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
        }
    }
}
