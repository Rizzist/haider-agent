package ai.diffforge.haider.ui.onboarding

import android.content.Intent
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes

@Composable
fun OnboardingScreen(onDone: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    val context = LocalContext.current

    val smsLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { /* result reflected when the transport reports capabilities.changed */ }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.bg)
            .windowInsetsPadding(WindowInsets.safeDrawing)
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 16.dp),
    ) {
        Text("Set up Haider", style = type.h1, color = colors.text)
        Spacer(Modifier.height(6.dp))
        Text(
            "Grant the capabilities the agent drives on your phone. Everything runs on-device; " +
                "nothing leaves it.",
            style = type.chatBody,
            color = colors.textMuted,
        )
        Spacer(Modifier.height(20.dp))

        PermissionCard(
            title = "Accessibility",
            body = "Read the on-screen UI tree and inject taps/swipes. Enable “Haider” under " +
                "Settings → Accessibility.",
            action = "Open Accessibility settings",
            onClick = {
                context.startActivity(
                    Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                )
            },
        )
        Spacer(Modifier.height(12.dp))
        PermissionCard(
            title = "Screen capture",
            body = "Capture screenshots via MediaProjection. Android asks for consent each session " +
                "when the agent first needs a frame — no setup needed here.",
            action = null,
            onClick = {},
        )
        Spacer(Modifier.height(12.dp))
        PermissionCard(
            title = "SMS",
            body = "Read your inbox and receive incoming messages as they arrive.",
            action = "Grant SMS access",
            onClick = {
                smsLauncher.launch(
                    arrayOf(
                        android.Manifest.permission.READ_SMS,
                        android.Manifest.permission.RECEIVE_SMS,
                    ),
                )
            },
        )

        Spacer(Modifier.height(28.dp))
        PrimaryButton(text = "Continue to chat", onClick = onDone)
        Spacer(Modifier.height(12.dp))
    }
}

@Composable
private fun PermissionCard(
    title: String,
    body: String,
    action: String?,
    onClick: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.card)
            .background(colors.surface)
            .border(1.dp, colors.border, ForgeShapes.card)
            .padding(16.dp),
    ) {
        Text(title, style = type.h4, color = colors.text)
        Spacer(Modifier.height(6.dp))
        Text(body, style = type.chatBody, color = colors.textMuted)
        if (action != null) {
            Spacer(Modifier.height(12.dp))
            Row(
                modifier = Modifier
                    .clip(ForgeShapes.pill)
                    .background(colors.accent.copy(alpha = 0.16f))
                    .border(1.dp, colors.accentSoft.copy(alpha = 0.35f), ForgeShapes.pill)
                    .clickable { onClick() }
                    .padding(horizontal = 14.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(action, style = type.chip, color = colors.accentSoft)
            }
        }
    }
}

@Composable
private fun PrimaryButton(text: String, onClick: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.pill)
            .background(colors.accent)
            .clickable { onClick() }
            .padding(vertical = 14.dp),
        horizontalArrangement = Arrangement.Center,
    ) {
        Text(text, style = type.h4, color = androidx.compose.ui.graphics.Color.White)
    }
}
