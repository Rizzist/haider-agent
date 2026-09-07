package ai.diffforge.haider.ui.state

/**
 * `shouldShowRequestPermissionRationale` returns false in two completely
 * different situations: the permission has never been asked for, and the user
 * has refused it twice so Android will not ask again. Reading only that flag
 * made a fresh install say "Android will not ask again" before it had asked
 * anything — while the very next tap still opened the system dialog.
 *
 * The missing coordinate is whether a request has actually happened.
 */
object PermissionClassifier {
    fun permanentlyDenied(
        granted: Boolean,
        everRequested: Boolean,
        shouldShowRationale: Boolean,
    ): Boolean = !granted && everRequested && !shouldShowRationale

    /** Never asked: ordinary, and the dialog is still available. */
    fun neverAsked(granted: Boolean, everRequested: Boolean): Boolean = !granted && !everRequested
}
