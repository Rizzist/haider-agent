package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.state.ModelChipState
import ai.diffforge.haider.ui.state.ModelNames
import ai.diffforge.haider.ui.state.ComposerState
import ai.diffforge.haider.ui.state.SendButtonState
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Add
import androidx.compose.material.icons.rounded.ArrowUpward
import androidx.compose.material.icons.rounded.Stop
import androidx.compose.material3.CircularProgressIndicator
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
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow

/**
 * Two rows: a 32 dp context row and the input row (UI-SPEC 3.6).
 *
 * The 970 chip row forced `heightIn(min = 48.dp)` *and* vertical padding inside
 * a horizontal scroller, so a 48 dp pill sat above a 50 dp input and ate 100 dp
 * of the short axis (D5). The chip is 32 dp now and grows its touch box, not its
 * pixels. The placeholder moved from `textDisabled` (2.85:1) to `textMuted`
 * (5.27:1), which is D7.
 *
 * Voice is not shipped in 971: Android has no mic capture path
 * (docs/android-port-scope.md:186-191). The slot is reserved; no dead button is
 * drawn.
 */
@Composable
fun Composer(
    text: String,
    onTextChange: (String) -> Unit,
    composer: ComposerState,
    chip: ModelChipState,
    contextTokens: Long?,
    contextExact: Boolean?,
    onSend: () -> Unit,
    onStop: () -> Unit,
    onStartDaemon: () -> Unit,
    onOpenModel: () -> Unit,
    onRetryModels: () -> Unit,
    onAttach: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    var focused by remember { mutableStateOf(false) }

    Column(
        modifier = modifier
            .fillMaxWidth()
            .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.md),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.contextRow),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ModelChip(
                state = chip,
                onOpenModel = onOpenModel,
                onRetry = onRetryModels,
                onStartDaemon = onStartDaemon,
            )
            Box(Modifier.weight(1f))
            // Absent means unknown: no segment rather than a printed zero.
            ModelNames.tokens(contextTokens)?.let { tokens ->
                Text(
                    if (contextExact == true) {
                        stringResource(R.string.composer_context_exact, tokens)
                    } else {
                        stringResource(R.string.composer_context_estimated, tokens)
                    },
                    style = type.numeric,
                    color = colors.textMuted,
                    maxLines = 1,
                )
            }
        }

        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.composerMin)
                .clip(ForgeShapes.composer)
                .background(colors.surfaceRaised)
                .border(
                    ForgeSize.hairline,
                    if (focused) colors.focusRing else colors.borderStrong,
                    ForgeShapes.composer,
                )
                .padding(horizontal = ForgeSpace.xs),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ForgeIconButton(
                onClick = onAttach,
                contentDescription = stringResource(R.string.cd_attach),
                enabled = composer.inputEnabled,
            ) {
                Icon(
                    Icons.Rounded.Add,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.icon),
                )
            }
            BasicTextField(
                value = text,
                onValueChange = onTextChange,
                enabled = composer.inputEnabled,
                textStyle = type.chatBody.copy(color = colors.text),
                cursorBrush = SolidColor(colors.accent),
                maxLines = 6,
                modifier = Modifier
                    .weight(1f)
                    .onFocusChanged { focused = it.isFocused }
                    .padding(vertical = ForgeSpace.md),
                decorationBox = { inner ->
                    Box {
                        if (text.isEmpty()) {
                            Text(
                                stringResource(composer.placeholderRes),
                                style = type.chatBody,
                                color = colors.textMuted,
                            )
                        }
                        inner()
                    }
                },
            )
            SendControl(
                composer = composer,
                onSend = onSend,
                onStop = onStop,
            )
        }

        composer.helperRes?.let { helper ->
            Text(
                stringResource(helper),
                style = type.sessionMeta,
                color = colors.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier
                    .padding(start = ForgeSpace.xl)
                    .semantics { liveRegion = LiveRegionMode.Polite },
            )
        }
    }
}

@Composable
private fun SendControl(
    composer: ComposerState,
    onSend: () -> Unit,
    onStop: () -> Unit,
) {
    val colors = Forge.colors
    if (composer.button == SendButtonState.Starting) {
        Box(Modifier.size(ForgeSize.touch), contentAlignment = Alignment.Center) {
            CircularProgressIndicator(
                modifier = Modifier.size(ForgeSize.iconSm),
                color = colors.amber,
            )
        }
        return
    }
    val stopping = composer.button == SendButtonState.Stop
    val background = when {
        stopping -> colors.red.copy(alpha = 0.16f)
        composer.button.enabled -> colors.accent
        else -> colors.surfaceControl
    }
    ForgeIconButton(
        onClick = if (stopping) onStop else onSend,
        contentDescription = stringResource(composer.contentDescriptionRes),
        enabled = composer.button.enabled,
        background = background,
    ) {
        Icon(
            if (stopping) Icons.Rounded.Stop else Icons.Rounded.ArrowUpward,
            contentDescription = null,
            // A filled accent surface takes accentInk, never Color.White: white
            // on the 971 ember accent is 2.64:1 in dark (UI-SPEC 2.1).
            tint = when {
                stopping -> colors.red
                composer.button.enabled -> colors.accentInk
                else -> colors.textDisabled
            },
            modifier = Modifier.size(ForgeSize.iconSm),
        )
    }
}

/** The sticky stop chip that floats above the composer while a turn streams. */
@Composable
fun StickyStopChip(onStop: () -> Unit, modifier: Modifier = Modifier) {
    val colors = Forge.colors
    val type = Forge.type
    val stopLabel = stringResource(R.string.cd_stop_turn)
    Row(
        modifier = modifier
            .height(ForgeSize.stopChip)
            .clip(ForgeShapes.pill)
            .background(colors.red.copy(alpha = 0.16f))
            .border(ForgeSize.hairline, colors.red.copy(alpha = 0.5f), ForgeShapes.pill)
            .clickable(onClick = onStop)
            .semantics { contentDescription = stopLabel; role = Role.Button }
            .padding(horizontal = ForgeSpace.lg),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
    ) {
        Icon(
            Icons.Rounded.Stop,
            contentDescription = null,
            tint = colors.red,
            modifier = Modifier.size(ForgeSize.iconSm),
        )
        Text(stringResource(R.string.action_stop_turn), style = type.chip, color = colors.red)
    }
}
