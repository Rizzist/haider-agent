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
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.width
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
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.material.icons.rounded.Accessibility
import androidx.compose.material.icons.rounded.AccountTree
import androidx.compose.material.icons.rounded.Screenshot
import androidx.compose.material.icons.rounded.Sms
import androidx.compose.material.icons.rounded.Notifications
import androidx.compose.material.icons.rounded.BatteryFull
import ai.diffforge.haider.ui.state.PermissionStanding
import androidx.compose.material.icons.rounded.ChevronRight
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.platform.testTag

/**
 * The only full screen in the app: it hosts flows that leave it (Accessibility,
 * SMS, notifications, battery), which a sheet cannot survive.
 *
 * The 970 onboarding screen's permission cards moved here; its host/port/token
 * `ConnectCard` is gone, because in 971 there is nothing to connect to — the
 * daemon is in the app.
 */
/** The read-only usage.report line in the daemon card. */
const val USAGE_FOOTER_TAG = "usage_footer"

@Composable
fun SettingsScreen(
    state: AppUiState,
    @Suppress("UNUSED_PARAMETER") themeMode: ThemeMode,
    appVersion: String,
    accountsSummary: String,
    elapsedRealtimeMs: Long,
    onBack: () -> Unit,
    onThemeMode: (ThemeMode) -> Unit,
    onOpenAccounts: () -> Unit,
    /** The Loom registry: agent types and workflows (lane 971-UI-workflows). */
    onOpenLooms: () -> Unit = {},
    onStartDaemon: () -> Unit,
    onStopDaemon: () -> Unit,
    onRestartDaemon: () -> Unit,
    onOpenAccessibility: () -> Unit,
    onGrantSms: () -> Unit,
    onRequestScreenCapture: () -> Unit,
    onRequestNotifications: () -> Unit,
    onOpenBattery: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    var detail by remember { mutableStateOf<PermissionDetail?>(null) }
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
                // Regular type: session counts and uptime are not code
                // (addition F, G1).
                if (line.isNotEmpty()) {
                    Text(line, style = type.sessionMeta, color = colors.textMuted)
                }
                // The version belongs here, not in the drawer or on the start
                // screen (addition F, T1/D1/F1).
                Text(
                    stringResource(R.string.settings_about_version, appVersion),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
                // usage.report, read-only. The protocol calls est_cost_usd
                // "never a bill — an estimate", and so does this line.
                UsageFooter(state.usage)
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
                subtitle = accountsSummary,
                onClick = onOpenAccounts,
            )

            // Lane 971-UI-workflows: the Loom registry lives behind one row
            // rather than a fourth drawer destination — it is inventory a
            // person visits, not a place they work.
            SectionLabel(R.string.settings_section_looms)
            NavigationRow(
                title = stringResource(R.string.looms_title),
                subtitle = stringResource(R.string.settings_looms_row),
                onClick = onOpenLooms,
                icon = Icons.Rounded.AccountTree,
            )

            SectionLabel(R.string.settings_section_permissions)
            // Rows, not paragraphs: title, one-line status, chevron. The
            // explanation and the action live on the row's detail sheet
            // (addition F, T2).
            // Every status below is read back from Android, not printed from
            // a literal (verify-6 O3).
            PermissionRow(
                icon = Icons.Rounded.Accessibility,
                title = stringResource(R.string.settings_permission_accessibility),
                status = standing(state.permissions.accessibility),
                onClick = { detail = PermissionDetail.Accessibility },
            )
            PermissionRow(
                icon = Icons.Rounded.Screenshot,
                title = stringResource(R.string.settings_permission_screen),
                status = standing(state.permissions.screenCapture),
                onClick = { detail = PermissionDetail.ScreenCapture },
            )
            PermissionRow(
                icon = Icons.Rounded.Sms,
                title = stringResource(R.string.settings_permission_sms),
                status = standing(state.permissions.sms),
                onClick = { detail = PermissionDetail.Sms },
            )
            PermissionRow(
                icon = Icons.Rounded.Notifications,
                title = stringResource(R.string.settings_permission_notifications),
                status = stringResource(
                    if (state.environment.notificationsGranted) {
                        R.string.permission_granted
                    } else {
                        R.string.permission_not_granted
                    },
                ),
                onClick = { detail = PermissionDetail.Notifications },
            )
            PermissionRow(
                icon = Icons.Rounded.BatteryFull,
                title = stringResource(R.string.settings_permission_battery),
                status = stringResource(
                    if (state.environment.batteryRestricted) {
                        R.string.permission_not_granted
                    } else {
                        R.string.permission_granted
                    },
                ),
                onClick = { detail = PermissionDetail.Battery },
            )

        }

        detail?.let { open ->
            PermissionDetailSheet(
                detail = open,
                onDismiss = { detail = null },
                onAction = {
                    when (open) {
                        PermissionDetail.Accessibility -> onOpenAccessibility()
                        PermissionDetail.Sms -> onGrantSms()
                        PermissionDetail.Notifications -> onRequestNotifications()
                        PermissionDetail.Battery -> onOpenBattery()
                        PermissionDetail.ScreenCapture -> onRequestScreenCapture()
                    }
                    detail = null
                },
            )
        }
    }
}

@Composable
internal fun SectionLabel(labelRes: Int) {
    Text(
        stringResource(labelRes),
        style = Forge.type.sessionTitle,
        color = Forge.colors.textSoft,
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

/** Which permission's detail sheet is open. */
internal enum class PermissionDetail { Accessibility, ScreenCapture, Sms, Notifications, Battery }

@Composable
private fun PermissionRow(
    icon: ImageVector,
    title: String,
    status: String,
    onClick: () -> Unit,
) {
    // T2 asks for a leading icon; without it every row is a wall of text and
    // the list cannot be scanned (verify-6 O9).
    NavigationRow(title = title, subtitle = status, onClick = onClick, icon = icon)
}

/** The word for what Android said. An unobserved status says so. */
@Composable
private fun standing(value: PermissionStanding): String = stringResource(
    when (value) {
        PermissionStanding.Granted -> R.string.permission_granted
        PermissionStanding.NotGranted -> R.string.permission_not_granted
        PermissionStanding.AskEachTime -> R.string.permission_ask_each_time
        PermissionStanding.Unknown -> R.string.permission_unknown
    },
)

/** The explanation and the action, on the row that asked for them. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PermissionDetailSheet(
    detail: PermissionDetail,
    onDismiss: () -> Unit,
    onAction: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val (titleRes, bodyRes, actionRes) = when (detail) {
        PermissionDetail.Accessibility -> Triple(
            R.string.settings_permission_accessibility,
            R.string.settings_permission_accessibility_body,
            R.string.settings_permission_accessibility_action,
        )
        PermissionDetail.ScreenCapture -> Triple(
            R.string.settings_permission_screen,
            R.string.settings_permission_screen_body,
            // Android issues this per projection, so there is always something
            // to do here — round 9 left the sheet with no action at all
            // (verify-8 O2).
            R.string.settings_permission_screen_action,
        )
        PermissionDetail.Sms -> Triple(
            R.string.settings_permission_sms,
            R.string.settings_permission_sms_body,
            R.string.settings_permission_sms_action,
        )
        PermissionDetail.Notifications -> Triple(
            R.string.settings_permission_notifications,
            R.string.settings_permission_notifications_body,
            R.string.step_notify_action,
        )
        PermissionDetail.Battery -> Triple(
            R.string.settings_permission_battery,
            R.string.settings_permission_battery_body,
            R.string.step_battery_action,
        )
    }
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier.padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.lg),
        ) {
            Text(stringResource(titleRes), style = type.h4, color = colors.text)
            Text(stringResource(bodyRes), style = type.sessionMeta, color = colors.textMuted)
            actionRes?.let {
                ForgeButton(text = stringResource(it), onClick = onAction)
            }
        }
    }
}

@Composable
internal fun NavigationRow(
    title: String,
    subtitle: String?,
    onClick: () -> Unit,
    icon: ImageVector? = null,
) {
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
        icon?.let {
            Icon(
                it,
                contentDescription = null,
                tint = colors.textSoft,
                modifier = Modifier
                    .size(ForgeSize.iconMd)
                    .padding(end = ForgeSpace.xxs),
            )
            Spacer(Modifier.width(ForgeSpace.lg))
        }
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

/**
 * `usage.report` totals (`usage.rs:296`).
 *
 * Absence is stated, not drawn as zeros: a daemon that does not advertise
 * `usage_report_v1` says so.
 */
@Composable
private fun UsageFooter(snapshot: ai.diffforge.haider.ui.daemon.UsageSnapshot) {
    val colors = Forge.colors
    val type = Forge.type
    if (!snapshot.supported) {
        Text(
            stringResource(R.string.usage_footer_unavailable),
            style = type.sessionMeta,
            color = colors.textMuted,
            modifier = Modifier.testTag(USAGE_FOOTER_TAG),
        )
        return
    }
    val totals = snapshot.totals
    Column(modifier = Modifier.testTag(USAGE_FOOTER_TAG)) {
        Text(
            buildString {
                append(stringResource(R.string.usage_footer_label))
                append("  ")
                append(tokens(totals.inputTokens))
                append(" in · ")
                append(tokens(totals.outputTokens))
                append(" out")
                if (totals.cachedTokens > 0) {
                    append(" · ")
                    append(tokens(totals.cachedTokens))
                    append(" cached")
                }
            },
            style = type.sessionMeta,
            color = colors.textMuted,
        )
        totals.estCostUsd?.let { cost ->
            Text(
                "~$" + "%.2f".format(cost) + " " + stringResource(R.string.usage_footer_estimate),
                style = type.selectLabel,
                color = colors.textMuted,
            )
        }
    }
}

private fun tokens(value: Long): String = when {
    value >= 1_000_000 -> "%.1fM".format(value / 1_000_000.0)
    value >= 1_000 -> "%.1fk".format(value / 1_000.0)
    else -> value.toString()
}
