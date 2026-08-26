package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.transport.ChatReply
import ai.diffforge.haider.transport.ChatSegment
import ai.diffforge.haider.transport.DaemonConnection
import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.transport.TransportState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.launch

/** Transcript projection and the APK's correlation boundary for daemon chat streams. */
class ChatViewModel : ViewModel() {
    private val _messages = mutableStateListOf<Message>()
    val messages: List<Message> get() = _messages

    var connection by mutableStateOf(ConnectionState.Idle)
        private set
    var sessionConfig by mutableStateOf<SessionConfig?>(null)
        private set
    var settingsError by mutableStateOf<String?>(null)
        private set
    var selectionBusy by mutableStateOf(false)
        private set

    val isBusy: Boolean get() = _messages.any { it.role == Role.Agent && it.streaming }

    private var nextId = 1L
    private val turnAgents = mutableMapOf<Long, Long>()

    init {
        viewModelScope.launch {
            DaemonConnection.state.collect { state ->
                val previous = connection
                connection = when (state) {
                    TransportState.Disconnected -> ConnectionState.Idle
                    TransportState.Connecting -> ConnectionState.Connecting
                    TransportState.Authenticated -> ConnectionState.Connected
                    is TransportState.Error -> ConnectionState.Error
                }
                if (previous == ConnectionState.Connected && connection != ConnectionState.Connected) {
                    selectionBusy = false
                    sealInterruptedTurns()
                }
            }
        }
        viewModelScope.launch {
            DaemonConnection.sessionConfig.collect { config ->
                sessionConfig = config
                if (config != null) {
                    selectionBusy = false
                    settingsError = null
                }
            }
        }
        viewModelScope.launch {
            DaemonConnection.sessionErrors.collect { message ->
                selectionBusy = false
                settingsError = message
            }
        }
        viewModelScope.launch {
            DaemonConnection.chatReplies.collect(::applyReply)
        }
    }

    fun send(raw: String) {
        val text = raw.trim()
        if (text.isEmpty()) return
        _messages.add(Message(nextId++, Role.User, text))
        if (connection != ConnectionState.Connected) {
            _messages.add(
                Message(
                    id = nextId++,
                    role = Role.Agent,
                    text = "The daemon isn't connected. Open setup and check its host, port, and token.",
                    error = "Message not sent",
                ),
            )
            return
        }

        val agentId = nextId++
        _messages.add(
            Message(
                id = agentId,
                role = Role.Agent,
                text = "",
                streaming = true,
                status = "queued…",
                provider = sessionConfig?.current?.provider,
            ),
        )
        viewModelScope.launch {
            var reservedId: Long? = null
            try {
                DaemonConnection.sendChat(text) { turnId ->
                    reservedId = turnId
                    turnAgents[turnId] = agentId
                }
            } catch (error: Exception) {
                reservedId?.let(turnAgents::remove)
                mutateAgent(agentId) {
                    it.copy(
                        streaming = false,
                        status = null,
                        error = error.message ?: "Couldn't reach the daemon",
                    )
                }
            }
        }
    }

    fun selectModel(provider: String, model: String) {
        if (connection != ConnectionState.Connected || selectionBusy) return
        selectionBusy = true
        settingsError = null
        viewModelScope.launch {
            try {
                DaemonConnection.selectModel(provider, model)
            } catch (error: Exception) {
                selectionBusy = false
                settingsError = error.message ?: "Couldn't change the model"
            }
        }
    }

    fun selectEffort(effort: String?) {
        if (connection != ConnectionState.Connected || selectionBusy) return
        selectionBusy = true
        settingsError = null
        viewModelScope.launch {
            try {
                DaemonConnection.selectEffort(effort)
            } catch (error: Exception) {
                selectionBusy = false
                settingsError = error.message ?: "Couldn't change effort"
            }
        }
    }

    fun refreshSessionConfig() {
        if (connection != ConnectionState.Connected) return
        settingsError = null
        viewModelScope.launch {
            try {
                DaemonConnection.requestSessionConfig()
            } catch (error: Exception) {
                settingsError = error.message ?: "Couldn't refresh models"
            }
        }
    }

    private fun applyReply(reply: ChatReply) {
        val agentId = turnAgents[reply.id] ?: return
        when (reply) {
            is ChatReply.Delta -> mutateAgent(agentId) { message ->
                when (reply.segment) {
                    ChatSegment.Answer -> message.copy(text = message.text + reply.text)
                    ChatSegment.Thinking -> message.copy(thinking = message.thinking + reply.text)
                }
            }
            is ChatReply.Tool -> mutateAgent(agentId) { message ->
                val tool = ToolCall(
                    callId = reply.callId,
                    name = reply.name,
                    summary = reply.summary,
                    status = ToolStatus.fromWire(reply.status),
                    result = reply.result,
                )
                val existing = message.tools.indexOfFirst { it.callId == tool.callId }
                val tools = message.tools.toMutableList()
                if (existing >= 0) tools[existing] = tool else tools.add(tool)
                message.copy(tools = tools)
            }
            is ChatReply.Status -> mutateAgent(agentId) { it.copy(status = reply.text) }
            is ChatReply.Done -> {
                turnAgents.remove(reply.id)
                mutateAgent(agentId) { it.copy(streaming = false, status = null) }
            }
            is ChatReply.Error -> {
                turnAgents.remove(reply.id)
                mutateAgent(agentId) {
                    it.copy(streaming = false, status = null, error = reply.message)
                }
            }
        }
    }

    private fun mutateAgent(id: Long, transform: (Message) -> Message) {
        val index = _messages.indexOfLast { it.id == id }
        if (index >= 0) _messages[index] = transform(_messages[index])
    }

    private fun sealInterruptedTurns() {
        val ids = turnAgents.values.toSet()
        turnAgents.clear()
        ids.forEach { id ->
            mutateAgent(id) {
                it.copy(
                    streaming = false,
                    status = null,
                    error = "Connection lost before this reply completed",
                )
            }
        }
    }
}
