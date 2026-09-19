package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.ui.checkpoints.BranchOutcome
import ai.diffforge.haider.ui.checkpoints.BranchView
import ai.diffforge.haider.ui.checkpoints.CheckpointListResult
import ai.diffforge.haider.ui.checkpoints.CheckpointOutcome
import ai.diffforge.haider.ui.checkpoints.CheckpointUnavailable
import ai.diffforge.haider.ui.checkpoints.Checkpoints
import ai.diffforge.haider.ui.chat.Message
import ai.diffforge.haider.ui.state.PermissionMode
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * The default [DaemonService.branchSelection]: one shared, permanently empty
 * flow, so a facade that does not implement branch selection does not mint a
 * new flow on every read.
 */
private val NO_BRANCH_SELECTION: StateFlow<Map<String, String>> =
    MutableStateFlow<Map<String, String>>(emptyMap()).asStateFlow()

/**
 * The only interface the UI lane consumes (UI-SPEC 5.3).
 *
 * The daemon-embedding lane (971-2/971-3) implements it over the Binder control
 * plane plus the RPC data plane; this lane ships [FakeDaemonService] so every
 * screen and every state-matrix row is reachable without a daemon.
 */

// ---------- daemon lifecycle (Android-owned) ----------

sealed interface DaemonStatus {
    data object Stopped : DaemonStatus
    data object Starting : DaemonStatus

    /**
     * The service is draining. contracts-v1 C2 requires this to read as a local
     * transition that does not accept new turns, rather than as Stopped.
     */
    data object Stopping : DaemonStatus
    data object Restarting : DaemonStatus
    data class Failed(val reason: String, val code: String?) : DaemonStatus
    data class Running(val info: DaemonInfo) : DaemonStatus
}

data class DaemonInfo(
    // from `daemon.status` / status.snapshot — daemon-owned
    val version: String,
    val generation: Long,
    val pid: Int? = null,
    val socketPath: String? = null,
    val ready: Boolean = true,
    val sessionCount: Long? = null,
    val waitingForRouteCount: Long? = null,
    val profilePath: String? = null,
    val runtimeDir: String? = null,
    // SERVICE-OWNED — the wire status carries no uptime and no memory number.
    // contracts-v1 C2 appends both to the snapshot as service-owned metrics:
    // a monotonic start time and a PSS sample, deliberately *not* labelled RSS.
    // Unknown metrics stay null.
    val startedAtElapsedRealtimeMs: Long? = null,
    val pssBytes: Long? = null,
)

// ---------- session roster ----------

enum class SessionVisualState { Running, NeedsInput, WaitingForNetwork, Idle, Errored, Unknown }

data class ForkProvenance(val sessionId: String, val seq: Long)

data class MenuOption(
    val key: String,
    val label: String,
    val detail: String? = null,
    /** allow_once | allow_always | reject_once | reject_always — style from this, never the label. */
    val decision: String? = null,
)

data class NeedsInput(
    /** permission|question|approval|recovery|secret|update|trust_hook|choice|conflict|file|exhausted|unknown — MAY GROW. */
    val kind: String,
    val title: String,
    /** Render verbatim; never rewrite into prose. */
    val safeBody: List<String> = emptyList(),
    val menuId: String? = null,
    val requestSeq: Long? = null,
    val workerGeneration: Long? = null,
    val sinceMs: Long? = null,
    val options: List<MenuOption> = emptyList(),
    /** true ⇒ NEVER a plain text field (UI-SPEC 3.7). */
    val secretAnswer: Boolean = false,
) {
    /** The daemon's own title, falling back to the first safe-body line. */
    val displayTitle: String
        get() = title.ifBlank { safeBody.firstOrNull().orEmpty() }

    /** The quote block, minus any line already promoted to the title. */
    val bodyLines: List<String>
        get() = if (title.isBlank()) safeBody.drop(1) else safeBody
}

data class SessionRow(
    val id: String,
    val title: String? = null,
    val state: SessionVisualState = SessionVisualState.Unknown,
    /** Raw ObserveRunStateWire string, kept for diagnostics. */
    val runState: String? = null,
    val provider: String? = null,
    val model: String? = null,
    val effort: String? = null,
    val fast: Boolean? = null,
    val agentType: String? = null,
    /** null = unknown, NOT 0. */
    val lastActivityMs: Long? = null,
    val createdAtMs: Long? = null,
    val seenAtMs: Long? = null,
    val turnCount: Long? = null,
    /** Some(0) = truly empty; null = unknown. */
    val footprintTokens: Long? = null,
    val footprintExact: Boolean? = null,
    val workspaceCwd: String? = null,
    val forkedFrom: ForkProvenance? = null,
    val parentSessionId: String? = null,
    val kind: String? = null,
    val needsInput: NeedsInput? = null,
    /**
     * `SessionSnapshot.workflow` (frame.rs:2299) — the session's own
     * `GraphStatus`, under `session_workflow_state_v1`. Null means the roster
     * carried none, which is why the chip's own state machine, and not this
     * field, decides between "no workflow" and "not read".
     */
    val workflow: ai.diffforge.haider.ui.workflow.SessionWorkflow? = null,
    /** A completed run may retain its ID; use [hasActiveRun] for activity. */
    val runId: String? = null,
    val workerGeneration: Long = 0L,
    val headSeq: Long = 0L,
    /**
     * `ObserveSessionWire.branches` — the DURABLE named refs. Main is implicit
     * and never appears here: the wire says so ("Main is implicit and is added
     * by observation clients"), so a client that showed a `main` row would be
     * showing a branch the daemon has no registry entry for.
     */
    val branches: List<BranchView> = emptyList(),
    /**
     * `ObserveSessionWire.active_branch_id`. `null` names the implicit main
     * branch — it is not "unknown", and it is not a branch id this client may
     * invent one for.
     */
    val activeBranchId: String? = null,
    /**
     * `ObserveSessionWire.main_head_node_id` / `main_head_seq` — the exact
     * committed node `branch.create` forks from. Both are required together;
     * a null node id means there is nothing to branch from yet, and the UI says
     * that rather than sending half a coordinate.
     */
    val mainHeadNodeId: String? = null,
    val mainHeadSeq: Long = 0L,
) {
    val hasActiveRun: Boolean
        get() = runId != null && when (runState) {
            "running", "waiting_for_route", "parked_permission", "parked_input", "effect_unknown" -> true
            else -> false
        }

    val unseen: Boolean
        get() = lastActivityMs != null && seenAtMs != null && lastActivityMs > seenAtMs
}

/**
 * One model from `ProviderSummaryWire.model_details`. Efforts belong to the
 * *model*, not the provider: Opus allows medium and high where Sonnet also
 * allows low, and flattening them by provider offered a setting the catalog
 * rejects.
 */
data class ModelOption(
    val id: String,
    val supportedEfforts: List<String> = emptyList(),
    val defaultEffort: String? = null,
    val contextWindow: Long? = null,
)

/** One provider row from `provider.list`, as the pickers need it. */
data class ProviderOption(
    val id: String,
    val label: String,
    val models: List<ModelOption>,
    val defaultModel: String?,
    val available: Boolean,
    val unavailableReason: String?,
    /**
     * `ModelInventoryAuthorityWire` (frame.rs:1227): whether this provider's
     * published list is the last word or merely advisory. Unknown by default,
     * which is what keeps a free-text field from appearing on a provider that
     * never said its list was advisory.
     */
    val inventoryAuthority: String? = null,
) {
    val modelIds: List<String> get() = models.map { it.id }
}

/** The `provider.list` snapshot plus its revision. */
data class ProviderInventory(
    val providers: List<ProviderOption> = emptyList(),
    val revision: Long? = null,
    val error: String? = null,
) {
    fun provider(id: String?): ProviderOption? = providers.firstOrNull { it.id == id }

    fun modelsFor(provider: String?): List<ModelOption> = provider(provider)?.models.orEmpty()

    fun model(provider: String?, model: String?): ModelOption? =
        provider(provider)?.models?.firstOrNull { it.id == model }

    /**
     * The efforts the *selected model* actually supports. An unknown pair
     * offers nothing rather than a plausible-looking default: the picker would
     * rather be empty than wrong.
     */
    fun effortsFor(provider: String?, model: String?): List<String> =
        model(provider, model)?.supportedEfforts.orEmpty()

    /** True when the catalog does not list this effort for this model. */
    fun rejectsEffort(provider: String?, model: String?, effort: String?): Boolean {
        if (effort == null) return false
        val supported = effortsFor(provider, model)
        return supported.isNotEmpty() && effort !in supported
    }
}

/** Daemon-observed eligibility for an explicit user command in one session. */
data class ShellAvailability(
    val available: Boolean = false,
    val reason: String? = null,
    val sessionId: String? = null,
    val workerGeneration: Long? = null,
)

/** Roster page state, so the drawer can scroll hundreds of sessions honestly. */
data class RosterPaging(
    val loading: Boolean = false,
    val hasMore: Boolean = false,
    val cursor: String? = null,
)

/**
 * The `run_state` -> visual-state fold (UI-SPEC 5.3). The only sanctioned one.
 *
 * `Unknown` is neutral on purpose (sessionActivity.js:91-94): it never renders
 * green, never animates, never claims the session is fine.
 */
object SessionVisualStateFold {
    fun fold(runState: String?, needsInput: NeedsInput?): SessionVisualState {
        if (needsInput != null) return SessionVisualState.NeedsInput
        return when (runState?.lowercase()) {
            "parked_input", "parked_permission" -> SessionVisualState.NeedsInput
            "running" -> SessionVisualState.Running
            "waiting_for_route" -> SessionVisualState.WaitingForNetwork
            "idle", "cancelled" -> SessionVisualState.Idle
            "errored" -> SessionVisualState.Errored
            else -> SessionVisualState.Unknown
        }
    }

    /** Neither a rail nor a pill is drawn for these; nothing animates either. */
    fun rendersRail(state: SessionVisualState): Boolean = when (state) {
        SessionVisualState.Running,
        SessionVisualState.NeedsInput,
        SessionVisualState.WaitingForNetwork,
        SessionVisualState.Errored,
        -> true
        SessionVisualState.Idle, SessionVisualState.Unknown -> false
    }

    /** Errored never animates — nothing pulses for a corpse (render.rs:1129-1144). */
    fun animates(state: SessionVisualState): Boolean = state == SessionVisualState.Running
}

/** The compare-and-set coordinates a cancel needs, read from one snapshot. */
data class CancelCoordinates(
    val sessionId: String,
    val runId: String,
    val workerGeneration: Long,
)

/**
 * The compare-and-set identity `WireFrame::MenuAnswer` requires
 * (frame.rs:5926). Every field must come from the *same* snapshot that
 * rendered the card: `menu_id` from one poll paired with a `request_seq` from
 * another answers a question that no longer exists.
 */
data class MenuCoordinates(
    val sessionId: String,
    val menuId: String,
    val requestSeq: Long,
    val workerGeneration: Long,
    val commandId: String,
) {
    companion object {
        /**
         * Null whenever the rendered prompt is missing any coordinate — the UI
         * then shows no answer affordance rather than guessing one.
         */
        fun of(sessionId: String, needsInput: NeedsInput?, commandId: String): MenuCoordinates? {
            val menuId = needsInput?.menuId ?: return null
            val requestSeq = needsInput.requestSeq ?: return null
            val workerGeneration = needsInput.workerGeneration ?: return null
            return MenuCoordinates(sessionId, menuId, requestSeq, workerGeneration, commandId)
        }
    }
}

/** `MenuInput` (frame.rs:5805). A secret only ever travels as a reference. */
sealed interface MenuAnswerInput {
    data class Text(val text: String) : MenuAnswerInput
    data class Secret(val vaultReference: String) : MenuAnswerInput {
        override fun toString(): String = "MenuAnswerInput.Secret(redacted)"
    }
}

object TurnCancel {
    /**
     * `run_id` and `worker_generation` must come from the same message as
     * `run_state`; pairing an id from one poll with a state from another can
     * cancel a run that already ended (frame.rs:1716-1730).
     */
    fun coordinates(row: SessionRow?): CancelCoordinates? {
        val runId = row?.runId ?: return null
        // Current daemons may retain the terminal run's identity in an idle
        // summary. Identity alone does not authorize a Stop affordance.
        if (!row.hasActiveRun) return null
        return CancelCoordinates(row.id, runId, row.workerGeneration)
    }
}

// ---------- the service the UI talks to ----------

/**
 * Lane 971-UI-workflows extends the facade by inheritance, not by editing the
 * body: [WorkflowDaemon] and [LoomDaemon] carry their own honest defaults, so an
 * implementation that has not wired those doors reports the daemon feature it
 * would need instead of failing to compile or, worse, answering emptily.
 */
interface DaemonService : WorkflowDaemon, LoomDaemon {
    val status: StateFlow<DaemonStatus>

    /** Network / notification / battery signals, from the C2 snapshot. */
    val environment: StateFlow<DaemonEnvironment>

    /** Forward Activity permission results to the Android control-plane adapter. */
    suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean)
    val sessions: StateFlow<List<SessionRow>>
    val paging: StateFlow<RosterPaging>

    /** True only after a successful roster baseline in this RPC connection epoch. */
    val rosterReady: StateFlow<Boolean>
    val activeSessionId: StateFlow<String?>
    val models: StateFlow<SessionConfig?>

    /** `provider.list` inventory, for the composer's provider/model pickers. */
    val providers: StateFlow<ProviderInventory>

    /**
     * How much the daemon lets the model do without asking (addition H6).
     *
     * The policy lives in the daemon — android-standalone Auto mode is not
     * something the UI can grant itself — so this is the door the UI reads and
     * sets it through, not a local flag.
     */
    val permissionMode: StateFlow<PermissionMode>
    /** Modes supported by this adapter's actual policy contract. */
    val supportedPermissionModes: Set<PermissionMode> get() = PermissionMode.entries.toSet()
    suspend fun setPermissionMode(mode: PermissionMode)

    /** Current selected session capability, revoked while unknown/disconnected. */
    val shell: StateFlow<ShellAvailability>
    /** Bounded recent command history for observed sessions, reconstructed from redacted replay. */
    val shellExecutions: StateFlow<Map<String, List<ShellExecution>>>
    suspend fun refreshShell()
    /** submissionId is a caller-retained UUID; reuse it only to retry the SAME submission. */
    suspend fun startShell(sessionId: String, submissionId: String, command: String, cwd: String? = null): ShellExecutionRef
    suspend fun cancelShell(execution: ShellExecutionRef)
    val catalogError: StateFlow<String?>

    /** When the catalog request went out; drives the model chip's 6 s deadline. */
    val catalogRequestedAtMs: StateFlow<Long?>

    suspend fun start()
    suspend fun stop()
    suspend fun restart()
    suspend fun refreshRoster()

    /** Paginated `session.list`; appends the next page to [sessions]. */
    suspend fun loadMoreSessions()
    suspend fun createSession(model: String? = null, effort: String? = null): String
    suspend fun activate(sessionId: String)
    suspend fun markSeen(sessionId: String)
    suspend fun rename(sessionId: String, title: String)
    suspend fun fork(sessionId: String): String

    /** Refuses unless [TurnCancel.coordinates] resolves from the current snapshot. */
    suspend fun stopTurn(sessionId: String)
    /**
     * Answers with the full frozen coordinate set. There is no overload that
     * takes a bare `menuId`: an answer without `request_seq` and
     * `worker_generation` cannot be a compare-and-set.
     */
    suspend fun answer(
        coordinates: MenuCoordinates,
        optionKey: String,
        optionIndex: Int,
        input: MenuAnswerInput? = null,
    )

    /**
     * `vault.stage` with purpose `menu_secret`, returning the opaque reference
     * an answer may carry. The plaintext never leaves this call.
     */
    suspend fun stageMenuSecret(secret: CharArray): String
    /**
     * [confirmNewEpoch] is the user's own answer, never inferred.
     *
     * Switching model or effort can invalidate the prompt cache, and the daemon
     * refuses until the caller says it understands that. The facade sends false
     * unless a person has answered a refusal, so consent cannot be manufactured
     * by a retry loop (lane 971-3 handoff).
     */
    suspend fun selectModel(provider: String, model: String, confirmNewEpoch: Boolean = false)
    suspend fun selectEffort(effort: String?, confirmNewEpoch: Boolean = false)
    suspend fun refreshModels()

    /** `provider.list`; also the door the provider picker refreshes through. */
    suspend fun refreshProviders()
    suspend fun selectProvider(provider: String)
    /**
     * `turn.submit`. [attachments] and [mode] are defaulted, so every existing
     * caller and every sibling implementation keeps compiling; the wire
     * default for mode is Steer (`haider-protocol/lib.rs:174`).
     *
     * Refusals come back as [AttachmentRefused] carrying the daemon's own code
     * — `too_many_attachments` or `attachments_too_large`.
     */
    suspend fun send(
        sessionId: String,
        text: String,
        attachments: List<Attachment> = emptyList(),
        mode: Delivery = Delivery.Steer,
    )

    /**
     * Puts one local file into the daemon's CAS and returns the block that
     * names it. Null means the daemon would not take it; the caller shows the
     * reason it reported rather than guessing.
     */
    suspend fun stageAttachment(bytes: ByteArray, mime: String, name: String?): Attachment?

    /** CAS bytes for a thumbnail. Null when the artifact is gone. */
    suspend fun attachmentBytes(artifact: String): ByteArray?

    /** `queue.list` plus its deltas; absence is not an empty list. */
    val queue: StateFlow<QueueSnapshot>
    suspend fun refreshQueue(sessionId: String)

    /** Both fenced by the revision they were read at. */
    suspend fun removeQueued(sessionId: String, id: String, revision: Long)
    suspend fun promoteQueued(sessionId: String, id: String, revision: Long)

    /** `usage.report`. Read-only, and an estimate is labelled as one. */
    val usage: StateFlow<UsageSnapshot>
    suspend fun refreshUsage()

    /**
     * `session.attach{after_seq:0, mode:"view"}` replay, paged through
     * `session.read` (contracts-v1, full RPC/history).
     *
     * Returns a [TranscriptLoad] rather than a bare list because history can be
     * genuinely partial: a range is capped at [SESSION_READ_MAX_ENVELOPES], and
     * an envelope over the negotiated mobile limit has to take a truthful
     * unavailable path instead of silent truncation presented as complete.
     */
    suspend fun transcript(sessionId: String): TranscriptLoad

    /** Collect for the active session. Production folds live pushes without reattaching per event. */
    fun transcriptUpdates(sessionId: String): Flow<TranscriptLoad> =
        kotlinx.coroutines.flow.flow { emit(transcript(sessionId)) }

    /** Progress of the local transcript index that drawer search reads. */
    val searchIndex: StateFlow<SearchIndexState>

    /** Title/metadata plus indexed transcript content, honest about coverage. */
    suspend fun search(query: String): SearchOutcome

    // ---------- subagents and the descendant fleet (lane 971-UI-fleet) ----------

    /**
     * `session.fleet` (frame.rs:3495): the bounded descendant tree and the
     * daemon's own rollup, read-only and receipt-free.
     *
     * The default is [FleetLoad.Unavailable] rather than an empty snapshot: an
     * implementation that has not wired the read has not learned that a session
     * has no subagents, and the two must not render the same.
     */
    suspend fun fleet(sessionId: String): FleetLoad =
        FleetLoad.Unavailable(FLEET_NOT_WIRED)

    /**
     * `session.observe`'s `subagents` (frame.rs:2271) for one session — the
     * chip state on the session header.
     *
     * Same rule as [fleet]: not-wired is its own answer, never an empty roster.
     */
    suspend fun subagents(sessionId: String): SubagentLoad =
        SubagentLoad.Unavailable(SUBAGENTS_NOT_WIRED)
    // ---------- checkpoints and branches ----------

    /**
     * Which branch each session's NEXT `turn.submit` carries, by session id.
     *
     * An absent entry is the implicit main branch. This is a client choice, not
     * a daemon one: the wire has `branch.create` and a `branch_id` on
     * `turn.submit` (frame.rs:3697, transcript entry 82) but no `branch.switch`,
     * so "switching" is choosing what the next turn is submitted on — it does
     * not move anything the daemon already committed, and the sheet says so.
     */
    val branchSelection: StateFlow<Map<String, String>> get() = NO_BRANCH_SELECTION

    /** Chooses the branch [sessionId]'s next turn is submitted on. Null = main. */
    suspend fun selectBranch(sessionId: String, branchId: String?) = Unit

    /**
     * `checkpoint.list`, newest first.
     *
     * The default is the honest one for a facade that does not serve
     * `checkpoint_v1`: unavailable, rather than an empty page that would read
     * as "this session changed nothing".
     */
    suspend fun checkpoints(
        sessionId: String,
        branchId: String? = null,
        cursor: Long? = null,
        limit: Int = Checkpoints.PAGE_LIMIT,
    ): CheckpointListResult = CheckpointListResult.Unavailable(CheckpointUnavailable.FEATURE_ABSENT)

    /** `checkpoint.undo`. [target] is a checkpoint id or [Checkpoints.TARGET_LAST]. */
    suspend fun undoCheckpoint(
        sessionId: String,
        target: String,
        branchId: String? = null,
    ): CheckpointOutcome = CheckpointOutcome.Unavailable(CheckpointUnavailable.FEATURE_ABSENT)

    /** `checkpoint.redo`, same target vocabulary. */
    suspend fun redoCheckpoint(
        sessionId: String,
        target: String,
        branchId: String? = null,
    ): CheckpointOutcome = CheckpointOutcome.Unavailable(CheckpointUnavailable.FEATURE_ABSENT)

    /** `checkpoint.rollback_turn` — all of one run's durable edits, or none. */
    suspend fun rollbackTurn(
        sessionId: String,
        runId: String,
        branchId: String? = null,
    ): CheckpointOutcome = CheckpointOutcome.Unavailable(CheckpointUnavailable.FEATURE_ABSENT)

    /**
     * `branch.create` at an EXACT committed node.
     *
     * Both coordinates are required and must come from the same published fact;
     * the caller reads them from the row, and a row without a head node has
     * nothing to fork from.
     */
    suspend fun createBranch(
        sessionId: String,
        forkNodeId: String,
        forkSeq: Long,
        name: String?,
        sourceBranchId: String? = null,
    ): BranchOutcome = BranchOutcome.Unavailable(CheckpointUnavailable.BRANCH_FEATURE_ABSENT)
}

/** What one `session.observe` said about a session's subagents. */
sealed interface SubagentLoad {
    /** Empty for every case but [Observed]: nothing invents a roster. */
    val subagents: List<Subagent> get() = emptyList()

    /** Nobody has asked yet. Distinct from a digest that listed none. */
    data object Unread : SubagentLoad
    data class Observed(override val subagents: List<Subagent>) : SubagentLoad
    data class Unavailable(val reason: String) : SubagentLoad
}

/** The daemon-facing reason strings for an unwired fleet seam. */
const val FLEET_NOT_WIRED = "session_fleet_not_wired"
const val SUBAGENTS_NOT_WIRED = "session_observe_subagents_not_wired"

// ---------- history ----------

/** `session.read` ranges start at 1 and carry at most this many envelopes. */
const val SESSION_READ_MAX_ENVELOPES: Long = 1_024

/**
 * The read pager. Ranges are inclusive, start at sequence 1 and never exceed
 * [SESSION_READ_MAX_ENVELOPES]; the caller shrinks [pageSize] further when the
 * negotiated byte cap demands it.
 */
object TranscriptPager {
    data class Range(val startSeq: Long, val endSeq: Long)

    fun ranges(
        headSeq: Long,
        pageSize: Long = SESSION_READ_MAX_ENVELOPES,
        fromSeq: Long = 1,
    ): List<Range> {
        require(pageSize >= 1) { "page size must be positive" }
        val capped = minOf(pageSize, SESSION_READ_MAX_ENVELOPES)
        val start = maxOf(1L, fromSeq)
        if (headSeq < start) return emptyList()
        val out = mutableListOf<Range>()
        var cursor = start
        while (cursor <= headSeq) {
            val end = minOf(headSeq, cursor + capped - 1)
            out += Range(cursor, end)
            cursor = end + 1
        }
        return out
    }
}

/** What a replay actually returned. Partial history says so out loud. */
sealed interface TranscriptLoad {
    val messages: List<Message>

    data class Complete(override val messages: List<Message>) : TranscriptLoad

    /** Some envelopes are still being read, or one exceeded the mobile limit. */
    data class Partial(
        override val messages: List<Message>,
        val reason: String,
        val loadedThroughSeq: Long,
        val headSeq: Long,
    ) : TranscriptLoad

    data class Unavailable(val reason: String) : TranscriptLoad {
        override val messages: List<Message> get() = emptyList()
    }
}

/**
 * Search completeness is known only after coverage through each recorded head,
 * so the UI shows how far the index has got rather than implying totality.
 */
data class SearchIndexState(
    val indexedSessions: Int = 0,
    val totalSessions: Int = 0,
    val complete: Boolean = false,
) {
    val inProgress: Boolean get() = !complete && totalSessions > 0
}

data class SearchHit(val sessionId: String, val snippet: String?, val seq: Long?)

data class SearchOutcome(
    val hits: List<SearchHit>,
    val index: SearchIndexState,
    /** False while the index is still catching up: results may be incomplete. */
    val complete: Boolean,
)

/** Raised when a cancel was asked for without live coordinates. */
class MissingRunCoordinates(sessionId: String) :
    IllegalStateException("No active run for session $sessionId")
