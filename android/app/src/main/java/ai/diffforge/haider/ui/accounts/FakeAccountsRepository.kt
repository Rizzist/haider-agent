package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * In-memory accounts, so Settings -> Accounts is fully operable before lane
 * 971-3 lands the RPC client. It models exactly the frozen doors, including the
 * parts that are easy to wish away:
 *
 *  - an API key is staged before it is committed, and a key that fails
 *    validation leaves no account behind;
 *  - the raw key is never kept — only the daemon-style masked hint;
 *  - an OAuth flow goes Waiting -> Exchanging -> Ready, because the provider's
 *    success page is sent before the token exchange finishes, so the UI has to
 *    render the in-between rather than assume instant success;
 *  - a flow can be Lost, because flow ownership is bound to the daemon
 *    instance, connection and attempt, and cannot move to a new connection.
 */
class FakeAccountsRepository(
    seed: List<Account> = defaultAccounts,
    private val waitingPolls: Int = 1,
    private val exchangingPolls: Int = 1,
) : AccountsRepository {

    private val _providers = MutableStateFlow(defaultProviders)
    private val _snapshot = MutableStateFlow(AccountsSnapshot(revision = 1, accounts = seed))
    private val _loadError = MutableStateFlow<String?>(null)

    override val providers: StateFlow<List<ProviderDescriptor>> = _providers.asStateFlow()
    override val snapshot: StateFlow<AccountsSnapshot> = _snapshot.asStateFlow()
    override val loadError: StateFlow<String?> = _loadError.asStateFlow()

    /** Method names only — never arguments; an argument here could be a key. */
    val calls = mutableListOf<String>()

    private var polls = 0
    private var terminal: OAuthStatus? = null

    /** Set to a public code to make the next mutation fail. */
    var nextFailure: String? = null

    /** Set to simulate the connection or UI process dying mid-flow. */
    var flowLost: Boolean = false

    override suspend fun refresh() {
        calls += AccountsRpcAdapter.METHOD_ACCOUNT_LIST
    }

    override suspend fun addApiKey(
        provider: String,
        alias: String?,
        apiKey: CharArray,
        replaceExisting: Boolean,
    ): AccountResult {
        calls += AccountsRpcAdapter.METHOD_VAULT_STAGE
        when (val validated = validateApiKey(provider, apiKey)) {
            is AccountResult.Failed -> return validated
            AccountResult.Ok -> Unit
        }
        calls += AccountsRpcAdapter.METHOD_ACCOUNT_LOGIN_API
        nextFailure?.let { nextFailure = null; return AccountResult.Failed(it) }
        val resolvedAlias = alias?.takeIf { it.isNotBlank() } ?: provider
        val current = _snapshot.value
        if (!replaceExisting && current.accounts.any { it.alias == resolvedAlias }) {
            return AccountResult.Failed("account_exists")
        }
        _snapshot.value = AccountsSnapshot(
            revision = current.revision + 1,
            accounts = current.accounts.filterNot { it.alias == resolvedAlias } + Account(
                alias = resolvedAlias,
                provider = provider,
                label = null,
                authKind = AuthKind.ApiKey,
                active = current.accounts.none { it.active },
                // The daemon exposes a masked hint, never the key.
                identity = "••••" + String(apiKey).takeLast(4),
                status = "ok",
            ),
        )
        return AccountResult.Ok
    }

    override suspend fun validateApiKey(provider: String, apiKey: CharArray): AccountResult =
        if (apiKey.size < MIN_KEY_LENGTH) AccountResult.Failed("invalid_api_key") else AccountResult.Ok

    override suspend fun remove(alias: String): AccountResult {
        calls += AccountsRpcAdapter.METHOD_ACCOUNT_REMOVE
        nextFailure?.let { nextFailure = null; return AccountResult.Failed(it) }
        val current = _snapshot.value
        _snapshot.value = AccountsSnapshot(
            revision = current.revision + 1,
            accounts = current.accounts.filterNot { it.alias == alias },
        )
        return AccountResult.Ok
    }

    override suspend fun setActive(alias: String): AccountResult {
        calls += AccountsRpcAdapter.METHOD_ACCOUNT_SET_ACTIVE
        val current = _snapshot.value
        _snapshot.value = AccountsSnapshot(
            revision = current.revision + 1,
            accounts = current.accounts.map { it.copy(active = it.alias == alias) },
        )
        return AccountResult.Ok
    }

    override suspend fun refreshProviders() {
        calls += AccountsRpcAdapter.METHOD_PROVIDER_LIST
    }

    override suspend fun startOAuth(
        provider: String,
        desiredAlias: String?,
        attemptId: String,
    ): OAuthFlow {
        calls += AccountsRpcAdapter.METHOD_OAUTH_START
        polls = 0
        terminal = null
        flowLost = false
        val descriptor = _providers.value.firstOrNull { it.id == provider }
        if (descriptor?.supportsOAuth != true || !descriptor.available) {
            return OAuthFlow.Unavailable(provider, descriptor?.unavailableReason ?: "not supported")
        }
        val device = descriptor.oauthStyle == OAuthStyle.Device
        return OAuthFlow.Started(
            provider = provider,
            alias = desiredAlias?.takeIf { it.isNotBlank() } ?: provider,
            flowId = "flow-$provider",
            attemptId = attemptId,
            style = descriptor.oauthStyle,
            // The daemon supplies the URL; the UI never composes a redirect.
            authorizationUrl = "http://127.0.0.1:41287/callback-$provider",
            userCode = if (device) "HAID-971" else null,
            expiresAtMs = null,
        )
    }

    override suspend fun pollOAuth(flow: OAuthFlow.Started): OAuthStatus {
        calls += AccountsRpcAdapter.METHOD_OAUTH_STATUS
        if (flowLost) return OAuthStatus.Lost
        terminal?.let { return it }
        polls += 1
        return when {
            polls <= waitingPolls -> OAuthStatus.Waiting
            polls <= waitingPolls + exchangingPolls -> OAuthStatus.Exchanging
            else -> OAuthStatus.Ready("ref-${flow.flowId}", "you@${flow.provider}")
        }
    }

    override suspend fun completeOAuth(
        flow: OAuthFlow.Started,
        oauthReference: String,
    ): AccountResult {
        calls += AccountsRpcAdapter.METHOD_ACCOUNT_ADD
        nextFailure?.let { nextFailure = null; return AccountResult.Failed(it) }
        val current = _snapshot.value
        _snapshot.value = AccountsSnapshot(
            revision = current.revision + 1,
            accounts = current.accounts.filterNot { it.alias == flow.alias } + Account(
                alias = flow.alias,
                provider = flow.provider,
                label = null,
                authKind = AuthKind.OAuth,
                active = current.accounts.none { it.active },
                identity = "you@${flow.provider}",
                status = "ok",
            ),
        )
        return AccountResult.Ok
    }

    override suspend fun cancelOAuth(flow: OAuthFlow.Started) {
        calls += AccountsRpcAdapter.METHOD_OAUTH_CANCEL
        terminal = OAuthStatus.Failed(null, "cancelled")
    }

    override suspend fun accountExists(provider: String, alias: String): Boolean =
        _snapshot.value.accounts.any { it.provider == provider && it.alias == alias }

    companion object {
        const val MIN_KEY_LENGTH = 12

        val defaultProviders = listOf(
            ProviderDescriptor("anthropic", "Anthropic", supportsApiKey = true, supportsOAuth = true),
            ProviderDescriptor("openai", "OpenAI", supportsApiKey = true, supportsOAuth = true),
            ProviderDescriptor(
                "kimi",
                "Kimi",
                supportsApiKey = true,
                supportsOAuth = true,
                oauthStyle = OAuthStyle.Device,
            ),
            ProviderDescriptor("google", "Google", supportsApiKey = true, supportsOAuth = false),
            ProviderDescriptor(
                "deepseek",
                "DeepSeek",
                supportsApiKey = true,
                supportsOAuth = false,
                available = false,
                unavailableReason = "no route from this device",
            ),
        )

        val defaultAccounts = listOf(
            Account(
                alias = "anthropic",
                provider = "anthropic",
                label = "Work",
                authKind = AuthKind.OAuth,
                active = true,
                identity = "you@anthropic",
                status = "ok",
            ),
        )
    }
}
