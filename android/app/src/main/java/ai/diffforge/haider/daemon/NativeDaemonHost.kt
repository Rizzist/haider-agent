package ai.diffforge.haider.daemon

import android.content.Context
import org.json.JSONObject

/** All calls, including load/version/observe/release, belong to one 8 MiB owner thread. */
interface NativeDaemonHost {
    fun version(): String
    fun init(pathsJson: String): Int
    fun start(vaultDek: ByteArray, policyJson: String): Int
    fun observe(): String
    fun shutdown(forced: Boolean, deadlineMs: Long): Int
    fun release()
}

class JniNativeDaemonHost(private val context: Context) : NativeDaemonHost {
    private var loaded = false
    override fun version(): String {
        if (!loaded) {
            System.loadLibrary("haider")
            loaded = true
        }
        return NativeDaemon.nativeVersion()
    }
    override fun init(pathsJson: String) = NativeDaemon.nativeInit(context.applicationContext, pathsJson)
    override fun start(vaultDek: ByteArray, policyJson: String) = NativeDaemon.nativeStart(vaultDek, policyJson)
    override fun observe() = NativeDaemon.nativeObserve()
    override fun shutdown(forced: Boolean, deadlineMs: Long) = NativeDaemon.nativeShutdown(forced, deadlineMs)
    override fun release() = NativeDaemon.nativeRelease()
}

object NativeStatus {
    const val OK = 0
    const val ALREADY_RUNNING = 1
    const val NOT_INITIALIZED = 2
    const val BAD_PATHS = 3
    const val BAD_POLICY = 4
    const val VAULT_KEY_INVALID = 5
    const val STORE_RECOVERY_FAILED = 6
    const val INTERNAL = 7
    const val SHUTDOWN_TIMEOUT = 8
    const val SHUTDOWN_FORCED = 9
    const val BAD_ARGUMENT = 10
    private val codes = listOf("OK", "ALREADY_RUNNING", "NOT_INITIALIZED", "BAD_PATHS", "BAD_POLICY",
        "VAULT_KEY_INVALID", "STORE_RECOVERY_FAILED", "INTERNAL", "SHUTDOWN_TIMEOUT",
        "SHUTDOWN_FORCED", "BAD_ARGUMENT")
    fun code(status: Int) = codes.getOrElse(status) { "INTERNAL" }
    fun safeCode(value: String) = value.takeIf { it in codes } ?: "INTERNAL"
}

internal data class NativeObservation(
    val phase: String,
    val generation: Long,
    val endpoint: String?,
    val errorCode: String?,
    val retryable: Boolean,
) {
    companion object {
        fun parse(json: String, expectedEndpoint: String): NativeObservation {
            require(json.length <= 16_384)
            val value = JSONObject(json)
            require(value.exactLong("jni_version") == 1L)
            val phase = value.getString("phase")
            require(phase in setOf("Starting", "Recovering", "Ready", "Draining", "Failed", "Stopped"))
            val generation = value.exactLong("daemon_generation")
            require(generation >= 0)
            val endpoint = if (value.has("endpoint_path")) value.getString("endpoint_path") else null
            if (phase == "Ready") {
                require(generation > 0 && endpoint == expectedEndpoint)
                require(value.exactLong("ready_since_unix_ms") > 0)
            } else {
                require(endpoint == null && !value.has("ready_since_unix_ms"))
            }
            return NativeObservation(phase, generation, endpoint,
                if (value.has("error_code")) NativeStatus.safeCode(value.getString("error_code")) else null,
                value.optBoolean("retryable", false))
        }
    }
}

/** JSONObject's getLong coerces fractional numbers and numeric strings; neither is JNI v1. */
internal fun JSONObject.exactLong(name: String): Long = when (val value = get(name)) {
    is Int -> value.toLong()
    is Long -> value
    else -> throw IllegalArgumentException("Expected integer")
}
