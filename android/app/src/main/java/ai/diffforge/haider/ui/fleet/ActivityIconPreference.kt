package ai.diffforge.haider.ui.fleet

import android.content.Context
import androidx.compose.runtime.Composable
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.platform.LocalContext

/**
 * 973-activity-icons: the phone's own on/off switch for subagent activity
 * icons (orchestrator ruling). A display preference stored locally in
 * SharedPreferences, default ON; it never reaches the daemon.
 */
object ActivityIconPreference {
    const val FILE = "forge_activity_icons"
    const val KEY_ENABLED = "enabled"

    /** Observable cache so every row recomposes when Settings flips it. */
    private val cached = mutableStateOf<Boolean?>(null)

    fun enabled(context: Context): Boolean {
        cached.value?.let { return it }
        val stored = runCatching {
            context.applicationContext
                .getSharedPreferences(FILE, Context.MODE_PRIVATE)
                .getBoolean(KEY_ENABLED, true)
        }.getOrDefault(true)
        cached.value = stored
        return stored
    }

    fun set(context: Context, enabled: Boolean) {
        runCatching {
            context.applicationContext
                .getSharedPreferences(FILE, Context.MODE_PRIVATE)
                .edit()
                .putBoolean(KEY_ENABLED, enabled)
                .apply()
        }
        cached.value = enabled
    }

    /** Drop the in-memory cache (tests; a fresh process re-reads the file). */
    fun resetCache() {
        cached.value = null
    }

    @Composable
    fun current(): Boolean {
        val context = LocalContext.current
        return cached.value ?: enabled(context)
    }
}
