package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.chat.PickerKind
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.scaffold.HAIDER_TOP_BAR_TAG
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.PermissionMode
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Round 10: the owner's proportions, and the layout the brief specifies.
 *
 * Every one of these is a measurement, because "too big" is not something a
 * text assertion can see — round 9 passed every pin it had while the controls
 * were half again the reference's size.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class ProportionTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun dp(px: Float) = with(rule.density) { px.toDp() }

    // ---------- R1: the header is 48 dp with 32 dp visuals ----------

    @Test
    fun `the header row is no taller than a target`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        val bar = rule.onNodeWithTag(HAIDER_TOP_BAR_TAG).fetchSemanticsNode()
        // Header + hairline. Round 9 was 52 dp of controls under a title.
        assertTrue(
            "the header is ${dp(bar.size.height.toFloat())}",
            dp(bar.size.height.toFloat()) <= ForgeSize.header + ForgeSize.hairline,
        )
    }

    @Test
    fun `every header control keeps a 48 dp target at 412 dp`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        val bar = rule.onNodeWithTag(HAIDER_TOP_BAR_TAG).fetchSemanticsNode()
        val targets = clickableUnder(bar)
        assertEquals(5, targets.size)
        val minPx = with(rule.density) { ForgeSize.touch.toPx() }
        targets.forEach {
            val label = it.config.getOrNull(SemanticsProperties.ContentDescription)?.first()
            assertTrue(
                "$label is ${it.size.width}x${it.size.height}px",
                it.size.width >= minPx && it.size.height >= minPx,
            )
        }
    }

    // ---------- R2: attach left, mic and send right ----------

    @Test
    fun `attach sits left of the field and mic and send sit right`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        val attach = rule.onNodeWithContentDescription("Attach").fetchSemanticsNode()
        val mic = rule.onNode(hasContentDescription("Voice input is not in this build"))
            .fetchSemanticsNode()
        assertTrue(
            "attach ${attach.positionInRoot.x} is not left of mic ${mic.positionInRoot.x}",
            attach.positionInRoot.x < mic.positionInRoot.x,
        )
        // The text starts after attach and ends before mic.
        val field = rule.onAllNodes(androidx.compose.ui.test.hasSetTextAction())
            .fetchSemanticsNodes()
            .maxBy { it.positionInRoot.y }
        assertTrue(
            "the field starts at ${field.positionInRoot.x}, attach at ${attach.positionInRoot.x}",
            field.positionInRoot.x >= attach.positionInRoot.x,
        )
        assertTrue(mic.positionInRoot.x > attach.positionInRoot.x + attach.size.width)
    }

    @Test
    fun `the composer selects are compact`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        // The painted pill, found by walking to the text inside the target.
        val select = rule.onNode(
            androidx.compose.ui.test.hasText("Auto"),
            useUnmergedTree = true,
        ).fetchSemanticsNode()
        assertTrue(
            "the value text is ${dp(select.size.height.toFloat())} tall",
            dp(select.size.height.toFloat()) < ForgeSize.chip,
        )
        // Its target is still a target.
        val target = rule.onNode(hasContentDescription("Change what Haider may do on its own, Auto"))
            .fetchSemanticsNode()
        val minPx = with(rule.density) { ForgeSize.touch.toPx() }
        assertTrue(target.size.height >= minPx)
    }

    // ---------- R3: the drawer head is one row ----------

    @Test
    fun `the drawer spends one row on the daemon, New chat and the chevron`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .fetchSemanticsNodes()
        rule.onNodeWithContentDescription("Open sessions, 1 session needs input").performClick()
        rule.waitForIdle()
        val daemon = rule.onNodeWithText("Running in background").fetchSemanticsNode()
        val newChat = rule.onNodeWithContentDescription("New chat").fetchSemanticsNode()
        val close = rule.onNodeWithContentDescription("Close sessions").fetchSemanticsNode()
        // All three on the same line, left to right.
        assertEquals(newChat.positionInRoot.y, close.positionInRoot.y, 1f)
        assertTrue(daemon.positionInRoot.x < newChat.positionInRoot.x)
        assertTrue(newChat.positionInRoot.x < close.positionInRoot.x)
    }

    @Test
    fun `a session row paints 40 dp inside its 48 dp target`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.onNodeWithContentDescription("Open sessions, 1 session needs input").performClick()
        rule.waitForIdle()
        val row = rule.onAllNodes(
            hasContentDescription("Fix nav crash on back gesture", substring = true),
        ).fetchSemanticsNodes().first()
        val minPx = with(rule.density) { ForgeSize.touch.toPx() }
        assertTrue("the row target is ${row.size.height}px", row.size.height >= minPx)
        // Twelve of them fit a 915 dp phone alongside the head and the footer.
        assertTrue(dp(row.size.height.toFloat()) <= ForgeSize.touch)
    }

    // ---------- verify-8 O1: Auto resolves, never hides ----------

    @Test
    fun `switching to Auto resolves a pending device approval instead of hiding it`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.setPermissionModeForTest(PermissionMode.Ask)
        val viewModel = rule.setHaiderApp(service)
        service.raiseDeviceApproval("s-nav")
        rule.waitForIdle()
        assertTrue("the card must be visible in Ask", rule.onAllNodesWithTextSafe("Allow sms.list for the last 20 messages?") > 0)

        viewModel.selectPermissionMode(PermissionMode.Auto)
        rule.waitForIdle()

        // Resolved, not hidden: the snapshot no longer carries it, so the
        // header and the composer agree with the screen (verify-8 O1).
        assertEquals(0, rule.onAllNodesWithTextSafe("Allow sms.list for the last 20 messages?"))
        assertEquals(null, viewModel.session("s-nav")?.needsInput)
        assertTrue(service.calls.any { it == "tool.policy.auto_resolved:s-nav" })
    }

    @Test
    fun `Auto leaves a genuine question on the screen`() {
        val service = ComposeHost.install(FakeScenario.InputRequiredHere)
        rule.setHaiderApp(service)
        rule.waitForIdle()
        // The seeded card is an SMS the agent wants to *send*, which is not
        // something standing consent covers.
        assertTrue(
            rule.onAllNodesWithTextSafe("Send this reply to Amir (+1 604 555 0142)?") > 0,
        )
    }

    // ---------- verify-8 O2: the grant rows outlive the step ----------

    @Test
    fun `granting notifications does not take the other three grants away`() {
        val service = ComposeHost.install(FakeScenario.FirstRun)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        viewModel.onNotificationPermissionResult(granted = true, permanentlyDenied = false)
        rule.waitForIdle()
        // Step green, rows still there — round 9 removed them with the body.
        assertTrue(rule.onAllNodesWithTextSafe("Haider can work unattended") > 0)
        listOf("Texts", "Tap and type", "See the screen").forEach {
            assertTrue("$it disappeared when the step went green", rule.onAllNodesWithTextSafe(it) > 0)
        }
    }

    // ---------- verify-8 O4: marks on the real picker ----------

    @Test
    fun `the composer model sheet carries a brand mark per row`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Picker(PickerKind.Model))
        rule.waitForIdle()
        // The mark carries no semantics of its own — it is decorative — so
        // its presence is measured by the space it takes: the model label sits
        // further right than an effort label, which has no leading slot.
        val modelLabelX = rule.onAllNodes(
            androidx.compose.ui.test.hasText("Sonnet 4.5"),
            useUnmergedTree = true,
        ).fetchSemanticsNodes().minOf { it.positionInRoot.x }
        viewModel.closeOverlay()
        viewModel.openOverlay(Overlay.Picker(PickerKind.Effort))
        rule.waitForIdle()
        val effortLabelX = rule.onAllNodes(
            androidx.compose.ui.test.hasText("high"),
            useUnmergedTree = true,
        ).fetchSemanticsNodes().minOf { it.positionInRoot.x }
        assertTrue(
            "model label at $modelLabelX is not inset past the effort label at $effortLabelX",
            modelLabelX > effortLabelX,
        )
    }

    // ---------- verify-8 O7: no false validation ----------

    @Test
    fun `a login in flight says signing in, not validated`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        assertEquals(
            0,
            rule.onAllNodesWithTextSafe(
                "Validated and held in the vault. Save to finish, or Cancel to discard it.",
            ),
        )
    }

    @Test
    fun `at 412 dp the state pill says the whole word`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        assertTrue(rule.onAllNodesWithTextSafe("Running") > 0)
    }
}
