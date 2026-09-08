package ai.diffforge.haider.state

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File

/**
 * Addition F, G2: uppercase tracked labels belong to the drawer section headers
 * and nowhere else.
 *
 * A rendered-text assertion can only cover the screens a test happens to open —
 * round 6 found PROVIDER, EFFORT, CONFIRM CACHE EPOCH and RUN FAILED sitting in
 * sheets that no Compose test reached. Shouting is a property of the *strings*,
 * so this reads them: every user-visible literal in `strings.xml` and every
 * hard-coded `Text("…")` in the source.
 *
 * The three drawer group headers are named exemptions. Widening that set is an
 * edit here, in the open.
 */
class ShoutSweepTest {

    private val exemptStrings = setOf(
        "drawer_group_needs_you",
        "drawer_group_active",
        "drawer_group_recent",
    )

    /**
     * Acronyms are how the thing is spelled, not a shout: "API key" and
     * "Grant SMS access" are sentence case with a name in them.
     */
    private val acronyms = setOf(
        "API", "SMS", "MMS", "URL", "URI", "IDE", "SDK", "NDK", "APK", "ABI",
        "CPU", "RAM", "PID", "ADB", "JSON", "HTTP", "HTTPS", "TLS", "UUID",
        "GPS", "OTP", "RPC", "UI", "OS", "ID", "MB", "GB", "KB",
        // Lane 971-UI-workflows: the workflow screen's second view is the
        // activation AST, and the graph it draws is a DAG. Both are how the
        // things are spelled — in the Rust, in the pipe spec and on the desktop
        // — not this app raising its voice. This is the acronym list, not an
        // exemption: `exemptStrings` is still the three drawer headers.
        "AST", "DAG",
    )
    private val word = Regex("""[A-Za-z][A-Za-z0-9]*""")

    /** A word of three or more capitals that is not one of those names. */
    private fun shouts(text: String): Boolean = word.findAll(text).any {
        val w = it.value
        w.length >= 3 && w == w.uppercase() && w.any(Char::isLetter) && w !in acronyms
    }

    private fun moduleRoot(): File? {
        var dir: File? = File(System.getProperty("user.dir") ?: ".").absoluteFile
        repeat(4) {
            if (File(dir, "src/main/java/ai/diffforge/haider").isDirectory) return dir
            dir = dir?.parentFile
        }
        return null
    }

    @Test
    fun `no shouting string survives outside the drawer section headers`() {
        val root = moduleRoot()
        assumeTrue("module root not reachable", root != null)
        val xml = File(root, "src/main/res/values/strings.xml")
        assertTrue("strings.xml not found at $xml", xml.isFile)
        val entry = Regex("""<string name="([^"]+)">(.*)</string>""")
        val offenders = xml.readLines().mapIndexedNotNull { index, line ->
            val match = entry.find(line) ?: return@mapIndexedNotNull null
            val (name, value) = match.destructured
            if (name in exemptStrings) return@mapIndexedNotNull null
            // Format specifiers and escaped entities are not words.
            val prose = value.replace(Regex("""%\d+\$[sd]"""), " ").replace("&#", " ")
            if (shouts(prose)) "strings.xml:${index + 1}: $name = $value" else null
        }
        assertEquals("these strings are still shouting:\n${offenders.joinToString("\n")}", 0, offenders.size)
    }

    @Test
    fun `no shouting literal is hard-coded into a composable`() {
        val root = moduleRoot()
        assumeTrue("module root not reachable", root != null)
        val literal = Regex("""Text\(\s*"([^"]{3,})"""")
        val offenders = mutableListOf<String>()
        File(root, "src/main/java/ai/diffforge/haider").walkTopDown()
            .filter { it.isFile && it.extension == "kt" }
            .forEach { file ->
                file.readLines().forEachIndexed { index, raw ->
                    val text = literal.find(raw)?.groupValues?.get(1) ?: return@forEachIndexed
                    if (shouts(text)) offenders += "${file.name}:${index + 1}: $text"
                }
            }
        assertEquals("these literals are still shouting:\n${offenders.joinToString("\n")}", 0, offenders.size)
    }

    @Test
    fun `the sweep would actually catch a shout`() {
        // A sweep that matches nothing passes forever.
        assertTrue(shouts("CONFIRM CACHE EPOCH"))
        assertTrue(shouts("RUN FAILED"))
        assertTrue(shouts("PROVIDER"))
        assertTrue(shouts("NEEDS YOU"))
        assertTrue(shouts("1 RUNNING"))
        // Sentence case, product names and spelled-out acronyms are not shouting.
        assertTrue(!shouts("Needs input"))
        assertTrue(!shouts("API key"))
        assertTrue(!shouts("Grant SMS access"))
        assertTrue(!shouts("AST"))
        assertTrue(!shouts("Draw the DAG"))
        // The list is names, not a licence: an invented one still shouts.
        assertTrue(shouts("XYZ"))
        assertTrue(!shouts("Haider 0.0.971"))
        assertTrue(!shouts("5 sessions · 3 active · 58 MB · up 4h12m"))
    }

    @Test
    fun `the exemption list is exactly the three drawer headers`() {
        assertEquals(3, exemptStrings.size)
        assertTrue(exemptStrings.all { it.startsWith("drawer_group_") })
    }
}
