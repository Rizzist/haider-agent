package ai.diffforge.haider.daemon

import android.content.Context

/** Frozen JNI v1 surface exercised by the disposable diagnostic host. */
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
