package ai.diffforge.haider.update

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.net.Uri
import android.os.Build
import android.os.Bundle
import java.io.FileInputStream

object PackageInstallerLauncher {
    fun install(context: Context, verified: VerifiedApk) {
        if (!UpdateChecker.sha256(verified.file).equals(verified.sha256, ignoreCase = true)) {
            throw ChecksumMismatchException()
        }

        val installer = context.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
            setAppPackageName(context.packageName)
            setSize(verified.file.length())
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_REQUIRED)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                setPackageSource(PackageInstaller.PACKAGE_SOURCE_DOWNLOADED_FILE)
            }
        }
        val sessionId = installer.createSession(params)
        try {
            installer.openSession(sessionId).use { session ->
                FileInputStream(verified.file).use { input ->
                    session.openWrite("base.apk", 0, verified.file.length()).use { output ->
                        input.copyTo(output)
                        session.fsync(output)
                    }
                }
                val callback = Intent(context, UpdateInstallReceiver::class.java).apply {
                    action = INSTALL_STATUS_ACTION
                    putExtra(EXTRA_RELEASE_TAG, verified.release.tag)
                    putExtra(EXTRA_SESSION_ID, sessionId)
                }
                val pendingIntentFlags = PendingIntent.FLAG_UPDATE_CURRENT or
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0
                val pendingIntent = PendingIntent.getBroadcast(
                    context,
                    sessionId,
                    callback,
                    pendingIntentFlags,
                )
                session.commit(pendingIntent.intentSender)
            }
        } catch (error: Exception) {
            runCatching { installer.abandonSession(sessionId) }
            throw error
        }
    }

    const val INSTALL_STATUS_ACTION = "ai.diffforge.haider.action.UPDATE_INSTALL_STATUS"
    const val EXTRA_RELEASE_TAG = "release_tag"
    const val EXTRA_SESSION_ID = "install_session_id"
}

class UpdateInstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != PackageInstallerLauncher.INSTALL_STATUS_ACTION) return
        val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
        val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)
        val tag = intent.getStringExtra(PackageInstallerLauncher.EXTRA_RELEASE_TAG)
        val sessionId = intent.getIntExtra(
            PackageInstallerLauncher.EXTRA_SESSION_ID,
            intent.getIntExtra(PackageInstaller.EXTRA_SESSION_ID, -1),
        )

        if (status == PackageInstaller.STATUS_PENDING_USER_ACTION) {
            val confirmation = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(Intent.EXTRA_INTENT)
            }
            if (confirmation == null || sessionId < 0) {
                PendingInstallConfirmationStore.clear(context)
                ApkUpdateCoordinator.onInstallerStatus(
                    PackageInstaller.STATUS_FAILURE,
                    "The system installer did not provide a confirmation screen",
                    tag,
                )
                return
            }
            val pending = PendingInstallConfirmationStore.save(
                context,
                sessionId,
                tag ?: "Haider update",
                confirmation,
            )
            ApkUpdateCoordinator.onInstallerConfirmationRequired(context, pending)
            return
        }

        PendingInstallConfirmationStore.clear(context)
        UpdateConfirmationNotification.cancel(context)
        ApkUpdateCoordinator.onInstallerStatus(status, message, tag)
    }
}

data class PendingInstallConfirmation(
    val tag: String,
    val action: PendingIntent,
)

/**
 * Keeps Android's exact confirmation Intent inside a system-owned PendingIntent. The token can
 * be looked up again after process death without serializing or reconstructing the system Intent.
 */
object PendingInstallConfirmationStore {
    fun save(
        context: Context,
        sessionId: Int,
        tag: String,
        confirmation: Intent,
    ): PendingInstallConfirmation {
        val trampoline = trampolineIntent(context, sessionId).apply {
            putExtra(EXTRA_CONFIRMATION_INTENT, confirmation)
            putExtra(PackageInstallerLauncher.EXTRA_RELEASE_TAG, tag)
        }
        val action = PendingIntent.getActivity(
            context,
            sessionId,
            trampoline,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
            .edit()
            .putInt(SESSION_ID, sessionId)
            .putString(RELEASE_TAG, tag)
            .commit()
        return PendingInstallConfirmation(tag, action)
    }

    fun recover(context: Context): PendingInstallConfirmation? {
        val preferences = context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
        val sessionId = preferences.getInt(SESSION_ID, -1)
        val tag = preferences.getString(RELEASE_TAG, null)
        if (sessionId < 0 || tag == null) return null
        val action = PendingIntent.getActivity(
            context,
            sessionId,
            trampolineIntent(context, sessionId),
            PendingIntent.FLAG_NO_CREATE or PendingIntent.FLAG_IMMUTABLE,
        )
        if (action == null) {
            clear(context)
            return null
        }
        return PendingInstallConfirmation(tag, action)
    }

    fun clear(context: Context) {
        val preferences = context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
        val sessionId = preferences.getInt(SESSION_ID, -1)
        if (sessionId >= 0) {
            PendingIntent.getActivity(
                context,
                sessionId,
                trampolineIntent(context, sessionId),
                PendingIntent.FLAG_NO_CREATE or PendingIntent.FLAG_IMMUTABLE,
            )?.cancel()
        }
        preferences.edit().clear().apply()
    }

    private fun trampolineIntent(context: Context, sessionId: Int) =
        Intent(context, UpdateConfirmationActivity::class.java).apply {
            action = OPEN_CONFIRMATION_ACTION
            data = Uri.parse("haider-update://installer/$sessionId")
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_NO_HISTORY)
        }

    const val OPEN_CONFIRMATION_ACTION = "ai.diffforge.haider.action.OPEN_UPDATE_CONFIRMATION"
    const val EXTRA_CONFIRMATION_INTENT = "confirmation_intent"
    private const val PREFERENCES = "pending_apk_confirmation"
    private const val SESSION_ID = "session_id"
    private const val RELEASE_TAG = "release_tag"
}

object UpdateConfirmationNotification {
    fun show(context: Context, pending: PendingInstallConfirmation) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "App updates", NotificationManager.IMPORTANCE_HIGH),
        )
        if (!manager.areNotificationsEnabled()) return
        val notification = Notification.Builder(context, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentTitle("Haider update ready")
            .setContentText("Tap to confirm ${pending.tag} in Android's installer")
            .setCategory(Notification.CATEGORY_SYSTEM)
            .setAutoCancel(true)
            .setContentIntent(pending.action)
            .build()
        runCatching { manager.notify(NOTIFICATION_ID, notification) }
    }

    fun cancel(context: Context) {
        context.getSystemService(NotificationManager::class.java).cancel(NOTIFICATION_ID)
    }

    private const val CHANNEL_ID = "haider_app_updates"
    private const val NOTIFICATION_ID = 962
}

/** User-launched trampoline that opens only PackageInstaller's supplied system Intent. */
class UpdateConfirmationActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val tag = intent.getStringExtra(PackageInstallerLauncher.EXTRA_RELEASE_TAG)
        if (intent.action != PendingInstallConfirmationStore.OPEN_CONFIRMATION_ACTION) {
            finish()
            return
        }
        val confirmation = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(
                PendingInstallConfirmationStore.EXTRA_CONFIRMATION_INTENT,
                Intent::class.java,
            )
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(PendingInstallConfirmationStore.EXTRA_CONFIRMATION_INTENT)
        }
        if (confirmation == null) {
            PendingInstallConfirmationStore.clear(this)
            ApkUpdateCoordinator.onInstallerStatus(
                PackageInstaller.STATUS_FAILURE,
                "The system installer confirmation expired",
                tag,
            )
            finish()
            return
        }
        ApkUpdateCoordinator.onInstallerConfirmationOpened(tag)
        UpdateConfirmationNotification.cancel(this)
        try {
            startActivity(confirmation)
        } catch (error: Exception) {
            PendingInstallConfirmationStore.clear(this)
            ApkUpdateCoordinator.onInstallerStatus(
                PackageInstaller.STATUS_FAILURE,
                error.message ?: "Couldn't open the system confirmation screen",
                tag,
            )
        }
        finish()
    }
}
