package ai.diffforge.haider.transport.rpc

import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.Process
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.*
import java.io.Closeable
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.UUID
import java.util.concurrent.CopyOnWriteArrayList
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/** Data-plane state is independent of the Binder/native Ready state. */
enum class RpcConnectionState { DISCONNECTED, CONNECTING, CONNECTED, PROTOCOL_ERROR }

/** Copy these facts from Binder; never derive an endpoint from UI text. */
data class RpcTarget(val path: String, val wireProtocol: Int, val daemonGeneration: Long, val appVersion: String)
data class RpcWelcome(val instanceId: String, val daemonGeneration: Long, val frameLimit: Int,
    val daemonVersion: String, val capabilities: Set<String>, val features: Set<String>)

interface RpcSocket : Closeable {
    val input: InputStream
    val output: OutputStream
    fun connect(path: String)
    fun readTimeout(milliseconds: Int)
}

/** Same-UID authentication happens before Hello, and always in the filesystem namespace. */
class AndroidRpcSocket : RpcSocket {
    private val socket = LocalSocket()
    override val input get() = socket.inputStream
    override val output get() = socket.outputStream
    override fun connect(path: String) {
        if (!path.startsWith('/') || !path.endsWith("/h.sock") || '\u0000' in path ||
            path.toByteArray(Charsets.UTF_8).size > 107) throw RpcProtocolException("invalid_endpoint")
        socket.connect(LocalSocketAddress(path, LocalSocketAddress.Namespace.FILESYSTEM))
        if (socket.peerCredentials.uid != Process.myUid()) throw RpcProtocolException("peer_uid_mismatch")
    }
    override fun readTimeout(milliseconds: Int) { socket.soTimeout = milliseconds }
    override fun close() = socket.close()
}

/** A process-scoped connection. Activity background/rotation must not call close. */
class RpcClient(
    private val scope: CoroutineScope,
    private val socketFactory: () -> RpcSocket = ::AndroidRpcSocket,
    private val requestTimeoutMs: Long = 30_000,
) : Closeable {
    private val lock = Any()
    private val lifecycle = Mutex()
    private val writes = Mutex()
    private val listeners = CopyOnWriteArrayList<(JsonObject) -> Unit>()
    private val pending = mutableMapOf<String, Pending>()
    private val _state = MutableStateFlow(RpcConnectionState.DISCONNECTED)
    private val _welcome = MutableStateFlow<RpcWelcome?>(null)
    val state: StateFlow<RpcConnectionState> = _state.asStateFlow()
    val welcome: StateFlow<RpcWelcome?> = _welcome.asStateFlow()
    private var socket: RpcSocket? = null
    private var reader: Job? = null
    private var epoch = 0L
    val connectionEpoch: Long get() = synchronized(lock) { epoch }

    /** Callbacks run in read order on IO. They must not block awaiting an RPC response. */
    fun observeFrames(listener: (JsonObject) -> Unit): Closeable {
        listeners.add(listener)
        return Closeable { listeners.remove(listener) }
    }

    suspend fun connect(target: RpcTarget, control: Boolean = true) = lifecycle.withLock {
        close()
        if (target.wireProtocol != 1 || target.daemonGeneration <= 0) {
            _state.value = RpcConnectionState.PROTOCOL_ERROR
            throw RpcProtocolException("invalid_endpoint_version")
        }
        val next = socketFactory()
        synchronized(lock) { socket = next; _state.value = RpcConnectionState.CONNECTING }
        try {
            val granted = withTimeout(10_000) { socketIo(next) {
                next.connect(target.path)
                next.readTimeout(10_000)
                RpcWire.write(next.output, RpcWire.hello(target.appVersion, UUID.randomUUID().toString(), control))
                val frame = RpcWire.read(next.input) ?: throw IOException("handshake_eof")
                validateWelcome(frame, target, control).also { next.readTimeout(0) }
            } }
            synchronized(lock) {
                if (socket !== next) throw IOException("connection_replaced")
                _welcome.value = granted
                _state.value = RpcConnectionState.CONNECTED
            }
            reader = scope.launch(Dispatchers.IO) {
                try {
                    while (isActive) {
                        val frame = RpcWire.read(next.input, granted.frameLimit) ?: throw IOException("connection_eof")
                        if (synchronized(lock) { socket === next }) receive(frame)
                    }
                } catch (error: Exception) {
                    fail(next, error is RpcProtocolException)
                }
            }
        } catch (error: Exception) {
            fail(next, error is RpcProtocolException)
            throw error
        }
    }

    suspend fun request(body: JsonObject, expectedEpoch: Long? = null, onResponse: (JsonObject) -> Unit = {}): JsonObject =
        exchange(body.string("method"), expectedEpoch, onResponse) { id -> RpcWire.request(id, body) }

    suspend fun menuAnswer(answer: JsonObject, expectedEpoch: Long? = null): JsonObject = exchange("menu.answer", expectedEpoch, {}) { id ->
        JsonObject(answer + mapOf("v" to JsonPrimitive(1), "kind" to JsonPrimitive("menu_answer"), "request_id" to JsonPrimitive(id)))
    }

    private suspend fun exchange(method: String, expectedEpoch: Long?, onResponse: (JsonObject) -> Unit, frame: (String) -> JsonObject): JsonObject {
        val id = UUID.randomUUID().toString()
        val wait = CompletableDeferred<JsonObject>()
        val active = synchronized(lock) {
            if (_state.value != RpcConnectionState.CONNECTED || (expectedEpoch != null && expectedEpoch != epoch))
                throw IOException("connection_lost")
            if (pending.size >= 64) throw IOException("too_many_requests")
            pending[id] = Pending(method, wait, onResponse)
            socket ?: throw IOException("connection_lost")
        }
        try {
            return withTimeout(requestTimeoutMs) {
                writes.withLock {
                        val limit = synchronized(lock) {
                            if (socket !== active) throw IOException("connection_lost")
                            _welcome.value?.frameLimit ?: throw IOException("connection_lost")
                        }
                        socketIo(active) { RpcWire.write(active.output, frame(id), limit) }
                }
                wait.await()
            }
        } catch (cancelled: CancellationException) {
            fail(active, false)
            throw cancelled
        } catch (error: IOException) {
            if (error !is RpcRemoteException) fail(active, error is RpcProtocolException)
            throw error
        } finally { synchronized(lock) { pending.remove(id) } }
    }

    /** Cancellation must close a LocalSocket; coroutine cancellation alone cannot interrupt its streams. */
    private suspend fun <T> socketIo(active: RpcSocket, operation: () -> T): T = suspendCancellableCoroutine { continuation ->
        val work = scope.launch(Dispatchers.IO, start = CoroutineStart.LAZY) {
            try { continuation.resume(operation()) }
            catch (error: Exception) { continuation.resumeWithException(error) }
        }
        continuation.invokeOnCancellation { active.closeQuietly(); work.cancel() }
        work.invokeOnCompletion { error -> if (error != null && continuation.isActive) continuation.cancel(error) }
        work.start()
    }

    private fun receive(frame: JsonObject) {
        when (frame.string("kind")) {
            "response" -> {
                val item = synchronized(lock) { pending.remove(frame.string("request_id")) } ?: return // Late timed-out response.
                try {
                    val body = frame.objectAt("body")
                    when (body.string("method")) {
                        "error" -> item.reply.completeExceptionally(RpcRemoteException(body.string("code")))
                        item.method -> { item.onResponse(body); item.reply.complete(body) }
                        else -> throw RpcProtocolException("response_method_mismatch")
                    }
                } catch (error: Exception) { item.reply.completeExceptionally(error); throw error }
            }
            "protocol_error" -> throw RpcProtocolException("server_protocol_error")
            "hello", "welcome", "request" -> throw RpcProtocolException("unexpected_frame")
            else -> listeners.forEach { it(frame) }
        }
    }

    private fun fail(active: RpcSocket, protocol: Boolean) {
        synchronized(lock) {
            if (socket !== active) return
            socket = null
            epoch++
            _welcome.value = null
            _state.value = if (protocol) RpcConnectionState.PROTOCOL_ERROR else RpcConnectionState.DISCONNECTED
            pending.values.forEach { it.reply.completeExceptionally(IOException("connection_lost")) }
            pending.clear()
        }
        active.closeQuietly()
    }

    override fun close() {
        synchronized(lock) { socket }?.let { fail(it, false) }
        reader?.cancel()
        reader = null
    }

    private fun RpcSocket.closeQuietly() { try { close() } catch (_: IOException) { } }
    private data class Pending(val method: String, val reply: CompletableDeferred<JsonObject>, val onResponse: (JsonObject) -> Unit)

    companion object {
        fun validateWelcome(frame: JsonObject, target: RpcTarget, control: Boolean): RpcWelcome {
            if (frame.string("kind") != "welcome" || frame.number("protocol") != 1L ||
                frame.optionalString("encoding") !in listOf(null, "json")) throw RpcProtocolException("unsupported_negotiation")
            val limit = frame.number("frame_limit")
            val caps = frame.strings("capabilities_granted")
            val requested = if (control) setOf("view", "control") else setOf("view")
            if (limit !in 1..RpcWire.MAX_BODY.toLong() || caps != requested) throw RpcProtocolException("invalid_grant")
            if (frame.string("daemon_version") != target.appVersion || frame.number("daemon_generation") != target.daemonGeneration)
                throw RpcProtocolException("native_version_or_generation_mismatch")
            if (frame.string("profile_id") != "android-default") throw RpcProtocolException("profile_mismatch")
            return RpcWelcome(frame.string("instance_id"), target.daemonGeneration, limit.toInt(), target.appVersion, caps, frame.strings("features"))
        }
    }
}
