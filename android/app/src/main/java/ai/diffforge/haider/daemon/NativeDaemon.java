package ai.diffforge.haider.daemon;

import android.content.Context;
import androidx.annotation.Keep;

/** Frozen JNI v1 owner. Loading is explicit and only happens on the native owner thread. */
@Keep
public final class NativeDaemon {
    private NativeDaemon() {}
    public static native String nativeVersion();
    public static native int nativeInit(Context context, String pathsJson);
    public static native int nativeStart(byte[] vaultDek, String policyJson);
    public static native String nativeObserve();
    public static native int nativeShutdown(boolean forced, long deadlineMs);
    public static native void nativeRelease();
}
