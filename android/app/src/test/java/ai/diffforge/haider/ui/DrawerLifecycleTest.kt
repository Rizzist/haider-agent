package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * The drawer had no open lifecycle at all in round 1: it stayed composed while
 * closed, so it never re-read the roster, and its "frozen" order was a
 * `remember` key that thawed whenever the key happened to change.
 *
 * `session_roster_delta` never reports removals, so an open must re-read
 * `session.list` — otherwise a deleted session stays on screen indefinitely.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class DrawerLifecycleTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun open() {
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
    }

    @Test
    fun `opening the drawer re-reads the authoritative roster`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        assertEquals(0, service.calls.count { it == "refreshRoster" })
        open()
        assertTrue(
            "an open must re-read session.list: deltas never report removals",
            service.calls.contains("refreshRoster"),
        )
    }

    @Test
    fun `closing and reopening re-reads again`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        rule.setHaiderApp(service)
        open()
        rule.onNodeWithContentDescription("Close sessions").performClick()
        rule.waitForIdle()
        open()
        assertTrue(service.calls.count { it == "refreshRoster" } >= 2)
    }

    @Test
    fun `the order is captured on open and released on close`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        assertEquals(null, viewModel.state.value.orderSnapshot)
        open()
        val snapshot = viewModel.state.value.orderSnapshot
        assertTrue("the rendered order must be frozen while open", snapshot != null)
        assertEquals(service.sessions.value.size, snapshot!!.ids.size)
        rule.onNodeWithContentDescription("Close sessions").performClick()
        rule.waitForIdle()
        assertEquals(
            "the freeze must lift, or the next open shows a stale order",
            null,
            viewModel.state.value.orderSnapshot,
        )
    }

    @Test
    fun `the frozen snapshot records the group each row was drawn under`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        open()
        val snapshot = viewModel.state.value.orderSnapshot!!
        assertEquals(snapshot.ids.size, snapshot.groups.size)
    }
}
