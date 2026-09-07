package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.daemon.DaemonService
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.MissingRunCoordinates
import ai.diffforge.haider.ui.daemon.MenuAnswerInput
import ai.diffforge.haider.ui.daemon.MenuCoordinates
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.TranscriptLoad
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.SessionFilter
import ai.diffforge.haider.ui.state.SessionListState
import ai.diffforge.haider.ui.state.SetupPlan
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * The app's single state owner.
 *
 * 970 held nine independent `mutableStateOf` fields and one implicit
 * conversation. 971 is multi-session: transcripts and drafts are keyed by
 * session id, and everything the tree reads is one immutable [AppUiState].
 */
class ChatViewModel(
    private val service: DaemonService,
    private val clock: () -> Long = System::currentTimeMillis,
    private val notificationsSupported: Boolean = true,
    /** Typing settles before the roster is searched; zero in tests. */
    private val searchDebounceMs: Long = SEARCH_DEBOUNCE_MS,
) : ViewModel() {

    private val _state = MutableStateFlow(AppUiState())
    val state: StateFlow<AppUiState> = _state.asStateFlow()

    private val transcripts = mutableMapOf<String, List<Message>>()
    private val drafts = mutableMapOf<String, String>()
    private var batterySkipped = false
    private var searchJob: Job? = null
    private var commandSeq = 0L

    init {
        viewModelScope.launch {
            service.status.collect { status ->
                update { it.copy(daemon = status) }
                recomputeSetup()
                // Addition D: with the daemon enabled the app opens straight
                // into a session. There is no setup screen to walk past once
                // there is nothing left to set up.
                if (status is DaemonStatus.Running) ensureActiveSession()
            }
        }
        viewModelScope.launch {
            service.environment.collect { env -> update { it.copy(environment = env) }; recomputeSetup() }
        }
        viewModelScope.launch {
            service.sessions.collect { rows ->
                update { current ->
                    current.copy(
                        sessions = rows,
                        // While the drawer is open the freeze absorbs new rows
                        // rather than leaving them outside it: they are ranked
                        // once, on arrival, and do not move again until it
                        // closes.
                        orderSnapshot = current.orderSnapshot?.extend(rows, current.activeSessionId),
                    )
                }
                // Pages that arrive after a query was typed have to join it,
                // or the result silently describes a smaller roster than the
                // one the user is looking at.
                val query = _state.value.query
                if (query.isNotBlank()) rerunSearch(query)
            }
        }
        viewModelScope.launch {
            service.paging.collect { paging -> update { it.copy(paging = paging) } }
        }
        viewModelScope.launch {
            service.activeSessionId.collect { id ->
                update { it.copy(activeSessionId = id, draft = drafts[id].orEmpty()) }
                if (id != null) loadTranscript(id)
            }
        }
        viewModelScope.launch {
            service.models.collect { config -> update { it.copy(models = config) }; recomputeSetup() }
        }
        viewModelScope.launch {
            service.catalogError.collect { error -> update { it.copy(catalogError = error) } }
        }
        viewModelScope.launch {
            service.catalogRequestedAtMs.collect { at -> update { it.copy(catalogRequestedAtMs = at) } }
        }
        viewModelScope.launch {
            service.searchIndex.collect { index -> update { it.copy(searchIndex = index) } }
        }
        viewModelScope.launch {
            service.providers.collect { inventory -> update { it.copy(providers = inventory) } }
        }
    }

    // ---------- daemon lifecycle ----------

    fun startDaemon() = viewModelScope.launch { service.start() }

    fun stopDaemon() = viewModelScope.launch { service.stop() }

    fun restartDaemon() = viewModelScope.launch { service.restart() }

    // ---------- roster ----------

    fun refreshRoster() = viewModelScope.launch { service.refreshRoster() }

    /**
     * Called when the drawer opens. `session_roster_delta` never reports
     * removals, so the authoritative list has to be re-read on open (C5), and
     * the rendered order is captured in the same breath so it can be frozen
     * until the drawer closes.
     */
    fun onDrawerOpened() {
        update { it.copy(orderSnapshot = SessionListState.OrderSnapshot.capture(it.sessions, it.activeSessionId)) }
        viewModelScope.launch { service.refreshRoster() }
    }

    /** The freeze lifts when the drawer closes, so the next open re-sorts. */
    fun onDrawerClosed() = update { it.copy(orderSnapshot = null) }

    fun loadMoreSessions() = viewModelScope.launch { service.loadMoreSessions() }

    fun newSession(model: String? = null, effort: String? = null) = viewModelScope.launch {
        val id = service.createSession(model, effort)
        loadTranscript(id)
    }

    fun activate(sessionId: String) = viewModelScope.launch {
        service.activate(sessionId)
        service.markSeen(sessionId)
        loadTranscript(sessionId)
    }

    fun rename(sessionId: String, title: String) = viewModelScope.launch {
        service.rename(sessionId, title)
        closeOverlay()
    }

    fun fork(sessionId: String) = viewModelScope.launch {
        val id = service.fork(sessionId)
        service.activate(id)
        closeOverlay()
    }

    fun clearTranscript(sessionId: String) {
        transcripts[sessionId] = emptyList()
        if (_state.value.activeSessionId == sessionId) update { it.copy(messages = emptyList()) }
    }

    // ---------- the turn ----------

    fun setDraft(text: String) {
        val id = _state.value.activeSessionId
        if (id != null) drafts[id] = text
        update { it.copy(draft = text) }
    }

    fun send() = viewModelScope.launch {
        val current = _state.value
        val text = current.draft.trim()
        if (text.isEmpty()) return@launch
        if (current.daemon !is DaemonStatus.Running) service.start()
        val id = current.activeSessionId ?: service.createSession()
        service.send(id, text)
        drafts[id] = ""
        update { it.copy(draft = "") }
        loadTranscript(id)
    }

    /**
     * Refuses without live coordinates: `run_id == null` means no active run, so
     * the button is not rendered and the call is not made
     * (UI-SPEC 5.3 non-negotiable 1).
     */
    fun stopTurn(sessionId: String? = _state.value.activeSessionId) = viewModelScope.launch {
        val id = sessionId ?: return@launch
        runCatching { service.stopTurn(id) }
            .onFailure { if (it !is MissingRunCoordinates) throw it }
        loadTranscript(id)
    }

    /**
     * Answers with coordinates read from the snapshot that rendered the card.
     * If any coordinate is missing there is nothing to compare and set, so no
     * answer is sent.
     */
    fun answer(
        rendered: MenuCoordinates,
        optionKey: String,
        optionIndex: Int,
        text: String? = null,
    ) = viewModelScope.launch {
        val live = liveCoordinates(rendered) ?: return@launch
        submitAnswer(live, optionKey, optionIndex, text?.let(MenuAnswerInput::Text))
    }

    /**
     * The masked path: the plaintext goes to `vault.stage` with purpose
     * `menu_secret`, the answer carries only the returned reference, and the
     * caller's buffer is wiped whatever happens.
     */
    fun answerSecret(
        rendered: MenuCoordinates,
        optionKey: String,
        optionIndex: Int,
        secret: CharArray,
    ) = viewModelScope.launch {
        try {
            val live = liveCoordinates(rendered) ?: return@launch
            val reference = runCatching { service.stageMenuSecret(secret) }.getOrNull()
                ?: return@launch
            submitAnswer(live, optionKey, optionIndex, MenuAnswerInput.Secret(reference))
        } finally {
            secret.fill(' ')
        }
    }

    /**
     * Compare-and-set means comparing. The card hands back the coordinates it
     * was *drawn* with; if the live snapshot now carries a different menu,
     * request sequence or worker generation, the prompt on screen is not the
     * prompt the daemon is waiting on, and the answer is dropped rather than
     * applied to whatever replaced it.
     */
    private fun liveCoordinates(rendered: MenuCoordinates): MenuCoordinates? {
        val current = MenuCoordinates.of(
            sessionId = rendered.sessionId,
            needsInput = session(rendered.sessionId)?.needsInput,
            commandId = "menu-answer-${++commandSeq}",
        ) ?: return null
        val matches = current.menuId == rendered.menuId &&
            current.requestSeq == rendered.requestSeq &&
            current.workerGeneration == rendered.workerGeneration
        if (!matches) {
            update { it.copy(answeredElsewhere = it.answeredElsewhere + rendered.menuId) }
            return null
        }
        return current
    }

    private suspend fun submitAnswer(
        coordinates: MenuCoordinates,
        optionKey: String,
        optionIndex: Int,
        input: MenuAnswerInput?,
    ) {
        runCatching { service.answer(coordinates, optionKey, optionIndex, input) }
            .onFailure { error ->
                if (error.message?.contains(ALREADY_RESOLVED) == true) {
                    update { it.copy(answeredElsewhere = it.answeredElsewhere + coordinates.menuId) }
                }
            }
        loadTranscript(coordinates.sessionId)
    }

    // ---------- model catalog ----------

    fun selectModel(provider: String, model: String) = viewModelScope.launch {
        // The chip's deadline anchors on whatever is in flight, so a selection
        // that never returns expires exactly like a catalog that never arrives.
        update { it.copy(selectionBusy = true, catalogRequestedAtMs = clock()) }
        runCatching { service.selectModel(provider, model) }
        update { it.copy(selectionBusy = false) }
    }

    fun selectEffort(effort: String?) = viewModelScope.launch {
        update { it.copy(selectionBusy = true, catalogRequestedAtMs = clock()) }
        runCatching { service.selectEffort(effort) }
        update { it.copy(selectionBusy = false) }
    }

    fun refreshModels() = viewModelScope.launch { service.refreshModels() }

    fun refreshProviders() = viewModelScope.launch { service.refreshProviders() }

    fun selectProvider(provider: String) = viewModelScope.launch {
        update { it.copy(selectionBusy = true, catalogRequestedAtMs = clock()) }
        runCatching { service.selectProvider(provider) }
        update { it.copy(selectionBusy = false) }
    }

    // ---------- surfaces ----------

    fun openOverlay(overlay: Overlay) = update { it.copy(overlay = overlay) }

    fun closeOverlay() = update { it.copy(overlay = Overlay.None) }

    fun setFilter(filter: SessionFilter) = update { it.copy(filter = filter) }

    /**
     * Metadata filtering is immediate; the full-roster transcript search runs
     * through the repository's paged read cache and its completeness is
     * surfaced rather than assumed.
     */
    fun setQuery(query: String) {
        update { it.copy(query = query, searching = query.isNotBlank()) }
        searchJob?.cancel()
        if (query.isBlank()) {
            update { it.copy(searchOutcome = null, searching = false) }
            return
        }
        searchJob = viewModelScope.launch {
            if (searchDebounceMs > 0) delay(searchDebounceMs)
            runSearch(query)
        }
    }

    private fun rerunSearch(query: String) {
        if (searchJob?.isActive == true) return
        searchJob = viewModelScope.launch { runSearch(query) }
    }

    private suspend fun runSearch(query: String) {
        val outcome = runCatching { service.search(query) }.getOrNull()
        update {
            if (it.query == query) it.copy(searchOutcome = outcome, searching = false) else it
        }
    }

    /** Called from the Activity's permission callback, granted or not. */
    fun onNotificationPermissionResult(granted: Boolean, permanentlyDenied: Boolean) =
        viewModelScope.launch {
            service.reportNotificationPermission(granted, permanentlyDenied)
        }

    fun skipBatteryStep() {
        batterySkipped = true
        recomputeSetup()
    }

    fun session(id: String?): SessionRow? = _state.value.sessions.firstOrNull { it.id == id }

    /**
     * Opens the session the user would expect to be looking at: the one already
     * active, else the most recent by the canonical order, else a fresh one.
     * Lanes 1/2/3 only have to make the facade truthful; this path is wired.
     */
    fun ensureActiveSession() = viewModelScope.launch {
        // The service's own value, not the mirrored state: the status flow can
        // arrive before the active-session flow has hydrated, and reading the
        // half-built copy would replace the session the daemon already has.
        if (service.activeSessionId.value != null) return@launch
        val current = _state.value
        if (current.activeSessionId != null) return@launch
        if (current.daemon !is DaemonStatus.Running) return@launch
        val candidate = SessionListState.order(service.sessions.value, null).firstOrNull()
        val id = candidate?.id ?: service.createSession()
        service.activate(id)
        service.markSeen(id)
        loadTranscript(id)
    }

    /**
     * A replay can be genuinely partial: `session.read` ranges are capped at
     * 1,024 envelopes and an oversized envelope has to take an unavailable path.
     * The notice is surfaced, never swallowed.
     */
    private fun loadTranscript(sessionId: String) {
        viewModelScope.launch {
            update { it.copy(transcriptLoading = true) }
            val load = service.transcript(sessionId)
            transcripts[sessionId] = load.messages
            val notice = when (load) {
                is TranscriptLoad.Complete -> null
                is TranscriptLoad.Partial ->
                    "History up to ${load.loadedThroughSeq} of ${load.headSeq} — ${load.reason}"
                is TranscriptLoad.Unavailable -> load.reason
            }
            update {
                if (it.activeSessionId == sessionId) {
                    it.copy(
                        messages = load.messages,
                        transcriptLoading = false,
                        transcriptNotice = notice,
                    )
                } else {
                    it.copy(transcriptLoading = false)
                }
            }
        }
    }

    private fun recomputeSetup() = update { current ->
        current.copy(
            setup = SetupPlan.build(
                daemonRunning = current.daemon is DaemonStatus.Running,
                notificationsGranted = current.environment.notificationsGranted,
                notificationsSupported = notificationsSupported,
                batteryRestricted = current.environment.batteryRestricted,
                batterySkipped = batterySkipped,
                modelResolved = current.models != null,
            ),
        )
    }

    private inline fun update(transform: (AppUiState) -> AppUiState) {
        _state.value = transform(_state.value)
    }

    companion object {
        const val SEARCH_DEBOUNCE_MS = 150L
        const val ALREADY_RESOLVED = "ERROR_CODE_ALREADY_RESOLVED"

        fun factory(
            service: DaemonService,
            clock: () -> Long = System::currentTimeMillis,
            notificationsSupported: Boolean = true,
        ): ViewModelProvider.Factory = object : ViewModelProvider.Factory {
            @Suppress("UNCHECKED_CAST")
            override fun <T : ViewModel> create(modelClass: Class<T>): T =
                ChatViewModel(service, clock, notificationsSupported) as T
        }
    }
}
