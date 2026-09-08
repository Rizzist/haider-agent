package ai.diffforge.haider.ui.accounts

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pins every account door to the canonical transcript
 * (`crates/haider-rpc/tests/fixtures/wire_transcript.json`) and the frozen Rust
 * declarations. Round 1 shipped shapes derived from the desktop client's Tauri
 * command names, which are not the wire: `value` instead of `secret`, a missing
 * `stage_id`, missing `command_id`s, `accounts` instead of `descriptors`,
 * `id`/`auth_kinds` instead of `provider`/`auth_methods`, and an `attempt_id`
 * demanded back from a response that never carries one.
 *
 * The request payloads below are the fixture bodies verbatim, minus the
 * envelope. If a builder drifts, one of these fails.
 */
class AccountsWireShapeTest {

    // ---------- requests ----------

    @Test
    fun `vault stage carries stage_id and secret, never value`() {
        val request = AccountsRpcAdapter.stageApiKeyRequest(
            "golden-placeholder-key".toCharArray(),
            stageId = "stage-1",
        )
        val body = request.body()
        // frame.rs:3951 — VaultStage { stage_id, purpose, secret }
        assertEquals("vault.stage", body.getString("method"))
        assertEquals("stage-1", body.getString("stage_id"))
        assertEquals("api_key", body.getString("purpose"))
        assertEquals("golden-placeholder-key", body.getString("secret"))
        assertFalse("`value` is not a wire field", body.has("value"))
        // And it still refuses to print itself.
        assertFalse(request.toString().contains("golden-placeholder-key"))
    }

    @Test
    fun `the menu secret purpose uses the same door`() {
        val body = AccountsRpcAdapter.stageMenuSecretRequest("s".toCharArray(), "stage-menu").body()
        assertEquals("menu_secret", body.getString("purpose"))
        assertTrue(body.has("secret"))
    }

    @Test
    fun `vault stage response yields the reference the commit claims`() {
        val response = JSONObject(
            """{"method":"vault.stage","stage_id":"stage-1",
               "vault_reference":"vaultref-0123456789abcdef","expires_at_ms":1753500060000}""",
        )
        assertEquals("vaultref-0123456789abcdef", AccountsRpcAdapter.parseVaultStage(response))
    }

    @Test
    fun `login_api matches the fixture body exactly`() {
        val body = AccountsRpcAdapter.loginApiRequest(
            commandId = "command-login",
            provider = "anthropic",
            alias = "work",
            vaultReference = "vaultref-0123456789abcdef",
        )
        assertEquals("account.login_api", body.getString("method"))
        assertEquals("command-login", body.getString("command_id"))
        assertEquals("anthropic", body.getString("provider"))
        assertEquals("work", body.getString("alias"))
        assertEquals("vaultref-0123456789abcdef", body.getString("vault_reference"))
        // Both are `skip_serializing_if`: omitted, never an explicit null.
        assertFalse(body.has("validation_model"))
        assertFalse(body.has("replace_existing"))
    }

    @Test
    fun `login_api omits an absent alias rather than sending null`() {
        val body = AccountsRpcAdapter.loginApiRequest("c", "anthropic", null, "ref")
        assertFalse(body.has("alias"))
    }

    @Test
    fun `remove carries a command id`() {
        val body = AccountsRpcAdapter.removeRequest("command-remove", "work", 8)
        assertEquals("account.remove", body.getString("method"))
        assertEquals("command-remove", body.getString("command_id"))
        assertEquals("work", body.getString("alias"))
        assertEquals(8L, body.getLong("expected_revision"))
    }

    @Test
    fun `set_active carries a command id and no expected revision`() {
        val body = AccountsRpcAdapter.setActiveRequest("command-set-active", "work")
        assertEquals("account.set_active", body.getString("method"))
        assertEquals("command-set-active", body.getString("command_id"))
        assertEquals("work", body.getString("alias"))
        // frame.rs:4060 has confirm_new_epoch, not expected_revision.
        assertFalse(body.has("expected_revision"))
        assertFalse(body.has("confirm_new_epoch"))
        assertTrue(
            AccountsRpcAdapter.setActiveRequest("c", "work", confirmNewEpoch = true)
                .getBoolean("confirm_new_epoch"),
        )
    }

    @Test
    fun `oauth start and status match the fixture bodies`() {
        val start = AccountsRpcAdapter.oauthStartRequest("fake-oauth", "work-oauth", "attempt-1")
        assertEquals("account.oauth_start", start.getString("method"))
        assertEquals("fake-oauth", start.getString("provider"))
        assertEquals("work-oauth", start.getString("desired_alias"))
        assertEquals("attempt-1", start.getString("attempt_id"))

        val status = AccountsRpcAdapter.oauthStatusRequest("oauth-flow-golden", "attempt-1")
        assertEquals("oauth-flow-golden", status.getString("flow_id"))
        assertEquals("attempt-1", status.getString("attempt_id"))
    }

    @Test
    fun `account add matches the fixture body`() {
        val body = AccountsRpcAdapter.accountAddRequest(
            commandId = "command-account-add",
            provider = "fake-oauth",
            alias = "work-oauth",
            flowId = "oauth-flow-golden",
            attemptId = "attempt-1",
            oauthReference = "oauth-ready-golden",
        )
        assertEquals("account.add", body.getString("method"))
        assertEquals("command-account-add", body.getString("command_id"))
        assertEquals("oauth", body.getString("auth_method"))
        assertEquals("oauth-ready-golden", body.getString("oauth_reference"))
    }

    // ---------- responses ----------

    @Test
    fun `provider list consumes the canonical projection`() {
        val body = JSONObject(
            """{"method":"provider.list","providers":[{"provider":"openai",
               "api_family":"openai_responses","endpoint":"https://api.openai.com/v1/responses",
               "models":["frontier-a"],"model_details":[{"name":"frontier-a"}],
               "auth_methods":["api_key"],"availability":"available",
               "default_model":"frontier-a","enabled":true}],"revision":7}""",
        )
        val providers = AccountsRpcAdapter.parseProviders(body)
        val openai = providers.single()
        assertEquals("openai", openai.id)
        assertEquals(listOf("frontier-a"), openai.models)
        assertEquals("frontier-a", openai.defaultModel)
        assertEquals("openai_responses", openai.apiFamily)
        assertTrue(openai.supportsApiKey)
        assertFalse(openai.supportsOAuth)
        assertTrue(openai.available)
        assertEquals(7L, AccountsRpcAdapter.parseProviderRevision(body))
    }

    @Test
    fun `an unavailable or disabled provider is not offered`() {
        val body = JSONObject(
            """{"providers":[{"provider":"deepseek","api_family":"openai_chat","models":[],
               "auth_methods":["api_key"],"availability":"needs_credentials",
               "availability_reason":"no account","enabled":true}]}""",
        )
        val row = AccountsRpcAdapter.parseProviders(body).single()
        assertFalse(row.available)
        assertEquals("no account", row.unavailableReason)
    }

    @Test
    fun `account list consumes descriptors`() {
        val body = JSONObject(
            """{"method":"account.list","descriptors":[{
               "alias":"anthropic-0123456789abcdef01234567","provider":"anthropic",
               "auth_method":"api_key","identity":"work","status":{"status":"ok"},
               "active":true}]}""",
        )
        val snapshot = AccountsRpcAdapter.parseAccounts(body)
        val account = snapshot.accounts.single()
        assertEquals("anthropic-0123456789abcdef01234567", account.alias)
        assertEquals(AuthKind.ApiKey, account.authKind)
        assertEquals("work", account.identity)
        assertEquals("ok", account.status)
        assertTrue(account.active)
        // A daemon that omits the revision leaves it null. Absent is not zero:
        // zero is a value a daemon can send (lane 971-3, UI-12).
        assertNull(snapshot.revision)
    }

    @Test
    fun `oauth is the explicit rename, not o_auth`() {
        val body = JSONObject(
            """{"descriptors":[{"alias":"a","provider":"p","auth_method":"oauth",
               "identity":"i","status":{"status":"ok"},"active":false}]}""",
        )
        assertEquals(AuthKind.OAuth, AccountsRpcAdapter.parseAccounts(body).accounts.single().authKind)
    }

    @Test
    fun `a single descriptor response is understood`() {
        val body = JSONObject(
            """{"method":"account.login_api","descriptor":{
               "alias":"anthropic-01","provider":"anthropic","auth_method":"api_key",
               "identity":"work","status":{"status":"ok"},"active":true}}""",
        )
        assertEquals("anthropic-01", AccountsRpcAdapter.parseDescriptor(body)!!.alias)
        assertNull(AccountsRpcAdapter.parseDescriptor(JSONObject("""{"method":"x"}""")))
    }

    @Test
    fun `oauth start does not require an attempt id the response never sends`() {
        val body = JSONObject(
            """{"method":"account.oauth_start","availability":{"available":true},
               "flow_id":"oauth-flow-golden",
               "authorization_url":"https://auth.example.invalid/authorize?state=golden",
               "provider_origin":"https://auth.example.invalid","loopback_port":49152,
               "expires_at_ms":1753500060000}""",
        )
        val flow = AccountsRpcAdapter.parseOAuthStart(
            provider = "fake-oauth",
            alias = "work-oauth",
            style = OAuthStyle.AuthorizationCode,
            body = body,
            attemptId = "attempt-1",
        )
        assertTrue(flow is OAuthFlow.Started)
        flow as OAuthFlow.Started
        assertEquals("oauth-flow-golden", flow.flowId)
        // The client's own attempt id, because the wire does not return one.
        assertEquals("attempt-1", flow.attemptId)
        assertEquals(1753500060000L, flow.expiresAtMs)
    }

    @Test
    fun `oauth status is internally tagged and every phase is understood`() {
        fun status(json: String) = AccountsRpcAdapter.parseOAuthStatus(JSONObject(json))
        assertEquals(OAuthStatus.Waiting, status("""{"status":{"status":"waiting_browser"}}"""))
        assertEquals(OAuthStatus.Waiting, status("""{"status":{"status":"waiting_device"}}"""))
        assertEquals(OAuthStatus.Exchanging, status("""{"status":{"status":"exchanging"}}"""))
        val ready = status(
            """{"method":"account.oauth_status","flow_id":"oauth-flow-golden","status":{
               "status":"ready","oauth_reference":"oauth-ready-golden",
               "identity":"person@example.invalid","expires_at_ms":1753500360000}}""",
        ) as OAuthStatus.Ready
        assertEquals("oauth-ready-golden", ready.oauthReference)
        assertEquals("person@example.invalid", ready.identity)
        val failed = status("""{"status":{"status":"failed","public_code":"provider_denied"}}""")
            as OAuthStatus.Failed
        assertEquals("provider_denied", failed.publicCode)
        assertEquals("expired", (status("""{"status":{"status":"expired"}}""") as OAuthStatus.Failed).terminalKind)
        // #[serde(other)] Unknown: keep polling rather than invent a verdict.
        assertEquals(OAuthStatus.Waiting, status("""{"status":{"status":"a_phase_after_971"}}"""))
    }
}
