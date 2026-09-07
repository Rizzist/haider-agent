package ai.diffforge.haider.daemon

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.UserManager

/** Only the platform's post-unlock boot and own-package replacement broadcasts are accepted. */
class DaemonRestartReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED && intent.action != Intent.ACTION_MY_PACKAGE_REPLACED) return
        if (!context.getSystemService(UserManager::class.java).isUserUnlocked) return
        val pending = goAsync()
        Thread({
            try {
                val store = FileDaemonLifecycleStore(context)
                val state = store.load()
                if (shouldResume(state)) {
                    if (intent.action == Intent.ACTION_MY_PACKAGE_REPLACED) {
                        // Protected broadcast proves replacement on API 26-33, where exit reasons
                        // are missing or alias package updates to REASON_USER_REQUESTED.
                        store.save(state.copy(updateUntilUnixMs = System.currentTimeMillis() + DaemonEngine.UPDATE_WINDOW_MS))
                    }
                    val action = if (intent.action == Intent.ACTION_MY_PACKAGE_REPLACED)
                        HaiderDaemonService.ACTION_PACKAGE_REPLACED else HaiderDaemonService.ACTION_RESUME_ENABLED
                    context.startForegroundService(Intent(context, HaiderDaemonService::class.java)
                        .setPackage(context.packageName).setAction(action))
                }
            } catch (_: Exception) {
                // A platform restriction is not permission to bypass the user's enabled state.
            } finally { pending.finish() }
        }, "haider-boot-state").start()
    }
    internal companion object {
        fun shouldResume(state: PersistedLifecycle) = state.enabled && state.latchedError == null
    }
}
