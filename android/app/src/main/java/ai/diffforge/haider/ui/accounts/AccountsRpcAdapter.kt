package ai.diffforge.haider.ui.accounts

import org.json.JSONArray
import org.json.JSONObject

/**
 * The one file that knows the account/provider wire shapes.
 *
 * Method names are the frozen ones from `docs/android/contracts-v1.md`: there
 * is no `oauth.*` namespace, `account.oauth_import*` is daemon-local CLI import
 * and not a browser callback API, and no new RequestBody method is introduced —
 * the 136-method pin is unchanged.
 *
 * Lane 971-3 points a real full-RPC client at these builders and parsers;
 * nothing else in the UI moves.
 *
 * Secrets: [stageApiKeyRequest] is the only place a key is ever written into a
 * payload, and the request type it returns is deliberately not a `data class`,
 * so a generated `toString()` can never print one.
 */
object AccountsRpcAdapter {
    const val METHOD_PROVIDER_LIST = "provider.list"
    const val METHOD_ACCOUNT_LIST = "account.list"
    const val METHOD_ACCOUNT_LIST_WATCH = "account.list_watch"
    const val METHOD_ACCOUNT_REFRESH = "account.refresh"
    const val METHOD_VAULT_STAGE = "vault.stage"
    const val METHOD_ACCOUNT_LOGIN_API = "account.login_api"
    const val METHOD_ACCOUNT_ADD = "account.add"
    const val METHOD_ACCOUNT_REMOVE = "account.remove"
    const val METHOD_ACCOUNT_SET_ACTIVE = "account.set_active"
    const val METHOD_ACCOUNT_SET_LABEL = "account.set_label"
    const val METHOD_ACCOUNT_SET_DEFAULT_MODEL = "account.set_default_model"
    const val METHOD_OAUTH_START = "account.oauth_start"
    const val METHOD_OAUTH_STATUS = "account.oauth_status"
    const val METHOD_OAUTH_CANCEL = "account.oauth_cancel"

    const val VAULT_PURPOSE_API_KEY = "api_key"
    const val AUTH_METHOD_OAUTH = "oauth"

    /** Never a `data class`: a generated `toString()` would print the key. */
    class SecretRequest(
        val method: String,
        private val payload: JSONObject,
    ) {
        fun body(): JSONObject = payload
        override fun toString(): String = "$method(secret=REDACTED)"
    }

    fun providerListRequest(): JSONObject = JSONObject().put("method", METHOD_PROVIDER_LIST)

    fun accountListRequest(provider: String? = null): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_LIST)
        .put("provider", provider ?: JSONObject.NULL)

    fun accountListWatchRequest(): JSONObject = JSONObject().put("method", METHOD_ACCOUNT_LIST_WATCH)

    /** Step one of an API-key add: the key goes into the vault, not into a log. */
    fun stageApiKeyRequest(apiKey: CharArray): SecretRequest = SecretRequest(
        METHOD_VAULT_STAGE,
        JSONObject()
            .put("method", METHOD_VAULT_STAGE)
            .put("purpose", VAULT_PURPOSE_API_KEY)
            .put("value", String(apiKey)),
    )

    /** Step two: commit the staged reference on the SAME connection. */
    fun loginApiRequest(
        commandId: String,
        provider: String,
        alias: String?,
        vaultReference: String,
        validationModel: String? = null,
        replaceExisting: Boolean = false,
    ): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_LOGIN_API)
        .put("command_id", commandId)
        .put("provider", provider)
        .put("alias", alias ?: JSONObject.NULL)
        .put("vault_reference", vaultReference)
        .put("validation_model", validationModel ?: JSONObject.NULL)
        .put("replace_existing", replaceExisting)

    fun removeRequest(alias: String, expectedRevision: Long?): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_REMOVE)
        .put("alias", alias)
        .put("expected_revision", expectedRevision ?: JSONObject.NULL)

    fun setActiveRequest(alias: String, expectedRevision: Long?): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_SET_ACTIVE)
        .put("alias", alias)
        .put("expected_revision", expectedRevision ?: JSONObject.NULL)

    fun oauthStartRequest(provider: String, desiredAlias: String?, attemptId: String): JSONObject =
        JSONObject()
            .put("method", METHOD_OAUTH_START)
            .put("provider", provider)
            .put("desired_alias", desiredAlias ?: provider)
            .put("attempt_id", attemptId)

    fun oauthStatusRequest(flowId: String, attemptId: String): JSONObject = JSONObject()
        .put("method", METHOD_OAUTH_STATUS)
        .put("flow_id", flowId)
        .put("attempt_id", attemptId)

    /** Commit, after Ready. The opaque reference goes straight back; no token. */
    fun accountAddRequest(
        commandId: String,
        provider: String,
        alias: String,
        flowId: String,
        attemptId: String,
        oauthReference: String,
    ): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_ADD)
        .put("command_id", commandId)
        .put("provider", provider)
        .put("alias", alias)
        .put("auth_method", AUTH_METHOD_OAUTH)
        .put("flow_id", flowId)
        .put("attempt_id", attemptId)
        .put("oauth_reference", oauthReference)

    fun oauthCancelRequest(flowId: String, attemptId: String): JSONObject = JSONObject()
        .put("method", METHOD_OAUTH_CANCEL)
        .put("flow_id", flowId)
        .put("attempt_id", attemptId)

    // ---------- parsers ----------

    fun parseProviders(body: JSONObject): List<ProviderDescriptor> {
        val array = body.optJSONArray("providers") ?: JSONArray()
        return (0 until array.length()).map { index ->
            val item = array.getJSONObject(index)
            val auth = item.optJSONArray("auth_kinds")
            val kinds = (0 until (auth?.length() ?: 0)).map { auth!!.getString(it) }
            ProviderDescriptor(
                id = item.getString("id"),
                label = item.optString("label", item.getString("id")),
                supportsApiKey = kinds.isEmpty() || kinds.contains("api_key"),
                supportsOAuth = kinds.contains(AUTH_METHOD_OAUTH),
                oauthStyle = if (item.optString("oauth_style") == "device") {
                    OAuthStyle.Device
                } else {
                    OAuthStyle.AuthorizationCode
                },
                available = item.optBoolean("available", true),
                unavailableReason = item.optStringOrNull("reason"),
            )
        }
    }

    fun parseAccounts(body: JSONObject): AccountsSnapshot {
        val array = body.optJSONArray("accounts") ?: JSONArray()
        val accounts = (0 until array.length()).map { index ->
            val item = array.getJSONObject(index)
            Account(
                alias = item.getString("alias"),
                provider = item.getString("provider"),
                label = item.optStringOrNull("label"),
                authKind = if (item.optString("auth_method") == AUTH_METHOD_OAUTH) {
                    AuthKind.OAuth
                } else {
                    AuthKind.ApiKey
                },
                active = item.optBoolean("active", false),
                identity = item.optStringOrNull("identity"),
                status = item.optString("status", "ok"),
            )
        }
        return AccountsSnapshot(revision = body.optLong("revision", 0L), accounts = accounts)
    }

    fun parseOAuthStart(
        provider: String,
        alias: String,
        style: OAuthStyle,
        body: JSONObject,
    ): OAuthFlow {
        val availability = body.optJSONObject("availability")
        if (availability != null && !availability.optBoolean("available", true)) {
            return OAuthFlow.Unavailable(provider, availability.optStringOrNull("reason"))
        }
        return OAuthFlow.Started(
            provider = provider,
            alias = alias,
            flowId = body.getString("flow_id"),
            attemptId = body.getString("attempt_id"),
            style = style,
            authorizationUrl = body.optStringOrNull("authorization_url")
                ?: body.optStringOrNull("verification_url"),
            userCode = body.optStringOrNull("user_code"),
            expiresAtMs = if (body.isNull("expires_at_ms")) null else body.optLong("expires_at_ms"),
        )
    }

    fun parseOAuthStatus(body: JSONObject): OAuthStatus {
        val status = body.optJSONObject("status") ?: body
        return when (val kind = status.optString("status", "unknown")) {
            "ready" -> OAuthStatus.Ready(
                oauthReference = status.optString("oauth_reference"),
                identity = status.optStringOrNull("identity"),
            )
            "exchanging" -> OAuthStatus.Exchanging
            "failed", "expired", "cancelled" -> OAuthStatus.Failed(
                publicCode = status.optStringOrNull("public_code"),
                terminalKind = kind,
            )
            // waiting_browser / waiting_device / unknown: keep polling.
            else -> OAuthStatus.Waiting
        }
    }

    private fun JSONObject.optStringOrNull(key: String): String? =
        if (isNull(key)) null else optString(key).ifBlank { null }
}
