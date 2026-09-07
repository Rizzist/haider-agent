package ai.diffforge.haider.state

import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File

/**
 * Acceptance 6.3.12 is a rule about the *source*, so it needs a source test.
 *
 * Round 1 asserted it with a `grep` in a report, and the verifier proved the
 * gap by replacing `ForgeSpace.xl` with a literal `16.dp`: all 196 tests
 * passed. A behavioural test cannot see the difference — 16 dp is 16 dp — so
 * this reads the files instead.
 *
 * Exemptions are exactly two files, by name. There is no prefix matching and no
 * "layout-ish" heuristic: the token objects declare the scale, and nothing else
 * may spell a dimension.
 */
class DpLiteralSweepTest {

    private val exemptFiles = setOf("ForgeDimens.kt", "ForgeTheme.kt")
    private val literal = Regex("""\b\d+(\.\d+)?\.dp\b""")

    private fun sourceRoot(): File? {
        var dir: File? = File(System.getProperty("user.dir") ?: ".").absoluteFile
        repeat(4) {
            val candidate = File(dir, "src/main/java/ai/diffforge/haider")
            if (candidate.isDirectory) return candidate
            dir = dir?.parentFile
        }
        return null
    }

    @Test
    fun `no dp literal outside the token objects`() {
        val root = sourceRoot()
        assumeTrue("source tree not reachable from ${System.getProperty("user.dir")}", root != null)
        val offenders = mutableListOf<String>()
        root!!.walkTopDown()
            .filter { it.isFile && it.extension == "kt" }
            .filterNot { it.name in exemptFiles }
            .forEach { file ->
                file.readLines().forEachIndexed { index, raw ->
                    val line = raw.trim()
                    // Prose about the old code is not the old code.
                    val isComment = line.startsWith("//") || line.startsWith("*") ||
                        line.startsWith("/*")
                    if (!isComment && literal.containsMatchIn(raw)) {
                        offenders += "${file.name}:${index + 1}: $line"
                    }
                }
            }
        assertTrue(
            "dp literals must come from ForgeSpace/ForgeSize/ForgeShapes:\n" +
                offenders.joinToString("\n"),
            offenders.isEmpty(),
        )
    }

    @Test
    fun `the exemption list is exactly the token files`() {
        // Stated as an assertion so widening it is a deliberate edit here,
        // not a quiet addition somewhere else.
        assertTrue(exemptFiles == setOf("ForgeDimens.kt", "ForgeTheme.kt"))
    }

    @Test
    fun `the sweep would actually catch a literal`() {
        // Guards the regex itself: a sweep that matches nothing passes forever.
        assertTrue(literal.containsMatchIn(".padding(horizontal = 16.dp)"))
        assertTrue(literal.containsMatchIn(".size(10.5.dp)"))
        assertTrue(literal.containsMatchIn("heightIn(min = 48.dp)"))
        assertTrue(!literal.containsMatchIn(".padding(ForgeSpace.xl)"))
    }
}
