package ai.diffforge.haider.ui.accounts

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Which providers a fresh profile may pick (971-V F5).
 *
 * The finding was circular: `provider.list` reports a built-in provider
 * unavailable until it has a credential, the add form gated its picker on that
 * availability, and so the only way to make a provider available was closed by
 * the provider not being available.
 */
class AccountSetupPolicyTest {

    @Test
    fun `an api-key form offers every provider that accepts a key`() {
        assertTrue(
            AddAccountFormPolicy.selectable(
                form = AddAccountForm.ApiKey,
                supportsApiKey = true,
                supportsOAuth = false,
            ),
        )
    }

    @Test
    fun `availability is not part of the gate`() {
        // The policy takes only the declared auth methods, so an unavailable
        // provider on a fresh profile is still selectable — which is the only
        // way its reason ever goes away.
        assertTrue(
            AddAccountFormPolicy.selectable(
                form = AddAccountForm.ApiKey,
                supportsApiKey = true,
                supportsOAuth = true,
            ),
        )
        assertTrue(
            AddAccountFormPolicy.selectable(
                form = AddAccountForm.SignIn,
                supportsApiKey = false,
                supportsOAuth = true,
            ),
        )
    }

    @Test
    fun `a form never offers an auth method the provider does not declare`() {
        assertFalse(
            AddAccountFormPolicy.selectable(
                form = AddAccountForm.SignIn,
                supportsApiKey = true,
                supportsOAuth = false,
            ),
        )
        assertFalse(
            AddAccountFormPolicy.selectable(
                form = AddAccountForm.ApiKey,
                supportsApiKey = false,
                supportsOAuth = true,
            ),
        )
    }

    @Test
    fun `the custom-server card and no form pick nothing from this list`() {
        // The custom card names its own server; there is no built-in row to
        // choose, and `None` has no form on screen at all.
        assertFalse(
            AddAccountFormPolicy.selectable(
                form = AddAccountForm.CustomServer,
                supportsApiKey = true,
                supportsOAuth = true,
            ),
        )
        assertFalse(
            AddAccountFormPolicy.selectable(
                form = AddAccountForm.None,
                supportsApiKey = true,
                supportsOAuth = true,
            ),
        )
    }
}
