package ai.diffforge.haider.ui.state

/**
 * What a refused `session.select_model` / `session.select_effort` needs next.
 *
 * The refusal panel offered "Change it anyway" for every refusal, because there
 * was only ever one kind of refusal in front of it: a cache-epoch confirmation.
 * The daemon has four more, and none of them is fixed by confirming:
 *
 *  - `model_unknown` — the model is outside the implied provider's KNOWN
 *    discovered inventory (frame.rs:257). `confirm_new_epoch` does not add it;
 *  - `provider_unavailable` — the row's provider is not creatable on this
 *    daemon (frame.rs:252);
 *  - `effort_unsupported` / `fast_unsupported` — the pair does not declare that
 *    level (frame.rs:262, 267).
 *
 * Offering a confirm button for those is a promise the retry cannot keep: it
 * sends the identical request with one more field and is refused identically.
 * So the panel asks this first, and a terminal refusal gets an explanation and
 * a way out instead of a button that does nothing.
 */
enum class SelectionRefusalKind {
    /** A second-step confirmation is exactly what the daemon asked for. */
    Confirmable,

    /** The daemon refused the row itself; a retry with consent is the same request. */
    Terminal,

    /**
     * A code this client does not recognise — a lost connection, a newer
     * daemon. Nothing is claimed about it, and the confirm is not offered,
     * because inventing consent for an unknown refusal is the failure this
     * whole path exists to prevent.
     */
    Unrecognised,
}

/** The stable refusal codes, spelled as the daemon sends them. */
object SelectionRefusalCodes {
    /** `ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED` (frame.rs:271). */
    const val CACHE_EPOCH_CONFIRMATION_REQUIRED = "cache_epoch_confirmation_required"

    /** `ERROR_CODE_MODEL_UNKNOWN` (frame.rs:257). */
    const val MODEL_UNKNOWN = "model_unknown"

    /** `ERROR_CODE_PROVIDER_UNAVAILABLE` (frame.rs:252). */
    const val PROVIDER_UNAVAILABLE = "provider_unavailable"

    /** `ERROR_CODE_EFFORT_UNSUPPORTED` (frame.rs:262). */
    const val EFFORT_UNSUPPORTED = "effort_unsupported"

    /** `ERROR_CODE_FAST_UNSUPPORTED` (frame.rs:266). */
    const val FAST_UNSUPPORTED = "fast_unsupported"

    private val terminal = setOf(
        MODEL_UNKNOWN,
        PROVIDER_UNAVAILABLE,
        EFFORT_UNSUPPORTED,
        FAST_UNSUPPORTED,
    )

    /**
     * Classifies what the facade reported.
     *
     * [text] is whatever the refusal carried — the bare code on a typed refusal,
     * or a message that contains it. Matching is on a whole token so
     * `model_unknown_thing` is not read as `model_unknown`, and the default is
     * [SelectionRefusalKind.Unrecognised]: an unknown refusal is never promoted
     * to confirmable.
     */
    fun kind(text: String): SelectionRefusalKind {
        val tokens = TOKEN.split(text).filter(String::isNotEmpty).toSet()
        return when {
            CACHE_EPOCH_CONFIRMATION_REQUIRED in tokens -> SelectionRefusalKind.Confirmable
            tokens.any { it in terminal } -> SelectionRefusalKind.Terminal
            else -> SelectionRefusalKind.Unrecognised
        }
    }

    /** Anything that is not part of a snake_case code separates two of them. */
    private val TOKEN = Regex("[^A-Za-z0-9_]+")
}
