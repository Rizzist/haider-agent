package ai.diffforge.haider.daemon

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

internal data class PersistedLifecycle(
    val enabled: Boolean = false,
    val active: Boolean = false,
    val crashes: List<Long> = emptyList(),
    val latchedError: String? = null,
    val retryAtUnixMs: Long? = null,
    val updateUntilUnixMs: Long? = null,
    val lastUserStartUnixMs: Long = 0,
    val lastExitUnixMs: Long = 0,
)

internal interface DaemonLifecycleStore {
    fun load(): PersistedLifecycle
    fun save(state: PersistedLifecycle)
}

internal class FileDaemonLifecycleStore(context: Context) : DaemonLifecycleStore {
    private val file = File(PrivateDaemonFiles.directory(context.noBackupFilesDir, "haider/enabled-state"), "state-v1.json")
    override fun load(): PersistedLifecycle {
        try {
            if (!PrivateDaemonFiles.exists(file)) return PersistedLifecycle()
            val data = JSONObject(String(PrivateDaemonFiles.read(file, 4096), Charsets.UTF_8))
            require(data.getInt("version") == 1)
            val crashes = data.getJSONArray("crashes")
            require(crashes.length() <= 3)
            val error = data.optString("latchedError").takeIf(String::isNotEmpty)
            require(error == null || error in LATCH_CODES)
            return PersistedLifecycle(data.getBoolean("enabled"), data.getBoolean("active"),
                List(crashes.length()) { crashes.getLong(it) }, error,
                data.optionalLong("retryAt"), data.optionalLong("updateUntil"),
                data.getLong("lastUserStart"), data.getLong("lastExit"))
        } catch (_: Exception) {
            // Damaged opt-in state cannot authorize background startup.
            return PersistedLifecycle(latchedError = "LIFECYCLE_STATE_INVALID")
        }
    }
    override fun save(state: PersistedLifecycle) {
        val data = JSONObject().put("version", 1).put("enabled", state.enabled).put("active", state.active)
            .put("crashes", JSONArray(state.crashes)).put("lastUserStart", state.lastUserStartUnixMs)
            .put("lastExit", state.lastExitUnixMs)
        state.latchedError?.let { data.put("latchedError", it) }
        state.retryAtUnixMs?.let { data.put("retryAt", it) }
        state.updateUntilUnixMs?.let { data.put("updateUntil", it) }
        PrivateDaemonFiles.write(file, data.toString().toByteArray(Charsets.UTF_8))
    }
    private fun JSONObject.optionalLong(key: String) = if (has(key)) getLong(key) else null
    private companion object {
        val LATCH_CODES = setOf("CRASH_LOOP", "VAULT_KEY_INVALID", "VAULT_WRAP_CORRUPT", "VAULT_KEY_UNAVAILABLE",
            "NATIVE_LIBRARY_UNAVAILABLE", "NATIVE_VERSION_MISMATCH", "NATIVE_PROTOCOL_INVALID", "LIFECYCLE_STATE_INVALID",
            "BAD_PATHS", "BAD_POLICY", "STORE_RECOVERY_FAILED", "INTERNAL", "SHUTDOWN_TIMEOUT", "ALREADY_RUNNING",
            "NOT_INITIALIZED", "BAD_ARGUMENT")
    }
}
