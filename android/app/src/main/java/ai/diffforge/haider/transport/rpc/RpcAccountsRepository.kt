package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.ui.accounts.AccountResult
import ai.diffforge.haider.ui.accounts.AuthKind
import ai.diffforge.haider.ui.accounts.OAuthStyle
import ai.diffforge.haider.ui.accounts.ModelDetail
import ai.diffforge.haider.ui.accounts.Account as UiAccount
import ai.diffforge.haider.ui.accounts.AccountsSnapshot as UiSnapshot
import ai.diffforge.haider.ui.accounts.ProviderDescriptor as UiProvider
import ai.diffforge.haider.ui.accounts.OAuthFlow as UiFlow
import ai.diffforge.haider.ui.accounts.OAuthStatus as UiStatus
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.Closeable
import java.io.IOException
import java.util.IdentityHashMap
import java.util.UUID

/** UI-named adapter over the same process/epoch-owned account socket as the session facade. */
class RpcAccountsRepository(private val client: RpcClient, private val source: AccountsRepository, scope: CoroutineScope,
    private val nowMs: () -> Long = System::currentTimeMillis) :
    ai.diffforge.haider.ui.accounts.AccountsRepository, Closeable {
    private val job = SupervisorJob(scope.coroutineContext[Job])
    private val owner = CoroutineScope(scope.coroutineContext + job)
    private val mutations = Mutex()
    private val commands = mutableMapOf<String, String>()
    private data class Owned(val flow: OAuthFlow, val completionId: String = UUID.randomUUID().toString())
    private val flows = IdentityHashMap<UiFlow.Started, Owned>()
    private class Staged(val epoch: Long, val expiresAtMs: Long, var operation: String? = null)
    private val staged = mutableMapOf<String, Staged>()
    override val providers = source.providers.map { rows -> rows.map(::provider) }
        .stateIn(owner, SharingStarted.Eagerly, source.providers.value.map(::provider))
    override val snapshot = source.snapshot.map { value -> UiSnapshot(value.revision, value.accounts.map(::account)) }
        .stateIn(owner, SharingStarted.Eagerly, UiSnapshot(source.snapshot.value.revision, source.snapshot.value.accounts.map(::account)))
    override val loadError = source.loadError
    init { owner.launch {
        combine(client.state, client.connectionEpochs) { state, epoch -> state to epoch }.collect { (state, epoch) ->
            synchronized(flows) { flows.entries.removeAll { state != RpcConnectionState.CONNECTED || it.value.flow.epoch != epoch } }
            synchronized(staged) { staged.entries.removeAll { state != RpcConnectionState.CONNECTED || it.value.epoch != epoch } }
        }
    } }
    override suspend fun refresh() = source.refresh()
    override suspend fun refreshProviders() = source.refreshProviders()
    private suspend fun mutation(key: String, action: suspend (String) -> Unit): AccountResult = mutations.withLock {
        if (key !in commands && commands.size >= 64) return@withLock AccountResult.Failed("unresolved_commands_limit")
        val id = commands.getOrPut(key) { UUID.randomUUID().toString() }
        try { action(id); commands.remove(key); AccountResult.Ok }
        catch (cancelled: CancellationException) { throw cancelled }
        catch (error: RpcRemoteException) { if (!error.retryable && error.code != "restage_required") commands.remove(key); AccountResult.Failed(error.code) }
        catch (_: IOException) { AccountResult.Failed("connection_lost") }
    }
    override suspend fun addApiKey(provider: String, alias: String?, apiKey: CharArray, replaceExisting: Boolean): AccountResult {
        try {
            return mutation(operationKey("api_key", provider, alias.orEmpty(), replaceExisting.toString())) {
                source.addApiKey(provider, alias, apiKey, it, replaceExisting = replaceExisting)
            }
        } finally { apiKey.fill('\u0000') }
    }
    override suspend fun validateApiKey(provider: String, apiKey: CharArray): AccountResult {
        apiKey.fill('\u0000')
        return AccountResult.Failed("validate_only_unavailable")
    }
    override suspend fun stageApiKey(apiKey: CharArray): String? = try {
        val epoch = client.connectionEpoch
        synchronized(staged) { sweepStages(); if (staged.size >= 64) throw IOException("staged_secret_limit") }
        val stage = source.stageApiKey(apiKey, epoch)
        synchronized(staged) {
            if (epoch != client.connectionEpoch || client.state.value != RpcConnectionState.CONNECTED) throw IOException("staged_secret_lost")
            sweepStages()
            if (stage.expiresAtMs <= nowMs()) throw IOException("staged_secret_lost")
            if (staged.size >= 64) throw IOException("staged_secret_limit")
            staged[stage.reference] = Staged(epoch, stage.expiresAtMs)
        }
        stage.reference
    } catch (cancelled: CancellationException) { throw cancelled }
    catch (_: IOException) { null }
    finally { apiKey.fill('\u0000') }

    /** Called under staged's monitor; expiry never discards a semantic command receipt. */
    private fun sweepStages() {
        val now = nowMs()
        val epoch = client.connectionEpoch
        staged.entries.removeAll { it.value.expiresAtMs <= now || it.value.epoch != epoch }
    }

    override suspend fun commitStagedApiKey(provider: String, alias: String?, vaultReference: String, replaceExisting: Boolean): AccountResult {
        val key = operationKey("staged_api_key", provider, alias.orEmpty(), replaceExisting.toString())
        // A locally lost capability must not discard a prior ambiguous command receipt.
        synchronized(staged) {
            sweepStages()
            val value = staged[vaultReference] ?: return AccountResult.Failed("staged_secret_lost")
            if (value.epoch != client.connectionEpoch) return AccountResult.Failed("staged_secret_lost")
            if (value.operation != null && value.operation != key) return AccountResult.Failed("staged_secret_mismatch")
        }
        return mutation(key) { command ->
            val epoch = synchronized(staged) {
                sweepStages()
                val value = staged[vaultReference] ?: throw RpcRemoteException("staged_secret_lost", retryable = true)
                if (value.epoch != client.connectionEpoch) throw RpcRemoteException("staged_secret_lost", retryable = true)
                if (value.operation != null && value.operation != key) throw RpcRemoteException("staged_secret_mismatch", retryable = true)
                // A fresh stage supersedes the prior single-use reference for this same command.
                staged.entries.removeAll { it.key != vaultReference && it.value.operation == key }
                value.operation = key
                value.epoch
            }
            try {
                source.commitStagedApiKey(provider, alias, vaultReference, command, epoch, replaceExisting = replaceExisting)
                synchronized(staged) { staged.entries.removeAll { it.value.operation == key } }
            } catch (error: RpcRemoteException) {
                if (!error.retryable || error.code == "restage_required") synchronized(staged) { staged.remove(vaultReference) }
                throw error
            }
        }
    }
    override suspend fun remove(alias: String): AccountResult {
        val revision = source.snapshot.value.revision ?: return AccountResult.Failed("account_revision_unavailable")
        return mutation(operationKey("remove", alias, revision.toString())) { source.remove(alias, it, revision) }
    }
    override suspend fun setActive(alias: String): AccountResult = mutation(operationKey("active", alias)) {
        // The existing UI has no confirmation coordinate. Never silently assert consent.
        source.setActive(alias, it, confirmNewEpoch = false)
    }
    override suspend fun startOAuth(provider: String, desiredAlias: String?, attemptId: String): UiFlow {
        if (attemptId.isBlank()) return UiFlow.Unavailable(provider, "invalid_attempt_id")
        // The wire requires an alias; a new caller attempt owns this valid generated alias.
        val alias = desiredAlias?.takeIf { it.isNotBlank() } ?: "android-${operationKey(provider, attemptId).take(16)}"
        try {
            synchronized(flows) { sweepFlows(); if (flows.size >= 64) throw IOException("oauth_flow_limit") }
            val flow = source.startOAuth(provider, alias, attemptId)
            val ui = UiFlow.Started(provider, alias, flow.flowId, attemptId, OAuthStyle.Unknown,
                flow.authorizationUrl, flow.userCode, flow.expiresAtMs)
            synchronized(flows) {
                sweepFlows()
                if (flow.epoch != client.connectionEpoch || flow.expiresAtMs?.let { it <= nowMs() } == true) throw IOException("connection_lost")
                if (flows.size >= 64) throw IOException("oauth_flow_limit")
                flows[ui] = Owned(flow)
            }
            return ui
        } catch (cancelled: CancellationException) { throw cancelled }
        catch (error: RpcRemoteException) { return UiFlow.Unavailable(provider, error.code) }
        catch (_: IOException) { return UiFlow.Unavailable(provider, "connection_lost") }
    }
    /** Called under flows' monitor. Ready flows retain their completion ID until resolution or expiry. */
    private fun sweepFlows() {
        val now = nowMs()
        val epoch = client.connectionEpoch
        flows.entries.removeAll { it.value.flow.epoch != epoch || it.value.flow.expiresAtMs?.let { expiry -> expiry <= now } == true }
    }
    private fun owned(flow: UiFlow.Started): Owned? = synchronized(flows) {
        sweepFlows()
        flows[flow]?.takeIf { it.flow.epoch == client.connectionEpoch && client.state.value == RpcConnectionState.CONNECTED }
    }
    override suspend fun pollOAuth(flow: UiFlow.Started): UiStatus {
        val owned = owned(flow) ?: return UiStatus.Lost
        return try { status(source.pollOAuth(owned.flow)).also { result ->
            if (result is UiStatus.Failed) synchronized(flows) { flows.remove(flow) }
        } }
        catch (cancelled: CancellationException) { throw cancelled }
        catch (error: RpcRemoteException) { synchronized(flows) { flows.remove(flow) }; UiStatus.Failed(error.code, "failed") }
        catch (_: IOException) { synchronized(flows) { flows.remove(flow) }; UiStatus.Lost }
    }
    override suspend fun completeOAuth(flow: UiFlow.Started, oauthReference: String): AccountResult {
        val owned = owned(flow) ?: return AccountResult.Failed("oauth_flow_lost")
        return try {
            source.completeOAuth(owned.flow, oauthReference, owned.completionId)
            synchronized(flows) { flows.remove(flow) }
            AccountResult.Ok
        } catch (cancelled: CancellationException) { throw cancelled }
        catch (error: RpcRemoteException) {
            if (!error.retryable) synchronized(flows) { flows.remove(flow) }
            AccountResult.Failed(error.code)
        }
        catch (_: IOException) { AccountResult.Failed("connection_lost") }
    }
    override suspend fun cancelOAuth(flow: UiFlow.Started) {
        val owned = owned(flow) ?: return
        try { source.cancelOAuth(owned.flow) } finally { synchronized(flows) { flows.remove(flow) } }
    }
    override suspend fun accountExists(provider: String, alias: String): Boolean {
        source.refresh()
        if (source.loadError.value != null) throw IOException("accounts_unavailable")
        return source.snapshot.value.accounts.any { it.provider == provider && it.alias == alias }
    }
    override fun close() { synchronized(flows) { flows.clear() }; synchronized(staged) { staged.clear() }; job.cancel() }
    companion object {
        internal fun account(value: Account) = UiAccount(value.alias, value.provider, value.label,
            when (value.authKind) { "api_key" -> AuthKind.ApiKey; "oauth" -> AuthKind.OAuth; else -> AuthKind.Unknown },
            value.active, value.identity, value.status)
        internal fun provider(value: ProviderDescriptor) = UiProvider(value.id, value.id, value.supportsApiKey, value.supportsOAuth,
            OAuthStyle.Unknown, value.available, value.unavailableReason, value.models,
            value.modelDetails.associate { it.id to ModelDetail(it.supportedEfforts, it.defaultEffort, it.contextWindow) }, value.defaultModel, value.apiFamily)
        internal fun status(value: OAuthStatus): UiStatus = when (value.status) {
            "waiting_browser", "waiting_device" -> UiStatus.Waiting
            "exchanging" -> UiStatus.Exchanging
            "ready" -> UiStatus.Ready(value.oauthReference ?: throw RpcProtocolException("missing_oauth_reference"), value.identity)
            "failed", "expired", "cancelled" -> UiStatus.Failed(value.publicCode, value.status)
            else -> UiStatus.Failed("unsupported_oauth_status", value.status)
        }
    }
}
