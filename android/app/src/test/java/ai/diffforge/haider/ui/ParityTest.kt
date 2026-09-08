package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.chat.ATTACHMENT_STRIP_TAG
import ai.diffforge.haider.ui.chat.DELIVERY_CHOOSER_TAG
import ai.diffforge.haider.ui.chat.QUEUE_PANEL_TAG
import ai.diffforge.haider.ui.chat.USAGE_LINE_TAG
import ai.diffforge.haider.ui.daemon.Attachment
import ai.diffforge.haider.ui.daemon.AttachmentLimits
import ai.diffforge.haider.ui.daemon.Delivery
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.QueueSnapshot
import ai.diffforge.haider.ui.daemon.QueuedMessage
import ai.diffforge.haider.ui.daemon.StaleQueueRevision
import ai.diffforge.haider.ui.daemon.TokenUsage
import ai.diffforge.haider.ui.daemon.UsageSnapshot
import ai.diffforge.haider.ui.settings.USAGE_FOOTER_TAG
import ai.diffforge.haider.ui.state.Overlay
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The SDK-parity slice: attachments, transcript blocks, steering and usage.
 *
 * Every shape here is the one in the Rust, read from `haider-protocol` and
 * `haider-rpc` rather than guessed — `AttachmentBlock` is tagged on `kind`,
 * `DeliveryMode` defaults to steer, `queue.remove`/`queue.promote_steer` are
 * fenced by the snapshot revision, and `est_cost_usd` is an estimate.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class ParityTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    // ---------- (1) transcript fidelity ----------

    @Test
    fun `a turn's image and file blocks render as tiles`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.attach(byteArrayOf(1, 2, 3), "image/png", name = null)
        rule.waitForIdle()
        viewModel.setDraft("look at this")
        viewModel.send()
        rule.waitForIdle()
        rule.onNodeWithTag(ATTACHMENT_STRIP_TAG, useUnmergedTree = true).assertIsDisplayed()
    }

    @Test
    fun `a per-turn usage line reads as an estimate`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.transcriptOverride = {
            ai.diffforge.haider.ui.daemon.TranscriptLoad.Complete(
                listOf(
                    ai.diffforge.haider.ui.chat.Message(
                        id = 1,
                        role = ai.diffforge.haider.ui.chat.Role.Agent,
                        text = "Done.",
                        usage = TokenUsage(
                            inputTokens = 18_400,
                            outputTokens = 2_100,
                            reasoningTokens = 900,
                            cachedTokens = 12_000,
                            estCostUsd = 0.42,
                        ),
                    ),
                ),
            )
        }
        rule.setHaiderApp(service)
        rule.waitForIdle()
        rule.onNodeWithTag(USAGE_LINE_TAG, useUnmergedTree = true).assertIsDisplayed()
        assertTrue(rule.onAllNodesWithTextSafe("18.4k in · 2.1k out · 900 thinking · 12.0k cached · ~$0.42 est.") > 0)
    }

    // ---------- (2) attachments ----------

    @Test
    fun `staging returns an image block with the wire's own shape`() = runTest {
        val service = ai.diffforge.haider.ui.daemon.FakeDaemonService(FakeScenario.Populated)
        val block = service.stageAttachment(byteArrayOf(9, 9), "image/png", name = null)
        assertTrue(block is Attachment.Image)
        val image = block as Attachment.Image
        // A CAS reference, never bytes (protocol tool.rs:386).
        assertTrue("artifact is not a CAS ref: ${image.artifact}", image.artifact.startsWith("blake3:"))
        assertEquals("image/png", image.mime)
    }

    @Test
    fun `too many attachments is the daemon's refusal, shown verbatim`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.attachmentCeiling = 1
        val viewModel = rule.setHaiderApp(service)
        viewModel.attach(byteArrayOf(1), "image/png", null)
        viewModel.attach(byteArrayOf(2), "image/jpeg", null)
        rule.waitForIdle()
        viewModel.setDraft("two pictures")
        viewModel.send()
        rule.waitForIdle()
        assertEquals(AttachmentLimits.TOO_MANY, viewModel.state.value.attachmentNotice)
        // The draft is intact: a refused turn does not eat the message.
        assertEquals("two pictures", viewModel.state.value.draft)
    }

    // ---------- (3) stop / steer / queue ----------

    @Test
    fun `send while running asks queue or steer, and Stop is untouched`() {
        val service = ComposeHost.install(FakeScenario.TurnRunning)
        val viewModel = rule.setHaiderApp(service)
        viewModel.setDraft("also check the manifest")
        rule.waitForIdle()
        viewModel.askDelivery()
        rule.waitForIdle()
        rule.onNodeWithTag(DELIVERY_CHOOSER_TAG, useUnmergedTree = true).assertIsDisplayed()
        // Addition E still holds: exactly one Stop, and it is the composer's.
        assertEquals(1, rule.onAllNodesWithContentDescriptionSafe("Stop this turn"))
    }

    @Test
    fun `queueing holds the message instead of starting a second turn`() {
        val service = ComposeHost.install(FakeScenario.TurnRunning)
        val viewModel = rule.setHaiderApp(service)
        viewModel.setDraft("after this one")
        viewModel.send(Delivery.Queue)
        rule.waitForIdle()
        assertTrue(
            service.calls.any { it.startsWith("turn.submit:") && it.contains(":queue:") },
        )
        assertEquals(1, viewModel.state.value.queue.rows.size)
    }

    @Test
    fun `a stale queue revision is refused, not applied to another row`() = runTest {
        val service = ai.diffforge.haider.ui.daemon.FakeDaemonService(FakeScenario.TurnRunning)
        service.setQueue(
            QueueSnapshot(
                revision = 7,
                supported = true,
                rows = listOf(
                    QueuedMessage("q-1", "first", Delivery.Queue, 1, 0),
                    QueuedMessage("q-2", "second", Delivery.Queue, 2, 0),
                ),
            ),
        )
        val stale = runCatching { service.removeQueued("s-nav", "q-1", revision = 6) }
        assertTrue(
            "a stale revision must be refused",
            stale.exceptionOrNull() is StaleQueueRevision,
        )
        assertEquals(2, service.queue.value.rows.size)
        // The current revision works and moves the snapshot on.
        service.removeQueued("s-nav", "q-1", revision = 7)
        assertEquals(1, service.queue.value.rows.size)
        assertEquals(8L, service.queue.value.revision)
    }

    @Test
    fun `the queue panel separates empty from unavailable`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.setQueue(QueueSnapshot(supported = false))
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Queue)
        rule.waitForIdle()
        rule.onNodeWithTag(QUEUE_PANEL_TAG, useUnmergedTree = true).assertIsDisplayed()
        rule.onNodeWithText("This daemon does not hold a queue.").assertIsDisplayed()
    }

    // ---------- (4) usage footer ----------

    @Test
    fun `the usage footer totals the report and calls the cost an estimate`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Settings)
        rule.waitForIdle()
        rule.onNodeWithTag(USAGE_FOOTER_TAG, useUnmergedTree = true).assertIsDisplayed()
        assertTrue(
            rule.onAllNodes(
                androidx.compose.ui.test.hasText("estimate, not a bill", substring = true),
            ).fetchSemanticsNodes().isNotEmpty(),
        )
        // The totals are the report's, summed, not a made-up number.
        assertTrue(
            rule.onAllNodes(
                androidx.compose.ui.test.hasText("184.2k in", substring = true),
            ).fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `a daemon with no usage report says so`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.setUsage(UsageSnapshot(supported = false))
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Settings)
        rule.waitForIdle()
        rule.onNodeWithText("This daemon does not report usage.").assertIsDisplayed()
    }
}
