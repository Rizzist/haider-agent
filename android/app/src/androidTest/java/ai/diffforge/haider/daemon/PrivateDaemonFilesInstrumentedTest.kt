package ai.diffforge.haider.daemon

import android.content.ContextWrapper
import android.os.Process
import android.system.Os
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class PrivateDaemonFilesInstrumentedTest {
    @Test fun privateAtomicStateAndSymlinkFailure() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val root = File(context.cacheDir, "daemon-file-test-${UUID.randomUUID()}").apply { mkdir() }
        try {
            val folder = PrivateDaemonFiles.directory(root, "owner-only")
            assertEquals(448, Os.lstat(folder.path).st_mode and 511)
            val file = File(folder, "synthetic")
            PrivateDaemonFiles.write(file, byteArrayOf(1, 2, 3))
            assertEquals(384, Os.lstat(file.path).st_mode and 511)
            assertEquals(Process.myUid(), Os.lstat(file.path).st_uid)
            assertArrayEquals(byteArrayOf(1, 2, 3), PrivateDaemonFiles.read(file, 4))
            PrivateDaemonFiles.write(file, byteArrayOf(4, 5))
            assertArrayEquals(byteArrayOf(4, 5), PrivateDaemonFiles.read(file, 4))
            val link = File(folder, "link")
            Os.symlink(file.path, link.path)
            try { PrivateDaemonFiles.read(link, 4); fail("Symlink must fail") } catch (_: android.system.ErrnoException) { }
            assertTrue(link.delete())
            Os.symlink(File(folder, "absent").path, link.path)
            assertTrue("Broken symlinks are not absent key records", PrivateDaemonFiles.exists(link))
            assertTrue(link.delete())
            val fixtureContext = object : ContextWrapper(context) { override fun getNoBackupFilesDir() = root }
            val store = FileDaemonLifecycleStore(fixtureContext)
            val expected = PersistedLifecycle(enabled = true, active = true, crashes = listOf(100),
                retryAtUnixMs = 200, updateUntilUnixMs = 300, lastUserStartUnixMs = 90, lastExitUnixMs = 80)
            store.save(expected)
            assertEquals(expected, FileDaemonLifecycleStore(fixtureContext).load())
        } finally { root.deleteRecursively() } // Only this test's UUID-named synthetic directory.
    }
}
