package ai.diffforge.haider.daemon

import android.content.Context
import android.os.Process
import android.system.Os
import android.system.ErrnoException
import android.system.OsConstants
import org.json.JSONObject
import java.io.File
import java.io.FileDescriptor
import java.util.UUID

/** Owner-only files. No credentials or ambient paths are accepted by this API. */
internal object PrivateDaemonFiles {
    // Os.open owns the handle. Android's streams constructed from a FileDescriptor
    // borrow it, so closing those streams does not close the underlying descriptor.
    private inline fun <T> withDescriptor(file: File, flags: Int, mode: Int = 0, use: (FileDescriptor) -> T): T {
        val fd = Os.open(file.path, flags or OsConstants.O_CLOEXEC, mode)
        return try { use(fd) } finally { Os.close(fd) }
    }

    fun directory(root: File, relative: String): File {
        var current = root.canonicalFile
        relative.split('/').filter(String::isNotEmpty).forEach { part ->
            require(part != "." && part != "..")
            current = File(current, part)
            if (!current.exists()) current.mkdir()
            val stat = Os.lstat(current.path)
            require(OsConstants.S_ISDIR(stat.st_mode) && stat.st_uid == Process.myUid())
            Os.chmod(current.path, 448) // 0700
        }
        return current
    }

    fun exists(file: File): Boolean = try {
        Os.lstat(file.path)
        true // Includes broken symlinks: they are damage, not permission to generate a new key.
    } catch (failure: ErrnoException) {
        if (failure.errno == OsConstants.ENOENT) false else throw failure
    }

    fun read(file: File, limit: Int): ByteArray {
        return withDescriptor(file, OsConstants.O_RDONLY or OsConstants.O_NOFOLLOW) { fd ->
            val stat = Os.fstat(fd)
            require(OsConstants.S_ISREG(stat.st_mode) && stat.st_uid == Process.myUid())
            require(stat.st_size in 1..limit.toLong())
            val bytes = ByteArray(limit + 1)
            var count = 0
            while (count < bytes.size) {
                val read = Os.read(fd, bytes, count, bytes.size - count)
                if (read == 0) break
                count += read
            }
            require(count in 1..limit)
            bytes.copyOf(count)
        }
    }

    fun write(file: File, bytes: ByteArray) {
        val parent = requireNotNull(file.parentFile)
        val temporary = File(parent, ".${file.name}-${UUID.randomUUID()}.tmp")
        try {
            withDescriptor(temporary,
                OsConstants.O_WRONLY or OsConstants.O_CREAT or OsConstants.O_EXCL or OsConstants.O_NOFOLLOW, 384) { fd ->
                var count = 0
                while (count < bytes.size) {
                    val written = Os.write(fd, bytes, count, bytes.size - count)
                    check(written > 0)
                    count += written
                }
                Os.fsync(fd)
            }
            Os.rename(temporary.path, file.path)
            withDescriptor(parent, OsConstants.O_RDONLY or OsConstants.O_NOFOLLOW) { directoryFd ->
                val stat = Os.fstat(directoryFd)
                require(OsConstants.S_ISDIR(stat.st_mode) && stat.st_uid == Process.myUid())
                Os.fsync(directoryFd)
            }
        } finally {
            temporary.delete()
        }
    }
}

internal data class DaemonPaths(val json: String, val endpoint: String, val vaultDirectory: File) {
    companion object {
        fun create(context: Context): DaemonPaths {
            val store = PrivateDaemonFiles.directory(context.filesDir, "haider/profiles/default")
            val runtime = PrivateDaemonFiles.directory(context.filesDir, "haider/runtime/android-default")
            val logs = PrivateDaemonFiles.directory(context.filesDir, "haider/logs")
            val workspace = PrivateDaemonFiles.directory(store, "workspace")
            val tmp = PrivateDaemonFiles.directory(runtime, "tmp")
            // Native performs the authoritative budget check including its staging suffixes.
            require(File(runtime, "mobile.sock").path.toByteArray(Charsets.UTF_8).size <= 107)
            return DaemonPaths(JSONObject().put("profile_id", "android-default")
                .put("store_dir", store.path).put("runtime_dir", runtime.path)
                .put("logs_dir", logs.path).put("workspace_dir", workspace.path)
                .put("tmp_dir", tmp.path).toString(), File(runtime, "h.sock").path, File(store, "vault"))
        }
    }
}
