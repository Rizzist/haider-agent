package ai.diffforge.haider.ui.accounts

/**
 * The phone's port of the desktop TUI's `+ Add custom server` card
 * (`crates/haider-tui/src/app.rs` `CustomProviderCard` / `CustomPhase` /
 * `CustomField`, pinned by `custom_provider_tests.rs`).
 *
 * The flow the TUI runs, and the only one that is honest against the frozen
 * wire, is four doors deep:
 *
 *  1. `vault.stage{purpose:"api_key"}` — keyed servers only. The plaintext is
 *     handed over once and the card's own copy is wiped in the same breath
 *     (`custom_server_key_paste_is_masked_and_debug_redacted`: "submit wiped
 *     the card's local copy");
 *  2. `provider.models_probe` — READ-ONLY discovery. It borrows the staged
 *     reference without consuming it and creates no provider, no account, no
 *     credential and no cache (`no_auth_and_anthropic_choices_probe_before_
 *     configuring_without_a_secret`);
 *  3. `provider.configure` — the durable create, carrying the discovered
 *     inventory and the chosen default, with `probe_vault_reference` ABSENT:
 *     the staged reference is not spent here;
 *  4. `account.login_api` — claims that same staged reference for the new
 *     provider (`keyed_custom_server_stages_discovers_picks_then_configures_
 *     and_consumes_one_reference`).
 *
 * A failed probe is not a dead end: the card falls back to a typed model id
 * ("type the model id"), and an EMPTY id still cannot configure.
 *
 * Nothing here holds the secret. [CustomServerForm] carries a masked LENGTH
 * only, so no `toString`, snapshot, log line or saved state can reconstruct a
 * key — the bytes live in a [SecretBuffer] the screen owns and wipes.
 */

/** `ProviderAuthRequirementWire` (frame.rs:1198), restricted to what a custom server may be. */
enum class CustomAuthMode(val wire: String) {
    /** A bearer key: stage, probe with it, configure, then claim it. */
    ApiKey("api_key"),

    /** A local server with no credential at all: no stage, no `login_api`. */
    None("none"),
}

/** `ProviderApiFamilyWire` (frame.rs:1133), restricted to the two a custom card offers. */
enum class CustomApiFamily(val wire: String, val label: String) {
    OpenAiChatCompletions("openai_chat_completions", "OpenAI-compatible"),
    AnthropicMessages("anthropic_messages", "Anthropic messages"),
}

/** `ProviderProbeFailureWire` (frame.rs:5737). Public coordinates only. */
enum class CustomProbeFailure(val wire: String) {
    Unreachable("unreachable"),
    Unauthorized("unauthorized"),
    NonCompatibleBody("non_open_ai_compatible_body"),
    EmptyList("empty_list"),
    Unavailable("unavailable"),
    Unknown("unknown"),
    ;

    companion object {
        fun of(wire: String?): CustomProbeFailure =
            entries.firstOrNull { it.wire == wire } ?: Unknown
    }
}

/** Where the card is in its flow (`CustomPhase`, app.rs:746). */
sealed interface CustomServerPhase {
    /** Typing name/origin/key. Also the retype state after a typed failure. */
    data class Editing(val error: String? = null) : CustomServerPhase

    /** Read-only discovery is in flight; a keyed card has already staged. */
    data object Probing : CustomServerPhase

    /**
     * Choose a discovered id, or type one when discovery failed.
     *
     * [models] empty with an [error] present is the manual-fallback state: the
     * server answered, the answer was unusable, and the person may still name a
     * passthrough id the chat wire will accept.
     */
    data class Choosing(
        val models: List<String>,
        val error: String? = null,
    ) : CustomServerPhase

    /** `provider.configure` (and, for a keyed card, `account.login_api`) in flight. */
    data object Submitting : CustomServerPhase
}

/**
 * The card's state. [maskedKeyLength] is the ONLY thing this object knows about
 * the key, and it exists so the field can render dots and the submit gate can
 * ask "is there one" without anybody holding the bytes.
 */
data class CustomServerForm(
    val name: String = DEFAULT_NAME,
    val origin: String = DEFAULT_ORIGIN,
    val authMode: CustomAuthMode = CustomAuthMode.ApiKey,
    val family: CustomApiFamily = CustomApiFamily.OpenAiChatCompletions,
    val model: String = "",
    val maskedKeyLength: Int = 0,
    val phase: CustomServerPhase = CustomServerPhase.Editing(),
    /**
     * Attempt identity. Every reply must correlate to it or die silently: the
     * TUI's `abandoned_probe_replies_cannot_overwrite_an_edited_connection`
     * pins that an answer to a superseded attempt may neither overwrite the
     * fields nor create a provider.
     */
    val attempt: Long = 1L,
) {
    /** A key is never printed, and neither is anything that could hint at one. */
    override fun toString(): String =
        "CustomServerForm(name=$name, origin=$origin, auth=$authMode, family=$family, " +
            "phase=${phase::class.simpleName}, attempt=$attempt, key=REDACTED)"

    val keyless: Boolean get() = authMode == CustomAuthMode.None

    val busy: Boolean
        get() = phase is CustomServerPhase.Probing || phase is CustomServerPhase.Submitting

    /**
     * Discovery may start when the connection is named and, for a keyed card,
     * a key has actually been typed. The TUI walks the same three gates before
     * it will issue a probe.
     */
    val canProbe: Boolean
        get() = !busy &&
            aliasOk(name) &&
            origin.isNotBlank() &&
            (keyless || maskedKeyLength > 0)

    /**
     * A create may go through only with a model. An enabled create without an
     * inventory and a default is refused by the daemon (`submit_custom_add`:
     * "the card refuses to submit what would bounce"), so it is refused here
     * rather than sent to bounce.
     */
    val canConfigure: Boolean
        get() = !busy &&
            phase is CustomServerPhase.Choosing &&
            aliasOk(name) &&
            origin.isNotBlank() &&
            modelOk(model)

    /** The inventory a `provider.configure` carries: exactly what discovery returned. */
    val discoveredModels: List<String>
        get() = (phase as? CustomServerPhase.Choosing)?.models.orEmpty()

    /**
     * The staged key is dropped whenever it stops being the key this card is
     * about: switching to no-auth, and submitting.
     *
     * `custom_server_key_paste_is_masked_and_debug_redacted` pins both — an
     * abandoned key surviving a mode switch is the finding, not a convenience.
     */
    fun withAuthMode(mode: CustomAuthMode): CustomServerForm =
        copy(authMode = mode, maskedKeyLength = if (mode == CustomAuthMode.None) 0 else maskedKeyLength)

    fun withKeyLength(length: Int): CustomServerForm = copy(maskedKeyLength = maxOf(0, length))

    /** Submitting hands the bytes to the vault and forgets them here. */
    fun submittedKey(): CustomServerForm = copy(maskedKeyLength = 0)

    fun probing(): CustomServerForm = copy(phase = CustomServerPhase.Probing, maskedKeyLength = 0)

    /** Discovery answered. The advertised default is pre-selected, as the TUI does. */
    fun probed(models: List<String>, defaultModel: String?): CustomServerForm = copy(
        phase = CustomServerPhase.Choosing(models = models),
        model = defaultModel?.takeIf { it in models } ?: models.firstOrNull().orEmpty(),
    )

    /**
     * Discovery failed with a typed class. The card stays open on the manual
     * fallback with the daemon's own reason — never a flash, never a toast:
     * "discovery errors stay on the card".
     */
    fun probeFailed(failure: CustomProbeFailure, detail: String?): CustomServerForm = copy(
        phase = CustomServerPhase.Choosing(models = emptyList(), error = probeErrorLine(failure, detail)),
        model = "",
    )

    /** A stage that failed reopens the same card at the key, with nothing in it. */
    fun stageFailed(code: String, message: String?): CustomServerForm = copy(
        phase = CustomServerPhase.Editing(
            error = listOfNotNull(code, message?.takeIf(String::isNotBlank)).joinToString(" — "),
        ),
        maskedKeyLength = 0,
    )

    fun configureFailed(code: String): CustomServerForm = copy(
        phase = CustomServerPhase.Choosing(models = discoveredModels, error = code),
    )

    companion object {
        /** The TUI's own starting name and loopback origin (app.rs:11899). */
        const val DEFAULT_NAME = "custom"
        const val DEFAULT_ORIGIN = "http://127.0.0.1:8000/v1"

        /**
         * The alias is both the provider identity and the account alias, so it
         * must be usable in either daemon-owned namespace: non-empty, no
         * whitespace and no control characters.
         */
        fun aliasOk(alias: String): Boolean =
            alias.isNotBlank() &&
                alias.length <= 64 &&
                alias.none { it.isWhitespace() || it.isISOControl() }

        /** A model id is passthrough text; only emptiness and control bytes are ours to refuse. */
        fun modelOk(model: String): Boolean {
            val trimmed = model.trim()
            return trimmed.isNotEmpty() && trimmed.none(Char::isISOControl)
        }

        /**
         * The daemon's typed failure, in a sentence, plus the fallback the card
         * is now offering. The TUI's own wording is kept so the two clients say
         * the same thing about the same failure.
         */
        fun probeErrorLine(failure: CustomProbeFailure, detail: String?): String {
            val head = when (failure) {
                CustomProbeFailure.Unreachable -> "The server did not answer"
                CustomProbeFailure.Unauthorized -> "The API key unauthorized the model list"
                CustomProbeFailure.NonCompatibleBody -> "The model list was not a compatible document"
                CustomProbeFailure.EmptyList -> "The server listed no models"
                CustomProbeFailure.Unavailable -> "Model discovery is unavailable on this server"
                CustomProbeFailure.Unknown -> "Model discovery failed"
            }
            return listOfNotNull(head, detail?.takeIf(String::isNotBlank))
                .joinToString(" — ") + ". You can type the model id instead."
        }
    }
}

/**
 * Which add form is pending. There is at most one, ever.
 *
 * 971-tui-fixes F1(b): choosing another provider while an add form is pending
 * REPLACES it — never two forms at once. The phone reached the same shape from
 * the other side (one `AddMode`), so the rule that needs stating here is the
 * one that goes with it: whatever was typed into the outgoing form, including
 * a staged key, is discarded with it.
 */
enum class AddAccountForm { None, ApiKey, SignIn, CustomServer }

object AddAccountFormPolicy {

    /** Opening a form always replaces the pending one. */
    fun open(requested: AddAccountForm): AddAccountForm = requested

    /**
     * True when leaving [from] for [to] must wipe the secret buffer and drop a
     * staged reference. Any change of form does, including closing one.
     */
    fun discardsSecret(from: AddAccountForm, to: AddAccountForm): Boolean = from != to

    /** True when the person has typed something a silent replacement would lose. */
    fun hasUnsavedInput(form: AddAccountForm, alias: String, keyLength: Int): Boolean =
        form != AddAccountForm.None && (alias.isNotBlank() || keyLength > 0)

    /**
     * Which providers an add form may pick from.
     *
     * The gate is the auth method the provider declares, **not**
     * `ProviderDescriptor.available`. Availability answers "can this provider
     * serve a turn right now", and on a fresh profile the honest answer is no
     * *because there is no credential yet* — so gating the credential form on
     * it greyed out every built-in provider and left Save permanently disabled
     * with nothing a person could do about it (971-V F5, `api35-key-form.png`).
     *
     * An unavailable provider is still shown with its reason; what changes is
     * that it can be chosen, which is the only way the reason ever goes away.
     */
    fun selectable(form: AddAccountForm, supportsApiKey: Boolean, supportsOAuth: Boolean): Boolean =
        when (form) {
            AddAccountForm.ApiKey -> supportsApiKey
            AddAccountForm.SignIn -> supportsOAuth
            AddAccountForm.CustomServer, AddAccountForm.None -> false
        }
}
