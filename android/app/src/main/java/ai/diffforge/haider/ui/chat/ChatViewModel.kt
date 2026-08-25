package ai.diffforge.haider.ui.chat

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import ai.diffforge.haider.transport.ChatReply
import ai.diffforge.haider.transport.DaemonConnection
import ai.diffforge.haider.transport.TransportState
import kotlinx.coroutines.launch

/**
 * Holds transcript + connection state, bound to [DaemonConnection]: observes the
 * transport state, forwards a user turn via `sendChat`, and streams the daemon's
 * reply deltas (correlated by turn id) into a single agent bubble.
 */
class ChatViewModel : ViewModel() {
    private val _messages = mutableStateListOf<Message>()
    val messages: List<Message> get() = _messages

    var connection by mutableStateOf(ConnectionState.Idle)
        private set

    private var nextId = 1L
    private var currentTurnId: Long? = null
    private var streamingAgentId: Long? = null

    init {
        viewModelScope.launch {
            DaemonConnection.state.collect { st ->
                connection = when (st) {
                    TransportState.Disconnected -> ConnectionState.Idle
                    TransportState.Connecting -> ConnectionState.Connecting
                    TransportState.Authenticated -> ConnectionState.Connected
                    is TransportState.Error -> ConnectionState.Error
                }
            }
        }
        viewModelScope.launch {
            DaemonConnection.chatReplies.collect { reply ->
                if (reply.id != currentTurnId) return@collect
                when (reply) {
                    is ChatReply.Delta -> appendToAgent(reply.text)
                    is ChatReply.Done -> finishAgent()
                    is ChatReply.Error -> {
                        appendToAgent("\n[error: ${reply.message}]")
                        finishAgent()
                    }
                }
            }
        }
    }

    fun send(raw: String) {
        val text = raw.trim()
        if (text.isEmpty()) return
        _messages.add(Message(nextId++, Role.User, text))

        if (connection == ConnectionState.Connected) {
            val agentId = nextId++
            streamingAgentId = agentId
            _messages.add(Message(agentId, Role.Agent, "", streaming = true))
            viewModelScope.launch {
                try {
                    currentTurnId = DaemonConnection.sendChat(text)
                } catch (error: Exception) {
                    appendToAgent("[couldn't reach the daemon: ${error.message ?: "unknown error"}]")
                    finishAgent()
                }
            }
        } else {
            _messages.add(
                Message(
                    id = nextId++,
                    role = Role.Agent,
                    text = "Not connected to a daemon yet. Open setup (top-right) and point the app " +
                        "at your Termux haiderd — host, port, and token.",
                ),
            )
        }
    }

    private fun appendToAgent(delta: String) {
        val id = streamingAgentId ?: return
        val idx = _messages.indexOfLast { it.id == id }
        if (idx >= 0) {
            _messages[idx] = _messages[idx].copy(text = _messages[idx].text + delta)
        }
    }

    private fun finishAgent() {
        val id = streamingAgentId ?: return
        val idx = _messages.indexOfLast { it.id == id }
        if (idx >= 0) {
            _messages[idx] = _messages[idx].copy(streaming = false)
        }
        streamingAgentId = null
        currentTurnId = null
    }
}
