package ai.diffforge.haider.ui.state

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.NetworkState
import ai.diffforge.haider.ui.chat.UpdateBannerModel
import ai.diffforge.haider.update.UpdateUiState

/** A string the banner will render: a resource plus args, or a literal. */
data class BannerText(
    val resId: Int? = null,
    val args: List<Any> = emptyList(),
    val literal: String? = null,
) {
    companion object {
        fun of(resId: Int, vararg args: Any) = BannerText(resId = resId, args = args.toList())
        fun literal(value: String) = BannerText(literal = value)
    }
}

enum class BannerSeverity { Info, Warning, Error, Accent }

enum class BannerAction {
    StartDaemon,
    OpenNeedsInput,
    RequestNotifications,
    OpenAppSettings,
    OpenBatterySettings,
    OpenDaemonDetails,
    ContinueUpdate,
}

data class BannerModel(
    val rank: Int,
    val severity: BannerSeverity,
    val title: BannerText,
    val detail: BannerText?,
    /**
     * Text pinned to the end of the strip. The title ellipsises when the
     * session name is long; the suffix is the part that must survive, so it is
     * its own node rather than the tail of one string (addition F, S2).
     */
    val suffix: BannerText? = null,
    val action: BannerAction? = null,
    /**
     * What tapping the strip does. The one-line strip has no button to print it
     * on, so it is spoken rather than drawn — TalkBack still hears "Start
     * Haider" where a sighted user reads a chevron (addition F, S2).
     */
    val actionLabel: BannerText? = null,
    val dismissible: Boolean = false,
    /** Rank 2 draws a 2 dp indeterminate progress line under the banner. */
    val progress: Boolean = false,
)

/** Which other session is asking for a human, if any. */
data class NeedsInputElsewhere(
    val sessionId: String,
    val sessionTitle: String,
    val prompt: String,
    val waiting: String,
)

data class BannerInputs(
    val daemon: DaemonStatus,
    val needsInputElsewhere: NeedsInputElsewhere? = null,
    val notificationsGranted: Boolean = true,
    val notificationsSupported: Boolean = true,
    val notificationsPermanentlyDenied: Boolean = false,
    val batteryRestricted: Boolean = false,
    val network: NetworkState = NetworkState.Available,
    val update: UpdateUiState = UpdateUiState.Hidden,
    /**
     * During first-run setup the checklist *is* the message (UI-SPEC 3.8): the
     * daemon, notification and battery banners all repeat a step the user is
     * already looking at, so ranks 1, 2, 4 and 5 stay quiet. Ranks 3, 6 and 7
     * still speak, because none of them is on the checklist.
     */
    val firstRun: Boolean = false,
)

data class BannerResolution(
    val model: BannerModel?,
    /**
     * Dismissed ranks whose condition no longer holds. A dismissal is reset by
     * the relevant state change, so granting notifications and then losing them
     * again shows the banner again (UI-SPEC 3.2).
     */
    val staleDismissals: Set<Int> = emptySet(),
)

/**
 * The severity ladder (UI-SPEC 3.2): at most one banner is visible, and the
 * highest-ranked live condition wins. Ranks 1-3 cannot be dismissed; 4-7 can,
 * and a dismissal is remembered for seven days.
 */
object BannerResolver {
    const val DISMISS_WINDOW_MS: Long = 7L * 24 * 60 * 60 * 1000

    fun resolve(
        inputs: BannerInputs,
        dismissals: Map<Int, Long> = emptyMap(),
        nowMs: Long = 0L,
    ): BannerResolution {
        val live = candidates(inputs)
        val liveRanks = live.map { it.rank }.toSet()
        val stale = dismissals.keys.filter { it !in liveRanks }.toSet()
        val model = live.firstOrNull { candidate ->
            if (!candidate.dismissible) return@firstOrNull true
            val dismissedAt = dismissals[candidate.rank] ?: return@firstOrNull true
            nowMs - dismissedAt >= DISMISS_WINDOW_MS
        }
        return BannerResolution(model, stale)
    }

    /** Every live condition, highest severity first. Exposed for the tests. */
    fun candidates(inputs: BannerInputs): List<BannerModel> {
        val out = mutableListOf<BannerModel>()

        when (val daemon = if (inputs.firstRun) DaemonStatus.Stopped else inputs.daemon) {
            is DaemonStatus.Failed -> if (!inputs.firstRun) out += BannerModel(
                rank = 1,
                severity = BannerSeverity.Error,
                title = BannerText.of(R.string.banner_stopped_title),
                detail = BannerText.of(R.string.daemon_failed, daemon.reason),
                action = BannerAction.StartDaemon,
                actionLabel = BannerText.of(R.string.banner_stopped_action),
            )
            DaemonStatus.Stopped -> if (!inputs.firstRun) out += BannerModel(
                rank = 1,
                severity = BannerSeverity.Error,
                title = BannerText.of(R.string.banner_stopped_title),
                detail = BannerText.of(R.string.banner_stopped_body),
                action = BannerAction.StartDaemon,
                actionLabel = BannerText.of(R.string.banner_stopped_action),
            )
            DaemonStatus.Starting, DaemonStatus.Restarting, DaemonStatus.Stopping ->
                if (!inputs.firstRun) out += BannerModel(
                rank = 2,
                severity = BannerSeverity.Warning,
                title = BannerText.of(R.string.banner_starting_title),
                detail = BannerText.of(R.string.banner_starting_body),
                progress = true,
            )
            is DaemonStatus.Running -> Unit
        }

        inputs.needsInputElsewhere?.let { asking ->
            out += BannerModel(
                rank = 3,
                severity = BannerSeverity.Accent,
                title = BannerText.of(R.string.banner_needs_you_title, asking.sessionTitle),
                detail = BannerText.of(R.string.banner_needs_you_body, asking.waiting, asking.prompt),
                suffix = BannerText.of(R.string.banner_needs_you_suffix),
                action = BannerAction.OpenNeedsInput,
                actionLabel = BannerText.of(R.string.banner_needs_you_action),
            )
        }

        // Ranks 4 and 5 are setup steps 2 and 3: during first run the checklist
        // already asks for them, and a banner repeating it is noise.
        if (inputs.notificationsSupported && !inputs.notificationsGranted && !inputs.firstRun) {
            out += BannerModel(
                rank = 4,
                severity = BannerSeverity.Warning,
                title = BannerText.of(R.string.banner_notify_title),
                detail = if (inputs.notificationsPermanentlyDenied) {
                    BannerText.of(R.string.banner_notify_body_blocked)
                } else {
                    BannerText.of(R.string.banner_notify_body)
                },
                action = if (inputs.notificationsPermanentlyDenied) {
                    BannerAction.OpenAppSettings
                } else {
                    BannerAction.RequestNotifications
                },
                actionLabel = BannerText.of(
                    if (inputs.notificationsPermanentlyDenied) {
                        R.string.banner_notify_action_settings
                    } else {
                        R.string.banner_notify_action
                    },
                ),
                dismissible = true,
            )
        }

        if (inputs.batteryRestricted && !inputs.firstRun) {
            out += BannerModel(
                rank = 5,
                severity = BannerSeverity.Warning,
                title = BannerText.of(R.string.banner_battery_title),
                detail = BannerText.of(R.string.banner_battery_body),
                action = BannerAction.OpenBatterySettings,
                actionLabel = BannerText.of(R.string.banner_battery_action),
                dismissible = true,
            )
        }

        if (inputs.network == NetworkState.Unavailable) {
            // Informational, not an error: with an embedded daemon, offline
            // breaks provider calls only. Say that, don't red-flag it.
            out += BannerModel(
                rank = 6,
                severity = BannerSeverity.Info,
                title = BannerText.of(R.string.banner_offline_title),
                detail = BannerText.of(R.string.banner_offline_body),
                dismissible = true,
            )
        }

        UpdateBannerModel.from(inputs.update)?.let(out::add)

        return out.sortedBy { it.rank }
    }

}

/** Persistence seam for banner dismissals, so the resolver stays pure. */
interface BannerDismissals {
    fun snapshot(): Map<Int, Long>
    fun dismiss(rank: Int, atMs: Long)
    fun clear(ranks: Set<Int>)
}
