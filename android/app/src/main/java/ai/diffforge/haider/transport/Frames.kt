package ai.diffforge.haider.transport

import org.json.JSONArray
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

enum class ChatSegment { Answer, Thinking }

sealed interface ChatReply {
    val id: Long

    data class Delta(
        override val id: Long,
        val text: String,
        val segment: ChatSegment,
    ) : ChatReply

    data class Tool(
        override val id: Long,
        val callId: String,
        val name: String,
        val summary: String,
        val status: String,
        val result: String?,
    ) : ChatReply

    data class Status(override val id: Long, val text: String) : ChatReply
    data class Done(override val id: Long) : ChatReply
    data class Error(
        override val id: Long,
        val code: String,
        val message: String,
        val retryable: Boolean,
    ) : ChatReply
}

data class SessionSelection(
    val sessionId: String,
    val provider: String,
    val model: String,
    val effort: String?,
)

data class SessionModel(
    val id: String,
    val contextWindow: Long?,
    val supportedEfforts: List<String>,
    val defaultEffort: String?,
)

data class SessionProvider(
    val id: String,
    val enabled: Boolean,
    val availability: String,
    val availabilityReason: String?,
    val defaultModel: String?,
    val models: List<SessionModel>,
)

data class SessionConfig(
    val catalogRevision: Long,
    val catalogAvailable: Boolean,
    val unavailableReason: String?,
    val current: SessionSelection,
    val providers: List<SessionProvider>,
)

sealed interface SessionReply {
    data class Config(val value: SessionConfig) : SessionReply
    data class Error(val code: String, val message: String, val retryable: Boolean) : SessionReply
}

/** Four-byte unsigned big-endian length followed by a UTF-8 JSON envelope. */
object Frames {
    const val MAX_FRAME_BYTES: Int = 8 * 1024 * 1024

    data class Envelope(val id: Long, val body: JSONObject)

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
        if (length == 0L) throw ProtocolException("Empty protocol frame")

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
        if (payload.size > MAX_FRAME_BYTES) throw ProtocolException("Frame exceeds the protocol limit")
        val data = DataOutputStream(output)
        data.writeInt(payload.size)
        data.write(payload)
        data.flush()
    }

    fun envelope(id: Long, body: JSONObject): JSONObject = JSONObject()
        .put("id", id)
        .put("body", body)

    @Throws(ProtocolException::class)
    fun parseEnvelope(json: JSONObject): Envelope = try {
        Envelope(id = json.getLong("id"), body = json.getJSONObject("body"))
    } catch (error: Exception) {
        throw ProtocolException("Invalid protocol envelope").also { it.initCause(error) }
    }

    fun hello(token: String, apkVersion: String = "0.0.1"): JSONObject = envelope(
        id = 1L,
        body = JSONObject()
            .put("type", "hello")
            .put("token", token)
            .put("apkVersion", apkVersion),
    )

    fun chatSend(text: String): JSONObject = JSONObject()
        .put("type", "chat.send")
        .put("text", text)

    fun sessionConfigGet(): JSONObject = JSONObject().put("type", "session.config.get")

    fun sessionSelectModel(provider: String, model: String): JSONObject = JSONObject()
        .put("type", "session.select_model")
        .put("provider", provider)
        .put("model", model)
        .put("confirmNewEpoch", true)

    fun sessionSelectEffort(effort: String?): JSONObject = JSONObject()
        .put("type", "session.select_effort")
        .put("effort", effort ?: JSONObject.NULL)
        .put("confirmNewEpoch", true)

    fun isChatReply(body: JSONObject): Boolean = when (body.optString("type")) {
        "chat.delta", "chat.tool", "chat.status", "chat.done", "chat.error" -> true
        else -> false
    }

    fun isSessionReply(body: JSONObject): Boolean = when (body.optString("type")) {
        "session.config", "session.error" -> true
        else -> false
    }

    @Throws(ProtocolException::class)
    fun parseChatReply(envelope: Envelope): ChatReply = try {
        when (val type = envelope.body.getString("type")) {
            "chat.delta" -> ChatReply.Delta(
                id = envelope.id,
                text = envelope.body.getString("text"),
                segment = when (envelope.body.optString("segment", "answer")) {
                    "answer" -> ChatSegment.Answer
                    "thinking" -> ChatSegment.Thinking
                    else -> throw ProtocolException("Unsupported chat delta segment")
                },
            )
            "chat.tool" -> ChatReply.Tool(
                id = envelope.id,
                callId = envelope.body.getString("callId"),
                name = envelope.body.getString("name"),
                summary = envelope.body.optString("summary"),
                status = envelope.body.optString("status", "unknown"),
                result = envelope.body.nullableString("result"),
            )
            "chat.status" -> ChatReply.Status(envelope.id, envelope.body.getString("text"))
            "chat.done" -> ChatReply.Done(envelope.id)
            "chat.error" -> ChatReply.Error(
                id = envelope.id,
                code = envelope.body.optString("code", "turn_failed"),
                message = envelope.body.getString("message"),
                retryable = envelope.body.optBoolean("retryable", false),
            )
            else -> throw ProtocolException("Unsupported chat frame type: $type")
        }
    } catch (error: ProtocolException) {
        throw error
    } catch (error: Exception) {
        throw ProtocolException("Invalid chat reply frame").also { it.initCause(error) }
    }

    @Throws(ProtocolException::class)
    fun parseSessionReply(envelope: Envelope): SessionReply = try {
        when (val type = envelope.body.getString("type")) {
            "session.config" -> SessionReply.Config(parseSessionConfig(envelope.body))
            "session.error" -> SessionReply.Error(
                code = envelope.body.optString("code", "session_error"),
                message = envelope.body.getString("message"),
                retryable = envelope.body.optBoolean("retryable", false),
            )
            else -> throw ProtocolException("Unsupported session frame type: $type")
        }
    } catch (error: ProtocolException) {
        throw error
    } catch (error: Exception) {
        throw ProtocolException("Invalid session reply frame").also { it.initCause(error) }
    }

    private fun parseSessionConfig(body: JSONObject): SessionConfig {
        val current = body.getJSONObject("current")
        return SessionConfig(
            catalogRevision = body.getLong("catalogRevision"),
            catalogAvailable = body.getBoolean("catalogAvailable"),
            unavailableReason = body.nullableString("unavailableReason"),
            current = SessionSelection(
                sessionId = current.getString("sessionId"),
                provider = current.getString("provider"),
                model = current.getString("model"),
                effort = current.nullableString("effort"),
            ),
            providers = body.getJSONArray("providers").mapObjects { provider ->
                SessionProvider(
                    id = provider.getString("id"),
                    enabled = provider.getBoolean("enabled"),
                    availability = provider.optString("availability", "unknown"),
                    availabilityReason = provider.nullableString("availabilityReason"),
                    defaultModel = provider.nullableString("defaultModel"),
                    models = provider.getJSONArray("models").mapObjects { model ->
                        SessionModel(
                            id = model.getString("id"),
                            contextWindow = model.nullableLong("contextWindow"),
                            supportedEfforts = model.getJSONArray("supportedEfforts").mapStrings(),
                            defaultEffort = model.nullableString("defaultEffort"),
                        )
                    },
                )
            },
        )
    }

    fun ack(ok: Boolean): JSONObject = JSONObject().put("type", "ack").put("ok", ok)
    fun rejected(reason: String): JSONObject = JSONObject().put("type", "rejected").put("reason", reason)
    fun error(reason: String): JSONObject = JSONObject().put("type", "error").put("reason", reason)

    private fun JSONObject.nullableString(name: String): String? =
        if (!has(name) || isNull(name)) null else getString(name)

    private fun JSONObject.nullableLong(name: String): Long? =
        if (!has(name) || isNull(name)) null else getLong(name)

    private fun JSONArray.mapStrings(): List<String> =
        List(length()) { index -> getString(index) }

    private fun <T> JSONArray.mapObjects(transform: (JSONObject) -> T): List<T> =
        List(length()) { index -> transform(getJSONObject(index)) }

    private const val LENGTH_PREFIX_BYTES = 4
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
