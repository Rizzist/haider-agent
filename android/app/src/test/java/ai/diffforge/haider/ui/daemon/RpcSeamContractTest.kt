package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.ui.accounts.AccountsRpcAdapter
import ai.diffforge.haider.ui.accounts.AccountsSnapshot
import ai.diffforge.haider.ui.accounts.AccountResult
import ai.diffforge.haider.ui.accounts.AuthKind
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthFlow
import ai.diffforge.haider.ui.accounts.OAuthStatus
import ai.diffforge.haider.ui.accounts.OAuthStyle
import ai.diffforge.haider.ui.chat.ChatViewModel
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The seam lane 971-3's facade has to land on.
 *
 * Every one of these is a place where the round-4 interface stated something
 * the frozen wire does not support, and the permissive default then reached the
 * screen as a fact: a missing revision that compared equal to a real one, an
 * unknown auth method rendered as "API key", a validation call that does not
 * exist. The corrections are contract shape, so they are pinned here rather
 * than left to the integration merge.
 */
class RpcSeamContractTest {

    // ---------- absence is not zero ----------

    @Test
    fun `a response that states no revision parses as null`() {
        assertNull(AccountsRpcAdapter.parseAccounts(JSONObject("""{"descriptors":[]}""")).revision)
        // Zero is a value a daemon may send, and it is not absence.
        assertEquals(
            0L,
            AccountsRpcAdapter.parseAccounts(JSONObject("""{"descriptors":[],"revision":0}""")).revision,
        )
    }

    @Test
    fun `an absent account revision stays null`() {
        val snapshot = AccountsSnapshot(revision = null, accounts = emptyList())
        assertNull(snapshot.revision)
        // The distinction that matters: unknown must not equal a real first
        // revision, or a stale snapshot reads as current.
        assertTrue(snapshot.revision != 0L)
    }

    @Test
    fun `an absent provider revision stays null`() {
        assertNull(ProviderInventory().revision)
    }

    @Test
    fun `an absent run state stays null rather than the word unknown`() {
        assertNull(SessionRow(id = "s").runState)
        // The fold reads the daemon's own word and is unbothered by absence.
        assertEquals(
            SessionVisualState.Unknown,
            SessionVisualStateFold.fold(null, null),
        )
    }

    @Test
    fun `the fake keeps its own known revision when it increments`() {
        val repository = FakeAccountsRepository()
        val before = repository.snapshot.value.revision
        assertEquals(FakeAccountsRepository.SEED_REVISION, before)
        runTest {
            repository.setActive(repository.snapshot.value.accounts.first().alias)
        }
        assertEquals(before!! + 1, repository.snapshot.value.revision)
    }

    // ---------- unknown is a real answer ----------

    @Test
    fun `an unrecognised auth method is Unknown, not an API key`() {
        val payload = JSONObject(
            """{"descriptors":[{"alias":"a","provider":"p","auth_method":"passkey"}]}""",
        )
        val snapshot = AccountsRpcAdapter.parseAccounts(payload)
        assertEquals(AuthKind.Unknown, snapshot.accounts.single().authKind)
    }

    @Test
    fun `a provider that names no auth methods claims none`() {
        val payload = JSONObject("""{"providers":[{"provider":"p"}]}""")
        val provider = AccountsRpcAdapter.parseProviders(payload).single()
        // "auth_methods absent" used to mean "API key supported".
        assertTrue(!provider.supportsApiKey && !provider.supportsOAuth)
        // ProviderSummaryWire has no oauth_style, so the style is unknown and
        // the public id is the label.
        assertEquals(OAuthStyle.Unknown, provider.oauthStyle)
        assertEquals("p", provider.label)
    }

    // ---------- there is no validation-only door ----------

    @Test
    fun `validateApiKey refuses, because the wire validates only on commit`() = runTest {
        val result = FakeAccountsRepository().validateApiKey("openai", "fake-971-key".toCharArray())
        assertEquals(
            AccountResult.Failed(FakeAccountsRepository.VALIDATE_ONLY_UNAVAILABLE),
            result,
        )
    }

    // ---------- capabilities are not printable ----------

    @Test
    fun `a live OAuth flow does not print its authorization URL`() {
        val flow = OAuthFlow.Started(
            provider = "openai",
            alias = "work",
            flowId = "f",
            attemptId = "a",
            style = OAuthStyle.Unknown,
            authorizationUrl = "https://example.invalid/authorize?code_challenge=SECRET",
            userCode = "HAID-971",
            expiresAtMs = null,
        )
        assertTrue("the URL leaked into toString", !flow.toString().contains("SECRET"))
        val ready = OAuthStatus.Ready(oauthReference = "vaultref-SECRET", identity = null)
        assertTrue("the reference leaked into toString", !ready.toString().contains("SECRET"))
    }

    // ---------- the wire code, not the Rust constant's name ----------

    @Test
    fun `already_resolved is the code the daemon actually sends`() {
        assertEquals("already_resolved", ChatViewModel.ALREADY_RESOLVED)
    }
}
