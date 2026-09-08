package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.ui.accounts.AccountsRpcAdapter
import ai.diffforge.haider.ui.checkpoints.BranchOutcome
import ai.diffforge.haider.ui.checkpoints.BranchView
import ai.diffforge.haider.ui.checkpoints.CHECKPOINT_KINDS
import ai.diffforge.haider.ui.checkpoints.CHECKPOINT_ORIGINS
import ai.diffforge.haider.ui.checkpoints.CheckpointCategory
import ai.diffforge.haider.ui.checkpoints.CheckpointCursorState
import ai.diffforge.haider.ui.checkpoints.CheckpointListResult
import ai.diffforge.haider.ui.checkpoints.CheckpointOutcome
import ai.diffforge.haider.ui.checkpoints.CheckpointPage
import ai.diffforge.haider.ui.checkpoints.CheckpointPathView
import ai.diffforge.haider.ui.checkpoints.CheckpointReceiptView
import ai.diffforge.haider.ui.checkpoints.CheckpointUnavailable
import ai.diffforge.haider.ui.checkpoints.CheckpointView
import ai.diffforge.haider.ui.checkpoints.Checkpoints
import ai.diffforge.haider.ui.checkpoints.CheckpointRpcAdapter
import ai.diffforge.haider.transport.SessionModel
import ai.diffforge.haider.transport.SessionProvider
import ai.diffforge.haider.transport.SessionSelection
import ai.diffforge.haider.ui.chat.Message
import ai.diffforge.haider.ui.chat.Role
import ai.diffforge.haider.ui.chat.ToolCall
import ai.diffforge.haider.ui.chat.ToolStatus
import ai.diffforge.haider.ui.state.CapabilityApproval
import ai.diffforge.haider.ui.state.PermissionMode
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.yield
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Scripted daemon for previews, screenshots and tests. Every row of the
 * UI-SPEC 3.8 state matrix is reachable through [FakeScenario], and the roster
 * is large enough (see [FakeScenario.LargeRoster]) to exercise the drawer's
 * paging and scrolling honestly.
 *
 * This is `src/main` on purpose: `@Preview` needs it, and lane 971-3 swaps it
 * for `TransportDaemonService` behind the same interface without touching a
 * single composable.
 */
enum class FakeScenario {
    FirstRun,
    DaemonStarting,
    DaemonStopped,
    DaemonFailed,
    NotificationsDenied,
    NotificationsPermanentlyDenied,
    NoNetwork,
    TurnRunning,
    InputRequiredHere,
    InputRequiredElsewhere,
    ErroredTurn,
    EmptyRosterReady,
    EmptyRosterStopped,
    Populated,
    LargeRoster,
}

class FakeDaemonService(
    scenario: FakeScenario = FakeScenario.Populated,
    private val nowMs: Long = FIXED_NOW,
) : DaemonService {

    private val _status = MutableStateFlow<DaemonStatus>(DaemonStatus.Stopped)
    private val _environment = MutableStateFlow(DaemonEnvironment())
    private val _sessions = MutableStateFlow<List<SessionRow>>(emptyList())
    private val _paging = MutableStateFlow(RosterPaging())
    private val _activeSessionId = MutableStateFlow<String?>(null)
    private val _models = MutableStateFlow<SessionConfig?>(null)
    private val _catalogError = MutableStateFlow<String?>(null)
    private val _catalogRequestedAtMs = MutableStateFlow<Long?>(null)
    private val _searchIndex = MutableStateFlow(SearchIndexState())
    private val _providers = MutableStateFlow(ProviderInventory())
    private val _shell = MutableStateFlow(
        // What `tools.inventory` reports on an android-standalone daemon:
        // ProcessExec is neither advertised nor dispatchable (C4).
        ShellAvailability(available = false, reason = "process_exec_disabled"),
    )

    override val status: StateFlow<DaemonStatus> = _status.asStateFlow()
    override val environment: StateFlow<DaemonEnvironment> = _environment.asStateFlow()
    override val sessions: StateFlow<List<SessionRow>> = _sessions.asStateFlow()
    override val paging: StateFlow<RosterPaging> = _paging.asStateFlow()
    override val activeSessionId: StateFlow<String?> = _activeSessionId.asStateFlow()
    override val models: StateFlow<SessionConfig?> = _models.asStateFlow()
    override val catalogError: StateFlow<String?> = _catalogError.asStateFlow()
    override val catalogRequestedAtMs: StateFlow<Long?> = _catalogRequestedAtMs.asStateFlow()
    override val searchIndex: StateFlow<SearchIndexState> = _searchIndex.asStateFlow()
    override val providers: StateFlow<ProviderInventory> = _providers.asStateFlow()
    override val shell: StateFlow<ShellAvailability> = _shell.asStateFlow()

    private val transcripts = mutableMapOf<String, MutableList<Message>>()
    private var hiddenPages: List<List<SessionRow>> = emptyList()
    private val readCache = mutableMapOf<String, TranscriptLoad>()
    private var nextMessageId = 1L

    /** Recorded calls, so a test can assert what the UI asked the daemon for. */
    val calls = mutableListOf<String>()

    /**
     * Auto by default, as the first-run step leaves it.
     *
     * The fake stays honest about what Auto *means*: it is the daemon that
     * stops sending capability approvals, so in Auto this fake does not
     * fabricate a `permission` needs-input for a device capability either. It
     * still raises questions, secrets and provider refusals.
     */
    private val _permissionMode = MutableStateFlow(PermissionMode.Auto)
    override val permissionMode: StateFlow<PermissionMode> = _permissionMode.asStateFlow()

    override suspend fun setPermissionMode(mode: PermissionMode) {
        calls += "tool.policy:${mode.name.lowercase()}"
        _permissionMode.value = mode
        applyAutoPolicy()
    }

    /**
     * What Auto actually *does*: the policy resolves device-capability
     * approvals instead of raising them.
     *
     * Round 9 modelled Auto as a UI filter, so switching to Auto with an
     * unanswered `sms.list` card hid the card and left the session parked on
     * "Needs you" with nothing to answer (verify-8 O1). A mode that changes
     * what the daemon asks has to change the snapshot, not the drawing.
     */
    private fun applyAutoPolicy() {
        if (_permissionMode.value != PermissionMode.Auto) return
        val resolved = mutableListOf<String>()
        _sessions.value = _sessions.value.map { row ->
            val pending = row.needsInput
            if (pending != null && CapabilityApproval.isDeviceCapabilityApproval(pending)) {
                calls += "tool.policy.auto_resolved:${row.id}"
                resolved += row.id
                row.copy(
                    needsInput = null,
                    state = SessionVisualState.Running,
                    runState = "running",
                )
            } else {
                row
            }
        }
        // Running is not a resting state. Round 10 cleared the card, set
        // Running and stopped there, so an Auto session sat "running" forever
        // with nothing to show for it (verify-9 V2). The allowed call runs and
        // the turn reaches a terminal snapshot, which is what the device check
        // "Auto SMS-read turn completes with no card" observes.
        resolved.forEach(::completeAutoTurn)
    }

    /**
     * Finish the turn the policy just allowed: the tool result lands in the
     * transcript, the assistant's line stops streaming, and the row goes Idle
     * with no run id.
     */
    private fun completeAutoTurn(sessionId: String) {
        transcripts.getOrPut(sessionId) { mutableListOf() }.let { messages ->
            val index = messages.indexOfLast { it.streaming }
            if (index >= 0) {
                messages[index] = messages[index].copy(
                    streaming = false,
                    tools = messages[index].tools.map { tool ->
                        if (tool.status == ToolStatus.Running) {
                            tool.copy(status = ToolStatus.Completed, durationMs = 1_200L)
                        } else {
                            tool
                        }
                    },
                )
            } else {
                messages += Message(
                    id = nextMessageId++,
                    role = Role.Agent,
                    text = "Read your recent texts.",
                    provider = "anthropic",
                    tools = listOf(
                        ToolCall(
                            callId = "call-auto-$sessionId",
                            name = "sms",
                            summary = "sms.list",
                            status = ToolStatus.Completed,
                            result = null,
                            durationMs = 1_200L,
                        ),
                    ),
                )
            }
        }
        _sessions.value = _sessions.value.map { row ->
            if (row.id == sessionId) {
                calls += "tool.policy.auto_completed:$sessionId"
                row.copy(
                    runId = null,
                    state = SessionVisualState.Idle,
                    runState = "idle",
                    needsInput = null,
                )
            } else {
                row
            }
        }
    }

    /** Sets the mode without asking the policy to run — test setup only. */
    fun setPermissionModeForTest(mode: PermissionMode) {
        _permissionMode.value = mode
    }

    /**
     * Raises the device-capability approval a real daemon raises in Ask mode,
     * so a test can watch what happens when the mode changes underneath it.
     */
    fun raiseDeviceApproval(sessionId: String) {
        _sessions.value = _sessions.value.map { row ->
            if (row.id != sessionId) {
                row
            } else {
                row.copy(
                    state = SessionVisualState.NeedsInput,
                    runState = "parked_permission",
                    needsInput = NeedsInput(
                        kind = "permission",
                        title = "Allow sms.list for the last 20 messages?",
                        safeBody = listOf("The agent asked to read your recent texts."),
                        menuId = "menu-sms-list",
                        requestSeq = 91,
                        workerGeneration = 3,
                        sinceMs = nowMs - 4_000,
                    ),
                )
            }
        }
        applyAutoPolicy()
    }

    init {
        apply(scenario)
    }

    fun apply(scenario: FakeScenario) {
        _catalogError.value = null
        _catalogRequestedAtMs.value = nowMs
        _models.value = catalog()
        _providers.value = inventory()
        _environment.value = DaemonEnvironment()
        hiddenPages = emptyList()
        readCache.clear()
        _paging.value = RosterPaging()
        when (scenario) {
            FakeScenario.FirstRun -> {
                _status.value = DaemonStatus.Stopped
                _models.value = null
                _catalogRequestedAtMs.value = null
                _environment.value = DaemonEnvironment(notificationsGranted = false)
                setSessions(emptyList())
            }
            FakeScenario.DaemonStarting -> {
                _status.value = DaemonStatus.Starting
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
            }
            FakeScenario.DaemonStopped -> {
                _status.value = DaemonStatus.Stopped
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
            }
            FakeScenario.DaemonFailed -> {
                _status.value = DaemonStatus.Failed("store_recovery_failed", "STORE_RECOVERY_FAILED")
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
            }
            // These two isolate their banner rank: a session asking for a human
            // is rank 3 and would outrank them, which is correct behaviour but
            // makes the scenario useless for exercising rank 4 or 6.
            FakeScenario.NotificationsDenied -> {
                _status.value = running()
                _environment.value = DaemonEnvironment(notificationsGranted = false)
                setSessions(populatedRoster().filter { it.needsInput == null })
                _activeSessionId.value = "s-nav"
            }
            // Distinct from a first refusal: the system dialog will not appear
            // again, so the action has to deep-link into app settings.
            FakeScenario.NotificationsPermanentlyDenied -> {
                _status.value = running()
                _environment.value = DaemonEnvironment(
                    notificationsGranted = false,
                    notificationsPermanentlyDenied = true,
                )
                setSessions(populatedRoster().filter { it.needsInput == null })
                _activeSessionId.value = "s-nav"
            }
            FakeScenario.NoNetwork -> {
                _status.value = running()
                _environment.value = DaemonEnvironment(network = NetworkState.Unavailable)
                setSessions(
                    populatedRoster()
                        .filter { it.needsInput == null }
                        .map { it.copy(runId = null, state = SessionVisualState.Idle, runState = "idle") },
                )
                _activeSessionId.value = "s-nav"
            }
            FakeScenario.TurnRunning -> {
                _status.value = running()
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
                transcripts["s-nav"] = runningTranscript()
            }
            FakeScenario.InputRequiredHere -> {
                _status.value = running()
                setSessions(populatedRoster().map { row ->
                    if (row.id == "s-sms") row else row
                })
                _activeSessionId.value = "s-sms"
                transcripts["s-sms"] = askTranscript()
            }
            FakeScenario.InputRequiredElsewhere -> {
                _status.value = running()
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
                transcripts["s-nav"] = runningTranscript()
            }
            FakeScenario.ErroredTurn -> {
                _status.value = running()
                setSessions(populatedRoster())
                _activeSessionId.value = "s-broken"
                transcripts["s-broken"] = erroredTranscript()
            }
            // Nothing to open into, because the daemon is not running: the
            // drawer's empty state still has to be reachable.
            FakeScenario.EmptyRosterStopped -> {
                _status.value = DaemonStatus.Stopped
                setSessions(emptyList())
                _activeSessionId.value = null
            }
            FakeScenario.EmptyRosterReady -> {
                _status.value = running()
                setSessions(emptyList())
                _activeSessionId.value = null
            }
            FakeScenario.Populated -> {
                _status.value = running()
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
                transcripts["s-nav"] = runningTranscript()
            }
            FakeScenario.LargeRoster -> {
                _status.value = running()
                val all = largeRoster(240)
                setSessions(all.take(60))
                hiddenPages = all.drop(60).chunked(60)
                _paging.value = RosterPaging(hasMore = hiddenPages.isNotEmpty(), cursor = "p1")
                _activeSessionId.value = all.first().id
            }
        }
    }

    fun setSessions(rows: List<SessionRow>) {
        _sessions.value = rows
        // Every path that publishes a roster runs the policy, so an approval
        // can never sit unresolved in Auto (verify-8 O1).
        applyAutoPolicy()
        _searchIndex.value = SearchIndexState(
            indexedSessions = rows.size,
            totalSessions = rows.size,
            complete = true,
        )
    }

    /** Drives the honest "still indexing" path the drawer has to render. */
    fun setSearchIndex(state: SearchIndexState) {
        _searchIndex.value = state
    }

    /** Makes the next replay report partial or unavailable history. */
    var transcriptOverride: ((String) -> TranscriptLoad?)? = null

    /**
     * The daemon has no catalog to give. A refresh then resolves to an error
     * rather than leaving the request in flight forever, which is what the
     * first-run picker has to render as a retry.
     */
    var catalogUnavailable: Boolean = false

    fun setStatus(status: DaemonStatus) {
        _status.value = status
    }

    fun setEnvironment(environment: DaemonEnvironment) {
        _environment.value = environment
    }

    fun setCatalogError(message: String?) {
        _catalogError.value = message
    }

    fun setModels(config: SessionConfig?) {
        _models.value = config
    }

    fun setCatalogRequestedAt(atMs: Long?) {
        _catalogRequestedAtMs.value = atMs
    }

    fun setProviders(inventory: ProviderInventory) {
        _providers.value = inventory
    }

    /** So a later lane's on-device shell can be exercised before it exists. */
    fun setShell(availability: ShellAvailability) {
        _shell.value = availability
    }

    override suspend fun start() {
        calls += "start"
        _status.value = DaemonStatus.Starting
        _status.value = running()
    }

    override suspend fun stop() {
        calls += "stop"
        _status.value = DaemonStatus.Stopped
    }

    override suspend fun restart() {
        calls += "restart"
        _status.value = DaemonStatus.Restarting
        _status.value = running()
    }

    override suspend fun reportNotificationPermission(
        granted: Boolean,
        permanentlyDenied: Boolean,
    ) {
        calls += "permissions.notifications:$granted"
        _environment.value = _environment.value.copy(
            notificationsGranted = granted,
            notificationsPermanentlyDenied = !granted && permanentlyDenied,
        )
    }

    override suspend fun refreshRoster() {
        calls += "refreshRoster"
    }

    override suspend fun loadMoreSessions() {
        if (hiddenPages.isEmpty()) {
            _paging.value = _paging.value.copy(hasMore = false, loading = false)
            return
        }
        calls += "loadMoreSessions"
        _paging.value = _paging.value.copy(loading = true)
        val page = hiddenPages.first()
        hiddenPages = hiddenPages.drop(1)
        _sessions.value = _sessions.value + page
        _paging.value = RosterPaging(loading = false, hasMore = hiddenPages.isNotEmpty(), cursor = "p")
    }

    override suspend fun createSession(model: String?, effort: String?): String {
        calls += "createSession"
        val id = "s-new-${_sessions.value.size + 1}"
        val row = SessionRow(
            id = id,
            title = null,
            state = SessionVisualState.Idle,
            runState = "idle",
            provider = model?.let { "anthropic" } ?: "anthropic",
            model = model ?: "claude-sonnet-4-5",
            effort = effort ?: "high",
            lastActivityMs = nowMs,
            seenAtMs = nowMs,
            createdAtMs = nowMs,
        )
        // A real create is a round trip: the id is not visible until it comes
        // back. Publishing it synchronously hid the window in which a second
        // caller sees no active session and creates another one.
        yield()
        _sessions.value = listOf(row) + _sessions.value
        // A new session has no history: it must not inherit a default fixture,
        // or "open and chat" lands on somebody else's transcript.
        transcripts[id] = mutableListOf()
        _activeSessionId.value = id
        return id
    }

    override suspend fun activate(sessionId: String) {
        calls += "activate:$sessionId"
        _activeSessionId.value = sessionId
    }

    override suspend fun markSeen(sessionId: String) {
        calls += "markSeen:$sessionId"
        _sessions.value = _sessions.value.map {
            if (it.id == sessionId) it.copy(seenAtMs = maxOf(nowMs, it.lastActivityMs ?: nowMs)) else it
        }
    }

    override suspend fun rename(sessionId: String, title: String) {
        calls += "rename:$sessionId"
        _sessions.value = _sessions.value.map {
            if (it.id == sessionId) it.copy(title = title) else it
        }
    }

    override suspend fun fork(sessionId: String): String {
        calls += "fork:$sessionId"
        val source = _sessions.value.first { it.id == sessionId }
        val id = "$sessionId-fork"
        _sessions.value = listOf(
            source.copy(
                id = id,
                forkedFrom = ForkProvenance(sessionId, source.headSeq),
                runId = null,
                state = SessionVisualState.Idle,
                runState = "idle",
                needsInput = null,
            ),
        ) + _sessions.value
        return id
    }

    override suspend fun stopTurn(sessionId: String) {
        val row = _sessions.value.firstOrNull { it.id == sessionId }
        val coordinates = TurnCancel.coordinates(row) ?: throw MissingRunCoordinates(sessionId)
        calls += "turn.cancel:${coordinates.sessionId}:${coordinates.runId}:${coordinates.workerGeneration}"
        _sessions.value = _sessions.value.map {
            if (it.id == sessionId) {
                it.copy(runId = null, state = SessionVisualState.Idle, runState = "cancelled")
            } else {
                it
            }
        }
        transcripts[sessionId]?.let { messages ->
            val index = messages.indexOfLast { it.streaming }
            if (index >= 0) messages[index] = messages[index].copy(streaming = false)
        }
    }

    override suspend fun answer(
        coordinates: MenuCoordinates,
        optionKey: String,
        optionIndex: Int,
        input: MenuAnswerInput?,
    ) {
        // The whole compare-and-set identity is recorded, so a test can prove
        // the coordinates came from the snapshot that rendered the card. A
        // secret is recorded as its reference; the plaintext is never here.
        val inputTag = when (input) {
            null -> "none"
            is MenuAnswerInput.Text -> "text"
            is MenuAnswerInput.Secret -> "secret:${input.vaultReference}"
        }
        calls += "menu.answer:${coordinates.sessionId}:${coordinates.menuId}:" +
            "${coordinates.requestSeq}:${coordinates.workerGeneration}:" +
            "$optionKey:$optionIndex:$inputTag"
        _sessions.value = _sessions.value.map {
            if (it.id == coordinates.sessionId) {
                it.copy(needsInput = null, state = SessionVisualState.Running, runState = "running")
            } else {
                it
            }
        }
    }

    override suspend fun selectModel(provider: String, model: String, confirmNewEpoch: Boolean) {
        if (confirmNewEpoch) confirmedSelections++
        nextSelectionFailure?.let { nextSelectionFailure = null; throw IllegalStateException(it) }
        calls += "selectModel:$provider/$model"
        val current = _models.value ?: return
        // The daemon re-derives effort when the model changes: an effort the
        // new model does not support cannot survive the switch.
        val inventory = _providers.value
        val effort = current.current.effort
        val resolved = if (inventory.rejectsEffort(provider, model, effort)) {
            inventory.model(provider, model)?.defaultEffort
        } else {
            effort
        }
        _models.value = current.copy(
            current = current.current.copy(provider = provider, model = model, effort = resolved),
        )
    }

    override suspend fun selectEffort(effort: String?, confirmNewEpoch: Boolean) {
        if (confirmNewEpoch) confirmedSelections++
        nextSelectionFailure?.let { nextSelectionFailure = null; throw IllegalStateException(it) }
        val current = _models.value ?: return
        // The daemon refuses an unsupported effort; so does the fake, or the
        // UI would look correct against a catalog that would reject it.
        if (_providers.value.rejectsEffort(current.current.provider, current.current.model, effort)) {
            calls += "selectEffort:rejected:$effort"
            _catalogError.value = "unsupported_effort"
            return
        }
        calls += "selectEffort:$effort"
        _models.value = current.copy(current = current.current.copy(effort = effort))
    }

    override suspend fun refreshModels() {
        calls += "refreshModels"
        _catalogRequestedAtMs.value = nowMs
        if (catalogUnavailable) {
            _models.value = null
            _catalogError.value = "catalog_unavailable"
            return
        }
        _catalogError.value = null
        _models.value = catalog()
    }

    override suspend fun refreshProviders() {
        calls += AccountsRpcAdapter.METHOD_PROVIDER_LIST
        _providers.value = if (catalogUnavailable) ProviderInventory() else inventory()
    }

    override suspend fun selectProvider(provider: String) {
        calls += "selectProvider:$provider"
        val option = _providers.value.provider(provider) ?: return
        val model = option.defaultModel ?: option.modelIds.firstOrNull() ?: return
        selectModel(provider, model)
    }

    override suspend fun stageMenuSecret(secret: CharArray): String {
        // Method name only: an argument here could be the secret itself.
        calls += AccountsRpcAdapter.METHOD_VAULT_STAGE
        return "vaultref-menu-${secret.size}"
    }

    override suspend fun send(sessionId: String, text: String) {
        // `turn.submit` carries `branch_id` when one is chosen and OMITS it for
        // the implicit main branch (frame.rs:3697; wire transcript entry 82).
        // The composer never passes a branch: the facade holds the selection,
        // so choosing a branch in the sheet threads through every later send
        // without the send path knowing branches exist.
        val branch = _branchSelection.value[sessionId]
        calls += if (branch == null) "chat.send:$sessionId" else "chat.send:$sessionId:branch=$branch"
        val messages = transcripts.getOrPut(sessionId) { mutableListOf() }
        messages += Message(nextMessageId++, Role.User, text)
        messages += Message(
            id = nextMessageId++,
            role = Role.Agent,
            text = "",
            streaming = true,
            provider = _models.value?.current?.provider,
        )
        _sessions.value = _sessions.value.map {
            if (it.id == sessionId) {
                it.copy(runId = "run-${it.id}", state = SessionVisualState.Running, runState = "running")
            } else {
                it
            }
        }
    }

    /**
     * True only because this fake installs an authoritative fixture and knows
     * it. A real facade has to hydrate first (lane 971-3 handoff).
     */
    private val _rosterReady = MutableStateFlow(true)
    override val rosterReady: StateFlow<Boolean> = _rosterReady.asStateFlow()

    /** Set to make the next selection refuse, the way the daemon can. */
    private var nextSelectionFailure: String? = null

    /** How many selections carried the user's explicit confirmation. */
    var confirmedSelections = 0
        private set

    fun failNextSelection(code: String) {
        nextSelectionFailure = code
    }

    /** Overridable so a live scenario can push folded updates into a test. */
    var transcriptStream: ((String) -> Flow<TranscriptLoad>)? = null

    override fun transcriptUpdates(sessionId: String): Flow<TranscriptLoad> =
        transcriptStream?.invoke(sessionId) ?: flow { emit(transcript(sessionId)) }

    override suspend fun transcript(sessionId: String): TranscriptLoad {
        calls += "session.attach:$sessionId"
        transcriptOverride?.invoke(sessionId)?.let { return it }
        val messages = transcripts.getOrPut(sessionId) { defaultTranscript(sessionId) }.toList()
        return TranscriptLoad.Complete(messages)
    }

    /**
     * Searches titles, metadata **and indexed transcript content**, reading
     * each session through [TranscriptPager] ranges so the 1,024-envelope cap
     * is respected. Coverage is reported honestly: a session whose replay came
     * back Partial or Unavailable is counted as not indexed, and the outcome
     * is then incomplete.
     */
    override suspend fun search(query: String): SearchOutcome {
        val needle = query.trim().lowercase()
        if (needle.isEmpty()) {
            return SearchOutcome(emptyList(), _searchIndex.value, complete = _searchIndex.value.complete)
        }
        // Follow `next_cursor` through *all* pages before answering. Searching
        // only what happens to be loaded reported "60 of 60 … still indexing"
        // while 180 rows sat unread, and found nothing on the last page.
        var guard = 0
        while (_paging.value.hasMore && guard++ < MAX_SEARCH_PAGES) {
            loadMoreSessions()
        }
        val rows = _sessions.value
        val hits = mutableListOf<SearchHit>()
        var indexed = 0
        rows.forEach { row ->
            val metadataHit = listOfNotNull(row.title, row.model, row.provider, row.agentType, row.id)
                .any { it.lowercase().contains(needle) }
            val load = readThroughCache(row)
            if (load is TranscriptLoad.Complete) indexed += 1
            val bodyHit = load.messages.firstOrNull { message ->
                message.text.lowercase().contains(needle)
            }
            when {
                bodyHit != null -> hits += SearchHit(row.id, bodyHit.text.take(120), row.headSeq)
                metadataHit -> hits += SearchHit(row.id, row.title, row.headSeq)
            }
        }
        val index = SearchIndexState(
            indexedSessions = indexed,
            totalSessions = rows.size,
            complete = indexed == rows.size && !_paging.value.hasMore,
        )
        _searchIndex.value = index
        return SearchOutcome(hits = hits, index = index, complete = index.complete)
    }

    /** The paged read cache: one `session.read` call per bounded range. */
    private suspend fun readThroughCache(row: SessionRow): TranscriptLoad {
        readCache[row.id]?.let { return it }
        val ranges = TranscriptPager.ranges(row.headSeq.coerceAtLeast(1))
        ranges.forEach { range -> calls += "session.read:${row.id}:${range.startSeq}-${range.endSeq}" }
        val load = transcript(row.id)
        readCache[row.id] = load
        return load
    }

    // ---------- fixtures ----------

    private fun running() = DaemonStatus.Running(
        DaemonInfo(
            version = "0.0.971",
            generation = 4L,
            pid = 4711,
            socketPath = "/data/user/0/ai.diffforge.haider/files/haider/runtime/android-default/h.sock",
            ready = true,
            sessionCount = 5,
            waitingForRouteCount = 0,
            profilePath = "/data/user/0/ai.diffforge.haider/files/haider/profiles/default",
            runtimeDir = "/data/user/0/ai.diffforge.haider/files/haider/runtime/android-default",
            startedAtElapsedRealtimeMs = FIXED_UPTIME - 4 * 60 * 60 * 1000L - 12 * 60 * 1000L,
            pssBytes = 58L * 1024 * 1024,
        ),
    )

    private fun inventory(): ProviderInventory = ProviderInventory(
        revision = 7,
        providers = listOf(
            ProviderOption(
                id = "anthropic",
                label = "Anthropic",
                models = listOf(
                    ModelOption("claude-sonnet-4-5", listOf("low", "medium", "high"), "high", 200_000),
                    // Opus does not offer `low`; the picker must not either.
                    ModelOption("claude-opus-4-1", listOf("medium", "high"), "high", 200_000),
                ),
                defaultModel = "claude-sonnet-4-5",
                available = true,
                unavailableReason = null,
            ),
            ProviderOption(
                id = "openai",
                label = "OpenAI",
                models = listOf(ModelOption("gpt-5", listOf("medium", "high"), "medium", 400_000)),
                defaultModel = "gpt-5",
                available = false,
                unavailableReason = "No account configured",
            ),
            ProviderOption(
                id = "local-lab",
                label = "local-lab",
                models = listOf(
                    ModelOption("router-fast", emptyList(), null, 128_000),
                    ModelOption("router-deep", emptyList(), null, 128_000),
                ),
                defaultModel = "router-deep",
                available = true,
                unavailableReason = null,
            ),
        ),
    )

    private fun catalog(): SessionConfig = SessionConfig(
        catalogRevision = 7,
        catalogAvailable = true,
        unavailableReason = null,
        current = SessionSelection(
            sessionId = "s-nav",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            effort = "high",
        ),
        providers = listOf(
            SessionProvider(
                id = "anthropic",
                enabled = true,
                availability = "available",
                availabilityReason = null,
                defaultModel = "claude-sonnet-4-5",
                models = listOf(
                    SessionModel("claude-sonnet-4-5", 200_000, listOf("low", "medium", "high"), "high"),
                    SessionModel("claude-opus-4-1", 200_000, listOf("medium", "high"), "high"),
                ),
            ),
            SessionProvider(
                id = "openai",
                enabled = false,
                availability = "needs_credentials",
                availabilityReason = "No account configured",
                defaultModel = null,
                models = listOf(SessionModel("gpt-5", 400_000, listOf("medium"), "medium")),
            ),
            // A custom compatible server. Its inventory is ADVISORY, which is
            // the only thing that licenses a free-text model id: routers omit
            // ids from /v1/models that their chat wire still accepts.
            SessionProvider(
                id = "local-lab",
                enabled = true,
                availability = "available",
                availabilityReason = null,
                defaultModel = "router-deep",
                models = listOf(
                    SessionModel("router-fast", 128_000, emptyList(), null),
                    SessionModel("router-deep", 128_000, emptyList(), null),
                ),
                inventoryAuthority = "advisory",
            ),
        ),
    )

    private fun populatedRoster(): List<SessionRow> = listOf(
        SessionRow(
            id = "s-sms",
            title = "Reply to Amir about the lease",
            state = SessionVisualState.NeedsInput,
            runState = "parked_permission",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            effort = "high",
            lastActivityMs = nowMs - 45_000,
            seenAtMs = nowMs - 200_000,
            turnCount = 4,
            footprintTokens = 18_400,
            footprintExact = false,
            runId = "run-sms",
            workerGeneration = 3,
            headSeq = 412,
            needsInput = NeedsInput(
                kind = "approval",
                title = "Send this reply to Amir (+1 604 555 0142)?",
                safeBody = listOf(
                    "“Yes — 4 pm still works. I'll bring the signed copy.”",
                ),
                menuId = "menu-9f2",
                requestSeq = 88,
                workerGeneration = 3,
                sinceMs = nowMs - 134_000,
                options = listOf(
                    MenuOption("send", "Send it", decision = "allow_once"),
                    MenuOption("skip", "Don't send", decision = "reject_once"),
                ),
            ),
        ),
        SessionRow(
            id = "s-nav",
            title = "Fix nav crash on back gesture",
            state = SessionVisualState.Running,
            runState = "running",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            effort = "high",
            lastActivityMs = nowMs - 5_000,
            seenAtMs = nowMs,
            turnCount = 11,
            footprintTokens = 42_100,
            footprintExact = true,
            runId = "run-nav",
            workerGeneration = 6,
            headSeq = 981,
            // Main is implicit and carries no registry row, exactly as the wire
            // states; only the named ref is listed.
            branches = listOf(
                BranchView(
                    branchId = "branch-plan-b",
                    name = "Plan B",
                    forkNodeId = "node-fork-1",
                    forkSeq = 41,
                    createdSeq = 52,
                    createdAtMs = nowMs - 90 * 60_000,
                    headNodeId = "node-plan-b-head",
                    headSeq = 60,
                ),
            ),
            activeBranchId = null,
            mainHeadNodeId = "node-nav-head",
            mainHeadSeq = 981,
        ),
        SessionRow(
            id = "s-route",
            title = "Summarise this week's texts",
            state = SessionVisualState.WaitingForNetwork,
            runState = "waiting_for_route",
            provider = "anthropic",
            model = "claude-opus-4-1",
            effort = "medium",
            lastActivityMs = nowMs - 12 * 60_000,
            seenAtMs = nowMs - 12 * 60_000,
            runId = "run-route",
            workerGeneration = 2,
            headSeq = 55,
        ),
        SessionRow(
            id = "s-broken",
            title = "Port the settings screen",
            state = SessionVisualState.Errored,
            runState = "errored",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            effort = "high",
            lastActivityMs = nowMs - 5 * 60 * 60_000,
            seenAtMs = nowMs - 5 * 60 * 60_000,
            headSeq = 120,
        ),
        SessionRow(
            id = "s-old",
            title = null,
            state = SessionVisualState.Idle,
            runState = "idle",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            lastActivityMs = nowMs - 3 * 24 * 60 * 60_000L,
            seenAtMs = nowMs - 3 * 24 * 60 * 60_000L,
            forkedFrom = ForkProvenance("s-nav", 44),
            headSeq = 44,
            mainHeadNodeId = "node-old-head",
            mainHeadSeq = 44,
        ),
        SessionRow(
            id = "s-unknown",
            title = "Sweep the download folder",
            state = SessionVisualState.Unknown,
            runState = "effect_unknown",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            lastActivityMs = null,
            seenAtMs = null,
            headSeq = 3,
        ),
    )

    private fun largeRoster(count: Int): List<SessionRow> = (0 until count).map { index ->
        SessionRow(
            id = "s-%04d".format(index),
            title = "Session task ${index + 1}",
            state = when (index % 7) {
                0 -> SessionVisualState.Running
                3 -> SessionVisualState.Errored
                else -> SessionVisualState.Idle
            },
            runState = when (index % 7) {
                0 -> "running"
                3 -> "errored"
                else -> "idle"
            },
            provider = "anthropic",
            model = if (index % 2 == 0) "claude-sonnet-4-5" else "claude-opus-4-1",
            effort = if (index % 3 == 0) "high" else "medium",
            lastActivityMs = nowMs - index * 61_000L,
            seenAtMs = nowMs - index * 61_000L,
            runId = if (index % 7 == 0) "run-$index" else null,
            workerGeneration = index.toLong(),
            headSeq = index.toLong(),
        )
    }

    private fun defaultTranscript(sessionId: String): MutableList<Message> = mutableListOf(
        Message(nextMessageId++, Role.User, "What changed in this session?"),
        Message(
            id = nextMessageId++,
            role = Role.Agent,
            text = "Replayed transcript for `$sessionId`. Everything below is local history.",
            provider = "anthropic",
        ),
    )

    private fun runningTranscript(): MutableList<Message> = mutableListOf(
        Message(nextMessageId++, Role.User, "The back gesture crashes on the settings screen."),
        Message(
            id = nextMessageId++,
            role = Role.Agent,
            text = "Reproduced it. The nav controller pops past the start destination.",
            thinking = "Checking the back stack invariants first.",
            streaming = true,
            provider = "anthropic",
            tools = listOf(
                ToolCall("call-1", "shell", "gradlew :app:test", ToolStatus.Running, null),
            ),
        ),
    )

    private fun askTranscript(): MutableList<Message> = mutableListOf(
        Message(nextMessageId++, Role.User, "Reply to Amir and confirm 4 pm."),
        Message(
            id = nextMessageId++,
            role = Role.Agent,
            text = "Drafted the reply. It needs your approval before it goes out.",
            provider = "anthropic",
            // A finished call, with the duration the daemon reported: the row
            // is where that number belongs now (S3, verify-6 O8).
            tools = listOf(
                ToolCall("call-sms", "sms", "read inbox", ToolStatus.Completed, null, durationMs = 41_000L),
            ),
        ),
    )

    private fun erroredTranscript(): MutableList<Message> = mutableListOf(
        Message(nextMessageId++, Role.User, "Port the settings screen."),
        Message(
            id = nextMessageId++,
            role = Role.Agent,
            text = "",
            error = "provider returned 529 after 3 attempts",
            provider = "anthropic",
        ),
    )

    // ---------- checkpoints and branches ----------

    private val _branchSelection = MutableStateFlow<Map<String, String>>(emptyMap())
    override val branchSelection: StateFlow<Map<String, String>> = _branchSelection.asStateFlow()

    /**
     * The journal, newest LAST, exactly as a store would hold it. Every read
     * sorts it newest-first; nothing here is pre-sorted for the UI's benefit.
     */
    private val journal = mutableMapOf<String, MutableList<CheckpointView>>()
    private var nextCheckpointSeq = 0L

    /** Set to make the daemon look like one that does not serve `checkpoint_v1`. */
    var checkpointsUnavailable = false

    /** Set to make the daemon look like one that does not serve `branch_create_v1`. */
    var branchCreateUnavailable = false

    /** Forces the NEXT mutation's outcome; typed refusals are not reachable otherwise. */
    var nextCheckpointOutcome: CheckpointOutcome? = null

    override suspend fun selectBranch(sessionId: String, branchId: String?) {
        calls += "branch.select:$sessionId:${branchId ?: "main"}"
        _branchSelection.value = _branchSelection.value.toMutableMap().apply {
            // Absence IS main. Storing a sentinel would make main a branch id,
            // and `turn.submit` would then carry one for the implicit branch.
            if (branchId == null) remove(sessionId) else put(sessionId, branchId)
        }
    }

    override suspend fun checkpoints(
        sessionId: String,
        branchId: String?,
        cursor: Long?,
        limit: Int,
    ): CheckpointListResult {
        calls += CheckpointRpcAdapter.METHOD_CHECKPOINT_LIST + ":" + sessionId
        if (checkpointsUnavailable) {
            return CheckpointListResult.Unavailable(CheckpointUnavailable.FEATURE_ABSENT)
        }
        val rows = Checkpoints.newestFirst(
            journal.getOrPut(sessionId) { seededJournal(sessionId) }
                .filter { it.branchId == branchId },
        )
        // Newest-first paging: a cursor is the last emitted sequence and the
        // next page is strictly OLDER than it (checkpoint.rs:114).
        val page = rows.filter { cursor == null || (it.seq ?: Long.MAX_VALUE) < cursor }
        val window = page.take(limit)
        val more = page.size > window.size
        return CheckpointListResult.Page(
            CheckpointPage(
                checkpoints = window,
                nextCursor = if (more) window.lastOrNull()?.seq else null,
                cursorState = if (more) CheckpointCursorState.More else CheckpointCursorState.End,
            ),
        )
    }

    override suspend fun undoCheckpoint(
        sessionId: String,
        target: String,
        branchId: String?,
    ): CheckpointOutcome = mutate(
        method = CheckpointRpcAdapter.METHOD_CHECKPOINT_UNDO,
        origin = "undo",
        sessionId = sessionId,
        branchId = branchId,
        target = target,
    )

    override suspend fun redoCheckpoint(
        sessionId: String,
        target: String,
        branchId: String?,
    ): CheckpointOutcome = mutate(
        method = CheckpointRpcAdapter.METHOD_CHECKPOINT_REDO,
        origin = "redo",
        sessionId = sessionId,
        branchId = branchId,
        target = target,
    )

    override suspend fun rollbackTurn(
        sessionId: String,
        runId: String,
        branchId: String?,
    ): CheckpointOutcome {
        calls += CheckpointRpcAdapter.METHOD_CHECKPOINT_ROLLBACK_TURN + ":" + runId
        nextCheckpointOutcome?.let { nextCheckpointOutcome = null; return it }
        if (checkpointsUnavailable) {
            return CheckpointOutcome.Unavailable(CheckpointUnavailable.FEATURE_ABSENT)
        }
        val rows = journal.getOrPut(sessionId) { seededJournal(sessionId) }
        val restored = rows.filter { it.runId == runId }.mapNotNull { it.checkpointId }
        if (restored.isEmpty()) return CheckpointOutcome.Failed("run_unknown")
        val recorded = record(
            sessionId = sessionId,
            branchId = branchId,
            runId = runId,
            origin = "rollback_turn",
            sourceCheckpointId = restored.last(),
            paths = rows.filter { it.runId == runId }.flatMap { it.paths },
        )
        rows += recorded
        return CheckpointOutcome.Committed(
            CheckpointReceiptView(recorded, restored, WORKER_GENERATION),
        )
    }

    override suspend fun createBranch(
        sessionId: String,
        forkNodeId: String,
        forkSeq: Long,
        name: String?,
        sourceBranchId: String?,
    ): BranchOutcome {
        calls += CheckpointRpcAdapter.METHOD_BRANCH_CREATE + ":" + sessionId
        if (branchCreateUnavailable) {
            return BranchOutcome.Unavailable(CheckpointUnavailable.BRANCH_FEATURE_ABSENT)
        }
        val row = _sessions.value.firstOrNull { it.id == sessionId }
            ?: return BranchOutcome.Failed("session_unknown")
        // The daemon normalizes the name and always returns one; an empty
        // request name is not echoed back as an empty branch name.
        val resolved = name?.trim().orEmpty().ifBlank { "branch-${row.branches.size + 1}" }
        val branch = BranchView(
            branchId = "branch-${resolved.lowercase().replace(' ', '-')}",
            name = resolved,
            sourceBranchId = sourceBranchId,
            forkNodeId = forkNodeId,
            forkSeq = forkSeq,
            createdSeq = row.headSeq + 1,
            createdAtMs = nowMs,
            headNodeId = forkNodeId,
            headSeq = forkSeq,
        )
        _sessions.value = _sessions.value.map {
            if (it.id == sessionId) it.copy(branches = it.branches + branch) else it
        }
        return BranchOutcome.Created(branch)
    }

    private fun mutate(
        method: String,
        origin: String,
        sessionId: String,
        branchId: String?,
        target: String,
    ): CheckpointOutcome {
        calls += "$method:$target"
        nextCheckpointOutcome?.let { nextCheckpointOutcome = null; return it }
        if (checkpointsUnavailable) {
            return CheckpointOutcome.Unavailable(CheckpointUnavailable.FEATURE_ABSENT)
        }
        val rows = journal.getOrPut(sessionId) { seededJournal(sessionId) }
        val ordered = Checkpoints.newestFirst(rows.filter { it.branchId == branchId })
        val source = if (target == Checkpoints.TARGET_LAST) {
            ordered.firstOrNull { it.addressable }
        } else {
            ordered.firstOrNull { it.checkpointId == target }
        } ?: return CheckpointOutcome.Failed("checkpoint_unknown")
        val recorded = record(
            sessionId = sessionId,
            branchId = branchId,
            runId = source.runId,
            origin = origin,
            sourceCheckpointId = source.checkpointId,
            paths = source.paths,
        )
        // An undo/redo is itself an ordinary append-only journal entry and can
        // be undone in turn (checkpoint.rs:43), so it goes on the end.
        rows += recorded
        return CheckpointOutcome.Committed(
            CheckpointReceiptView(
                checkpoint = recorded,
                restoredCheckpointIds = listOfNotNull(source.checkpointId),
                workerGeneration = WORKER_GENERATION,
            ),
        )
    }

    private fun record(
        sessionId: String,
        branchId: String?,
        runId: String?,
        origin: String,
        sourceCheckpointId: String?,
        paths: List<CheckpointPathView>,
    ): CheckpointView {
        nextCheckpointSeq += 1
        return CheckpointView(
            checkpointId = "checkpoint-$origin-$nextCheckpointSeq",
            sessionId = sessionId,
            branchId = branchId,
            runId = runId,
            effectId = "effect-$nextCheckpointSeq",
            callId = "call-$nextCheckpointSeq",
            seq = SEED_SEQ_BASE + nextCheckpointSeq,
            workspaceRevision = "workspace-$nextCheckpointSeq",
            kind = CheckpointCategory.of("write", CHECKPOINT_KINDS),
            origin = CheckpointCategory.of(origin, CHECKPOINT_ORIGINS),
            sourceCheckpointId = sourceCheckpointId,
            paths = paths,
            postDigest = "blake3:post-$nextCheckpointSeq",
            recordedAtMs = nowMs,
        )
    }

    /**
     * Two turns' worth of durable edits, plus the two honest edge cases the
     * sheet has to render: an entry whose pre-image was too large to freeze,
     * and an entry whose `kind` this client does not recognise.
     */
    private fun seededJournal(sessionId: String): MutableList<CheckpointView> {
        if (sessionId != "s-nav") return mutableListOf()
        nextCheckpointSeq = maxOf(nextCheckpointSeq, 5)
        fun row(
            index: Int,
            run: String,
            kind: String,
            origin: String,
            paths: List<CheckpointPathView>,
        ) = CheckpointView(
            checkpointId = "checkpoint-nav-$index",
            sessionId = sessionId,
            branchId = null,
            runId = run,
            effectId = "effect-nav-$index",
            callId = "call-nav-$index",
            seq = SEED_SEQ_BASE + index,
            workspaceRevision = "workspace-nav-$index",
            kind = CheckpointCategory.of(kind, CHECKPOINT_KINDS),
            origin = CheckpointCategory.of(origin, CHECKPOINT_ORIGINS),
            sourceCheckpointId = null,
            paths = paths,
            postDigest = "blake3:nav-$index",
            recordedAtMs = nowMs - (6 - index) * 60_000L,
        )
        return mutableListOf(
            row(
                1, "run-nav-1", "create", "tool",
                listOf(CheckpointPathView("app/src/main/java/NavHost.kt", postDigest = "blake3:a")),
            ),
            row(
                2, "run-nav-1", "edit", "tool",
                listOf(
                    CheckpointPathView(
                        "app/src/main/java/NavHost.kt",
                        preDigest = "blake3:a",
                        postDigest = "blake3:b",
                        preArtifact = "blake3:artifact-a",
                    ),
                ),
            ),
            row(
                3, "run-nav-2", "write", "tool",
                listOf(
                    CheckpointPathView(
                        "app/src/main/assets/dump.bin",
                        preDigest = "blake3:big",
                        postDigest = "blake3:big-post",
                        truncatedReason = "pre-image exceeds 8388608 bytes",
                    ),
                ),
            ),
            row(
                4, "run-nav-2", "delete", "tool",
                listOf(CheckpointPathView("app/src/main/java/Legacy.kt", preDigest = "blake3:c")),
            ),
            // A kind this client does not know. It renders the daemon's own
            // word and says it does not recognise it, rather than dropping the
            // row or folding it into a neighbour.
            row(5, "run-nav-2", "rename", "tool", emptyList()),
        )
    }

    companion object {
        /** Bounds a runaway cursor rather than paging forever. */
        const val MAX_SEARCH_PAGES = 64

        /** The generation every seeded checkpoint receipt is fenced against. */
        const val WORKER_GENERATION: Long = 6

        /** Journal sequences start well above zero; zero is a producer placeholder. */
        const val SEED_SEQ_BASE: Long = 400

        /** A fixed wall clock so screenshots are byte-stable. */
        const val FIXED_NOW: Long = 1_772_000_000_000L

        /** A fixed *monotonic* clock; uptime is measured against this one. */
        const val FIXED_UPTIME: Long = 40_000_000L
    }
}
