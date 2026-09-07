package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.state.SessionGroupKind
import ai.diffforge.haider.ui.state.SessionListState
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onFirst
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Contiguous sections mean a kind can legitimately appear twice — an Active row
 * appended behind a frozen Recent row does exactly that. The section header key
 * was still per *kind*, so LazyColumn threw
 * `IllegalArgumentException: Key "group-Active" was already used`.
 *
 * The pure ordering was correct; only the rendering crashed, which is why the
 * pure tests could not see it.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class RepeatedSectionTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun row(id: String, running: Boolean, seed: SessionRow) = seed.copy(
        id = id,
        title = id,
        needsInput = null,
        runId = if (running) "run-$id" else null,
        runState = if (running) "running" else "idle",
        state = if (running) SessionVisualState.Running else SessionVisualState.Idle,
        lastActivityMs = 1,
        seenAtMs = 2,
    )

    @Test
    fun `an appended active section renders instead of crashing`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val seed = service.sessions.value.first()
        val a = row("a-active", running = true, seed = seed)
        val b = row("b-recent", running = false, seed = seed)
        val c = row("c-new-active", running = true, seed = seed)
        service.setSessions(listOf(a, b))

        rule.setHaiderApp(service)
        rule.waitForIdle()
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()

        // The order is frozen, so the new Active row lands after the frozen
        // Recent row and opens a *second* ACTIVE section.
        rule.runOnIdle { service.setSessions(listOf(a, b, c)) }
        rule.waitForIdle()

        assertTrue(
            "the appended section did not render",
            rule.onAllNodesWithTextSafe("c-new-active") > 0,
        )
    }

    @Test
    fun `the roster really does produce two sections of the same kind`() {
        // Guards the test above: if grouping stopped repeating kinds, the
        // regression it pins would silently stop being exercised.
        val seed = SessionRow(id = "seed")
        val a = row("a-active", running = true, seed = seed)
        val b = row("b-recent", running = false, seed = seed)
        val c = row("c-new-active", running = true, seed = seed)
        val snapshot = SessionListState.OrderSnapshot.capture(listOf(a, b), null)
        val groups = SessionListState.groups(listOf(a, b, c), null, snapshot = snapshot)
        assertEquals(
            listOf(SessionGroupKind.Active, SessionGroupKind.Recent, SessionGroupKind.Active),
            groups.map { it.kind },
        )
        // And the flat order is still the canonical one.
        assertEquals(
            listOf("a-active", "b-recent", "c-new-active"),
            SessionListState.flatten(groups),
        )
    }

    @Test
    fun `every rendered section key is unique`() {
        val seed = SessionRow(id = "seed")
        val rows = (0 until 12).map { index ->
            row("s-$index", running = index % 2 == 0, seed = seed)
        }
        val groups = SessionListState.groups(rows, null)
        val keys = groups.map { "section-${it.kind}-${it.rows.first().id}" }
        assertEquals("section keys must be unique: $keys", keys.size, keys.toSet().size)
    }
}
