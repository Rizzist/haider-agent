package ai.diffforge.haider.ui.chat

/** One stable transcript turn. Only the live agent tail mutates while streaming. */
data class Message(
    val id: Long,
    val role: Role,
    val text: String,
    val thinking: String = "",
    val streaming: Boolean = false,
    val status: String? = null,
    val tools: List<ToolCall> = emptyList(),
    val error: String? = null,
    val provider: String? = null,
)

enum class Role { User, Agent }

data class ToolCall(
    val callId: String,
    val name: String,
    val summary: String,
    val status: ToolStatus,
    val result: String?,
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

enum class ConnectionState(val label: String) {
    Idle("idle"),
    Connecting("connecting"),
    Connected("connected"),
    Error("error"),
}
