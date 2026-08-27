package ai.diffforge.haider.build

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class HaiderVersionTest {
    @Test
    fun `wave version maps directly to its monotonic Android code`() {
        assertEquals(HaiderVersion("0.0.962", 962), HaiderVersion.parse("0.0.962"))
    }

    @Test
    fun `major and minor releases remain monotonic`() {
        val patchRelease = HaiderVersion.parse("0.0.9999")
        val minorRelease = HaiderVersion.parse("0.1.0")
        val majorRelease = HaiderVersion.parse("1.0.0")

        assertTrue(patchRelease.code < minorRelease.code)
        assertTrue(minorRelease.code < majorRelease.code)
    }

    @Test
    fun `reads only the workspace package version`() {
        val manifest = """
            [package]
            version = "9.9.9"

            [workspace.package]
            version = "0.0.962"
            edition = "2024"

            [workspace.dependencies]
            version = "1"
        """.trimIndent()

        assertEquals(HaiderVersion("0.0.962", 962), HaiderVersion.fromWorkspaceManifest(manifest))
    }

    @Test
    fun `rejects a version that cannot be encoded safely`() {
        assertFailsWith<IllegalArgumentException> { HaiderVersion.parse("0.100.0") }
        assertFailsWith<IllegalArgumentException> { HaiderVersion.parse("0.0.10000") }
    }
}
