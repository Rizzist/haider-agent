package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.transport.*
import ai.diffforge.haider.ui.daemon.*
import ai.diffforge.haider.ui.state.PermissionMode
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.serialization.json.*
import java.io.Closeable
import java.io.File
import java.io.IOException
import java.util.UUID

/** Lane 2 supplies this port from its sequenced Binder snapshots; null revokes authority on Binder death. */
interface RpcControlPlane {
    val snapshots: StateFlow<DaemonServiceSnapshot?>
    val notificationsPermanentlyDenied: StateFlow<Boolean> get() = MutableStateFlow(false)
    suspend fun start()
    suspend fun stop()
    suspend fun restart()
    /** Android permission observation, not a new C2 Binder or RPC method. */
    suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean)
}

/** Process-scoped production UI facade. Own this alongside Binder, never in an Activity. */
class RpcDaemonService(
    scope: CoroutineScope, private val control: RpcControlPlane, cacheDirectory: File,
    private val workspaceCwd: String, private val defaultProvider: String, private val defaultModel: String,
    private val maxTokens: Long, val client: RpcClient = RpcClient(scope),
    private val nowMs: () -> Long = System::currentTimeMillis,
) : DaemonService, Closeable {
    private val ownerJob = SupervisorJob(scope.coroutineContext[Job])
    private val owner = CoroutineScope(scope.coroutineContext + ownerJob)
    private val targets = control.snapshots.map { snapshot ->
        try { RpcUiMapping.target(snapshot) } catch (_: RpcProtocolException) { null }
    }.stateIn(owner, SharingStarted.Eagerly, null)
    private val connection = StandaloneRpcConnection(owner, targets, client)
    private val cache = TranscriptCache(cacheDirectory)
    private val roster = SessionRosterRepository(client, owner, cache)
    private val replay = TranscriptRepository(client, owner, cache)
    private val accountSource = AccountsRepository(client, owner)
    val accounts: ai.diffforge.haider.ui.accounts.AccountsRepository = RpcAccountsRepository(client, accountSource, owner)
    private val commands = RpcCommands(client) { body ->
        val session = body.optionalString("session_id")
        if (session == null) client.request(body)
        else replay.withControlAttachment(session) { epoch -> client.request(body, epoch) }
    }
    private val selected = MutableStateFlow<String?>(null)
    private val catalog = MutableStateFlow<SessionConfig?>(null)
    private val inventory = MutableStateFlow(ProviderInventory())
    private val catalogFailure = MutableStateFlow<String?>(null)
    private val catalogTime = MutableStateFlow<Long?>(null)
    private data class SecretCoordinates(val epoch: Long, val session: String, val menu: String, val seq: Long, val generation: Long)
    private class SecretOwner(val coordinates: SecretCoordinates, val expiresAtMs: Long)
    private val secrets = mutableMapOf<String, SecretOwner>()
    override val sessions = roster.sessions.map { it.map(RpcUiMapping::row) }.stateIn(owner, SharingStarted.Eagerly, roster.sessions.value.map(RpcUiMapping::row))
    override val rosterReady = combine(roster.ready, client.state) { _, _ -> roster.isReady() }
        .stateIn(owner, SharingStarted.Eagerly, false)
    override val paging = roster.loading.map { RosterPaging(loading = it) }.stateIn(owner, SharingStarted.Eagerly, RosterPaging())
    override val activeSessionId: StateFlow<String?> = selected.asStateFlow()
    // Existing interactive sessions preserve daemon approval gates. The frozen
    // RPC contract has no mutation for the UI's new standing-consent mode.
    override val permissionMode: StateFlow<PermissionMode> = MutableStateFlow(PermissionMode.Ask).asStateFlow()
    override val supportedPermissionModes = setOf(PermissionMode.Ask)
    override suspend fun setPermissionMode(mode: PermissionMode) {
        if (mode !in supportedPermissionModes) throw IOException("permission_mode_unavailable")
    }
    // C4 is an immutable platform ceiling, independent of provider/session grants.
    override val shell: StateFlow<ShellAvailability> =
        MutableStateFlow(ShellAvailability(available = false, reason = "process_exec_disabled")).asStateFlow()
    override val models: StateFlow<SessionConfig?> = catalog.asStateFlow()
    override val providers: StateFlow<ProviderInventory> = inventory.asStateFlow()
    override val catalogError: StateFlow<String?> = catalogFailure.asStateFlow()
    override val catalogRequestedAtMs: StateFlow<Long?> = catalogTime.asStateFlow()
    override val environment = combine(control.snapshots, control.notificationsPermanentlyDenied) { snapshot, denied ->
        (snapshot?.let(DaemonSnapshotMapping::toEnvironment) ?: DaemonEnvironment(network = NetworkState.Unknown))
            .copy(notificationsPermanentlyDenied = denied)
    }
        .stateIn(owner, SharingStarted.Eagerly, DaemonEnvironment(network = NetworkState.Unknown))
    override val status = combine(control.snapshots, client.state, sessions, roster.ready) { snapshot, state, rows, ready ->
        if (snapshot?.phase == DaemonPhase.Ready && state == RpcConnectionState.CONNECTED &&
            (!ready || !roster.isReady() || rows != roster.sessions.value.map(RpcUiMapping::row))) DaemonStatus.Starting else snapshot?.let { DaemonSnapshotMapping.toStatus(it, RpcUiMapping.dataPlane(state), rows.size.toLong(),
            rows.count { row -> row.runState == "waiting_for_route" }.toLong()) } ?: DaemonStatus.Stopped
    }.stateIn(owner, SharingStarted.Eagerly, DaemonStatus.Stopped)
    private val indexValidity = combine(roster.loadError, roster.loading, client.state, roster.ready) { error, loading, state, ready ->
        error == null && !loading && ready && roster.isReady() && state == RpcConnectionState.CONNECTED
    }
    override val searchIndex = combine(replay.coverage, roster.sessions, indexValidity, replay.revision) { _, rows, valid, _ ->
        val indexed = rows.count { row -> cache.lastApplied(row.sessionId) == row.headSeq &&
            cache.entries(row.sessionId).none { it.display.optionalString("type") == "unrendered" } }
        SearchIndexState(indexed, rows.size, indexed == rows.size && valid)
    }.stateIn(owner, SharingStarted.Eagerly, SearchIndexState())

    init {
        owner.launch {
            combine(accountSource.providers, accountSource.providerRevision, roster.sessions, selected, accountSource.loadError) { _, _, _, _, _ -> Unit }
                .collect { updateCatalog() }
        }
        owner.launch {
            combine(client.state, client.connectionEpochs) { state, epoch -> state to epoch }.collect { (state, epoch) ->
                synchronized(secrets) { secrets.entries.removeAll { state != RpcConnectionState.CONNECTED || it.value.coordinates.epoch != epoch } }
            }
        }
    }
    override suspend fun start() = control.start()
    override suspend fun stop() = control.stop()
    override suspend fun restart() = control.restart()
    override suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean) =
        control.reportNotificationPermission(granted, permanentlyDenied)
    override suspend fun refreshRoster() = roster.refreshRoster()
    /** The repository eagerly loads every page during refresh; there is no pending cursor. */
    override suspend fun loadMoreSessions() = Unit
    private fun summary(id: String) = roster.sessions.value.firstOrNull { it.sessionId == id } ?: throw IOException("session_unavailable")
    private fun at(id: String) = summary(id).let { SessionCoordinate(id, it.workerGeneration) }
    private suspend fun mutation(key: String, body: (String) -> JsonObject): RpcResponses.Receipt {
        val receipt = RpcResponses.receipt(commands.execute(key, body))
        refreshRoster()
        return receipt
    }
    override suspend fun createSession(model: String?, effort: String?): String {
        if (!roster.isReady()) throw IOException("roster_not_ready")
        val current = selected.value?.let { id -> roster.sessions.value.firstOrNull { it.sessionId == id } }
        val provider = current?.provider ?: defaultProvider
        val chosenModel = model ?: current?.model ?: defaultModel
        val id = mutation(operationKey("create", provider, chosenModel, effort.orEmpty())) {
            RpcMethods.create(it, workspaceCwd, provider, chosenModel, maxTokens)
        }.sessionId
        activate(id)
        if (effort != null) selectEffort(effort)
        return id
    }
    override suspend fun activate(sessionId: String) {
        replay.attach(sessionId, control = true)
        val previous = selected.value
        selected.value = sessionId
        if (previous != null && previous != sessionId) replay.detach(previous)
        refreshRoster()
        refreshProviders()
    }
    override suspend fun markSeen(sessionId: String) { mutation(operationKey("seen", sessionId)) { RpcMethods.seen(it, at(sessionId)) } }
    override suspend fun rename(sessionId: String, title: String) { mutation(operationKey("rename", sessionId, title)) { RpcMethods.rename(it, at(sessionId), title) } }
    override suspend fun fork(sessionId: String): String {
        val digest = roster.observe(sessionId)
        val cut = RpcResponses.forkCut(digest)
        val node = cut.nodeId ?: throw IOException("fork_coordinates_unavailable")
        val seq = cut.seq
        if (seq == 0L) throw IOException("fork_coordinates_unavailable")
        return mutation(operationKey("fork", sessionId)) {
            RpcMethods.fork(it, cut.session, node, seq)
        }.sessionId.also { activate(it) }
    }
    override suspend fun stopTurn(sessionId: String) {
        val row = RpcUiMapping.row(summary(sessionId))
        val coordinates = TurnCancel.coordinates(row) ?: throw MissingRunCoordinates(sessionId)
        mutation(operationKey("cancel", sessionId, coordinates.runId)) {
            RpcMethods.cancel(it, SessionCoordinate(sessionId, coordinates.workerGeneration), coordinates.runId)
        }
    }
    override suspend fun answer(coordinates: MenuCoordinates, optionKey: String, optionIndex: Int, input: MenuAnswerInput?) {
        replay.withControlAttachment(coordinates.sessionId) { epoch ->
            val wanted = SecretCoordinates(epoch, coordinates.sessionId, coordinates.menuId, coordinates.requestSeq, coordinates.workerGeneration)
            val encoded = when (input) {
                is MenuAnswerInput.Text -> RpcMethods.textInput(input.text)
                is MenuAnswerInput.Secret -> {
                    if (synchronized(secrets) { sweepSecrets(); secrets[input.vaultReference]?.coordinates } != wanted) throw IOException("staged_secret_lost")
                    RpcMethods.secretInput(input.vaultReference)
                }
                null -> null
            }
            try {
                RpcResponses.menuAnswer(client.menuAnswer(RpcMethods.menu(coordinates.commandId,
                    MenuCoordinate(SessionCoordinate(coordinates.sessionId, coordinates.workerGeneration), coordinates.menuId,
                        coordinates.requestSeq, optionKey, optionIndex), encoded), epoch))
                synchronized(secrets) { secrets.entries.removeAll { it.value.coordinates == wanted } }
            } catch (error: RpcRemoteException) {
                if (input is MenuAnswerInput.Secret && (!error.retryable || error.code == "restage_required"))
                    synchronized(secrets) { secrets.remove(input.vaultReference) }
                throw error
            }
        }
        refreshRoster()
    }
    override suspend fun stageMenuSecret(secret: CharArray): String {
        try {
            val id = selected.value ?: throw IOException("no_active_session")
            val needs = RpcUiMapping.row(summary(id)).needsInput
            val coordinates = MenuCoordinates.of(id, needs, "stage") ?: throw IOException("menu_coordinates_unavailable")
            if (needs?.secretAnswer != true) throw IOException("not_a_secret_menu")
            val epoch = client.connectionEpoch
            synchronized(secrets) { sweepSecrets(); if (secrets.size >= 64) throw IOException("staged_secret_limit") }
            val stage = RpcResponses.stage(client.request(RpcMethods.stage(UUID.randomUUID().toString(), "menu_secret", String(secret)), epoch))
            synchronized(secrets) {
                sweepSecrets()
                if (epoch != client.connectionEpoch || stage.expiresAtMs <= nowMs()) throw IOException("staged_secret_lost")
                if (secrets.size >= 64) throw IOException("staged_secret_limit")
                secrets[stage.reference] = SecretOwner(SecretCoordinates(epoch, id, coordinates.menuId, coordinates.requestSeq, coordinates.workerGeneration), stage.expiresAtMs)
            }
            return stage.reference
        } finally { secret.fill('\u0000') }
    }
    /** Called under secrets' monitor. Expired references cannot own a later menu answer. */
    private fun sweepSecrets() {
        val now = nowMs()
        val epoch = client.connectionEpoch
        secrets.entries.removeAll { it.value.expiresAtMs <= now || it.value.coordinates.epoch != epoch }
    }
    override suspend fun selectModel(provider: String, model: String, confirmNewEpoch: Boolean) {
        val id = selected.value ?: throw IOException("no_active_session")
        mutation(operationKey("model", id, provider, model, confirmNewEpoch.toString())) { RpcMethods.selectModel(it, at(id), provider, model, confirmNewEpoch) }
    }
    override suspend fun selectEffort(effort: String?, confirmNewEpoch: Boolean) {
        val id = selected.value ?: throw IOException("no_active_session")
        mutation(operationKey("effort", id, effort.orEmpty(), confirmNewEpoch.toString())) { RpcMethods.selectEffort(it, at(id), effort, confirmNewEpoch) }
    }
    override suspend fun selectProvider(provider: String) {
        val model = accountSource.providers.value.firstOrNull { it.id == provider }?.defaultModel ?: throw IOException("provider_default_model_unavailable")
        selectModel(provider, model)
    }
    override suspend fun refreshModels() {
        val provider = selected.value?.let(::summary)?.provider ?: throw IOException("provider_unavailable")
        RpcResponses.refreshedProvider(client.request(RpcMethods.refreshModels(provider)))
        refreshProviders()
    }
    override suspend fun refreshProviders() {
        catalogTime.value = System.currentTimeMillis()
        try { accountSource.refreshProviders(); updateCatalog(); catalogFailure.value = null }
        catch (cancelled: CancellationException) { throw cancelled }
        catch (_: Exception) { catalogFailure.value = "catalog_unavailable" }
    }
    private fun updateCatalog() {
        val rows = accountSource.providers.value
        val revision = accountSource.providerRevision.value
        val current = selected.value?.let { id -> roster.sessions.value.firstOrNull { it.sessionId == id } }
        inventory.value = ProviderInventory(rows.map { provider -> ProviderOption(provider.id, provider.id,
            provider.models.map { name ->
                val detail = provider.modelDetails.firstOrNull { it.id == name }
                ModelOption(name, detail?.supportedEfforts.orEmpty(), detail?.defaultEffort, detail?.contextWindow)
            }, provider.defaultModel, provider.available, provider.unavailableReason) }, revision, accountSource.loadError.value)
        catalog.value = if (revision == null || current?.provider == null || current.model == null) null else SessionConfig(
            revision, accountSource.loadError.value == null, accountSource.loadError.value,
            SessionSelection(current.sessionId, current.provider, current.model, current.canonical.optionalString("effort")),
            rows.map { provider -> SessionProvider(provider.id, provider.available, if (provider.available) "available" else "unavailable",
                provider.unavailableReason, provider.defaultModel, provider.models.map { name ->
                    provider.modelDetails.firstOrNull { it.id == name } ?: SessionModel(name, null, emptyList(), null)
                }) })
    }
    override suspend fun send(sessionId: String, text: String, attachments: List<Attachment>, mode: Delivery) {
        // UI parity added CAS attachments after this integration's frozen chat
        // seam. Refuse unsupported blocks before submitting any part of a turn.
        if (attachments.isNotEmpty()) throw TurnRefused("attachment_transport_unavailable")
        try {
            mutation(operationKey("send", sessionId, text, mode.wire)) {
                RpcMethods.submit(it, at(sessionId), text, mode = mode.wire)
            }
        } catch (error: RpcRemoteException) {
            throw TurnRefused(error.code)
        }
    }

    // Explicit unavailable snapshots for the UI parity doors that have no
    // production adapter yet, matching the facade's fleet/workflow defaults.
    override suspend fun stageAttachment(bytes: ByteArray, mime: String, name: String?): Attachment? = null
    override suspend fun attachmentBytes(artifact: String): ByteArray? = null
    override val queue: StateFlow<QueueSnapshot> =
        MutableStateFlow(QueueSnapshot(error = "queue_transport_unavailable")).asStateFlow()
    override suspend fun refreshQueue(sessionId: String) = Unit
    override suspend fun removeQueued(sessionId: String, id: String, revision: Long): Nothing =
        throw IOException("queue_transport_unavailable")
    override suspend fun promoteQueued(sessionId: String, id: String, revision: Long): Nothing =
        throw IOException("queue_transport_unavailable")
    override val usage: StateFlow<UsageSnapshot> = MutableStateFlow(UsageSnapshot()).asStateFlow()
    override suspend fun refreshUsage() = Unit
    private fun cachedTranscript(sessionId: String, error: String? = null): TranscriptLoad {
        val entries = replay.transcript(sessionId)
        val row = roster.sessions.value.firstOrNull { it.sessionId == sessionId }
            ?: return TranscriptLoad.Unavailable("session_unavailable")
        val messages = RpcUiMapping.messages(entries)
        val loaded = cache.lastApplied(sessionId)
        val reason = error ?: when {
            client.state.value != RpcConnectionState.CONNECTED -> "connection_lost"
            !roster.isReady() -> "roster_unavailable"
            entries.any { it.display.optionalString("type") == "unrendered" } -> "unsupported_display_events"
            loaded < row.headSeq -> "history_incomplete"
            else -> null
        }
        return if (reason == null) TranscriptLoad.Complete(messages)
        else TranscriptLoad.Partial(messages, reason, loaded, maxOf(row.headSeq, loaded))
    }
    override suspend fun transcript(sessionId: String): TranscriptLoad {
        try {
            replay.attach(sessionId, control = true)
            replay.indexAll(listOf(summary(sessionId)))
            return cachedTranscript(sessionId, replay.coverage.value.error)
        } catch (cancelled: CancellationException) { throw cancelled }
        catch (_: Exception) { return cachedTranscript(sessionId, "history_unavailable") }
    }
    override fun transcriptUpdates(sessionId: String): Flow<TranscriptLoad> = flow {
        emit(transcript(sessionId))
        emitAll(combine(replay.revision, roster.sessions, client.state, roster.ready) { _, _, _, _ -> cachedTranscript(sessionId) })
    }.distinctUntilChanged()
    override suspend fun search(query: String): SearchOutcome {
        val rows = roster.sessions.value
        replay.indexAll(rows)
        val coverage = replay.coverage.value
        val index = SearchIndexState(coverage.indexedSessions, coverage.totalSessions, coverage.complete && roster.isReady() && roster.loadError.value == null && !roster.loading.value && client.state.value == RpcConnectionState.CONNECTED && rows == roster.sessions.value)
        return SearchOutcome(replay.search(query, rows).map { SearchHit(it.sessionId, it.text, it.seq) }, index, index.complete)
    }
    override fun close() {
        (accounts as? Closeable)?.close(); accountSource.close(); roster.close(); replay.close(); connection.close()
        synchronized(secrets) { secrets.clear() }; ownerJob.cancel()
    }
}
