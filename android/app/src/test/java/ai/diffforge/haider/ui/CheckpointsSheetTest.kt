package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.checkpoints.BRANCHES_NAME_FIELD_TAG
import ai.diffforge.haider.ui.checkpoints.BRANCHES_SHEET_TAG
import ai.diffforge.haider.ui.checkpoints.CHECKPOINTS_CONFIRM_TAG
import ai.diffforge.haider.ui.checkpoints.CHECKPOINTS_SHEET_TAG
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.state.Overlay
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performTextInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The checkpoints and branch sheets on screen.
 *
 * The pins that matter here are the ones a person can walk into: a destructive
 * command that fires on one tap, an unrecognised kind rendered as something
 * else, a "not published" fact quietly filled in, and a branch row that claims
 * to have moved history it only chose for the next turn.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class CheckpointsSheetTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun open(session: String = "s-nav") =
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated)).also { viewModel ->
            viewModel.openCheckpoints(session)
            rule.waitForIdle()
        }

    @Test
    fun `the sheet lists the timeline with its published facts`() {
        open()
        rule.onNodeWithTag(CHECKPOINTS_SHEET_TAG).assertIsDisplayed()
        rule.onNodeWithText("Durable workspace edits, newest first").assertIsDisplayed()
        assertTrue(rule.onAllNodesWithTextSafe("app/src/main/java/NavHost.kt") > 0)
        // A kind this client does not know is shown as the daemon wrote it and
        // marked, rather than dropped or folded into a neighbour.
        assertTrue(rule.onAllNodesWithTextSafe("rename (this app does not know that kind)") > 0)
    }

    @Test
    fun `an omitted pre-image says so on the path it belongs to`() {
        open()
        assertTrue(
            rule.onAllNodesWithTextSafe(
                "app/src/main/assets/dump.bin — no pre-image was frozen: " +
                    "pre-image exceeds 8388608 bytes",
            ) > 0,
        )
    }

    @Test
    fun `undo asks before it restores anything`() {
        val viewModel = open()
        rule.onAllNodes(androidx.compose.ui.test.hasText("Undo")).onFirst()
            .performScrollTo().performClick()
        rule.waitForIdle()

        rule.onNodeWithTag(CHECKPOINTS_CONFIRM_TAG).assertIsDisplayed()
        rule.onNodeWithText(
            "Undo this edit? The files it changed go back to the bytes it froze.",
        ).assertIsDisplayed()
        // Still nothing done: the confirm is a real gate, not a label.
        assertEquals(null, viewModel.state.value.checkpoints.receipt)

        rule.onNodeWithText("Yes, do it").performScrollTo().performClick()
        rule.waitForIdle()
        assertTrue(viewModel.state.value.checkpoints.receipt != null)
    }

    @Test
    fun `a rollback confirm names the whole turn, not one edit`() {
        open()
        rule.onAllNodes(androidx.compose.ui.test.hasText("Roll back this turn")).onFirst()
            .performScrollTo().performClick()
        rule.waitForIdle()
        rule.onNodeWithText(
            "Roll back this whole turn? Every durable edit the run made is restored " +
                "at once, or none of them is.",
        ).assertIsDisplayed()
    }

    @Test
    fun `a daemon without the feature shows no timeline and no buttons`() {
        val service = ComposeHost.install(FakeScenario.Populated).apply {
            checkpointsUnavailable = true
        }
        val viewModel = rule.setHaiderApp(service)
        viewModel.openCheckpoints("s-nav")
        rule.waitForIdle()

        rule.onNodeWithText("This daemon does not keep a checkpoint timeline.").assertIsDisplayed()
        // No affordance for a command the daemon does not serve.
        assertEquals(0, rule.onAllNodesWithTextSafe("Undo"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Roll back this turn"))
    }

    @Test
    fun `the branch sheet draws main itself and switches only the next turn`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Branches("s-nav"))
        rule.waitForIdle()

        rule.onNodeWithTag(BRANCHES_SHEET_TAG).assertIsDisplayed()
        rule.onNodeWithText("The branch you pick here carries on the next message you send.")
            .assertIsDisplayed()
        // Main is implicit on the wire and is drawn by this client.
        rule.onNodeWithContentDescription("Main, In use").assertIsDisplayed()
        rule.onNodeWithContentDescription("Plan B").performClick()
        rule.waitForIdle()

        assertEquals("branch-plan-b", viewModel.state.value.branchSelection["s-nav"])
        assertTrue(service.calls.contains("branch.select:s-nav:branch-plan-b"))
    }

    @Test
    fun `creating a branch states where it forks and sends both coordinates`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Branches("s-nav"))
        rule.waitForIdle()

        // A checkpoint is not a fork point on this wire, and the sheet says so
        // rather than offering one it cannot address.
        rule.onNodeWithText(
            "A branch forks at an exact committed node. Checkpoints record file " +
                "changes and carry no node, so a new branch starts at this session's head.",
        ).assertIsDisplayed()
        rule.onNodeWithTag(BRANCHES_NAME_FIELD_TAG).performScrollTo().performTextInput("Plan C")
        rule.onNodeWithText("New branch from here").performScrollTo().performClick()
        rule.waitForIdle()

        assertTrue(service.calls.contains("branch.create:s-nav"))
        val created = viewModel.state.value.sessions.first { it.id == "s-nav" }.branches.last()
        assertEquals("Plan C", created.name)
        assertEquals("node-nav-head", created.forkNodeId)
    }

    @Test
    fun `a session with no head node offers no create`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Branches("s-route"))
        rule.waitForIdle()

        rule.onNodeWithText(
            "This session has no published head node, so there is nothing to branch from yet.",
        ).assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTextSafe("New branch from here"))
    }
}
