package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.R
import ai.diffforge.haider.ui.state.SelectionRefusal
import ai.diffforge.haider.ui.state.SelectionRefusalCodes
import ai.diffforge.haider.ui.state.SelectionRefusalKind
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource

/** Test handle for the refusal panel, on whichever surface is showing it. */
const val MODEL_REFUSAL_TAG = "model_refusal"

/**
 * What the daemon said when it refused a model or effort change, and the only
 * button in the app that may retry with `confirm_new_epoch`.
 *
 * There is one of these because there are two ways into a selection — the full
 * picker dialog and the composer's bottom sheets — and round 7 gave the
 * refusal to only the first. The sheet dismissed itself on tap, so the refusal
 * arrived at a surface that had already closed and nobody ever saw it
 * (verify-7 P2).
 */
@Composable
fun SelectionRefusalPanel(
    refusal: SelectionRefusal,
    onConfirm: () -> Unit,
    onKeep: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = modifier
            .fillMaxWidth()
            .padding(ForgeSpace.xxl)
            .testTag(MODEL_REFUSAL_TAG),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) {
        val kind = SelectionRefusalCodes.kind(refusal.code)
        Text(stringResource(R.string.model_refusal_title), style = type.h4, color = colors.text)
        Text(refusal.code, style = type.sessionMeta, color = colors.amber)
        // What the code MEANS for the row that was tapped, where this client
        // can say it from the coordinates it already sent. Nothing is inferred
        // beyond the code itself.
        when {
            refusal.code.contains(SelectionRefusalCodes.MODEL_UNKNOWN) &&
                refusal.model != null && refusal.provider != null ->
                Text(
                    stringResource(
                        R.string.model_refusal_unknown,
                        refusal.model,
                        refusal.provider,
                    ),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
            refusal.code.contains(SelectionRefusalCodes.PROVIDER_UNAVAILABLE) &&
                refusal.provider != null ->
                Text(
                    stringResource(R.string.model_refusal_provider, refusal.provider),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
            else -> Unit
        }
        if (kind != SelectionRefusalKind.Confirmable) {
            // A retry with `confirm_new_epoch` is the same request plus one
            // field, and this refusal is not about consent. Saying so is the
            // honest alternative to a button that would be refused identically.
            Text(
                stringResource(R.string.model_refusal_terminal),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
        }
        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
            // ONLY a confirmation refusal gets the button that sets
            // `confirm_new_epoch`, and only a person's tap sets it.
            if (kind == SelectionRefusalKind.Confirmable) {
                ForgeButton(
                    text = stringResource(R.string.model_refusal_confirm),
                    onClick = onConfirm,
                    kind = ForgeButtonKind.Filled,
                )
            }
            ForgeButton(
                text = stringResource(
                    if (kind == SelectionRefusalKind.Confirmable) {
                        R.string.model_refusal_keep
                    } else {
                        R.string.model_refusal_close
                    },
                ),
                onClick = onKeep,
                kind = ForgeButtonKind.Ghost,
            )
        }
    }
}
