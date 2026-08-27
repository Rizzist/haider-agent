package ai.diffforge.haider.update

import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.FileOutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest

data class AvailableUpdate(
    val tag: String,
    val version: String,
    val apkUrl: String,
    val checksumUrl: String,
)

data class VerifiedApk internal constructor(
    val release: AvailableUpdate,
    val file: File,
    val sha256: String,
)

interface UpdateHttpClient {
    fun getText(url: String): String
    fun download(url: String, destination: File)
}

class ChecksumMismatchException : SecurityException("Downloaded APK SHA-256 does not match its release checksum")

/** Pure release selection and download verification, kept independent of Android UI and lifecycle APIs. */
class UpdateChecker(private val http: UpdateHttpClient) {
    fun check(installedVersion: String): AvailableUpdate? {
        val release = JSONObject(http.getText(LATEST_RELEASE_URL))
        val tag = release.getString("tag_name")
        val version = parseVersion(tag) ?: return null
        val installed = parseVersion(installedVersion) ?: return null
        if (version <= installed) return null

        val normalizedVersion = version.toString()
        val apkName = "haider-v$normalizedVersion-android.apk"
        val checksumName = "$apkName.sha256"
        var apkUrl: String? = null
        var checksumUrl: String? = null
        val assets = release.optJSONArray("assets") ?: return null
        for (index in 0 until assets.length()) {
            val asset = assets.getJSONObject(index)
            when (asset.optString("name")) {
                apkName -> apkUrl = asset.optString("browser_download_url").takeIf(String::isNotBlank)
                checksumName -> checksumUrl = asset.optString("browser_download_url").takeIf(String::isNotBlank)
            }
        }
        if (apkUrl == null || checksumUrl == null) return null
        require(apkUrl.startsWith("https://") && checksumUrl.startsWith("https://")) {
            "Release assets must use HTTPS"
        }
        return AvailableUpdate("v$normalizedVersion", normalizedVersion, apkUrl, checksumUrl)
    }

    fun downloadAndVerify(release: AvailableUpdate, cacheDir: File): VerifiedApk {
        val updateDir = cacheDir.resolve(UPDATE_CACHE_DIRECTORY).apply { mkdirs() }
        require(updateDir.isDirectory) { "Could not create the private update cache" }
        val apkName = "haider-${release.tag}-android.apk"
        val checksumName = "$apkName.sha256"
        val apkPart = updateDir.resolve("$apkName.part")
        val checksumPart = updateDir.resolve("$checksumName.part")
        val apk = updateDir.resolve(apkName)
        val checksum = updateDir.resolve(checksumName)
        apkPart.delete()
        checksumPart.delete()

        try {
            http.download(release.checksumUrl, checksumPart)
            require(checksumPart.length() in 1..MAX_CHECKSUM_BYTES) { "Invalid checksum asset size" }
            val expected = readExpectedChecksum(checksumPart, apkName)
            http.download(release.apkUrl, apkPart)
            require(apkPart.length() in 1..MAX_APK_BYTES) { "Invalid APK asset size" }
            val actual = sha256(apkPart)
            if (!actual.equals(expected, ignoreCase = true)) throw ChecksumMismatchException()

            promote(apkPart, apk)
            promote(checksumPart, checksum)
            apk.setLastModified(System.currentTimeMillis())
            checksum.setLastModified(apk.lastModified())
            pruneOldPackages(updateDir, keep = 2)
            return VerifiedApk(release, apk, actual)
        } catch (error: Exception) {
            apkPart.delete()
            checksumPart.delete()
            throw error
        }
    }

    companion object {
        const val LATEST_RELEASE_URL = "https://api.github.com/repos/Rizzist/haider-agent/releases/latest"
        private const val UPDATE_CACHE_DIRECTORY = "apk-updates"
        private const val MAX_CHECKSUM_BYTES = 8L * 1024L
        private const val MAX_APK_BYTES = 512L * 1024L * 1024L

        internal fun sha256(file: File): String {
            val digest = MessageDigest.getInstance("SHA-256")
            file.inputStream().buffered().use { input ->
                val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
                while (true) {
                    val read = input.read(buffer)
                    if (read < 0) break
                    digest.update(buffer, 0, read)
                }
            }
            return digest.digest().joinToString("") { byte -> "%02x".format(byte.toInt() and 0xff) }
        }

        private fun readExpectedChecksum(file: File, apkName: String): String {
            val checksumFile = Regex(
                """\A([0-9a-fA-F]{64}) (?: |\*)${Regex.escape(apkName)}(?:\r?\n)?\z""",
            )
            val match = checksumFile.matchEntire(file.readText())
                ?: throw IllegalArgumentException("Checksum asset has an invalid sha256sum format")
            return match.groupValues[1].lowercase()
        }

        private fun promote(source: File, destination: File) {
            try {
                Files.move(
                    source.toPath(),
                    destination.toPath(),
                    StandardCopyOption.ATOMIC_MOVE,
                    StandardCopyOption.REPLACE_EXISTING,
                )
            } catch (_: Exception) {
                Files.move(source.toPath(), destination.toPath(), StandardCopyOption.REPLACE_EXISTING)
            }
        }

        private fun pruneOldPackages(directory: File, keep: Int) {
            directory.listFiles { file -> file.name.endsWith("-android.apk") }
                ?.sortedByDescending(File::lastModified)
                ?.drop(keep)
                ?.forEach { oldApk ->
                    oldApk.delete()
                    oldApk.parentFile?.resolve("${oldApk.name}.sha256")?.delete()
                }
        }
    }
}

class JdkUpdateHttpClient : UpdateHttpClient {
    override fun getText(url: String): String {
        val bytes = request(url) { connection ->
            connection.inputStream.buffered().use { input ->
                val output = ByteArrayOutputStream()
                val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
                var total = 0
                while (true) {
                    val read = input.read(buffer)
                    if (read < 0) break
                    total += read
                    require(total <= MAX_METADATA_BYTES) { "Release metadata is too large" }
                    output.write(buffer, 0, read)
                }
                output.toByteArray()
            }
        }
        return bytes.toString(Charsets.UTF_8)
    }

    override fun download(url: String, destination: File) {
        request(url) { connection ->
            connection.inputStream.buffered().use { input ->
                FileOutputStream(destination).buffered().use { output ->
                    input.copyTo(output)
                }
            }
            Unit
        }
    }

    private fun <T> request(url: String, read: (HttpURLConnection) -> T): T {
        require(url.startsWith("https://")) { "Update downloads require HTTPS" }
        val connection = URL(url).openConnection() as HttpURLConnection
        try {
            connection.connectTimeout = CONNECT_TIMEOUT_MILLIS
            connection.readTimeout = READ_TIMEOUT_MILLIS
            connection.instanceFollowRedirects = true
            connection.setRequestProperty("Accept", "application/vnd.github+json, application/octet-stream")
            connection.setRequestProperty("User-Agent", "haider-android-updater")
            val status = connection.responseCode
            require(status in 200..299) { "Update server returned HTTP $status" }
            return read(connection)
        } finally {
            connection.disconnect()
        }
    }

    private companion object {
        const val CONNECT_TIMEOUT_MILLIS = 15_000
        const val READ_TIMEOUT_MILLIS = 60_000
        const val MAX_METADATA_BYTES = 2 * 1024 * 1024
    }
}

private data class SemanticVersion(val major: Int, val minor: Int, val patch: Int) : Comparable<SemanticVersion> {
    override fun compareTo(other: SemanticVersion): Int =
        compareValuesBy(this, other, SemanticVersion::major, SemanticVersion::minor, SemanticVersion::patch)

    override fun toString(): String = "$major.$minor.$patch"
}

private val VERSION_TAG = Regex("""^v?(\d+)\.(\d+)\.(\d+)$""")

private fun parseVersion(value: String): SemanticVersion? {
    val match = VERSION_TAG.matchEntire(value.trim()) ?: return null
    return runCatching {
        SemanticVersion(
            match.groupValues[1].toInt(),
            match.groupValues[2].toInt(),
            match.groupValues[3].toInt(),
        )
    }.getOrNull()
}
