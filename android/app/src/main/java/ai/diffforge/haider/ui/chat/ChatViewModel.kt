package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.daemon.DaemonService
import ai.diffforge.haider.ui.daemon.AttachmentLimits
import ai.diffforge.haider.ui.daemon.Delivery
import ai.diffforge.haider.ui.daemon.TurnRefused
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.MissingRunCoordinates
import ai.diffforge.haider.ui.daemon.MenuAnswerInput
import ai.diffforge.haider.ui.daemon.MenuCoordinates
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.TranscriptLoad
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.PermissionMode
import ai.diffforge.haider.ui.state.PermissionSnapshot
import ai.diffforge.haider.ui.state.PermissionStanding
import ai.diffforge.haider.ui.state.SessionFilter
import ai.diffforge.haider.ui.state.SelectionRefusal
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
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
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

    /**
     * Declared before `init`, because the status collector below can call
     * [ensureActiveSession] during construction — a mutex declared after it is
     * still null when the first Running arrives.
     */
    private val creationGate = Mutex()

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
            // collectLatest: switching sessions cancels the previous session's
            // stream rather than folding two transcripts into one screen.
            service.activeSessionId.collectLatest { id ->
                update { it.copy(activeSessionId = id, draft = drafts[id].orEmpty()) }
                if (id == null) return@collectLatest
                update { it.copy(transcriptLoading = true) }
                // The stream performs the initial load itself and then emits
                // each folded push. Round 6 called the one-shot transcript()
                // and only reloaded after explicit actions, so assistant output
                // that arrived on its own never reached the screen.
                service.transcriptUpdates(id).collect { load -> applyTranscript(id, load) }
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
        viewModelScope.launch {
            service.shell.collect { availability -> update { it.copy(shell = availability) } }
        }
        viewModelScope.launch {
            service.permissionMode.collect { mode -> update { it.copy(permissionMode = mode) } }
        }
        viewModelScope.launch {
            service.queue.collect { snapshot -> update { it.copy(queue = snapshot) } }
        }
        viewModelScope.launch {
            service.usage.collect { snapshot -> update { it.copy(usage = snapshot) } }
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

    // There is deliberately no clearTranscript. The daemon has no session.clear
    // RPC, so the only thing this could do was empty the local list while every
    // message stayed on disk — the transcript came straight back on the next
    // attach. contracts-v1 forbids an action that implies durable deletion it
    // cannot perform (verify-6 O1).

    // ---------- the turn ----------

    fun setDraft(text: String) {
        val id = _state.value.activeSessionId
        if (id != null) drafts[id] = text
        update { it.copy(draft = text) }
    }

    fun send(mode: Delivery = Delivery.Steer) = viewModelScope.launch {
        val current = _state.value
        val text = current.draft.trim()
        val attachments = current.draftAttachments
        if (text.isEmpty() && attachments.isEmpty()) return@launch
        if (current.daemon !is DaemonStatus.Running) service.start()
        val id = current.activeSessionId ?: service.createSession()
        // The daemon's refusal is the daemon's to word: too_many_attachments
        // and attachments_too_large are its codes, shown verbatim.
        val refusal = runCatching { service.send(id, text, attachments, mode) }
            .exceptionOrNull()
        if (refusal != null) {
            update {
                it.copy(
                    attachmentNotice = (refusal as? TurnRefused)?.code ?: refusal.message,
                    deliveryChooser = false,
                )
            }
            return@launch
        }
        drafts[id] = ""
        update {
            it.copy(draft = "", draftAttachments = emptyList(), deliveryChooser = false)
        }
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

    fun selectModel(provider: String, model: String, confirmNewEpoch: Boolean = false) =
        viewModelScope.launch {
            // The chip's deadline anchors on whatever is in flight, so a
            // selection that never returns expires exactly like a catalog that
            // never arrives.
            update {
                it.copy(
                    selectionBusy = true,
                    catalogRequestedAtMs = clock(),
                    selectionRefusal = null,
                )
            }
            // The refusal is rendered, not swallowed: runCatching used to drop
            // it and the chip then showed a model the daemon had not accepted.
            val failure = runCatching { service.selectModel(provider, model, confirmNewEpoch) }
                .exceptionOrNull()
            update {
                it.copy(
                    selectionBusy = false,
                    selectionRefusal = failure?.let { error ->
                        SelectionRefusal(
                            code = error.message ?: "selection_refused",
                            provider = provider,
                            model = model,
                        )
                    },
                )
            }
        }

    fun selectEffort(effort: String?, confirmNewEpoch: Boolean = false) = viewModelScope.launch {
        update {
            it.copy(selectionBusy = true, catalogRequestedAtMs = clock(), selectionRefusal = null)
        }
        val failure = runCatching { service.selectEffort(effort, confirmNewEpoch) }.exceptionOrNull()
        update {
            it.copy(
                selectionBusy = false,
                selectionRefusal = failure?.let { error ->
                    SelectionRefusal(code = error.message ?: "selection_refused", effort = effort)
                },
            )
        }
    }

    /**
     * Retry the refused selection *with* the user's confirmation.
     *
     * Only this path may set `confirm_new_epoch`, and only because somebody
     * pressed the button that calls it (lane 971-3 handoff).
     */
    fun confirmRefusedSelection() {
        val refusal = _state.value.selectionRefusal ?: return
        when {
            refusal.model != null && refusal.provider != null ->
                selectModel(refusal.provider, refusal.model, confirmNewEpoch = true)
            else -> selectEffort(refusal.effort, confirmNewEpoch = true)
        }
    }

    fun dismissSelectionRefusal() = update { it.copy(selectionRefusal = null) }

    // ---------- attachments ----------

    /**
     * Stages one local file and adds the block it returns to the draft.
     *
     * A null return is the daemon refusing to take it; the reason is shown
     * rather than guessed, and the draft is left alone.
     */
    fun attach(bytes: ByteArray, mime: String, name: String?) = viewModelScope.launch {
        val staged = service.stageAttachment(bytes, mime, name)
        if (staged == null) {
            update { it.copy(attachmentNotice = AttachmentLimits.TOO_LARGE) }
            return@launch
        }
        update { it.copy(draftAttachments = it.draftAttachments + staged) }
    }

    fun removeAttachment(artifact: String) = update {
        it.copy(draftAttachments = it.draftAttachments.filterNot { block -> block.artifact == artifact })
    }

    fun dismissAttachmentNotice() = update { it.copy(attachmentNotice = null) }

    // ---------- steering ----------

    /** Opens the queue-or-steer chooser; only shown while a turn is running. */
    fun askDelivery() = update { it.copy(deliveryChooser = true) }

    fun dismissDelivery() = update { it.copy(deliveryChooser = false) }

    // ---------- the queue ----------

    fun refreshQueue() = viewModelScope.launch {
        _state.value.activeSessionId?.let { service.refreshQueue(it) }
    }

    /**
     * Both queue mutations carry the revision the row was read at, so a stale
     * tap is refused by the daemon instead of hitting whatever moved into that
     * position.
     */
    fun removeQueued(id: String) = viewModelScope.launch {
        val sessionId = _state.value.activeSessionId ?: return@launch
        val snapshot = _state.value.queue
        runCatching { service.removeQueued(sessionId, id, snapshot.revision) }
            .onFailure { error -> update { it.copy(queueNotice = error.message) } }
    }

    fun promoteQueued(id: String) = viewModelScope.launch {
        val sessionId = _state.value.activeSessionId ?: return@launch
        val snapshot = _state.value.queue
        runCatching { service.promoteQueued(sessionId, id, snapshot.revision) }
            .onFailure { error -> update { it.copy(queueNotice = error.message) } }
    }

    fun dismissQueueNotice() = update { it.copy(queueNotice = null) }

    /**
     * The daemon owns the policy, so this is a request, not a local toggle:
     * the mode the UI shows is whatever the facade reports back (addition H6).
     */
    fun selectPermissionMode(mode: PermissionMode) = viewModelScope.launch {
        service.setPermissionMode(mode)
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

    fun selectViewTab(tab: ai.diffforge.haider.ui.state.SessionViewTab) =
        update { it.copy(viewTab = tab) }

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

    /**
     * The Activity is the only thing that can see Android's own answer, so it
     * pushes what it observed. Called on resume and after a permission result
     * (verify-6 O3).
     */
    fun onPermissionsObserved(snapshot: PermissionSnapshot) {
        update { it.copy(permissions = snapshot) }
        // The autonomy step is done when notifications and SMS are both in.
        recomputeSetup()
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
        // Several Running emissions arrive in a row on a real connection, and
        // each used to race the others into createSession(). The mutex makes
        // this one operation at a time, and every fact it decides on is read
        // *inside* the lock — a roster read taken before waiting is already
        // stale by the time the wait ends (lane 971-3 handoff).
        creationGate.withLock {
            if (service.activeSessionId.value != null) return@withLock
            if (_state.value.daemon !is DaemonStatus.Running) return@withLock
            // Nothing may be created against a roster that has not hydrated
            // against this epoch's baseline: that is how a second session
            // appears beside one that already existed.
            service.rosterReady.first { it }
            if (service.activeSessionId.value != null) return@withLock
            val candidate = SessionListState.order(service.sessions.value, null).firstOrNull()
            val id = candidate?.id ?: service.createSession()
            service.activate(id)
            service.markSeen(id)
        }
    }


    /**
     * A replay can be genuinely partial: `session.read` ranges are capped at
     * 1,024 envelopes and an oversized envelope has to take an unavailable path.
     * The notice is surfaced, never swallowed.
     */
    private fun loadTranscript(sessionId: String) {
        viewModelScope.launch { applyTranscript(sessionId, service.transcript(sessionId)) }
    }

    /**
     * Complete, Partial and Unavailable all stay visible. An unsupported tool
     * or display payload is deliberately Partial: it is not hidden behind a
     * Complete result (lane 971-3 handoff).
     */
    private fun applyTranscript(sessionId: String, load: TranscriptLoad) {
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

    private fun recomputeSetup() = update { current ->
        current.copy(
            setup = SetupPlan.build(
                daemonRunning = current.daemon is DaemonStatus.Running,
                notificationsGranted = current.environment.notificationsGranted,
                notificationsSupported = notificationsSupported,
                batteryRestricted = current.environment.batteryRestricted,
                batterySkipped = batterySkipped,
                modelResolved = current.models != null,
                smsGranted = current.permissions.sms == PermissionStanding.Granted,
            ),
        )
    }

    private inline fun update(transform: (AppUiState) -> AppUiState) {
        _state.value = transform(_state.value)
    }

    companion object {
        const val SEARCH_DEBOUNCE_MS = 150L
        /**
         * The stable code the daemon actually sends. Round 4 compared the name
         * of the Rust constant, which no frame ever carries, so a menu that had
         * already been answered elsewhere read as an unexplained failure
         * (lane 971-3 handoff).
         */
        const val ALREADY_RESOLVED = "already_resolved"

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
