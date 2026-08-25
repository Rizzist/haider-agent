package ai.diffforge.haider.service

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Intent
import android.graphics.Path
import android.graphics.Rect
import android.os.Bundle
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import ai.diffforge.haider.transport.CapabilityBus
import kotlinx.coroutines.suspendCancellableCoroutine
import org.json.JSONArray
import org.json.JSONObject
import java.util.ArrayDeque
import kotlin.coroutines.resume

class HaiderAccessibilityService : AccessibilityService() {
    override fun onServiceConnected() {
        super.onServiceConnected()
        instance = this
        CapabilityBus.set(CAPABILITY, true)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) = Unit

    override fun onInterrupt() = Unit

    override fun onUnbind(intent: Intent?): Boolean {
        clearInstance()
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        clearInstance()
        super.onDestroy()
    }

    fun snapshot(): JSONObject {
        val nodes = JSONArray()
        val root = rootInActiveWindow
            ?: return JSONObject().put("type", "a11yTree").put("nodes", nodes)
        val pending = ArrayDeque<AccessibilityNodeInfo>()
        pending.add(root)

        while (pending.isNotEmpty() && nodes.length() < MAX_SNAPSHOT_NODES) {
            val node = pending.removeFirst()
            try {
                nodes.put(nodeToJson(node))
                for (index in 0 until node.childCount) {
                    node.getChild(index)?.let(pending::addLast)
                }
            } finally {
                node.recycle()
            }
        }
        while (pending.isNotEmpty()) {
            pending.removeFirst().recycle()
        }
        return JSONObject()
            .put("type", "a11yTree")
            .put("nodes", nodes)
    }

    suspend fun tap(x: Int, y: Int): Boolean {
        val path = Path().apply { moveTo(x.toFloat(), y.toFloat()) }
        val gesture = GestureDescription.Builder()
            .addStroke(GestureDescription.StrokeDescription(path, 0L, TAP_DURATION_MS))
            .build()
        return dispatchAndAwait(gesture)
    }

    suspend fun swipe(x1: Int, y1: Int, x2: Int, y2: Int, durationMs: Long): Boolean {
        val path = Path().apply {
            moveTo(x1.toFloat(), y1.toFloat())
            lineTo(x2.toFloat(), y2.toFloat())
        }
        val gesture = GestureDescription.Builder()
            .addStroke(
                GestureDescription.StrokeDescription(
                    path,
                    0L,
                    durationMs.coerceIn(MIN_SWIPE_MS, MAX_SWIPE_MS),
                ),
            )
            .build()
        return dispatchAndAwait(gesture)
    }

    fun setFocusedText(text: String): Boolean {
        val root = rootInActiveWindow ?: return false
        val focused = root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
        if (focused == null) {
            root.recycle()
            return false
        }

        return try {
            if (!focused.isEditable) {
                false
            } else {
                val arguments = Bundle().apply {
                    putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text)
                }
                focused.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, arguments)
            }
        } finally {
            if (focused !== root) focused.recycle()
            root.recycle()
        }
    }

    private suspend fun dispatchAndAwait(gesture: GestureDescription): Boolean =
        suspendCancellableCoroutine { continuation ->
            val accepted = dispatchGesture(
                gesture,
                object : GestureResultCallback() {
                    override fun onCompleted(gestureDescription: GestureDescription?) {
                        if (continuation.isActive) continuation.resume(true)
                    }

                    override fun onCancelled(gestureDescription: GestureDescription?) {
                        if (continuation.isActive) continuation.resume(false)
                    }
                },
                null,
            )
            if (!accepted && continuation.isActive) {
                continuation.resume(false)
            }
        }

    private fun nodeToJson(node: AccessibilityNodeInfo): JSONObject {
        val bounds = Rect()
        node.getBoundsInScreen(bounds)
        return JSONObject()
            .put("text", node.text?.toString() ?: "")
            .put("contentDesc", node.contentDescription?.toString() ?: "")
            .put("className", node.className?.toString() ?: "")
            .put("resourceId", node.viewIdResourceName ?: "")
            .put(
                "bounds",
                JSONArray()
                    .put(bounds.left)
                    .put(bounds.top)
                    .put(bounds.right)
                    .put(bounds.bottom),
            )
            .put("clickable", node.isClickable)
    }

    private fun clearInstance() {
        if (instance === this) {
            instance = null
            CapabilityBus.set(CAPABILITY, false)
        }
    }

    companion object {
        private const val CAPABILITY = "accessibility"
        private const val MAX_SNAPSHOT_NODES = 500
        private const val TAP_DURATION_MS = 50L
        private const val MIN_SWIPE_MS = 1L
        private const val MAX_SWIPE_MS = 60_000L

        @Volatile
        var instance: HaiderAccessibilityService? = null
            private set
    }
}
