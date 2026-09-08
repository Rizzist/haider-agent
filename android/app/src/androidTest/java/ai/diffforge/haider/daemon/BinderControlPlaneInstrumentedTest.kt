package ai.diffforge.haider.daemon

import android.content.Intent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import ai.diffforge.haider.BuildConfig
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class BinderControlPlaneInstrumentedTest {
    @Test fun realAdapterReceivesAServiceSnapshotWithoutEnablingIt() = runBlocking {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val context = instrumentation.targetContext
        instrumentation.startActivitySync(Intent().setClassName(context.packageName,
            "ai.diffforge.haider.MainActivity").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
        val plane = BinderRpcControlPlane(context.applicationContext, scope)
        try {
            val snapshot = withTimeout(10_000) { plane.snapshots.first { it != null } }!!
            assertEquals(BuildConfig.VERSION_NAME, snapshot.appVersion)
            assertTrue(snapshot.snapshotSeq >= 0)
        } finally {
            withContext(Dispatchers.Main.immediate) { plane.close() }
            scope.cancel()
        }
    }
}
