package ai.diffforge.haider.ui.daemon

import org.json.JSONArray
import org.json.JSONObject

/**
 * Subagents and the descendant fleet, as the daemon publishes them.
 *
 * The wire truth is `crates/haider-rpc/src/frame.rs`:
 * `session.fleet` -> [SessionFleetSnapshot][fleetSnapshot] (frame.rs:2395),
 * `FleetNodeWire` (2321), `FleetRollupWire` (2382), `FleetAgentStateWire`
 * (2306), and `session.observe`'s `subagents: Vec<ObserveSubagentWire>` (2219,
 * carried at 2271).
 *
 * The house law is the desktop's (`fleetModel.js`), and it is about absence:
 *
 * - `folded_children` is honest. A node with `children: []` **and**
 *   `folded_children: N > 0` is bounded — "N more not shown" — never a leaf.
 *   Only `folded_children == 0` with no children is a real leaf.
 * - completeness (`truncated`, `complete`, `metrics_complete`) is tri-state.
 *   Only an explicit boolean is a daemon claim; absence is [FleetCompleteness.Unknown]
 *   and must never be presented as the complete tree.
 * - absent metrics/usage are "no data", never a fabricated zero.
 * - a missing `callsign` may fall back to the agent id, and the fallback is
 *   marked so it can never masquerade as a daemon-assigned identity.
 * - lineage comes only from the published `parent_session_id` /
 *   `parent_agent_id` / `agent_id`. An absent parent is unknown, never inferred.
 *
 * `ObserveSubagentWire.lockdown_bound` and `lockdown_auto_hermetic_bound` are
 * `#[serde(skip)]` (frame.rs:2232-2239): they never reach a client, so nothing
 * here reads them. `lockdown` is the public, self-sufficient view.
 */

/** `FleetAgentStateWire` (frame.rs:2306). `#[serde(other)]` makes Unknown real. */
enum class FleetAgentState { Queued, Live, Waiting, Done, Failed, Cancelled, Unknown }

/**
 * A published agent state plus the daemon's own word.
 *
 * [raw] null means the daemon published no state at all, which is a different
 * claim from a state it published that this build does not recognise — that one
 * keeps its raw string and renders as unknown rather than being coerced onto a
 * neighbouring known state.
 */
data class FleetStateView(val state: FleetAgentState, val raw: String?) {

    val recognised: Boolean get() = state != FleetAgentState.Unknown

    /** Queued, live and waiting: the closed set of non-terminal states. */
    val active: Boolean get() = state in ACTIVE_STATES

    val terminal: Boolean get() = state in TERMINAL_STATES

    companion object {
        val ACTIVE_STATES = setOf(
            FleetAgentState.Queued,
            FleetAgentState.Live,
            FleetAgentState.Waiting,
        )
        val TERMINAL_STATES = setOf(
            FleetAgentState.Done,
            FleetAgentState.Failed,
            FleetAgentState.Cancelled,
        )

        /** Absence is absence; an unrecognised word keeps itself. */
        fun of(raw: String?): FleetStateView {
            if (raw == null) return FleetStateView(FleetAgentState.Unknown, null)
            val state = when (raw) {
                "queued" -> FleetAgentState.Queued
                "live" -> FleetAgentState.Live
                "waiting" -> FleetAgentState.Waiting
                "done" -> FleetAgentState.Done
                "failed" -> FleetAgentState.Failed
                "cancelled" -> FleetAgentState.Cancelled
                else -> FleetAgentState.Unknown
            }
            return FleetStateView(state, raw)
        }
    }
}

/** `AgentUsageMetrics` (agent.rs:219), reduced to what a phone row shows. */
data class FleetUsage(
    val logicalInputTokens: Long,
    val billedOutputTokens: Long,
    val cacheReadTokens: Long,
    val cacheWriteTokens: Long,
)

/**
 * `AgentMetricsSnapshot` (agent.rs:196) for one node.
 *
 * Elapsed is `(terminal_at_ms | snapshot.generated_at_ms) - started_at_ms`, so
 * it is computed against the snapshot instant rather than the phone's clock.
 */
data class FleetNodeMetrics(
    val startedAtMs: Long,
    val terminalAtMs: Long? = null,
    val live: Boolean = false,
    val toolAttempts: Long = 0,
    val usage: FleetUsage? = null,
) {
    fun elapsedMs(generatedAtMs: Long?): Long? {
        val end = terminalAtMs ?: generatedAtMs ?: return null
        return (end - startedAtMs).coerceAtLeast(0)
    }
}

/** One recursively nested descendant — `FleetNodeWire` (frame.rs:2321). */
data class FleetNode(
    val agentId: String,
    val sessionId: String,
    val callsign: String? = null,
    val model: String? = null,
    val provider: String? = null,
    val task: String = "",
    val depth: Int? = null,
    val parentSessionId: String? = null,
    val parentAgentId: String? = null,
    val state: FleetStateView = FleetStateView.of(null),
    val metrics: FleetNodeMetrics? = null,
    val foldedChildren: Int = 0,
    val children: List<FleetNode> = emptyList(),
) {
    /** The daemon said there are more children than it returned. */
    val bounded: Boolean get() = foldedChildren > 0

    /** An empty child list alone proves nothing; this is the real leaf test. */
    val leaf: Boolean get() = foldedChildren == 0 && children.isEmpty()

    /**
     * Addressable means this node carries both coordinates a caller needs.
     * A node the daemon published without a parent session cannot be addressed
     * and the UI says so instead of substituting the surface's session.
     */
    val addressable: Boolean get() = parentSessionId != null && agentId.isNotEmpty()
}

/** `FleetStateCountsWire` (frame.rs:2358). Absence stays null. */
data class FleetStateCounts(
    val queued: Int? = null,
    val live: Int? = null,
    val waiting: Int? = null,
    val done: Int? = null,
    val failed: Int? = null,
    val cancelled: Int? = null,
)

/** `FleetRollupWire` (frame.rs:2382). Completeness flags are tri-state. */
data class FleetRollup(
    val nodeCount: Int? = null,
    val states: FleetStateCounts = FleetStateCounts(),
    val maxDepth: Int? = null,
    val elapsedMs: Long? = null,
    val toolAttempts: Long? = null,
    val usage: FleetUsage? = null,
    val metricsComplete: Boolean? = null,
    val complete: Boolean? = null,
)

/** `SessionFleetSnapshot` (frame.rs:2395). */
data class FleetSnapshot(
    val sessionId: String,
    val generatedAtMs: Long? = null,
    val nodeLimit: Int? = null,
    val depthLimit: Int? = null,
    val roots: List<FleetNode> = emptyList(),
    val rollup: FleetRollup? = null,
    val truncated: Boolean? = null,
)

/** How much of the tree this snapshot is. Absence is [Unknown], never complete. */
enum class FleetCompleteness { Bounded, Complete, Unknown }

/**
 * What a fleet read actually produced.
 *
 * [Unread] and [Snapshot] with no roots are different claims: nobody asked, and
 * the daemon answered "no subagents". Neither may be drawn as the other.
 */
sealed interface FleetLoad {
    data object Unread : FleetLoad
    data object Loading : FleetLoad
    data class Snapshot(val snapshot: FleetSnapshot) : FleetLoad

    /** The daemon does not offer `session_fleet_v1` (frame.rs:395). */
    data class Unavailable(val reason: String) : FleetLoad

    /** The read reached the daemon and failed; it can be retried. */
    data class Failed(val reason: String) : FleetLoad
}

/** `LockdownStatusWire` (frame.rs:1344), reduced to what a chip can say. */
data class SubagentLockdown(
    val provider: String? = null,
    val reason: String? = null,
    val quotaUsed: Long? = null,
    val quotaLimit: Long? = null,
)

/** `ObserveSubagentWire` (frame.rs:2219): the session header's chip state. */
data class Subagent(
    val agentId: String,
    val callsign: String? = null,
    val task: String = "",
    val state: FleetStateView = FleetStateView.of(null),
    val provider: String? = null,
    val lockdown: SubagentLockdown? = null,
)

/** A display identity, and whether it is the daemon's or this client's. */
data class FleetLabel(val text: String, val fallback: Boolean)

/** Pure transforms over the published tree. No daemon call happens here. */
object FleetModel {

    /** Bounds a client-side agent-id fallback so a row cannot be one long id. */
    const val AGENT_ID_FALLBACK_LENGTH = 12

    /**
     * The daemon's callsign, or a visibly-marked fallback over the agent id.
     * Never an invented title, and never the word "subagent" made up from an id.
     */
    fun label(callsign: String?, agentId: String): FleetLabel {
        val trimmed = callsign?.trim().orEmpty()
        if (trimmed.isNotEmpty()) return FleetLabel(trimmed, fallback = false)
        return FleetLabel(agentId.take(AGENT_ID_FALLBACK_LENGTH), fallback = true)
    }

    fun label(node: FleetNode): FleetLabel = label(node.callsign, node.agentId)

    fun label(subagent: Subagent): FleetLabel = label(subagent.callsign, subagent.agentId)

    /**
     * Boundedness as a tri-state.
     *
     * Complete is claimed only for an explicit `truncated: false` **and**
     * `complete: true`. Anything else — including a snapshot that published
     * neither — is unknown, which the UI must not present as the whole tree.
     */
    fun completeness(snapshot: FleetSnapshot): FleetCompleteness {
        val truncated = snapshot.truncated
        val complete = snapshot.rollup?.complete
        return when {
            truncated == true || complete == false -> FleetCompleteness.Bounded
            truncated == false && complete == true -> FleetCompleteness.Complete
            else -> FleetCompleteness.Unknown
        }
    }

    /** Every node in the tree, in tree order (pre-order, children in order). */
    fun flatten(roots: List<FleetNode>): List<FleetNode> {
        val out = mutableListOf<FleetNode>()
        fun walk(node: FleetNode) {
            out += node
            node.children.forEach(::walk)
        }
        roots.forEach(::walk)
        return out
    }

    /** The one selection coordinate: the real agent id. Never a substitute. */
    fun find(roots: List<FleetNode>, agentId: String?): FleetNode? {
        if (agentId.isNullOrEmpty()) return null
        return flatten(roots).firstOrNull { it.agentId == agentId }
    }

    /**
     * The child session id for one agent id, or null.
     *
     * `ObserveSubagentWire` carries no session id, so a chip built from an
     * observe digest can only open a transcript once the fleet snapshot has
     * published the pairing. Null means "not addressable yet", and the caller
     * has to offer the fleet panel rather than guess a session.
     */
    fun sessionOf(roots: List<FleetNode>, agentId: String?): String? =
        find(roots, agentId)?.sessionId?.takeIf { it.isNotEmpty() }

    /** Descendant session ids, tree order, deduped — the observe-batch feed. */
    fun sessionIds(roots: List<FleetNode>): List<String> =
        flatten(roots).map { it.sessionId }.filter { it.isNotEmpty() }.distinct()

    /**
     * The strongest state in a set, for a parent row's single dot.
     *
     * Attention first, exactly as the drawer orders sessions: a child asking
     * for a human outranks a failure, which outranks a running child. A fold
     * over an empty set is null — no children is not a state.
     */
    fun aggregate(states: List<FleetStateView>): FleetStateView? {
        if (states.isEmpty()) return null
        AGGREGATE_ORDER.forEach { kind ->
            states.firstOrNull { it.state == kind }?.let { return it }
        }
        return states.first()
    }

    private val AGGREGATE_ORDER = listOf(
        FleetAgentState.Waiting,
        FleetAgentState.Failed,
        FleetAgentState.Live,
        FleetAgentState.Queued,
        FleetAgentState.Cancelled,
        FleetAgentState.Done,
        FleetAgentState.Unknown,
    )
}

/**
 * `session.fleet` and `session.observe`, parsed from the frames the daemon
 * actually sends.
 *
 * Absence is preserved at every field: `has(...)` decides, not a zero default,
 * so "the daemon did not say" and "the daemon said zero" stay apart.
 */
object FleetRpcAdapter {
    const val METHOD_SESSION_FLEET = "session.fleet"
    const val METHOD_SESSION_OBSERVE = "session.observe"

    /** `RequestBody::SessionFleet` (frame.rs:3495). */
    fun fleetRequest(sessionId: String): JSONObject = JSONObject()
        .put("method", METHOD_SESSION_FLEET)
        .put("session_id", sessionId)

    /** `ResponseBody::SessionFleet { snapshot }` (frame.rs:4699). */
    fun parseFleet(body: JSONObject): FleetSnapshot {
        val snapshot = body.optJSONObject("snapshot") ?: body
        return FleetSnapshot(
            sessionId = snapshot.optString("session_id", ""),
            generatedAtMs = snapshot.longOrNull("generated_at_ms"),
            nodeLimit = snapshot.intOrNull("node_limit"),
            depthLimit = snapshot.intOrNull("depth_limit"),
            roots = snapshot.optJSONArray("roots").mapObjects(::parseNode),
            rollup = snapshot.optJSONObject("rollup")?.let(::parseRollup),
            truncated = snapshot.booleanOrNull("truncated"),
        )
    }

    fun parseNode(node: JSONObject): FleetNode = FleetNode(
        agentId = node.optString("agent_id", ""),
        sessionId = node.optString("session_id", ""),
        callsign = node.stringOrNull("callsign"),
        model = node.stringOrNull("model"),
        provider = node.stringOrNull("provider"),
        task = node.optString("task", ""),
        depth = node.intOrNull("depth"),
        parentSessionId = node.stringOrNull("parent_session_id"),
        parentAgentId = node.stringOrNull("parent_agent_id"),
        // `state` is a required field, so absence here is a malformed frame,
        // not a daemon claim — it stays unknown with no raw word.
        state = FleetStateView.of(node.stringOrNull("state")),
        metrics = node.optJSONObject("metrics")?.let(::parseMetrics),
        // `folded_children` is skipped when zero (`is_zero_u32`), so absence
        // really is zero here — the one place a default is the wire's own.
        foldedChildren = node.intOrNull("folded_children") ?: 0,
        children = node.optJSONArray("children").mapObjects(::parseNode),
    )

    fun parseRollup(rollup: JSONObject): FleetRollup {
        val states = rollup.optJSONObject("states")
        val metrics = rollup.optJSONObject("metrics")
        return FleetRollup(
            nodeCount = rollup.intOrNull("node_count"),
            states = FleetStateCounts(
                queued = states?.intOrNull("queued"),
                live = states?.intOrNull("live"),
                waiting = states?.intOrNull("waiting"),
                done = states?.intOrNull("done"),
                failed = states?.intOrNull("failed"),
                cancelled = states?.intOrNull("cancelled"),
            ),
            maxDepth = rollup.intOrNull("max_depth"),
            elapsedMs = metrics?.longOrNull("elapsed_ms"),
            toolAttempts = metrics?.longOrNull("tool_attempts"),
            usage = metrics?.optJSONObject("usage")?.let(::parseUsage),
            metricsComplete = rollup.booleanOrNull("metrics_complete"),
            complete = rollup.booleanOrNull("complete"),
        )
    }

    fun parseMetrics(metrics: JSONObject): FleetNodeMetrics = FleetNodeMetrics(
        startedAtMs = metrics.longOrNull("started_at_ms") ?: 0L,
        terminalAtMs = metrics.longOrNull("terminal_at_ms"),
        live = metrics.optBoolean("live", false),
        toolAttempts = metrics.longOrNull("tool_attempts") ?: 0L,
        usage = metrics.optJSONObject("usage")?.let(::parseUsage),
    )

    fun parseUsage(usage: JSONObject): FleetUsage = FleetUsage(
        logicalInputTokens = usage.longOrNull("logical_input_tokens") ?: 0L,
        billedOutputTokens = usage.longOrNull("billed_output_tokens") ?: 0L,
        cacheReadTokens = usage.longOrNull("cache_read_tokens") ?: 0L,
        cacheWriteTokens = usage.longOrNull("cache_write_tokens") ?: 0L,
    )

    /** `SessionObserveDigest.subagents` (frame.rs:2271). */
    fun parseSubagents(digest: JSONObject): List<Subagent> =
        digest.optJSONArray("subagents").mapObjects { entry ->
            Subagent(
                agentId = entry.optString("agent_id", ""),
                callsign = entry.stringOrNull("callsign"),
                task = entry.optString("task", ""),
                state = FleetStateView.of(entry.stringOrNull("state")),
                provider = entry.stringOrNull("provider"),
                lockdown = entry.optJSONObject("lockdown")?.let { lockdown ->
                    SubagentLockdown(
                        provider = lockdown.stringOrNull("provider"),
                        reason = lockdown.stringOrNull("reason"),
                        quotaUsed = lockdown.longOrNull("quota_used"),
                        quotaLimit = lockdown.longOrNull("quota_limit"),
                    )
                },
            )
        }
}

// ---------- absence-preserving readers ----------

private fun JSONObject.stringOrNull(key: String): String? =
    if (has(key) && !isNull(key)) optString(key).takeIf { it.isNotEmpty() } else null

private fun JSONObject.intOrNull(key: String): Int? =
    if (has(key) && !isNull(key)) optInt(key) else null

private fun JSONObject.longOrNull(key: String): Long? =
    if (has(key) && !isNull(key)) optLong(key) else null

private fun JSONObject.booleanOrNull(key: String): Boolean? =
    if (has(key) && !isNull(key)) optBoolean(key) else null

private fun <T> JSONArray?.mapObjects(transform: (JSONObject) -> T): List<T> {
    if (this == null) return emptyList()
    return (0 until length()).mapNotNull { index -> optJSONObject(index)?.let(transform) }
}
