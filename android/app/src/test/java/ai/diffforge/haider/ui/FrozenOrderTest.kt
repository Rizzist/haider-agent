package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithContentDescription
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
 * Rule 6, for the whole open lifecycle.
 *
 * The freeze used to cover only the rows present when the drawer opened.
 * Anything appended afterwards stayed outside the snapshot, so every later
 * delta re-sorted the appended rows among themselves — two rows added moments
 * apart swapped as soon as one of them saw activity. A list that reorders under
 * a thumb steals the tap that was already on its way.
 *
 * A row is ranked once, when it is first displayed, and keeps that rank until
 * the drawer closes.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class FrozenOrderTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun row(seed: SessionRow, id: String, at: Long, running: Boolean = false) = seed.copy(
        id = id,
        title = id,
        needsInput = null,
        runId = if (running) "run-$id" else null,
        runState = if (running) "running" else "idle",
        state = if (running) SessionVisualState.Running else SessionVisualState.Idle,
        lastActivityMs = at,
        seenAtMs = 1000,
    )

    private fun openDrawer() {
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
    }

    private fun top(id: String): Float =
        rule.onNodeWithText(id).fetchSemanticsNode().boundsInRoot.top

    private fun rendered(ids: List<String>): List<String> =
        ids.sortedBy { top(it) }

    @Test
    fun `an appended row does not move under a later activity delta`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val seed = service.sessions.value.first()
        val a = row(seed, "initial-row", 10)
        val c = row(seed, "appended-c", 2)
        val d = row(seed, "appended-d", 1)
        service.setSessions(listOf(a))
        rule.setHaiderApp(service)
        rule.waitForIdle()
        openDrawer()

        rule.runOnIdle { service.setSessions(listOf(a, c, d)) }
        rule.waitForIdle()
        val beforeC = top("appended-c")
        val beforeD = top("appended-d")
        assertTrue("fixture starts with c above d", beforeC < beforeD)

        // `d` becomes the most recent. It must not climb past `c`.
        rule.runOnIdle { service.setSessions(listOf(a, c, d.copy(lastActivityMs = 3))) }
        rule.waitForIdle()
        assertEquals("an appended row must not move", beforeC, top("appended-c"), 0.1f)
        assertEquals(beforeD, top("appended-d"), 0.1f)
    }

    @Test
    fun `several appends keep arriving at the end, in arrival order`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val seed = service.sessions.value.first()
        val a = row(seed, "initial-row", 10)
        val b = row(seed, "second-row", 9)
        service.setSessions(listOf(a, b))
        rule.setHaiderApp(service)
        rule.waitForIdle()
        openDrawer()

        // Each arrives later but is more recent than everything before it —
        // canonical order would put each new one at the top.
        val c = row(seed, "third-row", 100)
        rule.runOnIdle { service.setSessions(listOf(a, b, c)) }
        rule.waitForIdle()
        val d = row(seed, "fourth-row", 200)
        rule.runOnIdle { service.setSessions(listOf(a, b, c, d)) }
        rule.waitForIdle()

        assertEquals(
            listOf("initial-row", "second-row", "third-row", "fourth-row"),
            rendered(listOf("initial-row", "second-row", "third-row", "fourth-row")),
        )
    }

    @Test
    fun `a state change updates in place without moving the row`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val seed = service.sessions.value.first()
        val a = row(seed, "row-a", 10)
        val b = row(seed, "row-b", 9)
        val c = row(seed, "row-c", 8)
        service.setSessions(listOf(a, b, c))
        rule.setHaiderApp(service)
        rule.waitForIdle()
        openDrawer()
        val before = listOf("row-a", "row-b", "row-c").associateWith { top(it) }

        // The last row starts running: a promotion in canonical terms, and a
        // group change too. Neither may move it.
        rule.runOnIdle {
            service.setSessions(listOf(a, b, row(seed, "row-c", 500, running = true)))
        }
        rule.waitForIdle()

        before.forEach { (id, y) ->
            assertEquals("$id moved while the drawer was open", y, top(id), 0.1f)
        }
        // The change shows in the glyph and in what the row speaks — the badge
        // is gone (addition E).
        assertTrue(
            rule.onAllNodes(hasContentDescription("Running", substring = true))
                .fetchSemanticsNodes().isNotEmpty(),
        )
    }

    @Test
    fun `appended rows are ranked once, not re-ranked on every delta`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val seed = service.sessions.value.first()
        val a = row(seed, "anchor", 10)
        service.setSessions(listOf(a))
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        openDrawer()

        val c = row(seed, "later-c", 1)
        val d = row(seed, "later-d", 2)
        rule.runOnIdle { service.setSessions(listOf(a, c, d)) }
        rule.waitForIdle()
        val ranked = viewModel.state.value.orderSnapshot!!.ids

        repeat(3) { index ->
            rule.runOnIdle {
                service.setSessions(listOf(a, c.copy(lastActivityMs = 900L + index), d))
            }
            rule.waitForIdle()
        }
        assertEquals(
            "the frozen ranks must survive repeated deltas",
            ranked,
            viewModel.state.value.orderSnapshot!!.ids,
        )
    }

    @Test
    fun `closing the drawer releases the freeze and the next open re-sorts`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val seed = service.sessions.value.first()
        val a = row(seed, "row-a", 10)
        val b = row(seed, "row-b", 9)
        service.setSessions(listOf(a, b))
        val viewModel = rule.setHaiderApp(service)
        rule.waitForIdle()
        openDrawer()
        rule.runOnIdle { service.setSessions(listOf(a, b.copy(lastActivityMs = 100))) }
        rule.waitForIdle()
        assertTrue(top("row-a") < top("row-b"))

        rule.onNodeWithContentDescription("Close sessions").performClick()
        rule.waitForIdle()
        assertEquals(null, viewModel.state.value.orderSnapshot)

        openDrawer()
        // `row-b` is genuinely the most recent now, and a fresh open says so.
        assertTrue("a new open must re-sort", top("row-b") < top("row-a"))
    }
}
