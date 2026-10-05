package ai.diffforge.haider.service.presence

import ai.diffforge.haider.R
import android.accessibilityservice.AccessibilityService
import android.animation.ValueAnimator
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.graphics.PixelFormat
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.view.Choreographer
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.view.animation.DecelerateInterpolator
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import kotlin.coroutines.resume

/**
 * Draws the phone presence indicator from [HaiderAccessibilityService][ai.diffforge.haider.service.HaiderAccessibilityService]
 * with `TYPE_ACCESSIBILITY_OVERLAY` windows (no `SYSTEM_ALERT_WINDOW` permission is needed):
 *
 * - a full-screen, untouchable layer carrying the gold agent pointer and tap ripple;
 * - a "Haider is controlling your phone · Stop" chip that is touchable but never focusable, so
 *   pressing Stop cannot take input focus from the app being driven;
 * - an ongoing notification with a Stop action, reachable from the shade on any screen.
 *
 * Capture exclusion: MediaProjection records accessibility overlays, so [withHidden] makes both
 * windows invisible, waits until that frame is committed to the compositor, runs the capture (whose
 * virtual display is created afterwards), then restores them. The model never sees the overlay.
 */
class AccessibilityPresenceOverlay(
    private val service: AccessibilityService,
) : PresenceRenderer, CaptureShield {
    private val main = Handler(Looper.getMainLooper())
    private val windows = service.getSystemService(WindowManager::class.java)
    private val density = service.resources.displayMetrics.density
    private val pointerLayer = PointerLayer(service, density)
    private val chip = buildChip()
    private var chipAtTop = true
    private var shown = false
    private val idleCheck = object : Runnable {
        override fun run() {
            if (CuPresence.controller.tick()) main.postDelayed(this, IDLE_CHECK_MS)
        }
    }

    override fun show() = onMain {
        if (shown) return@onMain
        shown = true
        runCatching { windows.addView(pointerLayer, pointerParams()) }
        runCatching { windows.addView(chip, chipParams()) }
        postNotification()
        main.removeCallbacks(idleCheck)
        main.postDelayed(idleCheck, IDLE_CHECK_MS)
    }

    override fun pointer(x: Int?, y: Int?, mark: PresenceMark) = onMain {
        if (x != null && y != null) avoidChip(x, y)
        pointerLayer.moveTo(x, y, mark)
    }

    override fun stopping() = onMain {
        chip.findViewWithTag<Button>(STOP_TAG)?.text = service.getString(R.string.cu_presence_stopping)
    }

    override fun hide() = onMain {
        main.removeCallbacks(idleCheck)
        if (!shown) return@onMain
        shown = false
        runCatching { windows.removeView(pointerLayer) }
        runCatching { windows.removeView(chip) }
        chip.findViewWithTag<Button>(STOP_TAG)?.text = service.getString(R.string.cu_presence_stop)
        notifications().cancel(NOTIFICATION_ID)
    }

    override suspend fun <T> withHidden(block: suspend () -> T): T = shieldCapture(
        hide = {
            withContext(Dispatchers.Main.immediate) {
                if (shown) {
                    pointerLayer.visibility = View.INVISIBLE
                    chip.visibility = View.INVISIBLE
                }
                shown
            }
        },
        awaitHiddenFrame = { withContext(Dispatchers.Main.immediate) { awaitCommittedFrame(pointerLayer) } },
        // Idempotent: views are VISIBLE whenever they are not being shielded.
        restore = {
            withContext(Dispatchers.Main.immediate) {
                pointerLayer.visibility = View.VISIBLE
                chip.visibility = View.VISIBLE
            }
        },
        capture = block,
    )

    private suspend fun awaitCommittedFrame(view: View) {
        // Two vsyncs: one to draw the invisible state, one for it to reach the compositor.
        repeat(2) {
            suspendCancellableCoroutine { continuation ->
                Choreographer.getInstance().postFrameCallback {
                    if (continuation.isActive) continuation.resume(Unit)
                }
            }
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && view.isAttachedToWindow) {
            suspendCancellableCoroutine { continuation ->
                view.viewTreeObserver.registerFrameCommitCallback {
                    if (continuation.isActive) continuation.resume(Unit)
                }
                view.invalidate()
            }
        }
    }

    /** Moves the touchable chip to the other edge if a gesture targets it (never tap our own Stop). */
    private fun avoidChip(x: Int, y: Int) {
        if (!shown || !chip.isAttachedToWindow) return
        val location = IntArray(2)
        chip.getLocationOnScreen(location)
        val margin = dp(24f)
        val inside = x >= location[0] - margin && x <= location[0] + chip.width + margin &&
            y >= location[1] - margin && y <= location[1] + chip.height + margin
        if (inside) {
            chipAtTop = !chipAtTop
            runCatching { windows.updateViewLayout(chip, chipParams()) }
        }
    }

    private fun buildChip(): LinearLayout {
        val padding = dp(10f)
        val layout = LinearLayout(service).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(padding, padding / 2, padding / 2, padding / 2)
            background = GradientDrawable().apply {
                setColor(CHIP_FILL)
                cornerRadius = dp(22f).toFloat()
            }
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_YES
        }
        val dot = TextView(service).apply {
            text = "●"
            setTextColor(STOP_FILL)
            setPadding(0, 0, dp(8f), 0)
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        }
        val label = TextView(service).apply {
            text = service.getString(R.string.cu_presence_chip)
            setTextColor(Color.WHITE)
            textSize = 14f
            setPadding(0, 0, dp(10f), 0)
        }
        val stop = Button(service).apply {
            tag = STOP_TAG
            text = service.getString(R.string.cu_presence_stop)
            isAllCaps = false
            setTextColor(Color.WHITE)
            background = GradientDrawable().apply {
                setColor(STOP_FILL)
                cornerRadius = dp(18f).toFloat()
            }
            minHeight = dp(36f)
            minimumHeight = dp(36f)
            setPadding(dp(16f), 0, dp(16f), 0)
            contentDescription = service.getString(R.string.cu_presence_stop_description)
            setOnClickListener { CuPresence.controller.stopPressed() }
        }
        layout.addView(dot)
        layout.addView(label)
        layout.addView(stop)
        return layout
    }

    private fun overlayParams(width: Int, height: Int, flags: Int) = WindowManager.LayoutParams(
        width,
        height,
        WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
        flags,
        PixelFormat.TRANSLUCENT,
    )

    private fun pointerParams() = overlayParams(
        WindowManager.LayoutParams.MATCH_PARENT,
        WindowManager.LayoutParams.MATCH_PARENT,
        WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
            WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or
            WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
    ).apply { gravity = Gravity.TOP or Gravity.START }

    private fun chipParams() = overlayParams(
        WindowManager.LayoutParams.WRAP_CONTENT,
        WindowManager.LayoutParams.WRAP_CONTENT,
        WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
            WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL or
            WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
    ).apply {
        gravity = (if (chipAtTop) Gravity.TOP else Gravity.BOTTOM) or Gravity.CENTER_HORIZONTAL
        y = dp(if (chipAtTop) 40f else 72f)
    }

    private fun notifications(): NotificationManager =
        service.getSystemService(NotificationManager::class.java)

    private fun postNotification() {
        val manager = notifications()
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                service.getString(R.string.cu_presence_channel),
                NotificationManager.IMPORTANCE_LOW,
            ).apply { setShowBadge(false) },
        )
        val stopIntent = PendingIntent.getBroadcast(
            service,
            0,
            Intent(service, PresenceStopReceiver::class.java)
                .setAction(PresenceStopReceiver.ACTION_STOP)
                .setPackage(service.packageName),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = Notification.Builder(service, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_menu_view)
            .setContentTitle(service.getString(R.string.cu_presence_chip))
            .setContentText(service.getString(R.string.cu_presence_notification_text))
            .setCategory(Notification.CATEGORY_STATUS)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .addAction(
                Notification.Action.Builder(
                    null,
                    service.getString(R.string.cu_presence_stop),
                    stopIntent,
                ).build(),
            )
            .build()
        // Without POST_NOTIFICATIONS the chip remains the Stop affordance.
        runCatching { manager.notify(NOTIFICATION_ID, notification) }
    }

    private fun dp(value: Float): Int = (value * density).toInt()

    private fun onMain(block: () -> Unit) {
        if (Looper.myLooper() == Looper.getMainLooper()) block() else main.post(block)
    }

    /** Full-screen, untouchable canvas: the agent pointer and the tap ripple. */
    private class PointerLayer(context: Context, private val density: Float) : View(context) {
        private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = POINTER_FILL
            style = Paint.Style.FILL
        }
        private val outline = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = INK
            style = Paint.Style.STROKE
            strokeWidth = 1.6f * density
            strokeJoin = Paint.Join.ROUND
        }
        private val ripple = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = POINTER_FILL
            style = Paint.Style.STROKE
            strokeWidth = 3f * density
        }
        private val caption = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = INK
            textSize = 12f * density
            isFakeBoldText = true
        }
        private val captionFill = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = POINTER_FILL }
        private val arrow = Path().apply {
            // Same outline as the desktop pointer (haider-tools presence::art).
            val points = listOf(0f to 0f, 0f to 24f, 6f to 18.5f, 10.5f to 28f, 14.5f to 26.2f, 10.2f to 17f, 18f to 17f)
            moveTo(points[0].first * density, points[0].second * density)
            points.drop(1).forEach { (x, y) -> lineTo(x * density, y * density) }
            close()
        }
        private var x = -1f
        private var y = -1f
        private var label: String = "Haider"
        private var rippleProgress = -1f
        private var animator: ValueAnimator? = null

        init {
            importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_NO_HIDE_DESCENDANTS
        }

        fun moveTo(targetX: Int?, targetY: Int?, mark: PresenceMark) {
            label = when (mark) {
                PresenceMark.OBSERVE -> "Haider · looking"
                PresenceMark.TAP -> "Haider · tap"
                PresenceMark.SWIPE -> "Haider · swipe"
                PresenceMark.TYPE -> "Haider · typing"
                PresenceMark.OPEN_APP -> "Haider · opening app"
            }
            if (targetX == null || targetY == null) {
                invalidate()
                return
            }
            val fromX = if (x < 0) targetX.toFloat() else x
            val fromY = if (y < 0) targetY.toFloat() else y
            animator?.cancel()
            animator = ValueAnimator.ofFloat(0f, 1f).apply {
                duration = MOVE_MS
                interpolator = DecelerateInterpolator()
                addUpdateListener { animation ->
                    val t = animation.animatedValue as Float
                    x = fromX + (targetX - fromX) * t
                    y = fromY + (targetY - fromY) * t
                    invalidate()
                }
                start()
            }
            if (mark == PresenceMark.TAP || mark == PresenceMark.SWIPE) {
                ValueAnimator.ofFloat(0f, 1f).apply {
                    startDelay = MOVE_MS
                    duration = RIPPLE_MS
                    addUpdateListener { animation ->
                        rippleProgress = animation.animatedValue as Float
                        invalidate()
                    }
                    start()
                }
            }
        }

        override fun onDraw(canvas: Canvas) {
            if (x < 0 || y < 0) return
            if (rippleProgress in 0f..1f) {
                ripple.alpha = ((1f - rippleProgress) * 255).toInt()
                canvas.drawCircle(x, y, (10f + 18f * rippleProgress) * density, ripple)
            }
            canvas.save()
            canvas.translate(x, y)
            canvas.drawPath(arrow, fill)
            canvas.drawPath(arrow, outline)
            val left = 20f * density
            val top = 22f * density
            val width = caption.measureText(label) + 12f * density
            canvas.drawRoundRect(left, top, left + width, top + 18f * density, 9f * density, 9f * density, captionFill)
            canvas.drawText(label, left + 6f * density, top + 13f * density, caption)
            canvas.restore()
        }
    }

    companion object {
        const val CHANNEL_ID = "cu_presence"
        const val NOTIFICATION_ID = 1107
        private const val STOP_TAG = "cu_presence_stop"
        private const val IDLE_CHECK_MS = 1_000L
        private const val MOVE_MS = 160L
        private const val RIPPLE_MS = 450L
        private val POINTER_FILL = Color.rgb(0xF2, 0xA9, 0x00)
        private val INK = Color.rgb(0x14, 0x14, 0x18)
        private val CHIP_FILL = Color.argb(0xF0, 0x16, 0x16, 0x1C)
        private val STOP_FILL = Color.rgb(0xE5, 0x48, 0x4D)
    }
}
