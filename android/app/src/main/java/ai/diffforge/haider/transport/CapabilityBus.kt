package ai.diffforge.haider.transport

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONArray
import org.json.JSONObject

/** Tracks process-visible Android grants and lets the transport push grant changes. */
object CapabilityBus {
    private val _granted = MutableStateFlow<Set<String>>(emptySet())
    val granted = _granted.asStateFlow()

    @Synchronized
    fun set(capability: String, isGranted: Boolean) {
        val current = _granted.value
        val updated = if (isGranted) current + capability else current - capability
        if (updated != current) {
            _granted.value = updated
        }
    }

    fun toPush(capabilities: Set<String> = granted.value): JSONObject {
        val array = JSONArray()
        capabilities.sorted().forEach(array::put)
        return JSONObject()
            .put("type", "capabilities.changed")
            .put("granted", array)
    }
}
