package ai.diffforge.haider.ui.chat

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes

@Composable
fun Transcript(
    messages: List<Message>,
    modifier: Modifier = Modifier,
) {
    val listState = rememberLazyListState()
    LaunchedEffect(messages.size) {
        if (messages.isNotEmpty()) listState.animateScrollToItem(messages.size - 1)
    }
    LazyColumn(
        state = listState,
        modifier = modifier.fillMaxWidth(),
        contentPadding = androidx.compose.foundation.layout.PaddingValues(
            start = 20.dp, end = 20.dp, top = 12.dp, bottom = 12.dp,
        ),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        items(messages, key = { it.id }) { msg ->
            when (msg.role) {
                Role.User -> UserBubble(msg)
                Role.Agent -> AgentTurn(msg)
            }
        }
    }
}

@Composable
private fun UserBubble(msg: Message) {
    val colors = Forge.colors
    val type = Forge.type
    Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
        Box(
            modifier = Modifier
                .widthIn(max = 320.dp)
                .clip(RoundedCornerShape(14.dp))
                .background(colors.surfaceControl)
                .border(1.dp, colors.border, RoundedCornerShape(14.dp))
                .padding(horizontal = 14.dp, vertical = 10.dp),
        ) {
            Text(msg.text, style = type.userBody, color = colors.text)
        }
    }
}

@Composable
private fun AgentTurn(msg: Message) {
    val colors = Forge.colors
    val type = Forge.type
    Column(modifier = Modifier.fillMaxWidth()) {
        if (msg.tools.isNotEmpty()) {
            Column(
                modifier = Modifier.padding(bottom = 8.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                msg.tools.forEach { ToolRow(it) }
            }
        }
        if (msg.text.isNotEmpty()) {
            Text(msg.text, style = type.chatBody, color = colors.chatText)
        }
    }
}

@Composable
private fun ToolRow(tool: ToolCall) {
    val colors = Forge.colors
    val type = Forge.type
    val dot = when (tool.status) {
        ToolStatus.Running -> colors.amber
        ToolStatus.Ok -> colors.green
        ToolStatus.Error -> colors.red
        ToolStatus.Unknown -> colors.textMuted // never green until terminal
    }
    Row(
        modifier = Modifier
            .clip(ForgeShapes.cardTight)
            .background(colors.surface)
            .border(1.dp, colors.border, ForgeShapes.cardTight)
            .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(6.dp).clip(CircleShape).background(dot))
        Spacer(Modifier.width(8.dp))
        Text(tool.name, style = type.toolRow, color = colors.textSoft)
        Spacer(Modifier.width(8.dp))
        Text(tool.summary, style = type.toolRow, color = colors.textMuted)
    }
}
