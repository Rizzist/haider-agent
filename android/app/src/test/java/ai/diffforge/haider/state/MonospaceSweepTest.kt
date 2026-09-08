package ai.diffforge.haider.state

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File

/**
 * Addition F, G1: monospace is for code, commands and tool output.
 *
 * Round 6 fixed the sites a Compose test happened to reach and left the rest —
 * the provider availability reason, the picker's own explanatory paragraph, a
 * model's context window, the daemon version — reading as machine output in
 * sheets nobody opened (verify-7 P3). Rendered assertions cannot see this: the
 * text is correct, the face is wrong.
 *
 * The rule is per *use site*, not per file. A blanket file exemption would hand
 * a whole screen a pass, and these are big files: `AccountsScreen.kt` legitimately
 * renders a device code and illegitimately rendered field labels. So every
 * monospace use must carry a `// mono:` comment saying why, within a few lines
 * above it. Anything unexplained is a finding.
 */
class MonospaceSweepTest {

    /** The type tokens whose family is Monospace, plus the family itself. */
    private val monospace = Regex("""type\.(toolRow|toolStrong|numeric)\b|FontFamily\.Monospace""")

    /** Only the file that declares the ramp is exempt. */
    private val exemptFiles = setOf("ForgeTheme.kt")

    private val justification = "// mono:"

    private fun sourceRoot(): File? {
        var dir: File? = File(System.getProperty("user.dir") ?: ".").absoluteFile
        repeat(4) {
            val candidate = File(dir, "src/main/java/ai/diffforge/haider")
            if (candidate.isDirectory) return candidate
            dir = dir?.parentFile
        }
        return null
    }

    /** A use is justified when `// mono:` appears within the four lines above it. */
    internal fun unjustified(lines: List<String>): List<Int> =
        lines.mapIndexedNotNull { index, raw ->
            val line = raw.trim()
            if (line.startsWith("//") || line.startsWith("*")) return@mapIndexedNotNull null
            if (!monospace.containsMatchIn(raw)) return@mapIndexedNotNull null
            val window = lines.subList(maxOf(0, index - 4), index)
            if (window.any { it.contains(justification) }) null else index + 1
        }

    @Test
    fun `every monospace use says why it is monospace`() {
        val root = sourceRoot()
        assumeTrue("source tree not reachable", root != null)
        val offenders = mutableListOf<String>()
        root!!.walkTopDown()
            .filter { it.isFile && it.extension == "kt" && it.name !in exemptFiles }
            .forEach { file ->
                unjustified(file.readLines()).forEach { offenders += "${file.name}:$it" }
            }
        assertEquals(
            "monospace is for code, commands and tool output — justify or convert:\n" +
                offenders.joinToString("\n"),
            0,
            offenders.size,
        )
    }

    @Test
    fun `the exemption is exactly the file that declares the ramp`() {
        assertEquals(setOf("ForgeTheme.kt"), exemptFiles)
    }

    @Test
    fun `the sweep catches an unexplained monospace and accepts an explained one`() {
        val bare = """
            |            Text(
            |                versionLine,
            |                style = type.numeric,
            |            )
        """.trimMargin().lines()
        assertEquals(listOf(3), unjustified(bare))

        val explained = """
            |            Text(
            |                socketPath,
            |                // mono: a path a person pastes into a shell.
            |                style = type.numeric,
            |            )
        """.trimMargin().lines()
        assertTrue(unjustified(explained).isEmpty())

        // A justification four lines up still counts; five does not.
        val distant = """
            |            // mono: too far away to be about this line.
            |            a()
            |            b()
            |            c()
            |            d()
            |            Text(x, style = type.toolRow)
        """.trimMargin().lines()
        assertEquals(listOf(6), unjustified(distant))
    }
}
