package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.ui.loom.LoomAuthorConfirmed
import ai.diffforge.haider.ui.loom.LoomAuthorDraft
import ai.diffforge.haider.ui.loom.LoomAuthorError
import ai.diffforge.haider.ui.loom.LoomAuthorKind
import ai.diffforge.haider.ui.loom.LoomEntryKind
import ai.diffforge.haider.ui.loom.LoomFence
import ai.diffforge.haider.ui.loom.LoomInstallJob
import ai.diffforge.haider.ui.loom.LoomRegistry
import ai.diffforge.haider.ui.loom.LoomRpcAdapter
import ai.diffforge.haider.ui.loom.LoomValidation
import ai.diffforge.haider.ui.workflow.ChildGraphLink
import ai.diffforge.haider.ui.workflow.WorkflowGraphRead
import ai.diffforge.haider.ui.workflow.WorkflowRpcAdapter
import ai.diffforge.haider.ui.workflow.WorkflowWatchPage

/**
 * The workflow and Loom halves of the facade, added beside [DaemonService]
 * rather than inside it so the daemon-embedding lane's implementation keeps
 * compiling untouched.
 *
 * **Every default is the honest unavailable answer.** An implementation that
 * has not wired these doors yet reports the daemon feature it would need
 * (`workflow_graph_v1`, `loom_v1`, `loom_authoring_v1`), and the screens render
 * that verbatim. Nothing on this platform is drawn as working because a method
 * exists: on android-standalone some of these RPCs genuinely may not be there,
 * and a plausible empty list is exactly the lie the 971 UI rules forbid.
 */
interface WorkflowDaemon {

    /**
     * `workflow.graph.state`. Omitting [graphId] asks for the session's most
     * recently changed activation graph.
     *
     * The three distinguishable answers all survive the return type:
     * [WorkflowGraphRead.Graph], [WorkflowGraphRead.NoGraph] (the daemon said
     * `state: null`) and [WorkflowGraphRead.Unavailable].
     */
    suspend fun workflowGraphState(
        sessionId: String,
        graphId: String? = null,
    ): WorkflowGraphRead = WorkflowGraphRead.Unavailable(
        WorkflowRpcAdapter.FEATURE_WORKFLOW_GRAPH_V1,
    )

    /**
     * `workflow.graph.watch`. The page is a **change signal**: the caller reads
     * [ai.diffforge.haider.ui.workflow.WorkflowGraphModel.watchSignal] and, when
     * it says changed, re-reads [workflowGraphState]. No node phase is ever
     * folded out of these events.
     *
     * [afterCursor] is a decimal u64 string.
     */
    suspend fun workflowGraphWatch(
        sessionId: String,
        afterCursor: String,
        limit: Int = WorkflowRpcAdapter.WATCH_LIMIT,
    ): WorkflowWatchPage? = null

    /**
     * Every `ChildGraphAttached` fact in this session's journal.
     *
     * This is what makes a node tap open the right child: the daemon recorded
     * the `parent_attempt` → `child_session_id` mapping itself, and the UI
     * matches on that coordinate rather than on a name.
     */
    suspend fun childGraphLinks(sessionId: String): List<ChildGraphLink> = emptyList()
}

interface LoomDaemon {

    /** `loom.list`. [includeArchived] decides whether archived is a list or null. */
    suspend fun loomList(includeArchived: Boolean = false): LoomRegistry? = null

    /** The daemon's reason when the registry is not available, or null. */
    suspend fun loomUnavailable(): String? = LoomRpcAdapter.FEATURE_LOOM_V1

    /** `loom.install.status` with no filter: the newest retained jobs. */
    suspend fun loomInstallJobs(): List<LoomInstallJob> = emptyList()

    /**
     * `loom.archive` / `loom.unarchive`.
     *
     * The fence is required: `expected_rev` is not optional on this door, so an
     * entry whose revision was never read cannot be archived.
     */
    suspend fun loomSetArchived(
        kind: LoomEntryKind,
        id: String,
        archived: Boolean,
        fence: LoomFence,
    ): Boolean = false

    /** `loom.validate` — non-mutating; its digest is a preview, not a registration. */
    suspend fun loomValidate(kind: LoomAuthorKind, text: String): LoomValidation? = null

    /**
     * `loom.author.draft`. The session supplies the provider/model used for the
     * AI draft, which is why a session id is required and why a daemon with no
     * usable model answers with an unavailable reason rather than a draft.
     */
    suspend fun loomAuthorDraft(
        sessionId: String,
        kind: LoomAuthorKind,
        prose: String,
    ): LoomAuthorDraft = throw LoomUnavailable(LoomRpcAdapter.FEATURE_LOOM_AUTHORING_V1)

    /** `loom.author.revise` — re-parses the user's exact text under the draft's fence. */
    suspend fun loomAuthorRevise(draft: LoomAuthorDraft, text: String): LoomAuthorDraft =
        throw LoomUnavailable(LoomRpcAdapter.FEATURE_LOOM_AUTHORING_V1)

    /**
     * `loom.author.confirm`.
     *
     * The pair is the wire's own `{confirmed?, errors[]}`: a null receipt means
     * nothing was registered, and it is returned as a value rather than thrown,
     * because it is an answer and not a fault.
     */
    suspend fun loomAuthorConfirm(
        draft: LoomAuthorDraft,
        text: String,
        registryFence: LoomFence? = null,
    ): Pair<LoomAuthorConfirmed?, List<LoomAuthorError>> =
        throw LoomUnavailable(LoomRpcAdapter.FEATURE_LOOM_AUTHORING_V1)
}

/**
 * The daemon does not offer this door.
 *
 * Carries the daemon's own feature name or error code so the screen can show
 * it, rather than a sentence this app wrote about a daemon it cannot see.
 */
class LoomUnavailable(val reason: String) : IllegalStateException(reason)
