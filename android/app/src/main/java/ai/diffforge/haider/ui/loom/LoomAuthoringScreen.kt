package ai.diffforge.haider.ui.loom

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource

const val AUTHORING_SCREEN_TAG = "loom_authoring_screen"
const val AUTHORING_PROSE_TAG = "loom_authoring_prose"
const val AUTHORING_TEXT_TAG = "loom_authoring_text"

/**
 * prompt → draft → revise → confirm.
 *
 * The screen never claims more than the daemon said. A draft is a draft until
 * `loom.author.confirm` comes back with a receipt; a confirm that answered
 * `confirmed: null` leaves the document on screen, its errors beside it, and
 * says plainly that nothing was registered. A daemon with no model to draft
 * with gets its own state with its own reason, because a spinner that never
 * ends is the same lie told slowly.
 */
@Composable
fun LoomAuthoringScreen(
    state: LoomAuthoringScreenState,
    onBack: () -> Unit,
    onKind: (LoomAuthorKind) -> Unit,
    onProse: (String) -> Unit,
    onText: (String) -> Unit,
    onDraft: () -> Unit,
    onRevise: () -> Unit,
    onValidate: () -> Unit,
    onConfirm: () -> Unit,
    onStartOver: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier
            .fillMaxSize()
            .background(colors.bg)
            .imePadding()
            .testTag(AUTHORING_SCREEN_TAG),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.header)
                .padding(horizontal = ForgeSpace.xs),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ForgeIconButton(
                onClick = onBack,
                contentDescription = stringResource(R.string.action_back),
            ) {
                Icon(
                    Icons.Rounded.ArrowBack,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.icon),
                )
            }
            Text(
                stringResource(
                    when (state.kind) {
                        LoomAuthorKind.AgentType -> R.string.authoring_title_agent_type
                        LoomAuthorKind.Workflow -> R.string.authoring_title_workflow
                    },
                ),
                style = type.sessionTitle,
                color = colors.text,
                modifier = Modifier.weight(1f).padding(start = ForgeSpace.md),
            )
        }

        Column(
            Modifier
                .weight(1f)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            when (val authoring = state.authoring) {
                is LoomAuthoringState.Unavailable -> Text(
                    stringResource(R.string.authoring_unavailable, authoring.reason),
                    style = type.emptyBody,
                    color = colors.textMuted,
                )

                is LoomAuthoringState.Confirmed -> Confirmed(
                    receipt = authoring.receipt,
                    onStartOver = onStartOver,
                )

                else -> {
                    KindPicker(state.kind, onKind, enabled = authoring is LoomAuthoringState.Idle)
                    Prompt(
                        prose = state.prose,
                        kind = state.kind,
                        enabled = authoring is LoomAuthoringState.Idle,
                        drafting = authoring is LoomAuthoringState.Drafting,
                        onProse = onProse,
                        onDraft = onDraft,
                    )
                    val draft = LoomAuthoringMachine.draftOf(authoring)
                    if (draft != null) {
                        DraftEditor(
                            authoring = authoring,
                            text = LoomAuthoringMachine.textOf(authoring),
                            onText = onText,
                            onRevise = onRevise,
                            onValidate = onValidate,
                            onConfirm = onConfirm,
                            onStartOver = onStartOver,
                        )
                    }
                    if (authoring is LoomAuthoringState.Failed) {
                        Text(
                            stringResource(R.string.authoring_failed, authoring.reason),
                            style = type.sessionMeta,
                            color = colors.red,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun KindPicker(
    kind: LoomAuthorKind,
    onKind: (LoomAuthorKind) -> Unit,
    enabled: Boolean,
) {
    val colors = Forge.colors
    Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
        LoomAuthorKind.entries.forEach { candidate ->
            val selected = candidate == kind
            ForgeChip(
                onClick = { if (enabled) onKind(candidate) },
                enabled = enabled,
                selected = selected,
                contentDescription = stringResource(kindLabel(candidate)),
            ) {
                Text(
                    stringResource(kindLabel(candidate)),
                    style = Forge.type.chip,
                    color = if (selected) colors.text else colors.textMuted,
                )
            }
        }
    }
}

private fun kindLabel(kind: LoomAuthorKind): Int = when (kind) {
    LoomAuthorKind.AgentType -> R.string.authoring_kind_agent_type
    LoomAuthorKind.Workflow -> R.string.authoring_kind_workflow
}

@Composable
private fun Prompt(
    prose: String,
    kind: LoomAuthorKind,
    enabled: Boolean,
    drafting: Boolean,
    onProse: (String) -> Unit,
    onDraft: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Text(
        stringResource(R.string.authoring_step_prompt),
        style = type.sessionMeta,
        color = colors.textSoft,
    )
    Field(
        value = prose,
        onValueChange = onProse,
        enabled = enabled,
        placeholder = stringResource(
            when (kind) {
                LoomAuthorKind.AgentType -> R.string.authoring_prompt_hint_agent
                LoomAuthorKind.Workflow -> R.string.authoring_prompt_hint_workflow
            },
        ),
        minHeight = ForgeSize.composerMin,
        mono = false,
        tag = AUTHORING_PROSE_TAG,
    )
    ForgeButton(
        text = stringResource(if (drafting) R.string.authoring_drafting else R.string.authoring_draft),
        onClick = onDraft,
        enabled = enabled && prose.isNotBlank(),
    )
}

@Composable
private fun DraftEditor(
    authoring: LoomAuthoringState,
    text: String,
    onText: (String) -> Unit,
    onRevise: () -> Unit,
    onValidate: () -> Unit,
    onConfirm: () -> Unit,
    onStartOver: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val errors = LoomAuthoringMachine.errorsOf(authoring)
    val busy = authoring is LoomAuthoringState.Confirming ||
        (authoring as? LoomAuthoringState.Editing)?.busy == true

    Text(
        stringResource(R.string.authoring_step_draft),
        style = type.sessionMeta,
        color = colors.textSoft,
    )
    Field(
        value = text,
        onValueChange = onText,
        enabled = !busy,
        placeholder = "",
        minHeight = ForgeSize.authoringEditorMin,
        // mono: the authoring document is a typed source file the daemon
        // re-parses character for character.
        mono = true,
        tag = AUTHORING_TEXT_TAG,
    )

    if (authoring is LoomAuthoringState.Refused) {
        // The daemon answered, and its answer was that nothing was registered.
        Text(
            stringResource(R.string.authoring_refused),
            style = type.sessionMeta,
            color = colors.amber,
        )
        authoring.reason?.let {
            Text(it, style = type.sessionMeta, color = colors.textMuted)
        }
    }

    if (errors.isNotEmpty()) {
        Text(
            pluralStringResource(R.plurals.authoring_errors, errors.size, errors.size),
            style = type.sessionMeta,
            color = colors.red,
        )
        errors.forEach { error ->
            Text(
                stringResource(
                    R.string.authoring_error_line,
                    error.line,
                    error.column,
                    error.field,
                    error.message,
                ),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
        }
    }
    (authoring as? LoomAuthoringState.Editing)?.validation?.canonicalDigestPreview?.let {
        // Named a preview because that is what it is: `loom.validate` registers
        // nothing, so this digest is not on any registry entry.
        // mono: a digest, compared rather than read.
        Text(
            stringResource(R.string.authoring_digest_preview, it),
            style = type.numeric,
            color = colors.textMuted,
        )
    }

    Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
        ForgeButton(
            text = stringResource(R.string.authoring_revise),
            onClick = onRevise,
            enabled = !busy,
            kind = ForgeButtonKind.Ghost,
            minHeight = ForgeSize.bannerAction,
        )
        ForgeButton(
            text = stringResource(R.string.authoring_check),
            onClick = onValidate,
            enabled = !busy && authoring is LoomAuthoringState.Editing,
            kind = ForgeButtonKind.Ghost,
            minHeight = ForgeSize.bannerAction,
        )
    }
    Text(
        stringResource(R.string.authoring_step_confirm),
        style = type.sessionMeta,
        color = colors.textSoft,
    )
    ForgeButton(
        text = stringResource(
            if (authoring is LoomAuthoringState.Confirming) {
                R.string.authoring_confirming
            } else {
                R.string.authoring_confirm
            },
        ),
        onClick = onConfirm,
        // Confirm is offered only for a draft the daemon last called valid: a
        // confirm of a document carrying errors asks for a refusal, and the
        // round trip is the user's time.
        enabled = LoomAuthoringMachine.canConfirm(authoring),
    )
    ForgeButton(
        text = stringResource(R.string.authoring_start_over),
        onClick = onStartOver,
        kind = ForgeButtonKind.Ghost,
        minHeight = ForgeSize.bannerAction,
    )
}

@Composable
private fun Confirmed(receipt: LoomAuthorConfirmed, onStartOver: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Text(
        stringResource(
            if (receipt.updated) R.string.authoring_confirmed else R.string.authoring_confirmed_noop,
            receipt.registrationId,
            receipt.rev,
        ),
        style = type.sessionTitle,
        color = colors.text,
    )
    // mono: the daemon-issued execution digest, compared against a pin.
    Text(
        stringResource(R.string.authoring_execution_digest, receipt.executionDigest),
        style = type.numeric,
        color = colors.textMuted,
    )
    receipt.installJobId?.let {
        Text(
            stringResource(R.string.authoring_install_job, it),
            style = type.sessionMeta,
            color = colors.textMuted,
        )
    }
    // mono: the canonical text the registry stored, byte for byte.
    Text(
        receipt.canonicalText,
        style = type.toolRow,
        color = colors.textSoft,
    )
    ForgeButton(
        text = stringResource(R.string.authoring_start_over),
        onClick = onStartOver,
        kind = ForgeButtonKind.Ghost,
    )
}

@Composable
private fun Field(
    value: String,
    onValueChange: (String) -> Unit,
    enabled: Boolean,
    placeholder: String,
    minHeight: androidx.compose.ui.unit.Dp,
    mono: Boolean,
    tag: String,
) {
    val colors = Forge.colors
    val type = Forge.type
    // mono: the authoring document is source; the prose prompt is not.
    val style = if (mono) type.toolRow else type.chatBody
    Column(
        Modifier
            .fillMaxWidth()
            .heightIn(min = minHeight)
            .clip(ForgeShapes.card)
            .background(colors.surfaceControl)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.card)
            .padding(ForgeSpace.lg),
    ) {
        if (value.isEmpty() && placeholder.isNotEmpty()) {
            Text(placeholder, style = style, color = colors.textDisabled)
        }
        BasicTextField(
            value = value,
            onValueChange = onValueChange,
            enabled = enabled,
            textStyle = style.copy(color = colors.text),
            cursorBrush = SolidColor(colors.accent),
            modifier = Modifier.fillMaxWidth().testTag(tag),
        )
    }
}
