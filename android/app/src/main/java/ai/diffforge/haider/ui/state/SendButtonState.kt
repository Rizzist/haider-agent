package ai.diffforge.haider.ui.state

import ai.diffforge.haider.R
import ai.diffforge.haider.daemon.DaemonStatus

/** What the composer's trailing control is, right now (UI-SPEC 3.6). */
enum class SendButtonState {
    /** Nothing to send. */
    Disabled,

    /** Filled accent; `chat.send`. */
    Send,

    /** Daemon stopped: start it, then send when ready. */
    StartAndSend,

    /** A turn is running: `turn.cancel(run_id, worker_generation)`. */
    Stop,

    /** Daemon starting: a spinner, no action. */
    Starting,

    /** The turn is paused on a question; the answer is inline, not here. */
    Paused,
    ;

    val enabled: Boolean
        get() = this == Send || this == StartAndSend || this == Stop
}

data class ComposerState(
    val button: SendButtonState,
    /** A helper line, or null when there is nothing true to say. */
    val helperRes: Int? = null,
    val contentDescriptionRes: Int = R.string.cd_send,
    val placeholderRes: Int = R.string.composer_placeholder,
    val inputEnabled: Boolean = true,
)

object SendButtonMatrix {
    fun resolve(
        daemon: DaemonStatus,
        turnRunning: Boolean,
        inputRequired: Boolean,
        hasText: Boolean,
        setupComplete: Boolean = true,
    ): ComposerState = when {
        daemon is DaemonStatus.Starting || daemon is DaemonStatus.Restarting -> ComposerState(
            button = SendButtonState.Starting,
            helperRes = R.string.composer_helper_starting,
            inputEnabled = false,
        )
        daemon is DaemonStatus.Running && inputRequired -> ComposerState(
            button = SendButtonState.Paused,
            helperRes = R.string.composer_helper_paused,
            placeholderRes = R.string.composer_placeholder_paused,
            inputEnabled = false,
        )
        daemon is DaemonStatus.Running && turnRunning -> ComposerState(
            button = SendButtonState.Stop,
            helperRes = R.string.composer_helper_running,
            contentDescriptionRes = R.string.cd_stop_turn,
        )
        !setupComplete -> ComposerState(
            button = SendButtonState.Disabled,
            helperRes = R.string.composer_helper_setup,
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
