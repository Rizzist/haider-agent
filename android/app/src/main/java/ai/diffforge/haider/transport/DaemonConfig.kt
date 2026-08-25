package ai.diffforge.haider.transport

import android.content.Context

/** Persisted endpoint and bearer token for the phone's Haider daemon. */
data class DaemonConfig(
    val host: String = DEFAULT_HOST,
    val port: Int,
    val token: String,
) {
    companion object {
        const val DEFAULT_HOST: String = "127.0.0.1"

        private const val PREFERENCES_NAME = "haider_daemon"
        private const val KEY_HOST = "host"
        private const val KEY_PORT = "port"
        private const val KEY_TOKEN = "token"

        /** Returns null until a valid port and non-empty token have been saved. */
        fun load(context: Context): DaemonConfig? {
            val preferences = context.getSharedPreferences(
                PREFERENCES_NAME,
                Context.MODE_PRIVATE,
            )
            if (!preferences.contains(KEY_PORT) || !preferences.contains(KEY_TOKEN)) return null

            return try {
                val host = preferences.getString(KEY_HOST, DEFAULT_HOST)
                    ?.trim()
                    .orEmpty()
                    .ifEmpty { DEFAULT_HOST }
                val port = preferences.getInt(KEY_PORT, 0)
                val token = preferences.getString(KEY_TOKEN, null).orEmpty()
                if (port !in 1..65535 || token.isEmpty()) {
                    null
                } else {
                    DaemonConfig(host = host, port = port, token = token)
                }
            } catch (_: ClassCastException) {
                null
            }
        }

        fun save(
            context: Context,
            host: String = DEFAULT_HOST,
            port: Int,
            token: String,
        ) {
            require(port in 1..65535) { "Daemon port is outside the valid range" }
            require(token.isNotEmpty()) { "Daemon token must not be empty" }
            val normalizedHost = host.trim().ifEmpty { DEFAULT_HOST }
            context.getSharedPreferences(PREFERENCES_NAME, Context.MODE_PRIVATE)
                .edit()
                .putString(KEY_HOST, normalizedHost)
                .putInt(KEY_PORT, port)
                .putString(KEY_TOKEN, token)
                .apply()
        }

        fun clear(context: Context) {
            context.getSharedPreferences(PREFERENCES_NAME, Context.MODE_PRIVATE)
                .edit()
                .clear()
                .apply()
        }
    }
}
