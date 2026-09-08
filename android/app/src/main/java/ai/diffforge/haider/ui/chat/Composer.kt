package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.BrandMarkOnly
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.state.ComposerState
import ai.diffforge.haider.ui.state.ModelChipState
import ai.diffforge.haider.ui.state.ModelNames
import ai.diffforge.haider.ui.state.PermissionMode
import ai.diffforge.haider.ui.state.SendButtonState
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
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowUpward
import androidx.compose.material.icons.rounded.Attachment
import androidx.compose.material.icons.rounded.ExpandMore
import androidx.compose.material.icons.rounded.Mic
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
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow

/**
 * The reference composer (owner addition H3).
 *
 * `TerminalChatComposerControls` (dashboard.js:39675) is a wrapping row of
 * labelled pill selects, and `TerminalChatTextarea` (:40056) is a 52 px field
 * at radius 26 with the send (:40094) and mic (:40156) circles sitting inside
 * its right edge. Round 8's composer was two bare chips over a 56 dp rounded
 * row — the owner's "not good looking composer".
 *
 * The labels come back for exactly these three selects, which is the one
 * exemption H3 grants to G2: a value alone cannot say whether `Auto` is the
 * permission mode or the effort. Everything else in the app stays label-free.
 *
 * Voice: Android has no mic capture path in 971
 * (`docs/android-port-scope.md:186-191`). The circle is drawn because the
 * reference composer has it and the owner asked for it, and it is drawn
 * **disabled** with a description that says why — a control that pretends to
 * listen would be worse than one that admits it cannot.
 */
/** The text area inside the field, excluding the controls beside it. */
const val COMPOSER_TEXT_TAG = "composer_text"

/** The painted pill inside a select's 48 dp target. */
const val SELECT_INK_TAG = "select_ink"

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun Composer(
    showPickers: Boolean = true,
    text: String,
    onTextChange: (String) -> Unit,
    composer: ComposerState,
    chip: ModelChipState,
    effort: String?,
    permissionMode: PermissionMode,
    /** Blocks staged for this turn, mirrored back above the field. */
    attachments: List<ai.diffforge.haider.ui.daemon.Attachment> = emptyList(),
    /** The daemon's own refusal code, when it refused one. */
    attachmentNotice: String? = null,
    service: ai.diffforge.haider.ui.daemon.DaemonService? = null,
    queued: Int = 0,
    onRemoveAttachment: (String) -> Unit = {},
    onOpenQueue: () -> Unit = {},
    onSend: () -> Unit,
    onStop: () -> Unit,
    onStartDaemon: () -> Unit,
    onOpenModel: () -> Unit,
    onOpenEffort: () -> Unit,
    onOpenPermissions: () -> Unit,
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
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
    ) {
        if (showPickers) {
            // One row at 360 dp. Round 10 used a FlowRow and the third select
            // wrapped at exactly 360, a 52 dp displacement (verify-10 O3), so
            // the three share the width instead: each takes a third and its
            // value ellipsises rather than pushing the next one down. It still
            // wraps below 360, where a third of the width cannot hold a label
            // and a value.
            // A Row, not a FlowRow. `weight` inside a FlowRow is applied after
            // it has already decided to wrap, so the third select still moved
            // to a second line at exactly 360 dp (verify-10 O3). Three equal
            // thirds always share one row and ellipsise their value instead.
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                ModelSelect(
                    state = chip,
                    onOpenModel = onOpenModel,
                    onRetry = onRetryModels,
                    onStartDaemon = onStartDaemon,
                    modifier = Modifier.weight(1f),
                )
                LabelledSelect(
                    label = stringResource(R.string.select_label_effort),
                    value = effort ?: stringResource(R.string.select_value_default),
                    contentDescription = stringResource(R.string.cd_change_effort),
                    onClick = onOpenEffort,
                    modifier = Modifier.weight(1f),
                )
                LabelledSelect(
                    label = stringResource(R.string.select_label_permissions),
                    value = stringResource(
                        when (permissionMode) {
                            PermissionMode.Auto -> R.string.permission_mode_auto
                            PermissionMode.Ask -> R.string.permission_mode_ask
                        },
                    ),
                    contentDescription = stringResource(R.string.cd_change_permissions),
                    onClick = onOpenPermissions,
                    modifier = Modifier.weight(1f),
                )
            }
        }

        // The input mirror: what is going with this turn, above the field.
        if (attachments.isNotEmpty()) {
            AttachmentStrip(
                attachments = attachments,
                service = service,
                modifier = Modifier.fillMaxWidth(),
            )
        }
        attachmentNotice?.let { code ->
            Text(
                // The daemon's word, not ours.
                code,
                style = type.sessionMeta,
                color = colors.amber,
                maxLines = 2,
                modifier = Modifier.padding(start = ForgeSpace.xl),
            )
        }
        if (queued > 0) {
            Row(
                modifier = Modifier
                    .heightIn(min = ForgeSize.touch)
                    .clickable(onClick = onOpenQueue)
                    .semantics {
                        contentDescription = "$queued waiting to send"
                        role = Role.Button
                    }
                    .padding(horizontal = ForgeSpace.xl),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    "$queued waiting to send",
                    style = type.sessionMeta,
                    color = colors.accentSoft,
                )
            }
        }
        Box(modifier = Modifier.fillMaxWidth()) {
            BasicTextField(
                value = text,
                onValueChange = onTextChange,
                enabled = composer.inputEnabled,
                textStyle = type.chatBody.copy(color = colors.text),
                cursorBrush = SolidColor(colors.accent),
                maxLines = 6,
                // H3 puts Stop in the send circle while a turn runs, which
                // would otherwise take the mid-turn follow-up with it — the
                // exact objection addition E raised about replacing Send. The
                // keyboard's own Send action keeps that path open.
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                keyboardActions = KeyboardActions(
                    onSend = { if (composer.button.enabled) onSend() },
                ),
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = ForgeSize.composerField)
                    .clip(ForgeShapes.composer)
                    .background(colors.surfaceControl)
                    .border(
                        ForgeSize.hairline,
                        if (focused) colors.focusRing else colors.border,
                        ForgeShapes.composer,
                    )
                    .onFocusChanged { focused = it.isFocused },
                decorationBox = { inner ->
                    Row(
                        modifier = Modifier.heightIn(min = ForgeSize.composerField),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        // Attach is on the left, where the owner asked for it,
                        // and the text starts after it (round 10, R2).
                        CircleControl(
                            onClick = onAttach,
                            contentDescription = stringResource(R.string.cd_attach),
                            enabled = composer.inputEnabled,
                        ) {
                            Icon(
                                Icons.Rounded.Attachment,
                                contentDescription = null,
                                tint = colors.textMuted,
                                modifier = Modifier.size(ForgeSize.iconSm),
                            )
                        }
                        Box(
                            modifier = Modifier
                                .weight(1f)
                                .padding(horizontal = ForgeSpace.sm)
                                .testTag(COMPOSER_TEXT_TAG),
                        ) {
                            if (text.isEmpty()) {
                                Text(
                                    stringResource(composer.placeholderRes),
                                    style = type.chatBody,
                                    color = colors.textMuted,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                            }
                            inner()
                        }
                        // Laid out *after* the text, not over it. Round 10
                        // overlaid these on a full-width field, so a long line
                        // ran underneath the mic and the send circle
                        // (verify-9 V3).
                        CircleControl(
                            onClick = {},
                            contentDescription = stringResource(R.string.cd_voice_unavailable),
                            enabled = false,
                        ) {
                            Icon(
                                Icons.Rounded.Mic,
                                contentDescription = null,
                                tint = colors.textMuted,
                                modifier = Modifier.size(ForgeSize.iconSm),
                            )
                        }
                        if (composer.showStop) {
                            // Stop replaces send while a turn runs, and there
                            // is still exactly one of it (addition E2).
                            CircleControl(
                                onClick = onStop,
                                contentDescription = stringResource(R.string.cd_stop_turn),
                                enabled = true,
                                fill = colors.red.copy(alpha = 0.16f),
                                outline = colors.red.copy(alpha = 0.45f),
                            ) {
                                Icon(
                                    Icons.Rounded.Stop,
                                    contentDescription = null,
                                    tint = colors.red,
                                    modifier = Modifier.size(ForgeSize.iconSm),
                                )
                            }
                        } else {
                            SendCircle(composer = composer, onSend = onSend)
                        }
                    }
                },
            )
        }

        // Only the disconnected / error line G5 allows.
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

/**
 * `TerminalChatComposerControl` (:39685): a hairline pill carrying a tiny
 * tracked uppercase label, the value, and a chevron.
 */
@Composable
private fun LabelledSelect(
    label: String,
    value: String,
    contentDescription: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    leading: (@Composable () -> Unit)? = null,
) {
    val colors = Forge.colors
    val type = Forge.type
    Box(
        modifier = modifier
            .heightIn(min = ForgeSize.touch)
            .clickable(enabled = enabled, onClick = onClick)
            .semantics {
                this.contentDescription = "$contentDescription, $value"
                this.role = Role.Button
            },
        contentAlignment = Alignment.Center,
    ) {
        Row(
            modifier = Modifier
                .height(ForgeSize.chip)
                .testTag(SELECT_INK_TAG)
                .clip(ForgeShapes.pill)
                .background(colors.surfaceControl)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.pill)
                .padding(horizontal = ForgeSpace.sm),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
        ) {
            leading?.invoke()
            Text(label, style = type.selectLabel, color = colors.textMuted, maxLines = 1)
            Text(
                value,
                style = type.chip,
                color = colors.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            Icon(
                Icons.Rounded.ExpandMore,
                contentDescription = null,
                tint = colors.textMuted,
                modifier = Modifier.size(ForgeSize.iconXs),
            )
        }
    }
}

/** MODEL, with the brand mark folded in: the provider sheet is inside it. */
@Composable
private fun ModelSelect(
    state: ModelChipState,
    onOpenModel: () -> Unit,
    onRetry: () -> Unit,
    onStartDaemon: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val label = stringResource(R.string.select_label_model)
    when (state) {
        is ModelChipState.Resolved -> LabelledSelect(
            label = label,
            modifier = modifier,
            value = state.shortModel,
            contentDescription = stringResource(R.string.cd_change_model),
            onClick = onOpenModel,
            leading = {
                BrandMarkOnly(model = state.fullModel, provider = state.provider)
            },
        )
        ModelChipState.Loading -> LabelledSelect(
            label = label,
            modifier = modifier,
            value = stringResource(R.string.select_value_default),
            contentDescription = stringResource(R.string.chip_model_loading_cd),
            onClick = onOpenModel,
            enabled = false,
        )
        is ModelChipState.Error -> LabelledSelect(
            label = label,
            modifier = modifier,
            value = stringResource(R.string.chip_model_error),
            contentDescription = stringResource(R.string.chip_model_error),
            onClick = onRetry,
        )
        ModelChipState.DaemonDown -> LabelledSelect(
            label = label,
            modifier = modifier,
            value = stringResource(R.string.chip_model_no_daemon),
            contentDescription = stringResource(R.string.chip_model_no_daemon),
            onClick = onStartDaemon,
        )
        ModelChipState.Changing -> LabelledSelect(
            label = label,
            modifier = modifier,
            value = stringResource(R.string.chip_model_changing),
            contentDescription = stringResource(R.string.cd_change_model),
            onClick = onOpenModel,
            enabled = false,
        )
    }
}

/** A 34 dp circle inside a 48 dp target, as the reference draws them. */
@Composable
private fun CircleControl(
    onClick: () -> Unit,
    contentDescription: String,
    enabled: Boolean,
    fill: androidx.compose.ui.graphics.Color? = null,
    outline: androidx.compose.ui.graphics.Color? = null,
    content: @Composable () -> Unit,
) {
    val colors = Forge.colors
    ForgeIconButton(
        onClick = onClick,
        contentDescription = contentDescription,
        enabled = enabled,
        background = fill ?: androidx.compose.ui.graphics.Color.Transparent,
        visual = ForgeSize.composerCircle,
        border = outline ?: colors.border,
        content = content,
    )
}

@Composable
private fun SendCircle(composer: ComposerState, onSend: () -> Unit) {
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
    CircleControl(
        onClick = onSend,
        contentDescription = stringResource(composer.contentDescriptionRes),
        enabled = composer.button.enabled,
        fill = colors.accentWash,
        outline = colors.accentLine,
    ) {
        Icon(
            Icons.Rounded.ArrowUpward,
            contentDescription = null,
            tint = colors.accentSoft,
            modifier = Modifier.size(ForgeSize.iconSm),
        )
    }
}
