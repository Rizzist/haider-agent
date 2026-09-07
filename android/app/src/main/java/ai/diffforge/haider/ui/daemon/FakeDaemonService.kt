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
import kotlinx.coroutines.flow.MutableStateFlow
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
            if (index >= 0) messages[index] = messages[index].copy(streaming = false, status = null)
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

    override suspend fun selectModel(provider: String, model: String) {
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

    override suspend fun selectEffort(effort: String?) {
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
            status = "queued…",
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
            status = "shell · gradlew test — 41s",
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
    }
}
