package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.daemon.*
import ai.diffforge.haider.ui.fleet.*
import ai.diffforge.haider.ui.theme.ForgeTheme
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowSystemClock
import java.time.Duration

@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class AstraClockOwnershipTest {
    init { ComposeHost.install() }
    @get:Rule val rule = createAndroidComposeRule<MainActivity>()
    private val instant = 1_800_000_000_000L

    private fun snap(id: String, at: Long, touchedAt: Long = instant): FleetLoad.Snapshot =
        FleetLoad.Snapshot(FleetSnapshot(
            sessionId = id, generatedAtMs = at, nodeLimit = 64, depthLimit = 4,
            roots = listOf(FleetNode(
                agentId = "agent-$id", sessionId = "child-$id", callsign = "member-$id",
                task = "Reading owned session $id", depth = 1, parentSessionId = id,
                state = FleetStateView.of("live"), activity = FleetActivity.Read,
                metrics = FleetNodeMetrics(
                    startedAtMs = instant, live = true,
                    activity = FleetActivityState(
                        touched = mapOf(FleetActivity.Read to touchedAt),
                        latest = FleetActivity.Read to touchedAt,
                    ),
                ),
            )),
            rollup = FleetRollup(nodeCount = 1, maxDepth = 1, metricsComplete = true, complete = true),
            truncated = false,
        ))

    private fun count(id: String, expected: Int) = rule
        .onAllNodesWithTag(fleetActivityTag("agent-$id"), useUnmergedTree = true)
        .assertCountEquals(expected)

    private fun capture(name: String) {
        val folder = System.getenv("HAIDER_EVIDENCE_DIR") ?: "build/outputs/activity-clock"
        java.io.File(folder).mkdirs()
        rule.onRoot().captureRoboImage("$folder/$name.png")
    }

    /** Astra's exact two-session repro, plus fresh-current/history controls. */
    @Test fun unrelated_snapshot_must_not_resurrect_expired_activity() {
        ActivityIconPreference.set(rule.activity, true)
        val panel = mutableStateOf(mapOf("a" to snap("a", instant), "b" to snap("b", instant)))
        val loading = mutableStateOf(false)
        rule.setContent {
            ForgeTheme(dark = true) {
                FleetPanelContent(panel = panel.value, sessions = emptyList(), loading = loading.value,
                    onJump = {}, onOpenChild = { _, _, _ -> }, onRefresh = {})
            }
        }
        rule.waitForIdle()
        count("a", 1)
        count("b", 1)
        rule.runOnIdle { ShadowSystemClock.advanceBy(Duration.ofSeconds(61)); loading.value = true }
        rule.waitForIdle()
        count("a", 0)
        count("b", 0)
        capture("clock-before-other-session")
        rule.runOnIdle { panel.value = panel.value + ("b" to snap("b", instant + 61_000)) }
        rule.waitForIdle()
        capture("clock-after-other-session")
        count("a", 0)
        count("b", 0) // fresh receipt of already-expired history is still expired
        rule.runOnIdle { panel.value = panel.value + ("b" to snap("b", instant + 61_000, instant + 61_000)) }
        rule.waitForIdle()
        count("a", 0)
        count("b", 1) // a genuinely fresh activity snapshot stays visible
    }

    @Test fun retained_snapshot_ages_while_panel_is_closed_and_stays_expired_on_reopen() {
        ActivityIconPreference.set(rule.activity, true)
        val retained = mapOf("a" to snap("a", instant))
        val visible = mutableStateOf(true)
        rule.setContent {
            ForgeTheme(dark = true) {
                if (visible.value) FleetPanelContent(panel = retained, sessions = emptyList(), loading = false,
                    onJump = {}, onOpenChild = { _, _, _ -> }, onRefresh = {})
            }
        }
        rule.waitForIdle()
        count("a", 1)
        rule.runOnIdle { visible.value = false }
        rule.waitForIdle()
        rule.runOnIdle { ShadowSystemClock.advanceBy(Duration.ofSeconds(61)); visible.value = true }
        rule.waitForIdle()
        count("a", 0)
    }

    @Test fun cached_service_snapshot_keeps_receipt_time_when_panel_reopens() {
        ActivityIconPreference.set(rule.activity, true)
        val service = ComposeHost.install(FakeScenario.Fleet)
        val cached = snap("s-fleet", instant)
        service.fleetOverride = { id ->
            if (id == "s-fleet") cached else FleetLoad.Unavailable("fake cached service")
        }
        val model = rule.setHaiderApp(service)
        rule.waitForIdle()
        model.openFleet()
        rule.waitForIdle()
        count("s-fleet", 1)
        rule.runOnIdle {
            model.closeOverlay()
            ShadowSystemClock.advanceBy(Duration.ofSeconds(61))
        }
        rule.waitForIdle()
        model.openFleet()
        rule.waitForIdle()
        capture("clock-cached-service-after-reopen")
        val returned = model.state.value.fleet.panel["s-fleet"] as FleetLoad.Snapshot
        org.junit.Assert.assertEquals(cached.receivedAtElapsedMs, returned.receivedAtElapsedMs)
        count("s-fleet", 0)
    }

    @Test fun unrelated_loading_failure_removal_and_reordering_keep_expired_session_absent() {
        ActivityIconPreference.set(rule.activity, true)
        val a = snap("a", instant)
        val panel = mutableStateOf<Map<String, FleetLoad>>(mapOf("a" to a, "b" to snap("b", instant)))
        val loading = mutableStateOf(false)
        rule.setContent {
            ForgeTheme(dark = true) {
                FleetPanelContent(panel = panel.value, sessions = emptyList(), loading = loading.value,
                    onJump = {}, onOpenChild = { _, _, _ -> }, onRefresh = {})
            }
        }
        rule.waitForIdle()
        rule.runOnIdle { ShadowSystemClock.advanceBy(Duration.ofSeconds(61)); loading.value = true }
        rule.waitForIdle()
        for (other in listOf(FleetLoad.Loading, FleetLoad.Failed("fake failure"), FleetLoad.Unavailable("unsupported"))) {
            rule.runOnIdle { panel.value = linkedMapOf("b" to other, "a" to a) }
            rule.waitForIdle()
            count("a", 0)
        }
        rule.runOnIdle { panel.value = mapOf("a" to a) }
        rule.waitForIdle()
        count("a", 0)
    }

    @Test fun another_session_refresh_does_not_restart_hot_priority_window() {
        ActivityIconPreference.set(rule.activity, true)
        val original = snap("a", instant)
        val node = original.snapshot.roots.single()
        val a = original.copy(snapshot = original.snapshot.copy(roots = listOf(node.copy(
            metrics = node.metrics!!.copy(activity = FleetActivityState(
                touched = mapOf(FleetActivity.Edit to instant, FleetActivity.Read to instant),
                latest = FleetActivity.Read to instant,
            )),
        ))))
        val panel = mutableStateOf<Map<String, FleetLoad>>(mapOf("a" to a, "b" to snap("b", instant)))
        val loading = mutableStateOf(false)
        rule.setContent {
            ForgeTheme(dark = true) {
                FleetPanelContent(panel = panel.value, sessions = emptyList(), loading = loading.value,
                    onJump = {}, onOpenChild = { _, _, _ -> }, onRefresh = {})
            }
        }
        rule.waitForIdle()
        rule.onNodeWithTag(fleetActivityTag("agent-a"), useUnmergedTree = true).assertTextEquals(FleetActivity.Edit.emoji)
        rule.runOnIdle { ShadowSystemClock.advanceBy(Duration.ofMillis(8_001)); loading.value = true }
        rule.waitForIdle()
        rule.onNodeWithTag(fleetActivityTag("agent-a"), useUnmergedTree = true).assertTextEquals(FleetActivity.Read.emoji)
        rule.runOnIdle { panel.value = panel.value + ("b" to snap("b", instant + 8_001)) }
        rule.waitForIdle()
        rule.onNodeWithTag(fleetActivityTag("agent-a"), useUnmergedTree = true).assertTextEquals(FleetActivity.Read.emoji)
    }
}
