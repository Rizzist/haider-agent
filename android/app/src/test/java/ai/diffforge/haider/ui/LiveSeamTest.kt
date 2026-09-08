package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.chat.MODEL_REFUSAL_TAG
import ai.diffforge.haider.ui.chat.Message
import ai.diffforge.haider.ui.chat.Role
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.TranscriptLoad
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.SelectionRefusalCodes
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.flow.MutableStateFlow
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The three live behaviours lane 971-3's facade needs from this side.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class LiveSeamTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    @Test
    fun `assistant output that arrives on its own reaches the screen`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val stream = MutableStateFlow<TranscriptLoad>(
            TranscriptLoad.Complete(listOf(Message(1, Role.User, "What changed?"))),
        )
        service.transcriptStream = { stream }
        rule.setHaiderApp(service)
        rule.waitForIdle()
        rule.onNodeWithText("What changed?").assertIsDisplayed()

        // No action, no reload: the daemon simply pushed. Round 6 called the
        // one-shot transcript() once and never saw this.
        stream.value = TranscriptLoad.Complete(
            listOf(
                Message(1, Role.User, "What changed?"),
                Message(2, Role.Agent, "The nav graph lost its start destination."),
            ),
        )
        rule.waitForIdle()
        rule.onNodeWithText("The nav graph lost its start destination.").assertIsDisplayed()
    }

    @Test
    fun `a partial replay stays visibly partial while it streams`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.transcriptStream = {
            MutableStateFlow(
                TranscriptLoad.Partial(
                    messages = listOf(Message(1, Role.User, "Older history")),
                    loadedThroughSeq = 512,
                    headSeq = 4096,
                    reason = "range capped at 1024 envelopes",
                ),
            )
        }
        rule.setHaiderApp(service)
        rule.waitForIdle()
        assertTrue(
            rule.onAllNodesWithTextSafe(
                "History up to 512 of 4096 — range capped at 1024 envelopes",
            ) > 0,
        )
    }

    @Test
    fun `several Running emissions create exactly one session`() {
        val service = ComposeHost.install(FakeScenario.EmptyRosterReady)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        // The status flow really does emit Running more than once on a live
        // connection; each used to race the others into createSession().
        repeat(6) { viewModel.ensureActiveSession() }
        rule.waitForIdle()
        assertEquals(
            "concurrent auto-creation made more than one session",
            1,
            service.calls.count { it == "createSession" },
        )
    }

    @Test
    fun `a refused model change is shown, and only a person can confirm it`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        // `cache_epoch_confirmation_required` is the code the daemon actually
        // sends (frame.rs:271). The fixture used to arm
        // "confirm_new_epoch_required", which is the name of a REQUEST FIELD and
        // is not a code any frame carries — the same class of defect the roster
        // already fixed once for `already_resolved`. The panel now classifies the
        // code to decide whether a confirmation can fix the refusal at all, so an
        // invented code correctly gets no confirm button.
        service.failNextSelection(SelectionRefusalCodes.CACHE_EPOCH_CONFIRMATION_REQUIRED)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.ModelPicker)
        viewModel.selectModel("anthropic", "claude-opus-4-5")
        rule.waitForIdle()

        rule.onNodeWithTag(MODEL_REFUSAL_TAG).assertIsDisplayed()
        rule.onNodeWithText(SelectionRefusalCodes.CACHE_EPOCH_CONFIRMATION_REQUIRED)
            .assertIsDisplayed()
        // Nothing has been confirmed yet.
        assertEquals(0, service.confirmedSelections)

        rule.onNodeWithText("Change it anyway").performClick()
        rule.waitForIdle()
        assertEquals(1, service.confirmedSelections)
    }
}
