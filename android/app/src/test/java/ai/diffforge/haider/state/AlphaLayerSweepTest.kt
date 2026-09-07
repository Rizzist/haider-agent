package ai.diffforge.haider.state

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File

/**
 * Round 5, P2: the Save button rendered with no fill at all on the device.
 *
 * `Modifier.alpha` is a graphics layer, and a layer only affects what comes
 * *after* it in the chain. `ForgeButton` had it after `.background()` and
 * `.border()`, so the fill was painted outside the layer that was supposed to
 * be dimming it — a state modifier that did not actually own the pixels it
 * claimed. On the JVM renderer at alpha 1f that is invisible; on a real device
 * across an IME relayout it was not.
 *
 * A rendered assertion cannot see this (the pre-fix source passes the pixel pin
 * at alpha 1f — recorded honestly in the round 6 report), so the rule is about
 * the modifier chain, and this reads it.
 */
class AlphaLayerSweepTest {

    private fun sourceRoot(): File? {
        var dir: File? = File(System.getProperty("user.dir") ?: ".").absoluteFile
        repeat(4) {
            val candidate = File(dir, "src/main/java/ai/diffforge/haider")
            if (candidate.isDirectory) return candidate
            dir = dir?.parentFile
        }
        return null
    }

    /**
     * Offenders are `.alpha(` calls that follow a paint call in the same chain.
     * A chain is a run of lines each beginning with `.`; anything else ends it.
     */
    internal fun offenders(lines: List<String>): List<Int> {
        val found = mutableListOf<Int>()
        var paintedInChain = false
        lines.forEachIndexed { index, raw ->
            val line = raw.trim()
            if (!line.startsWith(".")) {
                paintedInChain = false
                return@forEachIndexed
            }
            if (line.startsWith(".alpha(") && paintedInChain) found += index + 1
            if (line.startsWith(".background(") || line.startsWith(".border(")) {
                paintedInChain = true
            }
        }
        return found
    }

    @Test
    fun `no dimming layer is applied after the fill it dims`() {
        val root = sourceRoot()
        assumeTrue("source tree not reachable", root != null)
        val bad = mutableListOf<String>()
        root!!.walkTopDown()
            .filter { it.isFile && it.extension == "kt" }
            .forEach { file ->
                offenders(file.readLines()).forEach { bad += "${file.name}:$it" }
            }
        assertEquals(
            "Modifier.alpha must precede the background/border it dims:\n${bad.joinToString("\n")}",
            0,
            bad.size,
        )
    }

    @Test
    fun `the sweep replays the round 5 defect and accepts the fix`() {
        // Exactly the pre-fix ForgeButton chain.
        val defect = """
            |            modifier = Modifier
            |                .heightIn(min = minHeight)
            |                .clip(ForgeShapes.pill)
            |                .background(fill)
            |                .border(ForgeSize.hairline, outline, ForgeShapes.pill)
            |                .alpha(if (enabled) 1f else DISABLED_ALPHA)
            |                .padding(horizontal = ForgeSpace.xl),
        """.trimMargin().lines()
        assertEquals(listOf(6), offenders(defect))

        val fixed = """
            |            modifier = Modifier
            |                .alpha(if (enabled) 1f else DISABLED_ALPHA)
            |                .clip(ForgeShapes.pill)
            |                .background(fill)
            |                .border(ForgeSize.hairline, outline, ForgeShapes.pill),
        """.trimMargin().lines()
        assertTrue(offenders(fixed).isEmpty())

        // Two separate chains do not contaminate each other.
        val separate = """
            |        Box(Modifier.background(colors.bg)) {
            |            Text("x")
            |        }
            |        Box(
            |            Modifier
            |                .alpha(0.5f)
            |        )
        """.trimMargin().lines()
        assertTrue(offenders(separate).isEmpty())
    }
}
