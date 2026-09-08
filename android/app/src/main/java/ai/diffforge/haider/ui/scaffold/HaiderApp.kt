package ai.diffforge.haider.ui.scaffold

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.accounts.AccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.chat.PickerKind
import ai.diffforge.haider.ui.chat.SessionPickerSheet
import ai.diffforge.haider.ui.daemon.MenuCoordinates
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.chat.Composer
import ai.diffforge.haider.ui.chat.InputRequiredCard
import ai.diffforge.haider.ui.chat.ModelPicker
import ai.diffforge.haider.ui.chat.ShellView
import ai.diffforge.haider.ui.chat.Transcript
import ai.diffforge.haider.ui.drawer.RenameSheet
import ai.diffforge.haider.ui.drawer.SessionActionsSheet
import ai.diffforge.haider.ui.drawer.SessionDrawer
import ai.diffforge.haider.ui.drawer.SessionRowAction
import ai.diffforge.haider.ui.daemon.FleetModel
import ai.diffforge.haider.ui.daemon.FleetLoad
import ai.diffforge.haider.ui.fleet.ChildTranscriptScreen
import ai.diffforge.haider.ui.fleet.FleetSheet
import ai.diffforge.haider.ui.fleet.SubagentStrip
import ai.diffforge.haider.ui.settings.AccountsScreen
import ai.diffforge.haider.ui.settings.SettingsScreen
import ai.diffforge.haider.ui.start.AutonomyGrant
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.PermissionStanding
import ai.diffforge.haider.ui.start.StartSurface
import ai.diffforge.haider.ui.state.BannerAction
import ai.diffforge.haider.ui.state.BannerInputs
import ai.diffforge.haider.ui.state.BannerResolver
import ai.diffforge.haider.ui.state.ModelChipState
import ai.diffforge.haider.ui.state.ModelChipStateMachine
import ai.diffforge.haider.ui.state.NeedsInputElsewhere
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.RelativeTime
import ai.diffforge.haider.ui.state.SendButtonMatrix
import ai.diffforge.haider.ui.state.SessionViewTab
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
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.res.stringResource
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
    oauth: OAuthAttemptController,
    appVersion: String,
    themeMode: ThemeMode,
    onThemeMode: (ThemeMode) -> Unit,
    updateState: UpdateUiState = UpdateUiState.Hidden,
    onUpdateAction: () -> Unit = {},
    onOpenUrl: (String) -> Unit = {},
    onSystemAction: (SystemAction) -> Unit = {},
    dismissals: ai.diffforge.haider.ui.state.BannerDismissals = InMemoryBannerDismissals(),
    nowMsProvider: () -> Long = System::currentTimeMillis,
    elapsedRealtimeProvider: () -> Long = android.os.SystemClock::elapsedRealtime,
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
                notificationsPermanentlyDenied = state.environment.notificationsPermanentlyDenied,
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

        // `session_roster_delta` never reports removals, so the authoritative
        // list is re-read on every open, and the rendered order is captured in
        // the same breath and frozen until the drawer closes.
        LaunchedEffect(drawerState.isOpen) {
            if (drawerState.isOpen) viewModel.onDrawerOpened() else viewModel.onDrawerClosed()
        }

        // Back closes the drawer first, then any overlay (UI-SPEC 3.0).
        BackHandler(enabled = drawerState.isOpen) {
            scope.launch { drawerState.close() }
        }
        BackHandler(enabled = drawerState.isClosed && state.overlay != Overlay.None) {
            viewModel.closeOverlay()
        }

        val accountsSnapshot by accounts.snapshot.collectAsState()
        val accountsSummary = if (accountsSnapshot.accounts.isEmpty()) {
            stringResource(R.string.accounts_status_empty)
        } else {
            stringResource(
                R.string.accounts_status_line,
                accountsSnapshot.accounts.size,
                accountsSnapshot.accounts.joinToString(", ") { it.provider },
            )
        }

        when (state.overlay) {
            Overlay.Settings -> {
                SettingsScreen(
                    state = state,
                    themeMode = themeMode,
                    appVersion = appVersion,
                    accountsSummary = accountsSummary,
                    elapsedRealtimeMs = elapsedRealtimeProvider(),
                    onBack = viewModel::closeOverlay,
                    onThemeMode = onThemeMode,
                    onOpenAccounts = { viewModel.openOverlay(Overlay.Accounts) },
                    onStartDaemon = { viewModel.startDaemon() },
                    onStopDaemon = { viewModel.stopDaemon() },
                    onRestartDaemon = { viewModel.restartDaemon() },
                    onOpenAccessibility = { onSystemAction(SystemAction.OpenAccessibility) },
                    onGrantSms = { onSystemAction(SystemAction.GrantSms) },
            onRequestScreenCapture = { onSystemAction(SystemAction.RequestScreenCapture) },
                    onRequestNotifications = { onSystemAction(SystemAction.RequestNotifications) },
                    onOpenBattery = { onSystemAction(SystemAction.OpenBattery) },
                )
                return@ForgeTheme
            }
            Overlay.Accounts -> {
                AccountsScreen(
                    repository = accounts,
                    oauth = oauth,
                    onBack = { viewModel.openOverlay(Overlay.Settings) },
                    onOpenUrl = onOpenUrl,
                )
                return@ForgeTheme
            }
            // A descendant's own transcript is a screen, not a sheet: it hosts
            // a full replay and its own input-required card, and a sheet over
            // the parent would put two transcripts on one surface.
            is Overlay.ChildTranscript -> {
                val open = state.childTranscript
                if (open != null) {
                    val roots = (state.fleet.active as? FleetLoad.Snapshot)
                        ?.snapshot?.roots.orEmpty()
                    ChildTranscriptScreen(
                        state = open,
                        node = FleetModel.find(roots, open.agentId),
                        // The child's own roster row, when the daemon lists it:
                        // that is where its needs-input card and coordinates
                        // come from. Absent, the screen shows no card at all.
                        row = viewModel.session(open.sessionId),
                        parentTitle = viewModel.session(open.parentSessionId)?.title
                            ?: open.parentSessionId,
                        nowMs = nowMs,
                        answeredElsewhere = state.answeredElsewhere,
                        onBack = viewModel::closeOverlay,
                        onAnswer = { rendered, key, index, text ->
                            viewModel.answer(rendered, key, index, text)
                        },
                        onAnswerSecret = { rendered, key, index, secret ->
                            viewModel.answerSecret(rendered, key, index, secret)
                        },
                    )
                    return@ForgeTheme
                }
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
                        // The footer's full-size row and the composer's 32 dp
                        // chip open the same sheet — that is what makes the
                        // chip a sanctioned shortcut rather than a lone path.
                        onOpenModel = { viewModel.openOverlay(Overlay.Picker(PickerKind.Model)) },
                        onOpenSettings = { viewModel.openOverlay(Overlay.Settings) },
                        onThemeMode = onThemeMode,
                        onLoadMore = { viewModel.loadMoreSessions() },
                        onToggleFamily = { viewModel.toggleFamily(it) },
                        onOpenFleet = {
                            scope.launch { drawerState.close() }
                            viewModel.openFleet()
                        },
                        nowMsProvider = nowMsProvider,
                        elapsedRealtimeProvider = elapsedRealtimeProvider,
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
                    dark = dark,
                    onOpenDrawer = { scope.launch { drawerState.open() } },
                    onToggleTheme = {
                        onThemeMode(if (dark) ThemeMode.Light else ThemeMode.Dark)
                    },
                    onRefresh = {
                        viewModel.refreshRoster()
                        viewModel.refreshModels()
                    },
                    onSelectTab = viewModel::selectViewTab,
                )
                StatusBanner(
                    model = resolution.model,
                    onAction = { action ->
                        when (action) {
                            BannerAction.StartDaemon -> viewModel.startDaemon()
                            BannerAction.OpenNeedsInput -> elsewhere?.let { viewModel.activate(it.id) }
                            BannerAction.RequestNotifications ->
                                onSystemAction(SystemAction.RequestNotifications)
                            BannerAction.OpenAppSettings ->
                                onSystemAction(SystemAction.OpenAppSettings)
                            BannerAction.OpenBatterySettings -> onSystemAction(SystemAction.OpenBattery)
                            BannerAction.OpenDaemonDetails -> viewModel.openOverlay(Overlay.DaemonDetails)
                            BannerAction.ContinueUpdate -> onUpdateAction()
                        }
                    },
                    onDismiss = { rank -> dismissals.dismiss(rank, nowMs) },
                )

                Box(Modifier.fillMaxWidth().weight(1f)) {
                    // Never hidden. Auto means the *daemon* resolves device
                    // approvals so none is raised; it does not mean the UI
                    // draws over one that is still pending. Round 9 filtered
                    // here, and switching to Auto with an unanswered sms.list
                    // card left the session stranded — header "Needs you",
                    // composer "Answer above…", and nothing to answer
                    // (verify-8 O1).
                    val needsInput = state.activeSession?.needsInput
                    if (state.viewTab == SessionViewTab.Shell) {
                        ShellView(availability = state.shell, modifier = Modifier.fillMaxSize())
                    } else if (state.messages.isEmpty() && needsInput == null) {
                        StartSurface(
                            state = state,
                            appVersion = appVersion,
                            nowMs = nowMs,
                            elapsedRealtimeMs = elapsedRealtimeProvider(),
                            onStepAction = { step ->
                                when (step) {
                                    SetupStepId.RunService -> viewModel.startDaemon()
                                    // "Allow all four": Android will only show
                                    // one dialog at a time, so this starts the
                                    // sequence at the first thing still
                                    // outstanding and `onResume` brings the
                                    // next one up (addition H6).
                                    SetupStepId.Autonomy -> onSystemAction(
                                        nextAutonomyRequest(state),
                                    )
                                    SetupStepId.Battery -> onSystemAction(SystemAction.OpenBattery)
                                    SetupStepId.Model ->
                                        viewModel.openOverlay(Overlay.Picker(PickerKind.Model))
                                }
                            },
                            // Fills the composer; the user stays the author.
                            onGrant = { grant ->
                                onSystemAction(
                                    when (grant) {
                                        AutonomyGrant.Notifications ->
                                            SystemAction.RequestNotifications
                                        AutonomyGrant.Sms -> SystemAction.GrantSms
                                        AutonomyGrant.Accessibility ->
                                            SystemAction.OpenAccessibility
                                        AutonomyGrant.ScreenCapture ->
                                            SystemAction.RequestScreenCapture
                                    },
                                )
                            },
                            onSelectSession = viewModel::activate,
                            onSeeAllSessions = { scope.launch { drawerState.open() } },
                        )
                    } else {
                        Column(Modifier.fillMaxSize()) {
                            if (needsInput != null) {
                                // Keyed on the prompt: a replacement is a new
                                // card, not a mutated one, so a callback held
                                // from the old card keeps the old coordinates
                                // and is refused rather than misapplied.
                                key(
                                    needsInput.menuId,
                                    needsInput.requestSeq,
                                    needsInput.workerGeneration,
                                ) {
                                InputRequiredCard(
                                    needsInput = needsInput,
                                    nowMs = nowMs,
                                    answeredElsewhere = needsInput.menuId in state.answeredElsewhere,
                                    // No coordinates, no answer affordance: a
                                    // compare-and-set needs all of them.
                                    coordinates = MenuCoordinates.of(
                                        sessionId = state.activeSessionId.orEmpty(),
                                        needsInput = needsInput,
                                        commandId = "rendered",
                                    ),
                                    onAnswer = { rendered, key, index, text ->
                                        viewModel.answer(rendered, key, index, text)
                                    },
                                    onAnswerSecret = { rendered, key, index, secret ->
                                        viewModel.answerSecret(rendered, key, index, secret)
                                    },
                                    modifier = Modifier.padding(
                                        horizontal = ForgeSpace.xl,
                                        vertical = ForgeSpace.lg,
                                    ),
                                )
                                }
                            }
                            // The session's own delegated agents, from
                            // `session.observe` (frame.rs:2271). Renders
                            // nothing when the daemon published none.
                            SubagentStrip(
                                load = state.fleet.subagents,
                                fleet = state.fleet.active,
                                // A chip comes from *this* session's observe
                                // digest, so this session is its parent by
                                // construction — not an inferred one.
                                onOpenChild = { childId, agentId ->
                                    viewModel.openChildTranscript(
                                        sessionId = childId,
                                        agentId = agentId,
                                        parentSessionId = state.activeSessionId,
                                    )
                                },
                                onOpenFleet = { viewModel.openFleet() },
                            )
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
                            // One Stop control, and it lives in the composer.
                            Transcript(
                                messages = state.messages,
                                onRetry = { viewModel.send() },
                                modifier = Modifier.fillMaxSize(),
                            )
                        }
                    }
                }

                val setupPending = !state.setup.complete && state.sessions.isEmpty()
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
                        // The picker row is meaningless before the daemon can
                        // answer; the input stays visible and disabled so the
                        // shape of the screen does not jump (addition F, F2).
                        showPickers = !setupPending,
                        text = state.draft,
                        onTextChange = viewModel::setDraft,
                        composer = composerState,
                        chip = chip,
                        effort = state.models?.current?.effort,
                        permissionMode = state.permissionMode,
                        onSend = { viewModel.send() },
                        onStop = { viewModel.stopTurn() },
                        onStartDaemon = { viewModel.startDaemon() },
                        onOpenModel = { viewModel.openOverlay(Overlay.Picker(PickerKind.Model)) },
                        onOpenEffort = { viewModel.openOverlay(Overlay.Picker(PickerKind.Effort)) },
                        onOpenPermissions = {
                            viewModel.openOverlay(Overlay.Picker(PickerKind.Permissions))
                        },
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
            nowMs = nowMs,
            onSystemAction = onSystemAction,
        )
    }
}

@Composable
private fun Overlays(
    viewModel: ChatViewModel,
    state: ai.diffforge.haider.ui.state.AppUiState,
    nowMs: Long,
    onSystemAction: (SystemAction) -> Unit,
) {
    // Opening any picker asks for what it needs; the legacy sheet never did,
    // which is how first run could sit on an empty catalog forever.
    LaunchedEffect(state.overlay) {
        if (state.overlay is Overlay.Picker && state.models == null) {
            viewModel.refreshProviders()
            viewModel.refreshModels()
        }
    }
    when (val overlay = state.overlay) {
        is Overlay.Picker -> SessionPickerSheet(
            kind = overlay.kind,
            busy = state.selectionBusy,
            refusal = state.selectionRefusal,
            permissionMode = state.permissionMode,
            onSelectPermissionMode = viewModel::selectPermissionMode,
            onConfirmRefused = viewModel::confirmRefusedSelection,
            onDismissRefusal = viewModel::dismissSelectionRefusal,
            inventory = state.providers,
            currentProvider = state.models?.current?.provider,
            currentModel = state.models?.current?.model,
            currentEffort = state.models?.current?.effort,
            // The same 6 s rule the composer chip obeys: a sheet is not allowed
            // to sit on "asking…" either.
            pending = ModelChipStateMachine.resolve(
                daemon = state.daemon,
                config = state.models,
                catalogError = state.catalogError,
                selectionBusy = state.selectionBusy,
                requestedAtMs = state.catalogRequestedAtMs,
                nowMs = nowMs,
            ) == ModelChipState.Loading,
            onRetry = {
                viewModel.refreshProviders()
                viewModel.refreshModels()
            },
            onDismiss = viewModel::closeOverlay,
            onSelectModel = viewModel::selectModel,
            onSelectEffort = viewModel::selectEffort,
        )
        Overlay.ModelPicker, Overlay.NewSessionWith -> ModelPicker(
            config = state.models,
            error = state.catalogError,
            busy = state.selectionBusy,
            refusal = state.selectionRefusal,
            onSelectModel = { provider, model -> viewModel.selectModel(provider, model) },
            onSelectEffort = { viewModel.selectEffort(it) },
            onConfirmRefused = viewModel::confirmRefusedSelection,
            onDismissRefusal = viewModel::dismissSelectionRefusal,
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
        Overlay.Fleet -> FleetSheet(
            panel = state.fleet.panel,
            sessions = state.sessions,
            loading = state.fleet.panelLoading,
            onDismiss = viewModel::closeOverlay,
            onJump = { sessionId ->
                viewModel.closeOverlay()
                viewModel.activate(sessionId)
            },
            onOpenChild = { childId, agentId, parentId ->
                viewModel.openChildTranscript(
                    sessionId = childId,
                    agentId = agentId,
                    parentSessionId = parentId,
                )
            },
            onRefresh = { viewModel.openFleet() },
        )
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

    /** For a permanently denied permission: the dialog will not come back. */
    data object OpenAppSettings : SystemAction
    data object OpenBattery : SystemAction
    data object OpenAccessibility : SystemAction
    data object GrantSms : SystemAction

    /**
     * MediaProjection consent. Android issues it per projection session and
     * has no pre-grant, so this is asked on first run and again whenever the
     * projection token is gone — never treated as a stored permission.
     */
    data object RequestScreenCapture : SystemAction
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
        SessionRowAction.CopyId -> closeOverlay()
    }
}

/**
 * The next outstanding autonomy popup, in the order H6 lists them.
 *
 * Android shows one system dialog at a time, so "Allow all four" starts the
 * sequence rather than firing four intents that would stack on each other.
 */
fun nextAutonomyRequest(state: AppUiState): SystemAction = when {
    !state.environment.notificationsGranted -> SystemAction.RequestNotifications
    state.permissions.sms != PermissionStanding.Granted -> SystemAction.GrantSms
    state.permissions.accessibility != PermissionStanding.Granted ->
        SystemAction.OpenAccessibility
    else -> SystemAction.RequestScreenCapture
}
