package ai.diffforge.haider.ui.theme

/**
 * Which colourway of the Haider Code wordmark renders (owner request
 * 2026-09-13). The site ships one palette, drawn for dark grounds; the app
 * pairs it with a darkened counterpart for light grounds. [Auto] follows the
 * app theme and is the default.
 */
enum class LogoStyle(val wireValue: String) {
    Auto("auto"),
    Light("light"),
    Dark("dark"),
    ;

    /**
     * True when the dark-ground palette (the site's original) renders.
     * An explicit choice wins; [Auto] takes the theme's answer.
     */
    fun resolvesToDark(themeDark: Boolean): Boolean = when (this) {
        Auto -> themeDark
        Light -> false
        Dark -> true
    }

    companion object {
        fun fromWire(value: String?): LogoStyle =
            entries.firstOrNull { it.wireValue == value } ?: Auto
    }
}

/**
 * Persisted next to the theme mode, in the same `forge_theme` file and through
 * the same [ThemeStore] seam, so it is testable without Android and follows
 * the app's one preference pattern.
 */
object LogoPreferences {
    const val KEY_STYLE = "logo_style"

    fun load(store: ThemeStore): LogoStyle =
        LogoStyle.fromWire(store.getString(KEY_STYLE, null))

    fun save(store: ThemeStore, style: LogoStyle) {
        store.putString(KEY_STYLE, style.wireValue)
    }
}
