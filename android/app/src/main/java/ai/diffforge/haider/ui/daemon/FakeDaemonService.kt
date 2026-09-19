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
import ai.diffforge.haider.ui.loom.LoomAuthorConfirmed
import ai.diffforge.haider.ui.loom.LoomAuthorDraft
import ai.diffforge.haider.ui.loom.LoomAuthorError
import ai.diffforge.haider.ui.loom.LoomAuthorKind
import ai.diffforge.haider.ui.loom.LoomEntryKind
import ai.diffforge.haider.ui.loom.LoomFence
import ai.diffforge.haider.ui.loom.LoomInstallJob
import ai.diffforge.haider.ui.loom.LoomRegistry
import ai.diffforge.haider.ui.loom.LoomRpcAdapter
import ai.diffforge.haider.ui.loom.LoomValidation
import ai.diffforge.haider.ui.state.CapabilityApproval
import ai.diffforge.haider.ui.state.PermissionMode
import ai.diffforge.haider.ui.workflow.ChildGraphLink
import ai.diffforge.haider.ui.workflow.WorkflowGraphRead
import ai.diffforge.haider.ui.workflow.WorkflowRpcAdapter
import ai.diffforge.haider.ui.workflow.WorkflowWatchPage
import kotlinx.coroutines.flow.Flow
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
    /** A session running the fake workflow, mid-retry (lane 971-UI-workflows). */
    WorkflowRunning,

    /** The same session on a daemon that does not advertise `workflow_graph_v1`. */
    WorkflowUnavailable,

    /** The daemon carries no Loom registry: the screen says so and offers nothing. */
    LoomUnavailable,
}

class FakeDaemonService(
    scenario: FakeScenario = FakeScenario.Populated,
    private val nowMs: Long = FIXED_NOW,
) : DaemonService {

    private val _status = MutableStateFlow<DaemonStatus>(DaemonStatus.Stopped)
    private val _environment = MutableStateFlow(DaemonEnvironment())
    private val _sessions = MutableStateFlow<List<SessionRow>>(emptyList())
    /** This fixture starts hydrated; tests can model delayed Binder/RPC readiness. */
    override val rosterReady = MutableStateFlow(true)
    private val _paging = MutableStateFlow(RosterPaging())
    private val _activeSessionId = MutableStateFlow<String?>(null)
    private val _models = MutableStateFlow<SessionConfig?>(null)
    private val _catalogError = MutableStateFlow<String?>(null)
    private val _catalogRequestedAtMs = MutableStateFlow<Long?>(null)
    private val _searchIndex = MutableStateFlow(SearchIndexState())
    private val _providers = MutableStateFlow(ProviderInventory())
    private val _shell = MutableStateFlow(
        // Deliberately unavailable fixture until setShell supplies a capability.
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
    private val _shellExecutions = MutableStateFlow<Map<String, List<ShellExecution>>>(emptyMap())
    override val shellExecutions = _shellExecutions.asStateFlow()
    override suspend fun refreshShell() = Unit
    override suspend fun startShell(sessionId: String, submissionId: String, command: String, cwd: String?): ShellExecutionRef {
        val existing = _shellExecutions.value.values.flatten().firstOrNull { it.ref.commandId == submissionId }
        if (existing != null) {
            require(existing.ref.sessionId == sessionId && existing.command == command)
            return existing.ref
        }
        require(command.isNotBlank() && command.toByteArray(Charsets.UTF_8).size <= 8192)
        require(cwd == null || (cwd.isNotEmpty() && !java.io.File(cwd).isAbsolute))
        check(_shell.value.available) { _shell.value.reason ?: "shell_unavailable" }
        val session = _sessions.value.firstOrNull { it.id == sessionId } ?: error("session_unavailable")
        val history = _shellExecutions.value[sessionId].orEmpty()
        check(history.none { it.status == ShellExecutionStatus.Running }) { "session_busy" }
        val ref = ShellExecutionRef(sessionId, submissionId, "shell-run-$submissionId", "shell-item-$submissionId",
            session.workerGeneration ?: 1)
        _shellExecutions.value = _shellExecutions.value + (sessionId to (history + ShellExecution(ref, command, ShellExecutionStatus.Running)).takeLast(64))
        return ref
    }
    override suspend fun cancelShell(execution: ShellExecutionRef) {
        setShellResult(execution, ShellExecutionStatus.Cancelled)
    }
    /** Deterministic UI fixture controls: no actual shell or auto-generated success. */
    fun appendShellOutput(execution: ShellExecutionRef, text: String, stream: ShellOutputStream = ShellOutputStream.Stdout) {
        updateShell(execution) { current -> current.copy(output = current.output + ShellOutput(
            (current.output.lastOrNull()?.seq ?: 0) + 1, stream,
            java.util.Base64.getEncoder().encodeToString(text.toByteArray(Charsets.UTF_8)))) }
    }
    fun setShellResult(execution: ShellExecutionRef, status: ShellExecutionStatus, exitCode: Int? = null, error: String? = null) {
        updateShell(execution) { it.copy(status = status, exitCode = exitCode, error = error) }
    }
    private fun updateShell(execution: ShellExecutionRef, update: (ShellExecution) -> ShellExecution) {
        val history = _shellExecutions.value[execution.sessionId].orEmpty()
        check(history.any { it.ref == execution }) { "shell_execution_unavailable" }
        _shellExecutions.value = _shellExecutions.value + (execution.sessionId to history.map { if (it.ref == execution) update(it) else it })
    }


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
        // The transcript is what a person looks at. Round 11 flipped the row to
        // Idle and left the approval prose on screen with a null result, so the
        // visible transcript and the canonical one disagreed (verify-10 O2).
        val completed = ToolCall(
            callId = "call-auto-$sessionId",
            name = "sms",
            summary = "sms.list --limit 20",
            status = ToolStatus.Completed,
            result = AUTO_SMS_RESULT,
            durationMs = 1_200L,
        )
        val messages = transcripts.getOrPut(sessionId) { mutableListOf() }
        val index = messages.indexOfLast { it.streaming }
        if (index >= 0) {
            messages[index] = messages[index].copy(
                streaming = false,
                text = AUTO_SMS_TEXT,
                tools = messages[index].tools.filterNot { it.name == "sms" } + completed,
            )
        } else {
            messages += Message(
                id = nextMessageId++,
                role = Role.Agent,
                text = AUTO_SMS_TEXT,
                provider = "anthropic",
                tools = listOf(completed),
            )
        }
        // The stream the UI actually collects has to carry it, or the screen
        // keeps whatever it loaded first.
        publishTranscript(sessionId)
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

    /**
     * The scripted workflow/Loom daemon.
     *
     * Declared **above** the `init` block on purpose: property initialisers run
     * in declaration order, and [apply] reaches for this on the workflow
     * scenarios. Below the init it would still be null when the constructor
     * ran, and only the two new scenarios would have found out.
     *
     * Public so a test can walk its recorded activation steps: the screen is
     * driven by daemon facts arriving in order, and a test that cannot advance
     * them can only ever check one frame of a live surface.
     */
    val workflowLoom = FakeWorkflowLoom()

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
            // The workflow surfaces. The roster is the populated one, because a
            // workflow is a fact about a session and not a mode the app is in.
            FakeScenario.WorkflowRunning -> {
                _status.value = running()
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
                transcripts["s-nav"] = runningTranscript()
            }
            FakeScenario.WorkflowUnavailable -> {
                _status.value = running()
                workflowLoom.graphUnavailable = true
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
                transcripts["s-nav"] = runningTranscript()
            }
            FakeScenario.LoomUnavailable -> {
                _status.value = running()
                workflowLoom.loomUnavailableReason = workflowLoom.loomFeature
                workflowLoom.authoringUnavailableReason = "no_model_selected"
                setSessions(populatedRoster())
                _activeSessionId.value = "s-nav"
            }
        }
    }

    // ---------- workflow + Loom (lane 971-UI-workflows) ----------

    override suspend fun workflowGraphState(sessionId: String, graphId: String?): WorkflowGraphRead {
        calls += "${WorkflowRpcAdapter.METHOD_GRAPH_STATE}:$sessionId"
        // Only the session the fixture actually runs a workflow on has one. A
        // fake that answered for every id would hide the "no live graph" path,
        // which is the state most sessions are in.
        val row = _sessions.value.firstOrNull { it.id == sessionId }
        if (row?.workflow == null) return WorkflowGraphRead.NoGraph
        return workflowLoom.graphState()
    }

    override suspend fun workflowGraphWatch(
        sessionId: String,
        afterCursor: String,
        limit: Int,
    ): WorkflowWatchPage {
        calls += "${WorkflowRpcAdapter.METHOD_GRAPH_WATCH}:$sessionId:$afterCursor"
        return workflowLoom.watch(afterCursor, limit)
    }

    override suspend fun childGraphLinks(sessionId: String): List<ChildGraphLink> =
        workflowLoom.childLinks()

    override suspend fun loomList(includeArchived: Boolean): LoomRegistry? {
        calls += "${LoomRpcAdapter.METHOD_LIST}:$includeArchived"
        if (workflowLoom.loomUnavailableReason != null) return null
        return workflowLoom.loomList(includeArchived)
    }

    override suspend fun loomUnavailable(): String? = workflowLoom.loomUnavailableReason

    override suspend fun loomInstallJobs(): List<LoomInstallJob> {
        if (workflowLoom.loomUnavailableReason != null) return emptyList()
        calls += LoomRpcAdapter.METHOD_INSTALL_STATUS
        return workflowLoom.installJobs()
    }

    override suspend fun loomSetArchived(
        kind: LoomEntryKind,
        id: String,
        archived: Boolean,
        fence: LoomFence,
    ): Boolean {
        val method = if (archived) LoomRpcAdapter.METHOD_ARCHIVE else LoomRpcAdapter.METHOD_UNARCHIVE
        // The fence is recorded with the call: an archive without the revision
        // it read is a compare-and-set against a value nobody observed.
        calls += "$method:$id:${fence.expectedRev}"
        return workflowLoom.setArchived(kind, id, archived, fence)
    }

    override suspend fun loomValidate(kind: LoomAuthorKind, text: String): LoomValidation {
        calls += LoomRpcAdapter.METHOD_VALIDATE
        return workflowLoom.validation(text)
    }

    override suspend fun loomAuthorDraft(
        sessionId: String,
        kind: LoomAuthorKind,
        prose: String,
    ): LoomAuthorDraft {
        calls += "${LoomRpcAdapter.METHOD_AUTHOR_DRAFT}:$sessionId"
        return workflowLoom.authorDraft(kind, prose)
    }

    override suspend fun loomAuthorRevise(draft: LoomAuthorDraft, text: String): LoomAuthorDraft {
        calls += "${LoomRpcAdapter.METHOD_AUTHOR_REVISE}:${draft.authoringId}:${draft.revision}"
        return workflowLoom.authorRevise(draft, text)
    }

    override suspend fun loomAuthorConfirm(
        draft: LoomAuthorDraft,
        text: String,
        registryFence: LoomFence?,
    ): Pair<LoomAuthorConfirmed?, List<LoomAuthorError>> {
        calls += "${LoomRpcAdapter.METHOD_AUTHOR_CONFIRM}:${draft.authoringId}:${draft.revision}"
        return workflowLoom.authorConfirm(draft, text)
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

    /** Configure the same capability consumed by shell submission. */
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

    /** Set to make the next submit refuse, with the daemon's own code. */
    var nextSendRefusal: String? = null

    /** How many attachments the fake accepts, mirroring the daemon's ceiling. */
    var attachmentCeiling = 4

    override suspend fun send(
        sessionId: String,
        text: String,
        attachments: List<Attachment>,
        mode: Delivery,
    ) {
        // `turn.submit` carries `branch_id` when one is chosen and OMITS it for
        // the implicit main branch (frame.rs:3697; wire transcript entry 82).
        // The composer never passes a branch: the facade holds the selection,
        // so choosing a branch in the sheet threads through every later send
        // without the send path knowing branches exist (lane 971-UI-extras).
        val branch = _branchSelection.value[sessionId]
        calls += buildString {
            append("turn.submit:")
            append(sessionId)
            append(':')
            append(mode.wire)
            append(':')
            append(attachments.size)
            if (branch != null) append(":branch=").append(branch)
        }
        nextSendRefusal?.let { code -> nextSendRefusal = null; throw TurnRefused(code) }
        if (attachments.size > attachmentCeiling) {
            throw TurnRefused(AttachmentLimits.TOO_MANY)
        }
        val running = _sessions.value.firstOrNull { it.id == sessionId }?.runId != null
        if (running && mode == Delivery.Queue) {
            // Held behind the active turn, exactly as queue.list would report.
            val next = _queue.value
            _queue.value = next.copy(
                revision = next.revision + 1,
                rows = next.rows + QueuedMessage(
                    id = "q-${next.rows.size + 1}",
                    text = text,
                    delivery = Delivery.Queue,
                    ordinal = next.rows.size + 1,
                    createdAtMs = nowMs,
                ),
                supported = true,
            )
            return
        }
        val messages = transcripts.getOrPut(sessionId) { mutableListOf() }
        messages += Message(nextMessageId++, Role.User, text, attachments = attachments)
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
        publishTranscript(sessionId)
    }

    override suspend fun stageAttachment(
        bytes: ByteArray,
        mime: String,
        name: String?,
    ): Attachment? {
        calls += "vault.stage_attachment:$mime:${bytes.size}"
        if (bytes.size > MAX_ATTACHMENT_BYTES) return null
        val artifact = "blake3:${"%064x".format(bytes.size.toBigInteger())}"
        cas[artifact] = bytes
        return when {
            mime.startsWith("image/") -> Attachment.Image(artifact, mime, width = 1024, height = 768)
            name != null -> Attachment.TextFile(artifact, name, lines = 42)
            else -> Attachment.PastedText(artifact, lines = 12)
        }
    }

    override suspend fun attachmentBytes(artifact: String): ByteArray? = cas[artifact]

    private val cas = mutableMapOf<String, ByteArray>()

    private val _queue = MutableStateFlow(QueueSnapshot(supported = true))
    override val queue: StateFlow<QueueSnapshot> = _queue.asStateFlow()

    override suspend fun refreshQueue(sessionId: String) {
        calls += "queue.list:$sessionId"
    }

    override suspend fun removeQueued(sessionId: String, id: String, revision: Long) {
        calls += "queue.remove:$sessionId:$id:$revision"
        // Fenced: a stale revision is refused, never applied to whatever moved
        // into that row.
        if (revision != _queue.value.revision) throw StaleQueueRevision(revision)
        _queue.value = _queue.value.let { snapshot ->
            snapshot.copy(
                revision = snapshot.revision + 1,
                rows = snapshot.rows.filterNot { it.id == id },
            )
        }
    }

    override suspend fun promoteQueued(sessionId: String, id: String, revision: Long) {
        calls += "queue.promote_steer:$sessionId:$id:$revision"
        if (revision != _queue.value.revision) throw StaleQueueRevision(revision)
        _queue.value = _queue.value.let { snapshot ->
            snapshot.copy(
                revision = snapshot.revision + 1,
                rows = snapshot.rows.filterNot { it.id == id },
            )
        }
    }

    private val _usage = MutableStateFlow(
        UsageSnapshot(
            generatedAtMs = nowMs,
            supported = true,
            accounts = listOf(
                AccountUsage(
                    provider = "anthropic",
                    alias = "work",
                    identity = "you@anthropic",
                    plan = "max",
                    usage = TokenUsage(
                        inputTokens = 184_200,
                        outputTokens = 39_400,
                        reasoningTokens = 12_100,
                        cachedTokens = 96_800,
                        estCostUsd = 2.41,
                    ),
                ),
            ),
        ),
    )
    override val usage: StateFlow<UsageSnapshot> = _usage.asStateFlow()

    override suspend fun refreshUsage() {
        calls += "usage.report"
    }

    fun setQueue(snapshot: QueueSnapshot) {
        _queue.value = snapshot
    }

    fun setUsage(snapshot: UsageSnapshot) {
        _usage.value = snapshot
    }

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

    /**
     * A live stream per session, not a one-shot.
     *
     * The one-shot default meant a mid-turn change — the Auto completion, for
     * one — never reached the screen, because nothing emitted again after the
     * initial load (verify-10 O2).
     */
    private val transcriptFlows = mutableMapOf<String, MutableStateFlow<TranscriptLoad>>()

    override fun transcriptUpdates(sessionId: String): Flow<TranscriptLoad> =
        transcriptStream?.invoke(sessionId) ?: transcriptFlow(sessionId).asStateFlow()

    private fun transcriptFlow(sessionId: String): MutableStateFlow<TranscriptLoad> =
        transcriptFlows.getOrPut(sessionId) {
            // The stream performs the initial replay, so it records the attach
            // the real facade would make. Round 12 built the first value
            // straight from the snapshot and lost that, which the fleet lane's
            // "the child's own session was replayed" pin caught immediately.
            calls += "session.attach:$sessionId"
            MutableStateFlow(snapshotTranscript(sessionId))
        }

    /** The same value [transcript] returns, without the suspend or the call log. */
    private fun snapshotTranscript(sessionId: String): TranscriptLoad =
        transcriptOverride?.invoke(sessionId)
            ?: TranscriptLoad.Complete(
                transcripts.getOrPut(sessionId) { defaultTranscript(sessionId) }.toList(),
            )

    /** Pushes the current transcript to whoever is collecting this session. */
    private fun publishTranscript(sessionId: String) {
        transcriptFlow(sessionId).value = snapshotTranscript(sessionId)
    }

    override suspend fun transcript(sessionId: String): TranscriptLoad {
        calls += "session.attach:$sessionId"
        return snapshotTranscript(sessionId)
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
            version = "0.0.970",
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
                // A router whose published list is advisory: the one provider
                // where a free-text id is legitimate.
                inventoryAuthority = "advisory",
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
            // The only row with a workflow. Every other session reads "no
            // workflow", which is the state most sessions are actually in.
            workflow = FakeWorkflowLoom.SESSION_WORKFLOW,
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
        /** The fake's own byte ceiling; the daemon's is its own business. */
        const val MAX_ATTACHMENT_BYTES = 8 * 1024 * 1024

        /** What the allowed call returned, rendered verbatim in the tool row. */
        const val AUTO_SMS_RESULT =
            "2 messages\n  Amir  \"4 pm still works\"\n  Bank  \"Payment received\""

        /** The assistant's line once the allowed call came back. */
        const val AUTO_SMS_TEXT = "Read your recent texts: 2 new, nothing urgent."

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

        /**
         * The bounds the daemon applies to one `session.fleet` read
         * (frame.rs:87). They are echoed in every snapshot so a bounded one can
         * say what it was bounded by.
         */
        const val NODE_LIMIT = 64
        const val DEPTH_LIMIT = 4
    }
}
