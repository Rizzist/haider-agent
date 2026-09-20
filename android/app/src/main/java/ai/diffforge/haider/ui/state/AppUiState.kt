package ai.diffforge.haider.ui.state

import ai.diffforge.haider.ui.daemon.DaemonEnvironment
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.daemon.ProviderInventory
import ai.diffforge.haider.ui.daemon.RosterPaging
import ai.diffforge.haider.ui.daemon.SearchIndexState
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.daemon.SearchOutcome
import ai.diffforge.haider.ui.daemon.ShellAvailability
import ai.diffforge.haider.ui.daemon.ShellExecution
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
    /** What is held behind the running turn (`queue.list`). */
    data object Queue : Overlay
    data object DaemonDetails : Overlay
    data object NewSessionWith : Overlay

    /** Provider / model / effort pickers for the composer bar. */
    data class Picker(val kind: ai.diffforge.haider.ui.chat.PickerKind) : Overlay
    data object Settings : Overlay
    data object Accounts : Overlay
    data class SessionActions(val sessionId: String) : Overlay
    data class Rename(val sessionId: String, val current: String) : Overlay

    /** The cross-session subagent panel (lane 971-UI-fleet). */
    data object Fleet : Overlay

    /**
     * One descendant's own transcript, read-only.
     *
     * [agentId] is the fleet's selection coordinate and [parentSessionId] the
     * lineage the daemon published; both are carried verbatim so the screen can
     * say "unknown" rather than infer either from the session it came from.
     */
    data class ChildTranscript(
        val sessionId: String,
        val agentId: String?,
        val parentSessionId: String?,
    ) : Overlay

    // ---------- lane 971-UI-workflows ----------

    /**
     * The live workflow graph for one session. Three full screens, because each
     * hosts a flow with its own back stack: a graph drills into child sessions,
     * and authoring is a multi-step exchange with the daemon that a sheet
     * dismissed by a stray tap would lose.
     */
    data class WorkflowGraph(val sessionId: String, val graphId: String? = null) : Overlay
    data object Looms : Overlay
    data class LoomAuthoring(
        val kind: ai.diffforge.haider.ui.loom.LoomAuthorKind,
    ) : Overlay
    /** The session's durable workspace timeline, with undo/redo/rollback. */
    data class Checkpoints(val sessionId: String) : Overlay

    /** Which branch the session's next turn is submitted on, and creating one. */
    data class Branches(val sessionId: String) : Overlay
}

/**
 * Where Back goes from a full screen.
 *
 * There is no navigation library here, so the parent relation is stated once
 * and both the on-screen back control and the system gesture read it. Round 11
 * wired the control to Settings and left the gesture on "close everything", so
 * Back from Looms dropped to the session surface (971-V F8). [Overlay.None]
 * means the session surface, which is the root.
 */
object OverlayNavigation {
    fun parent(overlay: Overlay): Overlay = when (overlay) {
        Overlay.Looms, Overlay.Accounts -> Overlay.Settings
        is Overlay.LoomAuthoring -> Overlay.Looms
        else -> Overlay.None
    }
}

/**
 * What this client has read about subagents (lane 971-UI-fleet).
 *
 * [active] and [subagents] describe the session on screen; [panel] is keyed by
 * the parent session each snapshot was read for, because a fleet snapshot is
 * only ever true of the session it was requested for.
 */
data class FleetState(
    val active: ai.diffforge.haider.ui.daemon.FleetLoad =
        ai.diffforge.haider.ui.daemon.FleetLoad.Unread,
    val subagents: ai.diffforge.haider.ui.daemon.SubagentLoad =
        ai.diffforge.haider.ui.daemon.SubagentLoad.Unread,
    val panel: Map<String, ai.diffforge.haider.ui.daemon.FleetLoad> = emptyMap(),
    val panelLoading: Boolean = false,
)

/** One descendant's replayed transcript, mounted read-only beside its parent. */
data class ChildTranscriptState(
    val sessionId: String,
    val agentId: String?,
    val parentSessionId: String?,
    val messages: List<Message> = emptyList(),
    val loading: Boolean = true,
    /** Set when the replay came back partial or unavailable; never hidden. */
    val notice: String? = null,
)

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
    /** Blocks staged for the next turn.submit. */
    val draftAttachments: List<ai.diffforge.haider.ui.daemon.Attachment> = emptyList(),
    /** The daemon's own refusal code for an attachment or a submit. */
    val attachmentNotice: String? = null,
    /** True while the queue-or-steer chooser is open. */
    val deliveryChooser: Boolean = false,
    /** `queue.list`; absence is not an empty list. */
    val queue: ai.diffforge.haider.ui.daemon.QueueSnapshot =
        ai.diffforge.haider.ui.daemon.QueueSnapshot(),
    val queueNotice: String? = null,
    /** `usage.report`, for the footer. */
    val usage: ai.diffforge.haider.ui.daemon.UsageSnapshot =
        ai.diffforge.haider.ui.daemon.UsageSnapshot(),
    val supportedPermissionModes: Set<PermissionMode> = PermissionMode.entries.toSet(),
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
    /** The facade's full-replacement projection, keyed by session id. */
    val shellExecutions: Map<String, List<ShellExecution>> = emptyMap(),
    /** The active session's unsent command line, keyed like [draft]. */
    val shellDraft: String = "",
    /**
     * The active session's submission whose response was lost. It keeps its
     * minted id: only an explicit user retry may resubmit it, with the SAME
     * id — never a regenerated one (FACADE-SHELL.md).
     */
    val shellPending: ShellSubmission? = null,
    /** The daemon's or facade's own refusal code for the last shell action. */
    val shellNotice: String? = null,
    /** True while this session's `shell.exec` round trip is in flight. */
    val shellBusy: Boolean = false,
    val answeredElsewhere: Set<String> = emptySet(),
    /** Subagent reads for the visible session and for the fleet panel. */
    val fleet: FleetState = FleetState(),
    /** The read-only descendant transcript, non-null only while it is open. */
    val childTranscript: ChildTranscriptState? = null,
    /**
     * Families the user has folded *away from* their default, so the default
     * can follow the child count without a stale boolean stranding a choice
     * ([SessionTree.expanded]).
     */
    val familyToggles: Set<String> = emptySet(),
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

    /** The terminal history the Shell tab renders, per the selected session. */
    val activeShellExecutions: List<ShellExecution>
        get() = shellExecutions[activeSessionId].orEmpty()

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

/**
 * One shell submission the UI minted an id for (lane 972-android-shell).
 *
 * FACADE-SHELL.md: the id is retained until `startShell` has definitely
 * returned or the user abandons an uncertain submission, and may be reused
 * only to retry this EXACT `(sessionId, command, cwd)` after response loss.
 */
data class ShellSubmission(
    val sessionId: String,
    val submissionId: String,
    val command: String,
    val cwd: String? = null,
)
