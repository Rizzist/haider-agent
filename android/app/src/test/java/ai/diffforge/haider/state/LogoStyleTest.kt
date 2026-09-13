package ai.diffforge.haider.state

import ai.diffforge.haider.ui.components.HaiderLogoPalette
import ai.diffforge.haider.ui.theme.ForgeDark
import ai.diffforge.haider.ui.theme.ForgeLight
import ai.diffforge.haider.ui.theme.LogoPreferences
import ai.diffforge.haider.ui.theme.LogoStyle
import ai.diffforge.haider.ui.theme.ThemeStore
import androidx.compose.ui.graphics.Color
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.max
import kotlin.math.min
import kotlin.math.pow

private class MapStore : ThemeStore {
    val values = mutableMapOf<String, Any>()

    override fun getString(key: String, fallback: String?): String? =
        values[key] as? String ?: fallback

    override fun contains(key: String): Boolean = values.containsKey(key)

    override fun getBoolean(key: String, fallback: Boolean): Boolean =
        values[key] as? Boolean ?: fallback

    override fun putString(key: String, value: String) {
        values[key] = value
    }

    override fun remove(key: String) {
        values.remove(key)
    }
}

/** Which wordmark colourway renders, and that the choice survives storage. */
class LogoStyleTest {

    // ---------- resolution ----------

    @Test
    fun `Auto follows the app theme`() {
        assertTrue(LogoStyle.Auto.resolvesToDark(themeDark = true))
        assertFalse(LogoStyle.Auto.resolvesToDark(themeDark = false))
    }

    @Test
    fun `an explicit choice beats the theme`() {
        assertFalse(LogoStyle.Light.resolvesToDark(themeDark = true))
        assertFalse(LogoStyle.Light.resolvesToDark(themeDark = false))
        assertTrue(LogoStyle.Dark.resolvesToDark(themeDark = false))
        assertTrue(LogoStyle.Dark.resolvesToDark(themeDark = true))
    }

    // ---------- persistence ----------

    @Test
    fun `a fresh store defaults to Auto`() {
        assertEquals(LogoStyle.Auto, LogoPreferences.load(MapStore()))
    }

    @Test
    fun `a saved choice loads back`() {
        val store = MapStore()
        LogoStyle.entries.forEach { style ->
            LogoPreferences.save(store, style)
            assertEquals(style, LogoPreferences.load(store))
        }
    }

    @Test
    fun `an unrecognised stored value falls back to Auto rather than crashing`() {
        val store = MapStore()
        store.putString(LogoPreferences.KEY_STYLE, "sepia")
        assertEquals(LogoStyle.Auto, LogoPreferences.load(store))
    }

    // ---------- the light variant's reason to exist ----------

    private fun channel(value: Float): Double {
        val c = value.toDouble()
        return if (c <= 0.03928) c / 12.92 else ((c + 0.055) / 1.055).pow(2.4)
    }

    private fun luminance(color: Color): Double =
        0.2126 * channel(color.red) + 0.7152 * channel(color.green) + 0.0722 * channel(color.blue)

    private fun ratio(foreground: Color, background: Color): Double {
        val fg = luminance(foreground)
        val bg = luminance(background)
        return (max(fg, bg) + 0.05) / (min(fg, bg) + 0.05)
    }

    /** WCAG 1.4.11: a meaningful graphic clears 3:1 against its ground. */
    private fun assertVisible(name: String, foreground: Color, background: Color) {
        val value = ratio(foreground, background)
        assertTrue("$name is %.2f:1, below the 3:1 graphics floor".format(value), value >= 3.0)
    }

    @Test
    fun `both logo tones clear 3 to 1 on the grounds that carry them`() {
        listOf(
            "light primary" to HaiderLogoPalette.lightPrimary,
            "light secondary" to HaiderLogoPalette.lightSecondary,
        ).forEach { (name, tone) ->
            assertVisible("$name on light surface", tone, ForgeLight.surface)
            assertVisible("$name on light bg", tone, ForgeLight.bg)
        }
        listOf(
            "dark primary" to HaiderLogoPalette.darkPrimary,
            "dark secondary" to HaiderLogoPalette.darkSecondary,
        ).forEach { (name, tone) ->
            assertVisible("$name on dark surface", tone, ForgeDark.surface)
            assertVisible("$name on dark bg", tone, ForgeDark.bg)
        }
    }
}
