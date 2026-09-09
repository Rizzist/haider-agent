package ai.diffforge.haider.ui.state

import ai.diffforge.haider.ui.daemon.MenuOption
import ai.diffforge.haider.ui.daemon.NeedsInput

/**
 * Which option Auto presses on the person's behalf (owner addition H6).
 *
 * The frozen 136-method protocol has no permission-mode door: nothing on the
 * wire sets a standing policy, and inventing one would be a claim about a
 * daemon setting that does not exist (971-V F3 found Auto simply disabled
 * instead). Auto is therefore what it always described itself as — standing
 * consent held by *this client*, spent by answering the exact device-capability
 * approvals [CapabilityApproval] covers, with the daemon's own menu answer.
 *
 * Two rules make that safe to automate:
 *
 *  - the card must pass [CapabilityApproval.isDeviceCapabilityApproval], so
 *    `sms.send`, secrets, questions and unrecognised names are never answered;
 *  - only a **once** decision is pressed. `allow_always` would install a
 *    durable daemon-side grant that outlives the mode the person chose, so a
 *    card that offers nothing but `allow_always` is left for a human.
 */
object StandingConsent {

    /** `ObserveMenuOptionWire.decision` (frame.rs:2076); style and policy read this, never the label. */
    const val ALLOW_ONCE = "allow_once"

    /** One option and its index, both from the snapshot that carried the card. */
    data class Choice(val key: String, val index: Int)

    /**
     * The option Auto may press, or null when a person has to decide.
     *
     * The index is the option's position in the card as it was published: the
     * menu answer carries `option_key` **and** `option_index` from one
     * snapshot, and a recomputed index would answer a different row.
     */
    fun choice(mode: PermissionMode, needsInput: NeedsInput?): Choice? {
        if (mode != PermissionMode.Auto) return null
        if (needsInput == null) return null
        if (!CapabilityApproval.isDeviceCapabilityApproval(needsInput)) return null
        if (needsInput.menuId == null || needsInput.requestSeq == null || needsInput.workerGeneration == null) return null
        val index = needsInput.options.indexOfFirst { it.decision == ALLOW_ONCE }
        if (index < 0) return null
        return Choice(needsInput.options[index].key, index)
    }

    /** True when this card would be answered by standing consent under [mode]. */
    fun answers(mode: PermissionMode, needsInput: NeedsInput?): Boolean = choice(mode, needsInput) != null

    /** Test/readability helper: the decision an option carries, normalised. */
    fun decision(option: MenuOption): String? = option.decision?.lowercase()
}
