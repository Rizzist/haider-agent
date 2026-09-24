package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.ui.daemon.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.channels.Channel
import java.util.concurrent.atomic.AtomicLong
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.*
import java.io.IOException
import java.util.Base64

/** One-shot shell control over the existing session RPC and redacted journal. */
internal class ShellRepository(
    private val client: RpcClient,
    private val replay: TranscriptRepository,
    private val selected: StateFlow<String?>,
    private val ready: () -> Boolean,
    invalidations: Flow<Unit>,
    scope: CoroutineScope,
) {
    private val availability = MutableStateFlow(ShellAvailability(reason = "not_observed"))
    val shell: StateFlow<ShellAvailability> = availability.asStateFlow()
    private val history = MutableStateFlow<Map<String, List<ShellExecution>>>(emptyMap())
    val executions: StateFlow<Map<String, List<ShellExecution>>> = history.asStateFlow()
    private val observed = MutableStateFlow<Set<String>>(emptySet())
    private val submissions = mutableMapOf<String, Submission>()
    private val submitMutex = Mutex()
    private val refreshMutex = Mutex()
    private val observationVersion = AtomicLong()
    private val refreshes = Channel<Unit>(Channel.CONFLATED)
    private data class Submission(val session: String, val text: String, val cwd: String?, val body: JsonObject,
        var receipt: ShellExecutionRef? = null)

    init {
        scope.launch {
            invalidations.collect {
                observationVersion.incrementAndGet()
                availability.value = ShellAvailability(reason = if (ready()) "checking" else "disconnected", sessionId = selected.value)
                refreshes.trySend(Unit)
            }
        }
        // Do not cancel an in-flight RpcClient request when a new observation arrives:
        // cancellation deliberately closes that client's shared socket.
        scope.launch { for (ignored in refreshes) refresh() }
        // Repair a missed provider/grant publication; this does not confer authority.
        scope.launch { while (isActive) { delay(5_000); refreshes.trySend(Unit) } }
        scope.launch {
            combine(replay.revision, replay.caughtUp, client.state, observed) { _, caught, state, ids ->
                ids.associateWith { id -> ShellProjection.project(id, replay.transcript(id),
                    state == RpcConnectionState.CONNECTED && id in caught) }
            }.collect { history.value = it }
        }
        // Output chunks revise history without changing admission. Re-observe only
        // when the selected session's newest execution changes lifecycle state.
        scope.launch {
            combine(history, selected) { executions, id -> id to id?.let { executions[it]?.lastOrNull()?.status } }
                .distinctUntilChanged().drop(1).collect { refreshes.trySend(Unit) }
        }
    }

    suspend fun refresh() = refreshMutex.withLock {
        val id = selected.value
        if (id == null || !ready()) {
            availability.value = ShellAvailability(reason = if (id == null) "no_session" else "disconnected", sessionId = id)
            return@withLock
        }
        observed.update { (it + id).toList().takeLast(32).toSet() }
        val epoch = client.connectionEpoch
        val version = observationVersion.get()
        val result = try { capability(id) }
        catch (cancelled: CancellationException) { throw cancelled }
        catch (_: Exception) { ShellAvailability(reason = "capability_unavailable", sessionId = id) }
        if (selected.value == id && client.connectionEpoch == epoch && observationVersion.get() == version && ready()) availability.value = result
    }

    private suspend fun capability(id: String): ShellAvailability = replay.withControlAttachment(id, refresh = false) { epoch ->
        val body = client.request(RpcMethods.shellInventory(id), epoch)
        if (body.string("session_id") != id) throw RpcProtocolException("shell_session_mismatch")
        val value = body["shell"] as? JsonObject
            ?: return@withControlAttachment ShellAvailability(reason = "capability_unknown", sessionId = id)
        ShellAvailability(value.optionalBoolean("available") == true, value.optionalString("reason"), id,
            value.number("worker_generation"))
    }

    suspend fun start(session: String, submissionId: String, text: String, cwd: String?): ShellExecutionRef = submitMutex.withLock {
        require(submissionId.isNotBlank() && submissionId.length <= 128)
        require(text.isNotBlank() && text.toByteArray(Charsets.UTF_8).size <= 8192)
        require(cwd == null || (cwd.isNotEmpty() && !java.io.File(cwd).isAbsolute))
        val previous = submissions[submissionId]
        if (previous != null && (previous.session != session || previous.text != text || previous.cwd != cwd))
            throw IOException("submission_id_conflict")
        previous?.receipt?.let { return@withLock it }
        if (!ready()) throw IOException("disconnected")
        val pending = previous ?: run {
            if (submissions.size >= 128) {
                val projected = history.value
                submissions.entries.removeAll { (_, prior) ->
                    val receipt = prior.receipt ?: return@removeAll false
                    projected[receipt.sessionId].orEmpty().any {
                        it.ref == receipt && it.status !in setOf(ShellExecutionStatus.Running, ShellExecutionStatus.Reconnecting)
                    }
                }
            }
            if (submissions.size >= 128) throw IOException("shell_submission_limit")
            val capability = capability(session)
            if (!capability.available) throw IOException(capability.reason ?: "shell_unavailable")
            val generation = capability.workerGeneration ?: throw RpcProtocolException("shell_generation_missing")
            Submission(session, text, cwd, RpcMethods.shellExec(submissionId, SessionCoordinate(session, generation), text, cwd))
                .also { submissions[submissionId] = it }
        }
        observed.update { (it + session).toList().takeLast(32).toSet() }
        // A lost response leaves the exact command and original generation for an explicit retry.
        // Reconnect itself never calls this method or submits new work.
        val response = replay.withControlAttachment(session) { epoch -> client.request(pending.body, epoch) }
        if (response.string("session_id") != session) throw RpcProtocolException("shell_session_mismatch")
        ShellExecutionRef(session, submissionId, response.string("run_id"), response.string("item_id"),
            response.number("worker_generation")).also {
            pending.receipt = it
            // Mirror the observation the daemon itself would now report: the
            // session holds one nonterminal run, so `shell.inventory` says
            // session_busy until it settles (the lifecycle-change collector
            // re-observes then). This is a capability OBSERVATION, not a policy
            // denial — the terminal keeps rendering for it (FACADE-SHELL.md).
            if (selected.value == session) availability.value = ShellAvailability(reason = "session_busy", sessionId = session,
                workerGeneration = it.workerGeneration)
        }
    }

    suspend fun cancel(ref: ShellExecutionRef) {
        if (!ready()) throw IOException("disconnected")
        // Deterministic ID makes a response-loss retry the same cancellation.
        replay.withControlAttachment(ref.sessionId) { epoch ->
            client.request(RpcMethods.cancel("shell-cancel-${ref.commandId}",
                SessionCoordinate(ref.sessionId, ref.workerGeneration), ref.runId), epoch)
        }
    }

}

/** Only allowlisted cache projections enter the terminal; no raw CAS or tool arguments. */
internal object ShellProjection {
    const val MAX_COMMANDS = 64
    const val MAX_OUTPUT_BYTES = 256 * 1024
    fun project(session: String, entries: List<TranscriptCache.Entry>, caughtUp: Boolean): List<ShellExecution> {
        val commands = linkedMapOf<String, ShellExecution>()
        val sizes = mutableMapOf<String, Int>()
        val states = mutableMapOf<String, String>()
        val errors = mutableMapOf<String, String>()
        for (entry in entries) {
            val value = entry.display
            val run = value.optionalString("run_id") ?: continue
            when (value.optionalString("type")) {
                "shell_run_state" -> value.optionalString("state")?.let { states[run] = it }
                "run_failed" -> errors[run] = listOfNotNull(
                    value.optionalString("code"),
                    value.optionalString("provider_error_type"),
                    value.optionalNumber("provider_http_status")?.let { "HTTP $it" },
                    value.optionalString("provider_request_id")?.let { "Request ID: $it" },
                ).joinToString(" · ").ifEmpty { "run_failed" }
                "shell_item" -> {
                    val id = value.string("item_id")
                    val item = value["item"] as? JsonObject
                    if (item != null) {
                        val previous = commands[id]
                        // The daemon records an ordinary nonzero exit as a FAILED
                        // tool item that keeps its authoritative exit_code
                        // (process.rs ToolStatus::Failed). The terminal contract
                        // calls that Completed(exitCode); Error is reserved for
                        // an item that failed without one (FACADE-SHELL.md).
                        val status = when (item.string("status")) {
                            "completed" -> ShellExecutionStatus.Completed
                            "cancelled" -> ShellExecutionStatus.Cancelled
                            "failed" -> if (item.optionalNumber("exit_code") != null)
                                ShellExecutionStatus.Completed
                            else ShellExecutionStatus.Error
                            else -> ShellExecutionStatus.Running
                        }
                        commands[id] = ShellExecution(ShellExecutionRef(session, item.string("call_id"), run, id,
                            value.number("worker_generation")), item.string("command"), status,
                            previous?.output.orEmpty(), previous?.outputTruncated ?: false, item.optionalNumber("exit_code")?.toInt())
                        if (commands.size > MAX_COMMANDS) {
                            val oldest = commands.keys.first()
                            commands.remove(oldest); sizes.remove(oldest)
                        }
                    }
                    val output = value["output"] as? JsonObject
                    val previous = commands[id]
                    if (output != null && previous != null) {
                        val encoded = output.string("chunk_b64")
                        val size = Base64.getDecoder().decode(encoded).size
                        val total = (sizes[id] ?: 0) + size
                        sizes[id] = total
                        commands[id] = if (total > MAX_OUTPUT_BYTES) previous.copy(outputTruncated = true)
                        else previous.copy(output = previous.output + ShellOutput(entry.seq,
                            if (output.string("stream") == "stderr") ShellOutputStream.Stderr else ShellOutputStream.Stdout, encoded))
                    }
                }
            }
        }
        return commands.values.map { command ->
            val status = when (states[command.ref.runId]) {
                "cancelled" -> ShellExecutionStatus.Cancelled
                "errored" -> ShellExecutionStatus.Error
                // Completed item is the exit-code authority, not a coarse Done state alone.
                else -> command.status
            }
            command.copy(status = if (status == ShellExecutionStatus.Running && !caughtUp) ShellExecutionStatus.Reconnecting else status,
                error = errors[command.ref.runId])
        }
    }
}
