package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.scaffold.HAIDER_TOP_BAR_TAG
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The narrow header, which is where verify-8 O3 was found: at 360 dp the
 * unconstrained state pill squeezed the theme circle to 28 dp.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w360dp-h800dp-xhdpi")
class NarrowHeaderTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `all five controls keep 48 dp at 360 dp with a needs-input session`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.InputRequiredHere))
        rule.waitForIdle()
        val bar = rule.onNodeWithTag(HAIDER_TOP_BAR_TAG).fetchSemanticsNode()
        val targets = clickableUnder(bar)
        assertEquals(5, targets.size)
        val minPx = with(rule.density) { ForgeSize.touch.toPx() }
        targets.forEach {
            val label = it.config.getOrNull(SemanticsProperties.ContentDescription)?.first()
            assertTrue(
                "$label is ${it.size.width}x${it.size.height}px at 360 dp",
                it.size.width >= minPx && it.size.height >= minPx,
            )
        }
    }

    @Test
    fun `the pill drops its word rather than ellipsising it, and still speaks`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.InputRequiredHere))
        rule.waitForIdle()
        // No "Needs…" or "Runn…" on the row.
        assertEquals(0, rule.onAllNodesWithTextSafe("Needs you"))
        // The word is still there for a screen reader.
        assertTrue(
            rule.onAllNodes(hasContentDescription("Needs you")).fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `the three selects share one row at 360 dp`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        // Literal coordinates, as the finding asked: all three centres on the
        // same y. Round 11 put Permissions 52 dp lower (verify-10 O3).
        val ys = listOf(
            "Change model, Sonnet 4.5",
            "Change effort, high",
            "Change what Haider may do on its own, Auto",
        ).map { description ->
            rule.onNode(hasContentDescription(description)).fetchSemanticsNode()
                .positionInRoot.y
        }
        assertEquals("select centres: $ys", ys[0], ys[1], 1f)
        assertEquals("select centres: $ys", ys[0], ys[2], 1f)
    }

    /**
     * The values have to be readable, not just present (971-V F8).
     *
     * At 360 dp a third of the row held a label and an ellipsis, so the current
     * permission mode could not be read without opening its own picker. Below
     * the breakpoint the labels drop and the value gets the chip.
     */
    @Test
    fun `at 360 dp the selects show their value and drop their label`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        // The values are on screen, whole.
        assertTrue(rule.onAllNodesWithTextSafe("Auto") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("high") > 0)
        assertTrue(rule.onAllNodesWithTextSafe("Sonnet 4.5") > 0)
        // The labels are not; they live in the content description instead.
        assertEquals(0, rule.onAllNodesWithTextSafe("Permissions"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Effort"))
        assertTrue(
            rule.onAllNodes(hasContentDescription("Change what Haider may do on its own, Auto"))
                .fetchSemanticsNodes().isNotEmpty(),
        )
    }
}
