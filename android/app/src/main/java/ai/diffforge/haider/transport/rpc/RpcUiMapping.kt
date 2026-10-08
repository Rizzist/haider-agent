package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.ui.chat.Message
import ai.diffforge.haider.ui.chat.Role
import ai.diffforge.haider.ui.daemon.*
import kotlinx.serialization.json.*

internal object RpcUiMapping {
    fun dataPlane(state: RpcConnectionState) = when (state) {
        RpcConnectionState.DISCONNECTED -> DataPlaneState.Disconnected
        RpcConnectionState.CONNECTING -> DataPlaneState.Connecting
        RpcConnectionState.CONNECTED -> DataPlaneState.Connected
        RpcConnectionState.PROTOCOL_ERROR -> DataPlaneState.ProtocolError
    }
    fun target(snapshot: DaemonServiceSnapshot?): RpcTarget? {
        if (snapshot?.phase != DaemonPhase.Ready) return null
        val endpoint = snapshot.rpcEndpoint ?: return null
        if (endpoint.daemonGeneration != snapshot.daemonGeneration || endpoint.wireProtocol != snapshot.wireProtocol)
            throw RpcProtocolException("endpoint_snapshot_mismatch")
        return RpcTarget(endpoint.path, endpoint.wireProtocol, endpoint.daemonGeneration, snapshot.appVersion)
    }
    fun needsInput(value: JsonObject) = NeedsInput(value.string("kind"), value.string("title"),
        (value["safe_body"] as? JsonArray).orEmpty().map { (it as? JsonPrimitive)?.takeIf { line -> line.isString }?.content ?: throw RpcProtocolException("invalid_safe_body") }, value.optionalString("menu_id"), value.optionalNumber("request_seq"),
        value.optionalNumber("worker_generation"), value.optionalNumber("since_ms"),
        (value["options"] as? JsonArray).orEmpty().map {
            val option = it.jsonObject
            MenuOption(option.string("key"), option.string("label"), option.optionalString("detail"), option.optionalString("decision"))
        }, value["secret_answer"] == JsonPrimitive(true))
    fun row(summary: SessionSummary): SessionRow {
        val value = summary.canonical
        val needs = summary.needsInput?.let(::needsInput)
        val fork = (value["forked_from"] as? JsonObject)?.let { ForkProvenance(it.string("session_id"), it.number("seq")) }
        return SessionRow(id = summary.sessionId, title = summary.title,
            state = SessionVisualStateFold.fold(summary.runState, needs), runState = summary.runState,
            provider = summary.provider, model = summary.model, effort = value.optionalString("effort"),
            fast = value.optionalBoolean("fast"), agentType = value.optionalString("agent_type"),
            lastActivityMs = summary.lastActivityMs, createdAtMs = (value["metadata"] as? JsonObject)?.optionalNumber("created_at_ms"),
            seenAtMs = summary.seenAtMs, turnCount = value.optionalNumber("turn_count"),
            footprintTokens = value.optionalNumber("footprint_tokens"), footprintExact = when (value.optionalString("footprint_truth")) {
                "exact" -> true; "estimated" -> false; else -> null
            }, workspaceCwd = summary.workspaceCwd, forkedFrom = fork,
            parentSessionId = value.optionalString("parent_session_id"), kind = value.optionalString("kind"), needsInput = needs,
            runId = summary.runId, workerGeneration = summary.workerGeneration, headSeq = summary.headSeq)
    }
    /** Replace final items; only append deltas. Hidden/unrendered payloads never become messages. */
    fun messages(entries: List<TranscriptCache.Entry>): List<Message> {
        val messages = linkedMapOf<String, Message>()
        for (entry in entries) {
            val display = entry.display
            when (display.optionalString("type")) {
                "user_message" -> messages["user:${entry.seq}"] = Message(entry.seq, Role.User, display.optionalString("text").orEmpty())
                "history_node" -> {
                    val role = if (display.optionalString("kind") == "user_turn") Role.User else Role.Agent
                    val text = display.string("text")
                    // Current journals commit a node after its message/item. Older
                    // histories can contain only the node: retain that text once.
                    val previous = messages.values.lastOrNull()
                    if (previous == null || previous.role != role || previous.text != text)
                        messages["node:${entry.seq}"] = Message(entry.seq, role, text)
                }
                // The canonical cause, as its own card. The safe presentation
                // supplies the sentence a person needs and `code` the stable
                // reason a person reporting it needs; a journal that carried
                // neither still says the run failed rather than nothing at all
                // (971-V F7).
                "run_failed" -> {
                    val lines = listOfNotNull(
                        display.optionalString("title"),
                        display.optionalString("detail"),
                        display.optionalString("provider_error_type")?.let { "Provider error type: $it" },
                        display.optionalNumber("provider_http_status")?.let { "HTTP $it" },
                        display.optionalString("provider_request_id")?.let { "Request id: $it" },
                        display.optionalString("code"),
                    ).map(String::trim).filter(String::isNotEmpty).distinct()
                    messages["failed:${entry.seq}"] = Message(entry.seq, Role.Agent, "",
                        error = lines.joinToString(" · ").ifEmpty { "run_failed" },
                        errorRetryable = display.optionalBoolean("retryable") == true)
                }
                "item" -> {
                    val key = display.optionalString("item_id") ?: continue
                    val previous = messages[key] ?: Message(entry.seq, Role.Agent, "")
                    val item = display["item"] as? JsonObject
                    val delta = display["delta"] as? JsonObject
                    val thinking = item?.optionalString("item") == "reasoning" || delta?.optionalString("delta") == "reasoning"
                    val replacement = item?.let { it.optionalString("text") ?: it.optionalString("summary") ?: it.optionalString("reason") }
                    val text = replacement ?: ((if (thinking) previous.thinking else previous.text) + delta?.optionalString("text").orEmpty())
                    messages[key] = previous.copy(text = if (thinking) previous.text else text,
                        thinking = if (thinking) text else previous.thinking,
                        streaming = display.optionalString("event") != "completed")
                }
            }
        }
        return messages.values.toList()
    }
}

internal fun JsonObject.optionalBoolean(key: String): Boolean? =
    if (get(key) == null || get(key) == JsonNull) null else
        (get(key) as? JsonPrimitive)?.takeUnless { it.isString }?.booleanOrNull ?: throw RpcProtocolException("invalid_$key")
