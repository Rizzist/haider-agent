package ai.diffforge.haider.update

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.Assert.assertThrows
import java.io.File
import java.security.MessageDigest

class UpdateCheckerTest {
    @get:Rule
    val temporaryFolder = TemporaryFolder()

    @Test
    fun `newer GitHub tag with both assets is offered`() {
        val http = FakeHttpClient(releaseJson("v0.0.963"))
        val update = UpdateChecker(http).check("0.0.962")

        assertNotNull(update)
        assertEquals("v0.0.963", update?.tag)
        assertEquals(UpdateChecker.LATEST_RELEASE_URL, http.textRequests.single())
    }

    @Test
    fun `current older and invalid tags are not offered`() {
        assertNull(UpdateChecker(FakeHttpClient(releaseJson("v0.0.962"))).check("0.0.962"))
        assertNull(UpdateChecker(FakeHttpClient(releaseJson("v0.0.961"))).check("0.0.962"))
        assertNull(UpdateChecker(FakeHttpClient(releaseJson("nightly"))).check("0.0.962"))
    }

    @Test
    fun `newer tag without its exact APK and checksum pair is not offered`() {
        assertNull(
            UpdateChecker(FakeHttpClient(releaseJson("v0.0.964", assetVersion = "0.0.963")))
                .check("0.0.962"),
        )
        assertNull(
            UpdateChecker(FakeHttpClient(releaseJson("v0.0.963", includeChecksum = false)))
                .check("0.0.962"),
        )
    }

    @Test
    fun `download is accepted only after sha256 verification`() {
        val apk = "signed apk bytes".toByteArray()
        val hash = sha256(apk)
        val http = FakeHttpClient(
            releaseJson("v0.0.963"),
            downloads = mapOf(
                APK_URL to apk,
                SHA_URL to "$hash  haider-v0.0.963-android.apk\n".toByteArray(),
            ),
        )
        val checker = UpdateChecker(http)
        val release = requireNotNull(checker.check("0.0.962"))

        val verified = checker.downloadAndVerify(release, temporaryFolder.root)

        assertArrayEquals(apk, verified.file.readBytes())
        assertEquals(hash, verified.sha256)
        assertEquals(listOf(SHA_URL, APK_URL), http.downloadRequests)
    }

    @Test
    fun `sha256 mismatch refuses update and leaves no installable APK`() {
        val http = FakeHttpClient(
            releaseJson("v0.0.963"),
            downloads = mapOf(
                APK_URL to "tampered".toByteArray(),
                SHA_URL to "${"0".repeat(64)}  haider-v0.0.963-android.apk\n".toByteArray(),
            ),
        )
        val checker = UpdateChecker(http)
        val release = requireNotNull(checker.check("0.0.962"))

        assertThrows(ChecksumMismatchException::class.java) {
            checker.downloadAndVerify(release, temporaryFolder.root)
        }
        assertFalse(
            temporaryFolder.root.walkTopDown().any { it.name.endsWith("-android.apk") },
        )
    }

    @Test
    fun `checksum file rejects paths and trailing records`() {
        for (checksum in listOf(
            "${"0".repeat(64)}  other/haider-v0.0.963-android.apk\n",
            "${"0".repeat(64)}  haider-v0.0.963-android.apk\nextra\n",
            "${"0".repeat(64)}  haider-v0.0.963-android.apk\n\n",
        )) {
            val http = FakeHttpClient(
                releaseJson("v0.0.963"),
                downloads = mapOf(
                    APK_URL to "apk".toByteArray(),
                    SHA_URL to checksum.toByteArray(),
                ),
            )
            val checker = UpdateChecker(http)
            val release = requireNotNull(checker.check("0.0.962"))

            assertThrows(IllegalArgumentException::class.java) {
                checker.downloadAndVerify(release, temporaryFolder.root)
            }
        }
    }

    private class FakeHttpClient(
        private val metadata: String,
        private val downloads: Map<String, ByteArray> = emptyMap(),
    ) : UpdateHttpClient {
        val textRequests = mutableListOf<String>()
        val downloadRequests = mutableListOf<String>()

        override fun getText(url: String): String {
            textRequests += url
            return metadata
        }

        override fun download(url: String, destination: File) {
            downloadRequests += url
            destination.writeBytes(requireNotNull(downloads[url]) { "No fake response for $url" })
        }
    }

    companion object {
        private const val APK_URL = "https://example.test/haider.apk"
        private const val SHA_URL = "https://example.test/haider.apk.sha256"

        private fun releaseJson(
            tag: String,
            assetVersion: String = "0.0.963",
            includeChecksum: Boolean = true,
        ): String {
            val checksumAsset = if (includeChecksum) {
                """,{"name": "haider-v$assetVersion-android.apk.sha256", "browser_download_url": "$SHA_URL"}"""
            } else {
                ""
            }
            return """
            {
              "tag_name": "$tag",
              "assets": [
                {"name": "haider-v$assetVersion-android.apk", "browser_download_url": "$APK_URL"}
                $checksumAsset
              ]
            }
            """.trimIndent()
        }

        private fun sha256(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256")
            .digest(bytes)
            .joinToString("") { byte -> "%02x".format(byte.toInt() and 0xff) }
    }
}
