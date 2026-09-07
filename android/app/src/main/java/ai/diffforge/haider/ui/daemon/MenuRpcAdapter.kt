package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.ui.accounts.AccountsRpcAdapter
import org.json.JSONObject

/**
 * `WireFrame::MenuAnswer` (frame.rs:5926) is a **top-level frame**, not a
 * `RequestBody` method, and it carries the full compare-and-set identity:
 *
 * ```
 * {request_id?, command_id, session_id, menu_id, request_seq,
 *  worker_generation, option_key, option_index, input?}
 * ```
 *
 * `input` is `MenuInput` (frame.rs:5805): either `{kind:"text", text}` or
 * `{kind:"secret_vault_reference", vault_reference}`. **The raw secret must
 * never appear in this frame** — it goes through `vault.stage` with purpose
 * `menu_secret` and only its opaque reference travels here.
 */
object MenuRpcAdapter {
    const val KIND_MENU_ANSWER = "menu_answer"
    const val INPUT_TEXT = "text"
    const val INPUT_SECRET_VAULT_REFERENCE = "secret_vault_reference"

    fun menuAnswerFrame(
        coordinates: MenuCoordinates,
        optionKey: String,
        optionIndex: Int,
        input: MenuAnswerInput? = null,
        requestId: String? = null,
    ): JSONObject = JSONObject()
        .put("v", 1)
        .put("kind", KIND_MENU_ANSWER)
        .apply { if (requestId != null) put("request_id", requestId) }
        .put("command_id", coordinates.commandId)
        .put("session_id", coordinates.sessionId)
        .put("menu_id", coordinates.menuId)
        .put("request_seq", coordinates.requestSeq)
        .put("worker_generation", coordinates.workerGeneration)
        .put("option_key", optionKey)
        .put("option_index", optionIndex)
        .apply {
            when (input) {
                null -> Unit
                is MenuAnswerInput.Text -> put(
                    "input",
                    JSONObject().put("kind", INPUT_TEXT).put("text", input.text),
                )
                is MenuAnswerInput.Secret -> put(
                    "input",
                    JSONObject()
                        .put("kind", INPUT_SECRET_VAULT_REFERENCE)
                        .put("vault_reference", input.vaultReference),
                )
            }
        }

    /** The staging door a secret answer goes through first. */
    fun stageMenuSecretRequest(secret: CharArray, stageId: String) =
        AccountsRpcAdapter.stageMenuSecretRequest(secret, stageId)
}
