package ai.diffforge.haider.daemon

import android.app.Notification
import android.app.NotificationManager
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Parcel
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class DaemonAndroidContractTest {
    private val context get() = RuntimeEnvironment.getApplication()
    @Test fun parcelRoundTripsEveryFieldAndNullPresenceBit() {
        val full = DaemonServiceSnapshot(true, "READY", "0.0.971", "0.0.971", 1, 41,
            RpcEndpoint("/private/h.sock", 1, 41), 2, 99, "AVAILABLE", true, true,
            "INTERNAL", true, 71, 800, 4096)
        for (snapshot in listOf(full, DaemonServiceSnapshot(appVersion = "0.0.971"))) {
            val parcel = Parcel.obtain()
            try {
                snapshot.writeToParcel(parcel, 0)
                parcel.setDataPosition(0)
                assertEquals(snapshot, DaemonServiceSnapshot.CREATOR.createFromParcel(parcel))
                assertEquals(0, parcel.dataAvail())
            } finally { parcel.recycle() }
        }
        val parcel = Parcel.obtain()
        try {
            full.writeToParcel(parcel, 0); parcel.setDataPosition(0)
            assertEquals(1, parcel.readInt())
            assertEquals("READY", parcel.readString())
            assertEquals("0.0.971", parcel.readString()); assertEquals("0.0.971", parcel.readString())
            assertEquals(1, parcel.readInt()); assertEquals(41L, parcel.readLong())
            assertEquals(1, parcel.readInt()); assertEquals("/private/h.sock", parcel.readString())
            assertEquals(1, parcel.readInt()); assertEquals(41L, parcel.readLong())
            assertEquals(2, parcel.readInt()); assertEquals(1, parcel.readInt()); assertEquals(99L, parcel.readLong())
            assertEquals("AVAILABLE", parcel.readString()); assertEquals(1, parcel.readInt()); assertEquals(1, parcel.readInt())
            assertEquals(1, parcel.readInt()); assertEquals("INTERNAL", parcel.readString()); assertEquals(1, parcel.readInt())
            assertEquals(71L, parcel.readLong()); assertEquals(1, parcel.readInt()); assertEquals(800L, parcel.readLong())
            assertEquals(1, parcel.readInt()); assertEquals(4096L, parcel.readLong()); assertEquals(0, parcel.dataAvail())
        } finally { parcel.recycle() }
    }
    @Test fun manifestProtectsDaemonAndCoLocatesCapabilityComponents() {
        val info = context.packageManager.getPackageInfo(context.packageName,
            PackageManager.GET_SERVICES or PackageManager.GET_RECEIVERS or PackageManager.GET_PERMISSIONS)
        val service = requireNotNull(info.services).single { it.name.endsWith(".HaiderDaemonService") }
        assertFalse(service.exported)
        assertEquals("${context.packageName}:daemon", service.processName)
        assertEquals(0, service.flags and android.content.pm.ServiceInfo.FLAG_STOP_WITH_TASK)
        for (suffix in listOf(".ScreenCaptureService", ".HaiderAccessibilityService")) {
            assertEquals(service.processName, requireNotNull(info.services).single { it.name.endsWith(suffix) }.processName)
        }
        assertEquals("android.permission.BIND_ACCESSIBILITY_SERVICE", requireNotNull(info.services).single { it.name.endsWith(".HaiderAccessibilityService") }.permission)
        val boot = requireNotNull(info.receivers).single { it.name.endsWith(".DaemonRestartReceiver") }
        assertFalse(boot.exported); assertFalse(boot.directBootAware)
        val sms = requireNotNull(info.receivers).single { it.name.endsWith(".SmsReceiver") }
        assertEquals(service.processName, sms.processName)
        assertEquals("android.permission.BROADCAST_SMS", sms.permission)
        assertTrue(requireNotNull(info.requestedPermissions).contains("android.permission.FOREGROUND_SERVICE_SPECIAL_USE"))
        assertTrue(requireNotNull(info.requestedPermissions).contains("android.permission.RECEIVE_BOOT_COMPLETED"))
        assertFalse(requireNotNull(info.requestedPermissions).contains("android.permission.MANAGE_EXTERNAL_STORAGE"))
    }
    @Test fun exactOAuthFilterAndRelayDiscardBrowserExtrasWithoutStartingService() {
        val intent = Intent(Intent.ACTION_VIEW, Uri.parse("haider://oauth/return"))
            .addCategory(Intent.CATEGORY_BROWSABLE).putExtra("code", "synthetic-canary")
        val resolved = context.packageManager.queryIntentActivities(intent, PackageManager.MATCH_DEFAULT_ONLY)
        assertTrue(resolved.any { it.activityInfo.name.endsWith(".OAuthReturnActivity") })
        assertFalse(OAuthReturnActivity.isOAuthReturn(Intent(Intent.ACTION_VIEW, Uri.parse("haider://oauth/return?code=synthetic-canary"))))
        assertFalse(OAuthReturnActivity.isOAuthReturn(Intent(Intent.ACTION_VIEW, Uri.parse("haider://oauth/return/extra"))))
        val activity = Robolectric.buildActivity(OAuthReturnActivity::class.java, intent).create().get()
        val outgoing = shadowOf(activity).nextStartedActivity
        assertEquals(DaemonIntents.OPEN_SETTINGS, outgoing.action)
        assertNull(outgoing.data); assertNull(outgoing.extras)
        assertEquals(context.packageName, outgoing.`package`)
        assertEquals("ai.diffforge.haider.MainActivity", outgoing.component!!.className)
        assertNull(shadowOf(context).nextStartedService)
    }
    @Test fun notificationChannelsAndIntentsAreExplicitImmutableAndSecretFree() {
        val notifications = DaemonNotifications(context)
        notifications.createChannels()
        val manager = context.getSystemService(NotificationManager::class.java)
        assertEquals(NotificationManager.IMPORTANCE_LOW, manager.getNotificationChannel(DaemonNotifications.STATUS_CHANNEL).importance)
        assertEquals(NotificationManager.IMPORTANCE_HIGH, manager.getNotificationChannel(DaemonNotifications.ATTENTION_CHANNEL).importance)
        assertEquals(NotificationManager.IMPORTANCE_DEFAULT, manager.getNotificationChannel(DaemonNotifications.COMPLETION_CHANNEL).importance)
        val status = notifications.status(DaemonServiceSnapshot(phase = "ERROR", errorCode = "synthetic-secret-canary"))
        assertFalse(status.extras.toString().contains("synthetic-secret-canary"))
        assertEquals("Haider", status.extras.getString(Notification.EXTRA_TITLE))
        for (pending in listOf(status.contentIntent) + status.actions.map { it.actionIntent }) {
            val shadow = shadowOf(pending)
            assertTrue(shadow.isImmutable)
            assertNotNull(shadow.savedIntent.component)
            assertEquals(context.packageName, shadow.savedIntent.`package`)
        }
        val row = NotificationSession("session-1", 9, 2, "run-1", "parked_input", NotificationInput("menu-1", 8, 2))
        val input = shadowOf(DaemonIntents.session(context, row, true)).savedIntent
        assertEquals(DaemonIntents.OPEN_INPUT, input.action)
        assertEquals(8, input.getLongExtra(DaemonIntents.REQUEST_SEQ, -1))
        assertEquals(2, input.getLongExtra(DaemonIntents.WORKER_GENERATION, -1))
        assertNotEquals(DaemonIntents.session(context, row, true), DaemonIntents.session(context, row.copy(sessionId = "session-2"), true))
    }
    @Test fun restartReceiverCannotEnableDisabledOrLatchedState() {
        assertFalse(DaemonRestartReceiver.shouldResume(PersistedLifecycle()))
        assertFalse(DaemonRestartReceiver.shouldResume(PersistedLifecycle(updateUntilUnixMs = 99)))
        assertFalse(DaemonRestartReceiver.shouldResume(PersistedLifecycle(enabled = true, latchedError = "CRASH_LOOP")))
        assertTrue(DaemonRestartReceiver.shouldResume(PersistedLifecycle(enabled = true)))
    }
}
