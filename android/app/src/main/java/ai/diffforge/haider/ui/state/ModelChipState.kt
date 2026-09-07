package ai.diffforge.haider.ui.state

import ai.diffforge.haider.daemon.DaemonStatus
import ai.diffforge.haider.transport.SessionConfig

/**
 * The five states the model chip may occupy (UI-SPEC 3.6).
 *
 * The 970 chip printed `loading…` whenever the label was null, so with no saved
 * endpoint it sat there forever (D4). Here a request has a deadline: after
 * [DEADLINE_MS] with nothing back the chip says *Retry*, and with no daemon it
 * says *Start Haider first*. The two failures the old chip conflated are now
 * two different sentences.
 */
sealed interface ModelChipState {
    data class Resolved(
        val shortModel: String,
        val fullModel: String,
        val provider: String,
        val effort: String?,
    ) : ModelChipState

    data object Loading : ModelChipState
    data class Error(val message: String?) : ModelChipState
    data object DaemonDown : ModelChipState
    data object Changing : ModelChipState
}

object ModelChipStateMachine {
    /** The chip is never allowed to sit on "loading…". */
    const val DEADLINE_MS: Long = 6_000

    fun resolve(
        daemon: DaemonStatus,
        config: SessionConfig?,
        catalogError: String?,
        selectionBusy: Boolean,
        requestedAtMs: Long?,
        nowMs: Long,
    ): ModelChipState {
        // A late success after the deadline still resolves.
        if (config != null && !selectionBusy) {
            return ModelChipState.Resolved(
                shortModel = ModelNames.short(config.current.model),
                fullModel = config.current.model,
                provider = config.current.provider,
                effort = config.current.effort,
            )
        }
        if (selectionBusy) return ModelChipState.Changing
        if (catalogError != null) return ModelChipState.Error(catalogError)
        if (daemon !is DaemonStatus.Running) return ModelChipState.DaemonDown
        if (requestedAtMs == null) return ModelChipState.Loading
        return if (nowMs - requestedAtMs >= DEADLINE_MS) {
            ModelChipState.Error(null)
        } else {
            ModelChipState.Loading
        }
    }
}

/**
 * Display names: strip the provider prefix and the date suffix, keep the full id
 * for `contentDescription` and the drawer footer (UI-SPEC 3.6).
 * `claude-sonnet-4-5` -> `Sonnet 4.5`.
 */
object ModelNames {
    private val vendorPrefixes = setOf(
        "claude", "gemini", "deepseek", "qwen", "kimi", "moonshot",
        "mistral", "llama", "meta", "anthropic", "openai", "google",
    )
    private val upperCased = mapOf("gpt" to "GPT", "glm" to "GLM", "ai" to "AI", "o" to "o")
    private val dateSuffix = Regex("-(\\d{8}|\\d{4}-\\d{2}-\\d{2}|latest|preview)$")

    fun short(model: String?): String {
        if (model.isNullOrBlank()) return ""
        val bare = model.substringAfterLast('/').substringAfterLast(':')
        val withoutDate = dateSuffix.replace(bare.lowercase(), "")
        val tokens = withoutDate.split('-', '_').filter { it.isNotBlank() }
        if (tokens.isEmpty()) return bare
        val meaningful = if (tokens.size > 1 && tokens.first() in vendorPrefixes) {
            tokens.drop(1)
        } else {
            tokens
        }
        val out = StringBuilder()
        var index = 0
        while (index < meaningful.size) {
            val token = meaningful[index]
            if (token.all(Char::isDigit)) {
                val numbers = mutableListOf(token)
                var next = index + 1
                while (next < meaningful.size && meaningful[next].all(Char::isDigit)) {
                    numbers += meaningful[next]
                    next++
                }
                if (out.isNotEmpty()) out.append(' ')
                out.append(numbers.joinToString("."))
                index = next
            } else {
                if (out.isNotEmpty()) out.append(' ')
                out.append(upperCased[token] ?: token.replaceFirstChar(Char::uppercase))
                index++
            }
        }
        return out.toString()
    }

    /** `anthropic / claude-sonnet-4-5 · high`, for secondary lines. */
    fun full(provider: String?, model: String?, effort: String?): String {
        val head = listOfNotNull(provider, model).joinToString(" / ")
        return if (effort.isNullOrBlank()) head else "$head · $effort"
    }

    /** `18.4k`, for the context-usage readout. Absent stays absent. */
    fun tokens(count: Long?): String? = when {
        count == null -> null
        count >= 1_000_000 -> String.format("%.1fM", count / 1_000_000.0)
        count >= 1_000 -> String.format("%.1fk", count / 1_000.0)
        else -> count.toString()
    }
}
