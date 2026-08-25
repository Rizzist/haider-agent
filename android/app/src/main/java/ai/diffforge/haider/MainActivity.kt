package ai.diffforge.haider

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Tune
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
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import ai.diffforge.haider.transport.DaemonConnection
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.chat.Composer
import ai.diffforge.haider.ui.chat.ConnectionState
import ai.diffforge.haider.ui.chat.Transcript
import ai.diffforge.haider.ui.onboarding.OnboardingScreen
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        // Connects if a daemon endpoint is already configured; a no-op otherwise.
        DaemonConnection.start(applicationContext)
        setContent {
            ForgeTheme {
                AppRoot()
            }
        }
    }
}

@Composable
private fun AppRoot() {
    var showOnboarding by remember { mutableStateOf(false) }
    val vm: ChatViewModel = viewModel()
    if (showOnboarding) {
        OnboardingScreen(onDone = { showOnboarding = false })
    } else {
        SessionDeckScreen(vm = vm, onOpenSetup = { showOnboarding = true })
    }
}

@Composable
private fun SessionDeckScreen(vm: ChatViewModel, onOpenSetup: () -> Unit) {
    val colors = Forge.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.bg)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        HeaderRow(connection = vm.connection, onOpenSetup = onOpenSetup)
        Box(modifier = Modifier.fillMaxWidth().weight(1f)) {
            if (vm.messages.isEmpty()) {
                Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    HomeState(onOpenSetup = onOpenSetup)
                }
            } else {
                Transcript(messages = vm.messages, modifier = Modifier.fillMaxSize())
            }
        }
        Composer(onSend = vm::send)
    }
}

@Composable
private fun HeaderRow(connection: ConnectionState, onOpenSetup: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Column {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(start = 16.dp, end = 16.dp, top = 10.dp, bottom = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Haider", style = type.h4, color = colors.text)
            Spacer(Modifier.weight(1f))
            StatusPill(connection)
            Spacer(Modifier.width(10.dp))
            Box(
                modifier = Modifier
                    .size(32.dp)
                    .clip(CircleShape)
                    .background(colors.surfaceControl)
                    .border(1.dp, colors.border, CircleShape)
                    .clickable { onOpenSetup() },
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    Icons.Rounded.Tune,
                    contentDescription = "Setup",
                    tint = colors.textSoft,
                    modifier = Modifier.size(17.dp),
                )
            }
        }
        Box(Modifier.fillMaxWidth().height(1.dp).background(colors.border))
    }
}

@Composable
private fun StatusPill(connection: ConnectionState) {
    val colors = Forge.colors
    val type = Forge.type
    val dot = when (connection) {
        ConnectionState.Idle -> colors.textMuted
        ConnectionState.Connecting -> colors.amber
        ConnectionState.Connected -> colors.green
        ConnectionState.Error -> colors.red
    }
    Row(
        modifier = Modifier
            .clip(ForgeShapes.pill)
            .background(colors.surfaceControl)
            .border(1.dp, colors.border, ForgeShapes.pill)
            .padding(horizontal = 10.dp, vertical = 5.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(6.dp).clip(CircleShape).background(dot))
        Spacer(Modifier.width(6.dp))
        Text(connection.label, style = type.statusPill, color = colors.textSoft)
    }
}

@Composable
private fun HomeState(onOpenSetup: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        modifier = Modifier.widthIn(max = 480.dp).padding(horizontal = 24.dp),
    ) {
        Text("Haider", style = type.h1, color = colors.text)
        Spacer(Modifier.height(8.dp))
        Text(
            "Your phone, driven by an agent. Grant Accessibility, screen capture, and SMS to start.",
            style = type.chatBody,
            color = colors.textMuted,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(20.dp))
        Row(
            modifier = Modifier
                .clip(ForgeShapes.pill)
                .background(colors.accent.copy(alpha = 0.16f))
                .border(1.dp, colors.accentSoft.copy(alpha = 0.35f), ForgeShapes.pill)
                .clickable { onOpenSetup() }
                .padding(horizontal = 16.dp, vertical = 9.dp),
        ) {
            Text("Set up permissions", style = type.chip, color = colors.accentSoft)
        }
    }
}
