package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.DaemonInfo
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.FleetLoad
import ai.diffforge.haider.ui.daemon.NeedsInput
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.drawer.FAMILY_TOGGLE_TAG
import ai.diffforge.haider.ui.drawer.SessionDrawer
import ai.diffforge.haider.ui.fleet.CHILD_TRANSCRIPT_TAG
import ai.diffforge.haider.ui.fleet.SUBAGENT_STRIP_TAG
import ai.diffforge.haider.ui.fleet.fleetRowTag
import ai.diffforge.haider.ui.fleet.subagentChipTag
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.theme.ForgeTheme
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The delegation surfaces, on screen.
 *
 * The model laws are pinned in `SessionTreeTest` and `FleetWireShapeTest`; what
 * these assert is that the drawer, the header strip, the child screen and the
 * panel actually carry them — including the two a screenshot cannot show: that
 * the child transcript has no way to send anything, and that a chip whose child
 * session the daemon has not published opens the panel instead of a wrong
 * session.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class FleetUiTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    // ---------- the drawer ----------

    private fun row(
        id: String,
        title: String,
        parent: String? = null,
        state: SessionVisualState = SessionVisualState.Idle,
        needsInput: Boolean = false,
        activityMs: Long = FakeDaemonService.FIXED_NOW,
    ) = SessionRow(
        id = id,
        title = title,
        state = state,
        provider = "anthropic",
        model = "claude-sonnet-4-5",
        lastActivityMs = activityMs,
        seenAtMs = activityMs,
        parentSessionId = parent,
        needsInput = if (needsInput) {
            NeedsInput(
                kind = "question",
                title = "Which contract version?",
                menuId = "m-$id",
                requestSeq = 1,
                workerGeneration = 1,
            )
        } else {
            null
        },
    )

    private fun drawer(rows: List<SessionRow>, toggled: Set<String> = emptySet()) {
        rule.setContent {
            ForgeTheme(dark = true) {
                SessionDrawer(
                    state = AppUiState(
                        daemon = DaemonStatus.Running(DaemonInfo(version = "0.0.971", generation = 1)),
                        sessions = rows,
                        activeSessionId = rows.first().id,
                        familyToggles = toggled,
                    ),
                    themeMode = ThemeMode.Dark,
                    appVersion = "0.0.971",
                    onClose = {},
                    onNewSession = {},
                    onNewSessionWith = {},
                    onSelect = {},
                    onRowAction = { _, _ -> },
                    onFilter = {},
                    onQuery = {},
                    onStartDaemon = {},
                    onStopDaemon = {},
                    onOpenDaemonDetails = {},
                    onOpenModel = {},
                    onOpenSettings = {},
                    onThemeMode = {},
                    onLoadMore = {},
                    nowMsProvider = { FakeDaemonService.FIXED_NOW },
                    elapsedRealtimeProvider = { FakeDaemonService.FIXED_UPTIME },
                )
            }
        }
        rule.waitForIdle()
    }

    private val smallFamily = listOf(
        row("p", "Refactor the transport layer", state = SessionVisualState.Running),
        row("c1", "scout", parent = "p"),
        row("c2", "auditor", parent = "p", state = SessionVisualState.NeedsInput, needsInput = true),
    )

    @Test
    fun `a small family renders its children under the parent`() {
        drawer(smallFamily)
        rule.onNodeWithText("Refactor the transport layer", useUnmergedTree = true)
            .assertIsDisplayed()
        rule.onNodeWithText("scout", useUnmergedTree = true).assertIsDisplayed()
        rule.onNodeWithText("auditor", useUnmergedTree = true).assertIsDisplayed()
    }

    @Test
    fun `the parent states its descendant count`() {
        drawer(smallFamily)
        // Two descendants, and the control says which way it will go.
        rule.onNodeWithContentDescription("Hide 2 agents under Refactor the transport layer")
            .assertIsDisplayed()
    }

    @Test
    fun `a folded parent still speaks the strongest child state`() {
        // Colour alone would hide it: the aggregate dot is decorative, and the
        // control's state description is what a screen reader hears.
        val wide = wideFamily()
        drawer(wide)
        val toggle = rule.onNodeWithTag(FAMILY_TOGGLE_TAG).fetchSemanticsNode()
        assertEquals(
            "Needs input",
            toggle.config.getOrNull(SemanticsProperties.StateDescription),
        )
    }

    @Test
    fun `a family over the threshold arrives folded`() {
        drawer(wideFamily())
        rule.onAllNodesWithText("worker 1", useUnmergedTree = true).assertCountEquals(0)
        rule.onNodeWithContentDescription("Show 5 agents under Sweep the dependency tree")
            .assertIsDisplayed()
    }

    @Test
    fun `a toggled family shows its children`() {
        drawer(wideFamily(), toggled = setOf("p"))
        rule.onNodeWithText("worker 1", useUnmergedTree = true).assertIsDisplayed()
        rule.onNodeWithContentDescription("Hide 5 agents under Sweep the dependency tree")
            .assertIsDisplayed()
    }

    private fun wideFamily(): List<SessionRow> =
        listOf(row("p", "Sweep the dependency tree", state = SessionVisualState.Running)) +
            (1..4).map { row("w$it", "worker $it", parent = "p") } +
            listOf(
                row(
                    "w5",
                    "worker 5",
                    parent = "p",
                    state = SessionVisualState.NeedsInput,
                    needsInput = true,
                ),
            )

    @Test
    fun `a childless row draws no family control at all`() {
        drawer(listOf(row("a", "Just a conversation")))
        rule.onAllNodesWithTag(FAMILY_TOGGLE_TAG).assertCountEquals(0)
    }

    // ---------- the header strip ----------

    private lateinit var viewModel: ai.diffforge.haider.ui.chat.ChatViewModel

    /**
     * The strip scrolls horizontally, so a chip past the fold has to be brought
     * into the viewport before it can be tapped — a click aimed at a node whose
     * centre is off-screen lands on nothing and reports no failure.
     */
    private fun chip(agentId: String) =
        rule.onNodeWithTag(subagentChipTag(agentId)).performScrollTo()

    private fun app(scenario: FakeScenario = FakeScenario.Fleet): FakeDaemonService {
        val service = ComposeHost.install(scenario)
        viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        return service
    }

    @Test
    fun `the session header carries a chip per published subagent`() {
        app()
        rule.onNodeWithTag(SUBAGENT_STRIP_TAG).assertIsDisplayed()
        rule.onNodeWithTag(subagentChipTag("agent-scout")).assertIsDisplayed()
        rule.onNodeWithTag(subagentChipTag("agent-auditor")).assertIsDisplayed()
    }

    @Test
    fun `an ordinary session draws no strip`() {
        app(FakeScenario.Populated)
        rule.onAllNodesWithTag(SUBAGENT_STRIP_TAG).assertCountEquals(0)
    }

    @Test
    fun `tapping a chip opens that agent's own transcript`() {
        val service = app()
        chip("agent-auditor").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(CHILD_TRANSCRIPT_TAG).assertIsDisplayed()
        // The child's own session was replayed — not the parent's.
        assertTrue(
            service.calls.toString(),
            service.calls.any { it.contains("s-fleet-b") },
        )
    }

    @Test
    fun `a chip the fleet snapshot never paired opens the panel, not a session`() {
        // `ObserveSubagentWire` carries no session id, and this agent is absent
        // from the bounded snapshot: guessing one would open the wrong session.
        app()
        chip("agent-7f2c91ab4de0").performClick()
        rule.waitForIdle()
        rule.onAllNodesWithTag(CHILD_TRANSCRIPT_TAG).assertCountEquals(0)
        assertEquals(Overlay.Fleet, viewModel.state.value.overlay)
    }

    // ---------- the child transcript ----------

    @Test
    fun `the child transcript is read-only and cannot send`() {
        val service = app()
        chip("agent-auditor").performClick()
        rule.waitForIdle()
        rule.onNodeWithTag(CHILD_TRANSCRIPT_TAG).assertIsDisplayed()
        // No composer, no send, no stop: delegation is the parent's turn.
        rule.onAllNodes(hasContentDescription("Send", substring = true)).assertCountEquals(0)
        rule.onAllNodes(hasContentDescription("Stop", substring = true)).assertCountEquals(0)
        rule.onAllNodes(hasContentDescription("Attach", substring = true)).assertCountEquals(0)
        assertTrue(service.calls.none { it.startsWith("chat.send") })
    }

    @Test
    fun `the child transcript names the parent it came from`() {
        app()
        chip("agent-auditor").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Back to Refactor the transport layer")
            .assertIsDisplayed()
        // Its own history, replayed from its own session.
        rule.onNodeWithText(
            "Both v1 and v2 frames are present. I need to know which one to assume.",
            useUnmergedTree = true,
        ).assertIsDisplayed()
    }

    @Test
    fun `back from the child returns to the parent surface`() {
        app()
        chip("agent-auditor").performClick()
        rule.waitForIdle()
        rule.onNodeWithContentDescription("Back to Refactor the transport layer").performClick()
        rule.waitForIdle()
        rule.onAllNodesWithTag(CHILD_TRANSCRIPT_TAG).assertCountEquals(0)
        rule.onNodeWithTag(SUBAGENT_STRIP_TAG).assertIsDisplayed()
    }

    // ---------- the panel ----------

    @Test
    fun `the panel lists active children across sessions and jumps to one`() {
        val service = app()
        viewModel.openFleet()
        rule.waitForIdle()

        // Live, waiting and queued from two different parents.
        rule.onNodeWithTag(fleetRowTag("agent-scout")).assertExists()
        rule.onNodeWithTag(fleetRowTag("agent-auditor")).assertExists()
        rule.onNodeWithTag(fleetRowTag("agent-worker-1")).assertExists()
        // `scribe` is done, so it is counted rather than listed.
        rule.onAllNodesWithTag(fleetRowTag("agent-scribe")).assertCountEquals(0)

        rule.onNodeWithTag(fleetRowTag("agent-worker-1")).performClick()
        rule.waitForIdle()
        assertTrue(
            service.calls.toString(),
            service.calls.contains("activate:s-swarm-1"),
        )
        assertEquals(Overlay.None, viewModel.state.value.overlay)
    }

    @Test
    fun `the panel says what the daemon bounded and what it refused`() {
        app()
        viewModel.openFleet()
        rule.waitForIdle()
        // assertExists, not assertIsDisplayed: a partially expanded sheet can
        // put these notices below the fold, and what is being pinned is that
        // the panel *states* them — the goldens cover where they sit.
        rule.onNodeWithText(
            "Bounded snapshot — not the complete tree (node limit 64, depth limit 4).",
        ).assertExists()
        rule.onNodeWithText("2 more not shown (bounded).").assertExists()
        rule.onNodeWithText("Session s-broken: session_fleet_v1 unsupported").assertExists()
    }

    @Test
    fun `an unread panel is not an empty one`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.fleetOverride = { FleetLoad.Unread }
        val model = rule.setHaiderApp(service)
        rule.waitForIdle()
        assertNull((model.state.value.fleet.active as? FleetLoad.Snapshot)?.snapshot)
    }
}
