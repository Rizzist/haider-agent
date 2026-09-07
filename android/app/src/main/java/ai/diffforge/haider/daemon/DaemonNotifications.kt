package ai.diffforge.haider.daemon

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import ai.diffforge.haider.R

internal class DaemonNotifications(private val context: Context) : SessionNotificationSink {
    private val manager = context.getSystemService(NotificationManager::class.java)
    fun createChannels() {
        manager.createNotificationChannels(listOf(
            NotificationChannel(STATUS_CHANNEL, context.getString(R.string.daemon_channel_status), NotificationManager.IMPORTANCE_LOW),
            NotificationChannel(ATTENTION_CHANNEL, context.getString(R.string.daemon_channel_attention), NotificationManager.IMPORTANCE_HIGH),
            NotificationChannel(COMPLETION_CHANNEL, context.getString(R.string.daemon_channel_completion), NotificationManager.IMPORTANCE_DEFAULT),
        ))
    }
    fun status(snapshot: DaemonServiceSnapshot): Notification = Notification.Builder(context, STATUS_CHANNEL)
        .setSmallIcon(android.R.drawable.stat_notify_sync)
        .setContentTitle(context.getString(R.string.daemon_notification_title))
        .setContentText(context.getString(when (snapshot.phase) {
            "READY" -> if (snapshot.network == "UNAVAILABLE") R.string.daemon_status_offline else R.string.daemon_status_ready
            "RECOVERING" -> R.string.daemon_status_recovering
            "RESTARTING" -> R.string.daemon_status_restarting
            "ERROR" -> R.string.daemon_status_error
            "STOPPING" -> R.string.daemon_status_stopping
            "DISABLED" -> R.string.daemon_status_disabled
            else -> R.string.daemon_status_starting
        }))
        .setContentIntent(DaemonIntents.openStatus(context))
        .setCategory(Notification.CATEGORY_SERVICE).setOngoing(true).setOnlyAlertOnce(true)
        .setVisibility(Notification.VISIBILITY_PRIVATE)
        .addAction(Notification.Action.Builder(null, context.getString(R.string.daemon_action_open), DaemonIntents.openStatus(context)).build())
        .addAction(Notification.Action.Builder(null, context.getString(R.string.daemon_action_restart), DaemonIntents.service(context, DaemonIntents.RESTART_DAEMON)).build())
        .addAction(Notification.Action.Builder(null, context.getString(R.string.daemon_action_stop), DaemonIntents.service(context, DaemonIntents.STOP_DAEMON)).build())
        .build()

    fun updateStatus(snapshot: DaemonServiceSnapshot) = notifySafely(null, STATUS_ID, status(snapshot))
    override fun attention(session: NotificationSession) {
        notifySafely(session.sessionId, ATTENTION_ID, Notification.Builder(context, ATTENTION_CHANNEL)
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle(context.getString(R.string.daemon_attention_title))
            .setContentText(context.getString(R.string.daemon_attention_body))
            .setContentIntent(DaemonIntents.session(context, session, true))
            .setGroup("haider-session-${session.sessionId}")
            .setCategory(Notification.CATEGORY_REMINDER).setAutoCancel(true)
            .setVisibility(Notification.VISIBILITY_PRIVATE).build())
    }
    override fun clearAttention(sessionId: String) { manager.cancel(sessionId, ATTENTION_ID) }
    override fun completion(session: NotificationSession) {
        notifySafely(session.sessionId, COMPLETION_ID, Notification.Builder(context, COMPLETION_CHANNEL)
            .setSmallIcon(android.R.drawable.stat_notify_chat)
            .setContentTitle(context.getString(R.string.daemon_completion_title))
            .setContentText(context.getString(when (session.runState) {
                "errored" -> R.string.daemon_completion_failed
                "cancelled" -> R.string.daemon_completion_cancelled
                else -> R.string.daemon_completion_done
            }))
            .setContentIntent(DaemonIntents.session(context, session, false))
            .setGroup("haider-session-${session.sessionId}")
            .setCategory(Notification.CATEGORY_STATUS).setAutoCancel(true)
            .setVisibility(Notification.VISIBILITY_PRIVATE).build())
    }
    private fun notifySafely(tag: String?, id: Int, notification: Notification) {
        try { manager.notify(tag, id, notification) } catch (_: SecurityException) {
            // Notification denial is reflected in Binder status; it cannot crash the daemon.
        }
    }
    companion object {
        const val STATUS_CHANNEL = "haider_daemon_status"
        const val ATTENTION_CHANNEL = "haider_attention"
        const val COMPLETION_CHANNEL = "haider_completion"
        const val STATUS_ID = 9712
        private const val ATTENTION_ID = 9713
        private const val COMPLETION_ID = 9714
    }
}
