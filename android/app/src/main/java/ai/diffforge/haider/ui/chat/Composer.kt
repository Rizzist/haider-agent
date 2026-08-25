package ai.diffforge.haider.ui.chat

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowUpward
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes

@Composable
fun Composer(
    onSend: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    var text by remember { mutableStateOf("") }
    val canSend = text.isNotBlank()

    fun fire() {
        if (text.isNotBlank()) {
            onSend(text)
            text = ""
        }
    }

    Row(
        modifier = modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .weight(1f)
                .clip(ForgeShapes.composer)
                .background(colors.surfaceControl)
                .border(1.dp, colors.border, ForgeShapes.composer)
                .padding(horizontal = 16.dp, vertical = 12.dp),
        ) {
            BasicTextField(
                value = text,
                onValueChange = { text = it },
                textStyle = type.chatBody.copy(color = colors.text),
                cursorBrush = SolidColor(colors.ember),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                keyboardActions = KeyboardActions(onSend = { fire() }),
                modifier = Modifier.fillMaxWidth(),
                decorationBox = { inner ->
                    if (text.isEmpty()) {
                        Text("Message Haider…", style = type.chatBody, color = colors.textDisabled)
                    }
                    inner()
                },
            )
        }
        Spacer(Modifier.width(10.dp))
        Box(
            modifier = Modifier
                .size(40.dp)
                .clip(CircleShape)
                .background(if (canSend) colors.accent else colors.surfaceControl)
                .border(1.dp, colors.border, CircleShape)
                .clickable(enabled = canSend) { fire() },
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                Icons.Rounded.ArrowUpward,
                contentDescription = "Send",
                tint = if (canSend) Color.White else colors.textDisabled,
                modifier = Modifier.size(20.dp),
            )
        }
    }
}
