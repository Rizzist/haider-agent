package ai.diffforge.haider.transport

import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.asSharedFlow
import org.json.JSONObject

data class IncomingSms(
    val address: String,
    val body: String,
    val timestampMs: Long,
)

/** In-process bridge between the manifest SMS receiver and the active daemon transport. */
object SmsBus {
    private val _messages = MutableSharedFlow<IncomingSms>(
        extraBufferCapacity = 64,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val messages = _messages.asSharedFlow()

    fun publish(message: IncomingSms) {
        _messages.tryEmit(message)
    }

    fun toPush(message: IncomingSms): JSONObject = JSONObject()
        .put("type", "sms.incoming")
        .put("address", message.address)
        .put("body", message.body)
        .put("ts", message.timestampMs)
}
