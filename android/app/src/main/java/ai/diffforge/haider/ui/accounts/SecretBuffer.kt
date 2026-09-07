package ai.diffforge.haider.ui.accounts

/**
 * A transient owner for one secret.
 *
 * The point is the `finally`. A suspending call can be cancelled — the user
 * taps back, the composition is disposed, the screen navigates away — and a
 * `wipe()` written *after* the call simply never runs. Every read here is
 * wrapped, so the bytes are cleared on success, on failure, and on
 * cancellation alike.
 *
 * The buffer holds `CharArray`, not `String`, because a `String` cannot be
 * zeroed: it lives until the collector decides otherwise. Compose's text
 * pipeline does hand us a `String` while the user types (that is the
 * platform's, not ours), which is exactly why the field is masked and the
 * value is moved into a buffer and wiped the moment it is used.
 */
class SecretBuffer {
    private var chars: CharArray = EMPTY

    val length: Int get() = chars.size
    val isEmpty: Boolean get() = chars.isEmpty()

    /** Replaces the contents, wiping whatever was there first. */
    fun set(value: String) {
        wipe()
        chars = if (value.isEmpty()) EMPTY else value.toCharArray()
    }

    /**
     * Runs [block] with a private copy and wipes that copy on **every** exit
     * path, including cancellation and a thrown exception.
     */
    inline fun <T> use(block: (CharArray) -> T): T {
        val copy = copy()
        return try {
            block(copy)
        } finally {
            copy.fill(' ')
        }
    }

    /** Zeroes the stored bytes. Idempotent, and safe to call from onDispose. */
    fun wipe() {
        chars.fill(' ')
        chars = EMPTY
    }

    /** Exposed for [use]; not part of the intended surface. */
    fun copy(): CharArray = chars.copyOf()

    /** What the account list shows once the daemon has the key. */
    fun maskedHint(): String = if (chars.size <= HINT) {
        "•".repeat(chars.size)
    } else {
        "••••" + String(chars, chars.size - HINT, HINT)
    }

    override fun toString(): String = "SecretBuffer(length=$length, value=REDACTED)"

    private companion object {
        val EMPTY = CharArray(0)
        const val HINT = 4
    }
}
