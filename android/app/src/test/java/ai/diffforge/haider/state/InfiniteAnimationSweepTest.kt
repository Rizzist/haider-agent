package ai.diffforge.haider.state

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File

/**
 * Verify 10 measured what a per-frame animation costs on a real device: with
 * the app resumed on a running session and nothing changing, Haider held
 * 54–59 % CPU and SurfaceFlinger 20–34 %.
 *
 * `rememberInfiniteTransition` runs off the frame clock, so it invalidates
 * every frame whether or not anything can see it, and there was one per running
 * drawer row — in a drawer that stays composed while closed — plus the header
 * pill plus the streaming caret. `MotionTicker` replaced them with one 3 Hz
 * source; this keeps a new file from quietly reintroducing the old shape.
 *
 * Exemption is exactly the file that owns the ticker.
 */
class InfiniteAnimationSweepTest {

    private val exemptFiles = setOf("MotionTicker.kt")

    private val banned = Regex("""rememberInfiniteTransition|infiniteRepeatable""")

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
    fun `no per-frame infinite animation outside the shared ticker`() {
        val root = sourceRoot()
        assumeTrue("source tree not reachable", root != null)
        val offenders = mutableListOf<String>()
        root!!.walkTopDown()
            .filter { it.isFile && it.extension == "kt" && it.name !in exemptFiles }
            .forEach { file ->
                file.readLines().forEachIndexed { index, raw ->
                    val line = raw.trim()
                    val isComment = line.startsWith("//") || line.startsWith("*")
                    if (!isComment && banned.containsMatchIn(raw)) {
                        offenders += "${file.name}:${index + 1}: $line"
                    }
                }
            }
        assertEquals(
            "looping animation goes through MotionTicker:\n${offenders.joinToString("\n")}",
            0,
            offenders.size,
        )
    }

    @Test
    fun `the exemption is exactly the ticker`() {
        assertEquals(setOf("MotionTicker.kt"), exemptFiles)
    }

    @Test
    fun `the sweep would catch either spelling`() {
        assertTrue(banned.containsMatchIn("val t = rememberInfiniteTransition(label = \"x\")"))
        assertTrue(banned.containsMatchIn("animationSpec = infiniteRepeatable(tween(700))"))
        assertTrue(!banned.containsMatchIn("val pulse = rememberPulse(active = true)"))
    }

    @Test
    fun `the markdown cache keys on the text and nothing that blinks`() {
        val root = sourceRoot()
        assumeTrue("source tree not reachable", root != null)
        val markdown = File(root, "ui/chat/MarkdownText.kt").readText()
        // Round 12 dropped the alpha from the key and left `showCaret` in it,
        // so every caret flip still rebuilt the string and re-laid out the
        // line (verify-11 O5). The key is the text and the colours only.
        val key = Regex("""remember\(([^)]*)\)\s*\{\s*\n?\s*inlineMarkdown""")
            .find(markdown)
            ?.groupValues
            ?.get(1)
        assertTrue("no memoised inlineMarkdown call found", key != null)
        assertTrue(
            "the cache key still contains a blinking input: $key",
            !key!!.contains("showCaret") && !key.contains("caret"),
        )
    }

    @Test
    fun `the markdown builder takes no per-frame alpha`() {
        val root = sourceRoot()
        assumeTrue("source tree not reachable", root != null)
        val markdown = File(root, "ui/chat/MarkdownText.kt").readText()
        // The caret's alpha was an argument to the AnnotatedString builder, so
        // every frame re-parsed the whole message and re-laid out the
        // transcript. It is a boolean now, toggled at the ticker's rate.
        assertTrue(
            "caretAlpha is back in the markdown builder",
            !markdown.contains("caretAlpha"),
        )
    }
}
