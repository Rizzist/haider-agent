package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.*
import java.io.Closeable
import java.util.UUID

/** Public account descriptors only. OAuth capabilities are deliberately absent. */
data class Account(val alias: String, val provider: String, val label: String?, val authKind: String,
    val active: Boolean, val identity: String?, val status: String)
data class ProviderDescriptor(val id: String, val supportsApiKey: Boolean, val supportsOAuth: Boolean,
    val available: Boolean, val models: List<String>, val defaultModel: String?)
data class AccountsSnapshot(val revision: Long?, val accounts: List<Account>)

/** Transient connection-owned capability; never use in saved-state, logs, Binder or notifications. */
class OAuthFlow internal constructor(val provider: String, val alias: String, val flowId: String,
    val attemptId: String, val authorizationUrl: String?, val userCode: String?, val expiresAtMs: Long?,
    internal val epoch: Long) {
    override fun toString() = "OAuthFlow(redacted)"
}
class OAuthStatus internal constructor(val status: String, val oauthReference: String?, val identity: String?) {
    override fun toString() = "OAuthStatus(redacted)"
}

/** Same client as the session UI: account/OAuth operations consume no additional socket. */
class AccountsRepository(private val client: RpcClient, scope: CoroutineScope) : AccountsDataSource, Closeable {
    private val refreshMutex = Mutex()
    private var watchedEpoch: Long? = null
    private val _providers = MutableStateFlow<List<ProviderDescriptor>>(emptyList())
    private val _snapshot = MutableStateFlow(AccountsSnapshot(null, emptyList()))
    private val _error = MutableStateFlow<String?>(null)
    override val providers: StateFlow<List<ProviderDescriptor>> = _providers.asStateFlow()
    override val snapshot: StateFlow<AccountsSnapshot> = _snapshot.asStateFlow()
    override val loadError: StateFlow<String?> = _error.asStateFlow()
    private val refreshes = kotlinx.coroutines.channels.Channel<Unit>(kotlinx.coroutines.channels.Channel.CONFLATED)
    private val listener = client.observeFrames { if (it.string("kind") == "accounts_changed") refreshes.trySend(Unit) }
    private val worker = scope.launch { for (ignored in refreshes) refresh() }
    private val reconnect = scope.launch { client.state.collect { if (it == RpcConnectionState.CONNECTED) refreshes.trySend(Unit) } }

    override suspend fun refresh() = refreshMutex.withLock {
        try {
            val epoch = client.connectionEpoch
            if (watchedEpoch != epoch) {
                val watch = client.request(RpcMethods.watchAccounts(), epoch)
                if (watch["accepted"] != JsonPrimitive(true)) throw RpcProtocolException("watch_rejected")
                watchedEpoch = epoch
            }
            val providers = client.request(RpcMethods.providers(), epoch)
            val accounts = client.request(RpcMethods.accounts(), epoch)
            checkAvailability(providers)
            checkAvailability(accounts)
            val nextProviders = providers.objects("providers").map(::parseProvider)
            val nextAccounts = AccountsSnapshot(accounts.optionalNumber("revision"), accounts.objects("descriptors").map(::parseAccount))
            if (client.connectionEpoch != epoch) throw java.io.IOException("connection_lost")
            _providers.value = nextProviders
            _snapshot.value = nextAccounts
            _error.value = null
        } catch (cancelled: CancellationException) { throw cancelled }
        catch (_: Exception) { _error.value = "accounts_unavailable" }
    }

    /** login_api validates AND commits. A caller retains commandId to resolve a lost response safely. */
    override suspend fun addApiKey(provider: String, alias: String?, apiKey: CharArray, commandId: String,
        validationModel: String?, replaceExisting: Boolean): Account {
        val epoch = client.connectionEpoch
        try {
            val stage = client.request(RpcMethods.stage(UUID.randomUUID().toString(), "api_key", String(apiKey)), epoch)
            apiKey.fill('\u0000')
            val result = client.request(RpcMethods.loginApi(commandId, provider, alias, stage.string("vault_reference"), validationModel, replaceExisting), epoch)
            val descriptor = parseAccount(result.objectAt("descriptor"))
            refreshes.trySend(Unit)
            return descriptor
        } finally { apiKey.fill('\u0000') }
    }

    /** The frozen 136-method protocol has no validate-only door. Never silently add an account here. */
    override suspend fun validateApiKey(provider: String, apiKey: CharArray): Nothing {
        apiKey.fill('\u0000')
        throw RpcRemoteException("validate_only_unavailable")
    }

    override suspend fun remove(alias: String, commandId: String, expectedRevision: Long?) {
        client.request(RpcMethods.remove(commandId, alias, expectedRevision)); refreshes.trySend(Unit)
    }
    override suspend fun setActive(alias: String, commandId: String, confirmNewEpoch: Boolean) {
        client.request(RpcMethods.setActive(commandId, alias, confirmNewEpoch)); refreshes.trySend(Unit)
    }
    override suspend fun refreshAccount(alias: String) {
        client.request(RpcMethods.refreshAccount(alias)); refreshes.trySend(Unit)
    }
    override suspend fun startOAuth(provider: String, desiredAlias: String): OAuthFlow {
        val attempt = UUID.randomUUID().toString()
        val epoch = client.connectionEpoch
        val result = client.request(RpcMethods.oauthStart(provider, desiredAlias, attempt), epoch)
        if (result.objectAt("availability")["available"] != JsonPrimitive(true)) throw RpcRemoteException("oauth_unavailable")
        return OAuthFlow(provider, desiredAlias, result.string("flow_id"), attempt,
            result.optionalString("authorization_url"), result.optionalString("user_code"), result.optionalNumber("expires_at_ms"), epoch)
    }
    override suspend fun pollOAuth(flow: OAuthFlow): OAuthStatus {
        val result = client.request(RpcMethods.oauthStatus(flow.flowId, flow.attemptId), flow.epoch).objectAt("status")
        val status = result.string("status")
        return OAuthStatus(status, if (status == "ready") result.string("oauth_reference") else null, result.optionalString("identity"))
    }
    override suspend fun completeOAuth(flow: OAuthFlow, oauthReference: String, commandId: String): Account {
        val result = client.request(RpcMethods.addOAuth(commandId, flow.provider, flow.alias, flow.flowId, flow.attemptId, oauthReference), flow.epoch)
        val account = parseAccount(result.objectAt("descriptor"))
        refreshes.trySend(Unit)
        return account
    }
    override suspend fun cancelOAuth(flow: OAuthFlow) { client.request(RpcMethods.oauthCancel(flow.flowId, flow.attemptId), flow.epoch) }
    override fun close() { listener.close(); reconnect.cancel(); worker.cancel(); refreshes.close() }

    companion object {
        internal fun parseAccount(value: JsonObject) = Account(value.string("alias"), value.string("provider"),
            value.optionalString("label"), value.string("auth_method"), value["active"] == JsonPrimitive(true),
            value.optionalString("identity"), value.objectAt("status").string("status"))
        internal fun parseProvider(value: JsonObject): ProviderDescriptor {
            val methods = value.strings("auth_methods")
            return ProviderDescriptor(value.string("provider"), "api_key" in methods, "oauth" in methods,
                value["enabled"] == JsonPrimitive(true) && value.optionalString("availability") == "available",
                value.strings("models").toList(), value.optionalString("default_model"))
        }
        private fun checkAvailability(value: JsonObject) {
            if ((value["availability"] as? JsonObject)?.optionalString("state") == "unavailable") throw RpcRemoteException("snapshot_unavailable")
        }
    }
}
