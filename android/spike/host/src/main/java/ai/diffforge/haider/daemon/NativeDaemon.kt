package ai.diffforge.haider.daemon

import android.content.Context

/** Spike only: no credentials and no production standalone policy. */
class NativeDaemon private constructor() {
    companion object {
        init { System.loadLibrary("haider") }
        @JvmStatic external fun nativeVersion(): String
        @JvmStatic external fun nativeInit(context: Context, pathsJson: String): Int
        @JvmStatic external fun nativeStart(vaultDek: ByteArray, policyJson: String): Int
        @JvmStatic external fun nativeObserve(): String
        @JvmStatic external fun nativeShutdown(forced: Boolean, deadlineMs: Long): Int
        @JvmStatic external fun nativeRelease()
    }
}
