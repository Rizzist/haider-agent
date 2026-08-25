package ai.diffforge.haider

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowUpward
import androidx.compose.material.icons.rounded.MoreHoriz
import androidx.compose.material3.Icon
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.material3.Text
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            ForgeTheme {
                SessionDeckScreen()
            }
        }
    }
}

private enum class DeckView(val label: String) {
    Chat("Chat"), Shell("Shell"), Traj("Traj")
}

@Composable
private fun SessionDeckScreen() {
    val colors = Forge.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.bg)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        HeaderRow()
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .weight(1f),
            contentAlignment = Alignment.Center,
        ) {
            HomeState()
        }
        Composer()
    }
}

@Composable
private fun HeaderRow() {
    val colors = Forge.colors
    val type = Forge.type
    var view by remember { mutableStateOf(DeckView.Chat) }
    Column {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(start = 16.dp, end = 16.dp, top = 10.dp, bottom = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Haider", style = type.h4, color = colors.text)
            Spacer(Modifier.width(6.dp))
            Icon(
                Icons.Rounded.MoreHoriz,
                contentDescription = "Session menu",
                tint = colors.textMuted,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.weight(1f))
            SegmentedToggle(view) { view = it }
            Spacer(Modifier.width(12.dp))
            StatusPill(dot = colors.textMuted, label = "idle")
        }
        Box(
            Modifier
                .fillMaxWidth()
                .height(1.dp)
                .background(colors.border),
        )
    }
}

@Composable
private fun SegmentedToggle(selected: DeckView, onSelect: (DeckView) -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .clip(ForgeShapes.pill)
            .background(colors.surfaceControl)
            .border(1.dp, colors.border, ForgeShapes.pill)
            .padding(2.dp),
    ) {
        DeckView.entries.forEach { v ->
            val active = v == selected
            Box(
                modifier = Modifier
                    .clip(ForgeShapes.pill)
                    .background(if (active) colors.accent.copy(alpha = 0.22f) else Color.Transparent)
                    .then(
                        if (active) Modifier.border(1.dp, colors.accentSoft.copy(alpha = 0.35f), ForgeShapes.pill)
                        else Modifier,
                    )
                    .padding(horizontal = 12.dp, vertical = 5.dp),
            ) {
                Text(
                    v.label,
                    style = type.chip,
                    color = if (active) colors.text else colors.textMuted,
                )
            }
        }
    }
}

@Composable
private fun StatusPill(dot: Color, label: String) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .clip(ForgeShapes.pill)
            .background(colors.surfaceControl)
            .border(1.dp, colors.border, ForgeShapes.pill)
            .padding(horizontal = 10.dp, vertical = 5.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier
                .size(6.dp)
                .clip(CircleShape)
                .background(dot),
        )
        Spacer(Modifier.width(6.dp))
        Text(label, style = type.statusPill, color = colors.textSoft)
    }
}

@Composable
private fun HomeState() {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        modifier = Modifier
            .widthIn(max = 480.dp)
            .padding(horizontal = 24.dp),
    ) {
        Text("Haider", style = type.h1, color = colors.text)
        Spacer(Modifier.height(8.dp))
        Text(
            "Your phone, driven by an agent. Grant Accessibility, screen capture, and SMS to start.",
            style = type.chatBody,
            color = colors.textMuted,
            textAlign = TextAlign.Center,
        )
    }
}

@Composable
private fun Composer() {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(
            modifier = Modifier
                .weight(1f)
                .clip(ForgeShapes.composer)
                .background(colors.surfaceControl)
                .border(1.dp, colors.border, ForgeShapes.composer)
                .padding(horizontal = 16.dp, vertical = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                "Message Haider…",
                style = type.chatBody,
                color = colors.textDisabled,
                modifier = Modifier.weight(1f),
            )
        }
        Spacer(Modifier.width(10.dp))
        Box(
            modifier = Modifier
                .size(40.dp)
                .clip(CircleShape)
                .background(colors.accent),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                Icons.Rounded.ArrowUpward,
                contentDescription = "Send",
                tint = Color.White,
                modifier = Modifier.size(20.dp),
            )
        }
    }
}
