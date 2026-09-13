package ai.diffforge.haider.ui.settings

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.accounts.CustomApiFamily
import ai.diffforge.haider.ui.accounts.CustomAuthMode
import ai.diffforge.haider.ui.accounts.CustomServerForm
import ai.diffforge.haider.ui.accounts.CustomServerPhase
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.components.LabelledField
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material.icons.rounded.Visibility
import androidx.compose.material.icons.rounded.VisibilityOff
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow

/** Test handles for the custom-server card. */
const val CUSTOM_SERVER_CARD_TAG = "custom_server_card"
const val CUSTOM_SERVER_KEY_FIELD_TAG = "custom_server_key_field"
const val CUSTOM_SERVER_MODEL_FIELD_TAG = "custom_server_model_field"

/**
 * The phone's `+ Add custom server` card: alias, base URL, key, auth mode, API
 * flavour -> discover -> pick a model.
 *
 * It mirrors the desktop TUI's card rather than inventing a phone flow, because
 * the two clients drive the same four doors and a person who has configured a
 * local server on the desktop should recognise this one. What changes is the
 * ergonomics: the TUI's Tab ladder becomes fields in reading order, its
 * space-bar toggles become chips, and its `[1]…[n]` model list becomes rows.
 *
 * The key is masked, is held in the controller's [ai.diffforge.haider.ui.accounts.SecretBuffer],
 * and is gone from this screen the moment discovery starts — the field renders
 * dots from a LENGTH, never from a value it is storing.
 */
@Composable
fun CustomServerCard(
    form: CustomServerForm,
    keyText: String,
    onKeyText: (String) -> Unit,
    onEdit: ((CustomServerForm) -> CustomServerForm) -> Unit,
    onAuthMode: (CustomAuthMode) -> Unit,
    onSubmit: () -> Unit,
    onCancel: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    var revealKey by remember { mutableStateOf(false) }
    val phase = form.phase
    Column(
        modifier = Modifier.testTag(CUSTOM_SERVER_CARD_TAG),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.lg),
    ) {
        Text(
            stringResource(R.string.accounts_custom_title),
            style = type.h4,
            color = colors.text,
        )

        LabelledField(
            label = stringResource(R.string.accounts_custom_name),
            value = form.name,
            onValueChange = { value -> onEdit { it.copy(name = value) } },
        )
        if (form.name.isNotBlank() && !CustomServerForm.aliasOk(form.name)) {
            CardNote(stringResource(R.string.accounts_custom_name_invalid), colors.amber)
        }
        LabelledField(
            label = stringResource(R.string.accounts_custom_origin),
            value = form.origin,
            onValueChange = { value -> onEdit { it.copy(origin = value) } },
        )

        ChoiceRow(
            label = stringResource(R.string.accounts_custom_auth),
            options = listOf(
                CustomAuthMode.ApiKey to stringResource(R.string.accounts_custom_auth_key),
                CustomAuthMode.None to stringResource(R.string.accounts_custom_auth_none),
            ),
            selected = form.authMode,
            enabled = !form.busy,
            onSelect = onAuthMode,
        )
        ChoiceRow(
            label = stringResource(R.string.accounts_custom_family),
            options = CustomApiFamily.entries.map { it to it.label },
            selected = form.family,
            enabled = !form.busy,
            onSelect = { family -> onEdit { it.copy(family = family) } },
        )

        if (!form.keyless) {
            LabelledField(
                label = stringResource(R.string.accounts_api_key),
                value = keyText,
                onValueChange = onKeyText,
                masked = !revealKey,
                tag = CUSTOM_SERVER_KEY_FIELD_TAG,
                trailing = {
                    ForgeIconButton(
                        onClick = { revealKey = !revealKey },
                        contentDescription = stringResource(
                            if (revealKey) R.string.cd_hide_key else R.string.cd_show_key,
                        ),
                    ) {
                        Icon(
                            if (revealKey) Icons.Rounded.VisibilityOff else Icons.Rounded.Visibility,
                            contentDescription = null,
                            tint = colors.textMuted,
                            modifier = Modifier.size(ForgeSize.iconSm),
                        )
                    }
                },
            )
            CardNote(stringResource(R.string.accounts_custom_key_probe), colors.textMuted)
        } else {
            CardNote(stringResource(R.string.accounts_custom_keyless), colors.textMuted)
        }

        when (phase) {
            is CustomServerPhase.Editing ->
                phase.error?.let { CardNote(it, colors.red) }

            CustomServerPhase.Probing ->
                CardNote(stringResource(R.string.accounts_custom_probing), colors.accent)

            is CustomServerPhase.Choosing -> {
                phase.error?.let { CardNote(it, colors.amber) }
                if (phase.models.isEmpty()) {
                    // The manual fallback. The server answered with something
                    // unusable, and a passthrough id it will still accept is a
                    // fact only the person has.
                    LabelledField(
                        label = stringResource(R.string.accounts_custom_model_manual),
                        value = form.model,
                        onValueChange = { value -> onEdit { it.copy(model = value) } },
                        tag = CUSTOM_SERVER_MODEL_FIELD_TAG,
                    )
                } else {
                    Text(
                        stringResource(R.string.accounts_custom_model),
                        style = type.sessionMeta,
                        color = colors.textMuted,
                    )
                    phase.models.forEach { id ->
                        ModelRow(
                            id = id,
                            selected = form.model == id,
                            onClick = { onEdit { it.copy(model = id) } },
                        )
                    }
                }
            }

            CustomServerPhase.Submitting ->
                CardNote(stringResource(R.string.accounts_custom_creating), colors.accent)
        }

        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
            ForgeButton(
                text = stringResource(
                    if (phase is CustomServerPhase.Choosing) {
                        R.string.accounts_custom_create
                    } else {
                        R.string.accounts_custom_discover
                    },
                ),
                onClick = onSubmit,
                enabled = if (phase is CustomServerPhase.Choosing) {
                    form.canConfigure
                } else {
                    form.canProbe
                },
            )
            ForgeButton(
                text = stringResource(R.string.action_cancel),
                onClick = onCancel,
                kind = ForgeButtonKind.Ghost,
                enabled = !form.busy,
            )
        }
    }
}

@Composable
private fun CardNote(text: String, color: androidx.compose.ui.graphics.Color) {
    Text(text, style = Forge.type.sessionMeta, color = color)
}

/**
 * A labelled row of chips. `ForgeChip` derives `selected` semantics from its
 * `selected` parameter, so TalkBack says which one is in force rather than
 * reading four buttons with no state.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun <T> ChoiceRow(
    label: String,
    options: List<Pair<T, String>>,
    selected: T,
    enabled: Boolean,
    onSelect: (T) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
        Text(label, style = type.sessionMeta, color = colors.textMuted)
        FlowRow(
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
        ) {
            options.forEach { (value, text) ->
                val isSelected = value == selected
                ForgeChip(
                    onClick = { onSelect(value) },
                    selected = isSelected,
                    enabled = enabled,
                    contentDescription = text,
                ) {
                    Text(
                        text,
                        style = type.chip,
                        color = if (isSelected) colors.accent else colors.textSoft,
                    )
                }
            }
        }
    }
}

@Composable
private fun ModelRow(id: String, selected: Boolean, onClick: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.row)
            .background(if (selected) colors.surfaceSelected else androidx.compose.ui.graphics.Color.Transparent)
            .clickable(onClick = onClick)
            .padding(horizontal = ForgeSpace.lg)
            .semantics {
                contentDescription = id
                role = Role.Button
                this.selected = selected
            },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            id,
            style = type.userBody,
            color = colors.chatText,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(end = ForgeSpace.md),
        )
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
