package ai.diffforge.haider.ui

import ai.diffforge.haider.FILE_PICKER_UNAVAILABLE
import ai.diffforge.haider.IMAGE_PICKER_UNAVAILABLE
import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.loom.LOOMS_SCREEN_TAG
import ai.diffforge.haider.ui.loom.LoomAuthorKind
import ai.diffforge.haider.ui.state.Overlay
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The 971-V UI findings that need the real composition.
 *
 * The pure policies live beside them — [ai.diffforge.haider.state.StandingConsentTest],
 * [ai.diffforge.haider.state.OverlayNavigationTest] and
 * [ai.diffforge.haider.ui.accounts.AccountSetupPolicyTest] — because a rule
 * stated in one place is cheaper to keep true than one asserted through a
 * screen.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class Verify12RepairTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    // ---------- F10: a missing picker is reported, not fatal ----------

    @Test
    fun `an absent image picker reports itself on the composer`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        // What the Activity does when `launch` finds no activity to handle the
        // intent: on the 16 KiB image nothing serves ACTION_PICK_IMAGES, and
        // the unhandled throw killed the UI process (971-V F10).
        viewModel.noteAttachmentUnavailable(IMAGE_PICKER_UNAVAILABLE)
        rule.waitForIdle()
        assertEquals(IMAGE_PICKER_UNAVAILABLE, viewModel.state.value.attachmentNotice)
        assertTrue(rule.onAllNodesWithTextSafe(IMAGE_PICKER_UNAVAILABLE) > 0)
        // And it is dismissible, like every other attachment notice.
        viewModel.dismissAttachmentNotice()
        rule.waitForIdle()
        assertNull(viewModel.state.value.attachmentNotice)
        assertEquals(0, rule.onAllNodesWithTextSafe(IMAGE_PICKER_UNAVAILABLE))
    }

    @Test
    fun `a notice survives having no session to attach to`() {
        // First run has no active session, and the sheet is still openable, so
        // the report must not be dropped for want of somewhere to file it.
        val service = ComposeHost.install(FakeScenario.FirstRun)
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        assertNull(viewModel.state.value.activeSessionId)
        viewModel.noteAttachmentUnavailable(FILE_PICKER_UNAVAILABLE)
        rule.waitForIdle()
        assertEquals(FILE_PICKER_UNAVAILABLE, viewModel.state.value.attachmentNotice)
    }

    // ---------- F8: getting out of Looms ----------

    @Test
    fun `back leaves Looms for Settings and Settings for the session deck`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Looms)
        rule.waitForIdle()
        rule.onNodeWithTag(LOOMS_SCREEN_TAG).assertExists()
        viewModel.back()
        rule.waitForIdle()
        assertEquals(Overlay.Settings, viewModel.state.value.overlay)
        assertEquals(0, rule.onAllNodesWithTagSafe(LOOMS_SCREEN_TAG))
        viewModel.back()
        rule.waitForIdle()
        assertEquals(Overlay.None, viewModel.state.value.overlay)
    }

    @Test
    fun `back leaves authoring for the Looms screen it was opened from`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.LoomAuthoring(LoomAuthorKind.Workflow))
        rule.waitForIdle()
        viewModel.back()
        rule.waitForIdle()
        assertEquals(Overlay.Looms, viewModel.state.value.overlay)
        rule.onNodeWithTag(LOOMS_SCREEN_TAG).assertExists()
    }
}
