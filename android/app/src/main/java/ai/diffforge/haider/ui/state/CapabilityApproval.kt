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
     * The device tools C4 exposes, minus `sms.send`.
     *
     * The owner asked for reading texts to be automated. Sending one is a
     * message from the user to another person, and standing consent for that
     * is not what "read my SMS without asking" meant.
     */
    private val DEVICE_TOOLS = listOf(
        "sms.list",
        "sms.incoming",
        "screen.capture",
        "screen.",
        "a11y.",
        "accessibility",
        "app.open",
    )

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

    fun isDeviceCapabilityApproval(needsInput: NeedsInput): Boolean {
        if (needsInput.secretAnswer) return false
        val kind = needsInput.kind.lowercase()
        if (kind in NEVER_SUPPRESSED_KINDS) return false
        if (kind != "permission" && kind != "approval") return false
        val text = (listOf(needsInput.title) + needsInput.safeBody)
            .joinToString(" ")
            .lowercase()
        return DEVICE_TOOLS.any { text.contains(it) }
    }

    /** What the surface should render, given the standing mode. */
    fun suppresses(mode: PermissionMode, needsInput: NeedsInput?): Boolean =
        mode == PermissionMode.Auto &&
            needsInput != null &&
            isDeviceCapabilityApproval(needsInput)
}
