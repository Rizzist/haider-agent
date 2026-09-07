package ai.diffforge.haider.ui.state

import ai.diffforge.haider.ui.daemon.DaemonEnvironment
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.RosterPaging
import ai.diffforge.haider.ui.daemon.SearchIndexState
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.ui.chat.Message

/**
 * Destinations are sheets, not screens — one sealed overlay instead of the 970
 * boolean soup (UI-SPEC 3.0). Only Settings and Accounts are full screens,
 * because they host flows that leave the app.
 */
sealed interface Overlay {
    data object None : Overlay
    data object ModelPicker : Overlay
    data object Attach : Overlay
    data object DaemonDetails : Overlay
    data object NewSessionWith : Overlay
    data object Settings : Overlay
    data object Accounts : Overlay
    data class SessionActions(val sessionId: String) : Overlay
    data class Rename(val sessionId: String, val current: String) : Overlay
}

/** The four (or three, below SDK 33) first-run steps. */
enum class SetupStepId { RunService, Notifications, Battery, Model }

data class SetupStep(
    val id: SetupStepId,
    val done: Boolean,
    val current: Boolean,
    val skippable: Boolean = false,
)

data class SetupState(
    val steps: List<SetupStep>,
    val complete: Boolean,
) {
    val currentIndex: Int get() = steps.indexOfFirst { it.current }.coerceAtLeast(0)
    val total: Int get() = steps.size
}

object SetupPlan {
    fun build(
        daemonRunning: Boolean,
        notificationsGranted: Boolean,
        notificationsSupported: Boolean,
        batteryRestricted: Boolean,
        batterySkipped: Boolean,
        modelResolved: Boolean,
    ): SetupState {
        val done = buildList {
            add(SetupStepId.RunService to daemonRunning)
            if (notificationsSupported) add(SetupStepId.Notifications to notificationsGranted)
            add(SetupStepId.Battery to (!batteryRestricted || batterySkipped))
            add(SetupStepId.Model to modelResolved)
        }
        val firstPending = done.indexOfFirst { !it.second }
        val steps = done.mapIndexed { index, (id, isDone) ->
            SetupStep(
                id = id,
                done = isDone,
                current = index == firstPending,
                skippable = id == SetupStepId.Battery,
            )
        }
        return SetupState(steps, complete = firstPending == -1)
    }
}

/** One immutable object the whole tree reads from. */
data class AppUiState(
    val daemon: DaemonStatus = DaemonStatus.Stopped,
    val environment: DaemonEnvironment = DaemonEnvironment(),
    val sessions: List<SessionRow> = emptyList(),
    val paging: RosterPaging = RosterPaging(),
    val activeSessionId: String? = null,
    val messages: List<Message> = emptyList(),
    val draft: String = "",
    val models: SessionConfig? = null,
    val catalogError: String? = null,
    val catalogRequestedAtMs: Long? = null,
    val selectionBusy: Boolean = false,
    val overlay: Overlay = Overlay.None,
    val filter: SessionFilter = SessionFilter.All,
    val query: String = "",
    val setup: SetupState = SetupPlan.build(false, false, true, false, false, false),
    val transcriptLoading: Boolean = false,
    /** Set when a replay came back partial or unavailable; never hidden. */
    val transcriptNotice: String? = null,
    val searchIndex: SearchIndexState = SearchIndexState(),
    val answeredElsewhere: Set<String> = emptySet(),
) {
    val activeSession: SessionRow?
        get() = sessions.firstOrNull { it.id == activeSessionId }

    val turnRunning: Boolean
        get() = activeSession?.runId != null || messages.any { it.streaming }

    val needsInputHere: Boolean
        get() = activeSession?.needsInput != null

    /** Rank-3 banner input: another session, never the visible one. */
    val needsInputElsewhereRow: SessionRow?
        get() = sessions.firstOrNull { it.id != activeSessionId && it.needsInput != null }

    /** The drawer button's badge (UI-SPEC 3.1). */
    val attentionBadge: AttentionBadgeKind
        get() {
            val others = sessions.filter { it.id != activeSessionId }
            return when {
                others.any { it.needsInput != null } -> AttentionBadgeKind.NeedsInput
                others.any { it.state == SessionVisualState.Running } -> AttentionBadgeKind.Running
                others.any { it.state == SessionVisualState.Errored } -> AttentionBadgeKind.Errored
                else -> AttentionBadgeKind.None
            }
        }

    val attentionCount: Int
        get() {
            val others = sessions.filter { it.id != activeSessionId }
            return when (attentionBadge) {
                AttentionBadgeKind.NeedsInput -> others.count { it.needsInput != null }
                AttentionBadgeKind.Running -> others.count { it.state == SessionVisualState.Running }
                AttentionBadgeKind.Errored -> others.count { it.state == SessionVisualState.Errored }
                AttentionBadgeKind.None -> 0
            }
        }
}

enum class AttentionBadgeKind { None, NeedsInput, Running, Errored }
