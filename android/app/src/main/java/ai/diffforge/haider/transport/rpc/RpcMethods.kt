package ai.diffforge.haider.transport.rpc

import kotlinx.serialization.json.*

/** Wire coordinates are copied together from one authoritative snapshot. */
data class SessionCoordinate(val sessionId: String, val workerGeneration: Long)
data class MenuCoordinate(val session: SessionCoordinate, val menuId: String, val requestSeq: Long,
    val optionKey: String, val optionIndex: Int)

/** Pure encoders; semantic command IDs belong to the caller and survive response-loss retries. */
object RpcMethods {
    fun list(cursor: String? = null, limit: Int = 64, recency: Boolean = false): JsonObject {
        require(limit in 1..1024)
        return obj("method" to "session.list", "cursor" to cursor, "limit" to limit,
            "order" to if (recency) "recency_desc" else null)
    }
    fun watchSessions() = obj("method" to "session.list_watch")
    fun observe(session: String, lastEventLimit: Int = 20) = obj("method" to "session.observe",
        "session_id" to session, "last_event_limit" to lastEventLimit)
    fun read(session: String, start: Long, end: Long): JsonObject {
        require(start >= 1 && end >= start && end - start < 1024)
        return obj("method" to "session.read", "session_id" to session, "range" to obj("start_seq" to start, "end_seq" to end))
    }
    fun attach(session: String, after: Long = 0, control: Boolean = false) = obj(
        "method" to "session.attach", "session_id" to session, "after_seq" to after, "mode" to if (control) "control" else "view")
    fun detach(attachment: String) = obj("method" to "session.detach", "attachment_id" to attachment)
    fun create(command: String, cwd: String, provider: String, model: String, maxTokens: Long) = obj(
        "method" to "session.create", "command_id" to command, "cwd" to cwd, "provider" to provider,
        "model" to model, "max_tokens" to maxTokens)
    fun submit(command: String, at: SessionCoordinate, text: String, mode: String = "queue", branch: String? = null) =
        command("turn.submit", command, at, "text" to text, "mode" to mode,
            "attachments" to JsonArray(emptyList()), "branch_id" to branch)
    fun cancel(command: String, at: SessionCoordinate, run: String) =
        command("turn.cancel", command, at, "run_id" to run)
    fun rename(command: String, at: SessionCoordinate, title: String?) = command("session.rename", command, at, "title" to title)
    fun seen(command: String, at: SessionCoordinate) = command("session.seen", command, at)
    fun selectModel(command: String, at: SessionCoordinate, provider: String, model: String, confirmNewEpoch: Boolean = false) =
        command("session.select_model", command, at, "provider" to provider, "model" to model, "confirm_new_epoch" to if (confirmNewEpoch) true else null)
    fun selectEffort(command: String, at: SessionCoordinate, effort: String?, confirmNewEpoch: Boolean = false) =
        command("session.select_effort", command, at, "effort" to effort, "confirm_new_epoch" to if (confirmNewEpoch) true else null)
    fun fork(command: String, at: SessionCoordinate, node: String, seq: Long, name: String? = null) =
        command("session.fork", command, at, "fork_node_id" to node, "fork_seq" to seq, "name" to name)
    fun menu(command: String, at: MenuCoordinate, input: JsonObject? = null) = obj(
        "command_id" to command, "session_id" to at.session.sessionId, "worker_generation" to at.session.workerGeneration,
        "menu_id" to at.menuId, "request_seq" to at.requestSeq, "option_key" to at.optionKey,
        "option_index" to at.optionIndex, "input" to input)
    fun textInput(text: String) = obj("kind" to "text", "text" to text)
    fun secretInput(reference: String) = obj("kind" to "secret_vault_reference", "vault_reference" to reference)
    fun providers() = obj("method" to "provider.list")
    fun accounts() = obj("method" to "account.list")
    fun watchAccounts() = obj("method" to "account.list_watch")
    fun refreshAccount(alias: String) = obj("method" to "account.refresh", "alias" to alias)
    fun refreshModels(provider: String) = obj("method" to "provider.models_refresh", "provider" to provider)
    fun stage(stage: String, purpose: String, secret: String): JsonObject {
        require(purpose in setOf("api_key", "menu_secret"))
        return obj("method" to "vault.stage", "stage_id" to stage, "purpose" to purpose, "secret" to secret)
    }
    fun loginApi(command: String, provider: String, alias: String?, reference: String,
        validationModel: String? = null, replace: Boolean = false) = obj("method" to "account.login_api",
        "command_id" to command, "provider" to provider, "alias" to alias, "vault_reference" to reference,
        "validation_model" to validationModel, "replace_existing" to if (replace) true else null)
    fun oauthStart(provider: String, alias: String, attempt: String) = obj("method" to "account.oauth_start",
        "provider" to provider, "desired_alias" to alias, "attempt_id" to attempt)
    fun oauthStatus(flow: String, attempt: String) = obj("method" to "account.oauth_status", "flow_id" to flow, "attempt_id" to attempt)
    fun oauthCancel(flow: String, attempt: String) = obj("method" to "account.oauth_cancel", "flow_id" to flow, "attempt_id" to attempt)
    fun addOAuth(command: String, provider: String, alias: String, flow: String, attempt: String, reference: String) =
        obj("method" to "account.add", "command_id" to command, "provider" to provider, "alias" to alias,
            "auth_method" to "oauth", "flow_id" to flow, "attempt_id" to attempt, "oauth_reference" to reference)
    fun remove(command: String, alias: String, revision: Long?) = obj("method" to "account.remove", "command_id" to command,
        "alias" to alias, "expected_revision" to revision)
    fun setActive(command: String, alias: String, confirmNewEpoch: Boolean = false) = obj("method" to "account.set_active",
        "command_id" to command, "alias" to alias, "confirm_new_epoch" to if (confirmNewEpoch) true else null)
    fun setDefaultModel(command: String, provider: String, model: String, revision: Long) = obj(
        "method" to "account.set_default_model", "command_id" to command, "provider" to provider, "model" to model, "expected_revision" to revision)
    fun setLabel(alias: String, label: String?) = obj("method" to "account.set_label", "alias" to alias, "label" to label)
    private fun command(method: String, id: String, at: SessionCoordinate, vararg fields: Pair<String, Any?>) =
        obj("method" to method, "command_id" to id, "session_id" to at.sessionId, "worker_generation" to at.workerGeneration, *fields)
}
