package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.daemon.ShellAvailability
import ai.diffforge.haider.ui.daemon.ShellExecution
import ai.diffforge.haider.ui.daemon.ShellExecutionRef
import ai.diffforge.haider.ui.daemon.ShellExecutionStatus
import ai.diffforge.haider.ui.daemon.ShellOutputStream
import ai.diffforge.haider.ui.state.ShellSubmission
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
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowUpward
import androidx.compose.material.icons.rounded.Stop
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
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
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle

const val SHELL_VIEW_TAG = "shell_view"

/** The command line itself, for tests; its Run circle sits beside it. */
const val SHELL_INPUT_TAG = "shell_input"

/**
 * Availability reasons that are observations about this client's connection or
 * a not-yet-finished capability check, not capability denials. The terminal
 * keeps rendering its cached redacted history for them, with input disabled
 * and a truthful inline notice (FACADE-SHELL.md: show cached history while
 * reconnecting; never synthesize completion, never resubmit).
 */
private val shellConnectivityReasons = setOf("not_observed", "checking", "disconnected", "daemon_unavailable")

/**
 * The only reasons whose honest explanation is the on-device process policy.
 * Every other denial gets the generic truthful card plus the verbatim code;
 * unknown additive reasons stay unavailable rather than mapping to success.
 */
private val shellPolicyReasons = setOf("process_exec_disabled", "policy_unavailable")

/**
 * The session's terminal (lane 972-android-shell, FACADE-SHELL.md).
 *
 * One-shot, non-PTY commands over the daemon's own door: history is the
 * facade's durable, already-redacted projection for the SELECTED session, so
 * the list survives tab switches, reconnects and process death without this
 * view inventing anything. `Reconnecting` is replay catching up — it is never
 * rendered as a reason to resubmit, and nothing here resubmits on its own.
 *
 * When the daemon does not offer a shell the tab says what is true and names
 * the daemon's own reason code; there is no local capability inference and no
 * permission UI of its own — Ask flows stay in the permission-card system.
 */
@Composable
fun ShellView(
    availability: ShellAvailability,
    executions: List<ShellExecution>,
    draft: String,
    pending: ShellSubmission?,
    notice: String?,
    busy: Boolean,
    onDraft: (String) -> Unit,
    onRun: () -> Unit,
    onRetry: () -> Unit,
    onDiscard: () -> Unit,
    onDismissNotice: () -> Unit,
    onCancel: (ShellExecutionRef) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    // `available` is a capability observation, not a curtain. `session_busy`
    // means the terminal's own command is running — render it, its streaming
    // output and its Stop. Connection/observation loss keeps the cached
    // redacted history on screen with input disabled. Only a genuine denial
    // closes the door with the card, and only policy reasons may claim policy.
    val busyObserved = !availability.available && availability.reason == "session_busy"
    val connectionLost = !availability.available && availability.reason in shellConnectivityReasons
    if (!availability.available && !busyObserved && !connectionLost) {
        ShellUnavailable(availability, modifier)
        return
    }

    // The facade accepts one nonterminal run per session; the affordance says
    // so up front instead of collecting a session_busy refusal.
    val nonterminal = executions.any {
        it.status == ShellExecutionStatus.Running || it.status == ShellExecutionStatus.Reconnecting
    }
    val canRun = pending == null && !busy && !nonterminal && !busyObserved && !connectionLost &&
        draft.isNotBlank()
    val listState = rememberLazyListState()
    LaunchedEffect(executions.size, executions.lastOrNull()?.output?.size) {
        if (executions.isNotEmpty()) listState.scrollToItem(executions.lastIndex)
    }
    Column(
        modifier = modifier
            .fillMaxSize()
            // The composer is not rendered under this tab, so the terminal
            // owns the bottom inset union of navigation bar and IME itself.
            .windowInsetsPadding(WindowInsets.safeDrawing.only(WindowInsetsSides.Bottom))
            .testTag(SHELL_VIEW_TAG),
    ) {
        LazyColumn(
            state = listState,
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth(),
            contentPadding = PaddingValues(horizontal = ForgeSpace.xl, vertical = ForgeSpace.md),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            if (executions.isEmpty()) {
                item {
                    Text(
                        stringResource(R.string.shell_empty),
                        style = type.sessionMeta,
                        color = colors.textMuted,
                    )
                }
            }
            items(executions, key = { it.ref.itemId }) { execution ->
                ShellExecutionCard(execution = execution, onCancel = onCancel)
            }
        }
        notice?.let { code ->
            val dismiss = stringResource(R.string.cd_dismiss_shell_notice)
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = ForgeSize.touch)
                    .clickable(onClick = onDismissNotice)
                    .semantics {
                        contentDescription = dismiss
                        role = Role.Button
                    }
                    .padding(horizontal = ForgeSpace.xl),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                // The daemon's own code, never a sentence invented for it.
                Text(code, style = type.sessionMeta, color = colors.amber, maxLines = 2)
            }
        }
        pending?.let { submission ->
            Column(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.xs),
                verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
            ) {
                Text(
                    stringResource(R.string.shell_uncertain_notice),
                    style = type.sessionMeta,
                    color = colors.amber,
                )
                // mono: the exact retained command a retry would resubmit.
                Text(
                    submission.command,
                    style = type.toolRow,
                    color = colors.textMuted,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm)) {
                    // Retry resubmits the SAME submission id; Discard abandons
                    // it. Nothing else may spend or replace that id.
                    ForgeButton(
                        text = stringResource(R.string.shell_retry),
                        onClick = onRetry,
                        enabled = !busy,
                        minHeight = ForgeSize.bannerAction,
                    )
                    ForgeButton(
                        text = stringResource(R.string.shell_discard),
                        onClick = onDiscard,
                        kind = ForgeButtonKind.Ghost,
                        enabled = !busy,
                        minHeight = ForgeSize.bannerAction,
                    )
                }
            }
        }
        if (connectionLost) {
            Column(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.xs),
                verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
            ) {
                // The truthful state of THIS client's connection — never a
                // policy claim, never a request to resubmit anything.
                Text(
                    stringResource(
                        if (availability.reason == "disconnected" || availability.reason == "daemon_unavailable") {
                            R.string.shell_reconnecting_notice
                        } else {
                            R.string.shell_checking_notice
                        },
                    ),
                    style = type.sessionMeta,
                    color = colors.amber,
                )
                // mono: the daemon's/facade's own reason code, quoted verbatim.
                availability.reason?.let { Text(it, style = type.toolRow, color = colors.textMuted) }
            }
        }
        ShellInputRow(
            draft = draft,
            enabled = pending == null && !connectionLost,
            canRun = canRun,
            onDraft = onDraft,
            onRun = onRun,
        )
    }
}

@Composable
private fun ShellInputRow(
    draft: String,
    enabled: Boolean,
    canRun: Boolean,
    onDraft: (String) -> Unit,
    onRun: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    var focused by remember { mutableStateOf(false) }
    val inputCd = stringResource(R.string.cd_shell_input)
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        BasicTextField(
            value = draft,
            onValueChange = onDraft,
            enabled = enabled,
            // mono: the command line being typed.
            textStyle = type.toolRow.copy(color = colors.text),
            cursorBrush = SolidColor(colors.accent),
            maxLines = 4,
            // A command is typed verbatim: no autocorrect, no capitalisation.
            // The keyboard's Send runs it, like the composer's does.
            keyboardOptions = KeyboardOptions(
                capitalization = KeyboardCapitalization.None,
                autoCorrectEnabled = false,
                imeAction = ImeAction.Send,
            ),
            keyboardActions = KeyboardActions(onSend = { if (canRun) onRun() }),
            modifier = Modifier
                .weight(1f)
                .heightIn(min = ForgeSize.composerField)
                .clip(ForgeShapes.cardTight)
                .background(colors.surfaceControl)
                .border(
                    ForgeSize.hairline,
                    if (focused) colors.focusRing else colors.border,
                    ForgeShapes.cardTight,
                )
                .onFocusChanged { focused = it.isFocused }
                .semantics { contentDescription = inputCd }
                .testTag(SHELL_INPUT_TAG),
            decorationBox = { inner ->
                Row(
                    modifier = Modifier
                        .heightIn(min = ForgeSize.composerField)
                        .padding(horizontal = ForgeSpace.md),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    // mono: the prompt glyph of a command line.
                    Text("$", style = type.toolStrong, color = colors.textMuted)
                    Box(
                        Modifier
                            .weight(1f)
                            .padding(start = ForgeSpace.sm),
                    ) {
                        if (draft.isEmpty()) {
                            Text(
                                stringResource(R.string.shell_placeholder),
                                // mono: the hint sits where the command will.
                                style = type.toolRow,
                                color = colors.textMuted,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                        inner()
                    }
                }
            },
        )
        ForgeIconButton(
            onClick = onRun,
            contentDescription = stringResource(R.string.cd_shell_run),
            enabled = canRun,
            background = colors.accentWash,
            visual = ForgeSize.composerCircle,
            border = colors.accentLine,
        ) {
            Icon(
                Icons.Rounded.ArrowUpward,
                contentDescription = null,
                tint = colors.accentSoft,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
        }
    }
}

/**
 * One command and everything the daemon durably said about it. The Stop
 * affordance exists only while the projection reports a nonterminal status,
 * and it cancels with the execution's own retained coordinates.
 */
@Composable
private fun ShellExecutionCard(
    execution: ShellExecution,
    onCancel: (ShellExecutionRef) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    val nonterminal = execution.status == ShellExecutionStatus.Running ||
        execution.status == ShellExecutionStatus.Reconnecting
    Column(
        modifier = modifier
            .fillMaxWidth()
            .clip(ForgeShapes.cardTight)
            .background(colors.surface)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.cardTight)
            .padding(ForgeSpace.md),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            // mono: the command the daemon ran, prompt-prefixed.
            Text(
                "$ ${execution.command}",
                style = type.toolStrong,
                color = colors.text,
                modifier = Modifier.weight(1f),
            )
            if (nonterminal) {
                ForgeIconButton(
                    onClick = { onCancel(execution.ref) },
                    contentDescription = stringResource(R.string.cd_shell_stop),
                    background = colors.red.copy(alpha = 0.16f),
                    visual = ForgeSize.composerCircle,
                    border = colors.red.copy(alpha = 0.45f),
                ) {
                    Icon(
                        Icons.Rounded.Stop,
                        contentDescription = null,
                        tint = colors.red,
                        modifier = Modifier.size(ForgeSize.iconSm),
                    )
                }
            }
        }
        val segments = remember(execution.output) { ShellSegments.of(execution.output) }
        if (segments.isNotEmpty()) {
            // Interleaved in durable seq order; stderr is italic AND tinted so
            // the distinction does not hang on colour alone.
            val text = buildAnnotatedString {
                segments.forEach { segment ->
                    if (segment.stream == ShellOutputStream.Stderr) {
                        withStyle(SpanStyle(color = colors.red, fontStyle = FontStyle.Italic)) {
                            append(segment.text)
                        }
                    } else {
                        append(segment.text)
                    }
                }
            }
            // mono: the command's own stdout/stderr bytes.
            Text(text, style = type.toolRow, color = colors.chatText)
        }
        if (execution.outputTruncated) {
            // The 256 KiB UI projection bound, stated rather than hidden. The
            // daemon's own 1 MiB execution limit is a different fact.
            Text(
                stringResource(R.string.shell_output_truncated),
                style = type.sessionMeta,
                color = colors.amber,
            )
        }
        when (execution.status) {
            ShellExecutionStatus.Running -> Text(
                stringResource(R.string.shell_status_running),
                style = type.sessionMeta,
                color = colors.stateRunning,
            )
            // Replay catching up on a previously observed command. A statement
            // about THIS client's view — never a request to resubmit.
            ShellExecutionStatus.Reconnecting -> Text(
                stringResource(R.string.shell_status_reconnecting),
                style = type.sessionMeta,
                color = colors.amber,
            )
            ShellExecutionStatus.Completed -> {
                val exit = execution.exitCode
                Text(
                    stringResource(R.string.shell_status_exit, exit?.toString() ?: "?"),
                    style = type.sessionMeta,
                    color = if (exit == 0) colors.stateRunning else colors.red,
                )
            }
            ShellExecutionStatus.Cancelled -> Text(
                stringResource(R.string.shell_status_cancelled),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
            ShellExecutionStatus.Error -> Text(
                stringResource(R.string.shell_status_error, execution.error ?: "unknown"),
                style = type.sessionMeta,
                color = colors.red,
            )
        }
    }
}

/**
 * The honest closed door: the daemon's own reason, verbatim. The policy
 * sentence is reserved for reasons that ARE the process policy; every other
 * denial states only that the daemon is not offering a shell here.
 */
@Composable
private fun ShellUnavailable(
    availability: ShellAvailability,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    val policy = availability.reason in shellPolicyReasons
    Column(
        modifier = modifier
            .fillMaxSize()
            .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.xl),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .clip(ForgeShapes.card)
                .background(colors.surface)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.card)
                .padding(ForgeSpace.xl)
                .testTag(SHELL_VIEW_TAG),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Text(
                stringResource(
                    if (policy) R.string.shell_unavailable_title else R.string.shell_unavailable_generic_title,
                ),
                style = type.sessionTitle,
                color = colors.text,
            )
            Text(
                stringResource(
                    if (policy) R.string.shell_unavailable_body else R.string.shell_unavailable_generic_body,
                ),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
            availability.reason?.let { reason ->
                // The daemon's own code, in the one type face that is allowed
                // to be monospace: this is machine output (addition F, G1).
                // mono: the daemon's own reason code, quoted verbatim.
                Text(reason, style = type.toolRow, color = colors.textMuted)
            }
        }
    }
}
