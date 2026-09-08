package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.flow.StateFlow

/**
 * Provider and account management on the phone, shaped by the frozen account
 * doors in `docs/android/contracts-v1.md`.
 *
 * There is no `oauth.*` namespace and no callback-ingress RPC: an API key is
 * staged into the vault and committed with `account.login_api`, and a sign-in
 * runs `account.oauth_start` -> the daemon's own loopback capture ->
 * `account.oauth_status` -> `account.add`, all on one live RPC connection.
 *
 * Lane 971-3 implements this against that connection; this lane compiles
 * against the interface and runs on [FakeAccountsRepository].
 *
 * Secrets never leave this layer. A key is passed straight through to
 * `vault.stage` and is never stored on an [Account], never logged, never put in
 * a snapshot, saved state or search index, and never rendered back.
 */

enum class AuthKind { ApiKey, OAuth, Unknown }

/** Authorization-code providers use the loopback capture; device flows do not. */
enum class OAuthStyle { AuthorizationCode, Device, Unknown }

data class ProviderDescriptor(
    val id: String,
    val label: String,
    val supportsApiKey: Boolean,
    val supportsOAuth: Boolean,
    val oauthStyle: OAuthStyle = OAuthStyle.Unknown,
    val available: Boolean = true,
    val unavailableReason: String? = null,
    /** `ProviderSummaryWire.models`; the composer's model picker reads these. */
    val models: List<String> = emptyList(),
    val modelDetails: Map<String, ModelDetail> = emptyMap(),
    val defaultModel: String? = null,
    val apiFamily: String? = null,
)

data class ModelDetail(
    val supportedEfforts: List<String> = emptyList(),
    val defaultEffort: String? = null,
    val contextWindow: Long? = null,
)

data class Account(
    val alias: String,
    val provider: String,
    val label: String?,
    val authKind: AuthKind,
    val active: Boolean,
    /** A masked hint from the daemon's descriptor. Never the credential. */
    val identity: String?,
    val status: String,
)

/**
 * [revision] is null when the daemon did not state one. Absent is not zero: a
 * missing revision compared equal to a real first revision and made a stale
 * snapshot look current (lane 971-3 handoff, UI-12).
 */
data class AccountsSnapshot(
    val revision: Long?,
    val accounts: List<Account>,
)

/** What `account.oauth_start` returned for one attempt. */
sealed interface OAuthFlow {
    data class Started(
        val provider: String,
        val alias: String,
        val flowId: String,
        val attemptId: String,
        val style: OAuthStyle,
        /** Authorization-code: opened in a Custom Tab. Device: the verification URL. */
        val authorizationUrl: String?,
        /** Device flows only. */
        val userCode: String?,
        val expiresAtMs: Long?,
    ) : OAuthFlow {
        override fun toString(): String = "OAuthFlow.Started(redacted)"
    }

    data class Unavailable(val provider: String, val reason: String?) : OAuthFlow
}

sealed interface OAuthStatus {
    /** waiting_browser / waiting_device: keep polling. */
    data object Waiting : OAuthStatus

    /** The success page is sent before the token exchange finishes. */
    data object Exchanging : OAuthStatus

    /** The only state that yields the opaque reference `account.add` takes. */
    data class Ready(val oauthReference: String, val identity: String?) : OAuthStatus {
        override fun toString(): String = "OAuthStatus.Ready(redacted)"
    }

    data class Failed(val publicCode: String?, val terminalKind: String) : OAuthStatus

    /** The connection or the UI process died: the flow is gone, start a new attempt. */
    data object Lost : OAuthStatus
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

    /** `account.list`, re-read on an `accounts_changed` watch event. */
    suspend fun refresh()

    /** `vault.stage` purpose `api_key` -> `account.login_api` on the same connection. */
    suspend fun addApiKey(
        provider: String,
        alias: String?,
        apiKey: CharArray,
        replaceExisting: Boolean = false,
    ): AccountResult

    /** Stage and wipe the plaintext. The returned reference belongs to this connection epoch. */
    suspend fun stageApiKey(apiKey: CharArray): String?

    suspend fun commitStagedApiKey(
        provider: String,
        alias: String?,
        vaultReference: String,
        replaceExisting: Boolean = false,
    ): AccountResult

    /** Frozen v1 has no validate-only door; returns Failed("validate_only_unavailable"). */
    suspend fun validateApiKey(provider: String, apiKey: CharArray): AccountResult

    /** `provider.list`, the inventory both the pickers and this screen read. */
    suspend fun refreshProviders()

    /** `account.remove` with the revision it was read at. */
    suspend fun remove(alias: String): AccountResult
    suspend fun setActive(alias: String): AccountResult

    /**
     * `account.oauth_start{provider,desired_alias,attempt_id}`. The attempt id
     * is the *client's*: the response does not carry one back, and it is the
     * coordinate every later `oauth_status` call is bound to.
     */
    suspend fun startOAuth(provider: String, desiredAlias: String?, attemptId: String): OAuthFlow

    /** `account.oauth_status{flow_id,attempt_id}` on the original connection. */
    suspend fun pollOAuth(flow: OAuthFlow.Started): OAuthStatus

    /** `account.add{...auth_method:"oauth"...}`; tokens never leave Rust. */
    suspend fun completeOAuth(flow: OAuthFlow.Started, oauthReference: String): AccountResult

    /** `account.oauth_cancel{flow_id,attempt_id}`. */
    suspend fun cancelOAuth(flow: OAuthFlow.Started)

    /**
     * After a lost flow, the honest question is whether a durable commit already
     * landed — not whether a ready reference can be resurrected. It cannot: flow
     * ownership is bound to the daemon instance, connection and attempt.
     */
    suspend fun accountExists(provider: String, alias: String): Boolean
}
