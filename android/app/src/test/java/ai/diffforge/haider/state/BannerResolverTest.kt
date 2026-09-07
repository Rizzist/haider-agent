package ai.diffforge.haider.state

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.DaemonInfo
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.NetworkState
import ai.diffforge.haider.ui.state.BannerAction
import ai.diffforge.haider.ui.state.BannerInputs
import ai.diffforge.haider.ui.state.BannerResolver
import ai.diffforge.haider.ui.state.NeedsInputElsewhere
import ai.diffforge.haider.update.AvailableUpdate
import ai.diffforge.haider.update.UpdateUiState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class BannerResolverTest {
    private val running = DaemonStatus.Running(DaemonInfo(version = "0.0.971", generation = 1))
    private val asking = NeedsInputElsewhere("s-2", "Reply to Amir", "Send it?", "2m 14s")

    @Test
    fun `at most one banner is visible and the highest rank wins`() {
        val inputs = BannerInputs(
            daemon = DaemonStatus.Stopped,
            needsInputElsewhere = asking,
            notificationsGranted = false,
            batteryRestricted = true,
            network = NetworkState.Unavailable,
        )
        val candidates = BannerResolver.candidates(inputs)
        assertEquals(listOf(1, 3, 4, 5, 6), candidates.map { it.rank })
        assertEquals(1, BannerResolver.resolve(inputs).model!!.rank)
    }

    @Test
    fun `the ladder walks down as conditions clear`() {
        assertEquals(
            2,
            BannerResolver.resolve(BannerInputs(daemon = DaemonStatus.Starting)).model!!.rank,
        )
        assertEquals(
            3,
            BannerResolver.resolve(
                BannerInputs(daemon = running, needsInputElsewhere = asking),
            ).model!!.rank,
        )
        assertEquals(
            4,
            BannerResolver.resolve(
                BannerInputs(daemon = running, notificationsGranted = false),
            ).model!!.rank,
        )
        assertEquals(
            5,
            BannerResolver.resolve(
                BannerInputs(daemon = running, batteryRestricted = true),
            ).model!!.rank,
        )
        assertEquals(
            6,
            BannerResolver.resolve(
                BannerInputs(daemon = running, network = NetworkState.Unavailable),
            ).model!!.rank,
        )
        assertEquals(
            7,
            BannerResolver.resolve(
                BannerInputs(
                    daemon = running,
                    update = UpdateUiState.Available(AvailableUpdate("v0.0.972", "0.0.972", "apk", "sha")),
                ),
            ).model!!.rank,
        )
    }

    @Test
    fun `a healthy daemon with nothing to say shows no banner`() {
        assertNull(BannerResolver.resolve(BannerInputs(daemon = running)).model)
    }

    @Test
    fun `ranks one to three cannot be dismissed`() {
        listOf(
            BannerInputs(daemon = DaemonStatus.Stopped),
            BannerInputs(daemon = DaemonStatus.Starting),
            BannerInputs(daemon = running, needsInputElsewhere = asking),
        ).forEach { inputs ->
            assertFalse(BannerResolver.resolve(inputs).model!!.dismissible)
        }
    }

    @Test
    fun `a dismissal holds for seven days and then lapses`() {
        val inputs = BannerInputs(daemon = running, notificationsGranted = false)
        val dismissedAt = 1_000_000L
        assertNull(
            BannerResolver.resolve(inputs, mapOf(4 to dismissedAt), dismissedAt + 1_000).model,
        )
        assertNotNull(
            BannerResolver.resolve(
                inputs,
                mapOf(4 to dismissedAt),
                dismissedAt + BannerResolver.DISMISS_WINDOW_MS,
            ).model,
        )
    }

    @Test
    fun `a dismissal is reset by the state change that made it irrelevant`() {
        val granted = BannerInputs(daemon = running, notificationsGranted = true)
        val resolution = BannerResolver.resolve(granted, mapOf(4 to 1_000L), 2_000L)
        assertTrue(4 in resolution.staleDismissals)
        // Nothing else was dismissed, so nothing else is cleared.
        assertEquals(setOf(4), resolution.staleDismissals)
    }

    @Test
    fun `a failed daemon names the reason and offers daemon details`() {
        val model = BannerResolver.resolve(
            BannerInputs(daemon = DaemonStatus.Failed("store_recovery_failed", "STORE")),
        ).model!!
        assertEquals(R.string.daemon_failed, model.detail!!.resId)
        assertEquals(listOf<Any>("store_recovery_failed"), model.detail.args)
        assertEquals(BannerAction.OpenDaemonDetails, model.secondaryAction)
        assertTrue(model.filledAction)
    }

    @Test
    fun `starting draws the progress line`() {
        assertTrue(BannerResolver.resolve(BannerInputs(daemon = DaemonStatus.Starting)).model!!.progress)
    }

    @Test
    fun `offline is informational, not an error`() {
        val model = BannerResolver.resolve(
            BannerInputs(daemon = running, network = NetworkState.Unavailable),
        ).model!!
        assertEquals(ai.diffforge.haider.ui.state.BannerSeverity.Info, model.severity)
        assertNull(model.action)
    }

    @Test
    fun `first run shows no daemon banner - the checklist is the message`() {
        val inputs = BannerInputs(daemon = DaemonStatus.Stopped, firstRun = true)
        assertNull(BannerResolver.resolve(inputs).model)
        assertNull(
            BannerResolver.resolve(
                BannerInputs(daemon = DaemonStatus.Starting, firstRun = true),
            ).model,
        )
        // The notification and battery banners are checklist steps too.
        assertNull(
            BannerResolver.resolve(
                BannerInputs(
                    daemon = DaemonStatus.Stopped,
                    notificationsGranted = false,
                    batteryRestricted = true,
                    firstRun = true,
                ),
            ).model,
        )
        // What is not on the checklist still speaks.
        assertEquals(
            6,
            BannerResolver.resolve(
                BannerInputs(
                    daemon = DaemonStatus.Stopped,
                    network = NetworkState.Unavailable,
                    firstRun = true,
                ),
            ).model!!.rank,
        )
    }

    @Test
    fun `notifications are not nagged about below SDK 33`() {
        assertNull(
            BannerResolver.resolve(
                BannerInputs(
                    daemon = running,
                    notificationsGranted = false,
                    notificationsSupported = false,
                ),
            ).model,
        )
    }
}
