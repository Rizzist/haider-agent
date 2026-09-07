package ai.diffforge.haider.state

import ai.diffforge.haider.ui.state.RelativeTime
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Locale

/** The desktop rail's boundaries, ported exactly (sessionsModel.js:216-238). */
class RelativeTimeTest {
    private val now = 1_772_000_000_000L

    @Test
    fun `under a minute is now`() {
        assertEquals("now", RelativeTime.format(now, now, Locale.UK))
        assertEquals("now", RelativeTime.format(now - 59_000, now, Locale.UK))
    }

    @Test
    fun `sixty seconds becomes one minute`() {
        assertEquals("1m", RelativeTime.format(now - 60_000, now, Locale.UK))
        assertEquals("59m", RelativeTime.format(now - 59 * 60_000, now, Locale.UK))
    }

    @Test
    fun `sixty minutes becomes one hour`() {
        assertEquals("1h", RelativeTime.format(now - 60 * 60_000, now, Locale.UK))
        assertEquals("23h", RelativeTime.format(now - 23 * 60 * 60_000, now, Locale.UK))
    }

    @Test
    fun `twenty four hours becomes one day`() {
        assertEquals("1d", RelativeTime.format(now - RelativeTime.DAY_MS, now, Locale.UK))
        assertEquals("6d", RelativeTime.format(now - 6 * RelativeTime.DAY_MS, now, Locale.UK))
    }

    @Test
    fun `seven days falls back to a date`() {
        val label = RelativeTime.format(now - 7 * RelativeTime.DAY_MS, now, Locale.UK)
        assertTrue("expected a date label, got $label", label.any(Char::isLetter))
        assertTrue(label.none { it == 'd' && label.length <= 3 })
    }

    @Test
    fun `an unknown stamp renders as nothing, never as the epoch`() {
        assertEquals("", RelativeTime.format(null, now, Locale.UK))
        assertEquals("", RelativeTime.spoken(null, now, Locale.UK))
        assertEquals("", RelativeTime.duration(null, now))
    }

    @Test
    fun `a future stamp clamps rather than going negative`() {
        assertEquals("now", RelativeTime.format(now + 500_000, now, Locale.UK))
    }

    @Test
    fun `talkback expands the abbreviation and never says zero minutes`() {
        assertEquals("just now", RelativeTime.spoken(now - 1_000, now, Locale.UK))
        assertEquals("1 minute ago", RelativeTime.spoken(now - 60_000, now, Locale.UK))
        assertEquals("2 minutes ago", RelativeTime.spoken(now - 120_000, now, Locale.UK))
        assertEquals("5 hours ago", RelativeTime.spoken(now - 5 * 60 * 60_000, now, Locale.UK))
        assertEquals("3 days ago", RelativeTime.spoken(now - 3 * RelativeTime.DAY_MS, now, Locale.UK))
        assertTrue(
            (0..90).none {
                RelativeTime.spoken(now - it * 1000L, now, Locale.UK) == "0 minutes ago"
            },
        )
    }

    @Test
    fun `uptime reads as hours and minutes`() {
        assertEquals("4h12m", RelativeTime.duration(now - (4 * 60 + 12) * 60_000L, now))
        assertEquals("12m", RelativeTime.duration(now - 12 * 60_000L, now))
        assertEquals("1s", RelativeTime.duration(now, now))
    }

    @Test
    fun `waiting reads as minutes and seconds`() {
        assertEquals("2m 14s", RelativeTime.waiting(now - 134_000, now))
        assertEquals("9s", RelativeTime.waiting(now - 9_000, now))
        assertEquals("", RelativeTime.waiting(null, now))
    }
}
