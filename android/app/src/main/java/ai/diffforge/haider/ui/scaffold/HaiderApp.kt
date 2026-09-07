package ai.diffforge.haider.ui.scaffold

import ai.diffforge.haider.ui.accounts.AccountsRepository
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.chat.Composer
import ai.diffforge.haider.ui.chat.InputRequiredCard
import ai.diffforge.haider.ui.chat.ModelPicker
import ai.diffforge.haider.ui.chat.StickyStopChip
import ai.diffforge.haider.ui.chat.Transcript
import ai.diffforge.haider.ui.drawer.RenameSheet
import ai.diffforge.haider.ui.drawer.SessionActionsSheet
import ai.diffforge.haider.ui.drawer.SessionDrawer
import ai.diffforge.haider.ui.drawer.SessionRowAction
import ai.diffforge.haider.ui.settings.AccountsScreen
import ai.diffforge.haider.ui.settings.SettingsScreen
import ai.diffforge.haider.ui.start.StartSurface
import ai.diffforge.haider.ui.state.BannerAction
import ai.diffforge.haider.ui.state.BannerInputs
import ai.diffforge.haider.ui.state.BannerResolver
import ai.diffforge.haider.ui.state.ModelChipStateMachine
import ai.diffforge.haider.ui.state.NeedsInputElsewhere
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.RelativeTime
import ai.diffforge.haider.ui.state.SendButtonMatrix
import ai.diffforge.haider.ui.state.SetupStepId
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import ai.diffforge.haider.ui.theme.ForgeTheme
import ai.diffforge.haider.ui.theme.ThemeMode
import ai.diffforge.haider.update.UpdateUiState
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.material3.DrawerValue
import androidx.compose.material3.ModalDrawerSheet
import androidx.compose.material3.ModalNavigationDrawer
import androidx.compose.material3.Text
import androidx.compose.material3.rememberDrawerState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * The scaffold: a modal drawer over a Scaffold whose top bar is
 * [HaiderTopBar], whose body is the banner plus one surface, and whose bottom
 * bar is the composer (UI-SPEC 3.0).
 *
 * 970 switched screens with one boolean and had no back handling at all (D6).
 * Here back closes the drawer, then an overlay, then falls through.
 */
@Composable
fun HaiderApp(
    viewModel: ChatViewModel,
    accounts: AccountsRepository,
    appVersion: String,
    themeMode: ThemeMode,
    onThemeMode: (ThemeMode) -> Unit,
    updateState: UpdateUiState = UpdateUiState.Hidden,
    onUpdateAction: () -> Unit = {},
    onOpenUrl: (String) -> Unit = {},
    onSystemAction: (SystemAction) -> Unit = {},
    dismissals: ai.diffforge.haider.ui.state.BannerDismissals = InMemoryBannerDismissals(),
    nowMsProvider: () -> Long = System::currentTimeMillis,
    darkOverride: Boolean? = null,
) {
    val state by viewModel.state.collectAsState()
    val dark = darkOverride ?: when (themeMode) {
        ThemeMode.Dark -> true
        ThemeMode.Light -> false
        ThemeMode.System -> androidx.compose.foundation.isSystemInDarkTheme()
    }

    ForgeTheme(dark = dark) {
        val colors = Forge.colors
        val drawerState = rememberDrawerState(DrawerValue.Closed)
        val scope = rememberCoroutineScope()
        var nowMs by remember { mutableLongStateOf(nowMsProvider()) }
        LaunchedEffect(Unit) {
            while (true) {
                delay(1_000)
                nowMs = nowMsProvider()
            }
        }

        val screenWidth = LocalConfiguration.current.screenWidthDp.dp
        val drawerWidth = minOf(screenWidth - ForgeSize.drawerInset, ForgeSize.drawerWidth)

        val elsewhere = state.needsInputElsewhereRow
        val resolution = BannerResolver.resolve(
            inputs = BannerInputs(
                daemon = state.daemon,
                needsInputElsewhere = elsewhere?.needsInput?.let { asking ->
                    NeedsInputElsewhere(
                        sessionId = elsewhere.id,
                        sessionTitle = elsewhere.title ?: elsewhere.id.take(6),
                        prompt = asking.displayTitle,
                        waiting = RelativeTime.waiting(asking.sinceMs, nowMs),
                    )
                },
                notificationsGranted = state.environment.notificationsGranted,
                batteryRestricted = state.environment.batteryRestricted,
                network = state.environment.network,
                update = updateState,
                firstRun = !state.setup.complete && state.sessions.isEmpty(),
            ),
            dismissals = dismissals.snapshot(),
            nowMs = nowMs,
        )
        LaunchedEffect(resolution.staleDismissals) {
            dismissals.clear(resolution.staleDismissals)
        }

        // Back closes the drawer first, then any overlay (UI-SPEC 3.0).
        BackHandler(enabled = drawerState.isOpen) {
            scope.launch { drawerState.close() }
        }
        BackHandler(enabled = drawerState.isClosed && state.overlay != Overlay.None) {
            viewModel.closeOverlay()
        }

        when (state.overlay) {
            Overlay.Settings -> {
                SettingsScreen(
                    state = state,
                    themeMode = themeMode,
                    appVersion = appVersion,
                    nowMs = nowMs,
                    onBack = viewModel::closeOverlay,
                    onThemeMode = onThemeMode,
                    onOpenAccounts = { viewModel.openOverlay(Overlay.Accounts) },
                    onStartDaemon = { viewModel.startDaemon() },
                    onStopDaemon = { viewModel.stopDaemon() },
                    onRestartDaemon = { viewModel.restartDaemon() },
                    onOpenAccessibility = { onSystemAction(SystemAction.OpenAccessibility) },
                    onGrantSms = { onSystemAction(SystemAction.GrantSms) },
                    onRequestNotifications = { onSystemAction(SystemAction.RequestNotifications) },
                    onOpenBattery = { onSystemAction(SystemAction.OpenBattery) },
                )
                return@ForgeTheme
            }
            Overlay.Accounts -> {
                AccountsScreen(
                    repository = accounts,
                    onBack = { viewModel.openOverlay(Overlay.Settings) },
                    onOpenUrl = onOpenUrl,
                )
                return@ForgeTheme
            }
            else -> Unit
        }

        ModalNavigationDrawer(
            drawerState = drawerState,
            scrimColor = colors.scrim,
            drawerContent = {
                ModalDrawerSheet(
                    drawerContainerColor = colors.surface,
                    drawerShape = androidx.compose.ui.graphics.RectangleShape,
                    modifier = Modifier.widthIn(max = drawerWidth),
                ) {
                    SessionDrawer(
                        state = state,
                        themeMode = themeMode,
                        appVersion = appVersion,
                        onClose = { scope.launch { drawerState.close() } },
                        onNewSession = {
                            viewModel.newSession()
                            scope.launch { drawerState.close() }
                        },
                        onNewSessionWith = { viewModel.openOverlay(Overlay.NewSessionWith) },
                        onSelect = { id ->
                            viewModel.activate(id)
                            scope.launch { drawerState.close() }
                        },
                        onRowAction = { id, action -> viewModel.applyRowAction(id, action) },
                        onFilter = viewModel::setFilter,
                        onQuery = viewModel::setQuery,
                        onStartDaemon = { viewModel.startDaemon() },
                        onStopDaemon = { viewModel.stopDaemon() },
                        onOpenDaemonDetails = { viewModel.openOverlay(Overlay.DaemonDetails) },
                        onOpenModel = { viewModel.openOverlay(Overlay.ModelPicker) },
                        onOpenSettings = { viewModel.openOverlay(Overlay.Settings) },
                        onThemeMode = onThemeMode,
                        onLoadMore = { viewModel.loadMoreSessions() },
                        nowMsProvider = nowMsProvider,
                        modifier = Modifier.windowInsetsPadding(WindowInsets.safeDrawing),
                    )
                }
            },
        ) {
            Column(
                Modifier
                    .fillMaxSize()
                    .background(colors.bg)
                    .windowInsetsPadding(WindowInsets.safeDrawing),
            ) {
                HaiderTopBar(
                    state = state,
                    onOpenDrawer = { scope.launch { drawerState.open() } },
                    onOpenSessionSheet = {
                        state.activeSessionId?.let { viewModel.openOverlay(Overlay.SessionActions(it)) }
                    },
                    onAction = { action -> viewModel.applyTopBarAction(action) },
                )
                StatusBanner(
                    model = resolution.model,
                    onAction = { action ->
                        when (action) {
                            BannerAction.StartDaemon -> viewModel.startDaemon()
                            BannerAction.OpenNeedsInput -> elsewhere?.let { viewModel.activate(it.id) }
                            BannerAction.RequestNotifications ->
                                onSystemAction(SystemAction.RequestNotifications)
                            BannerAction.OpenBatterySettings -> onSystemAction(SystemAction.OpenBattery)
                            BannerAction.OpenDaemonDetails -> viewModel.openOverlay(Overlay.DaemonDetails)
                            BannerAction.ContinueUpdate -> onUpdateAction()
                        }
                    },
                    onDismiss = { rank -> dismissals.dismiss(rank, nowMs) },
                )

                Box(Modifier.fillMaxWidth().weight(1f)) {
                    val needsInput = state.activeSession?.needsInput
                    if (state.messages.isEmpty() && needsInput == null) {
                        StartSurface(
                            state = state,
                            appVersion = appVersion,
                            nowMs = nowMs,
                            onStepAction = { step ->
                                when (step) {
                                    SetupStepId.RunService -> viewModel.startDaemon()
                                    SetupStepId.Notifications ->
                                        onSystemAction(SystemAction.RequestNotifications)
                                    SetupStepId.Battery -> onSystemAction(SystemAction.OpenBattery)
                                    SetupStepId.Model -> viewModel.openOverlay(Overlay.ModelPicker)
                                }
                            },
                            // Fills the composer; the user stays the author.
                            onSuggestion = viewModel::setDraft,
                            onSelectSession = viewModel::activate,
                            onSeeAllSessions = { scope.launch { drawerState.open() } },
                        )
                    } else {
                        Column(Modifier.fillMaxSize()) {
                            if (needsInput != null) {
                                InputRequiredCard(
                                    needsInput = needsInput,
                                    nowMs = nowMs,
                                    answeredElsewhere = needsInput.menuId in state.answeredElsewhere,
                                    onAnswer = { key, index, text ->
                                        val session = state.activeSessionId ?: return@InputRequiredCard
                                        viewModel.answer(
                                            session,
                                            needsInput.menuId.orEmpty(),
                                            key,
                                            index,
                                            text,
                                        )
                                    },
                                    onOpenSecretVault = {
                                        viewModel.openOverlay(Overlay.DaemonDetails)
                                    },
                                    modifier = Modifier.padding(
                                        horizontal = ForgeSpace.xl,
                                        vertical = ForgeSpace.lg,
                                    ),
                                )
                            }
                            state.transcriptNotice?.let { notice ->
                                Text(
                                    notice,
                                    style = Forge.type.sessionMeta,
                                    color = colors.amber,
                                    modifier = Modifier.padding(
                                        horizontal = ForgeSpace.xl,
                                        vertical = ForgeSpace.md,
                                    ),
                                )
                            }
                            Box(Modifier.fillMaxSize()) {
                                Transcript(
                                    messages = state.messages,
                                    onRetry = { viewModel.send() },
                                    modifier = Modifier.fillMaxSize(),
                                )
                                // Nothing is streaming while the turn is parked
                                // on a question: the answer is the affordance.
                                if (state.turnRunning && !state.needsInputHere) {
                                    StickyStopChip(
                                        onStop = { viewModel.stopTurn() },
                                        modifier = Modifier
                                            .align(Alignment.BottomCenter)
                                            .padding(bottom = ForgeSpace.md),
                                    )
                                }
                            }
                        }
                    }
                }

                val composerState = SendButtonMatrix.resolve(
                    daemon = state.daemon,
                    turnRunning = state.turnRunning,
                    inputRequired = state.needsInputHere,
                    hasText = state.draft.isNotBlank(),
                    setupComplete = state.setup.complete || state.sessions.isNotEmpty(),
                )
                val chip = ModelChipStateMachine.resolve(
                    daemon = state.daemon,
                    config = state.models,
                    catalogError = state.catalogError,
                    selectionBusy = state.selectionBusy,
                    requestedAtMs = state.catalogRequestedAtMs,
                    nowMs = nowMs,
                )
                Box(Modifier.fillMaxWidth().imePadding(), contentAlignment = Alignment.Center) {
                    Composer(
                        text = state.draft,
                        onTextChange = viewModel::setDraft,
                        composer = composerState,
                        chip = chip,
                        contextTokens = state.activeSession?.footprintTokens,
                        contextExact = state.activeSession?.footprintExact,
                        onSend = { viewModel.send() },
                        onStop = { viewModel.stopTurn() },
                        onStartDaemon = { viewModel.startDaemon() },
                        onOpenModel = { viewModel.openOverlay(Overlay.ModelPicker) },
                        onRetryModels = { viewModel.refreshModels() },
                        onAttach = { viewModel.openOverlay(Overlay.Attach) },
                        modifier = Modifier.widthIn(max = ForgeSize.readableMax),
                    )
                }
            }
        }

        Overlays(
            viewModel = viewModel,
            state = state,
            onSystemAction = onSystemAction,
        )
    }
}

@Composable
private fun Overlays(
    viewModel: ChatViewModel,
    state: ai.diffforge.haider.ui.state.AppUiState,
    onSystemAction: (SystemAction) -> Unit,
) {
    when (val overlay = state.overlay) {
        Overlay.ModelPicker, Overlay.NewSessionWith -> ModelPicker(
            config = state.models,
            error = state.catalogError,
            busy = state.selectionBusy,
            onSelectModel = viewModel::selectModel,
            onSelectEffort = viewModel::selectEffort,
            onRefresh = { viewModel.refreshModels() },
            onDismiss = viewModel::closeOverlay,
        )
        Overlay.Attach -> AttachSheet(
            onDismiss = viewModel::closeOverlay,
            onScreenshot = {
                viewModel.closeOverlay()
                onSystemAction(SystemAction.Screenshot)
            },
            onPickFile = {
                viewModel.closeOverlay()
                onSystemAction(SystemAction.PickFile)
            },
        )
        Overlay.DaemonDetails -> DaemonDetailsSheet(
            status = state.daemon,
            onDismiss = viewModel::closeOverlay,
            onRestart = {
                viewModel.closeOverlay()
                viewModel.restartDaemon()
            },
            onCopyDiagnostics = { onSystemAction(SystemAction.CopyText(it)) },
        )
        is Overlay.SessionActions -> viewModel.session(overlay.sessionId)?.let { row ->
            SessionActionsSheet(
                row = row,
                onDismiss = viewModel::closeOverlay,
                onAction = { action -> viewModel.applyRowAction(row.id, action) },
            )
        }
        is Overlay.Rename -> RenameSheet(
            current = overlay.current,
            onDismiss = viewModel::closeOverlay,
            onRename = { title -> viewModel.rename(overlay.sessionId, title) },
        )
        else -> Unit
    }
}

/** Everything the UI needs from the platform, kept out of the composables. */
sealed interface SystemAction {
    data object RequestNotifications : SystemAction
    data object OpenBattery : SystemAction
    data object OpenAccessibility : SystemAction
    data object GrantSms : SystemAction
    data object Screenshot : SystemAction
    data object PickFile : SystemAction
    data class CopyText(val text: String) : SystemAction
}

/** Bridges the row/overflow menus onto the view model. */
fun ChatViewModel.applyRowAction(sessionId: String, action: SessionRowAction) {
    when (action) {
        SessionRowAction.Rename -> openOverlay(
            Overlay.Rename(sessionId, session(sessionId)?.title.orEmpty()),
        )
        SessionRowAction.Fork -> fork(sessionId)
        SessionRowAction.StopTurn -> stopTurn(sessionId)
        SessionRowAction.CopyId -> closeOverlay()
    }
}

fun ChatViewModel.applyTopBarAction(action: TopBarAction) {
    val sessionId = state.value.activeSessionId
    when (action) {
        TopBarAction.SessionDetails -> sessionId?.let { openOverlay(Overlay.SessionActions(it)) }
        TopBarAction.Rename -> sessionId?.let {
            openOverlay(Overlay.Rename(it, session(it)?.title.orEmpty()))
        }
        TopBarAction.Fork -> sessionId?.let { fork(it) }
        TopBarAction.StopTurn -> stopTurn(sessionId)
        TopBarAction.ClearTranscript -> sessionId?.let { clearTranscript(it) }
        TopBarAction.Settings -> openOverlay(Overlay.Settings)
    }
}
