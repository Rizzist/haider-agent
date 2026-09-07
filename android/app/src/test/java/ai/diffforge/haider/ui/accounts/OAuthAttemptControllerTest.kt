package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The attempt has to outlive the screen. Returning through
 * `haider://oauth/return` navigates to Settings, which disposes Accounts — and
 * a `remember`ed flow id would be gone exactly when `account.oauth_status`
 * needs it. The contract is explicit that the poll uses the in-memory
 * flow_id/attempt_id on the original connection, and that a lost flow is
 * restarted rather than resumed.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class OAuthAttemptControllerTest {

    private fun controller(
        scope: TestScope,
        repository: FakeAccountsRepository = FakeAccountsRepository(),
    ) = repository to OAuthAttemptController(
        repository = repository,
        scope = scope,
        pollIntervalMs = 100,
        attemptIds = { "attempt-fixed" },
    )

    @Test
    fun `the attempt survives the composition that started it`() = runTest {
        val (_, oauth) = controller(this)
        val url = oauth.start("anthropic", "personal")
        assertNotNull(url)
        // Nothing about the attempt lives in a composable; disposing the screen
        // is a no-op as far as the flow is concerned.
        assertNotNull("the attempt died with its screen", oauth.attempt.value)
        assertEquals("attempt-fixed", oauth.attempt.value!!.flow.attemptId)
        oauth.cancel()
        advanceUntilIdle()
    }

    @Test
    fun `polling walks waiting then exchanging then commits`() = runTest {
        val (repository, oauth) = controller(this)
        oauth.start("anthropic", "personal")
        assertEquals(OAuthAttemptController.Phase.Waiting, oauth.attempt.value!!.phase)
        advanceTimeBy(150)
        assertEquals(OAuthAttemptController.Phase.Waiting, oauth.attempt.value!!.phase)
        advanceTimeBy(100)
        // The provider's success page arrives before the exchange finishes.
        assertEquals(OAuthAttemptController.Phase.Exchanging, oauth.attempt.value!!.phase)
        advanceUntilIdle()
        assertNull(oauth.attempt.value)
        assertTrue(oauth.notice.value!!.startsWith("Signed in"))
        assertTrue(repository.accountExists("anthropic", "personal"))
        assertTrue(repository.calls.contains(AccountsRpcAdapter.METHOD_ACCOUNT_ADD))
    }

    @Test
    fun `a lost flow asks whether the commit landed instead of resuming`() = runTest {
        val (repository, oauth) = controller(this)
        oauth.start("anthropic", "personal")
        repository.flowLost = true
        advanceUntilIdle()
        assertNull(oauth.attempt.value)
        assertEquals("The sign-in was lost — start it again.", oauth.notice.value)
        assertTrue(repository.calls.contains(AccountsRpcAdapter.METHOD_OAUTH_STATUS))
    }

    @Test
    fun `a lost flow whose commit already landed says so`() = runTest {
        val repository = FakeAccountsRepository()
        val (_, oauth) = controller(this, repository)
        oauth.start("anthropic", "personal")
        // The commit landed on the connection that then died.
        repository.completeOAuth(oauth.attempt.value!!.flow, "ref")
        repository.flowLost = true
        advanceUntilIdle()
        assertEquals("Signed in: personal", oauth.notice.value)
    }

    @Test
    fun `cancelling stops the poll and tells the daemon`() = runTest {
        val (repository, oauth) = controller(this)
        oauth.start("anthropic", null)
        oauth.cancel()
        advanceUntilIdle()
        assertNull(oauth.attempt.value)
        assertTrue(repository.calls.contains(AccountsRpcAdapter.METHOD_OAUTH_CANCEL))
    }

    @Test
    fun `an unavailable provider produces a notice, not a phantom attempt`() = runTest {
        val (_, oauth) = controller(this)
        assertNull(oauth.start("google", null))
        assertNull(oauth.attempt.value)
        assertNotNull(oauth.notice.value)
    }

    @Test
    fun `starting again abandons the previous attempt`() = runTest {
        val repository = FakeAccountsRepository()
        val oauth = OAuthAttemptController(
            repository = repository,
            scope = TestScope(StandardTestDispatcher(testScheduler)),
            pollIntervalMs = 100,
            attemptIds = { "attempt-" + repository.calls.size },
        )
        oauth.start("anthropic", "one")
        val first = oauth.attempt.value!!.flow.attemptId
        oauth.start("anthropic", "two")
        val second = oauth.attempt.value!!.flow.attemptId
        assertTrue("a restart must be a new attempt", first != second)
        oauth.cancel()
        advanceUntilIdle()
    }
}
