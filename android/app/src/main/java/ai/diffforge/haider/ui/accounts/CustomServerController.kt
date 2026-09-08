package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Drives the `+ Add custom server` flow across its four doors, outside the
 * composition, for the same reason [OAuthAttemptController] is: the key must be
 * wiped whatever ends the screen, and a reply that arrives for an abandoned
 * attempt must die silently rather than overwrite whatever is on screen now.
 *
 * The order is stage -> probe -> configure -> claim, and each step is refused
 * unless the one before it actually returned:
 *
 *  - a probe never runs without a staged reference on a keyed card, and never
 *    carries one on a keyless card;
 *  - `provider.configure` never runs from the editing phase — only from a
 *    chosen model, so an enabled create always carries an inventory and a
 *    default (the daemon refuses otherwise);
 *  - `account.login_api` runs only after the provider exists, and claims the
 *    SAME reference the probe borrowed. There is exactly one stage per attempt.
 *
 * The secret lives in a [SecretBuffer] this controller owns. [CustomServerForm]
 * knows only its length; nothing here returns, logs or renders the bytes.
 */
class CustomServerController(private val repository: AccountsRepository) {

    private val _form = MutableStateFlow<CustomServerForm?>(null)
    val form: StateFlow<CustomServerForm?> = _form.asStateFlow()

    private val _notice = MutableStateFlow<String?>(null)
    val notice: StateFlow<String?> = _notice.asStateFlow()

    private val secret = SecretBuffer()

    /**
     * The reference `vault.stage` returned for THIS attempt. The probe borrows
     * it; `account.login_api` spends it. It is dropped whenever the attempt is.
     */
    private var stagedReference: String? = null

    private var attempts = 0L

    /** Method names only — an argument here could be the key. */
    val calls = mutableListOf<String>()

    /** Opens a fresh card, replacing and wiping any pending one (971-tui-fixes F1b). */
    fun open() {
        forget()
        attempts += 1
        _notice.value = null
        _form.value = CustomServerForm(attempt = attempts)
    }

    /** Closes the card and wipes everything it was holding. */
    fun close() {
        forget()
        _form.value = null
    }

    /** Edits a field. Editing invalidates nothing else; the phase is untouched. */
    fun edit(transform: (CustomServerForm) -> CustomServerForm) {
        _form.value = _form.value?.let(transform)
    }

    /**
     * The key, straight into the buffer.
     *
     * Switching to no-auth wipes it through [CustomServerForm.withAuthMode]; the
     * buffer has to follow, or an abandoned key would outlive the field that
     * showed it.
     */
    fun setKey(value: String) {
        secret.set(value)
        edit { it.withKeyLength(secret.length) }
    }

    fun setAuthMode(mode: CustomAuthMode) {
        if (mode == CustomAuthMode.None) secret.wipe()
        edit { it.withAuthMode(mode) }
    }

    /** Wipes the secret and the staged reference. Idempotent. */
    fun forget() {
        secret.wipe()
        stagedReference = null
    }

    fun clearNotice() {
        _notice.value = null
    }

    /**
     * One step of the flow, chosen by the phase the card is in.
     *
     * Returns true when the server was created (the caller then closes the card
     * and re-reads the account list).
     */
    suspend fun submit(): Boolean {
        val current = _form.value ?: return false
        return when (current.phase) {
            is CustomServerPhase.Editing -> {
                discover(current)
                false
            }
            is CustomServerPhase.Choosing -> create(current)
            CustomServerPhase.Probing, CustomServerPhase.Submitting -> false
        }
    }

    /** Steps 1 and 2: stage the key, then run the read-only probe. */
    private suspend fun discover(current: CustomServerForm) {
        if (!current.canProbe) return
        val attempt = current.attempt
        // The moment the vault has it, this card does not. The TUI pins the
        // same thing: "submit wiped the card's local copy".
        _form.value = current.probing()
        val reference = if (current.keyless) {
            null
        } else {
            calls += AccountsRpcAdapter.METHOD_VAULT_STAGE
            val staged = secret.use { repository.stageApiKey(it) }
            secret.wipe()
            if (staged == null) {
                if (live(attempt)) _form.value = current.stageFailed(STAGING_UNAVAILABLE, null)
                return
            }
            stagedReference = staged
            staged
        }
        calls += AccountsRpcAdapter.METHOD_PROVIDER_MODELS_PROBE
        val probe = repository.probeCustomModels(
            provider = current.name.trim(),
            origin = current.origin.trim(),
            apiFamily = current.family.wire,
            keyless = current.keyless,
            probeVaultReference = reference,
        )
        // A reply for an attempt that is no longer on screen dies here: it may
        // neither repopulate the fields nor create anything.
        if (!live(attempt)) return
        val card = _form.value ?: return
        _form.value = when (probe) {
            is CustomModelsProbe.Models ->
                if (probe.models.isEmpty()) {
                    card.probeFailed(CustomProbeFailure.EmptyList, null)
                } else {
                    card.probed(probe.models, probe.defaultModel)
                }
            is CustomModelsProbe.Failed -> card.probeFailed(probe.failure, probe.detail)
            is CustomModelsProbe.Unavailable ->
                card.probeFailed(CustomProbeFailure.Unavailable, probe.reason)
        }
    }

    /** Steps 3 and 4: create the provider, then claim the staged key for it. */
    private suspend fun create(current: CustomServerForm): Boolean {
        if (!current.canConfigure) return false
        val attempt = current.attempt
        val name = current.name.trim()
        val model = current.model.trim()
        // Exactly what discovery returned, plus the id actually chosen. A
        // manually typed fallback id IS the whole inventory: the daemon was
        // told no list, so claiming one would be a fabricated catalog.
        val models = current.discoveredModels.ifEmpty { listOf(model) }
        _form.value = current.copy(phase = CustomServerPhase.Submitting)
        calls += AccountsRpcAdapter.METHOD_PROVIDER_CONFIGURE
        val configured = repository.configureCustomProvider(
            provider = name,
            origin = current.origin.trim(),
            apiFamily = current.family.wire,
            authRequirement = current.authMode.wire,
            models = if (model in models) models else models + model,
            defaultModel = model,
            expectedRevision = repository.providerRevision.value ?: NO_REVISION_FENCE,
        )
        if (!live(attempt)) return false
        if (configured is AccountResult.Failed) {
            _form.value = current.configureFailed(configured.publicCode)
            return false
        }
        val reference = stagedReference
        if (!current.keyless && reference != null) {
            calls += AccountsRpcAdapter.METHOD_ACCOUNT_LOGIN_API
            val claimed = repository.commitStagedApiKey(name, name, reference)
            if (!live(attempt)) return false
            if (claimed is AccountResult.Failed) {
                // The provider exists; the credential does not. Say exactly
                // that rather than reporting a create that half happened.
                forget()
                _notice.value = claimed.publicCode
                _form.value = null
                repository.refresh()
                return true
            }
        }
        forget()
        _notice.value = null
        _form.value = null
        repository.refresh()
        repository.refreshProviders()
        return true
    }

    private fun live(attempt: Long): Boolean = _form.value?.attempt == attempt

    companion object {
        /** Staging failed for a reason that is not the key: connection, expiry, capacity. */
        const val STAGING_UNAVAILABLE = "staging_unavailable"

        /**
         * `provider.configure` requires an `expected_revision`, and a daemon
         * that published none leaves nothing to fence against. The TUI sends
         * zero here (`self.providers.revision.unwrap_or(0)`); this is the same
         * substitution, named so it is visible rather than inlined.
         */
        const val NO_REVISION_FENCE = 0L
    }
}
