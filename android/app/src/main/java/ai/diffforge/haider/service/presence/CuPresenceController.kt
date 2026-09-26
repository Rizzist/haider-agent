package ai.diffforge.haider.service.presence

/**
 * Kotlin mirror of `haider_protocol::computer::CU_PRESENCE_IDLE_SECS`: the phone indicator retires
 * after this long without a daemon capability request, exactly like the desktop overlay.
 */
const val CU_PRESENCE_IDLE_MS = 30_000L

/**
 * After the human presses Stop, mutating requests are refused until the daemon reports the run's
 * presence ended (`presence.end`) or this long passes — the daemon cancels the run through its turn
 * cancellation, so the latch only has to cover requests already in flight.
 */
const val CU_PRESENCE_STOP_LATCH_MS = 10_000L

/** How the agent pointer marks one capability request. */
enum class PresenceMark { OBSERVE, TAP, SWIPE, TYPE, OPEN_APP }

/** Draws the phone indicator. Implementations marshal to the main thread themselves. */
interface PresenceRenderer {
    fun show()

    /** Animate the agent pointer to ([x], [y]) in screen pixels, or mark in place when null. */
    fun pointer(x: Int?, y: Int?, mark: PresenceMark)

    fun stopping()

    fun hide()
}

/**
 * The phone's "Haider is controlling your phone · Stop" state machine. Pure Kotlin: the clock and
 * the Stop sink are injected so JVM tests drive it deterministically.
 *
 * It appears on the first capability request that operates the screen, follows every request,
 * retires after [idleMs] of silence or when the daemon sends `presence.end`, and on Stop hides,
 * latches further mutating requests off and asks the daemon to cancel the run via [onStop].
 */
class CuPresenceController(
    private val clock: () -> Long,
    private val onStop: () -> Unit,
    private val idleMs: Long = CU_PRESENCE_IDLE_MS,
    private val stopLatchMs: Long = CU_PRESENCE_STOP_LATCH_MS,
) {
    private var renderer: PresenceRenderer? = null
    private var lastActivity = 0L
    private var stoppedUntil = Long.MIN_VALUE

    @Volatile
    var visible: Boolean = false
        private set

    @Synchronized
    fun attach(renderer: PresenceRenderer) {
        this.renderer = renderer
        if (visible) renderer.show()
    }

    @Synchronized
    fun detach(renderer: PresenceRenderer) {
        if (this.renderer === renderer) {
            renderer.hide()
            this.renderer = null
        }
    }

    /**
     * Observes one daemon capability request of [type]. Returns false when a mutating request must
     * be refused because the human pressed Stop; true otherwise (including non-screen requests,
     * which never raise the indicator).
     */
    @Synchronized
    fun onRequest(type: String, x: Int?, y: Int?, mutating: Boolean): Boolean {
        val mark = markFor(type) ?: return true
        val now = clock()
        if (now < stoppedUntil) {
            // Observation after Stop is harmless and must not re-raise the chip.
            return !mutating
        }
        lastActivity = now
        if (!visible) {
            visible = true
            renderer?.show()
        }
        renderer?.pointer(x, y, mark)
        return true
    }

    /** The daemon ended this run's phone presence. */
    @Synchronized
    fun onPresenceEnd() {
        stoppedUntil = Long.MIN_VALUE
        hideLocked()
    }

    /** Periodic idle check; returns whether the indicator is still visible. */
    @Synchronized
    fun tick(): Boolean {
        if (visible && clock() - lastActivity >= idleMs) hideLocked()
        return visible
    }

    /**
     * The human pressed Stop (chip or notification). Returns false when nothing was being
     * controlled, so a stale notification action cannot cancel an unrelated later run.
     */
    fun stopPressed(): Boolean {
        synchronized(this) {
            if (!visible) return false
            stoppedUntil = clock() + stopLatchMs
            renderer?.stopping()
            hideLocked()
        }
        onStop()
        return true
    }

    private fun hideLocked() {
        if (!visible) return
        visible = false
        renderer?.hide()
    }

    companion object {
        /** Capability request types that operate (or look at) the phone's screen. */
        fun markFor(type: String): PresenceMark? = when (type) {
            "a11y.tap" -> PresenceMark.TAP
            "a11y.swipe" -> PresenceMark.SWIPE
            "a11y.text" -> PresenceMark.TYPE
            "app.open" -> PresenceMark.OPEN_APP
            "a11y.snapshot", "screen.capture" -> PresenceMark.OBSERVE
            else -> null
        }
    }
}
