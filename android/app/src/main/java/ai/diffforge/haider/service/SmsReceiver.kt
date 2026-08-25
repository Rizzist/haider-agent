package ai.diffforge.haider.service

import android.Manifest
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.provider.Telephony
import ai.diffforge.haider.transport.CapabilityBus
import ai.diffforge.haider.transport.IncomingSms
import ai.diffforge.haider.transport.SmsBus

class SmsReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Telephony.Sms.Intents.SMS_RECEIVED_ACTION) return
        val parts = Telephony.Sms.Intents.getMessagesFromIntent(intent)
        if (parts.isEmpty()) return

        SmsBus.publish(
            IncomingSms(
                address = parts.first().originatingAddress.orEmpty(),
                body = parts.joinToString(separator = "") { it.messageBody.orEmpty() },
                timestampMs = parts.maxOf { it.timestampMillis },
            ),
        )
        refreshSmsCapabilities(context)
    }
}

data class SmsRecord(
    val address: String,
    val body: String,
    val timestampMs: Long,
    val isRead: Boolean,
)

fun querySms(
    context: Context,
    sinceMs: Long? = null,
    limit: Int = DEFAULT_SMS_LIMIT,
): List<SmsRecord> {
    val boundedLimit = limit.coerceIn(0, MAX_SMS_LIMIT)
    if (boundedLimit == 0) {
        refreshSmsCapabilities(context)
        return emptyList()
    }
    val selection = sinceMs?.let { "${Telephony.Sms.DATE} >= ?" }
    val selectionArgs = sinceMs?.let { arrayOf(it.toString()) }
    val messages = ArrayList<SmsRecord>(boundedLimit)

    context.contentResolver.query(
        Telephony.Sms.Inbox.CONTENT_URI,
        arrayOf(
            Telephony.Sms.ADDRESS,
            Telephony.Sms.BODY,
            Telephony.Sms.DATE,
            Telephony.Sms.READ,
        ),
        selection,
        selectionArgs,
        "${Telephony.Sms.DATE} DESC",
    )?.use { cursor ->
        val addressIndex = cursor.getColumnIndexOrThrow(Telephony.Sms.ADDRESS)
        val bodyIndex = cursor.getColumnIndexOrThrow(Telephony.Sms.BODY)
        val dateIndex = cursor.getColumnIndexOrThrow(Telephony.Sms.DATE)
        val readIndex = cursor.getColumnIndexOrThrow(Telephony.Sms.READ)
        while (cursor.moveToNext() && messages.size < boundedLimit) {
            messages += SmsRecord(
                address = cursor.getString(addressIndex).orEmpty(),
                body = cursor.getString(bodyIndex).orEmpty(),
                timestampMs = cursor.getLong(dateIndex),
                isRead = cursor.getInt(readIndex) != 0,
            )
        }
    }
    refreshSmsCapabilities(context)
    return messages
}

fun refreshSmsCapabilities(context: Context) {
    CapabilityBus.set(
        "smsRead",
        context.checkSelfPermission(Manifest.permission.READ_SMS) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED,
    )
    CapabilityBus.set(
        "smsReceive",
        context.checkSelfPermission(Manifest.permission.RECEIVE_SMS) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED,
    )
}

/**
 * Refreshes this package's SMS grant state and pushes any change via
 * [refreshSmsCapabilities] → CapabilityBus. Android exposes no PUBLIC live
 * permission-change callback to a normal app (PackageManager's
 * OnPermissionsChangedListener is @SystemApi / hidden), so callers refresh on
 * natural boundaries — service start and whenever the app returns to foreground.
 */
object SmsPermissionMonitor {
    fun start(context: Context) {
        refreshSmsCapabilities(context.applicationContext)
    }
}

private const val DEFAULT_SMS_LIMIT = 50
private const val MAX_SMS_LIMIT = 500
