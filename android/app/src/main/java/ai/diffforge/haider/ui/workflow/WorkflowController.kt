package ai.diffforge.haider.ui.workflow

import ai.diffforge.haider.ui.daemon.DaemonService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/** Everything the DAG screen renders, in one immutable value. */
data class WorkflowScreenState(
    val sessionId: String = "",
    val sessionTitle: String = "",
    val read: WorkflowGraphRead = WorkflowGraphRead.Unread,
    val links: List<ChildGraphLink> = emptyList(),
    /**
     * The live watch position, verbatim. Null means the loop has not baselined
     * — honestly "no position yet", never a fabricated "0".
     */
    val watchCursor: String? = null,
    /** Bounded journal facts from watch pages; unknown kinds kept. */
    val recentEvents: List<WorkflowWatchEvent> = emptyList(),
    val showAst: Boolean = false,
    val selectedNode: String? = null,
    val loading: Boolean = false,
    /** A read that failed for a reason that is not "the daemon lacks it". */
    val error: String? = null,
) {
    val snapshot: WorkflowGraphSnapshot?
        get() = (read as? WorkflowGraphRead.Graph)?.snapshot

    /** The child a tap on [selectedNode] would open, if the daemon recorded one. */
    val selectedChild: ChildGraphLink?
        get() {
            val graphId = snapshot?.graphId ?: return null
            val node = selectedNode ?: return null
            return ChildGraphIndex.latestLink(links, graphId, node)
        }

    val selectedState: WorkflowNodeState?
        get() = selectedNode?.let { snapshot?.nodeState(it) }
}

/**
 * The watch loop for one session's activation graph.
 *
 * Its whole job is the law from `useWorkflowGraph.js`: **a watch page is a
 * change signal, never a reduction input.** Node phases only ever come from a
 * `workflow.graph.state` read; the page decides whether to take one. A page
 * with no `next_cursor` drops the position to null so the next read
 * re-baselines, rather than inventing a resume cursor.
 *
 * A daemon that answers `Unavailable` settles there for this controller's
 * lifetime: polling a feature the daemon just said it does not have is noise
 * on a phone's radio and a battery.
 */
class WorkflowController(
    private val service: DaemonService,
    private val scope: CoroutineScope,
    private val pollMs: Long = WATCH_POLL_MS,
) {
    private val _state = MutableStateFlow(WorkflowScreenState())
    val state: StateFlow<WorkflowScreenState> = _state.asStateFlow()

    private var job: Job? = null
    private var settledUnavailable = false

    fun open(sessionId: String, sessionTitle: String, graphId: String? = null) {
        if (_state.value.sessionId == sessionId && job?.isActive == true) return
        close()
        settledUnavailable = false
        _state.value = WorkflowScreenState(
            sessionId = sessionId,
            sessionTitle = sessionTitle,
            loading = true,
        )
        job = scope.launch {
            loadState(sessionId, graphId)
            loadLinks(sessionId)
            while (isActive && !settledUnavailable) {
                delay(pollMs)
                if (!isActive || settledUnavailable) break
                pump(sessionId, graphId)
            }
        }
    }

    fun close() {
        job?.cancel()
        job = null
    }

    fun toggleAst() {
        _state.value = _state.value.copy(showAst = !_state.value.showAst)
    }

    /** Tapping the selected node again closes its panel. */
    fun selectNode(node: String?) {
        val current = _state.value
        _state.value = current.copy(
            selectedNode = if (current.selectedNode == node) null else node,
        )
    }

    fun refresh() {
        val current = _state.value
        if (current.sessionId.isEmpty()) return
        settledUnavailable = false
        scope.launch {
            _state.value = _state.value.copy(loading = true)
            loadState(current.sessionId, current.snapshot?.graphId)
            loadLinks(current.sessionId)
        }
    }

    private suspend fun pump(sessionId: String, graphId: String?) {
        val cursor = _state.value.watchCursor
        val page = runCatching {
            service.workflowGraphWatch(
                sessionId = sessionId,
                afterCursor = cursor ?: WorkflowCursor.BASELINE,
                limit = WorkflowRpcAdapter.WATCH_LIMIT,
            )
        }.getOrNull() ?: return
        val signal = WorkflowGraphModel.watchSignal(page)
        if (page.events.isNotEmpty()) {
            _state.value = _state.value.copy(
                recentEvents = (page.events + _state.value.recentEvents).take(RECENT_EVENT_CAP),
            )
        }
        // A null next_cursor is honest: the position is dropped so the next
        // read re-baselines from `through_cursor` instead of resuming from a
        // cursor this client would have had to invent.
        _state.value = _state.value.copy(watchCursor = signal.nextAfterCursor)
        if (signal.changed) loadState(sessionId, graphId)
    }

    private suspend fun loadState(sessionId: String, graphId: String?) {
        val read = runCatching { service.workflowGraphState(sessionId, graphId) }
            .getOrElse { thrown ->
                _state.value = _state.value.copy(
                    loading = false,
                    error = thrown.message ?: thrown::class.java.simpleName,
                )
                return
            }
        if (read is WorkflowGraphRead.Unavailable) settledUnavailable = true
        val snapshot = (read as? WorkflowGraphRead.Graph)?.snapshot
        _state.value = _state.value.copy(
            read = read,
            loading = false,
            error = null,
            // Baseline the watch from the state's own `through_cursor` whenever
            // the loop has no position of its own.
            watchCursor = _state.value.watchCursor ?: snapshot?.throughCursor,
        )
    }

    private suspend fun loadLinks(sessionId: String) {
        val links = runCatching { service.childGraphLinks(sessionId) }.getOrDefault(emptyList())
        _state.value = _state.value.copy(links = links)
    }

    companion object {
        const val WATCH_POLL_MS = 1_500L
        const val RECENT_EVENT_CAP = 30
    }
}
