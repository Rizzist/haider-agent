package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.state.RelativeTime
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.rememberBlink
import ai.diffforge.haider.ui.components.BrandMarkOnly
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.motionEnabled
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.scrollBy
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ExpandMore
import androidx.compose.material.icons.rounded.KeyboardArrowRight
import androidx.compose.material3.Icon
import androidx.compose.material3.minimumInteractiveComponentSize
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.flow.distinctUntilChanged
import org.json.JSONArray
import org.json.JSONObject

@Composable
fun Transcript(
    messages: List<Message>,
    onRetry: () -> Unit = {},
    modifier: Modifier = Modifier,
    /** Only for fetching attachment bytes by CAS ref; null renders tiles bare. */
    service: ai.diffforge.haider.ui.daemon.DaemonService? = null,
) {
    val listState = rememberLazyListState()
    var stickToBottom by remember { mutableStateOf(true) }
    val nearBottomPx = with(LocalDensity.current) { (ForgeSpace.huge + ForgeSpace.huge).roundToPx() }

    LaunchedEffect(listState, nearBottomPx) {
        snapshotFlow {
            val layout = listState.layoutInfo
            val last = layout.visibleItemsInfo.lastOrNull()
            val distance = if (last?.index == layout.totalItemsCount - 1) {
                (last.offset + last.size - layout.viewportEndOffset).coerceAtLeast(0)
            } else {
                Int.MAX_VALUE
            }
            listState.isScrollInProgress to (distance <= nearBottomPx)
        }
            .distinctUntilChanged()
            .collect { (scrolling, nearBottom) ->
                if (scrolling) stickToBottom = nearBottom
            }
    }
    LaunchedEffect(messages.lastOrNull()) {
        if (stickToBottom && messages.isNotEmpty()) {
            listState.scrollToItem(messages.lastIndex)
            listState.scrollBy(Float.MAX_VALUE)
        }
    }

    LazyColumn(
        state = listState,
        modifier = modifier.fillMaxSize(),
        contentPadding = PaddingValues(
            start = ForgeSpace.xl,
            end = ForgeSpace.xl,
            top = ForgeSpace.xl,
            bottom = ForgeSpace.xl,
        ),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xl),
    ) {
        items(messages, key = { it.id }) { message ->
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.Center) {
                Box(modifier = Modifier.widthIn(max = ForgeSize.readableMax).fillMaxWidth()) {
                    when (message.role) {
                        Role.User -> UserBubble(message, service)
                        Role.Agent -> AgentTurn(message, onRetry, service)
                    }
                }
            }
        }
    }
}

@Composable
private fun UserBubble(message: Message, service: ai.diffforge.haider.ui.daemon.DaemonService?) {
    val colors = Forge.colors
    val type = Forge.type
    BoxWithConstraints(modifier = Modifier.fillMaxWidth()) {
        val maxBubbleWidth = maxWidth * 0.72f
        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
            Box(
                modifier = Modifier
                    .widthIn(max = maxBubbleWidth)
                    .clip(ForgeShapes.userBubble)
                    .background(colors.accentWash)
                    .border(
                        ForgeSize.hairline,
                        colors.accent.copy(alpha = 0.30f),
                        ForgeShapes.userBubble,
                    )
                    .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
            ) {
                Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.sm)) {
                    // Thumbnails above the text, the way the desktop mirrors
                    // an attached image back at you.
                    AttachmentStrip(message.attachments, service)
                    if (message.text.isNotEmpty()) {
                        Text(message.text, style = type.userBody, color = colors.chatText)
                    }
                }
            }
        }
    }
}

@Composable
private fun AgentTurn(
    message: Message,
    onRetry: () -> Unit,
    service: ai.diffforge.haider.ui.daemon.DaemonService?,
) {
    Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.Top) {
        BrandMarkOnly(
            model = null,
            provider = message.provider,
            modifier = Modifier.padding(top = ForgeSpace.xxs),
        )
        Spacer(Modifier.width(ForgeSpace.md))
        Box(modifier = Modifier.weight(1f)) {
            Column(
                modifier = Modifier.widthIn(max = ForgeSize.proseMax),
                verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
            ) {
                if (message.thinking.isNotEmpty()) ThinkingFold(message)
                if (message.tools.isNotEmpty()) ToolCluster(message.id, message.tools, message.streaming)
                if (message.text.isNotEmpty() || message.streaming) AssistantProse(message)
                AttachmentStrip(message.attachments, service)
                message.error?.let { ErrorCard(it, message.errorRetryable, onRetry) }
                // The turn's own tokens, when usage.report attributed any.
                message.usage?.let { UsageLine(it) }
            }
        }
    }
}

@Composable
private fun AssistantProse(message: Message) {
    // The caret is a boolean now, not an animated alpha threaded through the
    // markdown builder: it toggles at the shared ticker's rate, so the text is
    // laid out ~3 times a second instead of 60 (verify-10 O5).
    val caretVisible = rememberBlink(active = message.streaming)
    MarkdownText(message.text, showCaret = message.streaming && caretVisible)
}

@Composable
private fun ThinkingFold(message: Message) {
    val colors = Forge.colors
    val type = Forge.type
    var expanded by rememberSaveable(message.id) { mutableStateOf(message.streaming) }
    LaunchedEffect(message.streaming) {
        expanded = message.streaming
    }
    Column {
        Row(
            modifier = Modifier
                .clip(ForgeShapes.cardTight)
                .clickable { expanded = !expanded }
                .minimumInteractiveComponentSize()
                .padding(end = ForgeSpace.md),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                if (expanded) Icons.Rounded.ExpandMore else Icons.Rounded.KeyboardArrowRight,
                contentDescription = if (expanded) "Collapse thinking" else "Expand thinking",
                tint = colors.accent,
                modifier = Modifier.size(ForgeSize.iconMd),
            )
            Text("Thinking", style = type.sessionMeta, color = colors.textMuted)
        }
        if (expanded) {
            Text(
                message.thinking,
                style = type.thinking,
                color = colors.textMuted,
                // The disclosure already marks this as thinking; the accent
                // bar and the double indent were decoration (addition F, S4).
                modifier = Modifier.padding(top = ForgeSpace.xs),
            )
        }
    }
}

@Composable
private fun ToolCluster(messageId: Long, tools: List<ToolCall>, streaming: Boolean) {
    val colors = Forge.colors
    val type = Forge.type
    val needsAttention = tools.any { it.status.needsAttention }
    var expanded by rememberSaveable(messageId) { mutableStateOf(needsAttention || tools.any { it.status == ToolStatus.Running }) }
    LaunchedEffect(needsAttention, streaming) {
        if (needsAttention) expanded = true else if (!streaming) expanded = false
    }
    // The CLUSTER paints nothing. It used to draw the card, which is what a
    // golden scan measured as a 48 dp painted row: the container was the paint
    // and the row inside it was only layout (verify-11 O4). Each row paints its
    // own 36 dp body now, so one call is one band and two calls are two.
    Column(
        modifier = Modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        // One call is one row. A "1 tool call / 1 RUNNING" header above a
        // single row said the same thing twice and made the count a headline
        // (addition F, S3). The count header earns its place from two.
        if (tools.size > 1) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .clickable { expanded = !expanded }
                    .minimumInteractiveComponentSize()
                    .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(
                    if (expanded) Icons.Rounded.ExpandMore else Icons.Rounded.KeyboardArrowRight,
                    contentDescription = if (expanded) "Collapse tool calls" else "Expand tool calls",
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
                Spacer(Modifier.width(ForgeSpace.xs))
                Text(
                    "${tools.size} tool calls",
                    style = type.sessionMeta,
                    color = colors.textSoft,
                )
            }
        }
        if (expanded || tools.size == 1) {
            tools.forEachIndexed { index, tool ->
                if (index > 0) {
                    Box(
                        Modifier
                            .fillMaxWidth()
                            .background(colors.border)
                            .size(width = ForgeSize.hairline, height = ForgeSize.hairline),
                    )
                }
                ToolRow(tool)
            }
        }
    }
}

/** The 36 dp painted body inside a tool row's 48 dp target. */
const val TOOL_ROW_INK_TAG = "tool_row_ink"

@Composable
private fun ToolRow(tool: ToolCall) {
    val colors = Forge.colors
    val type = Forge.type
    var detailOpen by rememberSaveable(tool.callId) { mutableStateOf(tool.status.needsAttention) }
    // Two nodes, like the drawer row: the wrapper takes the 48 dp target and
    // the body paints 36. A `heightIn` minimum inside one node cannot shrink
    // the node it is inside (verify-10 O4).
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clickable(enabled = !tool.result.isNullOrBlank()) { detailOpen = !detailOpen },
        verticalArrangement = Arrangement.Center,
    ) {
        // The paint lives on THIS node and the 48 dp parent stays transparent.
        // Round 12 split the layout but left the container drawing its own
        // background and border, so a golden scan still measured 96 px of
        // painted row at 2x (verify-11 O4).
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.toolRowHeight)
                .testTag(TOOL_ROW_INK_TAG)
                .clip(ForgeShapes.cardTight)
                .background(colors.surface)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.cardTight)
                .padding(horizontal = ForgeSpace.lg),
            verticalArrangement = Arrangement.Center,
        ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            // mono: tool output, from the glyph to the result.
            Text(toolGlyph(tool.name), style = type.toolRow, color = colors.textMuted)
            Spacer(Modifier.width(ForgeSpace.md))
            // mono: the tool's own name.
            Text(tool.name, style = type.toolStrong, color = colors.textSoft, maxLines = 1)
            if (tool.summary.isNotBlank()) {
                Spacer(Modifier.width(ForgeSpace.md))
                Text(
                    tool.summary,
                    // mono: the command that was run.
                    style = type.toolRow,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
            } else {
                Spacer(Modifier.weight(1f))
            }
            // A finished call says how long it took, in the row itself, and
            // only when the daemon supplied the number (S3, verify-6 O8).
            tool.durationMs?.takeIf { tool.status != ToolStatus.Running }?.let {
                Spacer(Modifier.width(ForgeSpace.md))
                Text(
                    RelativeTime.elapsed(it),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    maxLines = 1,
                )
            }
            Spacer(Modifier.width(ForgeSpace.md))
            // A dot and a lowercase word, not a bordered pill (addition F, G3).
            Box(Modifier.size(ForgeSize.stateDot).clip(CircleShape).background(toolStatusColor(tool.status)))
            Spacer(Modifier.width(ForgeSpace.xs))
            Text(
                tool.status.label.lowercase(),
                style = type.sessionMeta,
                color = toolStatusColor(tool.status),
            )
        }
        }
        if (detailOpen && !tool.result.isNullOrBlank()) {
            val result = remember(tool.result) { prettyToolResult(tool.result) }
            Text(
                result,
                // mono: the tool's raw result.
                style = type.toolRow,
                color = colors.chatText,
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(max = ForgeSize.toolResultMax)
                    .verticalScroll(rememberScrollState())
                    .padding(top = ForgeSpace.md)
                    .clip(ForgeShapes.quote)
                    .background(colors.bgDeep)
                    .border(ForgeSize.hairline, colors.border, ForgeShapes.quote)
                    .padding(ForgeSpace.md),
            )
        }
    }
}


@Composable
private fun ErrorCard(message: String, retryable: Boolean, onRetry: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.cardTight)
            .background(colors.red.copy(alpha = 0.07f))
            .border(ForgeSize.hairline, colors.red.copy(alpha = 0.4f), ForgeShapes.cardTight)
            .drawBehind {
                val stroke = ForgeSize.rail.toPx()
                drawLine(
                    color = colors.red,
                    start = Offset(stroke / 2f, 0f),
                    end = Offset(stroke / 2f, size.height),
                    strokeWidth = stroke,
                )
            }
            .padding(start = ForgeSpace.lg, end = ForgeSpace.lg, top = ForgeSpace.md, bottom = ForgeSpace.md),
    ) {
        // State is a word, not a badge shout (addition F, G2/G3).
        Text("Run failed", style = type.sessionTitle, color = colors.red)
        Spacer(Modifier.size(ForgeSpace.xxs))
        Text(message, style = type.userBody, color = colors.chatText)
        // ChatReply.Error.retryable was parsed and thrown away in 970.
        if (retryable) {
            Spacer(Modifier.size(ForgeSpace.md))
            ForgeButton(
                text = stringResource(R.string.action_retry),
                onClick = onRetry,
                kind = ForgeButtonKind.Ghost,
            )
        }
    }
}

@Composable
private fun toolStatusColor(status: ToolStatus): Color {
    val colors = Forge.colors
    return when (status) {
        ToolStatus.Running -> colors.amber
        ToolStatus.Completed -> colors.green
        ToolStatus.Failed, ToolStatus.Rejected -> colors.red
        ToolStatus.Conflict -> colors.amber
        ToolStatus.Cancelled, ToolStatus.Unknown -> colors.textMuted
    }
}

private fun prettyToolResult(result: String): String = try {
    when {
        result.trimStart().startsWith("{") -> JSONObject(result).toString(2)
        result.trimStart().startsWith("[") -> JSONArray(result).toString(2)
        else -> result
    }
} catch (_: Exception) {
    result
}

private fun toolGlyph(name: String): String {
    val value = name.lowercase()
    return when {
        "web" in value || "http" in value -> "◎"
        "file" in value || "read" in value || "write" in value -> "▤"
        "shell" in value || "exec" in value || "command" in value -> ">_"
        "sms" in value || "message" in value -> "✉"
        "screen" in value || "a11y" in value || "tap" in value -> "◇"
        else -> "⚙"
    }
}

