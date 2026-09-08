package ai.diffforge.haider.ui.components

import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * One low-rate ticker for every looping animation in the app.
 *
 * Verify 10 measured the cost of the alternative: with the app resumed on a
 * running session and nothing changing, Haider held 54–59 % CPU and
 * SurfaceFlinger 20–34 % — about four cores painting pixels that were
 * identical. `rememberInfiniteTransition` runs off the frame clock, so each one
 * invalidates every frame whether or not anything can see it, and there was one
 * per running drawer row plus the header pill plus the streaming caret.
 *
 * This replaces all of them:
 *
 * - **One** coroutine, not one per animation, advancing a shared phase.
 * - **[TICK_MS] apart**, not per frame: a breathing dot does not need 60 Hz.
 * - **Reference counted**: the coroutine exists only while something is
 *   subscribed, so an idle screen costs nothing at all.
 * - **Lifecycle gated**: STOPPED cancels it, RESUMED restarts it.
 *
 * [activeSubscribers] is the pin's handle: with the drawer closed and the
 * session idle it must be zero.
 */
object MotionTicker {

    /** ~3 Hz. Fast enough to read as a pulse, slow enough to be free. */
    const val TICK_MS = 320L

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var job: Job? = null

    private val phaseState = mutableIntStateOf(0)
    private val subscriberState = mutableIntStateOf(0)
    private var resumed = true

    /** Monotonic tick count; animations map it to whatever they need. */
    val phase: State<Int> get() = phaseState

    /** How many composables are currently asking to be animated. */
    val activeSubscribers: Int get() = subscriberState.intValue

    /** True only while the shared coroutine is running. */
    val running: Boolean get() = job?.isActive == true

    fun subscribe() {
        subscriberState.intValue += 1
        reconcile()
    }

    fun unsubscribe() {
        subscriberState.intValue = (subscriberState.intValue - 1).coerceAtLeast(0)
        reconcile()
    }

    /** Called by [MotionLifecycleGate]; a stopped app animates nothing. */
    fun setResumed(value: Boolean) {
        resumed = value
        reconcile()
    }

    private fun reconcile() {
        val wanted = resumed && subscriberState.intValue > 0
        if (wanted && job?.isActive != true) {
            job = scope.launch {
                while (true) {
                    delay(TICK_MS)
                    phaseState.intValue += 1
                }
            }
        } else if (!wanted) {
            job?.cancel()
            job = null
        }
    }

    /** Test seam: drops every subscriber and stops the coroutine. */
    fun resetForTest() {
        subscriberState.intValue = 0
        resumed = true
        job?.cancel()
        job = null
        phaseState.intValue = 0
    }
}

/** Installs the lifecycle gate once, at the top of the tree. */
@Composable
fun MotionLifecycleGate() {
    val owner = LocalLifecycleOwner.current
    DisposableEffect(owner) {
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_RESUME -> MotionTicker.setResumed(true)
                Lifecycle.Event.ON_PAUSE -> MotionTicker.setResumed(false)
                else -> Unit
            }
        }
        owner.lifecycle.addObserver(observer)
        onDispose { owner.lifecycle.removeObserver(observer) }
    }
}

/**
 * A 0..1 pulse while [active], and a flat 1 otherwise.
 *
 * Subscribing is the *only* way to make the ticker run, so passing `active =
 * false` — an off-screen row, a closed drawer, an idle session — costs nothing
 * rather than costing a frame callback.
 */
@Composable
fun rememberPulse(active: Boolean, low: Float = 0.45f, high: Float = 1f): Float {
    val enabled = active && motionEnabled()
    DisposableEffect(enabled) {
        if (enabled) MotionTicker.subscribe()
        onDispose { if (enabled) MotionTicker.unsubscribe() }
    }
    if (!enabled) return high
    val phase by MotionTicker.phase
    // A triangle wave over six ticks: ~2 s up and down at 320 ms.
    val steps = 6
    val position = phase % steps
    val ramp = if (position < steps / 2) {
        position.toFloat() / (steps / 2)
    } else {
        (steps - position).toFloat() / (steps / 2)
    }
    return low + (high - low) * ramp
}

/** A blink for the streaming caret: on for two ticks, off for one. */
@Composable
fun rememberBlink(active: Boolean): Boolean {
    val enabled = active && motionEnabled()
    DisposableEffect(enabled) {
        if (enabled) MotionTicker.subscribe()
        onDispose { if (enabled) MotionTicker.unsubscribe() }
    }
    if (!enabled) return active
    val phase by MotionTicker.phase
    return phase % 3 != 2
}

/** Kept so a caller can hold a stable `remember` slot without animating. */
@Composable
internal fun rememberStatic(): Boolean = remember { mutableStateOf(false) }.value
