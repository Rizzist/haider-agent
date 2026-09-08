package ai.diffforge.haider.daemon

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith

/** This process has no native owner; the actual foreground owner lives in :daemon. */
@RunWith(AndroidJUnit4::class)
class NativeBoundaryInstrumentedTest {
    @Test fun malformedStartupClearsSyntheticKeysAndLeavesRuntimeStopped() {
        // The foreground service loads its own process. This test must not
        // depend on another instrumentation class loading the UI process copy.
        System.loadLibrary("haider")
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        assertEquals(NativeStatus.BAD_PATHS, NativeDaemon.nativeInit(context, "{}"))
        val wrongLength = ByteArray(31) { 42 }
        assertEquals(NativeStatus.VAULT_KEY_INVALID, NativeDaemon.nativeStart(wrongLength, "{}"))
        assertTrue(wrongLength.all { it == 0.toByte() })
        val synthetic = ByteArray(32) { 42 }
        assertEquals(NativeStatus.BAD_POLICY, NativeDaemon.nativeStart(synthetic, "{}"))
        assertTrue(synthetic.all { it == 0.toByte() })
        assertEquals(NativeStatus.BAD_ARGUMENT, NativeDaemon.nativeShutdown(false, -1))
        assertEquals(NativeStatus.OK, NativeDaemon.nativeShutdown(false, 0))
        assertEquals("Stopped", JSONObject(NativeDaemon.nativeObserve()).getString("phase"))
    }
}
