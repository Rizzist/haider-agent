package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.ui.loom.LoomAgentTypeEntry
import ai.diffforge.haider.ui.loom.LoomArchivedRef
import ai.diffforge.haider.ui.loom.LoomAuthorConfirmed
import ai.diffforge.haider.ui.loom.LoomAuthorDraft
import ai.diffforge.haider.ui.loom.LoomAuthorError
import ai.diffforge.haider.ui.loom.LoomAuthorKind
import ai.diffforge.haider.ui.loom.LoomEntryKind
import ai.diffforge.haider.ui.loom.LoomFence
import ai.diffforge.haider.ui.loom.LoomInstallJob
import ai.diffforge.haider.ui.loom.LoomInstallState
import ai.diffforge.haider.ui.loom.LoomRegistry
import ai.diffforge.haider.ui.loom.LoomRpcAdapter
import ai.diffforge.haider.ui.loom.LoomValidation
import ai.diffforge.haider.ui.loom.LoomWorkflowEntry
import ai.diffforge.haider.ui.workflow.ChildGraphLink
import ai.diffforge.haider.ui.workflow.ParentGraphAttempt
import ai.diffforge.haider.ui.workflow.SessionWorkflow
import ai.diffforge.haider.ui.workflow.WorkflowAst
import ai.diffforge.haider.ui.workflow.WorkflowAstNode
import ai.diffforge.haider.ui.workflow.WorkflowEdge
import ai.diffforge.haider.ui.workflow.WorkflowEdgeKind
import ai.diffforge.haider.ui.workflow.WorkflowGraphPhase
import ai.diffforge.haider.ui.workflow.WorkflowGraphRead
import ai.diffforge.haider.ui.workflow.WorkflowGraphSnapshot
import ai.diffforge.haider.ui.workflow.WorkflowJoin
import ai.diffforge.haider.ui.workflow.WorkflowNodePhase
import ai.diffforge.haider.ui.workflow.WorkflowNodeState
import ai.diffforge.haider.ui.workflow.WorkflowRejection
import ai.diffforge.haider.ui.workflow.WorkflowRpcAdapter
import ai.diffforge.haider.ui.workflow.WorkflowWatchEvent
import ai.diffforge.haider.ui.workflow.WorkflowWatchEventKind
import ai.diffforge.haider.ui.workflow.WorkflowWatchPage

/**
 * A scripted daemon for the workflow and Loom surfaces.
 *
 * The DAG is small on purpose and every shape the screen has to draw is in it:
 * a fan-out (IMPLEMENT into REVIEW and TEST), a join (SHIP over both), a
 * convergence gate, a back edge (a rejected REVIEW reopens IMPLEMENT) and a
 * second iteration whose child session is a **different** session from the
 * first attempt's. [advance] walks the eight recorded activation steps, so the
 * live transitions are the daemon's own facts replayed in order rather than an
 * animation the UI invented.
 *
 * Cursors move in irregular jumps because a session journal's do: a screen that
 * only works when cursors increase by one is a screen that will break.
 */
class FakeWorkflowLoom {

    companion object {
        const val GRAPH_ID = "g-nav-1"
        const val WORKFLOW_ID = "implement_verify"

        /** The scripted validator's defect marker; a revise removes it. */
        const val BAD_MARKER = "unregistered_type"

        private const val PLAN = "PLAN"
        private const val IMPLEMENT = "IMPLEMENT"
        private const val REVIEW = "REVIEW"
        private const val TEST = "TEST"
        private const val SHIP = "SHIP"

        /** The `ast_digest` fence. Carried verbatim; never recomputed here. */
        const val AST_DIGEST = "b7d41f0c9a2e5648"

        val AST: WorkflowAst = WorkflowAst(
            workflowId = WORKFLOW_ID,
            workflowDigest = "9f31c0a7c4e21b55",
            inputType = "Task",
            outputType = "Ship",
            nodes = listOf(
                WorkflowAstNode(PLAN, "Task", "Plan", WorkflowJoin(initialAll = listOf(1))),
                WorkflowAstNode(
                    IMPLEMENT,
                    "Plan",
                    "Patch",
                    WorkflowJoin(initialAll = listOf(2), reactivateAny = listOf(7)),
                ),
                WorkflowAstNode(REVIEW, "Patch", "Verdict", WorkflowJoin(initialAll = listOf(3))),
                WorkflowAstNode(TEST, "Patch", "Report", WorkflowJoin(initialAll = listOf(4))),
                WorkflowAstNode(
                    SHIP,
                    "Verdict",
                    "Ship",
                    WorkflowJoin(initialAll = listOf(5, 6)),
                    convergenceGate = true,
                ),
            ),
            edges = listOf(
                WorkflowEdge(1, WorkflowEdgeKind.GraphInput, "graph_input", null, PLAN, "Task"),
                WorkflowEdge(2, WorkflowEdgeKind.Forward, "forward", PLAN, IMPLEMENT, "Plan"),
                WorkflowEdge(3, WorkflowEdgeKind.Forward, "forward", IMPLEMENT, REVIEW, "Patch"),
                WorkflowEdge(4, WorkflowEdgeKind.Forward, "forward", IMPLEMENT, TEST, "Patch"),
                WorkflowEdge(5, WorkflowEdgeKind.Forward, "forward", REVIEW, SHIP, "Verdict"),
                WorkflowEdge(6, WorkflowEdgeKind.Forward, "forward", TEST, SHIP, "Report"),
                // The retry: a rejected verdict reopens the implementation.
                WorkflowEdge(7, WorkflowEdgeKind.Back, "back", REVIEW, IMPLEMENT, "Verdict"),
            ),
            edgesPublished = true,
            maxBackEdgeActivations = 3,
        )

        /**
         * The child sessions the daemon recorded, keyed by the exact
         * `parent_attempt`. Two IMPLEMENT attempts exist and they are different
         * sessions: a drill-in that matched on the node name alone would open
         * the first one after the retry.
         */
        val CHILD_LINKS: List<ChildGraphLink> = listOf(
            ChildGraphLink(
                parentAttempt = ParentGraphAttempt(GRAPH_ID, IMPLEMENT, 1),
                childSessionId = "s-child-implement-1",
                childGraphId = "g-child-impl-1",
                childRunId = "run-child-impl-1",
                workflow = "plain",
                template = "implement_child",
                digest = "c1a0",
                parentSlot = "patch",
            ),
            ChildGraphLink(
                parentAttempt = ParentGraphAttempt(GRAPH_ID, IMPLEMENT, 2),
                childSessionId = "s-child-implement-2",
                childGraphId = "g-child-impl-2",
                childRunId = "run-child-impl-2",
                workflow = "plain",
                template = "implement_child",
                digest = "c1a1",
                parentSlot = "patch",
            ),
            ChildGraphLink(
                parentAttempt = ParentGraphAttempt(GRAPH_ID, REVIEW, 1),
                childSessionId = "s-child-review-1",
                childGraphId = "g-child-review-1",
                childRunId = "run-child-review-1",
                workflow = "workflow_ref(reviewer_pass)",
                template = "reviewer_pass",
                digest = "c2b0",
                parentSlot = "verdict",
            ),
            // A same-named node under a DIFFERENT graph. Nothing in the drill-in
            // may resolve this for GRAPH_ID.
            ChildGraphLink(
                parentAttempt = ParentGraphAttempt("g-other-9", IMPLEMENT, 1),
                childSessionId = "s-child-other",
                childGraphId = "g-child-other",
                workflow = "plain",
            ),
        )

        /** The session-level `graph.status` fact the chip reads. */
        val SESSION_WORKFLOW: SessionWorkflow = SessionWorkflow(
            graphId = GRAPH_ID,
            template = WORKFLOW_ID,
            digest = "9f31c0a7c4e21b55",
            phase = "active",
            currentNode = IMPLEMENT,
            readyNodes = listOf(REVIEW, TEST),
        )
    }

    /** One recorded activation step: the state after it, and the facts it emitted. */
    private data class Step(
        val cursor: String,
        val phase: WorkflowGraphPhase,
        val backEdgeActivations: Int,
        val nodes: List<WorkflowNodeState>,
        val events: List<WorkflowWatchEvent>,
    )

    private fun node(
        name: String,
        phase: WorkflowNodePhase,
        iteration: Int = 0,
        order: Int? = null,
        cursor: String? = null,
        rejection: WorkflowRejection? = null,
        outputs: Int = 0,
        convergence: String? = null,
    ) = WorkflowNodeState(
        node = name,
        phase = phase,
        phaseRaw = when (phase) {
            WorkflowNodePhase.Waiting -> "waiting"
            WorkflowNodePhase.Activated -> "activated"
            WorkflowNodePhase.Completed -> "completed"
            WorkflowNodePhase.Rejected -> "rejected"
            else -> null
        },
        iteration = iteration,
        activationOrder = order?.toLong(),
        inputEdgeIds = AST.nodes.firstOrNull { it.node == name }?.join?.initialAll.orEmpty(),
        outputCount = outputs,
        convergenceDigest = convergence,
        rejection = rejection,
        updatedCursor = cursor,
    )

    private fun event(cursor: String, kind: WorkflowWatchEventKind, node: String?) =
        WorkflowWatchEvent(
            cursor = cursor,
            kind = kind,
            typeRaw = when (kind) {
                WorkflowWatchEventKind.GraphStarted -> "workflow_graph_started"
                WorkflowWatchEventKind.NodeActivated -> "workflow_node_activated"
                WorkflowWatchEventKind.NodeCompleted -> "workflow_node_completed"
                WorkflowWatchEventKind.NodeRejected -> "workflow_node_rejected"
                WorkflowWatchEventKind.Unknown -> null
            },
            node = node,
        )

    private val steps: List<Step> = buildSteps()

    private fun buildSteps(): List<Step> {
        val waiting = { name: String -> node(name, WorkflowNodePhase.Waiting) }
        return listOf(
            Step(
                cursor = "10",
                phase = WorkflowGraphPhase.Active,
                backEdgeActivations = 0,
                nodes = listOf(
                    node(PLAN, WorkflowNodePhase.Activated, 1, 1, "10"),
                    waiting(IMPLEMENT), waiting(REVIEW), waiting(TEST), waiting(SHIP),
                ),
                events = listOf(
                    event("9", WorkflowWatchEventKind.GraphStarted, null),
                    event("10", WorkflowWatchEventKind.NodeActivated, PLAN),
                ),
            ),
            Step(
                cursor = "14",
                phase = WorkflowGraphPhase.Active,
                backEdgeActivations = 0,
                nodes = listOf(
                    node(PLAN, WorkflowNodePhase.Completed, 1, 1, "12", outputs = 1),
                    node(IMPLEMENT, WorkflowNodePhase.Activated, 1, 2, "14"),
                    waiting(REVIEW), waiting(TEST), waiting(SHIP),
                ),
                events = listOf(
                    event("12", WorkflowWatchEventKind.NodeCompleted, PLAN),
                    event("14", WorkflowWatchEventKind.NodeActivated, IMPLEMENT),
                ),
            ),
            Step(
                cursor = "19",
                phase = WorkflowGraphPhase.Active,
                backEdgeActivations = 0,
                nodes = listOf(
                    node(PLAN, WorkflowNodePhase.Completed, 1, 1, "12", outputs = 1),
                    node(IMPLEMENT, WorkflowNodePhase.Completed, 1, 2, "17", outputs = 1),
                    node(REVIEW, WorkflowNodePhase.Activated, 1, 3, "18"),
                    node(TEST, WorkflowNodePhase.Activated, 1, 4, "19"),
                    waiting(SHIP),
                ),
                events = listOf(
                    event("17", WorkflowWatchEventKind.NodeCompleted, IMPLEMENT),
                    event("18", WorkflowWatchEventKind.NodeActivated, REVIEW),
                    event("19", WorkflowWatchEventKind.NodeActivated, TEST),
                ),
            ),
            Step(
                cursor = "23",
                phase = WorkflowGraphPhase.Active,
                backEdgeActivations = 0,
                nodes = listOf(
                    node(PLAN, WorkflowNodePhase.Completed, 1, 1, "12", outputs = 1),
                    node(IMPLEMENT, WorkflowNodePhase.Completed, 1, 2, "17", outputs = 1),
                    node(
                        REVIEW,
                        WorkflowNodePhase.Rejected,
                        1,
                        3,
                        "23",
                        rejection = WorkflowRejection(
                            code = "evidence_rejected",
                            message = "The patch loses the back-gesture test.",
                        ),
                    ),
                    node(TEST, WorkflowNodePhase.Completed, 1, 4, "21", outputs = 1),
                    waiting(SHIP),
                ),
                events = listOf(
                    event("21", WorkflowWatchEventKind.NodeCompleted, TEST),
                    event("23", WorkflowWatchEventKind.NodeRejected, REVIEW),
                ),
            ),
            Step(
                cursor = "26",
                phase = WorkflowGraphPhase.Active,
                backEdgeActivations = 1,
                nodes = listOf(
                    node(PLAN, WorkflowNodePhase.Completed, 1, 1, "12", outputs = 1),
                    node(IMPLEMENT, WorkflowNodePhase.Activated, 2, 5, "26"),
                    node(REVIEW, WorkflowNodePhase.Waiting, 1, 3, "23"),
                    node(TEST, WorkflowNodePhase.Completed, 1, 4, "21", outputs = 1),
                    waiting(SHIP),
                ),
                events = listOf(event("26", WorkflowWatchEventKind.NodeActivated, IMPLEMENT)),
            ),
            Step(
                cursor = "31",
                phase = WorkflowGraphPhase.Active,
                backEdgeActivations = 1,
                nodes = listOf(
                    node(PLAN, WorkflowNodePhase.Completed, 1, 1, "12", outputs = 1),
                    node(IMPLEMENT, WorkflowNodePhase.Completed, 2, 5, "29", outputs = 2),
                    node(REVIEW, WorkflowNodePhase.Activated, 2, 6, "30"),
                    node(TEST, WorkflowNodePhase.Activated, 2, 7, "31"),
                    waiting(SHIP),
                ),
                events = listOf(
                    event("29", WorkflowWatchEventKind.NodeCompleted, IMPLEMENT),
                    event("30", WorkflowWatchEventKind.NodeActivated, REVIEW),
                    event("31", WorkflowWatchEventKind.NodeActivated, TEST),
                ),
            ),
            Step(
                cursor = "36",
                phase = WorkflowGraphPhase.Active,
                backEdgeActivations = 1,
                nodes = listOf(
                    node(PLAN, WorkflowNodePhase.Completed, 1, 1, "12", outputs = 1),
                    node(IMPLEMENT, WorkflowNodePhase.Completed, 2, 5, "29", outputs = 2),
                    node(REVIEW, WorkflowNodePhase.Completed, 2, 6, "33", outputs = 1),
                    node(TEST, WorkflowNodePhase.Completed, 2, 7, "34", outputs = 1),
                    node(SHIP, WorkflowNodePhase.Activated, 1, 8, "36"),
                ),
                events = listOf(
                    event("33", WorkflowWatchEventKind.NodeCompleted, REVIEW),
                    event("34", WorkflowWatchEventKind.NodeCompleted, TEST),
                    event("36", WorkflowWatchEventKind.NodeActivated, SHIP),
                ),
            ),
            Step(
                cursor = "39",
                phase = WorkflowGraphPhase.Completed,
                backEdgeActivations = 1,
                nodes = listOf(
                    node(PLAN, WorkflowNodePhase.Completed, 1, 1, "12", outputs = 1),
                    node(IMPLEMENT, WorkflowNodePhase.Completed, 2, 5, "29", outputs = 2),
                    node(REVIEW, WorkflowNodePhase.Completed, 2, 6, "33", outputs = 1),
                    node(TEST, WorkflowNodePhase.Completed, 2, 7, "34", outputs = 1),
                    node(
                        SHIP,
                        WorkflowNodePhase.Completed,
                        1,
                        8,
                        "39",
                        outputs = 1,
                        convergence = "d31e77aa",
                    ),
                ),
                events = listOf(event("39", WorkflowWatchEventKind.NodeCompleted, SHIP)),
            ),
        )
    }

    /** Which recorded step the scripted daemon has reached. */
    var stepIndex: Int = 2
        private set

    /** The daemon lacks `workflow_graph_v1`. The screen must say so, not draw. */
    var graphUnavailable: Boolean = false

    /** The daemon answered `state: null` — a live session with no workflow. */
    var noGraph: Boolean = false

    fun advance(): Boolean {
        if (stepIndex >= steps.lastIndex) return false
        stepIndex += 1
        return true
    }

    fun rewind(index: Int) {
        stepIndex = index.coerceIn(0, steps.lastIndex)
    }

    val stepCount: Int get() = steps.size

    fun graphState(): WorkflowGraphRead = when {
        graphUnavailable ->
            WorkflowGraphRead.Unavailable(WorkflowRpcAdapter.FEATURE_WORKFLOW_GRAPH_V1)
        noGraph -> WorkflowGraphRead.NoGraph
        else -> {
            val step = steps[stepIndex]
            WorkflowGraphRead.Graph(
                WorkflowGraphSnapshot(
                    graphId = GRAPH_ID,
                    ast = AST,
                    astDigest = AST_DIGEST,
                    phase = step.phase,
                    phaseRaw = when (step.phase) {
                        WorkflowGraphPhase.Active -> "active"
                        WorkflowGraphPhase.Completed -> "completed"
                        WorkflowGraphPhase.Rejected -> "rejected"
                        WorkflowGraphPhase.Unknown -> null
                    },
                    throughCursor = step.cursor,
                    nextActivationOrder = step.nodes.mapNotNull { it.activationOrder }
                        .maxOrNull()?.plus(1),
                    backEdgeActivations = step.backEdgeActivations,
                    nodes = step.nodes,
                ),
            )
        }
    }

    /**
     * A watch page for everything after [afterCursor].
     *
     * `replay_through_cursor` is the step's own cursor even when no event is
     * returned, which is exactly how a real gap appears: the scan moved and the
     * page could not explain all of it, so the caller must re-read state.
     */
    fun watch(afterCursor: String, limit: Int): WorkflowWatchPage {
        if (graphUnavailable || noGraph) {
            return WorkflowWatchPage(afterCursor, afterCursor, afterCursor, emptyList())
        }
        val step = steps[stepIndex]
        val after = afterCursor.toBigIntegerOrNull()
        val fresh = steps.take(stepIndex + 1)
            .flatMap { it.events }
            .filter { entry ->
                val at = entry.cursor?.toBigIntegerOrNull() ?: return@filter false
                after == null || at > after
            }
            .take(limit)
        return WorkflowWatchPage(
            requestedAfterCursor = afterCursor,
            replayThroughCursor = step.cursor,
            nextCursor = fresh.lastOrNull()?.cursor ?: step.cursor,
            events = fresh,
        )
    }

    private fun String.toBigIntegerOrNull(): java.math.BigInteger? =
        runCatching { java.math.BigInteger(this) }.getOrNull()

    fun childLinks(): List<ChildGraphLink> = if (graphUnavailable) emptyList() else CHILD_LINKS

    // ---------- Loom ----------

    /** The daemon has no Loom registry at all. */
    var loomUnavailableReason: String? = null

    /** No provider/model is available for an AI draft. */
    var authoringUnavailableReason: String? = null

    private var registry: LoomRegistry = seedRegistry()

    private fun seedRegistry() = LoomRegistry(
        agentTypes = listOf(
            LoomAgentTypeEntry(
                id = "implementer",
                name = "Implementer",
                job = "Write the smallest change that makes the task true.",
                inType = "Plan",
                outType = "Patch",
                clis = listOf("git", "rg"),
                apis = listOf("api.anthropic.com"),
                denials = listOf("cli:curl"),
                skills = listOf("Kotlin", "Compose"),
                color = "#3B82F6",
                glyph = "I",
                rev = 4,
                digest = "a91c33f0",
            ),
            LoomAgentTypeEntry(
                id = "reviewer",
                name = "Reviewer",
                job = "Reject anything the evidence does not carry.",
                inType = "Patch",
                outType = "Verdict",
                clis = listOf("rg", "cargo"),
                color = "#DFA55A",
                glyph = "R",
                rev = 3,
                digest = "b0d4e112",
            ),
            LoomAgentTypeEntry(
                id = "shipper",
                name = "Shipper",
                job = "Land the change once every gate is green.",
                inType = "Verdict",
                outType = "Ship",
                clis = listOf("gh"),
                // No colour and no glyph: the registry has none, and the row
                // shows a neutral tile rather than one this app chose.
                rev = 1,
                digest = "cc7715a3",
            ),
        ),
        workflows = listOf(
            LoomWorkflowEntry(
                id = WORKFLOW_ID,
                pipeVersion = "pipe/v1",
                source = "plan @planner \"scope the task\" :cmd\n" +
                    "implement @implementer \"make it true\" :cmd\n" +
                    "review @reviewer \"reject unsupported claims\" :ship <-implement ↺implement\n" +
                    "test @implementer \"run the suite\" :cmd <-implement\n" +
                    "ship @shipper \"land it\" :all-of(2) <-review,test",
                inType = "Task",
                outType = "Ship",
                templateName = "implement_verify",
                nodeCount = 5,
                rev = 7,
                digest = "9f31c0a7",
            ),
            LoomWorkflowEntry(
                id = "triage_only",
                pipeVersion = "pipe/v1",
                source = "triage @reviewer \"decide if it is real\" :human",
                inType = "Report",
                outType = "Verdict",
                templateName = "triage_only",
                nodeCount = 1,
                rev = 2,
                digest = "5510aabb",
            ),
        ),
        // "cargo" is deliberately absent: the map says nothing about it, and
        // the row must read "not probed" rather than "missing".
        cliPresent = mapOf("git" to true, "rg" to true, "gh" to false),
    )

    /**
     * `archived_entries` rides only when the request asked for it — an empty
     * list after a default read is not proof that nothing is archived, so the
     * default read carries null.
     */
    fun loomList(includeArchived: Boolean): LoomRegistry = if (!includeArchived) {
        registry.copy(archived = null)
    } else {
        registry.copy(
            archived = registry.agentTypes.filter { it.archived }.map {
                LoomArchivedRef(LoomEntryKind.AgentType, it.id, it.rev ?: 0, it.digest.orEmpty(), true)
            } + registry.workflows.filter { it.archived }.map {
                LoomArchivedRef(LoomEntryKind.Workflow, it.id, it.rev ?: 0, it.digest.orEmpty(), true)
            },
        )
    }

    fun installJobs(): List<LoomInstallJob> = listOf(
        LoomInstallJob(
            jobId = "job-reviewer-3",
            agentTypeId = "reviewer",
            state = LoomInstallState.Failed,
            stateRaw = "failed",
            reason = "cargo was not found on PATH",
        ),
        LoomInstallJob(
            jobId = "job-implementer-4",
            agentTypeId = "implementer",
            state = LoomInstallState.Succeeded,
            stateRaw = "succeeded",
        ),
    )

    /**
     * `loom.archive` / `loom.unarchive`, with the CAS the real door has: a
     * fence whose revision is not the registry's current one loses, exactly as
     * it would against the daemon.
     */
    fun setArchived(kind: LoomEntryKind, id: String, archived: Boolean, fence: LoomFence): Boolean {
        val rev = fence.expectedRev ?: return false
        when (kind) {
            LoomEntryKind.AgentType -> {
                val current = registry.agentTypes.firstOrNull { it.id == id } ?: return false
                if (current.rev != rev) return false
                registry = registry.copy(
                    agentTypes = registry.agentTypes.map {
                        if (it.id == id) it.copy(archived = archived) else it
                    },
                )
            }
            LoomEntryKind.Workflow -> {
                val current = registry.workflows.firstOrNull { it.id == id } ?: return false
                if (current.rev != rev) return false
                registry = registry.copy(
                    workflows = registry.workflows.map {
                        if (it.id == id) it.copy(archived = archived) else it
                    },
                )
            }
            LoomEntryKind.Unknown -> return false
        }
        return true
    }

    // ---------- authoring ----------

    private var authoringRevision = 0L
    private var authoringSeq = 0

    /**
     * The scripted validator: any line containing [BAD_MARKER] is a typed
     * failure at that exact one-based line.
     *
     * Deliberately not clever. The point of the fake is that the *state
     * machine* is exercised — draft with errors, revise to fix, confirm — not
     * that a pipe compiler is reimplemented in Kotlin.
     */
    private fun validate(text: String): List<LoomAuthorError> =
        text.lines().mapIndexedNotNull { index, line ->
            val column = line.indexOf(BAD_MARKER)
            if (column < 0) {
                null
            } else {
                LoomAuthorError(
                    code = "unknown_agent_type",
                    message = "No agent type is registered under that id.",
                    line = index + 1,
                    column = column + 1,
                    field = "nodes[$index].agent_type",
                )
            }
        }

    fun validation(text: String): LoomValidation = LoomValidation(
        errors = validate(text),
        canonicalDigestPreview = if (validate(text).isEmpty()) "preview-%08x".format(text.hashCode()) else null,
    )

    fun authorDraft(kind: LoomAuthorKind, prose: String): LoomAuthorDraft {
        authoringUnavailableReason?.let { throw LoomUnavailable(it) }
        authoringSeq += 1
        authoringRevision = 1
        // The first draft comes back with a real defect, because that is the
        // case the revise step exists for.
        val text = when (kind) {
            LoomAuthorKind.AgentType -> agentTypeDraft(prose)
            LoomAuthorKind.Workflow -> workflowDraft(prose)
        }
        return LoomAuthorDraft(
            authoringId = "authoring-$authoringSeq",
            revision = authoringRevision,
            kind = kind,
            text = text,
            errors = validate(text),
        )
    }

    fun authorRevise(draft: LoomAuthorDraft, text: String): LoomAuthorDraft {
        authoringUnavailableReason?.let { throw LoomUnavailable(it) }
        // The daemon's compare-and-set: a stale fence loses.
        if (draft.revision != authoringRevision) {
            throw LoomUnavailable("authoring_revision_conflict")
        }
        authoringRevision += 1
        return draft.copy(revision = authoringRevision, text = text, errors = validate(text))
    }

    fun authorConfirm(
        draft: LoomAuthorDraft,
        text: String,
    ): Pair<LoomAuthorConfirmed?, List<LoomAuthorError>> {
        authoringUnavailableReason?.let { throw LoomUnavailable(it) }
        if (draft.revision != authoringRevision) {
            throw LoomUnavailable("authoring_revision_conflict")
        }
        val errors = validate(text)
        // `confirmed: null` with the errors beside it. Nothing is registered.
        if (errors.isNotEmpty()) return null to errors
        val id = if (draft.kind == LoomAuthorKind.AgentType) "planner" else "plan_only"
        val receipt = LoomAuthorConfirmed(
            authoringId = draft.authoringId,
            kind = draft.kind,
            canonicalText = text.trim(),
            registrationId = id,
            rev = 1,
            digest = "%08x".format(text.trim().hashCode()),
            updated = true,
            executionDigest = "exec-%08x".format(text.trim().hashCode()),
            installJobId = if (draft.kind == LoomAuthorKind.AgentType) "job-$id-1" else null,
        )
        registry = when (draft.kind) {
            LoomAuthorKind.AgentType -> registry.copy(
                agentTypes = registry.agentTypes + LoomAgentTypeEntry(
                    id = id,
                    name = "Planner",
                    job = "Break the task into the smallest verifiable steps.",
                    inType = "Task",
                    outType = "Plan",
                    clis = listOf("rg"),
                    color = "#3CCB7F",
                    glyph = "P",
                    rev = receipt.rev,
                    digest = receipt.digest,
                ),
            )
            LoomAuthorKind.Workflow -> registry.copy(
                workflows = registry.workflows + LoomWorkflowEntry(
                    id = id,
                    pipeVersion = "pipe/v1",
                    source = text.trim(),
                    inType = "Task",
                    outType = "Plan",
                    templateName = id,
                    nodeCount = text.trim().lines().size,
                    rev = receipt.rev,
                    digest = receipt.digest,
                ),
            )
        }
        return receipt to emptyList()
    }

    private fun agentTypeDraft(prose: String): String = """
        {
          "id": "planner",
          "name": "Planner",
          "job": ${quote(prose.ifBlank { "Break the task into the smallest verifiable steps." })},
          "in_type": "Task",
          "out_type": "Plan",
          "capability_keys": ["cli:rg", "cli:$BAD_MARKER"],
          "grants": ["cli:rg"],
          "denials": ["cli:$BAD_MARKER"],
          "color": "#3CCB7F",
          "glyph": "P"
        }
    """.trimIndent()

    private fun workflowDraft(prose: String): String = """
        {
          "name": "plan_only",
          "in_type": "Task",
          "out_type": "Plan",
          "nodes": [
            { "name": "plan", "agent_type": "$BAD_MARKER", "task": ${quote(prose.ifBlank { "scope the task" })}, "gate": "cmd" }
          ]
        }
    """.trimIndent()

    private fun quote(value: String) = org.json.JSONObject.quote(value)

    val loomFeature: String get() = LoomRpcAdapter.FEATURE_LOOM_V1
}
