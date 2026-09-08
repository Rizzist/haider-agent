package ai.diffforge.haider.ui.accounts

import org.json.JSONArray
import org.json.JSONObject

/**
 * The one file that knows the account/provider wire shapes.
 *
 * Every builder and parser below is derived from the frozen Rust declarations
 * and checked against the canonical transcript
 * (`crates/haider-rpc/tests/fixtures/wire_transcript.json`), not from the
 * desktop client's Tauri command names:
 *
 * | door | Rust | canonical shape |
 * |---|---|---|
 * | `vault.stage` | `RequestBody::VaultStage` (frame.rs:3951) | `{stage_id, purpose, secret}` — the field is **secret**, and `stage_id` is a required client nonce for same-connection retry dedupe |
 * | `account.login_api` | `RequestBody::AccountLoginApi` (frame.rs:3968) | `{command_id, provider, alias?, vault_reference, validation_model?, replace_existing?}`; the optionals are `skip_serializing_if`, so they are omitted, not sent as null |
 * | `account.oauth_start` | frame.rs:3984 / resp frame.rs:5120 | request `{provider, desired_alias, attempt_id}`; response `{availability, flow_id, authorization_url, provider_origin?, loopback_port?, expires_at_ms?}` — **no `attempt_id` comes back**; the client keeps its own |
 * | `account.oauth_status` | frame.rs:3992 / resp frame.rs:5131 | `{flow_id, attempt_id}` → `{flow_id, status: OAuthFlowStatusWire}` |
 * | `account.add` | frame.rs:4044 | `{command_id, provider, alias, auth_method, flow_id, attempt_id, oauth_reference}` |
 * | `account.remove` | frame.rs:4067 | `{command_id, alias, expected_revision?}` |
 * | `account.set_active` | frame.rs:4060 | `{command_id, alias, confirm_new_epoch?}` — **no** `expected_revision` |
 * | `account.list` | resp frame.rs:5216 | `{descriptors: CredentialDescriptor[], revision?}` — the key is **descriptors** |
 * | `provider.list` | resp frame.rs:5237 | `{providers: ProviderSummaryWire[], revision}`, each row keyed `provider` / `auth_methods` / `availability` |
 * | `provider.models_probe` | frame.rs:4117 / resp frame.rs:5253 | request `{provider, origin, api_family, keyless, probe_vault_reference?}` — READ-ONLY, **no `command_id`**, because discovery is not a durable mutation; response `{provider, models, default_model?}` |
 * | `provider.configure` | frame.rs:4134 | `{command_id, provider, api_family?, origin?, auth_requirement?, enabled, models, default_model?, probe_vault_reference?, trust?, expected_revision}` — `expected_revision` is REQUIRED, and `probe_vault_reference` is omitted once a probe has already borrowed the stage |
 *
 * `CredentialDescriptor` (haider-protocol `credential.rs:60`) is
 * `{alias, provider, auth_method, identity, status: {status}, active, label?}`;
 * `AuthMethod` renames OAuth to `oauth` explicitly so the acronym is not
 * mangled.
 *
 * Secrets: [stageApiKeyRequest] and [stageMenuSecretRequest] are the only
 * places a secret is written into a payload, and the type they return is
 * deliberately not a `data class`, so a generated `toString()` cannot print one.
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
    const val METHOD_PROVIDER_MODELS_PROBE = "provider.models_probe"
    const val METHOD_PROVIDER_CONFIGURE = "provider.configure"

    /** `FEATURE_PROVIDER_MODELS_PROBE_V1` / `FEATURE_PROVIDER_CONFIGURE_V1` (frame.rs:343,349). */
    const val FEATURE_PROVIDER_CONFIGURE_V1 = "provider_configure_v1"
    const val FEATURE_PROVIDER_MODELS_PROBE_V1 = "provider_models_probe_v1"

    /** `ErrorData::ProviderProbeFailed` (frame.rs:5636), tagged on `kind`. */
    const val ERROR_PROVIDER_PROBE_FAILED = "provider_probe_failed"

    /** `StagePurpose` (frame.rs:1549). */
    const val PURPOSE_API_KEY = "api_key"
    const val PURPOSE_MENU_SECRET = "menu_secret"

    /** `AccountAddMethod` (frame.rs:1535). */
    const val AUTH_METHOD_OAUTH = "oauth"
    const val AUTH_METHOD_API_KEY = "api_key"

    /** Never a `data class`: a generated `toString()` would print the secret. */
    class SecretRequest(
        val method: String,
        private val payload: JSONObject,
    ) {
        fun body(): JSONObject = payload
        override fun toString(): String = "$method(secret=REDACTED)"
    }

    // ---------- reads ----------

    fun providerListRequest(provider: String? = null): JSONObject = JSONObject()
        .put("method", METHOD_PROVIDER_LIST)
        .putIfPresent("provider", provider)

    fun accountListRequest(provider: String? = null): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_LIST)
        .putIfPresent("provider", provider)

    fun accountListWatchRequest(): JSONObject = JSONObject().put("method", METHOD_ACCOUNT_LIST_WATCH)

    // ---------- secrets ----------

    /**
     * Step one of an API-key add. `stage_id` is an ephemeral client nonce: the
     * same id with the same bytes returns the same reference, the same id with
     * different bytes is invalid, so it must be fresh per attempt.
     */
    fun stageApiKeyRequest(apiKey: CharArray, stageId: String = "stage-api-key"): SecretRequest =
        SecretRequest(
            METHOD_VAULT_STAGE,
            JSONObject()
                .put("method", METHOD_VAULT_STAGE)
                .put("stage_id", stageId)
                .put("purpose", PURPOSE_API_KEY)
                .put("secret", String(apiKey)),
        )

    /** The same door with the menu purpose, for `MenuInput::SecretVaultReference`. */
    fun stageMenuSecretRequest(secret: CharArray, stageId: String): SecretRequest = SecretRequest(
        METHOD_VAULT_STAGE,
        JSONObject()
            .put("method", METHOD_VAULT_STAGE)
            .put("stage_id", stageId)
            .put("purpose", PURPOSE_MENU_SECRET)
            .put("secret", String(secret)),
    )

    fun parseVaultStage(body: JSONObject): String = body.getString("vault_reference")

    // ---------- writes ----------

    /** Step two: claim the staged reference on the SAME connection. */
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
        .putIfPresent("alias", alias)
        .put("vault_reference", vaultReference)
        .putIfPresent("validation_model", validationModel)
        .apply { if (replaceExisting) put("replace_existing", true) }

    fun removeRequest(
        commandId: String,
        alias: String,
        expectedRevision: Long? = null,
    ): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_REMOVE)
        .put("command_id", commandId)
        .put("alias", alias)
        .apply { if (expectedRevision != null) put("expected_revision", expectedRevision) }

    /**
     * The provider is intentionally absent: the daemon derives it from
     * descriptor truth. There is no `expected_revision` on this door —
     * `confirm_new_epoch` is the explicit confirmation coordinate.
     */
    fun setActiveRequest(
        commandId: String,
        alias: String,
        confirmNewEpoch: Boolean = false,
    ): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_SET_ACTIVE)
        .put("command_id", commandId)
        .put("alias", alias)
        .apply { if (confirmNewEpoch) put("confirm_new_epoch", true) }

    fun setDefaultModelRequest(
        commandId: String,
        provider: String,
        model: String,
        expectedRevision: Long,
    ): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_SET_DEFAULT_MODEL)
        .put("command_id", commandId)
        .put("provider", provider)
        .put("model", model)
        .put("expected_revision", expectedRevision)

    fun oauthStartRequest(provider: String, desiredAlias: String, attemptId: String): JSONObject =
        JSONObject()
            .put("method", METHOD_OAUTH_START)
            .put("provider", provider)
            .put("desired_alias", desiredAlias)
            .put("attempt_id", attemptId)

    fun oauthStatusRequest(flowId: String, attemptId: String): JSONObject = JSONObject()
        .put("method", METHOD_OAUTH_STATUS)
        .put("flow_id", flowId)
        .put("attempt_id", attemptId)

    fun oauthCancelRequest(flowId: String, attemptId: String): JSONObject = JSONObject()
        .put("method", METHOD_OAUTH_CANCEL)
        .put("flow_id", flowId)
        .put("attempt_id", attemptId)

    /** Commit after Ready. The opaque reference goes straight back; no token. */
    fun accountAddRequest(
        commandId: String,
        provider: String,
        alias: String,
        flowId: String,
        attemptId: String,
        oauthReference: String,
        authMethod: String = AUTH_METHOD_OAUTH,
    ): JSONObject = JSONObject()
        .put("method", METHOD_ACCOUNT_ADD)
        .put("command_id", commandId)
        .put("provider", provider)
        .put("alias", alias)
        .put("auth_method", authMethod)
        .put("flow_id", flowId)
        .put("attempt_id", attemptId)
        .put("oauth_reference", oauthReference)

    // ---------- custom servers ----------

    /**
     * Read-only discovery, before any provider exists.
     *
     * There is deliberately no `command_id`: the TUI pins that this is not a
     * durable mutation (`probe.command_id().is_none()`). A staged reference is
     * BORROWED — the daemon leaves it available for the later `login_api` — and
     * a keyless probe carries none at all.
     */
    fun providerModelsProbeRequest(
        provider: String,
        origin: String,
        apiFamily: String,
        keyless: Boolean,
        probeVaultReference: String? = null,
    ): JSONObject = JSONObject()
        .put("method", METHOD_PROVIDER_MODELS_PROBE)
        .put("provider", provider)
        .put("origin", origin)
        .put("api_family", apiFamily)
        .put("keyless", keyless)
        .putIfPresent("probe_vault_reference", probeVaultReference)

    /**
     * The durable create.
     *
     * `expected_revision` is required and is the `provider.list` revision the
     * form was read at. `probe_vault_reference` is omitted when a probe already
     * borrowed the stage: the reference is spent by `account.login_api`, not
     * here (`keyed_custom_server_stages_discovers_picks_then_configures_and_
     * consumes_one_reference` pins `probe_vault_reference: None`).
     */
    fun providerConfigureRequest(
        commandId: String,
        provider: String,
        origin: String,
        apiFamily: String,
        authRequirement: String,
        models: List<String>,
        defaultModel: String?,
        expectedRevision: Long,
        enabled: Boolean = true,
        probeVaultReference: String? = null,
    ): JSONObject = JSONObject()
        .put("method", METHOD_PROVIDER_CONFIGURE)
        .put("command_id", commandId)
        .put("provider", provider)
        .put("api_family", apiFamily)
        .put("origin", origin)
        .put("auth_requirement", authRequirement)
        .put("enabled", enabled)
        .put("models", JSONArray(models))
        .putIfPresent("default_model", defaultModel)
        .putIfPresent("probe_vault_reference", probeVaultReference)
        .put("expected_revision", expectedRevision)

    fun parseProviderModelsProbe(body: JSONObject): Pair<List<String>, String?> {
        val array = body.optJSONArray("models") ?: JSONArray()
        val models = (0 until array.length()).mapNotNull { array.optString(it).ifBlank { null } }
        return models to body.optStringOrNull("default_model")
    }

    /** `ProviderProbeFailureWire` (frame.rs:5737) out of an error body's typed data. */
    fun parseProbeFailure(data: JSONObject?): CustomProbeFailure =
        CustomProbeFailure.of(data?.optStringOrNull("failure"))

    // ---------- parsers ----------

    fun parseProviders(body: JSONObject): List<ProviderDescriptor> {
        val array = body.optJSONArray("providers") ?: JSONArray()
        return (0 until array.length()).map { index ->
            val item = array.getJSONObject(index)
            val id = item.getString("provider")
            val methods = item.stringList("auth_methods")
            val availability = item.optString("availability", "available")
            ProviderDescriptor(
                id = id,
                label = item.optStringOrNull("label") ?: id,
                // An empty auth_methods list is silence, not consent: it used
                // to be read as "API key supported" (lane 971-3, UI-12).
                supportsApiKey = methods.contains(AUTH_METHOD_API_KEY),
                supportsOAuth = methods.contains(AUTH_METHOD_OAUTH),
                // ProviderSummaryWire carries no oauth_style. The returned
                // authorization URL is the right destination for both styles,
                // so the style stays Unknown rather than being guessed.
                oauthStyle = when (item.optStringOrNull("oauth_style")) {
                    "device" -> OAuthStyle.Device
                    "authorization_code" -> OAuthStyle.AuthorizationCode
                    else -> OAuthStyle.Unknown
                },
                available = availability == "available" && item.optBoolean("enabled", true),
                unavailableReason = item.optStringOrNull("availability_reason"),
                models = item.stringList("models"),
                modelDetails = item.modelDetails(),
                defaultModel = item.optStringOrNull("default_model"),
                apiFamily = item.optStringOrNull("api_family"),
                // Absent is Unknown, and Unknown is conservative: a summary
                // that never named an authority does not license a free-text
                // model id (frame.rs:1217).
                inventoryAuthority = ModelInventoryAuthority.of(
                    item.optStringOrNull("inventory_authority"),
                ),
                endpoint = item.optStringOrNull("endpoint"),
            )
        }
    }

    fun parseProviderRevision(body: JSONObject): Long = body.optLong("revision", 0L)

    fun parseAccounts(body: JSONObject): AccountsSnapshot {
        // The response key is `descriptors`; `accounts` is not a wire name.
        val array = body.optJSONArray("descriptors") ?: JSONArray()
        val accounts = (0 until array.length()).map { index ->
            val item = array.getJSONObject(index)
            Account(
                alias = item.getString("alias"),
                provider = item.getString("provider"),
                label = item.optStringOrNull("label"),
                // An absent or unrecognised auth_method is Unknown. Mapping it
                // to ApiKey made the row claim a fact the daemon never sent
                // (lane 971-3 handoff, UI-12).
                authKind = when (item.optStringOrNull("auth_method")) {
                    AUTH_METHOD_OAUTH -> AuthKind.OAuth
                    AUTH_METHOD_API_KEY -> AuthKind.ApiKey
                    else -> AuthKind.Unknown
                },
                active = item.optBoolean("active", false),
                identity = item.optStringOrNull("identity"),
                // CredentialStatus is internally tagged on `status`.
                status = item.optJSONObject("status")?.optString("status") ?: "ok",
            )
        }
        return AccountsSnapshot(
            // Absent is null, not zero. Zero is a value a daemon can send,
            // and collapsing the two made a snapshot that stated no revision
            // compare equal to one that stated the first (971-3, UI-12).
            revision = if (body.isNull("revision")) null else body.optLong("revision"),
            accounts = accounts,
        )
    }

    /** One descriptor, as returned by login_api / add / set_active. */
    fun parseDescriptor(body: JSONObject): Account? {
        val item = body.optJSONObject("descriptor") ?: return null
        return parseAccounts(
            JSONObject().put("descriptors", JSONArray().put(item)),
        ).accounts.firstOrNull()
    }

    /**
     * The response carries no `attempt_id` — the client already has it, and
     * requiring one back is how a real flow gets rejected as malformed.
     */
    fun parseOAuthStart(
        provider: String,
        alias: String,
        style: OAuthStyle,
        body: JSONObject,
        attemptId: String = "",
    ): OAuthFlow {
        val availability = body.optJSONObject("availability")
        if (availability != null && !availability.optBoolean("available", true)) {
            return OAuthFlow.Unavailable(provider, availability.optStringOrNull("reason"))
        }
        return OAuthFlow.Started(
            provider = provider,
            alias = alias,
            flowId = body.getString("flow_id"),
            attemptId = attemptId,
            style = style,
            authorizationUrl = body.optStringOrNull("authorization_url")
                ?: body.optStringOrNull("verification_url"),
            userCode = body.optStringOrNull("user_code"),
            expiresAtMs = if (body.isNull("expires_at_ms")) null else body.optLong("expires_at_ms"),
        )
    }

    /** `OAuthFlowStatusWire` (frame.rs:1513), internally tagged on `status`. */
    fun parseOAuthStatus(body: JSONObject): OAuthStatus {
        val status = body.optJSONObject("status") ?: body
        return when (val kind = status.optString("status", "unknown")) {
            "ready" -> OAuthStatus.Ready(
                oauthReference = status.optString("oauth_reference"),
                identity = status.optStringOrNull("identity"),
            )
            "exchanging" -> OAuthStatus.Exchanging
            "failed" -> OAuthStatus.Failed(status.optStringOrNull("public_code"), kind)
            "expired", "cancelled" -> OAuthStatus.Failed(null, kind)
            // waiting_browser / waiting_device / unknown: keep polling.
            else -> OAuthStatus.Waiting
        }
    }

    private fun JSONObject.optStringOrNull(key: String): String? =
        if (isNull(key)) null else optString(key).ifBlank { null }

    /**
     * `ProviderSummaryWire.model_details` (frame.rs:1383). `models` is the flat
     * id list; the per-model detail is where supported efforts live, and the
     * effort picker reads it rather than a provider-wide guess.
     */
    private fun JSONObject.modelDetails(): Map<String, ModelDetail> {
        val array = optJSONArray("model_details") ?: return emptyMap()
        return (0 until array.length()).mapNotNull { index ->
            val item = array.optJSONObject(index) ?: return@mapNotNull null
            val name = item.optString("name").ifBlank { null } ?: return@mapNotNull null
            name to ModelDetail(
                supportedEfforts = item.stringList("supported_efforts"),
                defaultEffort = item.optStringOrNull("default_effort"),
                contextWindow = if (item.isNull("context_window")) {
                    null
                } else {
                    item.optLong("context_window")
                },
            )
        }.toMap()
    }

    private fun JSONObject.stringList(key: String): List<String> {
        val array = optJSONArray(key) ?: return emptyList()
        return (0 until array.length()).mapNotNull { index ->
            when (val value = array.opt(index)) {
                is String -> value
                is JSONObject -> value.optString("name").ifBlank { null }
                else -> null
            }
        }
    }

    /** Optional wire fields are omitted, never sent as an explicit null. */
    private fun JSONObject.putIfPresent(key: String, value: String?): JSONObject =
        if (value.isNullOrBlank()) this else put(key, value)
}
