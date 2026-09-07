package ai.diffforge.haider.accounts

import org.json.JSONArray
import org.json.JSONObject

/**
 * The one file that knows the account/provider wire shapes.
 *
 * Field names are the daemon's, taken verbatim from the desktop client's calls
 * (`rust-diffforge/src/accounts/AccountsView.jsx`): `provider.list`,
 * `account.list`, `account.add_api_key`, `account.remove`,
 * `account.set_active`, `account.oauth_start|status|add|cancel`. Lane 971-3
 * points a real RPC client at these builders and parsers; nothing else in the
 * UI moves.
 *
 * Secrets: [addApiKeyRequest] is the only place an API key is ever written into
 * a payload, and the request object it returns overrides `toString()` so a key
 * cannot reach a log, a crash report or a bug-report attachment by accident.
 */
object AccountsRpcAdapter {
    const val METHOD_PROVIDER_LIST = "provider.list"
    const val METHOD_ACCOUNT_LIST = "account.list"
    const val METHOD_ACCOUNT_ADD_API_KEY = "account.add_api_key"
    const val METHOD_ACCOUNT_VALIDATE = "account.validate_api_key"
    const val METHOD_ACCOUNT_REMOVE = "account.remove"
    const val METHOD_ACCOUNT_SET_ACTIVE = "account.set_active"
    const val METHOD_OAUTH_START = "account.oauth_start"
    const val METHOD_OAUTH_STATUS = "account.oauth_status"
    const val METHOD_OAUTH_ADD = "account.oauth_add"
    const val METHOD_OAUTH_CANCEL = "account.oauth_cancel"

    /** Never `data class`: a generated `toString()` would print the key. */
    class ApiKeyRequest(
        val method: String,
        private val payload: JSONObject,
    ) {
        fun body(): JSONObject = payload
        override fun toString(): String = "$method(api_key=REDACTED)"
    }

    fun providerListRequest(): JSONObject = JSONObject().put("method", METHOD_PROVIDER_LIST)

    fun accountListRequest(provider: String? = null): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_LIST)
        .put("provider", provider ?: JSONObject.NULL)

    fun addApiKeyRequest(
        provider: String,
        alias: String?,
        apiKey: CharArray,
        validationModel: String? = null,
    ): ApiKeyRequest = ApiKeyRequest(
        METHOD_ACCOUNT_ADD_API_KEY,
        JSONObject()
            .put("provider", provider)
            .put("alias", alias ?: JSONObject.NULL)
            .put("api_key", String(apiKey))
            .put("validation_model", validationModel ?: JSONObject.NULL),
    )

    fun validateApiKeyRequest(provider: String, apiKey: CharArray): ApiKeyRequest = ApiKeyRequest(
        METHOD_ACCOUNT_VALIDATE,
        JSONObject()
            .put("provider", provider)
            .put("api_key", String(apiKey)),
    )

    fun removeRequest(alias: String, expectedRevision: Long?): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_REMOVE)
        .put("alias", alias)
        .put("expected_revision", expectedRevision ?: JSONObject.NULL)

    fun setActiveRequest(alias: String, expectedRevision: Long?): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_SET_ACTIVE)
        .put("alias", alias)
        .put("expected_revision", expectedRevision ?: JSONObject.NULL)

    fun oauthStartRequest(provider: String, desiredAlias: String?): JSONObject = JSONObject()
        .put("method", METHOD_OAUTH_START)
        .put("provider", provider)
        .put("desired_alias", desiredAlias ?: provider)

    fun oauthStatusRequest(flowId: String, attemptId: String): JSONObject = JSONObject()
        .put("method", METHOD_OAUTH_STATUS)
        .put("flow_id", flowId)
        .put("attempt_id", attemptId)

    fun oauthAddRequest(
        provider: String,
        alias: String,
        flowId: String,
        attemptId: String,
        oauthReference: String,
    ): JSONObject = JSONObject()
        .put("method", METHOD_OAUTH_ADD)
        .put("provider", provider)
        .put("alias", alias)
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
                supportsOAuth = kinds.contains("oauth"),
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
                authKind = if (item.optString("auth_kind") == "oauth") AuthKind.OAuth else AuthKind.ApiKey,
                active = item.optBoolean("active", false),
                identity = item.optStringOrNull("identity"),
                status = item.optString("status", "ok"),
            )
        }
        return AccountsSnapshot(revision = body.optLong("revision", 0L), accounts = accounts)
    }

    fun parseOAuthStart(provider: String, alias: String, body: JSONObject): OAuthFlow {
        val availability = body.optJSONObject("availability")
        if (availability != null && !availability.optBoolean("available", true)) {
            return OAuthFlow.Unavailable(provider, availability.optStringOrNull("reason"))
        }
        return OAuthFlow.Started(
            provider = provider,
            alias = alias,
            flowId = body.getString("flow_id"),
            attemptId = body.getString("attempt_id"),
            authorizationUrl = body.optStringOrNull("authorization_url"),
            userCode = body.optStringOrNull("user_code"),
        )
    }

    fun parseOAuthStatus(body: JSONObject): OAuthStatus {
        val status = body.optJSONObject("status") ?: body
        return when (val kind = status.optString("status", "unknown")) {
            "ready" -> OAuthStatus.Ready(
                oauthReference = status.optString("oauth_reference"),
                identity = status.optStringOrNull("identity"),
            )
            "failed", "expired", "cancelled" -> OAuthStatus.Failed(
                publicCode = status.optStringOrNull("public_code"),
                terminalKind = kind,
            )
            // waiting_browser / waiting_device / exchanging / unknown: keep polling.
            else -> OAuthStatus.Waiting
        }
    }

    private fun JSONObject.optStringOrNull(key: String): String? =
        if (isNull(key)) null else optString(key).ifBlank { null }
}
