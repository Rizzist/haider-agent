package ai.diffforge.haider.daemon

import android.app.Activity
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle

object DaemonIntents {
    const val OPEN_SESSION = "ai.diffforge.haider.action.OPEN_SESSION"
    const val OPEN_INPUT = "ai.diffforge.haider.action.OPEN_INPUT"
    const val STOP_DAEMON = "ai.diffforge.haider.action.STOP_DAEMON"
    const val RESTART_DAEMON = "ai.diffforge.haider.action.RESTART_DAEMON"
    const val OPEN_DAEMON_STATUS = "ai.diffforge.haider.action.OPEN_DAEMON_STATUS"
    const val OPEN_SETTINGS = "ai.diffforge.haider.action.OPEN_SETTINGS"
    const val SESSION_ID = "session_id"
    const val HEAD_SEQ = "head_seq"
    const val MENU_ID = "menu_id"
    const val REQUEST_SEQ = "request_seq"
    const val WORKER_GENERATION = "worker_generation"

    fun activityIntent(context: Context, action: String): Intent = Intent(action)
        .setClassName(context.packageName, "ai.diffforge.haider.MainActivity")
        .setPackage(context.packageName)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)

    fun openStatus(context: Context): PendingIntent = PendingIntent.getActivity(context, 0,
        activityIntent(context, OPEN_DAEMON_STATUS), FLAGS)

    /** Suitable for the Custom Tab Return to Haider action; no current browser URL is copied. */
    fun returnToSettings(context: Context): PendingIntent = PendingIntent.getActivity(context, 0,
        activityIntent(context, OPEN_SETTINGS), FLAGS)

    fun session(context: Context, row: NotificationSession, forInput: Boolean): PendingIntent {
        val action = if (forInput) OPEN_INPUT else OPEN_SESSION
        val intent = activityIntent(context, action)
            // PendingIntent identity includes data, not extras. Distinct sessions cannot overwrite.
            .setData(Uri.Builder().scheme("haider-internal").authority("notification")
                .appendPath(action).appendPath(row.sessionId).build())
            .putExtra(SESSION_ID, row.sessionId).putExtra(HEAD_SEQ, row.headSeq)
        if (forInput) row.input?.let {
            intent.putExtra(MENU_ID, it.menuId).putExtra(REQUEST_SEQ, it.requestSeq)
                .putExtra(WORKER_GENERATION, it.workerGeneration)
        }
        return PendingIntent.getActivity(context, 0, intent, FLAGS)
    }

    fun service(context: Context, action: String): PendingIntent {
        require(action == STOP_DAEMON || action == RESTART_DAEMON)
        val intent = Intent(context, HaiderDaemonService::class.java).setAction(action).setPackage(context.packageName)
        return PendingIntent.getForegroundService(context, 0, intent, FLAGS)
    }
    private const val FLAGS = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
}

/** UI-process relay. Both entry paths discard every browser field before navigation. */
class OAuthReturnActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        navigate(intent)
    }
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        navigate(intent)
    }
    private fun navigate(incoming: Intent?) {
        if (isOAuthReturn(incoming)) startActivity(DaemonIntents.activityIntent(this, DaemonIntents.OPEN_SETTINGS))
        // Do not keep an OAuth/browser intent in this Activity's state.
        setIntent(Intent())
        finish()
    }
    companion object {
        fun isOAuthReturn(intent: Intent?): Boolean = intent?.action == Intent.ACTION_VIEW &&
            intent.data?.toString() == "haider://oauth/return"
    }
}
