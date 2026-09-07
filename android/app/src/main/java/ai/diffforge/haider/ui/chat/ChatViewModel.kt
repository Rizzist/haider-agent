package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.daemon.DaemonService
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.MissingRunCoordinates
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.TranscriptLoad
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.SessionFilter
import ai.diffforge.haider.ui.state.SetupPlan
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
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
) : ViewModel() {

    private val _state = MutableStateFlow(AppUiState())
    val state: StateFlow<AppUiState> = _state.asStateFlow()

    private val transcripts = mutableMapOf<String, List<Message>>()
    private val drafts = mutableMapOf<String, String>()
    private var batterySkipped = false

    init {
        viewModelScope.launch {
            service.status.collect { status -> update { it.copy(daemon = status) }; recomputeSetup() }
        }
        viewModelScope.launch {
            service.environment.collect { env -> update { it.copy(environment = env) }; recomputeSetup() }
        }
        viewModelScope.launch {
            service.sessions.collect { rows -> update { it.copy(sessions = rows) } }
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
    }

    // ---------- daemon lifecycle ----------

    fun startDaemon() = viewModelScope.launch { service.start() }

    fun stopDaemon() = viewModelScope.launch { service.stop() }

    fun restartDaemon() = viewModelScope.launch { service.restart() }

    // ---------- roster ----------

    fun refreshRoster() = viewModelScope.launch { service.refreshRoster() }

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

    fun answer(sessionId: String, menuId: String, optionKey: String, optionIndex: Int, text: String? = null) =
        viewModelScope.launch {
            runCatching { service.answer(sessionId, menuId, optionKey, optionIndex, text) }
                .onFailure { error ->
                    if (error.message?.contains(ALREADY_RESOLVED) == true) {
                        update { it.copy(answeredElsewhere = it.answeredElsewhere + menuId) }
                    }
                }
            loadTranscript(sessionId)
        }

    // ---------- model catalog ----------

    fun selectModel(provider: String, model: String) = viewModelScope.launch {
        update { it.copy(selectionBusy = true) }
        runCatching { service.selectModel(provider, model) }
        update { it.copy(selectionBusy = false) }
    }

    fun selectEffort(effort: String?) = viewModelScope.launch {
        update { it.copy(selectionBusy = true) }
        runCatching { service.selectEffort(effort) }
        update { it.copy(selectionBusy = false) }
    }

    fun refreshModels() = viewModelScope.launch { service.refreshModels() }

    // ---------- surfaces ----------

    fun openOverlay(overlay: Overlay) = update { it.copy(overlay = overlay) }

    fun closeOverlay() = update { it.copy(overlay = Overlay.None) }

    fun setFilter(filter: SessionFilter) = update { it.copy(filter = filter) }

    fun setQuery(query: String) = update { it.copy(query = query) }

    fun skipBatteryStep() {
        batterySkipped = true
        recomputeSetup()
    }

    fun session(id: String?): SessionRow? = _state.value.sessions.firstOrNull { it.id == id }

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
