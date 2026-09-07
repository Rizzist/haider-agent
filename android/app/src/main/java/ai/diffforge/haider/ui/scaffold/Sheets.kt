package ai.diffforge.haider.ui.scaffold

import ai.diffforge.haider.R
import ai.diffforge.haider.daemon.DaemonStatus
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics

/**
 * Attach: only what the app can actually do today. Voice is not shipped in 971
 * — Android has no mic capture path, so the slot is reserved and no dead mic
 * button is drawn (UI-SPEC 3.6, 6.5).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AttachSheet(
    onDismiss: () -> Unit,
    onScreenshot: () -> Unit,
    onPickFile: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(Modifier.padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl)) {
            Text(stringResource(R.string.attach_title), style = type.h4, color = colors.text)
            SheetRow(stringResource(R.string.attach_screenshot), onScreenshot)
            SheetRow(stringResource(R.string.attach_file), onPickFile)
        }
    }
}

/**
 * Daemon details: version, pid, socket path, profile path, runtime dir and
 * generation. Every value that is unknown is omitted rather than printed as a
 * dash or a zero.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DaemonDetailsSheet(
    status: DaemonStatus,
    onDismiss: () -> Unit,
    onRestart: () -> Unit,
    onCopyDiagnostics: (String) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val info = (status as? DaemonStatus.Running)?.info
    val lines = buildList {
        info?.let {
            add("version" to it.version)
            add("generation" to it.generation.toString())
            it.pid?.let { pid -> add("pid" to pid.toString()) }
            it.socketPath?.let { path -> add("socket" to path) }
            it.profilePath?.let { path -> add("profile" to path) }
            it.runtimeDir?.let { path -> add("runtime" to path) }
        }
        if (status is DaemonStatus.Failed) {
            add("error" to status.reason)
            status.code?.let { add("code" to it) }
        }
    }
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier.padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Text(stringResource(R.string.daemon_details_title), style = type.h4, color = colors.text)
            lines.forEach { (label, value) ->
                Row(Modifier.fillMaxWidth()) {
                    Text(
                        label,
                        style = type.label,
                        color = colors.textMuted,
                        modifier = Modifier.padding(end = ForgeSpace.lg),
                    )
                    Text(value, style = type.numeric, color = colors.chatText)
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                ForgeButton(
                    text = stringResource(R.string.daemon_action_restart),
                    onClick = onRestart,
                    kind = ForgeButtonKind.Ghost,
                )
                ForgeButton(
                    text = stringResource(R.string.daemon_copy_diagnostics),
                    onClick = {
                        onCopyDiagnostics(lines.joinToString("\n") { "${it.first}: ${it.second}" })
                    },
                    kind = ForgeButtonKind.Ghost,
                )
            }
        }
    }
}

@Composable
private fun SheetRow(label: String, onClick: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.row)
            .clickable(onClick = onClick)
            .padding(horizontal = ForgeSpace.lg)
            .semantics {
                contentDescription = label
                role = Role.Button
            },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, style = type.button, color = colors.text)
    }
}
