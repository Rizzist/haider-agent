package ai.diffforge.haider.ui.drawer

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
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
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow

/**
 * Row actions as a sheet, not a dialog, and reachable from three places: the
 * row's long press, the row's accessibility `customActions`, and the header
 * overflow. Long-press is never the only path to an action (UI-SPEC 3.3.5).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SessionActionsSheet(
    row: SessionRow,
    onDismiss: () -> Unit,
    onAction: (SessionRowAction) -> Unit,
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
            Text(
                displayTitle(row),
                style = type.h4,
                color = colors.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                row.id,
                // mono: a session id is a literal token, and the sheet's own
                // Copy session id action exists because people paste it.
                style = type.numeric,
                color = colors.textMuted,
                modifier = Modifier.padding(bottom = ForgeSpace.lg),
            )
            ActionRow(R.string.action_rename) { onAction(SessionRowAction.Rename) }
            ActionRow(R.string.action_fork) { onAction(SessionRowAction.Fork) }
            // The drawer row's own surface is where a session's branch is
            // chosen and its workspace timeline is opened: both are per-session
            // facts, and neither belongs on the composer.
            ActionRow(R.string.action_checkpoints) { onAction(SessionRowAction.Checkpoints) }
            ActionRow(R.string.branches_title) { onAction(SessionRowAction.Branches) }
            // Stopping a turn is the composer's job and only the composer's
            // (E2, verify-6 O2).
            ActionRow(R.string.action_copy_session_id) { onAction(SessionRowAction.CopyId) }
        }
    }
}

@Composable
private fun ActionRow(labelRes: Int, destructive: Boolean = false, onClick: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    val label = stringResource(labelRes)
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
        Text(label, style = type.button, color = if (destructive) colors.red else colors.text)
    }
}

/** Rename, as a sheet with one field and two buttons. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RenameSheet(
    current: String,
    onDismiss: () -> Unit,
    onRename: (String) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    var value by remember { mutableStateOf(current) }
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
            Text(stringResource(R.string.action_rename), style = type.h4, color = colors.text)
            Box(
                Modifier
                    .fillMaxWidth()
                    .heightIn(min = ForgeSize.touch)
                    .clip(ForgeShapes.cardTight)
                    .background(colors.surfaceControl)
                    .border(ForgeSize.hairline, colors.border, ForgeShapes.cardTight)
                    .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.lg),
            ) {
                BasicTextField(
                    value = value,
                    onValueChange = { value = it },
                    singleLine = true,
                    textStyle = type.chatBody.copy(color = colors.text),
                    cursorBrush = SolidColor(colors.accent),
                    modifier = Modifier.fillMaxWidth(),
                )
            }
            Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.lg)) {
                ForgeButton(
                    text = stringResource(R.string.action_cancel),
                    onClick = onDismiss,
                    kind = ForgeButtonKind.Ghost,
                    modifier = Modifier.weight(1f),
                )
                ForgeButton(
                    text = stringResource(R.string.action_save),
                    onClick = { onRename(value.trim()) },
                    enabled = value.isNotBlank(),
                    modifier = Modifier.weight(1f),
                )
            }
        }
    }
}
