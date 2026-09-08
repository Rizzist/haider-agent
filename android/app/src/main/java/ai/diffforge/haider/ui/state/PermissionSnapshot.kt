package ai.diffforge.haider.ui.state

/**
 * What Android actually says about a permission, as observed by the Activity.
 *
 * Round 6 printed three of these as fixed strings, so Settings claimed SMS was
 * "Not granted" after the user had granted it in the system dialog, and claimed
 * Accessibility was "Ask each time" while system Settings showed the service
 * off (verify-6 O3). A status the app cannot observe is [Unknown], never a
 * cheerful guess.
 */
enum class PermissionStanding { Granted, NotGranted, AskEachTime, Unknown }

/**
 * The Android-side permission facts, refreshed on resume and after a result.
 *
 * [screenCapture] is [PermissionStanding.AskEachTime] by construction, not by
 * assumption: MediaProjection issues a per-session consent and keeps no
 * durable grant to read back.
 */
data class PermissionSnapshot(
    val accessibility: PermissionStanding = PermissionStanding.Unknown,
    /**
     * Granted only while a projection is actually live.
     *
     * MediaProjection has no durable grant to read back, so this is the
     * capture service's own state, not a constant: reporting AskEachTime while
     * a projection was running made the row lie and re-open consent
     * (verify-8 O2).
     */
    val screenCapture: PermissionStanding = PermissionStanding.AskEachTime,
    val sms: PermissionStanding = PermissionStanding.Unknown,
)
