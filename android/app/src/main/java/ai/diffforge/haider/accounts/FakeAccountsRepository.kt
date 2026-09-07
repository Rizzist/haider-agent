package ai.diffforge.haider.accounts

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * In-memory accounts, so Settings -> Accounts is fully operable before the
 * daemon lanes land. It mirrors the daemon's behaviour where it matters:
 *
 *  - a key is validated before it is accepted, and a key that fails validation
 *    is not stored;
 *  - the raw key is never kept — only the fact that a key exists, plus the last
 *    four characters, which is what the daemon's own descriptors expose;
 *  - an OAuth flow polls before it can be claimed, so the UI must render the
 *    waiting state rather than assume instant success.
 */
class FakeAccountsRepository(
    seed: List<Account> = defaultAccounts,
    private val oauthPollsBeforeReady: Int = 2,
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
    private var cancelled = false

    /** Set to a public code to make the next mutation fail. */
    var nextFailure: String? = null

    override suspend fun refresh() {
        calls += AccountsRpcAdapter.METHOD_ACCOUNT_LIST
    }

    override suspend fun addApiKey(
        provider: String,
        alias: String?,
        apiKey: CharArray,
    ): AccountResult {
        calls += AccountsRpcAdapter.METHOD_ACCOUNT_ADD_API_KEY
        nextFailure?.let { nextFailure = null; return AccountResult.Failed(it) }
        when (val validated = validateApiKey(provider, apiKey)) {
            is AccountResult.Failed -> return validated
            AccountResult.Ok -> Unit
        }
        val resolvedAlias = alias?.takeIf { it.isNotBlank() } ?: provider
        val current = _snapshot.value
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

    override suspend fun validateApiKey(provider: String, apiKey: CharArray): AccountResult {
        calls += AccountsRpcAdapter.METHOD_ACCOUNT_VALIDATE
        return if (apiKey.size < MIN_KEY_LENGTH) {
            AccountResult.Failed("invalid_api_key")
        } else {
            AccountResult.Ok
        }
    }

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

    override suspend fun startOAuth(provider: String, desiredAlias: String?): OAuthFlow {
        calls += AccountsRpcAdapter.METHOD_OAUTH_START
        polls = 0
        cancelled = false
        val descriptor = _providers.value.firstOrNull { it.id == provider }
        if (descriptor?.supportsOAuth != true) {
            return OAuthFlow.Unavailable(provider, descriptor?.unavailableReason ?: "not supported")
        }
        return OAuthFlow.Started(
            provider = provider,
            alias = desiredAlias?.takeIf { it.isNotBlank() } ?: provider,
            flowId = "flow-$provider",
            attemptId = "attempt-1",
            authorizationUrl = "https://example.invalid/oauth/$provider",
            userCode = "HAID-971",
        )
    }

    override suspend fun pollOAuth(flow: OAuthFlow.Started): OAuthStatus {
        calls += AccountsRpcAdapter.METHOD_OAUTH_STATUS
        if (cancelled) return OAuthStatus.Failed(null, "cancelled")
        polls += 1
        return if (polls > oauthPollsBeforeReady) {
            OAuthStatus.Ready("ref-${flow.flowId}", "you@${flow.provider}")
        } else {
            OAuthStatus.Waiting
        }
    }

    override suspend fun completeOAuth(
        flow: OAuthFlow.Started,
        oauthReference: String,
    ): AccountResult {
        calls += AccountsRpcAdapter.METHOD_OAUTH_ADD
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
        cancelled = true
    }

    companion object {
        const val MIN_KEY_LENGTH = 12

        val defaultProviders = listOf(
            ProviderDescriptor("anthropic", "Anthropic", supportsApiKey = true, supportsOAuth = true),
            ProviderDescriptor("openai", "OpenAI", supportsApiKey = true, supportsOAuth = true),
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
