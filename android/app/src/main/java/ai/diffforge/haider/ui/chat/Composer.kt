package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowUpward
import androidx.compose.material.icons.rounded.ExpandMore
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.unit.dp

@Composable
fun Composer(
    modelLabel: String?,
    effortLabel: String?,
    selectionBusy: Boolean,
    onOpenModel: () -> Unit,
    onSend: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    var text by remember { mutableStateOf("") }
    var focused by remember { mutableStateOf(false) }
    val canSend = text.isNotBlank() && !selectionBusy

    fun fire() {
        if (canSend) {
            onSend(text)
            text = ""
        }
    }

    Column(modifier = modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp)) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .horizontalScroll(rememberScrollState())
                .padding(start = 4.dp, bottom = 8.dp),
        ) {
            ComposerChip(
                label = "MODEL",
                value = when {
                    selectionBusy -> "changing…"
                    modelLabel != null -> modelLabel
                    else -> "loading…"
                },
                onClick = onOpenModel,
            )
            if (effortLabel != null) {
                Spacer(Modifier.width(7.dp))
                ComposerChip(label = "EFFORT", value = effortLabel, onClick = onOpenModel)
            }
        }
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 50.dp, max = 160.dp)
                .clip(ForgeShapes.composer)
                .background(colors.surfaceRaised)
                .border(
                    1.dp,
                    if (focused) colors.accentSoft.copy(alpha = 0.5f) else colors.borderStrong,
                    ForgeShapes.composer,
                )
                .padding(start = 17.dp, end = 6.dp, top = 6.dp, bottom = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            BasicTextField(
                value = text,
                onValueChange = { text = it },
                textStyle = type.chatBody.copy(color = colors.text),
                cursorBrush = SolidColor(colors.ember),
                maxLines = 6,
                modifier = Modifier
                    .weight(1f)
                    .onFocusChanged { focused = it.isFocused }
                    .padding(vertical = 6.dp),
                decorationBox = { inner ->
                    Box {
                        if (text.isEmpty()) {
                            Text("Message Haider…", style = type.chatBody, color = colors.textDisabled)
                        }
                        inner()
                    }
                },
            )
            Spacer(Modifier.width(8.dp))
            Box(
                modifier = Modifier
                    .size(48.dp)
                    .clip(CircleShape)
                    .background(if (canSend) colors.accent else colors.surfaceControl)
                    .border(
                        1.dp,
                        if (canSend) colors.accentSoft.copy(alpha = 0.45f) else colors.border,
                        CircleShape,
                    )
                    .clickable(enabled = canSend) { fire() },
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    Icons.Rounded.ArrowUpward,
                    contentDescription = "Send message",
                    tint = if (canSend) Color.White else colors.textDisabled,
                    modifier = Modifier.size(20.dp),
                )
            }
        }
    }
}

@Composable
private fun ComposerChip(label: String, value: String, onClick: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .clip(ForgeShapes.pill)
            .background(colors.surfaceControl)
            .border(1.dp, colors.border, ForgeShapes.pill)
            .clickable(onClick = onClick)
            .heightIn(min = 48.dp)
            .padding(start = 10.dp, end = 5.dp, top = 6.dp, bottom = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, style = type.label, color = colors.textMuted)
        Spacer(Modifier.width(5.dp))
        Text(value, style = type.chip, color = colors.textSoft, maxLines = 1)
        Icon(
            Icons.Rounded.ExpandMore,
            contentDescription = null,
            tint = colors.textMuted,
            modifier = Modifier.size(17.dp),
        )
    }
}
