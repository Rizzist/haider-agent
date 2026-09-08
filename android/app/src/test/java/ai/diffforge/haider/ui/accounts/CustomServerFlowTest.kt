package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.launch
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The custom-server flow, door by door, against the TUI's own pins.
 *
 * `crates/haider-tui/src/custom_provider_tests.rs` is the reference for every
 * assertion here: the order of the four doors, exactly one stage per attempt,
 * a key that is gone from the card the moment it is submitted, a probe that
 * creates nothing, and a reply for an abandoned attempt that changes nothing.
 */
class CustomServerFlowTest {

    private fun controller(repository: FakeAccountsRepository = FakeAccountsRepository()) =
        CustomServerController(repository) to repository

    // ---------- wire shapes ----------

    @Test
    fun `the probe is read-only and carries no command id`() {
        val body = AccountsRpcAdapter.providerModelsProbeRequest(
            provider = "custom",
            origin = "https://router.example.test/v1",
            apiFamily = "openai_chat_completions",
            keyless = false,
            probeVaultReference = "vault-ref-custom-1",
        )
        assertEquals("provider.models_probe", body.getString("method"))
        // Discovery is not a durable mutation; the TUI pins the same absence.
        assertFalse(body.has("command_id"))
        assertEquals("vault-ref-custom-1", body.getString("probe_vault_reference"))
        assertFalse(body.getBoolean("keyless"))
    }

    @Test
    fun `a keyless probe carries no reference at all`() {
        val body = AccountsRpcAdapter.providerModelsProbeRequest(
            provider = "local",
            origin = "http://127.0.0.1:11434/v1",
            apiFamily = "anthropic_messages",
            keyless = true,
        )
        assertTrue(body.getBoolean("keyless"))
        assertFalse(body.has("probe_vault_reference"))
        assertEquals("anthropic_messages", body.getString("api_family"))
    }

    @Test
    fun `provider configure matches the transcript body`() {
        // wire_transcript.json entry 57, verbatim minus the envelope.
        val body = AccountsRpcAdapter.providerConfigureRequest(
            commandId = "command-provider-configure",
            provider = "local-lab",
            origin = "http://127.0.0.1:11434",
            apiFamily = "openai_chat_completions",
            authRequirement = "none",
            models = listOf("local-frontier-a"),
            defaultModel = "local-frontier-a",
            expectedRevision = 10,
        )
        assertEquals("provider.configure", body.getString("method"))
        assertEquals("command-provider-configure", body.getString("command_id"))
        assertEquals("local-lab", body.getString("provider"))
        assertEquals("openai_chat_completions", body.getString("api_family"))
        assertEquals("http://127.0.0.1:11434", body.getString("origin"))
        assertEquals("none", body.getString("auth_requirement"))
        assertTrue(body.getBoolean("enabled"))
        assertEquals("local-frontier-a", body.getJSONArray("models").getString(0))
        assertEquals("local-frontier-a", body.getString("default_model"))
        assertEquals(10L, body.getLong("expected_revision"))
        // The probe already borrowed the stage; `account.login_api` spends it.
        assertFalse(body.has("probe_vault_reference"))
    }

    @Test
    fun `a discovered provider publishes an advisory inventory`() {
        val summary = AccountsRpcAdapter.parseProviders(
            JSONObject(
                """{"providers":[{"provider":"local-lab","endpoint":"http://127.0.0.1:11434",
                     "auth_methods":["api_key"],"availability":"available","enabled":true,
                     "models":["router-fast"],"inventory_authority":"advisory"}]}""",
            ),
        ).single()
        assertEquals(ModelInventoryAuthority.Advisory, summary.inventoryAuthority)
        assertTrue(summary.inventoryAuthority.acceptsCustomModelId)
        assertEquals("http://127.0.0.1:11434", summary.endpoint)
    }

    @Test
    fun `an unstated authority stays unknown and licenses nothing`() {
        val summary = AccountsRpcAdapter.parseProviders(
            JSONObject("""{"providers":[{"provider":"anthropic"}]}"""),
        ).single()
        assertEquals(ModelInventoryAuthority.Unknown, summary.inventoryAuthority)
        assertFalse(summary.inventoryAuthority.acceptsCustomModelId)
        assertFalse(ModelInventoryAuthority.Authoritative.acceptsCustomModelId)
    }

    @Test
    fun `a typed probe failure is read from the error data`() {
        val failure = AccountsRpcAdapter.parseProbeFailure(
            JSONObject("""{"kind":"provider_probe_failed","provider":"custom","failure":"unauthorized"}"""),
        )
        assertEquals(CustomProbeFailure.Unauthorized, failure)
        assertEquals(CustomProbeFailure.Unknown, AccountsRpcAdapter.parseProbeFailure(null))
    }

    // ---------- the flow ----------

    @Test
    fun `a keyed server stages once, probes, configures, then claims the same reference`() =
        runTest {
            val (custom, repository) = controller()
            custom.open()
            custom.edit { it.copy(name = "custom", origin = "https://router.example.test/v1") }
            custom.setKey("CUSTOM_KEY_SENTINEL_4f21")

            custom.submit()

            // Probe only. Nothing was configured and no account exists yet.
            assertEquals(
                listOf(
                    AccountsRpcAdapter.METHOD_VAULT_STAGE,
                    AccountsRpcAdapter.METHOD_PROVIDER_MODELS_PROBE,
                ),
                custom.calls,
            )
            assertTrue(repository.configured.isEmpty())
            // The stage was BORROWED, not consumed.
            assertEquals(listOf("vaultref-24"), repository.probedReferences)
            val choosing = custom.form.value!!.phase as CustomServerPhase.Choosing
            assertEquals(listOf("router-fast", "router-deep"), choosing.models)
            // The advertised default is pre-selected.
            assertEquals("router-deep", custom.form.value!!.model)
            // Submitting wiped the card's copy of the key.
            assertEquals(0, custom.form.value!!.maskedKeyLength)

            val created = custom.submit()

            assertTrue(created)
            val configured = repository.configured.single()
            assertEquals("custom", configured.provider)
            assertEquals("openai_chat_completions", configured.apiFamily)
            assertEquals("api_key", configured.authRequirement)
            // Exactly what discovery returned, plus the id actually chosen.
            assertEquals(listOf("router-fast", "router-deep"), configured.models)
            assertEquals("router-deep", configured.defaultModel)
            assertEquals(FakeAccountsRepository.PROVIDER_SEED_REVISION, configured.expectedRevision)
            // …and then ONE `account.login_api`, claiming the staged reference.
            assertEquals(
                listOf(
                    AccountsRpcAdapter.METHOD_VAULT_STAGE,
                    AccountsRpcAdapter.METHOD_PROVIDER_MODELS_PROBE,
                    AccountsRpcAdapter.METHOD_PROVIDER_CONFIGURE,
                    AccountsRpcAdapter.METHOD_ACCOUNT_LOGIN_API,
                ),
                custom.calls,
            )
            assertTrue(repository.snapshot.value.accounts.any { it.alias == "custom" })
            assertNull(custom.form.value)
        }

    @Test
    fun `a keyless server never stages and never logs in`() = runTest {
        val (custom, repository) = controller()
        custom.open()
        custom.setAuthMode(CustomAuthMode.None)
        custom.edit { it.copy(name = "local", origin = "http://127.0.0.1:11434/v1") }

        custom.submit()
        custom.submit()

        assertEquals(
            listOf(
                AccountsRpcAdapter.METHOD_PROVIDER_MODELS_PROBE,
                AccountsRpcAdapter.METHOD_PROVIDER_CONFIGURE,
            ),
            custom.calls,
        )
        assertEquals(listOf<String?>(null), repository.probedReferences)
        assertEquals("none", repository.configured.single().authRequirement)
        // No credential is stored for a server that does not ask for one.
        assertTrue(repository.snapshot.value.accounts.none { it.alias == "local" })
    }

    @Test
    fun `switching to no auth wipes an abandoned key`() = runTest {
        val (custom, _) = controller()
        custom.open()
        custom.setKey("CUSTOM_KEY_SENTINEL_4f21")
        assertEquals(24, custom.form.value!!.maskedKeyLength)

        custom.setAuthMode(CustomAuthMode.None)

        assertEquals(0, custom.form.value!!.maskedKeyLength)
        assertTrue(custom.form.value!!.keyless)
    }

    @Test
    fun `the form never prints the key, only its length`() {
        val form = CustomServerForm().withKeyLength(24)
        val text = form.toString()
        assertTrue(text.contains("REDACTED"))
        assertFalse(text.contains("24"))
        // The form carries a length, not a value: there is nothing else to leak.
        assertEquals(24, form.maskedKeyLength)
    }

    @Test
    fun `a failed stage reopens the card with nothing in it`() = runTest {
        val repository = FakeAccountsRepository().apply { stagingUnavailable = true }
        val (custom, _) = controller(repository)
        custom.open()
        custom.setKey("CUSTOM_KEY_SENTINEL_4f21")

        custom.submit()

        val form = custom.form.value!!
        val editing = form.phase as CustomServerPhase.Editing
        assertTrue(editing.error!!.contains(CustomServerController.STAGING_UNAVAILABLE))
        assertEquals(0, form.maskedKeyLength)
        // A stage that failed is not a probe: nothing was discovered.
        assertTrue(custom.calls.none { it == AccountsRpcAdapter.METHOD_PROVIDER_MODELS_PROBE })
    }

    @Test
    fun `a failed probe falls back to a typed model id, and an empty one cannot create`() =
        runTest {
            val repository = FakeAccountsRepository().apply {
                nextProbe = CustomModelsProbe.Failed(
                    CustomProbeFailure.Unauthorized,
                    "server returned 401 for /models",
                )
            }
            val (custom, _) = controller(repository)
            custom.open()
            custom.setKey("throw-away-key")
            custom.submit()

            val failed = custom.form.value!!
            val choosing = failed.phase as CustomServerPhase.Choosing
            assertTrue(choosing.models.isEmpty())
            assertTrue(choosing.error!!.contains("unauthorized", ignoreCase = true))
            assertTrue(choosing.error.contains("401"))
            assertTrue(choosing.error.contains("type the model id"))
            // An empty manual id cannot configure.
            assertFalse(failed.canConfigure)
            assertFalse(custom.submit())
            assertTrue(repository.configured.isEmpty())

            custom.edit { it.copy(model = "llama3.1:8b") }
            assertTrue(custom.submit())
            val configured = repository.configured.single()
            // A manually typed id IS the whole inventory: no catalog is claimed.
            assertEquals(listOf("llama3.1:8b"), configured.models)
            assertEquals("llama3.1:8b", configured.defaultModel)
        }

    @Test
    fun `an empty discovered list is a failure, not an empty picker`() = runTest {
        val repository = FakeAccountsRepository().apply {
            nextProbe = CustomModelsProbe.Models(emptyList(), null)
        }
        val (custom, _) = controller(repository)
        custom.open()
        custom.setAuthMode(CustomAuthMode.None)
        custom.submit()

        val choosing = custom.form.value!!.phase as CustomServerPhase.Choosing
        assertTrue(choosing.models.isEmpty())
        assertTrue(choosing.error!!.contains("listed no models"))
    }

    @Test
    fun `a probe reply for an abandoned attempt cannot repopulate the card`() = runTest {
        val repository = FakeAccountsRepository().apply { holdProbe = true }
        val (custom, _) = controller(repository)
        custom.open()
        custom.setAuthMode(CustomAuthMode.None)
        custom.edit { it.copy(name = "first", origin = "http://first.test/v1") }
        val first = custom.form.value!!.attempt
        val inFlight = launch { custom.submit() }
        runCurrent()

        // The person cancels and starts again while discovery is out. The reply
        // that lands belongs to the attempt that is gone.
        custom.open()
        custom.edit { it.copy(name = "second", origin = "http://second.test/v1") }
        assertTrue(custom.form.value!!.attempt > first)
        repository.releaseProbe()
        inFlight.join()

        val card = custom.form.value!!
        assertEquals("second", card.name)
        assertTrue(card.phase is CustomServerPhase.Editing)
        // The stale inventory did not arrive, and nothing was created.
        assertEquals("", card.model)
        assertTrue(repository.configured.isEmpty())
    }

    @Test
    fun `a refused configure keeps the discovered list and says why`() = runTest {
        val repository = FakeAccountsRepository()
        val (custom, _) = controller(repository)
        custom.open()
        custom.setAuthMode(CustomAuthMode.None)
        custom.submit()
        repository.nextFailure = "provider_revision_conflict"

        assertFalse(custom.submit())

        val choosing = custom.form.value!!.phase as CustomServerPhase.Choosing
        assertEquals(listOf("router-fast", "router-deep"), choosing.models)
        assertEquals("provider_revision_conflict", choosing.error)
    }

    @Test
    fun `closing the card forgets the key and the staged reference`() = runTest {
        val (custom, _) = controller()
        custom.open()
        custom.setKey("CUSTOM_KEY_SENTINEL_4f21")
        custom.close()
        assertNull(custom.form.value)
        // Reopening starts from nothing rather than the abandoned attempt.
        custom.open()
        assertEquals(0, custom.form.value!!.maskedKeyLength)
        assertEquals(CustomServerForm.DEFAULT_NAME, custom.form.value!!.name)
    }

    // ---------- gates and the one-pending-form rule ----------

    @Test
    fun `the submit gates refuse what the daemon would bounce`() {
        val blank = CustomServerForm(name = "", maskedKeyLength = 4)
        assertFalse(blank.canProbe)
        val spaced = CustomServerForm(name = "my server", maskedKeyLength = 4)
        assertFalse(spaced.canProbe)
        val noOrigin = CustomServerForm(origin = "  ", maskedKeyLength = 4)
        assertFalse(noOrigin.canProbe)
        val noKey = CustomServerForm()
        assertFalse(noKey.canProbe)
        assertTrue(CustomServerForm(maskedKeyLength = 4).canProbe)
        assertTrue(CustomServerForm().withAuthMode(CustomAuthMode.None).canProbe)
        // An enabled create needs a model, and a control-bearing id is not one.
        val choosing = CustomServerForm(phase = CustomServerPhase.Choosing(listOf("a")))
        assertFalse(choosing.canConfigure)
        assertTrue(choosing.copy(model = "a").canConfigure)
        assertFalse(choosing.copy(model = "a" + 9.toChar() + "b").canConfigure)
    }

    @Test
    fun `opening a form replaces the pending one and discards what it held`() {
        // 971-tui-fixes F1b: never two forms at once.
        assertEquals(
            AddAccountForm.CustomServer,
            AddAccountFormPolicy.open(AddAccountForm.CustomServer),
        )
        assertTrue(
            AddAccountFormPolicy.discardsSecret(AddAccountForm.ApiKey, AddAccountForm.CustomServer),
        )
        assertTrue(
            AddAccountFormPolicy.discardsSecret(AddAccountForm.CustomServer, AddAccountForm.None),
        )
        assertFalse(
            AddAccountFormPolicy.discardsSecret(AddAccountForm.ApiKey, AddAccountForm.ApiKey),
        )
        assertTrue(
            AddAccountFormPolicy.hasUnsavedInput(AddAccountForm.CustomServer, "", 4),
        )
        assertFalse(AddAccountFormPolicy.hasUnsavedInput(AddAccountForm.None, "x", 4))
    }
}
