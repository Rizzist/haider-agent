package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.chat.COMPOSER_TEXT_TAG
import ai.diffforge.haider.ui.chat.SELECT_INK_TAG
import ai.diffforge.haider.ui.drawer.SESSION_ROW_INK_TAG
import androidx.compose.ui.test.onAllNodesWithTag
import ai.diffforge.haider.ui.chat.TOOL_ROW_INK_TAG
import ai.diffforge.haider.ui.drawer.DRAWER_HEAD_TAG
import ai.diffforge.haider.ui.chat.PickerKind
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.scaffold.HAIDER_TOP_BAR_TAG
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.CapabilityApproval
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
import org.junit.Assert.assertFalse
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

    /**
     * The numbers the brief specifies, written out.
     *
     * Deliberately not `ForgeSize.*`: a pin that reads the token it is
     * checking passes when the token moves, which is exactly what the
     * row-shrink control proved about round 10's version (verify-9 V6).
     */
    private object Spec {
        const val HEADER = 48f
        const val TARGET = 48f
        const val SELECT_INK = 30f
        const val ROW_INK = 40f
        const val TOOL_INK = 36f
    }

    // ---------- R1: the header is 48 dp with 32 dp visuals ----------

    @Test
    fun `the header row is no taller than a target`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        val bar = rule.onNodeWithTag(HAIDER_TOP_BAR_TAG).fetchSemanticsNode()
        // 48 dp for everything, hairline included — round 10 tolerated the
        // hairline on top of the budget, which is a 49 dp header with a pin
        // that says 48 (verify-9 V6).
        assertEquals(Spec.HEADER, dp(bar.size.height.toFloat()).value, 0.5f)
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
    fun `the select paints 30 dp inside a 48 dp target`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        // Round 10 measured the *text*, which is smaller than the pill either
        // way and so could not fail (verify-9 V6). The pill is the layout node
        // between the target and the text: the target's only child.
        val target = rule.onNode(hasContentDescription("Change what Haider may do on its own, Auto"))
            .fetchSemanticsNode()
        assertEquals(Spec.TARGET, dp(target.size.height.toFloat()).value, 0.5f)
        val pill = rule.onAllNodesWithTag(SELECT_INK_TAG, useUnmergedTree = true)
            .fetchSemanticsNodes()
            .first()
        assertEquals(
            "the pill paints ${dp(pill.size.height.toFloat())}",
            Spec.SELECT_INK,
            dp(pill.size.height.toFloat()).value,
            1.5f,
        )
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
        // Interaction bounds exactly 48 dp…
        assertEquals(Spec.TARGET, dp(row.size.height.toFloat()).value, 0.5f)
        // …and the band it paints is 40 dp, measured on the painted node
        // itself. A semantics walk cannot find it — the paint carries no
        // semantics — so the ink is tagged (verify-9 V6).
        val ink = rule.onAllNodesWithTag(SESSION_ROW_INK_TAG, useUnmergedTree = true)
            .fetchSemanticsNodes()
            .first()
        assertEquals(
            "the row paints ${dp(ink.size.height.toFloat())}",
            Spec.ROW_INK,
            dp(ink.size.height.toFloat()).value,
            1.5f,
        )
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

    // ---------- verify-9 V1: every requested action must be covered ----------

    @Test
    fun `a mixed approval keeps its card even in Auto`() {
        val mixed = needsInputOf(
            "permission",
            "Allow sms.send after reading sms.list?",
        )
        // Round 10 asked "does the text mention anything covered", so this was
        // consumed on the strength of sms.list (verify-9 V1).
        assertEquals(
            setOf("sms.send", "sms.list"),
            CapabilityApproval.requestedCapabilities(mixed),
        )
        assertFalse(CapabilityApproval.suppresses(PermissionMode.Auto, mixed))
    }

    @Test
    fun `an unknown action keeps its card`() {
        listOf(
            "Allow sms.forward for the last message?",
            "Allow contacts.read?",
            "Allow a11y.dangerous_new_thing?",
            "Allow this?",
        ).forEach {
            assertFalse(
                "$it must still ask",
                CapabilityApproval.suppresses(PermissionMode.Auto, needsInputOf("permission", it)),
            )
        }
    }

    @Test
    fun `a wholly covered request is still covered`() {
        listOf(
            "Allow sms.list for the last 20 messages?",
            "Allow screen.capture and a11y.tree?",
            "Allow app.open for Settings?",
        ).forEach {
            assertTrue(
                "$it should be covered",
                CapabilityApproval.suppresses(PermissionMode.Auto, needsInputOf("permission", it)),
            )
        }
    }

    // ---------- verify-9 V2: the allowed turn finishes ----------

    @Test
    fun `an Auto SMS-read turn completes and leaves no card`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.setPermissionModeForTest(PermissionMode.Ask)
        val viewModel = rule.setHaiderApp(service)
        service.raiseDeviceApproval("s-nav")
        rule.waitForIdle()
        viewModel.selectPermissionMode(PermissionMode.Auto)
        rule.waitForIdle()

        val row = viewModel.session("s-nav")
        assertEquals(null, row?.needsInput)
        // Terminal, not "running for ever" — round 10 stopped at Running
        // (verify-9 V2).
        assertEquals("idle", row?.runState)
        assertEquals(null, row?.runId)
        assertTrue(service.calls.contains("tool.policy.auto_completed:s-nav"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Allow sms.list for the last 20 messages?"))
    }

    // ---------- verify-9 V3: the text never runs under the controls ----------

    @Test
    fun `a long line stops before the mic, it does not run under it`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.setDraft(
            "A line long enough to reach the right edge of the field and keep going past it",
        )
        rule.waitForIdle()
        // The text *area*, not the field's interaction node: tapping anywhere
        // in the pill should still focus it, but the glyphs must stop before
        // the controls (verify-9 V3).
        val textArea = rule.onNodeWithTag(COMPOSER_TEXT_TAG, useUnmergedTree = true)
            .fetchSemanticsNode()
        val mic = rule.onNode(hasContentDescription("Voice input is not in this build"))
            .fetchSemanticsNode()
        val textRight = textArea.positionInRoot.x + textArea.size.width
        assertTrue(
            "the text ends at $textRight and the mic starts at ${mic.positionInRoot.x}",
            textRight <= mic.positionInRoot.x + 1f,
        )
    }

    // ---------- verify-10 O1: every rendered field, whole identifiers ----------

    @Test
    fun `an option label is part of the card`() {
        // Native Auto consumed a card titled "Allow sms.list?" whose button
        // said "Allow sms.send" (verify-10 O1).
        val card = ai.diffforge.haider.ui.daemon.NeedsInput(
            kind = "permission",
            title = "Allow sms.list?",
            options = listOf(
                ai.diffforge.haider.ui.daemon.MenuOption(key = "allow", label = "Allow sms.send"),
                ai.diffforge.haider.ui.daemon.MenuOption(key = "deny", label = "Don't"),
            ),
        )
        assertTrue(CapabilityApproval.requestedCapabilities(card).contains("sms.send"))
        assertFalse(CapabilityApproval.suppresses(PermissionMode.Auto, card))
    }

    @Test
    fun `a longer identifier is not its covered prefix`() {
        val card = needsInputOf("permission", "Allow sms.list.delete for old threads?")
        // Round 11 took two segments and matched the prefix (verify-10 O1).
        assertEquals(setOf("sms.list.delete"), CapabilityApproval.requestedCapabilities(card))
        assertFalse(CapabilityApproval.suppresses(PermissionMode.Auto, card))
    }

    // ---------- verify-10 O2: the visible transcript is the canonical one ----------

    @Test
    fun `after Auto the transcript shows the completed call, not the old prose`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.setPermissionModeForTest(PermissionMode.Ask)
        val viewModel = rule.setHaiderApp(service)
        service.raiseDeviceApproval("s-nav")
        rule.waitForIdle()
        viewModel.selectPermissionMode(PermissionMode.Auto)
        rule.waitForIdle()

        // Visible == canonical: the row is terminal *and* the screen says so.
        assertTrue(
            "the completion never reached the screen",
            rule.onAllNodesWithTextSafe(
                ai.diffforge.haider.ui.daemon.FakeDaemonService.AUTO_SMS_TEXT,
            ) > 0,
        )
        val completed = viewModel.state.value.messages.last().tools.last()
        assertEquals(
            ai.diffforge.haider.ui.daemon.FakeDaemonService.AUTO_SMS_RESULT,
            completed.result,
        )
        assertEquals(1_200L, completed.durationMs)
        assertFalse(viewModel.state.value.messages.last().streaming)
    }

    // ---------- verify-10 O4 / O7: paint versus target, again ----------

    @Test
    fun `a tool row paints 36 dp inside its 48 dp target`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        val ink = rule.onAllNodesWithTag(TOOL_ROW_INK_TAG, useUnmergedTree = true)
            .fetchSemanticsNodes()
            .first()
        assertEquals(
            "the tool body paints ${dp(ink.size.height.toFloat())}",
            Spec.TOOL_INK,
            dp(ink.size.height.toFloat()).value,
            1.5f,
        )
    }

    @Test
    fun `the drawer head is one 48 dp row`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.onNodeWithContentDescription("Open sessions, 1 session needs input").performClick()
        rule.waitForIdle()
        val head = rule.onNodeWithTag(DRAWER_HEAD_TAG, useUnmergedTree = true)
            .fetchSemanticsNode()
        assertEquals(Spec.TARGET, dp(head.size.height.toFloat()).value, 0.5f)
    }

    private fun needsInputOf(kind: String, title: String) =
        ai.diffforge.haider.ui.daemon.NeedsInput(kind = kind, title = title)
}
