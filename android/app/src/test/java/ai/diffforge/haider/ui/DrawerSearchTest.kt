package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.FakeScenario
import androidx.compose.ui.test.assertIsDisplayed
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
 * Round 1's search only filtered the rows already in memory: the repository's
 * `search` was never called, so a phrase that appears in a transcript — and not
 * in any title — found nothing, and the roster's unread pages could not
 * participate at all.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class DrawerSearchTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    private fun openDrawer(scenario: FakeScenario) {
        rule.setHaiderApp(ComposeHost.install(scenario))
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
    }

    @Test
    fun `a query reaches the repository, not just the visible rows`() {
        val service = ComposeHost.install(FakeScenario.LargeRoster)
        val viewModel = rule.setHaiderApp(service)
        viewModel.setQuery("Session task 3")
        rule.waitForIdle()
        assertTrue(
            "the repository search must run over the full roster",
            service.calls.any { it.startsWith("session.read:") },
        )
    }

    @Test
    fun `a transcript-only match is listed even though no metadata matches`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        // This phrase lives in a transcript body and in no title or model name.
        viewModel.setQuery("nav controller pops past")
        rule.waitForIdle()
        val outcome = viewModel.state.value.searchOutcome
        assertTrue("the repository found nothing", outcome != null)
        assertEquals(listOf("s-nav"), outcome!!.hits.map { it.sessionId })
    }

    @Test
    fun `incomplete coverage is stated, not implied`() {
        val service = ComposeHost.install(FakeScenario.LargeRoster)
        val viewModel = rule.setHaiderApp(service)
        rule.onAllNodes(hasContentDescription("Open sessions", substring = true))
            .onFirst()
            .performClick()
        rule.waitForIdle()
        viewModel.setQuery("Session task 3")
        rule.waitForIdle()
        val outcome = viewModel.state.value.searchOutcome!!
        assertTrue("unread pages cannot be searched yet", !outcome.complete)
        assertTrue(
            rule.onAllNodesWithTextSafe(
                "Searching ${outcome.index.indexedSessions} of " +
                    "${outcome.index.totalSessions} sessions — still indexing history.",
            ) > 0,
        )
    }

    @Test
    fun `clearing the query drops the outcome with it`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.setQuery("nav")
        rule.waitForIdle()
        viewModel.setQuery("")
        rule.waitForIdle()
        assertEquals(null, viewModel.state.value.searchOutcome)
    }

    @Test
    fun `the search field is offered once the roster is large`() {
        openDrawer(FakeScenario.LargeRoster)
        rule.onNodeWithTextSafeFirst("Search sessions").assertIsDisplayed()
    }
}

private fun androidx.compose.ui.test.junit4.ComposeTestRule.onNodeWithTextSafeFirst(text: String) =
    onAllNodes(androidx.compose.ui.test.hasText(text)).onFirst()
