package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.ui.accounts.AccountsRpcAdapter
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

    /**
     * Delegation: two parents with children, one of them deep enough to nest,
     * one wide enough to arrive collapsed, a child parked on a human, a
     * bounded snapshot, an agent the observe digest lists that the bounded
     * snapshot omitted, and a session whose fleet read the daemon refuses.
     */
    Fleet,
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
            FakeScenario.Fleet -> {
                _status.value = running()
                setSessions(populatedRoster() + fleetRoster())
                _activeSessionId.value = "s-fleet"
                transcripts["s-fleet"] = runningTranscript()
                transcripts["s-fleet-b"] = childTranscript()
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
        calls += "chat.send:$sessionId"
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

    // ---------- subagents and the fleet ----------

    /**
     * `session.fleet` (frame.rs:3495).
     *
     * The fixtures model the shapes the wire really produces, including the two
     * a client is most likely to get wrong: a node with no children **and** a
     * non-zero `folded_children` (bounded, not a leaf), and a snapshot whose
     * `truncated`/`complete` pair says the tree is not all of it.
     */
    override suspend fun fleet(sessionId: String): FleetLoad {
        calls += "${FleetRpcAdapter.METHOD_SESSION_FLEET}:$sessionId"
        fleetOverride?.invoke(sessionId)?.let { return it }
        return when (sessionId) {
            "s-fleet" -> FleetLoad.Snapshot(fleetSnapshot())
            "s-swarm" -> FleetLoad.Snapshot(swarmSnapshot())
            // A daemon that does not offer `session_fleet_v1` (frame.rs:395)
            // says so; the panel prints the reason rather than an empty list.
            "s-broken" -> FleetLoad.Unavailable("session_fleet_v1 unsupported")
            // Everything else: the daemon answered, and the answer is none.
            else -> FleetLoad.Snapshot(
                FleetSnapshot(
                    sessionId = sessionId,
                    generatedAtMs = nowMs,
                    nodeLimit = NODE_LIMIT,
                    depthLimit = DEPTH_LIMIT,
                    roots = emptyList(),
                    rollup = FleetRollup(
                        nodeCount = 0,
                        maxDepth = 0,
                        metricsComplete = true,
                        complete = true,
                    ),
                    truncated = false,
                ),
            )
        }
    }

    override suspend fun subagents(sessionId: String): SubagentLoad {
        calls += "${FleetRpcAdapter.METHOD_SESSION_OBSERVE}.subagents:$sessionId"
        subagentOverride?.invoke(sessionId)?.let { return it }
        return SubagentLoad.Observed(
            when (sessionId) {
                "s-fleet" -> listOf(
                    Subagent(
                        agentId = "agent-scout",
                        callsign = "scout",
                        task = "Map every transport call site",
                        state = FleetStateView.of("live"),
                        provider = "anthropic",
                    ),
                    Subagent(
                        agentId = "agent-auditor",
                        callsign = "auditor",
                        task = "Check the frame contracts",
                        state = FleetStateView.of("waiting"),
                        provider = "openai",
                    ),
                    // No callsign: the chip must show a marked id fallback, and
                    // this agent is deliberately absent from the bounded
                    // snapshot, so its child session is not addressable and the
                    // chip opens the panel rather than guessing a session id.
                    Subagent(
                        agentId = "agent-7f2c91ab4de0",
                        callsign = null,
                        task = "Re-run the wire fixtures",
                        state = FleetStateView.of("queued"),
                        provider = null,
                    ),
                )
                else -> emptyList()
            },
        )
    }

    /** Overrides for a test that needs a refused or failed read. */
    var fleetOverride: ((String) -> FleetLoad?)? = null
    var subagentOverride: ((String) -> SubagentLoad?)? = null

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

    /**
     * Two delegating parents and their children.
     *
     * `parent_session_id` is the daemon's own edge (frame.rs:1949), so these
     * are ordinary roster rows that happen to name a parent — exactly what a
     * real `session.list` returns for a delegated session.
     */
    private fun fleetRoster(): List<SessionRow> = listOf(
        SessionRow(
            id = "s-fleet",
            title = "Refactor the transport layer",
            state = SessionVisualState.Running,
            runState = "running",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            effort = "high",
            lastActivityMs = nowMs - 3_000,
            seenAtMs = nowMs,
            turnCount = 7,
            runId = "run-fleet",
            workerGeneration = 5,
            headSeq = 612,
        ),
        SessionRow(
            id = "s-fleet-a",
            title = "scout · map transport call sites",
            state = SessionVisualState.Running,
            runState = "running",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            lastActivityMs = nowMs - 9_000,
            seenAtMs = nowMs,
            parentSessionId = "s-fleet",
            kind = "subagent",
            runId = "run-fleet-a",
            workerGeneration = 5,
            headSeq = 88,
        ),
        SessionRow(
            id = "s-fleet-a1",
            title = "probe · read the frame tests",
            state = SessionVisualState.Idle,
            runState = "idle",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            lastActivityMs = nowMs - 20_000,
            seenAtMs = nowMs,
            parentSessionId = "s-fleet-a",
            kind = "subagent",
            headSeq = 12,
        ),
        // A child parked on a human. The family it belongs to therefore sorts
        // into NEEDS YOU as a whole: nesting must not bury an agent that is
        // asking for something.
        SessionRow(
            id = "s-fleet-b",
            title = "auditor · check the frame contracts",
            state = SessionVisualState.NeedsInput,
            runState = "parked_input",
            provider = "openai",
            model = "gpt-5",
            lastActivityMs = nowMs - 30_000,
            seenAtMs = nowMs - 30_000,
            parentSessionId = "s-fleet",
            kind = "subagent",
            runId = "run-fleet-b",
            workerGeneration = 5,
            headSeq = 41,
            needsInput = NeedsInput(
                kind = "question",
                title = "Which contract version should the audit assume?",
                safeBody = listOf("Both v1 and v2 frames are present in the fixtures."),
                menuId = "menu-audit",
                requestSeq = 12,
                workerGeneration = 5,
                sinceMs = nowMs - 26_000,
                options = listOf(
                    MenuOption("v1", "Assume v1"),
                    MenuOption("v2", "Assume v2"),
                ),
            ),
        ),
        SessionRow(
            id = "s-fleet-c",
            title = "scribe · write the migration note",
            state = SessionVisualState.Idle,
            runState = "idle",
            provider = "anthropic",
            model = "claude-sonnet-4-5",
            lastActivityMs = nowMs - 4 * 60_000,
            seenAtMs = nowMs - 4 * 60_000,
            parentSessionId = "s-fleet",
            kind = "subagent",
            headSeq = 30,
        ),
        SessionRow(
            id = "s-swarm",
            title = "Sweep the dependency tree",
            state = SessionVisualState.Running,
            runState = "running",
            provider = "anthropic",
            model = "claude-opus-4-1",
            effort = "medium",
            lastActivityMs = nowMs - 60_000,
            seenAtMs = nowMs - 60_000,
            runId = "run-swarm",
            workerGeneration = 2,
            headSeq = 210,
        ),
        // Five children: over COLLAPSE_THRESHOLD, so this family arrives folded
        // and the parent's pill is the only thing standing in for them.
    ) + (1..5).map { index ->
        SessionRow(
            id = "s-swarm-$index",
            title = "worker $index · audit module $index",
            state = if (index == 2) SessionVisualState.Errored else SessionVisualState.Running,
            runState = if (index == 2) "errored" else "running",
            provider = "anthropic",
            model = "claude-opus-4-1",
            lastActivityMs = nowMs - index * 11_000L,
            seenAtMs = nowMs - index * 11_000L,
            parentSessionId = "s-swarm",
            kind = "subagent",
            runId = if (index == 2) null else "run-swarm-$index",
            workerGeneration = 2,
            headSeq = index.toLong(),
        )
    }

    /**
     * The bounded snapshot for `s-fleet`.
     *
     * `scribe` carries `folded_children = 2` with an empty child list: the two
     * agents under it exist and were not returned. The snapshot says
     * `truncated = true` and the rollup says `complete = false`, which is the
     * only combination that may be drawn as bounded.
     */
    private fun fleetSnapshot(): FleetSnapshot = FleetSnapshot(
        sessionId = "s-fleet",
        generatedAtMs = nowMs,
        nodeLimit = NODE_LIMIT,
        depthLimit = DEPTH_LIMIT,
        roots = listOf(
            FleetNode(
                agentId = "agent-scout",
                sessionId = "s-fleet-a",
                callsign = "scout",
                model = "claude-sonnet-4-5",
                provider = "anthropic",
                task = "Map every transport call site",
                depth = 1,
                parentSessionId = "s-fleet",
                state = FleetStateView.of("live"),
                metrics = FleetNodeMetrics(
                    startedAtMs = nowMs - 120_000,
                    live = true,
                    toolAttempts = 14,
                    usage = FleetUsage(41_200, 3_100, 18_000, 900),
                ),
                children = listOf(
                    FleetNode(
                        agentId = "agent-probe",
                        sessionId = "s-fleet-a1",
                        callsign = "probe",
                        provider = "anthropic",
                        task = "Read the frame tests",
                        depth = 2,
                        parentSessionId = "s-fleet-a",
                        parentAgentId = "agent-scout",
                        state = FleetStateView.of("queued"),
                    ),
                ),
            ),
            FleetNode(
                agentId = "agent-auditor",
                sessionId = "s-fleet-b",
                callsign = "auditor",
                model = "gpt-5",
                provider = "openai",
                task = "Check the frame contracts",
                depth = 1,
                parentSessionId = "s-fleet",
                state = FleetStateView.of("waiting"),
                metrics = FleetNodeMetrics(
                    startedAtMs = nowMs - 90_000,
                    live = true,
                    toolAttempts = 3,
                ),
            ),
            FleetNode(
                agentId = "agent-scribe",
                sessionId = "s-fleet-c",
                callsign = "scribe",
                provider = "anthropic",
                task = "Write the migration note",
                depth = 1,
                parentSessionId = "s-fleet",
                state = FleetStateView.of("done"),
                // Bounded, not a leaf: two real children were not returned.
                foldedChildren = 2,
            ),
        ),
        rollup = FleetRollup(
            nodeCount = 4,
            states = FleetStateCounts(queued = 1, live = 1, waiting = 1, done = 1),
            maxDepth = 2,
            elapsedMs = 214_000,
            toolAttempts = 17,
            // One node has no durable usage truth, so the total is absent
            // rather than a partial sum presented as a whole one.
            usage = null,
            metricsComplete = false,
            complete = false,
        ),
        truncated = true,
    )

    /** A complete snapshot, so the bounded banner has something to contrast. */
    private fun swarmSnapshot(): FleetSnapshot = FleetSnapshot(
        sessionId = "s-swarm",
        generatedAtMs = nowMs,
        nodeLimit = NODE_LIMIT,
        depthLimit = DEPTH_LIMIT,
        roots = (1..5).map { index ->
            FleetNode(
                agentId = "agent-worker-$index",
                sessionId = "s-swarm-$index",
                callsign = "worker $index",
                provider = "anthropic",
                task = "Audit module $index",
                depth = 1,
                parentSessionId = "s-swarm",
                state = FleetStateView.of(if (index == 2) "failed" else "live"),
            )
        },
        rollup = FleetRollup(
            nodeCount = 5,
            states = FleetStateCounts(live = 4, failed = 1),
            maxDepth = 1,
            elapsedMs = 402_000,
            toolAttempts = 61,
            usage = FleetUsage(120_000, 9_400, 44_000, 2_100),
            metricsComplete = true,
            complete = true,
        ),
        truncated = false,
    )

    private fun childTranscript(): MutableList<Message> = mutableListOf(
        Message(nextMessageId++, Role.User, "Check the frame contracts against the fixtures."),
        Message(
            id = nextMessageId++,
            role = Role.Agent,
            text = "Both v1 and v2 frames are present. I need to know which one to assume.",
            provider = "openai",
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

    companion object {
        /** Bounds a runaway cursor rather than paging forever. */
        const val MAX_SEARCH_PAGES = 64

        /** A fixed wall clock so screenshots are byte-stable. */
        const val FIXED_NOW: Long = 1_772_000_000_000L

        /** A fixed *monotonic* clock; uptime is measured against this one. */
        const val FIXED_UPTIME: Long = 40_000_000L

        /**
         * The bounds the daemon applies to one `session.fleet` read
         * (frame.rs:87). They are echoed in every snapshot so a bounded one can
         * say what it was bounded by.
         */
        const val NODE_LIMIT = 64
        const val DEPTH_LIMIT = 4
    }
}
