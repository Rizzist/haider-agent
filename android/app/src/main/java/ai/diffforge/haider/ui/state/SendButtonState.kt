package ai.diffforge.haider.ui.state

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.DaemonStatus

/** What the composer's trailing control is, right now (UI-SPEC 3.6). */
enum class SendButtonState {
    /** Nothing to send. */
    Disabled,

    /** Filled accent; `chat.send`. */
    Send,

    /** Daemon stopped: start it, then send when ready. */
    StartAndSend,

    /** Daemon starting: a spinner, no action. */
    Starting,

    /** The turn is paused on a question; the answer is inline, not here. */
    Paused,
    ;

    val enabled: Boolean
        get() = this == Send || this == StartAndSend
}

data class ComposerState(
    val button: SendButtonState,
    /**
     * A helper line only for a state the user cannot otherwise see: the daemon
     * is coming up, or setup is unfinished. The running and paused sentences
     * are gone — the Stop control and the answer card already say those things,
     * and saying them twice is the clutter, not the clarity (addition F, G5).
     */
    val helperRes: Int? = null,
    val contentDescriptionRes: Int = R.string.cd_send,
    val placeholderRes: Int = R.string.composer_placeholder,
    val inputEnabled: Boolean = true,
    /**
     * Stop is **additive** to Send, not a replacement (SessionComposer.jsx:684):
     * mid-turn Send is how a follow-up is queued, and swapping it for Stop
     * would make that keyboard-only exactly when it matters. Present only when
     * the snapshot names the run — never a guess.
     */
    val showStop: Boolean = false,
)

object SendButtonMatrix {
    fun resolve(
        daemon: DaemonStatus,
        turnRunning: Boolean,
        inputRequired: Boolean,
        /**
         * True when there is *anything to send* — nonblank text or a staged
         * attachment. The name predates attachments (verify-11 O8).
         */
        hasText: Boolean,
        setupComplete: Boolean = true,
    ): ComposerState = when {
        // STOPPING reads as a transition that does not accept new turns
        // (contracts-v1 C2), not as a stopped daemon you can queue against.
        daemon is DaemonStatus.Starting ||
            daemon is DaemonStatus.Restarting ||
            daemon is DaemonStatus.Stopping -> ComposerState(
            // No helper line. G5 leaves the composer with the allowed
            // error/disconnected sentence only, and the banner above already
            // says "Starting Haider…" with a progress line (verify-6 O7).
            button = SendButtonState.Starting,
            inputEnabled = false,
        )
        daemon is DaemonStatus.Running && inputRequired -> ComposerState(
            button = SendButtonState.Paused,
            placeholderRes = R.string.composer_placeholder_paused,
            inputEnabled = false,
            showStop = turnRunning,
        )
        daemon is DaemonStatus.Running && turnRunning -> ComposerState(
            button = if (hasText) SendButtonState.Send else SendButtonState.Disabled,
            showStop = true,
        )
        !setupComplete -> ComposerState(
            button = SendButtonState.Disabled,
            placeholderRes = R.string.start_composer_disabled,
            inputEnabled = false,
        )
        daemon is DaemonStatus.Stopped || daemon is DaemonStatus.Failed -> if (hasText) {
            ComposerState(
                button = SendButtonState.StartAndSend,
                contentDescriptionRes = R.string.cd_start_and_send,
            )
        } else {
            ComposerState(button = SendButtonState.Disabled)
        }
        hasText -> ComposerState(button = SendButtonState.Send)
        else -> ComposerState(button = SendButtonState.Disabled)
    }
}
