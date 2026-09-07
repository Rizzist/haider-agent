package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.ui.state.RelativeTime
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `startedAtElapsedRealtimeMs` is monotonic (`SystemClock.elapsedRealtime`) and
 * `lastActivityMs` is wall-clock Unix time. Round 1 subtracted the first from
 * the second, which reads as an uptime of about fifty-seven years.
 */
class UptimeClockTest {

    @Test
    fun `uptime is measured against the monotonic clock`() {
        val bootMs = 40_000_000L
        val startedAt = bootMs - (4 * 60 + 12) * 60_000L
        assertEquals("4h12m", RelativeTime.duration(startedAt, bootMs))
    }

    @Test
    fun `mixing the clocks is the bug this pins`() {
        val wallClockNow = 1_772_000_000_000L
        val startedAtElapsed = 40_000_000L - 12 * 60_000L
        val wrong = RelativeTime.duration(startedAtElapsed, wallClockNow)
        // Left uncorrected, the two clocks produce a nonsense number. The card
        // must never be handed `nowMs` for this field.
        assertTrue("expected an absurd uptime from mixed clocks, got $wrong", wrong.endsWith("m"))
        assertTrue(wrong.removeSuffix("m").substringBefore("h").toLong() > 100_000)
    }

    @Test
    fun `an unknown start time prints no uptime segment at all`() {
        assertEquals("", RelativeTime.duration(null, 40_000_000L))
    }

    @Test
    fun `the fake keeps its two clocks apart`() {
        val service = FakeDaemonService(FakeScenario.Populated)
        val info = (service.status.value as DaemonStatus.Running).info
        assertEquals("4h12m", RelativeTime.duration(info.startedAtElapsedRealtimeMs, FakeDaemonService.FIXED_UPTIME))
    }
}
