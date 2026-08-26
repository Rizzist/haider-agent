package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import android.os.PowerManager
import android.os.Handler
import android.os.Looper
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.database.ContentObserver
import android.provider.Settings
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
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
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import kotlinx.coroutines.flow.distinctUntilChanged
import org.json.JSONArray
import org.json.JSONObject

@Composable
fun Transcript(messages: List<Message>, modifier: Modifier = Modifier) {
    val listState = rememberLazyListState()
    var stickToBottom by remember { mutableStateOf(true) }
    val nearBottomPx = with(LocalDensity.current) { 60.dp.roundToPx() }

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
        contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 18.dp),
        verticalArrangement = Arrangement.spacedBy(18.dp),
    ) {
        items(messages, key = { it.id }) { message ->
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.Center) {
                Box(modifier = Modifier.widthIn(max = 776.dp).fillMaxWidth()) {
                    when (message.role) {
                        Role.User -> UserBubble(message)
                        Role.Agent -> AgentTurn(message)
                    }
                }
            }
        }
    }
}

@Composable
private fun UserBubble(message: Message) {
    val colors = Forge.colors
    val type = Forge.type
    BoxWithConstraints(modifier = Modifier.fillMaxWidth()) {
        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
            Box(
                modifier = Modifier
                    .widthIn(max = maxWidth * 0.72f)
                    .clip(USER_SHAPE)
                    .background(colors.accent.copy(alpha = 0.12f))
                    .border(1.dp, colors.accentSoft.copy(alpha = 0.28f), USER_SHAPE)
                    .padding(horizontal = 13.dp, vertical = 9.dp),
            ) {
                Text(message.text, style = type.userBody, color = colors.chatText)
            }
        }
    }
}

@Composable
private fun AgentTurn(message: Message) {
    Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.Top) {
        BrandMark(message.provider, modifier = Modifier.padding(top = 2.dp))
        Spacer(Modifier.width(10.dp))
        Box(modifier = Modifier.weight(1f)) {
            Column(modifier = Modifier.widthIn(max = 600.dp), verticalArrangement = Arrangement.spacedBy(9.dp)) {
                if (message.thinking.isNotEmpty()) ThinkingFold(message)
                if (message.tools.isNotEmpty()) ToolCluster(message.id, message.tools, message.streaming)
                if (message.text.isNotEmpty() || message.streaming) AssistantProse(message)
                if (message.streaming && !message.status.isNullOrBlank()) LiveStatus(message.status)
                message.error?.let { ErrorCard(it) }
            }
        }
    }
}

@Composable
private fun AssistantProse(message: Message) {
    if (message.streaming && motionEnabled()) {
        val transition = rememberInfiniteTransition(label = "stream-caret")
        val caretAlpha by transition.animateFloat(
            initialValue = 1f,
            targetValue = 0.15f,
            animationSpec = infiniteRepeatable(tween(500), RepeatMode.Reverse),
            label = "caret-alpha",
        )
        MarkdownText(message.text, showCaret = true, caretAlpha = caretAlpha)
    } else {
        MarkdownText(message.text, showCaret = message.streaming)
    }
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
                .heightIn(min = 48.dp)
                .padding(end = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                if (expanded) Icons.Rounded.ExpandMore else Icons.Rounded.KeyboardArrowRight,
                contentDescription = if (expanded) "Collapse thinking" else "Expand thinking",
                tint = colors.ember,
                modifier = Modifier.size(20.dp),
            )
            Text("Thinking", style = type.toolRow, color = colors.textMuted)
        }
        if (expanded) {
            Text(
                message.thinking,
                style = type.thinking,
                color = colors.textMuted,
                modifier = Modifier
                    .padding(start = 9.dp, top = 5.dp)
                    .drawBehind {
                        val stroke = 2.dp.toPx()
                        drawLine(
                            color = colors.ember.copy(alpha = 0.45f),
                            start = Offset(stroke / 2f, 0f),
                            end = Offset(stroke / 2f, size.height),
                            strokeWidth = stroke,
                        )
                    }
                    .padding(start = 12.dp),
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
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.cardTight)
            .background(colors.surface)
            .border(1.dp, colors.border, ForgeShapes.cardTight),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .clickable { expanded = !expanded }
                .heightIn(min = 48.dp)
                .padding(horizontal = 10.dp, vertical = 9.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                if (expanded) Icons.Rounded.ExpandMore else Icons.Rounded.KeyboardArrowRight,
                contentDescription = if (expanded) "Collapse tool calls" else "Expand tool calls",
                tint = colors.textMuted,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.width(4.dp))
            Text(
                if (tools.size == 1) "1 tool call" else "${tools.size} tool calls",
                style = type.toolRow,
                color = colors.textSoft,
            )
            Spacer(Modifier.weight(1f))
            ToolSummaryTags(tools)
        }
        if (expanded) {
            tools.forEachIndexed { index, tool ->
                if (index > 0) Box(Modifier.fillMaxWidth().background(colors.border).size(width = 1.dp, height = 1.dp))
                ToolRow(tool)
            }
        }
    }
}

@Composable
private fun ToolRow(tool: ToolCall) {
    val colors = Forge.colors
    val type = Forge.type
    var detailOpen by rememberSaveable(tool.callId) { mutableStateOf(tool.status.needsAttention) }
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(enabled = !tool.result.isNullOrBlank()) { detailOpen = !detailOpen }
            .heightIn(min = 48.dp)
            .padding(horizontal = 11.dp, vertical = 9.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(toolGlyph(tool.name), style = type.toolRow, color = colors.textMuted)
            Spacer(Modifier.width(8.dp))
            Text(tool.name, style = type.toolStrong, color = colors.textSoft, maxLines = 1)
            if (tool.summary.isNotBlank()) {
                Spacer(Modifier.width(8.dp))
                Text(
                    tool.summary,
                    style = type.toolRow,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
            } else {
                Spacer(Modifier.weight(1f))
            }
            Spacer(Modifier.width(8.dp))
            Box(Modifier.size(6.dp).clip(CircleShape).background(toolStatusColor(tool.status)))
            Spacer(Modifier.width(5.dp))
            Text(tool.status.label.uppercase(), style = type.label, color = toolStatusColor(tool.status))
        }
        if (detailOpen && !tool.result.isNullOrBlank()) {
            val result = remember(tool.result) { prettyToolResult(tool.result) }
            Text(
                result,
                style = type.toolRow,
                color = colors.chatText,
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(max = 220.dp)
                    .verticalScroll(rememberScrollState())
                    .padding(top = 8.dp)
                    .clip(RoundedCornerShape(6.dp))
                    .background(colors.bgDeep)
                    .border(1.dp, colors.border, RoundedCornerShape(6.dp))
                    .padding(9.dp),
            )
        }
    }
}

@Composable
private fun LiveStatus(status: String) {
    val colors = Forge.colors
    val type = Forge.type
    val alpha = if (motionEnabled()) pulsingStatusAlpha() else 1f
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(7.dp).clip(CircleShape).background(colors.amber.copy(alpha = alpha)))
        Spacer(Modifier.width(8.dp))
        Text(status, style = type.toolRow, color = colors.textMuted)
    }
}

@Composable
private fun pulsingStatusAlpha(): Float {
    val transition = rememberInfiniteTransition(label = "tool-shimmer")
    return transition.animateFloat(
        initialValue = 0.35f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(tween(700), RepeatMode.Reverse),
        label = "tool-shimmer-alpha",
    ).value
}

@Composable
private fun motionEnabled(): Boolean {
    val context = LocalContext.current
    var enabled by remember(context) { mutableStateOf(readMotionEnabled(context)) }
    DisposableEffect(context) {
        val refresh = { enabled = readMotionEnabled(context) }
        val powerReceiver = object : BroadcastReceiver() {
            override fun onReceive(receiverContext: Context?, intent: Intent?) = refresh()
        }
        val animatorObserver = object : ContentObserver(Handler(Looper.getMainLooper())) {
            override fun onChange(selfChange: Boolean) = refresh()
        }
        ContextCompat.registerReceiver(
            context,
            powerReceiver,
            IntentFilter(PowerManager.ACTION_POWER_SAVE_MODE_CHANGED),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        context.contentResolver.registerContentObserver(
            Settings.Global.getUriFor(Settings.Global.ANIMATOR_DURATION_SCALE),
            false,
            animatorObserver,
        )
        onDispose {
            context.unregisterReceiver(powerReceiver)
            context.contentResolver.unregisterContentObserver(animatorObserver)
        }
    }
    return enabled
}

private fun readMotionEnabled(context: Context): Boolean {
    val animatorScale = runCatching {
        Settings.Global.getFloat(
            context.contentResolver,
            Settings.Global.ANIMATOR_DURATION_SCALE,
            1f,
        )
    }.getOrDefault(1f)
    val powerSaver = context.getSystemService(PowerManager::class.java)?.isPowerSaveMode == true
    return animatorScale > 0f && !powerSaver
}

@Composable
private fun ErrorCard(message: String) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.cardTight)
            .background(colors.red.copy(alpha = 0.07f))
            .border(1.dp, colors.red.copy(alpha = 0.4f), ForgeShapes.cardTight)
            .drawBehind {
                val stroke = 3.dp.toPx()
                drawLine(
                    color = colors.red,
                    start = Offset(stroke / 2f, 0f),
                    end = Offset(stroke / 2f, size.height),
                    strokeWidth = stroke,
                )
            }
            .padding(start = 12.dp, end = 11.dp, top = 9.dp, bottom = 10.dp),
    ) {
        Text("RUN FAILED", style = type.label, color = colors.red)
        Spacer(Modifier.size(3.dp))
        Text(message, style = type.userBody, color = colors.chatText)
    }
}

@Composable
fun BrandMark(provider: String?, modifier: Modifier = Modifier) {
    val colors = Forge.colors
    val normalized = provider.orEmpty().lowercase()
    val (mark, color) = when {
        "anthropic" in normalized || "claude" in normalized -> "✳" to Color(0xFFD97757)
        "gemini" in normalized || "google" in normalized -> "✦" to Color(0xFF4E86F5)
        "deepseek" in normalized -> "D" to Color(0xFF4D6BFE)
        "qwen" in normalized -> "Q" to Color(0xFF615CED)
        "kimi" in normalized || "moonshot" in normalized -> "K" to Color(0xFF16A8F0)
        "mistral" in normalized -> "M" to Color(0xFFFF7000)
        "llama" in normalized || "meta" in normalized -> "L" to Color(0xFF0668E1)
        "glm" in normalized -> "G" to Color(0xFF3859FF)
        "openai" in normalized || "codex" in normalized -> "AI" to colors.text
        else -> "H" to colors.ember
    }
    Box(
        modifier = modifier
            .size(22.dp)
            .clip(CircleShape)
            .background(color.copy(alpha = 0.12f))
            .border(1.dp, color.copy(alpha = 0.45f), CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        Text(mark, style = Forge.type.label, color = color)
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

@Composable
private fun ToolSummaryTags(tools: List<ToolCall>) {
    val colors = Forge.colors
    val type = Forge.type
    val tags = listOf(
        Triple(tools.count { it.status == ToolStatus.Completed }, "OK", colors.green),
        Triple(
            tools.count { it.status in setOf(ToolStatus.Failed, ToolStatus.Rejected) },
            "FAILED",
            colors.red,
        ),
        Triple(tools.count { it.status == ToolStatus.Running }, "RUNNING", colors.amber),
        Triple(tools.count { it.status == ToolStatus.Conflict }, "CONFLICT", colors.amber),
        Triple(tools.count { it.status == ToolStatus.Cancelled }, "CANCELLED", colors.textMuted),
        Triple(tools.count { it.status == ToolStatus.Unknown }, "UNKNOWN", colors.textMuted),
    ).filter { it.first > 0 }
    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        tags.forEach { (count, label, color) ->
            Text("$count $label", style = type.label, color = color, maxLines = 1)
        }
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

private val USER_SHAPE = RoundedCornerShape(
    topStart = 14.dp,
    topEnd = 14.dp,
    bottomStart = 14.dp,
    bottomEnd = 5.dp,
)
