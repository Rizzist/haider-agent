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
    val screenCapture: PermissionStanding = PermissionStanding.AskEachTime,
    val sms: PermissionStanding = PermissionStanding.Unknown,
)
