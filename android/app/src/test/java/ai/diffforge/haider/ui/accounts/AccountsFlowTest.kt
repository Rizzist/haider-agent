package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The frozen account doors, and the one rule that matters more than any of
 * them: a secret never reaches a log, a snapshot or a rendered string.
 */
class AccountsFlowTest {

    @Test
    fun `an api key is staged before it is committed`() = runTest {
        val repository = FakeAccountsRepository()
        val result = repository.addApiKey("openai", "work", "sk-test-0123456789".toCharArray())
        assertEquals(AccountResult.Ok, result)
        assertEquals(
            listOf(
                AccountsRpcAdapter.METHOD_VAULT_STAGE,
                AccountsRpcAdapter.METHOD_ACCOUNT_LOGIN_API,
            ),
            repository.calls,
        )
    }

    @Test
    fun `a key that fails validation leaves no account behind`() = runTest {
        val repository = FakeAccountsRepository()
        val before = repository.snapshot.value.accounts.size
        val result = repository.addApiKey("openai", null, "short".toCharArray())
        assertTrue(result is AccountResult.Failed)
        assertEquals(before, repository.snapshot.value.accounts.size)
        assertFalse(repository.calls.contains(AccountsRpcAdapter.METHOD_ACCOUNT_LOGIN_API))
    }

    @Test
    fun `the stored account keeps a masked hint and never the key`() = runTest {
        val repository = FakeAccountsRepository()
        val secret = "sk-live-abcdefgh9999"
        repository.addApiKey("openai", "work", secret.toCharArray())
        val account = repository.snapshot.value.accounts.first { it.alias == "work" }
        assertFalse(account.toString().contains(secret))
        assertEquals("••••9999", account.identity)
        // Nothing anywhere in the recorded call log can carry an argument.
        assertTrue(repository.calls.none { it.contains(secret) })
    }

    @Test
    fun `the request type refuses to print a secret`() {
        val request = AccountsRpcAdapter.stageApiKeyRequest("sk-live-abcdefgh9999".toCharArray())
        assertFalse(request.toString().contains("sk-live"))
        assertTrue(request.toString().contains("REDACTED"))
        // The payload still carries it, because that is what the vault needs.
        assertTrue(request.body().getString("secret").startsWith("sk-live"))
        assertEquals(AccountsRpcAdapter.PURPOSE_API_KEY, request.body().getString("purpose"))
    }

    @Test
    fun `the oauth flow walks waiting then exchanging then ready`() = runTest {
        val repository = FakeAccountsRepository()
        val flow = repository.startOAuth("anthropic", "personal", "attempt-1") as OAuthFlow.Started
        assertEquals(OAuthStatus.Waiting, repository.pollOAuth(flow))
        // The provider's success page arrives before the exchange finishes.
        assertEquals(OAuthStatus.Exchanging, repository.pollOAuth(flow))
        val ready = repository.pollOAuth(flow) as OAuthStatus.Ready
        assertEquals(AccountResult.Ok, repository.completeOAuth(flow, ready.oauthReference))
        assertEquals(
            listOf(
                AccountsRpcAdapter.METHOD_OAUTH_START,
                AccountsRpcAdapter.METHOD_OAUTH_STATUS,
                AccountsRpcAdapter.METHOD_OAUTH_STATUS,
                AccountsRpcAdapter.METHOD_OAUTH_STATUS,
                AccountsRpcAdapter.METHOD_ACCOUNT_ADD,
            ),
            repository.calls,
        )
        assertTrue(repository.accountExists("anthropic", "personal"))
    }

    @Test
    fun `a device flow carries a user code instead of capturing a redirect`() = runTest {
        val repository = FakeAccountsRepository()
        val flow = repository.startOAuth("kimi", null, "attempt-1") as OAuthFlow.Started
        assertEquals(OAuthStyle.Device, flow.style)
        assertEquals("HAID-971", flow.userCode)
    }

    @Test
    fun `a lost flow is not resumable, only re-checked`() = runTest {
        val repository = FakeAccountsRepository()
        val flow = repository.startOAuth("anthropic", "personal", "attempt-1") as OAuthFlow.Started
        repository.flowLost = true
        assertEquals(OAuthStatus.Lost, repository.pollOAuth(flow))
        // The honest question is whether the commit already landed.
        assertFalse(repository.accountExists("anthropic", "personal"))
    }

    @Test
    fun `a provider without sign-in says so instead of offering it`() = runTest {
        val repository = FakeAccountsRepository()
        val unavailable = repository.startOAuth("google", null, "attempt-1")
        assertTrue(unavailable is OAuthFlow.Unavailable)
    }

    @Test
    fun `cancelling makes the next poll terminal`() = runTest {
        val repository = FakeAccountsRepository()
        val flow = repository.startOAuth("anthropic", null, "attempt-1") as OAuthFlow.Started
        repository.cancelOAuth(flow)
        val status = repository.pollOAuth(flow) as OAuthStatus.Failed
        assertEquals("cancelled", status.terminalKind)
    }

    @Test
    fun `the adapter builds the frozen method names, and no oauth namespace`() {
        assertEquals("account.oauth_start", AccountsRpcAdapter.METHOD_OAUTH_START)
        assertEquals("account.oauth_status", AccountsRpcAdapter.METHOD_OAUTH_STATUS)
        assertEquals("account.add", AccountsRpcAdapter.METHOD_ACCOUNT_ADD)
        assertEquals("account.login_api", AccountsRpcAdapter.METHOD_ACCOUNT_LOGIN_API)
        val add = AccountsRpcAdapter.accountAddRequest("c1", "anthropic", "work", "f", "a", "ref")
        assertEquals("oauth", add.getString("auth_method"))
        assertEquals("ref", add.getString("oauth_reference"))
        assertTrue(
            listOf(
                AccountsRpcAdapter.METHOD_OAUTH_START,
                AccountsRpcAdapter.METHOD_OAUTH_STATUS,
                AccountsRpcAdapter.METHOD_ACCOUNT_ADD,
            ).none { it.startsWith("oauth.") },
        )
    }

    @Test
    fun `exchanging is parsed rather than mistaken for ready`() {
        val body = JSONObject().put("status", JSONObject().put("status", "exchanging"))
        assertEquals(OAuthStatus.Exchanging, AccountsRpcAdapter.parseOAuthStatus(body))
        val waiting = JSONObject().put("status", JSONObject().put("status", "waiting_browser"))
        assertEquals(OAuthStatus.Waiting, AccountsRpcAdapter.parseOAuthStatus(waiting))
        val unknown = JSONObject().put("status", JSONObject().put("status", "a_state_after_971"))
        assertEquals(OAuthStatus.Waiting, AccountsRpcAdapter.parseOAuthStatus(unknown))
    }
}
