package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.accounts.ModelInventoryAuthority
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
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction

/** Test handles for the custom-model entry. */
const val CUSTOM_MODEL_ENTRY_TAG = "custom_model_entry"
const val CUSTOM_MODEL_FIELD_TAG = "custom_model_field"

/**
 * A free-text model id, for providers that accept one.
 *
 * Only an ADVISORY inventory licenses this row (`ModelInventoryAuthorityWire`,
 * frame.rs:1226): a router or local server routinely omits ids from its
 * `/v1/models` that its chat wire still serves, and `ModelInventoryStatusWire`
 * names that case explicitly — "`Unlisted` is not `Available`: it is an honest
 * advisory-catalog miss that a custom compatible server may still accept".
 *
 * An authoritative catalog is the daemon's own; a miss there is a real miss and
 * `session.select_model` refuses it with `model_unknown`. An UNKNOWN authority —
 * an older summary — is treated as authoritative, because it stated nothing and
 * a guess in the permissive direction is the one that hurts.
 *
 * Nothing is validated here beyond emptiness and control characters. The
 * daemon is the authority on whether the id exists, and its answer arrives
 * through `session.select_model`'s refusal path — the same panel a cache-epoch
 * refusal lands in, which now tells the two apart.
 */
@Composable
fun CustomModelEntry(
    authority: ModelInventoryAuthority,
    enabled: Boolean,
    onSelect: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    if (!authority.acceptsCustomModelId) return
    val colors = Forge.colors
    val type = Forge.type
    val focus = LocalFocusManager.current
    var open by remember { mutableStateOf(false) }
    var id by remember { mutableStateOf("") }
    val valid = CustomModelId.ok(id)
    val label = stringResource(R.string.model_custom_entry)

    Column(
        modifier = modifier
            .fillMaxWidth()
            .testTag(CUSTOM_MODEL_ENTRY_TAG),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        if (!open) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = ForgeSize.touch)
                    .clip(ForgeShapes.row)
                    .clickable(enabled = enabled) { open = true }
                    .padding(horizontal = ForgeSpace.md)
                    .semantics {
                        contentDescription = label
                        role = Role.Button
                    },
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(label, style = type.userBody, color = colors.accentSoft)
            }
            return@Column
        }

        Text(stringResource(R.string.model_custom_field), style = type.sessionMeta, color = colors.textMuted)
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
                value = id,
                onValueChange = { id = it },
                singleLine = true,
                textStyle = type.chatBody.copy(color = colors.text),
                cursorBrush = SolidColor(colors.accent),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                keyboardActions = KeyboardActions(onDone = { focus.clearFocus() }),
                modifier = Modifier
                    .fillMaxWidth()
                    .testTag(CUSTOM_MODEL_FIELD_TAG),
            )
        }
        Text(
            stringResource(R.string.model_custom_hint),
            style = type.sessionMeta,
            color = colors.textMuted,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
            ForgeButton(
                text = stringResource(R.string.model_custom_use),
                onClick = { onSelect(id.trim()) },
                enabled = enabled && valid,
            )
            ForgeButton(
                text = stringResource(R.string.action_cancel),
                onClick = { open = false; id = "" },
                kind = ForgeButtonKind.Ghost,
            )
        }
    }
}

/** What this client may refuse locally about a passthrough model id. */
object CustomModelId {
    /** `session.select_model` sends `model: String`; only these two are ours to refuse. */
    fun ok(id: String): Boolean {
        val trimmed = id.trim()
        return trimmed.isNotEmpty() && trimmed.none(Char::isISOControl)
    }
}
