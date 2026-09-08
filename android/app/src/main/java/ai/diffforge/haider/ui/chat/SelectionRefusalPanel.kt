package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.state.SelectionRefusal
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
        Text("The daemon refused that change", style = type.h4, color = colors.text)
        Text(refusal.code, style = type.sessionMeta, color = colors.amber)
        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
            ForgeButton(
                text = "Change it anyway",
                onClick = onConfirm,
                kind = ForgeButtonKind.Filled,
            )
            ForgeButton(
                text = "Keep the current one",
                onClick = onKeep,
                kind = ForgeButtonKind.Ghost,
            )
        }
    }
}
