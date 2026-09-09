package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.transport.*
import ai.diffforge.haider.ui.daemon.*
import ai.diffforge.haider.ui.loom.LoomRegistry
import ai.diffforge.haider.ui.loom.LoomRpcAdapter
import ai.diffforge.haider.ui.state.PermissionMode
import ai.diffforge.haider.ui.state.StandingConsent
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
    /** Locally retained preview bytes for the blocks this connection staged. */
    private val staged = linkedMapOf<String, ByteArray>()
    /**
     * Menus this client has already spent its standing consent on, this epoch.
     *
     * Keyed by session as well as menu and request sequence: a menu id is only
     * unique within the session that raised it.
     */
    private val consented = java.util.Collections.synchronizedSet(mutableSetOf<Triple<String, String, Long>>())
    override val sessions = roster.sessions.map { it.map(RpcUiMapping::row) }.stateIn(owner, SharingStarted.Eagerly, roster.sessions.value.map(RpcUiMapping::row))
    override val rosterReady = combine(roster.ready, client.state) { _, _ -> roster.isReady() }
        .stateIn(owner, SharingStarted.Eagerly, false)
    override val paging = roster.loading.map { RosterPaging(loading = it) }.stateIn(owner, SharingStarted.Eagerly, RosterPaging())
    override val activeSessionId: StateFlow<String?> = selected.asStateFlow()
    // The frozen RPC contract has no permission-mode mutation, so Auto is what
    // it always claimed to be: standing consent held by THIS client and spent
    // by answering the exact device-capability approvals StandingConsent
    // covers. Nothing pretends the daemon stopped asking; the answer is real,
    // it is the same menu answer a person would send, and `sms.send` and every
    // unrecognised card still stop for a human (971-V F3).
    private val mode = MutableStateFlow(PermissionMode.Ask)
    override val permissionMode: StateFlow<PermissionMode> = mode.asStateFlow()
    override val supportedPermissionModes = PermissionMode.entries.toSet()
    override suspend fun setPermissionMode(mode: PermissionMode) {
        if (mode !in supportedPermissionModes) throw IOException("permission_mode_unavailable")
        this.mode.value = mode
        applyStandingConsent()
    }

    /**
     * Answers every card standing consent covers, once each.
     *
     * The coordinates come from the same roster snapshot that carried the card,
     * and [answer] is the ordinary compare-and-set door: a card answered
     * elsewhere loses the race and is dropped rather than retried.
     */
    private suspend fun applyStandingConsent() {
        if (mode.value != PermissionMode.Auto) return
        roster.sessions.value.map(RpcUiMapping::row).forEach { row ->
            val choice = StandingConsent.choice(mode.value, row.needsInput) ?: return@forEach
            val coordinates = MenuCoordinates.of(row.id, row.needsInput, "auto-${row.id}-${row.needsInput?.menuId}")
                ?: return@forEach
            if (!consented.add(Triple(row.id, coordinates.menuId, coordinates.requestSeq))) return@forEach
            try { answer(coordinates, choice.key, choice.index, null) }
            catch (cancelled: CancellationException) { throw cancelled }
            catch (_: Exception) { /* A refused or already-resolved menu is the daemon's answer, not a retry. */ }
        }
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
                // A menu id belongs to one worker generation on one connection;
                // spent consent is not carried into a new epoch, and neither
                // are preview bytes for a CAS address from the old one.
                consented.clear()
                synchronized(staged) { staged.clear() }
                // A restarted daemon reconnects on the same socket path, so the
                // Binder target never changes and nothing re-read the session
                // the UI is showing: the roster repository re-hydrates itself,
                // but the control attachment, catalog and transcript stream did
                // not, which is what left Starting/Running on screen until the
                // app was killed (971-V F4). Re-observe here instead.
                if (state == RpcConnectionState.CONNECTED) reobserve()
            }
        }
        owner.launch { roster.sessions.collect { applyStandingConsent() } }
    }

    /**
     * Re-establishes everything that belongs to a connection, in the order the
     * UI reads it: roster, then the active session's control attachment, then
     * the catalog. Every step is allowed to fail without taking the others
     * down — a reconnect is not the place to raise.
     */
    private suspend fun reobserve() {
        step { refreshRoster() }
        selected.value?.let { id -> step { replay.attach(id, control = true) } }
        step { refreshProviders() }
    }

    /** Runs one recovery step; cancellation still propagates, a refusal does not. */
    private suspend inline fun step(action: suspend () -> Unit) {
        try { action() }
        catch (cancelled: CancellationException) { throw cancelled }
        catch (_: Exception) { /* Each step reports through its own state flow. */ }
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
            }, provider.defaultModel, provider.available, provider.unavailableReason,
            // An advisory catalog is what licenses the picker's free-text model
            // id. Constructing the option without it silently removed custom
            // model entry from every provider (971-V F6).
            provider.inventoryAuthority) }, revision, accountSource.loadError.value)
        catalog.value = if (revision == null || current?.provider == null || current.model == null) null else SessionConfig(
            revision, accountSource.loadError.value == null, accountSource.loadError.value,
            SessionSelection(current.sessionId, current.provider, current.model, current.canonical.optionalString("effort")),
            rows.map { provider -> SessionProvider(provider.id, provider.available, if (provider.available) "available" else "unavailable",
                provider.unavailableReason, provider.defaultModel, provider.models.map { name ->
                    provider.modelDetails.firstOrNull { it.id == name } ?: SessionModel(name, null, emptyList(), null)
                }, provider.inventoryAuthority) })
    }
    override suspend fun send(sessionId: String, text: String, attachments: List<Attachment>, mode: Delivery) {
        try {
            mutation(operationKey("send", sessionId, text, mode.wire,
                attachments.joinToString(",") { it.artifact })) {
                RpcMethods.submit(it, at(sessionId), text, mode = mode.wire, attachments = attachments)
            }
        } catch (error: RpcRemoteException) {
            throw TurnRefused(error.code)
        }
    }

    /**
     * `artifact.put` puts the bytes in the daemon's own CAS and answers with
     * the address the turn then names; nothing but the address rides on
     * `turn.submit` (`AttachmentBlock` carries a ref, never bytes).
     *
     * Only the kinds this client can state truthfully are built. A PDF's
     * `pages` is a daemon-verified page-tree count, so a PDF is refused here
     * rather than submitted with a page count the phone invented.
     */
    override suspend fun stageAttachment(bytes: ByteArray, mime: String, name: String?): Attachment {
        if (bytes.size > ARTIFACT_MAX_BYTES) throw AttachmentRefused(AttachmentLimits.ARTIFACT_TOO_LARGE)
        if (!mime.startsWith("image/") && !mime.startsWith("text/"))
            throw AttachmentRefused(AttachmentLimits.KIND_UNSUPPORTED)
        val reference = try {
            RpcResponses.artifact(client.request(
                RpcMethods.putArtifact(java.util.Base64.getEncoder().encodeToString(bytes)))).reference
        } catch (cancelled: CancellationException) { throw cancelled }
        catch (error: RpcRemoteException) { throw AttachmentRefused(error.code) }
        catch (_: Exception) { throw AttachmentRefused("connection_lost") }
        val block = if (mime.startsWith("image/")) Attachment.Image(reference, mime)
            else Attachment.TextFile(reference, basename(name) ?: "attachment.txt", lines(bytes))
        // The composer's own thumbnail: there is no artifact GET door on this
        // wire, so the bytes it just handed over are kept for the preview of
        // the draft they belong to and dropped with the connection.
        synchronized(staged) {
            if (staged.size >= STAGED_PREVIEW_LIMIT) staged.remove(staged.keys.first())
            staged[block.artifact] = bytes
        }
        return block
    }

    override suspend fun attachmentBytes(artifact: String): ByteArray? = synchronized(staged) { staged[artifact] }

    /** Sanitised BASENAME only, never a path (`AttachmentBlock::File`, tool.rs:399). */
    private fun basename(name: String?): String? = name?.substringAfterLast('/')
        ?.filterNot { it < ' ' }?.take(120)?.takeIf(String::isNotBlank)

    private fun lines(bytes: ByteArray): Int =
        String(bytes, Charsets.UTF_8).count { it == '\n' }.let { if (bytes.isEmpty()) 0 else it + 1 }
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
        // Naming the families it could not draw is the graceful summary the bare
        // code was not: "unsupported_display_events" alone told a person nothing
        // about what was missing from the transcript in front of them (971-V F7).
        val unrendered = entries.mapNotNull { entry ->
            entry.display.optionalString("family")?.takeIf { entry.display.optionalString("type") == "unrendered" }
        }.distinct().sorted()
        val reason = error ?: when {
            client.state.value != RpcConnectionState.CONNECTED -> "connection_lost"
            !roster.isReady() -> "roster_unavailable"
            entries.any { it.display.optionalString("type") == "unrendered" } ->
                if (unrendered.isEmpty()) "unsupported_display_events"
                else "unsupported_display_events(${unrendered.joinToString(", ")})"
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
    // ---------- Looms ----------

    /**
     * Null when the daemon's own Hello advertises `loom_v1`.
     *
     * The screen printed "the daemon does not offer loom_v1" while the actual
     * Hello advertised it, because this facade never overrode the interface's
     * unavailable default (971-V F8). The feature set the daemon sent is the
     * authority; an unread Welcome is honestly unavailable, not honestly empty.
     */
    override suspend fun loomUnavailable(): String? {
        val features = client.welcome.value?.features ?: return LoomRpcAdapter.FEATURE_LOOM_V1
        return if (LoomRpcAdapter.FEATURE_LOOM_V1 in features) null else LoomRpcAdapter.FEATURE_LOOM_V1
    }

    override suspend fun loomList(includeArchived: Boolean): LoomRegistry? =
        LoomRpcAdapter.parseList(bridge(client.request(RpcMethods.loomList(includeArchived))), includeArchived)

    /** kotlinx on the socket, `org.json` in the wire adapters the UI lane owns. */
    private fun bridge(body: JsonObject) = org.json.JSONObject(body.toString())

    override fun close() {
        (accounts as? Closeable)?.close(); accountSource.close(); roster.close(); replay.close(); connection.close()
        synchronized(secrets) { secrets.clear() }; synchronized(staged) { staged.clear() }; consented.clear()
        ownerJob.cancel()
    }

    private companion object {
        /** `ARTIFACT_PUT_MAX_BYTES` (frame.rs:85), applied before base64 expansion. */
        const val ARTIFACT_MAX_BYTES = 33 * 1024 * 1024

        /** How many draft previews are retained; a composer holds a handful, not a gallery. */
        const val STAGED_PREVIEW_LIMIT = 16
    }
}
