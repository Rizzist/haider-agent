package ai.diffforge.haider.ui.loom

import ai.diffforge.haider.ui.daemon.DaemonService
import ai.diffforge.haider.ui.daemon.LoomUnavailable
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

/** What the Looms screen renders. */
data class LoomScreenState(
    /** Null until a `loom.list` read landed — not the same as an empty registry. */
    val registry: LoomRegistry? = null,
    /** The daemon's own reason, shown verbatim when it has no registry. */
    val unavailable: String? = null,
    val installJobs: List<LoomInstallJob> = emptyList(),
    val includeArchived: Boolean = false,
    val loading: Boolean = false,
    val error: String? = null,
    /** The entry whose archive call is in flight, so its row can say so. */
    val busyId: String? = null,
    /** A compare-and-set that lost, named by the id it lost on. */
    val conflictId: String? = null,
) {
    val read: Boolean get() = registry != null || unavailable != null
}

class LoomController(
    private val service: DaemonService,
    private val scope: CoroutineScope,
) {
    private val _state = MutableStateFlow(LoomScreenState())
    val state: StateFlow<LoomScreenState> = _state.asStateFlow()

    fun refresh(includeArchived: Boolean = _state.value.includeArchived) {
        scope.launch {
            _state.value = _state.value.copy(loading = true, includeArchived = includeArchived)
            val unavailable = runCatching { service.loomUnavailable() }.getOrNull()
            if (unavailable != null) {
                // A daemon without the registry gets one honest screen, and no
                // list at all — an empty list would read as "you have none".
                _state.value = _state.value.copy(
                    registry = null,
                    unavailable = unavailable,
                    installJobs = emptyList(),
                    loading = false,
                )
                return@launch
            }
            val registry = runCatching { service.loomList(includeArchived) }.getOrElse { thrown ->
                _state.value = _state.value.copy(
                    loading = false,
                    error = thrown.message ?: thrown::class.java.simpleName,
                )
                return@launch
            }
            val jobs = runCatching { service.loomInstallJobs() }.getOrDefault(emptyList())
            _state.value = _state.value.copy(
                registry = registry,
                unavailable = null,
                installJobs = jobs,
                loading = false,
                error = null,
            )
        }
    }

    fun setIncludeArchived(include: Boolean) = refresh(include)

    /**
     * Archive or unarchive one entry under the fence the list actually
     * published. A `false` answer is a lost compare-and-set, and the row says
     * so rather than flipping and quietly disagreeing with the daemon.
     */
    fun setArchived(kind: LoomEntryKind, id: String, archived: Boolean, fence: LoomFence) {
        if (fence.expectedRev == null) {
            _state.value = _state.value.copy(conflictId = id)
            return
        }
        scope.launch {
            _state.value = _state.value.copy(busyId = id, conflictId = null)
            val ok = runCatching { service.loomSetArchived(kind, id, archived, fence) }
                .getOrDefault(false)
            _state.value = _state.value.copy(busyId = null, conflictId = if (ok) null else id)
            refresh()
        }
    }
}

/** The authoring flow's own state: the prompt, the kind, and where the cycle is. */
data class LoomAuthoringScreenState(
    val kind: LoomAuthorKind = LoomAuthorKind.AgentType,
    val prose: String = "",
    val authoring: LoomAuthoringState = LoomAuthoringState.Idle,
)

/**
 * prompt → draft → revise → confirm, for both authoring kinds.
 *
 * The controller owns exactly one rule the screen must not be trusted with:
 * every revise and confirm echoes the **draft's own** `authoring_id` and
 * `expected_revision`. There is no path here that sends either call without a
 * draft in hand, which is why [LoomAuthoringMachine] returns the state
 * unchanged instead of inventing a fence.
 */
class LoomAuthoringController(
    private val service: DaemonService,
    private val scope: CoroutineScope,
) {
    private val _state = MutableStateFlow(LoomAuthoringScreenState())
    val state: StateFlow<LoomAuthoringScreenState> = _state.asStateFlow()

    fun setKind(kind: LoomAuthorKind) {
        // Switching kind abandons the draft: an agent-type document is not a
        // workflow document, and carrying the text across would be nonsense
        // the daemon has to reject.
        _state.value = LoomAuthoringScreenState(kind = kind, prose = _state.value.prose)
    }

    fun setProse(prose: String) {
        _state.value = _state.value.copy(prose = prose)
    }

    fun setText(text: String) {
        _state.value = _state.value.copy(
            authoring = LoomAuthoringMachine.edited(_state.value.authoring, text),
        )
    }

    fun reset() {
        _state.value = LoomAuthoringScreenState(kind = _state.value.kind)
    }

    fun draft(sessionId: String) {
        val current = _state.value
        if (current.prose.isBlank()) return
        _state.value = current.copy(
            authoring = LoomAuthoringMachine.requestDraft(current.kind, current.prose),
        )
        scope.launch {
            val next = runCatching {
                LoomAuthoringMachine.drafted(
                    service.loomAuthorDraft(sessionId, current.kind, current.prose),
                )
            }.getOrElse { thrown -> unavailableOrFailed(thrown) }
            _state.value = _state.value.copy(authoring = next)
        }
    }

    fun revise() {
        val current = _state.value.authoring
        val draft = LoomAuthoringMachine.draftOf(current) ?: return
        val text = LoomAuthoringMachine.textOf(current)
        _state.value = _state.value.copy(authoring = LoomAuthoringMachine.requestRevise(current))
        scope.launch {
            val next = runCatching {
                LoomAuthoringMachine.revised(service.loomAuthorRevise(draft, text))
            }.getOrElse { thrown -> unavailableOrFailed(thrown) }
            _state.value = _state.value.copy(authoring = next)
        }
    }

    /** Non-mutating: it checks the text and never claims a registration. */
    fun validate() {
        val current = _state.value
        val editing = current.authoring as? LoomAuthoringState.Editing ?: return
        scope.launch {
            val validation = runCatching { service.loomValidate(current.kind, editing.text) }
                .getOrNull() ?: return@launch
            val held = _state.value.authoring
            if (held is LoomAuthoringState.Editing) {
                _state.value = _state.value.copy(authoring = held.copy(validation = validation))
            }
        }
    }

    fun confirm() {
        val current = _state.value.authoring
        if (!LoomAuthoringMachine.canConfirm(current)) return
        val draft = LoomAuthoringMachine.draftOf(current) ?: return
        val text = LoomAuthoringMachine.textOf(current)
        val pending = LoomAuthoringMachine.requestConfirm(current)
        _state.value = _state.value.copy(authoring = pending)
        scope.launch {
            val next = runCatching {
                val (receipt, errors) = service.loomAuthorConfirm(draft, text)
                // `confirmed: null` is an answer, not a fault: nothing was
                // registered and the draft survives so its errors can be fixed.
                LoomAuthoringMachine.confirmAnswered(pending, receipt, errors)
            }.getOrElse { thrown -> unavailableOrFailed(thrown, pending) }
            _state.value = _state.value.copy(authoring = next)
        }
    }

    /**
     * A daemon that says it cannot do this gets its own state, with its own
     * reason. Everything else is a failure with the message it carried; neither
     * is dressed up as a draft.
     */
    private fun unavailableOrFailed(
        thrown: Throwable,
        from: LoomAuthoringState = _state.value.authoring,
    ): LoomAuthoringState = when (thrown) {
        is LoomUnavailable -> LoomAuthoringState.Unavailable(thrown.reason)
        else -> LoomAuthoringMachine.failed(
            from,
            thrown.message ?: thrown::class.java.simpleName,
        )
    }
}
