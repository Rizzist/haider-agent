package ai.diffforge.haider.ui.state

import ai.diffforge.haider.ui.daemon.DaemonEnvironment
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.ProviderInventory
import ai.diffforge.haider.ui.daemon.RosterPaging
import ai.diffforge.haider.ui.daemon.SearchIndexState
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SearchOutcome
import ai.diffforge.haider.ui.daemon.ShellAvailability
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.daemon.TurnCancel
import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.ui.checkpoints.CheckpointsUiState
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

    /** Provider / model / effort pickers for the composer bar. */
    data class Picker(val kind: ai.diffforge.haider.ui.chat.PickerKind) : Overlay
    data object Settings : Overlay
    data object Accounts : Overlay
    data class SessionActions(val sessionId: String) : Overlay
    data class Rename(val sessionId: String, val current: String) : Overlay

    /** The session's durable workspace timeline, with undo/redo/rollback. */
    data class Checkpoints(val sessionId: String) : Overlay

    /** Which branch the session's next turn is submitted on, and creating one. */
    data class Branches(val sessionId: String) : Overlay
}

/** The four (or three, below SDK 33) first-run steps. */
/**
 * [Autonomy] is the owner's "model work should be automated": one step that
 * asks for every one-time OS popup at once — notifications, SMS, the
 * accessibility service and the screen-capture consent — instead of a card per
 * action later (addition H6).
 */
enum class SetupStepId { RunService, Autonomy, Battery, Model }

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
        smsGranted: Boolean = false,
    ): SetupState {
        val done = buildList {
            add(SetupStepId.RunService to daemonRunning)
            // Only notifications gate the step. SMS, the accessibility
            // service and the capture consent are asked for on the same screen
            // and each reports its own status, but none of them may block
            // setup: addition D says a running daemon opens straight into a
            // session, and a device where the user declined SMS must not be
            // parked on a checklist forever.
            if (notificationsSupported) add(SetupStepId.Autonomy to notificationsGranted)
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
    /** Android-side permission facts the Activity observed (verify-6 O3). */
    val permissions: PermissionSnapshot = PermissionSnapshot(),
    /** What the daemon lets the model do unattended (addition H6). */
    val permissionMode: PermissionMode = PermissionMode.Auto,
    val sessions: List<SessionRow> = emptyList(),
    val paging: RosterPaging = RosterPaging(),
    val activeSessionId: String? = null,
    val messages: List<Message> = emptyList(),
    val draft: String = "",
    val models: SessionConfig? = null,
    val catalogError: String? = null,
    val catalogRequestedAtMs: Long? = null,
    val selectionBusy: Boolean = false,
    /** A selection the daemon refused, with what it would take to retry. */
    val selectionRefusal: SelectionRefusal? = null,
    val overlay: Overlay = Overlay.None,
    val filter: SessionFilter = SessionFilter.All,
    val query: String = "",
    val setup: SetupState = SetupPlan.build(false, false, true, false, false, false),
    val transcriptLoading: Boolean = false,
    /** Set when a replay came back partial or unavailable; never hidden. */
    val transcriptNotice: String? = null,
    val searchIndex: SearchIndexState = SearchIndexState(),
    /** The repository's full-roster search result for [query], when it has one. */
    val searchOutcome: SearchOutcome? = null,
    val searching: Boolean = false,
    val providers: ProviderInventory = ProviderInventory(),
    /** Non-null exactly while the drawer is open, freezing the rendered order. */
    val orderSnapshot: SessionListState.OrderSnapshot? = null,
    /** Chat or Shell, per addition E's segmented switch. */
    val viewTab: SessionViewTab = SessionViewTab.Chat,
    val shell: ShellAvailability = ShellAvailability(),
    val answeredElsewhere: Set<String> = emptySet(),
    /** The checkpoints sheet's state, for the one session it is open on. */
    val checkpoints: CheckpointsUiState = CheckpointsUiState(),
    /**
     * Which branch each session's next `turn.submit` carries. An absent entry
     * is the implicit main branch — never a branch id this client made up.
     */
    val branchSelection: Map<String, String> = emptyMap(),
) {
    val activeSession: SessionRow?
        get() = sessions.firstOrNull { it.id == activeSessionId }

    /**
     * A Stop affordance exists only when the *current snapshot* carries run
     * coordinates. A streaming message left over from a connection drop is not
     * a live run, and offering Stop for it would cancel nothing — or worse,
     * cancel a run that already ended (frame.rs:1716-1730).
     */
    val turnRunning: Boolean
        get() = TurnCancel.coordinates(activeSession) != null

    /** A message still marked streaming while the roster reports no run. */
    val staleStream: Boolean
        get() = !turnRunning && messages.any { it.streaming }

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

/** The session surface's tabs. No Traj on a phone. */
enum class SessionViewTab { Chat, Shell }

/**
 * The daemon refused a model or effort change.
 *
 * Round 4 wrapped both calls in `runCatching` and dropped the result, so a
 * refusal — a required confirmation, a lost connection — looked like a change
 * that had happened. The refusal is held here until a person answers it, and
 * only their answer sets `confirm_new_epoch` (lane 971-3 handoff).
 */
data class SelectionRefusal(
    val code: String,
    val provider: String? = null,
    val model: String? = null,
    val effort: String? = null,
)
