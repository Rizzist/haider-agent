package ai.diffforge.haider.ui.state

import ai.diffforge.haider.ui.daemon.NeedsInput

/**
 * Which needs-input cards Auto answers for you (owner addition H6).
 *
 * Auto is standing consent for the *device* — read the texts, look at the
 * screen, tap, open an app — because that is the work the owner asked to be
 * automated. It is not consent for anything else, so this is deliberately a
 * narrow test rather than "kind == permission":
 *
 * - a secret is never suppressed, whatever its kind says;
 * - a question, a choice, a conflict, a recovery and a trust hook are not
 *   device capabilities and are never suppressed;
 * - a permission card that does not name a device capability is not suppressed
 *   either — an unrecognised approval is shown, not swallowed.
 *
 * The daemon in Auto mode should not send these at all; this is the UI holding
 * the same line so a mismatch degrades into "one extra card" rather than
 * "a device action nobody consented to".
 */
object CapabilityApproval {

    /**
     * The exact tool names standing consent covers.
     *
     * Exact, not substrings. Round 10 asked "does the text contain any covered
     * name", so "Allow sms.send after reading sms.list?" matched `sms.list` and
     * Auto consumed an approval that included sending a text to a person
     * (verify-9 V1). `sms.send` is not here, and neither is anything the
     * contract does not name.
     */
    private val COVERED = setOf(
        "sms.list",
        "sms.incoming",
        "screen.capture",
        "a11y.tree",
        "a11y.tap",
        "a11y.type",
        "a11y.swipe",
        "a11y.back",
        "a11y.home",
        "app.open",
    )

    /**
     * A dotted tool name as the daemon writes it.
     *
     * Anything that looks like one and is not in [COVERED] — `sms.send`, a
     * name from a newer daemon, even a version string — leaves the card up.
     * The failure direction is "ask", every time.
     */
    private val TOOL_TOKEN = Regex("""\b[a-z0-9_]+\.[a-z0-9_]+\b""")

    private val NEVER_SUPPRESSED_KINDS = setOf(
        "question",
        "choice",
        "conflict",
        "recovery",
        "secret",
        "trust_hook",
        "update",
        "exhausted",
        "file",
    )

    /** Every capability the card actually names. */
    fun requestedCapabilities(needsInput: NeedsInput): Set<String> =
        TOOL_TOKEN.findAll(
            (listOf(needsInput.title) + needsInput.safeBody).joinToString(" ").lowercase(),
        ).map { it.value }.toSet()

    /**
     * True only when the card names at least one capability and **every** one
     * it names is covered.
     */
    fun isDeviceCapabilityApproval(needsInput: NeedsInput): Boolean {
        if (needsInput.secretAnswer) return false
        val kind = needsInput.kind.lowercase()
        if (kind in NEVER_SUPPRESSED_KINDS) return false
        if (kind != "permission" && kind != "approval") return false
        val requested = requestedCapabilities(needsInput)
        // Named nothing recognisable: a person decides.
        if (requested.isEmpty()) return false
        return requested.all { it in COVERED }
    }

    /** What the surface should render, given the standing mode. */
    fun suppresses(mode: PermissionMode, needsInput: NeedsInput?): Boolean =
        mode == PermissionMode.Auto &&
            needsInput != null &&
            isDeviceCapabilityApproval(needsInput)
}
