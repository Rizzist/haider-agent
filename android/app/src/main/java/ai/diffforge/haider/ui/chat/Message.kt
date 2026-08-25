package ai.diffforge.haider.ui.chat

/** One transcript row. Agent prose renders full-width in chat-text; user turns
 *  render as a right-aligned pill card (Session Deck semantics). */
data class Message(
    val id: Long,
    val role: Role,
    val text: String,
    val streaming: Boolean = false,
    val tools: List<ToolCall> = emptyList(),
)

enum class Role { User, Agent }

/** A tool cluster row. Per the Session Deck rule, an UNKNOWN status never reads
 *  as green — it stays muted until the harness reports a terminal state. */
data class ToolCall(
    val name: String,
    val summary: String,
    val status: ToolStatus,
)

enum class ToolStatus { Running, Ok, Error, Unknown }

enum class ConnectionState(val label: String) {
    Idle("idle"),
    Connecting("connecting"),
    Connected("connected"),
    Error("error"),
}
