package ai.diffforge.haider.ui.checkpoints

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.LabelledField
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow

/** Test handles for the branch sheet. */
const val BRANCHES_SHEET_TAG = "branches_sheet"
const val BRANCHES_NAME_FIELD_TAG = "branches_name_field"

/**
 * Which branch this session's next message goes on, and how to make another one.
 *
 * Two facts shape this sheet, and both come straight from the wire:
 *
 *  - main is IMPLICIT. `ObserveSessionWire.branches` lists named refs only and
 *    `active_branch_id: null` names main (branch.rs:8), so the Main row here is
 *    drawn by this client and is deliberately not a branch id — choosing it
 *    OMITS `branch_id` from `turn.submit` rather than sending a made-up one;
 *  - there is no `branch.switch`. The only branch verbs are `branch.create` and
 *    a `branch_id` on `turn.submit`, so picking a row here changes what the
 *    NEXT turn is submitted on and moves nothing that is already committed. The
 *    sheet says that instead of implying a checkout.
 *
 * `branch.create` needs an exact `(fork_node_id, fork_seq)` pair from one
 * published fact. A checkpoint carries neither — it records a file mutation,
 * not a history node — so a branch forks at the session's own head, and the
 * sheet states that rather than offering a fork point it cannot address.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun BranchSheet(
    branches: List<BranchView>,
    selectedBranchId: String?,
    forkPointSeq: Long?,
    notice: String?,
    onDismiss: () -> Unit,
    onSelect: (String?) -> Unit,
    onCreate: (String) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    var name by remember { mutableStateOf("") }
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier
                .testTag(BRANCHES_SHEET_TAG)
                .verticalScroll(rememberScrollState())
                .padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Text(stringResource(R.string.branches_title), style = type.h4, color = colors.text)
            Text(
                stringResource(R.string.branches_switch_hint),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
            notice?.let {
                Text(it, style = type.sessionMeta, color = colors.amber)
            }

            BranchRow(
                label = stringResource(R.string.branches_main),
                detail = null,
                selected = selectedBranchId == null,
                onClick = { onSelect(null) },
            )
            branches.forEach { branch ->
                BranchRow(
                    label = branch.name,
                    detail = branch.headSeq?.let { stringResource(R.string.branches_head, it) },
                    selected = selectedBranchId == branch.branchId,
                    onClick = { onSelect(branch.branchId) },
                )
            }

            Text(
                stringResource(R.string.branches_fork_point),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
            if (forkPointSeq == null) {
                Text(
                    stringResource(R.string.branches_no_fork_point),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
            } else {
                // The exact node the new branch will fork at, so the create is
                // not a leap of faith about where "here" is.
                Text(
                    stringResource(R.string.branches_head, forkPointSeq),
                    style = type.sessionMeta,
                    color = colors.textSoft,
                )
                LabelledField(
                    label = stringResource(R.string.branches_name),
                    value = name,
                    onValueChange = { name = it },
                    tag = BRANCHES_NAME_FIELD_TAG,
                )
                ForgeButton(
                    text = stringResource(R.string.branches_create),
                    onClick = { onCreate(name) },
                    kind = ForgeButtonKind.Ghost,
                )
            }
        }
    }
}

@Composable
private fun BranchRow(
    label: String,
    detail: String?,
    selected: Boolean,
    onClick: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val inUse = stringResource(R.string.branches_current)
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.row)
            .background(if (selected) colors.surfaceSelected else Color.Transparent)
            .clickable(onClick = onClick)
            .padding(horizontal = ForgeSpace.lg)
            .semantics {
                contentDescription = if (selected) "$label, $inUse" else label
                role = Role.Button
                this.selected = selected
            },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.padding(end = ForgeSpace.md)) {
            Text(
                label,
                style = type.userBody,
                color = colors.chatText,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            detail?.let { Text(it, style = type.sessionMeta, color = colors.textMuted) }
        }
        if (selected) {
            Icon(
                Icons.Rounded.Check,
                contentDescription = null,
                tint = colors.accentSoft,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
        }
    }
}
