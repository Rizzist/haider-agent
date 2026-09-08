package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.Skeleton
import ai.diffforge.haider.ui.daemon.ProviderInventory
import ai.diffforge.haider.ui.state.ModelNames
import ai.diffforge.haider.ui.components.BrandMarkOnly
import ai.diffforge.haider.ui.state.PermissionMode
import ai.diffforge.haider.ui.state.SelectionRefusal
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.width
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow

/** Which composer chip a picker sheet belongs to. */
/**
 * Two pickers, not three: the model sheet is grouped by provider, so choosing
 * a model chooses its provider (addition F, S5).
 */
enum class PickerKind { Model, Effort, Permissions }

/**
 * Provider / model / effort pickers for the composer bar, matching the desktop
 * client's chips row (`SessionComposer.jsx` model + effort chips) but sized for
 * a phone: each chip opens a sheet rather than an inline dropdown.
 *
 * Everything here reads `provider.list` through
 * [ai.diffforge.haider.ui.daemon.ProviderInventory] and writes through the
 * canonical session-selection calls on the facade. A provider whose
 * availability is not `available` is shown with its reason and cannot be
 * chosen — the inventory's own truth, not a guess.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SessionPickerSheet(
    kind: PickerKind,
    inventory: ProviderInventory,
    currentProvider: String?,
    currentModel: String?,
    currentEffort: String?,
    /**
     * True only while a catalog request is still inside the 6 s deadline. Once
     * it expires — or if nothing was ever requested — the sheet says so and
     * offers Retry rather than spinning. First-run "Pick a model" sat on
     * "Asking the daemon for its model catalog…" indefinitely.
     */
    pending: Boolean,
    /** True while a selection is in flight; the sheet waits for its answer. */
    busy: Boolean,
    /** What the daemon said, if it refused. Rendered here, never swallowed. */
    refusal: SelectionRefusal?,
    onRetry: () -> Unit,
    onDismiss: () -> Unit,
    onSelectModel: (provider: String, model: String) -> Unit,
    onSelectEffort: (String?) -> Unit,
    permissionMode: PermissionMode,
    onSelectPermissionMode: (PermissionMode) -> Unit,
    onConfirmRefused: () -> Unit,
    onDismissRefusal: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    // Tapping a row used to dismiss the sheet on the spot, so a refusal landed
    // on a surface that had already closed (verify-7 P2). The sheet now stays
    // until the selection actually lands, and closes itself when it does.
    var submitted by remember { mutableStateOf(false) }
    LaunchedEffect(submitted, busy, refusal) {
        if (submitted && !busy && refusal == null) onDismiss()
    }
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier
                .padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Text(
                stringResource(
                    when (kind) {
                        PickerKind.Model -> R.string.picker_model_title
                        PickerKind.Effort -> R.string.picker_effort_title
                        PickerKind.Permissions -> R.string.picker_permissions_title
                    },
                ),
                style = type.h4,
                color = colors.text,
            )

            if (refusal != null) {
                SelectionRefusalPanel(
                    refusal = refusal,
                    onConfirm = onConfirmRefused,
                    onKeep = { submitted = false; onDismissRefusal() },
                )
                return@Column
            }

            when (kind) {
                PickerKind.Model -> {
                    if (inventory.providers.none { it.models.isNotEmpty() }) {
                        EmptyInventory(pending, onRetry)
                    }
                    // Grouped by provider, as the desktop chip is.
                    inventory.providers.forEach { option ->
                        if (option.models.isEmpty()) return@forEach
                        // Sentence case: the only uppercase left in the app is
                        // the drawer's optional section headers (addition F, G2).
                        Text(
                            if (option.available) {
                                option.label
                            } else {
                                stringResource(
                                    R.string.picker_unavailable,
                                    option.label,
                                    option.unavailableReason.orEmpty(),
                                )
                            },
                            style = type.sessionMeta,
                            color = colors.textMuted,
                            modifier = Modifier.padding(top = ForgeSpace.md),
                        )
                        option.models.forEach { model ->
                            PickerRow(
                                label = ModelNames.short(model.id),
                                secondary = model.id,
                                selected = model.id == currentModel && option.id == currentProvider,
                                enabled = option.available,
                                onClick = { submitted = true; onSelectModel(option.id, model.id) },
                                // The mark belongs on the surface people
                                // actually pick from (verify-8 O4).
                                leading = {
                                    BrandMarkOnly(model = model.id, provider = option.id)
                                },
                            )
                        }
                    }
                }

                // Auto is the owner's "model work should be automated": one
                // standing consent, given once, instead of a card per SMS read
                // (addition H6). Ask is the only other value — there is no
                // per-tool matrix to get lost in.
                PickerKind.Permissions -> {
                    PermissionMode.entries.forEach { mode ->
                        PickerRow(
                            label = stringResource(
                                when (mode) {
                                    PermissionMode.Auto -> R.string.permission_mode_auto
                                    PermissionMode.Ask -> R.string.permission_mode_ask
                                },
                            ),
                            secondary = stringResource(
                                when (mode) {
                                    PermissionMode.Auto -> R.string.permission_mode_auto_detail
                                    PermissionMode.Ask -> R.string.permission_mode_ask_detail
                                },
                            ),
                            selected = mode == permissionMode,
                            enabled = mode != permissionMode,
                            onClick = { onSelectPermissionMode(mode); onDismiss() },
                        )
                    }
                }

                PickerKind.Effort -> {
                    // Per *model*, not per provider: Opus allows medium and
                    // high where Sonnet also allows low, and offering `low` for
                    // Opus is offering something the catalog rejects.
                    val efforts = inventory.effortsFor(currentProvider, currentModel)
                    if (efforts.isEmpty()) EmptyInventory(pending, onRetry)
                    efforts.forEach { effort ->
                        PickerRow(
                            label = effort,
                            secondary = null,
                            selected = effort == currentEffort,
                            enabled = true,
                            onClick = { submitted = true; onSelectEffort(effort) },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun EmptyInventory(pending: Boolean, onRetry: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    if (pending) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Skeleton(width = ForgeSpace.huge * 3)
            Text(
                stringResource(R.string.picker_loading),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
        }
        return
    }
    Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
        Text(stringResource(R.string.picker_empty), style = type.sessionMeta, color = colors.textMuted)
        ForgeButton(
            text = stringResource(R.string.action_retry),
            onClick = onRetry,
            kind = ForgeButtonKind.Ghost,
        )
    }
}

@Composable
private fun PickerRow(
    label: String,
    secondary: String?,
    selected: Boolean,
    enabled: Boolean,
    onClick: () -> Unit,
    leading: (@Composable () -> Unit)? = null,
) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.row)
            .clickable(enabled = enabled, onClick = onClick)
            .alpha(if (enabled) 1f else 0.55f)
            .padding(horizontal = ForgeSpace.lg)
            .semantics {
                contentDescription = label
                role = Role.Button
                this.selected = selected
            },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        leading?.let {
            it()
            Spacer(Modifier.width(ForgeSpace.md))
        }
        Column(Modifier.weight(1f)) {
            Text(label, style = type.button, color = if (selected) colors.accent else colors.text)
            secondary?.let {
                Text(
                    // Metadata reads on the regular ramp (addition F, G1).
                    it,
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        if (selected) {
            Icon(
                Icons.Rounded.Check,
                contentDescription = null,
                tint = colors.accent,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
        } else {
            Box(Modifier.size(ForgeSize.iconSm))
        }
    }
}
