package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.chat.SHELL_INPUT_TAG
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.ShellAvailability
import ai.diffforge.haider.ui.daemon.ShellExecutionRef
import ai.diffforge.haider.ui.daemon.ShellExecutionStatus
import ai.diffforge.haider.ui.daemon.ShellOutputStream
import ai.diffforge.haider.ui.state.SessionViewTab
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The Shell tab's terminal against FACADE-SHELL.md, driven through the fake's
 * deterministic shell controls: one minted submission id per user submission,
 * seq-ordered interleaved output, cancel with retained coordinates, honest
 * exit/error/reconnect/truncation presentation, and no auto-resubmission —
 * ever (lane 972-android-shell).
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class ShellTerminalTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    /** The populated fixture's selected session. */
    private val session = "s-nav"

    private fun openShell(service: FakeDaemonService): ChatViewModel {
        val viewModel = rule.setHaiderApp(service)
        service.setShell(ShellAvailability(available = true, sessionId = session))
        viewModel.selectViewTab(SessionViewTab.Shell)
        rule.waitForIdle()
        return viewModel
    }

    private fun run(command: String) {
        rule.onNodeWithTag(SHELL_INPUT_TAG).performTextInput(command)
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Run this command").performClick()
        rule.waitForIdle()
    }

    private fun history(service: FakeDaemonService) =
        service.shellExecutions.value[session].orEmpty()

    private fun onlyRef(service: FakeDaemonService): ShellExecutionRef =
        history(service).single().ref

    // ---------- submission identity ----------

    @Test
    fun `one submission creates one execution carrying one minted id`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("echo one")
        val only = history(service).single()
        assertTrue(only.ref.commandId.isNotBlank())
        assertEquals("echo one", only.command)
        assertEquals(ShellExecutionStatus.Running, only.status)
        rule.onNodeWithText("$ echo one").assertIsDisplayed()
        assertTrue(rule.onAllNodesWithTextSafe("running") > 0)
        // Exactly one shell.exec left the UI.
        assertEquals(1, service.calls.count { it.startsWith("shell.exec:") })
    }

    @Test
    fun `a second submission mints a fresh id`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("echo one")
        service.setShellResult(onlyRef(service), ShellExecutionStatus.Completed, exitCode = 0)
        rule.waitForIdle()
        run("echo two")
        val ids = history(service).map { it.ref.commandId }
        assertEquals(2, ids.size)
        assertTrue(ids.all { it.isNotBlank() })
        assertNotEquals(ids[0], ids[1])
    }

    // ---------- output ----------

    @Test
    fun `output renders in durable seq order with stderr interleaved`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("./build.sh")
        val ref = onlyRef(service)
        service.appendShellOutput(ref, "compiling\n")
        service.appendShellOutput(ref, "warning: unused import\n", ShellOutputStream.Stderr)
        service.appendShellOutput(ref, "done")
        rule.waitForIdle()
        // One text run per execution: the interleaving is the string itself.
        rule.onNodeWithText("compiling\nwarning: unused import\ndone", substring = true)
            .assertIsDisplayed()
    }

    @Test
    fun `the truncation marker is surfaced with the retained prefix`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("cat big.log")
        val ref = onlyRef(service)
        service.appendShellOutput(ref, "retained prefix")
        service.setShellTruncated(ref)
        rule.waitForIdle()
        rule.onNodeWithText("retained prefix", substring = true).assertIsDisplayed()
        rule.onNodeWithText("Output truncated — showing the first 256 KiB.").assertIsDisplayed()
    }

    // ---------- terminal statuses ----------

    @Test
    fun `cancel goes through the retained coordinates and lands as cancelled`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("sleep 100")
        val ref = onlyRef(service)
        rule.onNodeWithContentDescription("Stop this command").performClick()
        rule.waitForIdle()
        assertTrue(service.calls.contains("shell.cancel:${ref.commandId}"))
        assertTrue(rule.onAllNodesWithTextSafe("cancelled") > 0)
        // Terminal means no Stop affordance is left.
        assertEquals(
            0,
            rule.onAllNodes(hasContentDescription("Stop this command"))
                .fetchSemanticsNodes().size,
        )
    }

    @Test
    fun `a nonzero exit code is shown as the authority it is`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("false")
        service.setShellResult(onlyRef(service), ShellExecutionStatus.Completed, exitCode = 2)
        rule.waitForIdle()
        rule.onNodeWithText("exit 2").assertIsDisplayed()
    }

    @Test
    fun `an errored run names the daemon's own code`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("startx")
        service.setShellResult(onlyRef(service), ShellExecutionStatus.Error, error = "spawn_failed")
        rule.waitForIdle()
        rule.onNodeWithText("error — spawn_failed").assertIsDisplayed()
    }

    // ---------- reconnect ----------

    @Test
    fun `reconnecting is replay catch-up, never a resubmission`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("tail -f app.log")
        val ref = onlyRef(service)
        service.appendShellOutput(ref, "line one\n")
        service.setShellResult(ref, ShellExecutionStatus.Reconnecting)
        rule.waitForIdle()
        rule.onNodeWithText("Reconnecting — replaying output…").assertIsDisplayed()
        // Cached history stays on screen while replay catches up.
        rule.onNodeWithText("line one", substring = true).assertIsDisplayed()
        // Nothing was resubmitted: one execution, one shell.exec, and Run is
        // held while the session has a nonterminal command.
        assertEquals(1, history(service).size)
        assertEquals(1, service.calls.count { it.startsWith("shell.exec:") })
        rule.onNodeWithTag(SHELL_INPUT_TAG).performTextInput("echo blocked")
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Run this command").assertIsNotEnabled()
    }

    // ---------- uncertain response loss ----------

    @Test
    fun `a lost response retains the submission and Retry reuses its exact id`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = openShell(service)
        service.nextShellFailure = java.io.IOException("connection_lost")
        run("make deploy")
        // Nothing reached the fake daemon; the submission is parked, id intact.
        assertEquals(0, history(service).size)
        rule.onNodeWithText("The daemon may not have received this command.").assertIsDisplayed()
        val retained = viewModel.state.value.shellPending
        val mintedId = retained!!.submissionId
        assertEquals("make deploy", retained.command)
        rule.onNodeWithText("Retry").performClick()
        rule.waitForIdle()
        val only = history(service).single()
        assertEquals(mintedId, only.ref.commandId)
        assertNull(viewModel.state.value.shellPending)
    }

    @Test
    fun `Discard abandons the uncertain submission and its id is never reused`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = openShell(service)
        service.nextShellFailure = java.io.IOException("connection_lost")
        run("make deploy")
        val abandoned = viewModel.state.value.shellPending!!.submissionId
        rule.onNodeWithText("Discard").performClick()
        rule.waitForIdle()
        assertNull(viewModel.state.value.shellPending)
        assertEquals(0, history(service).size)
        // The command line kept the text; running again is a NEW submission.
        rule.onNodeWithContentDescription("Run this command").performClick()
        rule.waitForIdle()
        assertNotEquals(abandoned, onlyRef(service).commandId)
    }

    @Test
    fun `a definite daemon refusal shows the code and parks nothing`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = openShell(service)
        // The fake refuses with the availability reason at submit time.
        service.setShell(ShellAvailability(available = false, reason = "session_busy"))
        rule.waitForIdle()
        // Force the submit path despite the closed door: the daemon's own
        // refusal must render, not an invented sentence — and no retry state.
        viewModel.setShellDraft("echo refused")
        viewModel.runShellCommand()
        rule.waitForIdle()
        assertNull(viewModel.state.value.shellPending)
        assertEquals("session_busy", viewModel.state.value.shellNotice)
        assertEquals(0, history(service).size)
    }

    // ---------- per-session history ----------

    @Test
    fun `history follows the selected session and comes back with it`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = openShell(service)
        run("echo nav")
        service.setShellResult(onlyRef(service), ShellExecutionStatus.Completed, exitCode = 0)
        rule.waitForIdle()
        viewModel.activate("s-sms")
        rule.waitForIdle()
        assertEquals(0, rule.onAllNodesWithTextSafe("$ echo nav"))
        viewModel.activate(session)
        rule.waitForIdle()
        rule.onNodeWithText("$ echo nav").assertIsDisplayed()
        rule.onNodeWithText("exit 0").assertIsDisplayed()
    }

    // ---------- unavailable ----------

    @Test
    fun `unavailable renders the existing closed door and no command line`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.selectViewTab(SessionViewTab.Shell)
        rule.waitForIdle()
        rule.onNodeWithText("No shell on this device").assertIsDisplayed()
        // The daemon's own code, verbatim.
        rule.onNodeWithText("process_exec_disabled").assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTagSafe(SHELL_INPUT_TAG))
    }

    @Test
    fun `a non-policy denial gets the generic truthful card, never the policy claim`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        service.setShell(ShellAvailability(available = false, reason = "lockdown", sessionId = session))
        viewModel.selectViewTab(SessionViewTab.Shell)
        rule.waitForIdle()
        rule.onNodeWithText("Shell unavailable").assertIsDisplayed()
        // The daemon's own code, verbatim — and no invented policy sentence.
        rule.onNodeWithText("lockdown").assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTextSafe("No shell on this device"))
        assertEquals(0, rule.onAllNodesWithTagSafe(SHELL_INPUT_TAG))
    }

    // ---------- availability is an observation, not a curtain (B1) ----------

    @Test
    fun `session_busy keeps the terminal, its output and a reachable Stop`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("sleep 300")
        val ref = onlyRef(service)
        service.appendShellOutput(ref, "tick\n")
        // The daemon's availability observation flips while the run is live.
        service.setShell(ShellAvailability(available = false, reason = "session_busy", sessionId = session))
        rule.waitForIdle()
        // The live terminal stays: command card, streamed output, running state.
        rule.onNodeWithText("$ sleep 300").assertIsDisplayed()
        rule.onNodeWithText("tick", substring = true).assertIsDisplayed()
        assertTrue(rule.onAllNodesWithTextSafe("running") > 0)
        // Busy is never presented as a policy denial (truthful messaging).
        assertEquals(0, rule.onAllNodesWithTextSafe("No shell on this device"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Shell unavailable"))
        // Stop is reachable inside the Shell tab and cancels with the retained
        // coordinates — no detour through Chat's global Stop.
        rule.onNodeWithContentDescription("Stop this command").performClick()
        rule.waitForIdle()
        assertTrue(service.calls.contains("shell.cancel:${ref.commandId}"))
        assertTrue(rule.onAllNodesWithTextSafe("cancelled") > 0)
    }

    @Test
    fun `disconnected keeps cached history with Reconnecting, input held, no policy claim`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        openShell(service)
        run("tail -f app.log")
        val ref = onlyRef(service)
        service.appendShellOutput(ref, "line one\n")
        // Connection loss: replay is catching up and availability observes it.
        service.setShellResult(ref, ShellExecutionStatus.Reconnecting)
        service.setShell(ShellAvailability(available = false, reason = "disconnected", sessionId = session))
        rule.waitForIdle()
        // Cached redacted history stays visible while replay catches up.
        rule.onNodeWithText("$ tail -f app.log").assertIsDisplayed()
        rule.onNodeWithText("line one", substring = true).assertIsDisplayed()
        rule.onNodeWithText("Reconnecting — replaying output…").assertIsDisplayed()
        // A truthful inline notice with the verbatim reason; input is disabled.
        rule.onNodeWithText("Connection lost — showing cached history. Reconnecting…").assertIsDisplayed()
        rule.onNodeWithText("disconnected").assertIsDisplayed()
        rule.onNodeWithTag(SHELL_INPUT_TAG).assertIsNotEnabled()
        // Never the policy card, and nothing was resubmitted.
        assertEquals(0, rule.onAllNodesWithTextSafe("No shell on this device"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Shell unavailable"))
        assertEquals(1, service.calls.count { it.startsWith("shell.exec:") })
    }
}
