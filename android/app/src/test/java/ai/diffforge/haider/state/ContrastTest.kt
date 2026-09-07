package ai.diffforge.haider.state

import ai.diffforge.haider.ui.theme.ForgeColors
import ai.diffforge.haider.ui.theme.ForgeDark
import ai.diffforge.haider.ui.theme.ForgeLight
import androidx.compose.ui.graphics.Color
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.max
import kotlin.math.min
import kotlin.math.pow

/**
 * WCAG 2.1 relative luminance for every documented pair, in both themes.
 *
 * This is the test that fails if a future token edit quietly drops a pair below
 * the 4.5:1 floor — which is how the 970 placeholder ended up at 2.85:1 (D7)
 * and white-on-accent at 2.64:1 (D8).
 */
class ContrastTest {

    private fun channel(value: Float): Double {
        val c = value.toDouble()
        return if (c <= 0.03928) c / 12.92 else ((c + 0.055) / 1.055).pow(2.4)
    }

    private fun luminance(color: Color): Double =
        0.2126 * channel(color.red) + 0.7152 * channel(color.green) + 0.0722 * channel(color.blue)

    /** Flattens a translucent foreground/background pair onto an opaque ground. */
    private fun over(top: Color, bottom: Color): Color {
        val a = top.alpha
        return Color(
            red = top.red * a + bottom.red * (1 - a),
            green = top.green * a + bottom.green * (1 - a),
            blue = top.blue * a + bottom.blue * (1 - a),
        )
    }

    private fun ratio(foreground: Color, background: Color): Double {
        val fg = luminance(over(foreground, background))
        val bg = luminance(background)
        return (max(fg, bg) + 0.05) / (min(fg, bg) + 0.05)
    }

    private fun assertReadable(name: String, foreground: Color, background: Color) {
        val value = ratio(foreground, background)
        assertTrue(
            "$name is %.2f:1, below the 4.5:1 floor".format(value),
            value >= 4.5,
        )
    }

    private fun documentedPairs(colors: ForgeColors, theme: String) {
        assertReadable("$theme accent on bg", colors.accent, colors.bg)
        assertReadable("$theme accentSoft on bg", colors.accentSoft, colors.bg)
        assertReadable("$theme text on bg", colors.text, colors.bg)
        assertReadable("$theme chatText on surface", colors.chatText, colors.surface)
        assertReadable("$theme textSoft on surface", colors.textSoft, colors.surface)
        // The placeholder and every helper line (D7).
        assertReadable("$theme textMuted on bg", colors.textMuted, colors.bg)
        assertReadable("$theme textMuted on surface", colors.textMuted, colors.surface)
        assertReadable("$theme textMuted on surfaceRaised", colors.textMuted, colors.surfaceRaised)
        assertReadable("$theme textMuted on surfaceControl", colors.textMuted, colors.surfaceControl)
        // Ink on a FILLED accent surface (D8).
        assertReadable("$theme accentInk on accent", colors.accentInk, colors.accent)
        assertReadable("$theme green on bg", colors.green, colors.bg)
        assertReadable("$theme red on bg", colors.red, colors.bg)
        assertReadable("$theme amber on bg", colors.amber, colors.bg)
        assertReadable("$theme link on bg", colors.link, colors.bg)
        // Every state colour is also a word, and the word has to be legible.
        assertReadable("$theme stateRunning on bg", colors.stateRunning, colors.bg)
        assertReadable("$theme stateNeedsInput on bg", colors.stateNeedsInput, colors.bg)
        assertReadable("$theme stateErrored on bg", colors.stateErrored, colors.bg)
        assertReadable("$theme stateIdle on bg", colors.stateIdle, colors.bg)
        assertReadable("$theme stateUnknown on bg", colors.stateUnknown, colors.bg)
        // The accent wash is a background, so its text must survive it.
        assertReadable("$theme accent on accentWash over bg", colors.accent, over(colors.accentWash, colors.bg))
        assertReadable("$theme text on accentWash over bg", colors.text, over(colors.accentWash, colors.bg))
    }

    @Test
    fun `every documented dark pair clears 4_5 to 1`() {
        documentedPairs(ForgeDark, "dark")
    }

    @Test
    fun `every documented light pair clears 4_5 to 1`() {
        documentedPairs(ForgeLight, "light")
    }

    @Test
    fun `white on a filled accent is the banned pattern it was measured to be`() {
        // Recorded, not asserted as acceptable: this is why filled accent
        // surfaces take accentInk instead.
        assertTrue(ratio(Color.White, ForgeDark.accent) < 4.5)
    }

    @Test
    fun `textDisabled is decorative only and is never asserted as readable`() {
        // Stated out loud so nobody promotes it to body text by accident.
        assertTrue(ratio(ForgeDark.textDisabled, ForgeDark.bg) < 4.5)
    }
}
