package ai.diffforge.haider.ui.theme

import android.content.Context

/** Three-state appearance selection (UI-SPEC 2.6). Default is [System]. */
enum class ThemeMode(val wireValue: String) {
    System("system"),
    Light("light"),
    Dark("dark"),
    ;

    companion object {
        fun fromWire(value: String?): ThemeMode =
            entries.firstOrNull { it.wireValue == value } ?: System
    }
}

/** The tiny key/value surface the migration needs, so it can be tested without Android. */
interface ThemeStore {
    fun getString(key: String, fallback: String?): String?
    fun contains(key: String): Boolean
    fun getBoolean(key: String, fallback: Boolean): Boolean
    fun putString(key: String, value: String)
    fun remove(key: String)
}

/**
 * Reads the appearance preference, migrating the 970 boolean once.
 *
 * 970 stored `forge_theme/dark: Boolean` (default true). 971 stores
 * `forge_theme/mode: String`. `true -> Dark`, `false -> Light`; a device with no
 * stored boolean lands on [ThemeMode.System], which is the new default.
 */
object ThemePreferences {
    const val FILE = "forge_theme"
    const val KEY_MODE = "mode"
    const val LEGACY_KEY_DARK = "dark"

    fun load(store: ThemeStore): ThemeMode {
        val stored = store.getString(KEY_MODE, null)
        if (stored != null) return ThemeMode.fromWire(stored)
        if (store.contains(LEGACY_KEY_DARK)) {
            val migrated = if (store.getBoolean(LEGACY_KEY_DARK, true)) ThemeMode.Dark else ThemeMode.Light
            store.putString(KEY_MODE, migrated.wireValue)
            store.remove(LEGACY_KEY_DARK)
            return migrated
        }
        store.putString(KEY_MODE, ThemeMode.System.wireValue)
        return ThemeMode.System
    }

    fun save(store: ThemeStore, mode: ThemeMode) {
        store.putString(KEY_MODE, mode.wireValue)
    }

    fun store(context: Context): ThemeStore = SharedPreferencesThemeStore(context)
}

private class SharedPreferencesThemeStore(context: Context) : ThemeStore {
    private val preferences =
        context.applicationContext.getSharedPreferences(ThemePreferences.FILE, Context.MODE_PRIVATE)

    override fun getString(key: String, fallback: String?): String? =
        runCatching { preferences.getString(key, fallback) }.getOrDefault(fallback)

    override fun contains(key: String): Boolean = preferences.contains(key)

    override fun getBoolean(key: String, fallback: Boolean): Boolean =
        runCatching { preferences.getBoolean(key, fallback) }.getOrDefault(fallback)

    override fun putString(key: String, value: String) {
        preferences.edit().putString(key, value).apply()
    }

    override fun remove(key: String) {
        preferences.edit().remove(key).apply()
    }
}
