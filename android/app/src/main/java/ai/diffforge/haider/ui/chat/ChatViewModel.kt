package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.checkpoints.BranchOutcome
import ai.diffforge.haider.ui.checkpoints.CheckpointCursorState
import ai.diffforge.haider.ui.checkpoints.CheckpointGesture
import ai.diffforge.haider.ui.checkpoints.CheckpointListResult
import ai.diffforge.haider.ui.checkpoints.CheckpointOutcome
import ai.diffforge.haider.ui.checkpoints.Checkpoints
import ai.diffforge.haider.ui.checkpoints.CheckpointsUiState
import ai.diffforge.haider.ui.daemon.DaemonService
import ai.diffforge.haider.ui.daemon.Attachment
import ai.diffforge.haider.ui.daemon.AttachmentLimits
import ai.diffforge.haider.ui.daemon.AttachmentRefused
import ai.diffforge.haider.ui.daemon.Delivery
import ai.diffforge.haider.ui.daemon.TurnRefused
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.FleetLoad
import ai.diffforge.haider.ui.daemon.SubagentLoad
import ai.diffforge.haider.ui.daemon.MissingRunCoordinates
import ai.diffforge.haider.ui.daemon.MenuAnswerInput
import ai.diffforge.haider.ui.daemon.MenuCoordinates
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.TranscriptLoad
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.ChildTranscriptState
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.OverlayNavigation
import ai.diffforge.haider.ui.state.PermissionMode
import ai.diffforge.haider.ui.state.PermissionSnapshot
import ai.diffforge.haider.ui.state.PermissionStanding
import ai.diffforge.haider.ui.state.SessionFilter
import ai.diffforge.haider.ui.state.SelectionRefusal
import ai.diffforge.haider.ui.state.SessionListState
import ai.diffforge.haider.ui.state.SessionTree
import ai.diffforge.haider.ui.state.SetupPlan
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
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

    private val _state = MutableStateFlow(AppUiState(
        permissionMode = service.permissionMode.value,
        supportedPermissionModes = service.supportedPermissionModes,
    ))
    val state: StateFlow<AppUiState> = _state.asStateFlow()

    private val transcripts = mutableMapOf<String, List<Message>>()
    private val drafts = mutableMapOf<String, String>()

    /**
     * Staged attachments, per session — like [drafts], and for the same reason.
     *
     * Round 12 kept one global list, so an image staged in one session followed
     * the user into the next one's composer and would have been submitted with
     * somebody else's message (verify-11 O10). The notice is scoped the same
     * way: a refusal belongs to the session that earned it.
     */
    private val draftAttachments = mutableMapOf<String, List<Attachment>>()
    private val attachmentNotices = mutableMapOf<String, String?>()
    private var batterySkipped = false
    private var searchJob: Job? = null
    private var childTranscriptJob: Job? = null
    private var commandSeq = 0L
    private val activeSessionMutex = Mutex()

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
                update {
                    it.copy(
                        activeSessionId = id,
                        draft = drafts[id].orEmpty(),
                        draftAttachments = draftAttachments[id].orEmpty(),
                        attachmentNotice = attachmentNotices[id],
                    )
                }
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
        // Subagents are read on their own collector rather than inside the
        // transcript one: that block collects a stream that never completes, so
        // anything appended after it would never run (lane 971-UI-fleet).
        viewModelScope.launch {
            service.activeSessionId.collectLatest { id ->
                if (id == null) {
                    update {
                        it.copy(
                            fleet = it.fleet.copy(
                                active = FleetLoad.Unread,
                                subagents = SubagentLoad.Unread,
                            ),
                        )
                    }
                } else {
                    refreshFleet(id).join()
                }
            }
        }
        // The branch each session's next submit carries (lane 971-UI-extras).
        viewModelScope.launch {
            service.branchSelection.collect { selection ->
                update { it.copy(branchSelection = selection) }
            }
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
        // The family-aware capture: a parent and the sessions it spawned are
        // frozen as one run, in the order the drawer actually draws them
        // (SessionTree.captureOrder). A flat capture pinned the parent in the
        // section its own tier named and the family then jumped on first render.
        update { it.copy(orderSnapshot = SessionTree.captureOrder(it.sessions, it.activeSessionId)) }
        viewModelScope.launch { service.refreshRoster() }
    }

    /** The freeze lifts when the drawer closes, so the next open re-sorts. */
    fun onDrawerClosed() = update { it.copy(orderSnapshot = null) }

    fun loadMoreSessions() = viewModelScope.launch { service.loadMoreSessions() }

    fun newSession(model: String? = null, effort: String? = null) = viewModelScope.launch {
        withReadyRoster {
            val id = service.createSession(model, effort)
            loadTranscript(id)
        }
    }

    fun activate(sessionId: String) = viewModelScope.launch {
        withReadyRoster {
            service.activate(sessionId)
            service.markSeen(sessionId)
            loadTranscript(sessionId)
        }
    }

    /** A cold notification may arrive before the Binder connection's first roster. */
    private suspend fun withReadyRoster(startIfNeeded: Boolean = false, action: suspend () -> Unit) {
        try {
            // A queued cold navigation can own the selection lock while it
            // waits for Ready. An explicit Start-and-send must still start it.
            if (startIfNeeded && service.status.value !is DaemonStatus.Running) service.start()
            activeSessionMutex.withLock {
                combine(service.status, service.rosterReady) { status, ready ->
                    status is DaemonStatus.Running && ready
                }.first { it }
                action()
            }
        } catch (cancelled: kotlinx.coroutines.CancellationException) { throw cancelled }
        catch (_: Exception) { update { it.copy(transcriptNotice = "Session unavailable. Try again.") } }
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
        val attachments = draftAttachments[current.activeSessionId].orEmpty()
        if (text.isEmpty() && attachments.isEmpty()) return@launch
        withReadyRoster(startIfNeeded = true) {
            // A staged file stays with the session that owned it when Send was pressed.
            val id = current.activeSessionId?.takeIf { attachments.isNotEmpty() }
                ?: service.activeSessionId.value ?: service.createSession()
            try {
                service.send(id, text, attachments, mode)
            } catch (cancelled: kotlinx.coroutines.CancellationException) {
                throw cancelled
            } catch (refusal: Exception) {
                noteAttachment(id, (refusal as? TurnRefused)?.code ?: refusal.message)
                update { it.copy(deliveryChooser = false) }
                return@withReadyRoster
            }
            // Preserve text or attachments edited while startup or RPC was pending.
            val visible = _state.value.activeSessionId == id
            val latestDraft = if (visible) _state.value.draft else drafts[id].orEmpty()
            if (latestDraft == current.draft && draftAttachments[id].orEmpty() == attachments) {
                drafts[id] = ""
                draftAttachments.remove(id)
                attachmentNotices.remove(id)
                if (visible) {
                    update {
                        it.copy(draft = "", draftAttachments = emptyList(), attachmentNotice = null, deliveryChooser = false)
                    }
                }
            }
            if (visible) loadTranscript(id)
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
                        if (error is kotlinx.coroutines.CancellationException) throw error
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
                    if (error is kotlinx.coroutines.CancellationException) throw error
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
        // The session as it was when staging *began*. Staging is a round trip,
        // and the user can switch sessions during it; without this the block
        // lands wherever they ended up (verify-11 O10).
        val target = _state.value.activeSessionId ?: return@launch
        // The refusal's own code, not a guess: a null return means the facade
        // has no staging adapter, and an AttachmentRefused carries the reason
        // the daemon or the block kind gave (971-V F2).
        val staged = try { service.stageAttachment(bytes, mime, name) }
        catch (cancelled: kotlinx.coroutines.CancellationException) { throw cancelled }
        catch (refused: AttachmentRefused) { noteAttachment(target, refused.code); return@launch }
        catch (error: Exception) {
            noteAttachment(target, error.message?.takeIf { it.isNotBlank() } ?: AttachmentLimits.TOO_LARGE)
            return@launch
        }
        if (staged == null) {
            noteAttachment(target, AttachmentLimits.TOO_LARGE)
            return@launch
        }
        draftAttachments[target] = draftAttachments[target].orEmpty() + staged
        if (_state.value.activeSessionId == target) {
            update { it.copy(draftAttachments = draftAttachments.getValue(target)) }
        }
    }

    /**
     * A platform affordance the composer offered and this device does not have.
     *
     * The Activity reports it rather than crashing on the launch (971-V F10);
     * it rides the same notice line as a daemon refusal because to the person
     * holding the phone it is the same fact: this attachment did not happen.
     */
    fun noteAttachmentUnavailable(code: String) {
        _state.value.activeSessionId?.let { noteAttachment(it, code) }
            ?: update { it.copy(attachmentNotice = code) }
    }

    /** Records a refusal against the session it belongs to. */
    private fun noteAttachment(sessionId: String, code: String?) {
        attachmentNotices[sessionId] = code
        if (_state.value.activeSessionId == sessionId) {
            update { it.copy(attachmentNotice = code) }
        }
    }

    fun removeAttachment(artifact: String) {
        val sessionId = _state.value.activeSessionId ?: return
        draftAttachments[sessionId] = draftAttachments[sessionId].orEmpty()
            .filterNot { block -> block.artifact == artifact }
        update { it.copy(draftAttachments = draftAttachments.getValue(sessionId)) }
        // Removing one is the way out of a too-many refusal, so the notice goes
        // with it (verify-11 O9).
        noteAttachment(sessionId, null)
    }

    fun dismissAttachmentNotice() {
        _state.value.activeSessionId?.let { noteAttachment(it, null) }
    }

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
        if (mode in service.supportedPermissionModes) {
            runCatching { service.setPermissionMode(mode) }.onFailure(::selectionFailed)
        }
    }

    private fun selectionFailed(error: Throwable) {
        if (error is kotlinx.coroutines.CancellationException) throw error
        // An IOException from the transport already carries its own stable code
        // as the message ("connection_lost"); reducing every failure to
        // "selection_unavailable" hid which one happened.
        val code = (error as? ai.diffforge.haider.transport.rpc.RpcRemoteException)?.code
            ?: error.message?.takeIf { it.isNotBlank() }
            ?: "selection_unavailable"
        update { it.copy(catalogError = code) }
    }

    /**
     * Refresh is the button a person presses *because* something looks wrong,
     * so it is exactly the call that must not raise: a daemon restart made it
     * throw `connection_lost` out of an unguarded coroutine and killed the UI
     * process while the daemon was fine (971-V F4, `api35-refresh-crash.log`).
     *
     * The failure becomes the chip's error state and the facade re-reads what
     * it can, which is what "Retry" means from here.
     */
    fun refreshModels() = viewModelScope.launch {
        update { it.copy(catalogRequestedAtMs = clock()) }
        try { service.refreshModels() }
        catch (cancelled: kotlinx.coroutines.CancellationException) { throw cancelled }
        catch (error: Exception) {
            selectionFailed(error)
            refreshProviders()
        }
    }

    fun refreshProviders() = viewModelScope.launch {
        try { service.refreshProviders() }
        catch (cancelled: kotlinx.coroutines.CancellationException) { throw cancelled }
        catch (error: Exception) { selectionFailed(error) }
    }

    fun selectProvider(provider: String) = viewModelScope.launch {
        update { it.copy(selectionBusy = true, catalogRequestedAtMs = clock()) }
        runCatching { service.selectProvider(provider) }.onFailure(::selectionFailed)
        update { it.copy(selectionBusy = false) }
    }

    // ---------- subagents and the fleet (lane 971-UI-fleet) ----------

    /**
     * Reads the visible session's descendant tree and its observe chips.
     *
     * Both are separate doors on purpose: `session.observe` is how a chip knows
     * a subagent exists, and `session.fleet` is the only place a child's own
     * session id is published (`ObserveSubagentWire` carries none, frame.rs:2219).
     * A chip therefore cannot open a transcript until the snapshot has landed,
     * and the UI offers the panel instead of guessing a session.
     */
    fun refreshFleet(sessionId: String? = _state.value.activeSessionId) =
        viewModelScope.launch {
            val id = sessionId ?: return@launch
            update { it.copy(fleet = it.fleet.copy(active = FleetLoad.Loading)) }
            val load = runCatching { service.fleet(id) }
                .getOrElse { FleetLoad.Failed(it.message ?: "session_fleet_failed") }
            val observed = runCatching { service.subagents(id) }
                .getOrElse { SubagentLoad.Unavailable(it.message ?: "session_observe_failed") }
            update { current ->
                // A read that resolves after the user has moved on describes a
                // session that is no longer on screen; it is dropped, not shown.
                if (current.activeSessionId != id) current
                else current.copy(
                    fleet = current.fleet.copy(
                        active = load,
                        subagents = observed,
                        panel = current.fleet.panel + (id to load),
                    ),
                )
            }
        }

    /**
     * Fills the cross-session panel.
     *
     * Only sessions the roster reports as roots are read: a descendant's own
     * fleet is already inside its parent's snapshot, and re-reading it would
     * double-count the same agent at two different heads — which the wire
     * explicitly forbids (frame.rs:2318).
     */
    fun openFleet() = viewModelScope.launch {
        update { it.copy(overlay = Overlay.Fleet, fleet = it.fleet.copy(panelLoading = true)) }
        val roots = _state.value.sessions
            .filter { it.parentSessionId == null }
            .take(FLEET_PANEL_SESSION_LIMIT)
        val reads = roots.associate { row ->
            row.id to runCatching { service.fleet(row.id) }
                .getOrElse { FleetLoad.Failed(it.message ?: "session_fleet_failed") }
        }
        update { it.copy(fleet = it.fleet.copy(panel = it.fleet.panel + reads, panelLoading = false)) }
    }

    /** Folds or unfolds one family in the drawer. */
    fun toggleFamily(sessionId: String) = update { current ->
        current.copy(
            familyToggles = if (sessionId in current.familyToggles) {
                current.familyToggles - sessionId
            } else {
                current.familyToggles + sessionId
            },
        )
    }

    /**
     * Mounts one descendant's own replay, read-only.
     *
     * It is a separate subscription from the active session's, so opening a
     * child never folds two transcripts into one screen, and the parent's feed
     * keeps running underneath.
     */
    fun openChildTranscript(
        sessionId: String,
        agentId: String? = null,
        parentSessionId: String? = null,
    ) {
        childTranscriptJob?.cancel()
        update {
            it.copy(
                overlay = Overlay.ChildTranscript(sessionId, agentId, parentSessionId),
                childTranscript = ChildTranscriptState(
                    sessionId = sessionId,
                    agentId = agentId,
                    parentSessionId = parentSessionId,
                ),
            )
        }
        childTranscriptJob = viewModelScope.launch {
            service.transcriptUpdates(sessionId).collect { load ->
                update { current ->
                    val open = current.childTranscript ?: return@update current
                    if (open.sessionId != sessionId) return@update current
                    current.copy(
                        childTranscript = open.copy(
                            messages = load.messages,
                            loading = false,
                            notice = when (load) {
                                is TranscriptLoad.Complete -> null
                                is TranscriptLoad.Partial ->
                                    "History up to ${load.loadedThroughSeq} of " +
                                        "${load.headSeq} — ${load.reason}"
                                is TranscriptLoad.Unavailable -> load.reason
                            },
                        ),
                    )
                }
            }
        }
    }

    // ---------- checkpoints and branches ----------

    /**
     * Opens the sheet and reads the timeline from authority.
     *
     * The state is replaced, not merged: a page read for the session that was
     * open before must never be rendered under this one's title.
     */
    fun openCheckpoints(sessionId: String) {
        update {
            it.copy(
                overlay = Overlay.Checkpoints(sessionId),
                checkpoints = CheckpointsUiState(sessionId = sessionId, loading = true),
            )
        }
        loadCheckpoints(sessionId)
    }

    fun openBranches(sessionId: String) = openOverlay(Overlay.Branches(sessionId))

    /** Re-reads the first page. Every mutation ends here rather than editing rows. */
    fun loadCheckpoints(sessionId: String, cursor: Long? = null) = viewModelScope.launch {
        // Even the spinner belongs to the session it was asked for: a read for
        // a session the sheet has left must not make this one look busy.
        update {
            if (it.checkpoints.sessionId != sessionId) {
                it
            } else {
                it.copy(checkpoints = it.checkpoints.copy(loading = true))
            }
        }
        // The facade owns the selection, so it is read from there rather than
        // from the mirrored copy in state: the mirror is one collector hop
        // behind a switch, and `checkpoint.list` is branch-scoped.
        val branch = service.branchSelection.value[sessionId]
        val result = service.checkpoints(sessionId, branch, cursor)
        update { current ->
            // A page that came back for a session the sheet has since left is
            // dropped, not shown.
            if (current.checkpoints.sessionId != sessionId) return@update current
            current.copy(
                checkpoints = when (result) {
                    is CheckpointListResult.Page -> current.checkpoints.copy(
                        rows = if (cursor == null) {
                            result.page.checkpoints
                        } else {
                            Checkpoints.merge(current.checkpoints.rows.orEmpty(), result.page.checkpoints)
                        },
                        nextCursor = result.page.nextCursor,
                        cursorState = result.page.cursorState,
                        loading = false,
                        unavailable = null,
                        error = null,
                        // A successful re-read is how a conflict is dismissed,
                        // after the person has looked at the moved workspace.
                        conflict = null,
                        rollbackConflict = null,
                        branchMismatch = null,
                    )
                    is CheckpointListResult.Unavailable -> current.checkpoints.copy(
                        loading = false,
                        unavailable = result.reason,
                    )
                    is CheckpointListResult.Failed -> current.checkpoints.copy(
                        loading = false,
                        error = result.code,
                    )
                },
            )
        }
    }

    fun loadMoreCheckpoints() {
        val state = _state.value.checkpoints
        val sessionId = state.sessionId ?: return
        if (state.loading || state.busy) return
        if (state.cursorState != CheckpointCursorState.More) return
        loadCheckpoints(sessionId, state.nextCursor ?: return)
    }

    /** Nothing destructive happens on one tap: the gesture waits for a confirm. */
    fun confirmCheckpointGesture(gesture: CheckpointGesture?) =
        update { it.copy(checkpoints = it.checkpoints.copy(confirming = gesture)) }

    /** Runs the confirmed gesture, then re-reads the timeline from authority. */
    fun applyCheckpointGesture() = viewModelScope.launch {
        val state = _state.value.checkpoints
        val sessionId = state.sessionId ?: return@launch
        val gesture = state.confirming ?: return@launch
        val branch = service.branchSelection.value[sessionId]
        update {
            it.copy(
                checkpoints = it.checkpoints.clearedOutcome().copy(
                    pending = gesture,
                    confirming = null,
                ),
            )
        }
        val outcome = when (gesture) {
            is CheckpointGesture.Undo -> service.undoCheckpoint(sessionId, gesture.target, branch)
            is CheckpointGesture.Redo -> service.redoCheckpoint(sessionId, gesture.target, branch)
            is CheckpointGesture.Rollback -> service.rollbackTurn(sessionId, gesture.runId, branch)
        }
        var committed = false
        update { current ->
            if (current.checkpoints.sessionId != sessionId) return@update current
            val next = when (outcome) {
                is CheckpointOutcome.Committed -> {
                    committed = true
                    current.checkpoints.copy(receipt = outcome.receipt)
                }
                is CheckpointOutcome.Conflict -> current.checkpoints.copy(conflict = outcome.conflict)
                is CheckpointOutcome.RollbackConflict ->
                    current.checkpoints.copy(rollbackConflict = outcome.conflict)
                is CheckpointOutcome.BranchMismatch ->
                    current.checkpoints.copy(branchMismatch = outcome.mismatch)
                is CheckpointOutcome.Unavailable ->
                    current.checkpoints.copy(unavailable = outcome.reason)
                is CheckpointOutcome.Failed -> current.checkpoints.copy(error = outcome.code)
            }
            current.copy(checkpoints = next.copy(pending = null))
        }
        // Only a committed receipt licenses a re-read. A conflict is terminal
        // for the gesture, and re-reading here would clear the very notice the
        // person has not seen yet.
        if (committed) loadCheckpoints(sessionId)
    }

    /**
     * Chooses the branch this session's next turn is submitted on.
     *
     * There is no `branch.switch` on the wire: nothing already committed moves.
     * The timeline is re-read because `checkpoint.list` is branch-scoped.
     */
    fun selectBranch(sessionId: String, branchId: String?) = viewModelScope.launch {
        service.selectBranch(sessionId, branchId)
        if (_state.value.checkpoints.sessionId == sessionId) {
            update { it.copy(checkpoints = it.checkpoints.copy(rows = null, loading = true)) }
            loadCheckpoints(sessionId)
        }
    }

    /**
     * `branch.create` at the row's own published head node.
     *
     * A row with no head node has nothing to fork from, and half a coordinate
     * is not sent: the sheet says so instead.
     */
    fun createBranch(sessionId: String, name: String) = viewModelScope.launch {
        val row = session(sessionId) ?: return@launch
        val source = service.branchSelection.value[sessionId]
        // The fork point and the source branch have to come from the SAME
        // fact. Round 12 sent the selected branch as the source while forking
        // at main's head, so choosing Plan B produced a branch that claimed
        // Plan B as its parent and started from main (verify-11 O11).
        val forkPoint = if (source == null) {
            // The implicit main branch: the row's own published head.
            row.mainHeadNodeId?.let { it to row.mainHeadSeq }
        } else {
            row.branches.firstOrNull { it.branchId == source }
                ?.let { branch ->
                    val node = branch.headNodeId
                    val seq = branch.headSeq
                    if (node != null && seq != null) node to seq else null
                }
        }
        if (forkPoint == null) {
            // Half a tuple is not sent: `branch.create` needs an exact
            // (fork_node_id, fork_seq) pair from one published head.
            update { it.copy(checkpoints = it.checkpoints.copy(branchNotice = NO_FORK_POINT)) }
            return@launch
        }
        val outcome = service.createBranch(
            sessionId = sessionId,
            forkNodeId = forkPoint.first,
            forkSeq = forkPoint.second,
            name = name.trim().ifBlank { null },
            sourceBranchId = source,
        )
        when (outcome) {
            is BranchOutcome.Created -> {
                update { it.copy(checkpoints = it.checkpoints.copy(branchNotice = null)) }
                // Created is not selected: the next turn moves only because
                // somebody chose it, which is the same rule the refusal path has.
                service.refreshRoster()
            }
            is BranchOutcome.Unavailable ->
                update { it.copy(checkpoints = it.checkpoints.copy(branchNotice = outcome.reason)) }
            is BranchOutcome.Failed ->
                update { it.copy(checkpoints = it.checkpoints.copy(branchNotice = outcome.code)) }
        }
    }

    // ---------- surfaces ----------

    fun openOverlay(overlay: Overlay) = update { it.copy(overlay = overlay) }

    fun closeOverlay() {
        // A descendant's replay is its own subscription; leaving it collecting
        // behind a closed screen would keep folding a session nobody is reading.
        childTranscriptJob?.cancel()
        childTranscriptJob = null
        update { it.copy(overlay = Overlay.None, childTranscript = null) }
    }

    /**
     * System Back on a full screen, which must land where its own back control
     * lands.
     *
     * Looms and Accounts are reached *through* Settings, and Back dropped
     * straight to the session surface instead of returning to it, so neither
     * the tapped control nor hardware Back went "back" (971-V F8).
     */
    fun back() {
        val parent = OverlayNavigation.parent(_state.value.overlay)
        if (parent == Overlay.None) closeOverlay() else openOverlay(parent)
    }

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
        activeSessionMutex.withLock {
            if (service.status.value !is DaemonStatus.Running) return@withLock
            service.rosterReady.first { it }
            if (service.status.value !is DaemonStatus.Running || service.activeSessionId.value != null) return@withLock
            try {
                val candidate = SessionListState.order(service.sessions.value, null).firstOrNull()
                val id = candidate?.id ?: service.createSession()
                if (service.activeSessionId.value != id) service.activate(id)
                service.markSeen(id)
            } catch (cancelled: kotlinx.coroutines.CancellationException) { throw cancelled }
            catch (_: Exception) { update { it.copy(transcriptNotice = "Session unavailable. Try again.") } }
        }
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
            applyTranscript(sessionId, load)
        }
    }

    private fun applyTranscript(sessionId: String, load: TranscriptLoad) {
        transcripts[sessionId] = load.messages
        val notice = when (load) {
            is TranscriptLoad.Complete -> null
            is TranscriptLoad.Partial -> "History up to ${load.loadedThroughSeq} of ${load.headSeq} — ${load.reason}"
            is TranscriptLoad.Unavailable -> load.reason
        }
        update {
            if (it.activeSessionId == sessionId) it.copy(messages = load.messages,
                transcriptLoading = false, transcriptNotice = notice) else it
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

        /**
         * How many root sessions the fleet panel reads in one open.
         *
         * A roster of hundreds would otherwise fire hundreds of `session.fleet`
         * calls the moment a sheet opens; the panel says how many it covered
         * rather than implying it read them all.
         */
        const val FLEET_PANEL_SESSION_LIMIT = 24

        /**
         * Stated when a branch was asked for at a session with no published
         * head node. `branch.create` needs an exact `(fork_node_id, fork_seq)`
         * pair from one fact, so half of it is not sent.
         */
        const val NO_FORK_POINT = "branch_fork_point_unpublished"

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
