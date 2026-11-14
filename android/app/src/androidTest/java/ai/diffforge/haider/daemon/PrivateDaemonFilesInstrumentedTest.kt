package ai.diffforge.haider.daemon

import android.content.ContextWrapper
import android.os.Process
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import android.util.Log
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
    private data class OpenFile(val number: String, val path: String, val device: Long, val inode: Long)

    private fun openFiles(root: File): List<OpenFile> {
        val path = root.canonicalPath
        return requireNotNull(File("/proc/self/fd").list()).mapNotNull { number ->
            val entry = "/proc/self/fd/$number"
            try {
                val target = Os.readlink(entry)
                // Atomic replacement and failed publication can leave an unlinked file open.
                val originalPath = target.removeSuffix(" (deleted)")
                if (originalPath == path || originalPath.startsWith("$path/")) {
                    val stat = Os.stat(entry)
                    OpenFile(number, target, stat.st_dev, stat.st_ino)
                } else null
            } catch (failure: ErrnoException) {
                // Unrelated framework handles may disappear while /proc is enumerated.
                if (failure.errno == OsConstants.ENOENT) null else throw failure
            }
        }.sortedBy { it.number.toInt() }
    }

    private fun assertOpenFiles(root: File, label: String, expected: List<OpenFile> = emptyList()) {
        val actual = openFiles(root)
        Log.i("PrivateDaemonFilesTest", "$label: open file count ${actual.size}; $actual")
        assertEquals(label, expected, actual)
    }

    private fun withFixture(use: (File, File) -> Unit) {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val root = File(context.cacheDir, "fd-test-${UUID.randomUUID()}").apply { mkdir() }
        // A sibling sharing the root's name prefix must not count as a private file.
        val unrelated = File(root.parentFile, "${root.name}-unrelated").apply { writeBytes(byteArrayOf(42)) }
        try { use(root, unrelated) } finally { root.deleteRecursively(); unrelated.delete() }
    }

    @Test fun privateStoresCloseDescriptorsAcrossRepeatedGenerations() = withFixture { root, unrelated ->
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
        assertOpenFiles(root, "First generation")
        repeat(32) {
            // Check while an unrelated file is open and again after it closes.
            FileInputStream(unrelated).use { noise ->
                generation()
                assertEquals(42, noise.read())
                assertOpenFiles(root, "Generation $it with unrelated file")
            }
            assertOpenFiles(root, "Generation $it")
        }
        // Closing private store handles must preserve the caller's exact file and descriptor.
        FileInputStream(file).use { caller ->
            val stat = Os.fstat(caller.fd)
            val expected = openFiles(root)
            assertEquals(1, expected.size)
            assertEquals(file.canonicalPath, expected.single().path)
            assertEquals(stat.st_dev, expected.single().device)
            assertEquals(stat.st_ino, expected.single().inode)
            FileInputStream(unrelated).use {
                generation()
                assertOpenFiles(root, "Caller file with unrelated file", expected)
            }
            assertEquals(stat.st_dev, Os.fstat(caller.fd).st_dev)
            assertEquals(stat.st_ino, Os.fstat(caller.fd).st_ino)
            assertTrue(caller.read() >= 0)
            assertOpenFiles(root, "Caller file after generation", expected)
        }
        assertOpenFiles(root, "Caller file closed")
    }

    @Test fun rejectedReadsAndFailedAtomicPublicationCloseDescriptors() = withFixture { root, unrelated ->
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
        assertOpenFiles(root, "First failure paths")
        repeat(32) {
            FileInputStream(unrelated).use { noise ->
                failures()
                assertEquals(42, noise.read())
                assertOpenFiles(root, "Failure paths $it with unrelated file")
            }
            assertOpenFiles(root, "Failure paths $it")
        }
        // Prove an unlinked temporary file remains visible until its handle closes.
        val temporary = File(root, ".retained-control.tmp").apply { writeBytes(byteArrayOf(42)) }
        FileInputStream(temporary).use { caller ->
            val stat = Os.fstat(caller.fd)
            val expected = openFiles(root)
            assertEquals(1, expected.size)
            assertEquals(temporary.canonicalPath, expected.single().path)
            assertEquals(stat.st_dev, expected.single().device)
            assertEquals(stat.st_ino, expected.single().inode)
            assertTrue(temporary.delete())
            FileInputStream(unrelated).use {
                assertOpenFiles(root, "Deleted file with unrelated file",
                    listOf(expected.single().copy(path = "${expected.single().path} (deleted)")))
            }
            assertOpenFiles(root, "Deleted file after unrelated close",
                listOf(expected.single().copy(path = "${expected.single().path} (deleted)")))
            assertEquals(42, caller.read())
        }
        assertOpenFiles(root, "Deleted file closed")
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
