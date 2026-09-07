package ai.diffforge.haider.ui.state

import java.util.Calendar
import java.util.Date
import java.util.Locale
import java.text.SimpleDateFormat

/**
 * Relative time, ported verbatim from the desktop rail
 * (`rust-diffforge/src/sessions/sessionsModel.js:216-238`):
 * `now` (<60 s) -> `12m` -> `5h` -> `3d` -> `Mar 4`.
 *
 * Never "0 minutes ago". An absent stamp renders as an empty string, not as an
 * epoch date: absent means unknown (UI-SPEC trap 6.6.5).
 */
object RelativeTime {
    const val MINUTE_MS = 60_000L
    const val HOUR_MS = 60 * MINUTE_MS
    const val DAY_MS = 24 * HOUR_MS
    const val WEEK_MS = 7 * DAY_MS

    fun format(thenMs: Long?, nowMs: Long, locale: Locale = Locale.getDefault()): String {
        if (thenMs == null) return ""
        val delta = (nowMs - thenMs).coerceAtLeast(0L)
        return when {
            delta < MINUTE_MS -> "now"
            delta < HOUR_MS -> "${delta / MINUTE_MS}m"
            delta < DAY_MS -> "${delta / HOUR_MS}h"
            delta < WEEK_MS -> "${delta / DAY_MS}d"
            else -> dateLabel(thenMs, locale)
        }
    }

    /** The TalkBack expansion: `2m` is spoken as "2 minutes ago" (UI-SPEC 4.2). */
    fun spoken(thenMs: Long?, nowMs: Long, locale: Locale = Locale.getDefault()): String {
        if (thenMs == null) return ""
        val delta = (nowMs - thenMs).coerceAtLeast(0L)
        return when {
            delta < MINUTE_MS -> "just now"
            delta < HOUR_MS -> plural(delta / MINUTE_MS, "minute")
            delta < DAY_MS -> plural(delta / HOUR_MS, "hour")
            delta < WEEK_MS -> plural(delta / DAY_MS, "day")
            else -> dateLabel(thenMs, locale)
        }
    }

    /** `4h12m`, for the daemon uptime segment. Empty when the start is unknown. */
    fun duration(startedAtMs: Long?, nowMs: Long): String {
        if (startedAtMs == null) return ""
        val delta = (nowMs - startedAtMs).coerceAtLeast(0L)
        val hours = delta / HOUR_MS
        val minutes = (delta % HOUR_MS) / MINUTE_MS
        return when {
            hours > 0 -> "${hours}h${minutes}m"
            minutes > 0 -> "${minutes}m"
            else -> "${(delta / 1000L).coerceAtLeast(1L)}s"
        }
    }

    /** `2m 14s`, for the needs-input waiting footer. */
    fun waiting(sinceMs: Long?, nowMs: Long): String {
        if (sinceMs == null) return ""
        val delta = (nowMs - sinceMs).coerceAtLeast(0L)
        val minutes = delta / MINUTE_MS
        val seconds = (delta % MINUTE_MS) / 1000L
        return if (minutes > 0) "${minutes}m ${seconds}s" else "${seconds}s"
    }

    /** `9:38`, for the "started" segment of a completed setup step. */
    fun clock(atMs: Long?, locale: Locale = Locale.getDefault()): String {
        if (atMs == null) return ""
        return SimpleDateFormat("H:mm", locale).format(Date(atMs))
    }

    private fun plural(value: Long, unit: String): String =
        if (value == 1L) "1 $unit ago" else "$value ${unit}s ago"

    private fun dateLabel(atMs: Long, locale: Locale): String {
        val format = SimpleDateFormat("MMM d", locale)
        return format.format(Date(atMs))
    }

    /** Local midnight of the given instant; used by day grouping. */
    fun startOfLocalDay(atMs: Long): Long {
        val calendar = Calendar.getInstance()
        calendar.timeInMillis = atMs
        calendar.set(Calendar.HOUR_OF_DAY, 0)
        calendar.set(Calendar.MINUTE, 0)
        calendar.set(Calendar.SECOND, 0)
        calendar.set(Calendar.MILLISECOND, 0)
        return calendar.timeInMillis
    }
}
