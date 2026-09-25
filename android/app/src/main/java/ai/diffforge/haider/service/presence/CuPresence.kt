package ai.diffforge.haider.service.presence

import android.os.SystemClock
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONObject

/**
 * Process-wide (`:daemon`) phone presence. The accessibility service attaches the overlay renderer;
 * the capability handler reports requests; the daemon transport forwards [stops] as the
 * `presence.stop` push, which cancels the run through the daemon's turn cancellation.
 */
object CuPresence {
    private val _stops = MutableSharedFlow<Unit>(
        extraBufferCapacity = 4,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    /** One element per human Stop; the transport turns each into a push. */
    val stops = _stops.asSharedFlow()

    val controller = CuPresenceController(
        clock = SystemClock::elapsedRealtime,
        onStop = { _stops.tryEmit(Unit) },
    )

    /** Hides the overlay for the duration of one model-facing screen capture. */
    @Volatile
    var captureShield: CaptureShield? = null

    suspend fun <T> withOverlayHidden(block: suspend () -> T): T {
        val shield = captureShield ?: return block()
        return shield.withHidden(block)
    }

    fun stopPush(): JSONObject = JSONObject().put("type", PUSH_STOP)

    const val PUSH_STOP = "presence.stop"
    const val REQUEST_END = "presence.end"
}

/** Keeps overlay pixels out of MediaProjection captures. */
interface CaptureShield {
    suspend fun <T> withHidden(block: suspend () -> T): T
}

/** Upper bound on waiting for the hidden overlay frame to reach the compositor. */
const val CAPTURE_SHIELD_FRAME_WAIT_MS = 250L

/**
 * Runs [capture] with the overlay hidden, and ALWAYS restores it: [restore] runs in `finally`
 * under [NonCancellable], so cancelling the capture at any point (while hiding, while waiting for
 * the hidden frame, during the capture itself) can never strand an invisible indicator. [restore]
 * must be idempotent. The frame wait is bounded by [frameWaitMs] because a frame-commit callback
 * is not guaranteed for an invisible view.
 */
suspend fun <T> shieldCapture(
    hide: suspend () -> Boolean,
    awaitHiddenFrame: suspend () -> Unit,
    restore: suspend () -> Unit,
    capture: suspend () -> T,
    frameWaitMs: Long = CAPTURE_SHIELD_FRAME_WAIT_MS,
): T {
    try {
        if (hide()) withTimeoutOrNull(frameWaitMs) { awaitHiddenFrame() }
        return capture()
    } finally {
        withContext(NonCancellable) { restore() }
    }
}
