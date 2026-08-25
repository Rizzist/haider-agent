package ai.diffforge.haider.service

import android.content.Context
import android.content.Intent
import android.util.Base64
import ai.diffforge.haider.transport.CapabilityHandler
import ai.diffforge.haider.transport.ControlGate
import ai.diffforge.haider.transport.Frames
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

/**
 * Android implementation of daemon capabilities. The effect ceiling rejects every device mutation
 * unless the request has `control=true` and [ControlGate.enabled] is true; observation requests do
 * not consult the gate.
 */
class AndroidCapabilityHandler(context: Context) : CapabilityHandler {
    private val appContext = context.applicationContext

    init {
        SmsPermissionMonitor.start(appContext)
    }

    override suspend fun handle(body: JSONObject): JSONObject {
        refreshSmsCapabilities(appContext)
        val type = body.optString("type")
        if (type in MUTATING_REQUESTS && !controlAllowed(body)) {
            return Frames.rejected("observe_only")
        }

        return when (type) {
            "a11y.snapshot" -> snapshot()
            "a11y.tap" -> Frames.ack(
                withAccessibility { service ->
                    service.tap(body.getInt("x"), body.getInt("y"))
                },
            )
            "a11y.swipe" -> Frames.ack(
                withAccessibility { service ->
                    service.swipe(
                        body.getInt("x1"),
                        body.getInt("y1"),
                        body.getInt("x2"),
                        body.getInt("y2"),
                        body.getLong("ms"),
                    )
                },
            )
            "a11y.text" -> Frames.ack(
                withContext(Dispatchers.Main.immediate) {
                    HaiderAccessibilityService.instance?.setFocusedText(body.getString("text"))
                        ?: false
                },
            )
            "screen.capture" -> captureScreen()
            "sms.list" -> listSms(body)
            "app.open" -> Frames.ack(openApp(body.getString("pkg")))
            else -> Frames.error("unsupported_request")
        }
    }

    private fun controlAllowed(body: JSONObject): Boolean =
        body.opt("control") == true && ControlGate.enabled

    private suspend fun snapshot(): JSONObject = withContext(Dispatchers.Main.immediate) {
        HaiderAccessibilityService.instance?.snapshot()
            ?: JSONObject()
                .put("type", "a11yTree")
                .put("nodes", JSONArray())
    }

    private suspend fun withAccessibility(
        block: suspend (HaiderAccessibilityService) -> Boolean,
    ): Boolean = withContext(Dispatchers.Main.immediate) {
        val service = HaiderAccessibilityService.instance ?: return@withContext false
        block(service)
    }

    private suspend fun captureScreen(): JSONObject {
        val service = ScreenCaptureService.instance
        if (service == null) {
            ScreenCaptureService.requestConsent(appContext)
            return Frames.ack(ok = false)
        }
        val png = service.captureOnce() ?: return Frames.ack(ok = false)
        return JSONObject()
            .put("type", "png")
            .put("base64", Base64.encodeToString(png, Base64.NO_WRAP))
    }

    private suspend fun listSms(body: JSONObject): JSONObject = withContext(Dispatchers.IO) {
        val sinceMs = if (body.has("sinceMs") && !body.isNull("sinceMs")) {
            body.getLong("sinceMs")
        } else {
            null
        }
        val limit = if (body.has("limit") && !body.isNull("limit")) {
            body.getInt("limit")
        } else {
            DEFAULT_SMS_LIMIT
        }
        val messages = try {
            querySms(appContext, sinceMs, limit)
        } catch (_: SecurityException) {
            emptyList()
        }
        val jsonMessages = JSONArray()
        messages.forEach { message ->
            jsonMessages.put(
                JSONObject()
                    .put("address", message.address)
                    .put("body", message.body)
                    .put("ts", message.timestampMs)
                    .put("read", message.isRead),
            )
        }
        JSONObject()
            .put("type", "smsList")
            .put("messages", jsonMessages)
    }

    private fun openApp(packageName: String): Boolean {
        val launchIntent = appContext.packageManager.getLaunchIntentForPackage(packageName)
            ?: return false
        launchIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        return try {
            appContext.startActivity(launchIntent)
            true
        } catch (_: RuntimeException) {
            false
        }
    }

    private companion object {
        const val DEFAULT_SMS_LIMIT = 50
        val MUTATING_REQUESTS = setOf(
            "a11y.tap",
            "a11y.swipe",
            "a11y.text",
            "app.open",
        )
    }
}
