package ai.diffforge.haider.transport.rpc

import kotlinx.serialization.json.*

/** Success projections shared by repositories, the UI facade and canonical-fixture tests. */
object RpcResponses {
    data class RosterPage(val sessions: List<SessionSummary>, val nextCursor: String?)
    data class ProviderInventory(val providers: List<ProviderDescriptor>, val revision: Long)
    data class Attachment(val id: String, val sessionId: String, val replayThrough: Long, val workerGeneration: Long)
    data class ReadPage(val sessionId: String, val headSeq: Long, val envelopes: List<JsonObject>)
    data class ForkCut(val session: SessionCoordinate, val nodeId: String?, val seq: Long)
    data class Receipt(val sessionId: String, val workerGeneration: Long?, val seq: Long?, val runId: String? = null)
    data class Stage(val reference: String, val expiresAtMs: Long) {
        override fun toString() = "Stage(redacted)"
    }

    fun watch(body: JsonObject): Boolean = (body["accepted"] == JsonPrimitive(true)).also {
        if (!it) throw RpcProtocolException("watch_rejected")
    }
    fun roster(body: JsonObject) = RosterPage(body.objects("sessions").map(SessionSummary::parse), body.optionalString("next_cursor"))
    fun providers(body: JsonObject): ProviderInventory {
        AccountsRepository.checkAvailability(body)
        return ProviderInventory(body.objects("providers").map(AccountsRepository::parseProvider), body.number("revision"))
    }
    fun accounts(body: JsonObject): AccountsSnapshot {
        AccountsRepository.checkAvailability(body)
        return AccountsSnapshot(body.optionalNumber("revision"), body.objects("descriptors").map(AccountsRepository::parseAccount))
    }
    fun descriptor(body: JsonObject) = AccountsRepository.parseAccount(body.objectAt("descriptor"))
    fun refreshedProvider(body: JsonObject) = AccountsRepository.parseProvider(body.objectAt("provider"))
    fun removed(body: JsonObject): String = body.string("removed_alias")
    fun stagedReference(body: JsonObject): String = body.string("vault_reference")
    fun stage(body: JsonObject) = Stage(stagedReference(body), body.number("expires_at_ms"))
    fun attachment(body: JsonObject): Attachment {
        val state = body.objectAt("attach_state")
        return Attachment(body.string("attachment_id"), state.string("session_id"), state.number("replay_through_seq"), state.number("worker_generation"))
    }
    fun detached(body: JsonObject): String = body.string("attachment_id")
    fun read(body: JsonObject): ReadPage {
        val result = body.objectAt("result")
        return ReadPage(result.string("session_id"), result.number("head_seq"), result.objects("envelopes"))
    }
    fun observe(body: JsonObject): JsonObject = body.objectAt("digest").also {
        it.string("session_id"); it.number("head_seq"); it.number("worker_generation")
    }
    fun forkCut(digest: JsonObject) = ForkCut(SessionCoordinate(digest.string("session_id"), digest.number("worker_generation")),
        digest.optionalString("main_head_node_id"), digest.number("main_head_seq"))
    fun receipt(body: JsonObject): Receipt {
        val field = when (body.string("method")) {
            "session.create", "session.fork" -> "created_seq"
            "session.rename" -> "renamed_seq"
            "session.seen" -> "seen_seq"
            "session.select_model", "session.select_effort" -> "selected_seq"
            "turn.submit" -> "accepted_seq"
            "turn.cancel" -> null
            else -> throw RpcProtocolException("unexpected_receipt")
        }
        return Receipt(body.string("session_id"), if (field != null) body.number("worker_generation") else null,
            field?.let(body::number), body.optionalString("run_id"))
    }
    fun menuAnswer(body: JsonObject): Long = body.number("resolution_seq")
    fun oauthStart(body: JsonObject, provider: String, alias: String, attempt: String, epoch: Long): OAuthFlow {
        if (body.objectAt("availability")["available"] != JsonPrimitive(true)) throw RpcRemoteException("oauth_unavailable")
        return OAuthFlow(provider, alias, body.string("flow_id"), attempt, body.optionalString("authorization_url"),
            body.optionalString("user_code"), body.optionalNumber("expires_at_ms"), epoch)
    }
    fun oauthStatus(status: JsonObject): OAuthStatus {
        val kind = status.string("status")
        return OAuthStatus(kind, if (kind == "ready") status.string("oauth_reference") else null,
            status.optionalString("identity"), status.optionalString("public_code"))
    }
}
