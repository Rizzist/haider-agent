package ai.diffforge.haider.transport

import org.json.JSONObject
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.EOFException
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets

/**
 * One item in a streamed chat reply. A `chat.send` request is followed by zero or more [Delta]
 * frames and exactly one terminal [Done] or [Error] frame with the same envelope ID. Chat does not
 * use single request/response correlation.
 */
sealed interface ChatReply {
    val id: Long

    data class Delta(
        override val id: Long,
        val text: String,
    ) : ChatReply

    data class Done(override val id: Long) : ChatReply

    data class Error(
        override val id: Long,
        val message: String,
    ) : ChatReply
}

/**
 * Reads and writes protocol frames shaped as a four-byte unsigned big-endian length followed by a
 * UTF-8 JSON envelope: `{\"id\": <long>, \"body\": {\"type\": <name>, ...}}`.
 */
object Frames {
    const val MAX_FRAME_BYTES: Int = 8 * 1024 * 1024

    data class Envelope(
        val id: Long,
        val body: JSONObject,
    )

    class ProtocolException(message: String) : IOException(message)

    /** Returns null only when EOF is reached before the next frame begins. */
    @Throws(IOException::class)
    fun read(input: InputStream): JSONObject? {
        val data = DataInputStream(input)
        val prefix = ByteArray(LENGTH_PREFIX_BYTES)
        val firstByte = input.read()
        if (firstByte == -1) return null
        prefix[0] = firstByte.toByte()
        try {
            data.readFully(prefix, 1, LENGTH_PREFIX_BYTES - 1)
        } catch (error: EOFException) {
            throw ProtocolException("Truncated frame header").also { it.initCause(error) }
        }
        val length = ByteBuffer.wrap(prefix).getInt().toLong() and 0xffff_ffffL
        if (length > MAX_FRAME_BYTES.toLong()) {
            throw ProtocolException("Frame exceeds the protocol limit")
        }
        if (length == 0L) {
            throw ProtocolException("Empty protocol frame")
        }

        val payload = ByteArray(length.toInt())
        try {
            data.readFully(payload)
        } catch (error: EOFException) {
            throw ProtocolException("Truncated protocol frame").also { it.initCause(error) }
        }

        return try {
            val json = StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(payload))
                .toString()
            JSONObject(json)
        } catch (error: Exception) {
            throw ProtocolException("Invalid JSON frame").also { it.initCause(error) }
        }
    }

    @Throws(IOException::class)
    fun write(output: OutputStream, json: JSONObject) {
        val payload = json.toString().toByteArray(StandardCharsets.UTF_8)
        if (payload.size > MAX_FRAME_BYTES) {
            throw ProtocolException("Frame exceeds the protocol limit")
        }
        val data = DataOutputStream(output)
        data.writeInt(payload.size)
        data.write(payload)
        data.flush()
    }

    fun envelope(id: Long, body: JSONObject): JSONObject = JSONObject()
        .put("id", id)
        .put("body", body)

    @Throws(ProtocolException::class)
    fun parseEnvelope(json: JSONObject): Envelope {
        return try {
            Envelope(
                id = json.getLong("id"),
                body = json.getJSONObject("body"),
            )
        } catch (error: Exception) {
            throw ProtocolException("Invalid protocol envelope").also { it.initCause(error) }
        }
    }

    fun hello(token: String, apkVersion: String = "0.0.1"): JSONObject = envelope(
        id = 1L,
        body = JSONObject()
            .put("type", "hello")
            .put("token", token)
            .put("apkVersion", apkVersion),
    )

    /** Builds `{"type":"chat.send","text":"..."}` for an APK-originated chat turn. */
    fun chatSend(text: String): JSONObject = JSONObject()
        .put("type", "chat.send")
        .put("text", text)

    fun isChat(body: JSONObject): Boolean = body.optString("type").startsWith(CHAT_TYPE_PREFIX)

    /**
     * Parses one streamed daemon reply body: `chat.delta` carries `text`, while `chat.done` is
     * terminal and `chat.error` is terminal with a `message`.
     */
    @Throws(ProtocolException::class)
    fun parseChatReply(envelope: Envelope): ChatReply {
        return try {
            when (val type = envelope.body.getString("type")) {
                "chat.delta" -> ChatReply.Delta(
                    id = envelope.id,
                    text = envelope.body.getString("text"),
                )
                "chat.done" -> ChatReply.Done(envelope.id)
                "chat.error" -> ChatReply.Error(
                    id = envelope.id,
                    message = envelope.body.getString("message"),
                )
                else -> throw ProtocolException("Unsupported chat frame type: $type")
            }
        } catch (error: ProtocolException) {
            throw error
        } catch (error: Exception) {
            throw ProtocolException("Invalid chat reply frame").also { it.initCause(error) }
        }
    }

    fun ack(ok: Boolean): JSONObject = JSONObject()
        .put("type", "ack")
        .put("ok", ok)

    fun rejected(reason: String): JSONObject = JSONObject()
        .put("type", "rejected")
        .put("reason", reason)

    fun error(reason: String): JSONObject = JSONObject()
        .put("type", "error")
        .put("reason", reason)

    private const val LENGTH_PREFIX_BYTES = 4
    private const val CHAT_TYPE_PREFIX = "chat."
}

/** Compares secret token bytes without returning early on content or length differences. */
fun constantTimeTokenEquals(expected: String, candidate: String): Boolean {
    val left = expected.toByteArray(StandardCharsets.UTF_8)
    val right = candidate.toByteArray(StandardCharsets.UTF_8)
    val longest = maxOf(left.size, right.size)
    var difference = left.size xor right.size
    for (index in 0 until longest) {
        val leftByte = if (index < left.size) left[index].toInt() else 0
        val rightByte = if (index < right.size) right[index].toInt() else 0
        difference = difference or (leftByte xor rightByte)
    }
    return difference == 0
}
