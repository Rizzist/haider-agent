package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import java.io.Closeable

/** Binder facade supplies targets. A null target immediately revokes all data-plane authority. */
class StandaloneRpcConnection(scope: CoroutineScope, targets: StateFlow<RpcTarget?>,
    val client: RpcClient = RpcClient(scope), control: Boolean = true) : Closeable {
    private val owner = scope.launch {
        targets.collectLatest { target ->
            client.close()
            if (target == null) return@collectLatest
            var backoff = 500L
            while (currentCoroutineContext().isActive) {
                try {
                    client.connect(target, control)
                    backoff = 500L
                    client.state.first { it != RpcConnectionState.CONNECTED }
                } catch (cancelled: CancellationException) { throw cancelled }
                catch (_: Exception) { /* Status is redacted by RpcClient; Binder target remains authority. */ }
                if (client.state.value == RpcConnectionState.PROTOCOL_ERROR) return@collectLatest
                delay(backoff)
                backoff = (backoff * 2).coerceAtMost(8000L)
            }
        }
    }
    override fun close() { owner.cancel(); client.close() }
}

/** UI fake adapters can bind these data-plane methods without implementing Binder or JNI again. */
interface SessionRoster {
    val sessions: StateFlow<List<SessionSummary>>
    val loading: StateFlow<Boolean>
    val loadError: StateFlow<String?>
    suspend fun refreshRoster()
    suspend fun observe(session: String): kotlinx.serialization.json.JsonObject
}

interface AccountsDataSource {
    val providers: StateFlow<List<ProviderDescriptor>>
    val snapshot: StateFlow<AccountsSnapshot>
    val loadError: StateFlow<String?>
    suspend fun refresh()
    suspend fun addApiKey(provider: String, alias: String?, apiKey: CharArray, commandId: String,
        validationModel: String? = null, replaceExisting: Boolean = false): Account
    suspend fun validateApiKey(provider: String, apiKey: CharArray): Nothing
    suspend fun remove(alias: String, commandId: String, expectedRevision: Long? = snapshot.value.revision)
    suspend fun setActive(alias: String, commandId: String, confirmNewEpoch: Boolean = false)
    suspend fun refreshAccount(alias: String)
    suspend fun startOAuth(provider: String, desiredAlias: String): OAuthFlow
    suspend fun pollOAuth(flow: OAuthFlow): OAuthStatus
    suspend fun completeOAuth(flow: OAuthFlow, oauthReference: String, commandId: String): Account
    suspend fun cancelOAuth(flow: OAuthFlow)
}
