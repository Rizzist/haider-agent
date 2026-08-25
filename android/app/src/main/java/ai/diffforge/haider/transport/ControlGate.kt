package ai.diffforge.haider.transport

/** Process-local effect ceiling. The UI may opt in to device mutation for the current process. */
object ControlGate {
    @Volatile
    var enabled: Boolean = false
}
