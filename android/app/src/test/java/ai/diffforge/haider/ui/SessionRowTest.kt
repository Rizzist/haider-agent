package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.drawer.SessionRowAction
import ai.diffforge.haider.ui.drawer.SessionRowItem
import ai.diffforge.haider.ui.theme.ForgeTheme
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.semantics.SemanticsNode
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class SessionRowTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private val now = FakeDaemonService.FIXED_NOW

    private fun render(row: SessionRow, selected: Boolean = false): List<String> {
        rule.setContent {
            ForgeTheme(dark = true) {
                SessionRowItem(
                    row = row,
                    selected = selected,
                    nowMs = now,
                    onClick = {},
                    onLongClick = {},
                    onAction = {},
                )
            }
        }
        return rowNode().config.getOrNull(SemanticsProperties.ContentDescription).orEmpty()
    }

    /** The single merged node the row collapses into. */
    private fun rowNode(): SemanticsNode =
        rule.onAllNodes(SemanticsMatcher.keyIsDefined(SemanticsProperties.ContentDescription))
            .fetchSemanticsNodes()
            .first()

    private fun customActions(): List<String> =
        rowNode().config.getOrNull(SemanticsActions.CustomActions).orEmpty().map { it.label }

    @Test
    fun `a running row speaks title, state, model and an expanded time`() {
        val spoken = render(
            SessionRow(
                id = "s-nav",
                title = "Fix nav crash on back gesture",
                state = SessionVisualState.Running,
                model = "claude-sonnet-4-5",
                lastActivityMs = now - 2 * 60_000,
                seenAtMs = now,
                runId = "run-1",
            ),
        ).joinToString(" ")
        assertTrue(spoken.contains("Fix nav crash on back gesture"))
        assertTrue(spoken.contains("Running"))
        assertTrue(spoken.contains("Sonnet 4.5"))
        // "2m" is spoken as "2 minutes ago", never as "2m" and never as "0 minutes".
        assertTrue(spoken.contains("2 minutes ago"))
    }

    @Test
    fun `an unseen row says new activity`() {
        val spoken = render(
            SessionRow(
                id = "s",
                title = "Unseen",
                lastActivityMs = now - 1000,
                seenAtMs = now - 60_000,
            ),
        ).joinToString(" ")
        assertTrue(spoken.contains("new activity"))
    }

    @Test
    fun `a viewed row does not - viewing is the acknowledgement`() {
        val spoken = render(
            SessionRow(id = "s", title = "Seen", lastActivityMs = now - 60_000, seenAtMs = now),
        ).joinToString(" ")
        assertFalse(spoken.contains("new activity"))
    }

    @Test
    fun `an unknown row claims nothing`() {
        val spoken = render(
            SessionRow(
                id = "s-unknown",
                title = "Sweep the download folder",
                state = SessionVisualState.Unknown,
                runState = "effect_unknown",
                lastActivityMs = null,
                seenAtMs = null,
            ),
        ).joinToString(" ")
        assertTrue(spoken.contains("Sweep the download folder"))
        assertFalse(spoken.contains("Running"))
        assertFalse(spoken.contains("Errored"))
        // An unknown stamp is not spoken as an epoch date.
        assertFalse(spoken.contains("1970"))
    }

    @Test
    fun `a row with no title falls back to its id, never to a product name`() {
        val spoken = render(SessionRow(id = "abcdef123456", title = null)).joinToString(" ")
        assertTrue(spoken.contains("Session abcdef"))
    }

    @Test
    fun `custom actions make long-press reachable without a long press`() {
        rule.setContent {
            ForgeTheme(dark = true) {
                SessionRowItem(
                    row = SessionRow(id = "s", title = "Row", runId = "run-1"),
                    selected = false,
                    nowMs = now,
                    onClick = {},
                    onLongClick = {},
                    onAction = {},
                )
            }
        }
        val actions = customActions()
        assertTrue(actions.contains("Rename"))
        assertTrue(actions.contains("Fork session"))
        // Stop turn only when the snapshot carries a run_id.
        assertTrue(actions.contains("Stop turn"))
        // There is no session.delete RPC, so there is no Delete action.
        assertFalse(actions.contains("Delete"))
    }

    @Test
    fun `a row with no run id exposes no stop action`() {
        rule.setContent {
            ForgeTheme(dark = true) {
                SessionRowItem(
                    row = SessionRow(id = "s", title = "Row", runId = null),
                    selected = false,
                    nowMs = now,
                    onClick = {},
                    onLongClick = {},
                    onAction = { _: SessionRowAction -> },
                )
            }
        }
        assertFalse(customActions().contains("Stop turn"))
    }

    @Test
    fun `the row is one merged node, not a pile of leaves`() {
        render(SessionRow(id = "s", title = "Merged", model = "claude-sonnet-4-5"))
        val row = rowNode()
        assertTrue(row.config.contains(SemanticsProperties.ContentDescription))
        assertEquals(0, row.children.size)
    }
}
