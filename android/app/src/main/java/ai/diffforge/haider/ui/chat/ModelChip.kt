package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.Skeleton
import ai.diffforge.haider.ui.state.ModelChipState
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ExpandMore
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow

/**
 * The model shortcut, extracted from the composer (UI-SPEC 3.6).
 *
 * The 970 chip printed `loading…` whenever the label was null, and with no saved
 * endpoint nothing was ever requested, so it sat on "loading…" forever (D4).
 * Here each failure has its own sentence: no daemon reads *Start Haider first*,
 * no catalog reads *Retry*.
 *
 * It is 32 dp, not 48: it is a shortcut, and the model is also reachable from
 * the drawer footer and the header overflow (UI-SPEC 4.3).
 */
@Composable
fun ModelChip(
    state: ModelChipState,
    onOpenModel: () -> Unit,
    onRetry: () -> Unit,
    onStartDaemon: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type

    when (state) {
        is ModelChipState.Resolved -> {
            // The value is the label. "MODEL Sonnet 4.5 · high" said the same
            // thing three times; the effort has its own chip (addition F, S5).
            ForgeChip(
                onClick = onOpenModel,
                modifier = modifier,
                contentDescription = "${stringResource(R.string.cd_change_model)}, ${state.fullModel}",
            ) {
                Text(
                    state.shortModel,
                    style = type.chip,
                    color = colors.textSoft,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Icon(
                    Icons.Rounded.ExpandMore,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
        }

        ModelChipState.Loading -> ForgeChip(
            onClick = onOpenModel,
            modifier = modifier,
            contentDescription = stringResource(R.string.chip_model_loading_cd),
        ) {
            Skeleton(width = ForgeSpace.huge * 2)
        }

        is ModelChipState.Error -> ForgeChip(
            onClick = onRetry,
            modifier = modifier,
            borderColor = colors.amber,
            backgroundColor = colors.amber.copy(alpha = 0.12f),
            contentDescription = stringResource(R.string.chip_model_error),
        ) {
            Text(
                stringResource(R.string.chip_model_error),
                style = type.chip,
                color = colors.amber,
                maxLines = 1,
            )
        }

        ModelChipState.DaemonDown -> ForgeChip(
            onClick = onStartDaemon,
            modifier = modifier,
            contentDescription = stringResource(R.string.chip_model_no_daemon),
        ) {
            Text(
                stringResource(R.string.chip_model_no_daemon),
                style = type.chip,
                color = colors.textMuted,
                maxLines = 1,
            )
        }

        ModelChipState.Changing -> ForgeChip(
            onClick = onOpenModel,
            modifier = modifier,
            contentDescription = stringResource(R.string.cd_change_model),
        ) {
            Text(stringResource(R.string.chip_model_changing), style = type.chip, color = colors.textSoft)
        }
    }
}
