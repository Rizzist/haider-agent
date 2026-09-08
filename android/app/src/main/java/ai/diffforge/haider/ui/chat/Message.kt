package ai.diffforge.haider.ui.chat

/** One stable transcript turn. Only the live agent tail mutates while streaming. */
data class Message(
    val id: Long,
    val role: Role,
    val text: String,
    val thinking: String = "",
    val streaming: Boolean = false,
    val tools: List<ToolCall> = emptyList(),
    val error: String? = null,
    /** `ChatReply.Error.retryable`; the 970 parser read it and threw it away. */
    val errorRetryable: Boolean = false,
    val provider: String? = null,
    /** `AttachmentBlock`s the turn carried: images, files, pasted text. */
    val attachments: List<ai.diffforge.haider.ui.daemon.Attachment> = emptyList(),
    /** Per-turn tokens, when `usage.report` attributed any to this turn. */
    val usage: ai.diffforge.haider.ui.daemon.TokenUsage? = null,
)

enum class Role { User, Agent }

data class ToolCall(
    val callId: String,
    val name: String,
    val summary: String,
    val status: ToolStatus,
    val result: String?,
    /**
     * How long the call took, when the daemon actually said so.
     *
     * S3 moved the duration off the trailing "— 41s" line and into the row;
     * round 6 deleted the line and never carried the number, so a finished call
     * showed no time at all (verify-6 O8). Null means the projection had no
     * duration — it is never computed from a clock the UI happens to own.
     */
    val durationMs: Long? = null,
)

enum class ToolStatus {
    Running,
    Completed,
    Failed,
    Rejected,
    Conflict,
    Cancelled,
    Unknown;

    val label: String
        get() = name.lowercase().replaceFirstChar { it.uppercase() }

    val needsAttention: Boolean
        get() = this in setOf(Failed, Rejected, Conflict, Unknown)

    companion object {
        fun fromWire(value: String): ToolStatus = when (value.lowercase()) {
            "running", "pending", "in_progress" -> Running
            "completed", "ok" -> Completed
            "failed", "error" -> Failed
            "rejected" -> Rejected
            "conflict" -> Conflict
            "cancelled", "canceled" -> Cancelled
            else -> Unknown
        }
    }
}
