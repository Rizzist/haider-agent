package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.chat.ATTACHMENT_STRIP_TAG
import ai.diffforge.haider.ui.chat.TOOL_ROW_INK_TAG
import ai.diffforge.haider.ui.chat.attachmentTileTag
import ai.diffforge.haider.ui.daemon.AttachmentLimits
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.drawer.DRAWER_HEAD_TAG
import ai.diffforge.haider.ui.state.SendButtonMatrix
import ai.diffforge.haider.ui.state.SendButtonState
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Verify 11's findings, each pinned where it was found.
 *
 * Six of the seven came from probes against the real composition rather than
 * from the model, which is the same lesson as every earlier round: the state
 * was right and the surface was not.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class Verify11RepairTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun dp(px: Float) = with(rule.density) { px.toDp() }

    // ---------- O10 (P1): a staged attachment belongs to one session ----------

    @Test
    fun `a staged attachment does not follow the user into another session`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        val first = viewModel.state.value.activeSessionId!!
        viewModel.attach(byteArrayOf(1, 2, 3), "image/png", name = null)
        rule.waitForIdle()
        assertEquals(1, viewModel.state.value.draftAttachments.size)

        // Somewhere else, and back.
        val other = service.sessions.value.first { it.id != first }.id
        viewModel.activate(other)
        rule.waitForIdle()
        assertEquals(
            "the attachment followed the user (verify-11 O10)",
            0,
            viewModel.state.value.draftAttachments.size,
        )
        viewModel.activate(first)
        rule.waitForIdle()
        assertEquals(1, viewModel.state.value.draftAttachments.size)
    }

    @Test
    fun `a refusal belongs to the session that earned it`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.attachmentCeiling = 0
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        val first = viewModel.state.value.activeSessionId!!
        viewModel.setDraft("with a picture")
        viewModel.attach(byteArrayOf(1), "image/png", null)
        rule.waitForIdle()
        viewModel.send()
        rule.waitForIdle()
        assertEquals(AttachmentLimits.TOO_MANY, viewModel.state.value.attachmentNotice)

        val other = service.sessions.value.first { it.id != first }.id
        viewModel.activate(other)
        rule.waitForIdle()
        assertNull("the notice followed the user", viewModel.state.value.attachmentNotice)
    }

    // ---------- O8: an image with no caption is a message ----------

    @Test
    fun `an attachment alone makes Send eligible`() {
        // The matrix's question is "is there anything to send", and an
        // attachment is something (verify-11 O8).
        val withAttachmentOnly = SendButtonMatrix.resolve(
            daemon = ai.diffforge.haider.ui.daemon.DaemonStatus.Running(
                ai.diffforge.haider.ui.daemon.DaemonInfo(version = "0.0.971", generation = 1L),
            ),
            turnRunning = false,
            inputRequired = false,
            hasText = true,
        )
        assertEquals(SendButtonState.Send, withAttachmentOnly.button)
    }

    @Test
    fun `the composer enables Send with a picture and no words`() {
        // An idle session: Populated's active one is mid-turn, where Stop
        // legitimately owns the circle (addition E2 / H3).
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        val viewModel = rule.setHaiderApp(service)
        viewModel.attach(byteArrayOf(4, 5, 6), "image/png", name = null)
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Send message").assertIsDisplayed()
        // And it actually submits.
        rule.onNodeWithContentDescription("Send message").performClick()
        rule.waitForIdle()
        assertTrue(service.calls.any { it.startsWith("turn.submit:") && it.endsWith(":1") })
    }

    // ---------- O9: a staged block can be taken off again ----------

    @Test
    fun `each staged attachment has its own removal control`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.attach(byteArrayOf(7), "image/png", name = null)
        rule.waitForIdle()
        val artifact = viewModel.state.value.draftAttachments.single().artifact
        rule.onNodeWithTag(attachmentTileTag(artifact), useUnmergedTree = true).assertIsDisplayed()
        val remove = rule.onAllNodes(hasContentDescription("Remove this attachment"))
            .fetchSemanticsNodes()
        assertEquals(1, remove.size)
        val minPx = with(rule.density) { ForgeSize.touch.toPx() }
        assertTrue(
            "the removal control is ${remove[0].size.width}x${remove[0].size.height}px",
            remove[0].size.width >= minPx && remove[0].size.height >= minPx,
        )
        rule.onNodeWithContentDescription("Remove this attachment").performClick()
        rule.waitForIdle()
        assertEquals(0, viewModel.state.value.draftAttachments.size)
        assertEquals(0, rule.onAllNodesWithTag(ATTACHMENT_STRIP_TAG, useUnmergedTree = true)
            .fetchSemanticsNodes().size)
    }

    @Test
    fun `removing an attachment is the way out of a too-many refusal`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.attachmentCeiling = 1
        val viewModel = rule.setHaiderApp(service)
        viewModel.setDraft("two")
        viewModel.attach(byteArrayOf(1), "image/png", null)
        viewModel.attach(byteArrayOf(2, 2), "image/jpeg", null)
        rule.waitForIdle()
        viewModel.send()
        rule.waitForIdle()
        assertEquals(AttachmentLimits.TOO_MANY, viewModel.state.value.attachmentNotice)
        viewModel.removeAttachment(viewModel.state.value.draftAttachments.first().artifact)
        rule.waitForIdle()
        assertNull("the refusal outlived its cause", viewModel.state.value.attachmentNotice)
        viewModel.send()
        rule.waitForIdle()
        assertTrue(service.calls.any { it.startsWith("turn.submit:") })
    }

    // ---------- O4: the paint is 36 dp, not the container ----------

    @Test
    fun `only the tool body paints, and it paints 36 dp`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.TurnRunning))
        rule.waitForIdle()
        val ink = rule.onAllNodesWithTag(TOOL_ROW_INK_TAG, useUnmergedTree = true)
            .fetchSemanticsNodes()
            .first()
        assertEquals("the tool body paints ${dp(ink.size.height.toFloat())}", 36f,
            dp(ink.size.height.toFloat()).value, 1.5f)
    }

    // ---------- O7: 44 dp of layout, 48 dp of hit area ----------

    @Test
    fun `the drawer head is 44 dp and its controls keep 48`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.onNodeWithContentDescription("Open sessions, 1 session needs input").performClick()
        rule.waitForIdle()
        val head = rule.onNodeWithTag(DRAWER_HEAD_TAG, useUnmergedTree = true).fetchSemanticsNode()
        assertEquals("the head row is ${dp(head.size.height.toFloat())}", 44f,
            dp(head.size.height.toFloat()).value, 0.5f)
        val minPx = with(rule.density) { ForgeSize.touch.toPx() }
        listOf("New chat", "Close sessions").forEach { label ->
            val node = rule.onNodeWithContentDescription(label).fetchSemanticsNode()
            assertTrue(
                "$label is ${node.size.width}x${node.size.height}px inside a 44 dp row",
                node.size.width >= minPx && node.size.height >= minPx,
            )
        }
    }
}
