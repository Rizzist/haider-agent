package ai.diffforge.haider.accounts

import kotlinx.coroutines.flow.StateFlow

/**
 * Provider and account management on the phone.
 *
 * The daemon's account RPCs are not reachable from the APK until lane 971-3
 * binds the real client, so the UI compiles against this interface and runs on
 * [FakeAccountsRepository]. The wire mapping lives in exactly one file
 * ([AccountsRpcAdapter]) so binding it later is a one-file change.
 *
 * Secrets never leave this layer: an API key is passed straight into a request
 * body and is never stored on an [Account], never logged, never put in a
 * snapshot and never rendered back to the user.
 */

enum class AuthKind { ApiKey, OAuth }

data class ProviderDescriptor(
    val id: String,
    val label: String,
    val supportsApiKey: Boolean,
    val supportsOAuth: Boolean,
    val available: Boolean = true,
    val unavailableReason: String? = null,
)

data class Account(
    val alias: String,
    val provider: String,
    val label: String?,
    val authKind: AuthKind,
    val active: Boolean,
    val identity: String?,
    val status: String,
)

data class AccountsSnapshot(
    val revision: Long,
    val accounts: List<Account>,
)

/** The non-terminal phases of an OAuth sign-in, mirroring the desktop flow. */
sealed interface OAuthFlow {
    data class Started(
        val provider: String,
        val alias: String,
        val flowId: String,
        val attemptId: String,
        val authorizationUrl: String?,
        val userCode: String?,
    ) : OAuthFlow

    data class Unavailable(val provider: String, val reason: String?) : OAuthFlow
}

sealed interface OAuthStatus {
    data object Waiting : OAuthStatus
    data class Ready(val oauthReference: String, val identity: String?) : OAuthStatus
    data class Failed(val publicCode: String?, val terminalKind: String) : OAuthStatus
}

/** Every mutation returns this; a failure carries only a redacted public code. */
sealed interface AccountResult {
    data object Ok : AccountResult
    data class Failed(val publicCode: String) : AccountResult
}

interface AccountsRepository {
    val providers: StateFlow<List<ProviderDescriptor>>
    val snapshot: StateFlow<AccountsSnapshot>
    val loadError: StateFlow<String?>

    suspend fun refresh()

    /** Validates the key with the provider and stores it in the on-device vault. */
    suspend fun addApiKey(provider: String, alias: String?, apiKey: CharArray): AccountResult

    /** Validate only; used by the key field's inline check. */
    suspend fun validateApiKey(provider: String, apiKey: CharArray): AccountResult

    suspend fun remove(alias: String): AccountResult
    suspend fun setActive(alias: String): AccountResult

    suspend fun startOAuth(provider: String, desiredAlias: String?): OAuthFlow
    suspend fun pollOAuth(flow: OAuthFlow.Started): OAuthStatus
    suspend fun completeOAuth(flow: OAuthFlow.Started, oauthReference: String): AccountResult
    suspend fun cancelOAuth(flow: OAuthFlow.Started)
}
