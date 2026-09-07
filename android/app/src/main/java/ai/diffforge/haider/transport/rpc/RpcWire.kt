package ai.diffforge.haider.transport.rpc

import kotlinx.serialization.json.*
import java.io.*
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction

class RpcProtocolException(val code: String) : IOException(code)
class RpcRemoteException(val code: String, val retryable: Boolean = false) : IOException(code)

internal val wireJson = Json { isLenient = false; ignoreUnknownKeys = true }
internal fun obj(vararg fields: Pair<String, Any?>): JsonObject = buildJsonObject {
    fields.forEach { (key, value) ->
        if (value != null) put(key, when (value) {
            is JsonElement -> value
            is String -> JsonPrimitive(value)
            is Boolean -> JsonPrimitive(value)
            is Number -> JsonPrimitive(value)
            else -> error("Unsupported JSON value")
        })
    }
}
internal fun JsonObject.string(key: String): String =
    (get(key) as? JsonPrimitive)?.takeIf { it.isString }?.content
        ?: throw RpcProtocolException("invalid_$key")
internal fun JsonObject.optionalString(key: String): String? =
    if (get(key) == null || get(key) == JsonNull) null else string(key)
internal fun JsonObject.number(key: String): Long =
    (get(key) as? JsonPrimitive)?.takeUnless { it.isString }?.longOrNull
        ?.takeIf { it >= 0 } ?: throw RpcProtocolException("invalid_$key")
internal fun JsonObject.optionalNumber(key: String): Long? =
    if (get(key) == null || get(key) == JsonNull) null else number(key)
internal fun JsonObject.objectAt(key: String): JsonObject =
    get(key) as? JsonObject ?: throw RpcProtocolException("invalid_$key")
internal fun JsonObject.objects(key: String): List<JsonObject> =
    (get(key) as? JsonArray)?.map { it as? JsonObject ?: throw RpcProtocolException("invalid_$key") }
        ?: throw RpcProtocolException("invalid_$key")
internal fun JsonObject.strings(key: String): Set<String> =
    (get(key) as? JsonArray)?.map { (it as? JsonPrimitive)?.takeIf { p -> p.isString }?.content
        ?: throw RpcProtocolException("invalid_$key") }?.toSet() ?: emptySet()

/** The Rust uds_codec.rs framing: u32 big endian length, then compact UTF-8 WireFrame JSON. */
object RpcWire {
    const val MAX_BODY = 8_388_608

    fun parse(body: String): JsonObject {
        val frame = try { wireJson.parseToJsonElement(body) as? JsonObject }
            catch (_: Exception) { null } ?: throw RpcProtocolException("invalid_json")
        if (frame.number("v") != 1L) throw RpcProtocolException("unsupported_protocol")
        frame.string("kind") // Unknown additive kinds remain intact.
        return frame
    }

    fun read(input: InputStream, limit: Int = MAX_BODY): JsonObject? {
        require(limit in 1..MAX_BODY)
        val first = input.read()
        if (first < 0) return null
        val header = ByteArray(4)
        header[0] = first.toByte()
        val data = DataInputStream(input)
        try { data.readFully(header, 1, 3) }
        catch (_: EOFException) { throw RpcProtocolException("truncated_header") }
        val length = ByteBuffer.wrap(header).int.toLong() and 0xffffffffL
        if (length !in 1..limit.toLong()) throw RpcProtocolException("frame_limit")
        val bytes = ByteArray(length.toInt())
        try { data.readFully(bytes) }
        catch (_: EOFException) { throw RpcProtocolException("truncated_body") }
        val text = try {
            Charsets.UTF_8.newDecoder().onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT).decode(ByteBuffer.wrap(bytes)).toString()
        } catch (_: Exception) { throw RpcProtocolException("invalid_utf8") }
        return parse(text)
    }

    fun write(output: OutputStream, frame: JsonObject, limit: Int = MAX_BODY) {
        require(limit in 1..MAX_BODY)
        val bytes = frame.toString().toByteArray(Charsets.UTF_8)
        if (bytes.size !in 1..limit) throw RpcProtocolException("frame_limit")
        DataOutputStream(output).apply { writeInt(bytes.size); write(bytes); flush() }
    }

    fun nonce(frame: JsonObject): JsonPrimitive = (frame["nonce"] as? JsonPrimitive)
        ?.takeIf { !it.isString && it.content.toULongOrNull() != null }
        ?: throw RpcProtocolException("invalid_nonce")
    fun ping(nonce: JsonPrimitive) = obj("v" to 1, "kind" to "ping", "nonce" to nonce)
    fun pong(nonce: JsonPrimitive) = obj("v" to 1, "kind" to "pong", "nonce" to nonce)

    fun request(id: String, body: JsonObject) = obj("v" to 1, "kind" to "request", "request_id" to id, "body" to body)
    fun hello(version: String, instance: String, control: Boolean) = obj(
        "v" to 1, "kind" to "hello", "protocol_min" to 1, "protocol_max" to 1,
        "client_name" to "haider-android", "client_version" to version,
        "client_instance_id" to instance, "client_kind" to "gui",
        "capabilities_requested" to JsonArray((if (control) listOf("view", "control") else listOf("view")).map(::JsonPrimitive)),
        "max_receive_frame" to MAX_BODY, "encodings" to JsonArray(listOf(JsonPrimitive("json"))),
    )
}
