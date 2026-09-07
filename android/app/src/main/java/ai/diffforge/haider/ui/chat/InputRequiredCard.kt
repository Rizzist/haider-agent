package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.daemon.MenuOption
import ai.diffforge.haider.daemon.NeedsInput
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.StateDot
import ai.diffforge.haider.ui.state.RelativeTime
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.BasicTextField
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
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics

/**
 * A transcript card, never a dialog (UI-SPEC 3.7).
 *
 * A dialog dies on rotation, cannot be scrolled back to, and cannot be
 * deep-linked from a notification. This can do all three.
 *
 * Buttons are styled from `options[].decision`, never by parsing the label, and
 * the answer carries `menu_id + request_seq + worker_generation + option_key +
 * option_index` from the same snapshot that rendered the card. The card never
 * animates: an attention state that flickers is harder to read, not easier.
 */
@Composable
fun InputRequiredCard(
    needsInput: NeedsInput,
    nowMs: Long,
    answeredElsewhere: Boolean,
    onAnswer: (optionKey: String, optionIndex: Int, text: String?) -> Unit,
    onOpenSecretVault: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    var freeText by remember { mutableStateOf("") }

    Column(
        modifier = modifier
            .fillMaxWidth()
            .clip(ForgeShapes.cardWide)
            .background(colors.surfaceRaised)
            .border(ForgeSize.hairline, colors.accent.copy(alpha = 0.55f), ForgeShapes.cardWide)
            .padding(ForgeSpace.xl)
            .semantics { liveRegion = LiveRegionMode.Assertive },
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.lg),
    ) {
        Text(stringResource(R.string.ask_eyebrow), style = type.drawerSection, color = colors.accent)
        Text(needsInput.displayTitle, style = type.h4, color = colors.text)

        // safe_body is rendered verbatim, never rewritten into prose.
        if (needsInput.safeBody.isNotEmpty()) {
            Column(
                Modifier
                    .fillMaxWidth()
                    .clip(ForgeShapes.quote)
                    .background(colors.bgDeep)
                    .border(ForgeSize.hairline, colors.border, ForgeShapes.quote)
                    .padding(ForgeSpace.lg),
                verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
            ) {
                needsInput.safeBody.forEach { line ->
                    Text(line, style = type.toolRow, color = colors.chatText)
                }
            }
        }

        when {
            answeredElsewhere -> Text(
                stringResource(R.string.ask_answered_elsewhere),
                style = type.sessionMeta,
                color = colors.textMuted,
            )

            // A secret must never travel as MenuInput::text.
            needsInput.secretAnswer -> Column(
                verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
            ) {
                Text(
                    stringResource(R.string.ask_secret_elsewhere),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
                ForgeButton(
                    text = stringResource(R.string.action_session_details),
                    onClick = onOpenSecretVault,
                    kind = ForgeButtonKind.Ghost,
                )
            }

            needsInput.options.isNotEmpty() -> OptionButtons(needsInput.options, onAnswer)

            else -> Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
            ) {
                Box(
                    Modifier
                        .weight(1f)
                        .heightIn(min = ForgeSize.actionButton)
                        .clip(ForgeShapes.cardTight)
                        .background(colors.surfaceControl)
                        .border(ForgeSize.hairline, colors.border, ForgeShapes.cardTight)
                        .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.lg),
                ) {
                    BasicTextField(
                        value = freeText,
                        onValueChange = { freeText = it },
                        singleLine = true,
                        textStyle = type.chatBody.copy(color = colors.text),
                        cursorBrush = SolidColor(colors.accent),
                        modifier = Modifier.fillMaxWidth(),
                        decorationBox = { inner ->
                            Box {
                                if (freeText.isEmpty()) {
                                    Text(
                                        stringResource(R.string.ask_free_text_hint),
                                        style = type.chatBody,
                                        color = colors.textMuted,
                                    )
                                }
                                inner()
                            }
                        },
                    )
                }
                ForgeButton(
                    text = stringResource(R.string.ask_send),
                    onClick = { onAnswer("", 0, freeText) },
                    enabled = freeText.isNotBlank(),
                )
            }
        }

        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            StateDot(colors.stateNeedsInput)
            Text(
                stringResource(R.string.ask_waiting, RelativeTime.waiting(needsInput.sinceMs, nowMs)),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
        }
    }
}

@Composable
private fun OptionButtons(
    options: List<MenuOption>,
    onAnswer: (String, Int, String?) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    if (options.size <= 2) {
        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.lg)) {
            options.forEachIndexed { index, option ->
                ForgeButton(
                    text = option.label,
                    onClick = { onAnswer(option.key, index, null) },
                    kind = option.kind(),
                    modifier = Modifier.weight(1f),
                )
            }
        }
    } else {
        Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
            options.forEachIndexed { index, option ->
                ForgeButton(
                    text = option.label,
                    onClick = { onAnswer(option.key, index, null) },
                    kind = option.kind(),
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        }
    }
    options.firstOrNull { it.decision == "allow_always" }?.let {
        Text(
            stringResource(R.string.ask_allow_always_note),
            style = type.sessionMeta,
            color = colors.textMuted,
        )
    }
}

/** Style from `decision`, never from the label text (UI-SPEC 3.7). */
fun MenuOption.kind(): ForgeButtonKind = when (decision) {
    "allow_once" -> ForgeButtonKind.Filled
    "reject_always" -> ForgeButtonKind.Destructive
    else -> ForgeButtonKind.Ghost
}
