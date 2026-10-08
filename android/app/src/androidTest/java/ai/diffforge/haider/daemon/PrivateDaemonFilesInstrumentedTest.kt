package ai.diffforge.haider.daemon

import android.content.ContextWrapper
import android.os.Process
import android.system.Os
import android.system.OsConstants
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.io.FileInputStream
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class PrivateDaemonFilesInstrumentedTest {
    private fun descriptorCount() = requireNotNull(File("/dev/fd").list()).size

    private fun withFixture(use: (File) -> Unit) {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val root = File(context.cacheDir, "fd-test-${UUID.randomUUID()}").apply { mkdir() }
        try { use(root) } finally { root.deleteRecursively() }
    }

    @Test fun privateStoresCloseDescriptorsAcrossRepeatedGenerations() = withFixture { root ->
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val fixtureContext = object : ContextWrapper(context) { override fun getNoBackupFilesDir() = root }
        val file = File(PrivateDaemonFiles.directory(root, "keys"), "synthetic-wrapped-dek")
        val key = object : DekWrappingKey {
            var created = false
            override fun exists() = created
            override fun create() { created = true }
            override fun wrap(dek: ByteArray) = dek.copyOf(64)
            override fun unwrap(blob: ByteArray) = blob.copyOf(32)
        }
        val storage = object : WrappedDekStorage {
            override fun exists() = PrivateDaemonFiles.exists(file)
            override fun credentialsExist() = false
            override fun read() = PrivateDaemonFiles.read(file, 64)
            override fun write(blob: ByteArray) = PrivateDaemonFiles.write(file, blob)
        }
        fun generation() {
            val store = FileDaemonLifecycleStore(fixtureContext)
            val state = store.load().copy(enabled = true)
            store.save(state.copy(active = true))
            VaultDekStore(key, storage).withDek { 0 }
            store.save(state.copy(active = false))
            assertEquals(state.copy(active = false), store.load())
        }
        generation() // Warm class initialization and the first wrapped-file publication.
        val baseline = descriptorCount()
        repeat(32) {
            generation()
            assertEquals("Private stores leaked at generation $it", baseline, descriptorCount())
        }
        // Closing private store handles must not close an unrelated caller's file.
        FileInputStream(file).use { caller ->
            assertEquals(baseline + 1, descriptorCount())
            generation()
            assertTrue(caller.read() >= 0)
            assertEquals(baseline + 1, descriptorCount())
        }
        assertEquals(baseline, descriptorCount())
    }

    @Test fun rejectedReadsAndFailedAtomicPublicationCloseDescriptors() = withFixture { root ->
        val empty = File(root, "empty").apply { writeBytes(byteArrayOf()) }
        val oversized = File(root, "oversized").apply { writeBytes(ByteArray(5)) }
        val destinationDirectory = File(root, "directory").apply { mkdir() }
        fun failures() {
            for (file in listOf(empty, oversized, destinationDirectory)) {
                try { PrivateDaemonFiles.read(file, 4); fail("Invalid read must fail") }
                catch (_: IllegalArgumentException) { }
            }
            try { PrivateDaemonFiles.write(destinationDirectory, byteArrayOf(1)); fail("Rename must fail") }
            catch (_: android.system.ErrnoException) { }
        }
        failures()
        val baseline = descriptorCount()
        repeat(32) {
            failures()
            assertEquals("Failure path leaked at iteration $it", baseline, descriptorCount())
        }
        assertEquals(setOf("empty", "oversized", "directory"), requireNotNull(root.list()).toSet())
    }

    @Test fun appSelinuxAllowsPathHandlesForPrivateDirectoryAncestors() {
        val root = InstrumentationRegistry.getInstrumentation().targetContext.filesDir.canonicalFile
        // Linux O_PATH is not exposed by Android's public OsConstants API.
        // Match rustix::fs::OFlags::PATH under the actual target app's SELinux domain.
        val pathOnly = 0x200000 or OsConstants.O_NOFOLLOW or OsConstants.O_CLOEXEC
        generateSequence(root) { it.parentFile }.forEach { ancestor ->
            val handle = Os.open(ancestor.path, pathOnly, 0)
            try { assertTrue(OsConstants.S_ISDIR(Os.fstat(handle).st_mode)) }
            finally { Os.close(handle) }
        }
        val readable = Os.open(root.path, OsConstants.O_RDONLY or OsConstants.O_NOFOLLOW, 0)
        try { Os.fsync(readable) } finally { Os.close(readable) }
    }

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
