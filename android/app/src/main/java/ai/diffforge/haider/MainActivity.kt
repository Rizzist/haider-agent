package ai.diffforge.haider

import ai.diffforge.haider.transport.DaemonConnection
import ai.diffforge.haider.ui.chat.BrandMark
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.chat.Composer
import ai.diffforge.haider.ui.chat.ConnectionState
import ai.diffforge.haider.ui.chat.ModelPicker
import ai.diffforge.haider.ui.chat.Transcript
import ai.diffforge.haider.ui.chat.UpdateBanner
import ai.diffforge.haider.ui.onboarding.OnboardingScreen
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeTheme
import ai.diffforge.haider.update.ApkUpdateCoordinator
import ai.diffforge.haider.update.UpdateUiState
import android.os.Bundle
import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
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
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.DarkMode
import androidx.compose.material.icons.rounded.LightMode
import androidx.compose.material.icons.rounded.Tune
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.core.view.WindowCompat

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        DaemonConnection.start(applicationContext)
        ApkUpdateCoordinator.start(applicationContext)
        setContent { AppRoot() }
    }

    override fun onResume() {
        super.onResume()
        ApkUpdateCoordinator.onActivityResumed(this)
    }

    override fun onPause() {
        ApkUpdateCoordinator.onActivityPaused(this)
        super.onPause()
    }
}

@Composable
private fun AppRoot() {
    val context = LocalContext.current
    val preferences = remember { context.getSharedPreferences(THEME_PREFERENCES, 0) }
    var dark by rememberSaveable { mutableStateOf(preferences.getBoolean(DARK_THEME_KEY, true)) }
    var showOnboarding by rememberSaveable { mutableStateOf(false) }
    val vm: ChatViewModel = viewModel()
    val updateState by ApkUpdateCoordinator.state.collectAsState()
    val view = LocalView.current

    if (!view.isInEditMode) {
        SideEffect {
            val window = context.findActivity()?.window ?: return@SideEffect
            WindowCompat.getInsetsController(window, view).apply {
                isAppearanceLightStatusBars = !dark
                isAppearanceLightNavigationBars = !dark
            }
        }
    }

    ForgeTheme(dark = dark) {
        if (showOnboarding) {
            OnboardingScreen(onDone = { showOnboarding = false })
        } else {
            SessionDeckScreen(
                vm = vm,
                updateState = updateState,
                onUpdate = { ApkUpdateCoordinator.onAffordanceTapped(context) },
                dark = dark,
                onToggleTheme = {
                    dark = !dark
                    preferences.edit().putBoolean(DARK_THEME_KEY, dark).apply()
                },
                onOpenSetup = { showOnboarding = true },
            )
        }
    }
}

@Composable
private fun SessionDeckScreen(
    vm: ChatViewModel,
    updateState: UpdateUiState,
    onUpdate: () -> Unit,
    dark: Boolean,
    onToggleTheme: () -> Unit,
    onOpenSetup: () -> Unit,
) {
    val colors = Forge.colors
    var showModelPicker by rememberSaveable { mutableStateOf(false) }
    val selection = vm.sessionConfig?.current
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.bg)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        HeaderRow(
            connection = vm.connection,
            provider = selection?.provider,
            model = selection?.model,
            dark = dark,
            onOpenModel = { showModelPicker = true },
            onToggleTheme = onToggleTheme,
            onOpenSetup = onOpenSetup,
        )
        UpdateBanner(state = updateState, onClick = onUpdate)
        Box(modifier = Modifier.fillMaxWidth().weight(1f)) {
            if (vm.messages.isEmpty()) {
                Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    HomeState(vm.connection, onOpenSetup)
                }
            } else {
                Transcript(messages = vm.messages, modifier = Modifier.fillMaxSize())
            }
        }
        Row(
            modifier = Modifier.fillMaxWidth().imePadding(),
            horizontalArrangement = androidx.compose.foundation.layout.Arrangement.Center,
        ) {
            Composer(
                modelLabel = selection?.let { "${it.provider} / ${it.model}" },
                effortLabel = selection?.effort ?: vm.sessionConfig?.let { "default" },
                selectionBusy = vm.selectionBusy,
                onOpenModel = { showModelPicker = true },
                onSend = vm::send,
                modifier = Modifier.widthIn(max = 776.dp).fillMaxWidth(),
            )
        }
    }
    if (showModelPicker) {
        ModelPicker(
            config = vm.sessionConfig,
            error = vm.settingsError,
            busy = vm.selectionBusy,
            onSelectModel = { provider, model -> vm.selectModel(provider, model) },
            onSelectEffort = vm::selectEffort,
            onRefresh = vm::refreshSessionConfig,
            onDismiss = { showModelPicker = false },
        )
    }
}

@Composable
private fun HeaderRow(
    connection: ConnectionState,
    provider: String?,
    model: String?,
    dark: Boolean,
    onOpenModel: () -> Unit,
    onToggleTheme: () -> Unit,
    onOpenSetup: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column {
        Row(
            modifier = Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 9.dp, bottom = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            BrandMark(provider)
            Spacer(Modifier.width(8.dp))
            Column(
                modifier = Modifier
                    .weight(1f)
                    .heightIn(min = 48.dp)
                    .clickable(onClick = onOpenModel),
                verticalArrangement = androidx.compose.foundation.layout.Arrangement.Center,
            ) {
                Text("Haider", style = type.h4, color = colors.text)
                Text(
                    model ?: "Diff Forge AI",
                    style = type.toolRow,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            StatusPill(connection)
            HeaderButton(
                onClick = onToggleTheme,
                contentDescription = if (dark) "Use light theme" else "Use dark theme",
            ) {
                Icon(
                    if (dark) Icons.Rounded.LightMode else Icons.Rounded.DarkMode,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(17.dp),
                )
            }
            HeaderButton(onClick = onOpenSetup, contentDescription = "Setup") {
                Icon(
                    Icons.Rounded.Tune,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(17.dp),
                )
            }
        }
        Box(Modifier.fillMaxWidth().height(1.dp).background(colors.border))
    }
}

@Composable
private fun HeaderButton(
    onClick: () -> Unit,
    contentDescription: String,
    icon: @Composable () -> Unit,
) {
    val colors = Forge.colors
    Box(
        modifier = Modifier
            .padding(start = 5.dp)
            .size(48.dp)
            .clip(CircleShape)
            .semantics { this.contentDescription = contentDescription }
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier = Modifier
                .size(32.dp)
                .clip(CircleShape)
                .background(colors.surfaceControl)
                .border(1.dp, colors.border, CircleShape),
            contentAlignment = Alignment.Center,
        ) { icon() }
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
            .padding(horizontal = 9.dp, vertical = 5.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(6.dp).clip(CircleShape).background(dot))
        Spacer(Modifier.width(6.dp))
        Text(connection.label, style = type.statusPill, color = colors.textSoft)
    }
}

@Composable
private fun HomeState(connection: ConnectionState, onOpenSetup: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        modifier = Modifier.widthIn(max = 480.dp).padding(horizontal = 24.dp),
    ) {
        Box(
            modifier = Modifier
                .size(46.dp)
                .clip(ForgeShapes.card)
                .background(colors.accent.copy(alpha = 0.14f))
                .border(1.dp, colors.accentSoft.copy(alpha = 0.35f), ForgeShapes.card),
            contentAlignment = Alignment.Center,
        ) {
            Text(">_", style = type.toolStrong, color = colors.ember)
        }
        Spacer(Modifier.height(13.dp))
        Text("Diff Forge AI", style = type.h1, color = colors.text)
        Spacer(Modifier.height(7.dp))
        Text(
            if (connection == ConnectionState.Connected) {
                "Ask Haider to read your latest SMS, inspect the screen, or help with anything on your phone."
            } else {
                "Connect your daemon, then let Haider work with your phone through the capabilities you grant."
            },
            style = type.chatBody,
            color = colors.textMuted,
            textAlign = TextAlign.Center,
        )
        if (connection != ConnectionState.Connected) {
            Spacer(Modifier.height(20.dp))
            Text(
                "Set up connection",
                style = type.chip,
                color = colors.accentSoft,
                modifier = Modifier
                    .clip(ForgeShapes.pill)
                    .background(colors.accent.copy(alpha = 0.16f))
                    .border(1.dp, colors.accentSoft.copy(alpha = 0.35f), ForgeShapes.pill)
                    .clickable(onClick = onOpenSetup)
                    .heightIn(min = 48.dp)
                    .padding(horizontal = 16.dp, vertical = 9.dp),
            )
        }
    }
}

private const val THEME_PREFERENCES = "forge_theme"
private const val DARK_THEME_KEY = "dark"

private tailrec fun Context.findActivity(): Activity? = when (this) {
    is Activity -> this
    is ContextWrapper -> baseContext.findActivity()
    else -> null
}
