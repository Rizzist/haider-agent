package ai.diffforge.haider.state

import ai.diffforge.haider.ui.theme.ThemeMode
import ai.diffforge.haider.ui.theme.ThemePreferences
import ai.diffforge.haider.ui.theme.ThemeStore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

private class MapThemeStore(initial: Map<String, Any> = emptyMap()) : ThemeStore {
    val values = initial.toMutableMap()
    var writes = 0

    override fun getString(key: String, fallback: String?): String? =
        values[key] as? String ?: fallback

    override fun contains(key: String): Boolean = values.containsKey(key)

    override fun getBoolean(key: String, fallback: Boolean): Boolean =
        values[key] as? Boolean ?: fallback

    override fun putString(key: String, value: String) {
        values[key] = value
        writes++
    }

    override fun remove(key: String) {
        values.remove(key)
    }
}

/** The 970 boolean migrates once, and idempotently. */
class ThemeMigrationTest {

    @Test
    fun `the old dark boolean becomes Dark`() {
        val store = MapThemeStore(mapOf(ThemePreferences.LEGACY_KEY_DARK to true))
        assertEquals(ThemeMode.Dark, ThemePreferences.load(store))
        assertEquals("dark", store.values[ThemePreferences.KEY_MODE])
        assertFalse(store.contains(ThemePreferences.LEGACY_KEY_DARK))
    }

    @Test
    fun `the old light boolean becomes Light`() {
        val store = MapThemeStore(mapOf(ThemePreferences.LEGACY_KEY_DARK to false))
        assertEquals(ThemeMode.Light, ThemePreferences.load(store))
    }

    @Test
    fun `a fresh install defaults to System`() {
        val store = MapThemeStore()
        assertEquals(ThemeMode.System, ThemePreferences.load(store))
        assertEquals("system", store.values[ThemePreferences.KEY_MODE])
    }

    @Test
    fun `the migration runs once and then stops writing`() {
        val store = MapThemeStore(mapOf(ThemePreferences.LEGACY_KEY_DARK to true))
        assertEquals(ThemeMode.Dark, ThemePreferences.load(store))
        val afterMigration = store.writes
        repeat(3) { assertEquals(ThemeMode.Dark, ThemePreferences.load(store)) }
        assertEquals(afterMigration, store.writes)
    }

    @Test
    fun `an explicit choice survives and beats the legacy boolean`() {
        val store = MapThemeStore(
            mapOf(
                ThemePreferences.KEY_MODE to "light",
                ThemePreferences.LEGACY_KEY_DARK to true,
            ),
        )
        assertEquals(ThemeMode.Light, ThemePreferences.load(store))
    }

    @Test
    fun `an unrecognised stored value falls back to System rather than crashing`() {
        val store = MapThemeStore(mapOf(ThemePreferences.KEY_MODE to "sepia"))
        assertEquals(ThemeMode.System, ThemePreferences.load(store))
    }
}
