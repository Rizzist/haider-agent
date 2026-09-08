package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * The default [AccountsRepository.providerRevision]: one shared, permanently
 * empty flow, so an implementation that never read a revision does not
 * manufacture a new one on every access.
 */
private val NO_PROVIDER_REVISION: StateFlow<Long?> = MutableStateFlow<Long?>(null).asStateFlow()

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

/**
 * [Unknown] is a real answer. The frozen descriptor does not always say how an
 * account authenticates, and the old parser filled that silence with ApiKey —
 * a guess the UI then presented as fact (lane 971-3 handoff, UI-12).
 */
enum class AuthKind { ApiKey, OAuth, Unknown }

/**
 * Authorization-code providers use the loopback capture; device flows do not.
 *
 * `ProviderSummaryWire` carries no `oauth_style`, so [Unknown] is the honest
 * default: the returned authorization URL is the correct destination for both
 * styles, and nothing else about the flow may be invented.
 */
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
    /** `model_details`, keyed by model id: where supported efforts live. */
    val modelDetails: Map<String, ModelDetail> = emptyMap(),
    val defaultModel: String? = null,
    val apiFamily: String? = null,
    /**
     * `ProviderSummaryWire.inventory_authority` — whether discovery may veto a
     * model id for this provider (frame.rs:1217). Advisory means a router or
     * local server that commonly omits otherwise valid passthrough ids from
     * `/v1/models`, so a model the catalog does not list may still be accepted.
     * Unknown keeps the conservative behaviour for older summaries.
     */
    val inventoryAuthority: ModelInventoryAuthority = ModelInventoryAuthority.Unknown,
    /**
     * `ProviderSummaryWire.endpoint`, when the daemon published one. It is
     * shown on a custom row so a person can tell two local servers apart; the
     * client never derives "is this custom" from it, because the wire carries
     * no such flag and a built-in adapter may state an endpoint too.
     */
    val endpoint: String? = null,
)

/**
 * `ModelInventoryAuthorityWire` (frame.rs:1226).
 *
 * Only [Advisory] licenses a free-text model id: an authoritative catalog is
 * the daemon's own and a miss there is a real miss, while Unknown is an older
 * summary that says nothing and is treated conservatively.
 */
enum class ModelInventoryAuthority(val wire: String) {
    Authoritative("authoritative"),
    Advisory("advisory"),
    Unknown("unknown"),
    ;

    /** True when a model id outside the published inventory may still be selected. */
    val acceptsCustomModelId: Boolean get() = this == Advisory

    companion object {
        fun of(wire: String?): ModelInventoryAuthority =
            entries.firstOrNull { it.wire == wire } ?: Unknown
    }
}

/** One row of `ProviderSummaryWire.model_details`. */
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
        /**
         * The authorization URL is a one-use capability. A default data-class
         * toString drops it into any log line, crash report or debugger view
         * that happens to print the object (lane 971-3 handoff, UI-12).
         */
        override fun toString(): String =
            "OAuthFlow.Started(provider=$provider, alias=$alias, style=$style, redacted)"
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
        /** The reference is a bearer capability; it is never printed. */
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

/**
 * What `provider.models_probe` returned.
 *
 * The probe is read-only: no provider, account, credential or cache is written
 * by any of these outcomes, and a staged key is borrowed rather than consumed.
 */
sealed interface CustomModelsProbe {
    data class Models(val models: List<String>, val defaultModel: String?) : CustomModelsProbe

    /** A typed `ProviderProbeFailureWire`. The card offers a manual id instead. */
    data class Failed(val failure: CustomProbeFailure, val detail: String? = null) :
        CustomModelsProbe

    /** The daemon does not serve `provider_models_probe_v1`. */
    data class Unavailable(val reason: String) : CustomModelsProbe
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

    /**
     * `vault.stage` alone. Returns the opaque reference, so the UI can consume
     * and wipe the plaintext immediately and carry only the reference forward
     * — the key is not needed again.
     */
    suspend fun stageApiKey(apiKey: CharArray): String?

    /** `account.login_api` with an already-staged reference. */
    suspend fun commitStagedApiKey(
        provider: String,
        alias: String?,
        vaultReference: String,
        replaceExisting: Boolean = false,
    ): AccountResult

    /** Stages and validates without committing, for the field's inline check. */
    /**
     * Always `Failed("validate_only_unavailable")`.
     *
     * The frozen wire validates as part of `account.login_api`, which also
     * commits. There is no validation-only door, and the round-4 interface
     * promised one — so this states the refusal rather than letting a caller
     * believe a key was checked (lane 971-3 handoff, UI-14).
     */
    suspend fun validateApiKey(provider: String, apiKey: CharArray): AccountResult

    /** `provider.list`, the inventory both the pickers and this screen read. */
    suspend fun refreshProviders()

    /**
     * The `provider.list` revision the current [providers] snapshot was read at.
     *
     * Null is "the daemon did not state one" and is not zero. `provider.configure`
     * still requires an `expected_revision`, and the TUI sends zero in that case
     * (`submit_custom_add`: `self.providers.revision.unwrap_or(0)`); this client
     * does the same, and does it in one place so the substitution is visible.
     */
    val providerRevision: StateFlow<Long?> get() = NO_PROVIDER_REVISION

    /**
     * `provider.models_probe` — read-only discovery for a server that does not
     * exist yet. [probeVaultReference] is borrowed, not consumed; a keyless
     * probe passes none.
     */
    suspend fun probeCustomModels(
        provider: String,
        origin: String,
        apiFamily: String,
        keyless: Boolean,
        probeVaultReference: String? = null,
    ): CustomModelsProbe = CustomModelsProbe.Unavailable(
        AccountsRpcAdapter.FEATURE_PROVIDER_MODELS_PROBE_V1,
    )

    /**
     * `provider.configure` — the durable create, under [expectedRevision].
     *
     * The staged reference is deliberately NOT passed here once a probe has
     * borrowed it: it is spent by the following [commitStagedApiKey].
     */
    suspend fun configureCustomProvider(
        provider: String,
        origin: String,
        apiFamily: String,
        authRequirement: String,
        models: List<String>,
        defaultModel: String?,
        expectedRevision: Long,
    ): AccountResult = AccountResult.Failed(AccountsRpcAdapter.FEATURE_PROVIDER_CONFIGURE_V1)

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
