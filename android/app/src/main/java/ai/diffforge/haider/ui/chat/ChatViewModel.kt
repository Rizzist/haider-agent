package ai.diffforge.haider.ui.chat

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel

/**
 * Holds transcript + connection state. Sending is a local placeholder until the
 * [ai.diffforge.haider.transport] client lands (APK-7 daemon half); at that point
 * [send] forwards the turn to the daemon and streams the reply back here.
 */
class ChatViewModel : ViewModel() {
    private val _messages = mutableStateListOf<Message>()
    val messages: List<Message> get() = _messages

    var connection by mutableStateOf(ConnectionState.Idle)
        private set

    private var nextId = 1L

    fun send(raw: String) {
        val text = raw.trim()
        if (text.isEmpty()) return
        _messages.add(Message(nextId++, Role.User, text))
        // Placeholder agent turn until the transport is wired. Kept honest: it
        // says it is not connected rather than faking a response.
        _messages.add(
            Message(
                id = nextId++,
                role = Role.Agent,
                text = if (connection == ConnectionState.Connected) {
                    "Received. (Daemon reply streaming will attach here once the transport lands.)"
                } else {
                    "Not connected to a daemon yet. Finish onboarding and point the app at your Termux haiderd."
                },
            ),
        )
    }

    fun setConnection(state: ConnectionState) {
        connection = state
    }
}
