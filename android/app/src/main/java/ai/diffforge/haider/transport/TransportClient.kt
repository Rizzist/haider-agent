package ai.diffforge.haider.transport

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.IOException
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

sealed interface TransportState {
    data object Disconnected : TransportState
    data object Connecting : TransportState
    data object Authenticated : TransportState

    data class Error(val kind: Failure) : TransportState

    enum class Failure {
        AUTH_REJECTED,
        PROTOCOL,
        IO,
    }
}

interface CapabilityHandler {
    suspend fun handle(body: JSONObject): JSONObject?
}

/** Safe linker default used until Android-backed capabilities are injected. */
class NoDeviceHandler : CapabilityHandler {
    override suspend fun handle(body: JSONObject): JSONObject = Frames.ack(ok = false)
}

/**
 * Persistent client for envelope-framed daemon requests and APK pushes. Calls [start] to connect.
 * Outbound pushes are always wrapped with `id = 0`; handler responses preserve request IDs. Chat
 * replies are streamed separately through [chatReplies], never dispatched to [handler].
 */
class TransportClient(
    private val host: String,
    private val port: Int,
    private val token: String,
    private val handler: CapabilityHandler = NoDeviceHandler(),
) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val lifecycleMutex = Any()
    private val writeMutex = Mutex()
    private val _state = MutableStateFlow<TransportState>(TransportState.Disconnected)
    private val _chatReplies = MutableSharedFlow<ChatReply>(
        replay = CHAT_REPLY_REPLAY,
        extraBufferCapacity = 64,
    )
    private val _sessionConfig = MutableStateFlow<SessionConfig?>(null)
    private val _sessionErrors = MutableSharedFlow<String>(extraBufferCapacity = 8)

    val state: StateFlow<TransportState> = _state.asStateFlow()
    val chatReplies: SharedFlow<ChatReply> = _chatReplies.asSharedFlow()
    val sessionConfig: StateFlow<SessionConfig?> = _sessionConfig.asStateFlow()
    val sessionErrors: SharedFlow<String> = _sessionErrors.asSharedFlow()

    @Volatile
    private var runner: Job? = null

    @Volatile
    private var configRetry: Job? = null

    private val pendingChats = ConcurrentHashMap<Long, String>()
    private val chatRetries = ConcurrentHashMap<Long, Job>()

    private var stopping = false

    @Volatile
    private var activeSocket: Socket? = null

    private var activeOutput: OutputStream? = null

    init {
        require(host.isNotBlank()) { "Transport host must not be blank" }
        require(port in 1..65535) { "Transport port is outside the valid range" }
        require(token.isNotEmpty()) { "Transport token must not be empty" }
    }

    fun start() {
        synchronized(lifecycleMutex) {
            if (stopping || runner?.isActive == true) return
            runner = scope.launch {
                coroutineScope {
                    launch {
                        SmsBus.messages.collect { message ->
                            sendPush(SmsBus.toPush(message))
                        }
                    }
                    launch {
                        CapabilityBus.granted.drop(1).collect { granted ->
                            sendPush(CapabilityBus.toPush(granted))
                        }
                    }
                    reconnectLoop()
                }
            }
        }
    }

    suspend fun stop() {
        val job = synchronized(lifecycleMutex) {
            if (stopping) return
            stopping = true
            runner
        }
        try {
            activeSocket?.closeQuietly()
            job?.cancel()
            withContext(NonCancellable) {
                job?.join()
                clearConnection()
                _state.value = TransportState.Disconnected
            }
        } finally {
            synchronized(lifecycleMutex) {
                if (runner === job) runner = null
                stopping = false
            }
        }
    }

    /** Sends a body as an APK push envelope. Returns false while unauthenticated or after I/O error. */
    suspend fun sendPush(json: JSONObject): Boolean = withContext(Dispatchers.IO) {
        writeMutex.withLock {
            val output = activeOutput
            if (_state.value !is TransportState.Authenticated || output == null) {
                return@withLock false
            }
            try {
                Frames.write(output, Frames.envelope(id = 0L, body = json))
                true
            } catch (_: IOException) {
                activeSocket?.closeQuietly()
                false
            }
        }
    }

    /**
     * Sends `{"id":<n>,"body":{"type":"chat.send","text":"..."}}` and returns `<n>`.
     * The response is a stream of [ChatReply] values on [chatReplies], not a single correlated
     * response. Throws [IllegalStateException] while the transport is unauthenticated.
     */
    suspend fun sendChat(text: String, onReserved: (Long) -> Unit = {}): Long {
        val id = allocateOutboundId()
        // Correlation is installed before the first suspension, so an immediate
        // daemon delta can never outrun the ViewModel's turn map.
        onReserved(id)
        pendingChats[id] = text
        try {
            writeRequest(id, Frames.chatSend(text))
        } catch (error: Exception) {
            pendingChats.remove(id)
            throw error
        }
        return id
    }

    suspend fun requestSessionConfig(): Long = sendRequest(Frames.sessionConfigGet())

    suspend fun selectModel(provider: String, model: String): Long =
        sendRequest(Frames.sessionSelectModel(provider, model))

    suspend fun selectEffort(effort: String?): Long =
        sendRequest(Frames.sessionSelectEffort(effort))

    private suspend fun sendRequest(body: JSONObject): Long {
        val id = allocateOutboundId()
        writeRequest(id, body)
        return id
    }

    private suspend fun writeRequest(id: Long, body: JSONObject) = withContext(Dispatchers.IO) {
        writeMutex.withLock {
            val output = activeOutput
            if (_state.value !is TransportState.Authenticated || output == null) {
                throw IllegalStateException("Daemon transport is not authenticated")
            }
            try {
                Frames.write(output, Frames.envelope(id, body))
            } catch (error: IOException) {
                activeSocket?.closeQuietly()
                throw error
            }
        }
    }

    private suspend fun reconnectLoop() {
        var backoffMs = MIN_BACKOFF_MS
        while (currentCoroutineContext().isActive) {
            _state.value = TransportState.Connecting
            try {
                runConnection {
                    backoffMs = MIN_BACKOFF_MS
                }
                throw IOException("Daemon closed the transport")
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (_: AuthRejectedException) {
                _state.value = TransportState.Error(TransportState.Failure.AUTH_REJECTED)
            } catch (_: Frames.ProtocolException) {
                _state.value = TransportState.Error(TransportState.Failure.PROTOCOL)
            } catch (_: Exception) {
                _state.value = TransportState.Error(TransportState.Failure.IO)
            } finally {
                activeSocket?.closeQuietly()
                clearConnection()
            }

            delay(backoffMs)
            backoffMs = (backoffMs * 2).coerceAtMost(MAX_BACKOFF_MS)
        }
    }

    private suspend fun runConnection(onAuthenticated: () -> Unit) {
        val socket = Socket()
        activeSocket = socket
        socket.tcpNoDelay = true
        socket.connect(InetSocketAddress(host, port), CONNECT_TIMEOUT_MS)
        val input = socket.getInputStream()
        val output = socket.getOutputStream()
        writeMutex.withLock {
            activeOutput = output
            Frames.write(output, Frames.hello(token))
        }

        val handshakeJson = Frames.read(input) ?: throw IOException("Daemon closed during handshake")
        val handshake = Frames.parseEnvelope(handshakeJson)
        if (handshake.id != HELLO_ID) {
            throw Frames.ProtocolException("Handshake response has the wrong ID")
        }
        when (handshake.body.optString("type")) {
            "authOk" -> Unit
            "authReject" -> throw AuthRejectedException()
            else -> throw Frames.ProtocolException("Unexpected handshake response")
        }

        _state.value = TransportState.Authenticated
        onAuthenticated()
        sendPush(CapabilityBus.toPush())
        requestSessionConfig()

        while (currentCoroutineContext().isActive) {
            val requestJson = Frames.read(input) ?: return
            val request = Frames.parseEnvelope(requestJson)
            if (request.id == 0L) {
                throw Frames.ProtocolException("Daemon request uses the reserved push ID")
            }
            if (Frames.isChatReply(request.body)) {
                val reply = Frames.parseChatReply(request)
                if (reply is ChatReply.Error &&
                    reply.code == "daemon_initializing" &&
                    reply.retryable &&
                    pendingChats.containsKey(reply.id)
                ) {
                    scheduleChatRetry(reply.id)
                    continue
                }
                if (reply is ChatReply.Done || reply is ChatReply.Error) {
                    pendingChats.remove(reply.id)
                    chatRetries.remove(reply.id)?.cancel()
                }
                _chatReplies.emit(reply)
                continue
            }
            if (Frames.isSessionReply(request.body)) {
                when (val reply = Frames.parseSessionReply(request)) {
                    is SessionReply.Config -> {
                        configRetry?.cancel()
                        configRetry = null
                        _sessionConfig.value = reply.value
                    }
                    is SessionReply.Error -> {
                        if (reply.code == "daemon_initializing" && reply.retryable) {
                            scheduleSessionConfigRetry()
                        } else {
                            _sessionErrors.emit(reply.message)
                        }
                    }
                }
                continue
            }
            val response = try {
                handler.handle(request.body)
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (_: Exception) {
                Frames.error("request_failed")
            }
            if (response != null) {
                writeMutex.withLock {
                    Frames.write(output, Frames.envelope(request.id, response))
                }
            }
        }
    }

    private suspend fun clearConnection() {
        configRetry?.cancel()
        configRetry = null
        chatRetries.values.forEach { it.cancel() }
        chatRetries.clear()
        pendingChats.clear()
        writeMutex.withLock {
            activeOutput = null
            activeSocket = null
        }
        _sessionConfig.value = null
    }

    private fun scheduleSessionConfigRetry() {
        if (configRetry?.isActive == true) return
        configRetry = scope.launch {
            delay(SESSION_CONFIG_RETRY_MS)
            if (_state.value is TransportState.Authenticated) {
                try {
                    requestSessionConfig()
                } catch (_: IllegalStateException) {
                    // A reconnect will request the snapshot after authentication.
                } catch (_: IOException) {
                    // The connection loop owns I/O recovery.
                }
            }
        }
    }

    private fun scheduleChatRetry(id: Long) {
        if (chatRetries[id]?.isActive == true) return
        val text = pendingChats[id] ?: return
        chatRetries[id] = scope.launch {
            delay(CHAT_STARTUP_RETRY_MS)
            if (_state.value is TransportState.Authenticated && pendingChats[id] == text) {
                try {
                    writeRequest(id, Frames.chatSend(text))
                } catch (_: IllegalStateException) {
                    // A disconnect seals the turn in the ViewModel.
                } catch (_: IOException) {
                    // The connection loop owns I/O recovery.
                }
            }
        }
    }

    private fun allocateOutboundId(): Long {
        while (true) {
            val id = nextOutboundId.get()
            val next = if (id == Long.MAX_VALUE) 2L else id + 1L
            if (nextOutboundId.compareAndSet(id, next)) return id
        }
    }

    private fun Socket.closeQuietly() {
        try {
            close()
        } catch (_: IOException) {
            // The read loop owns connection error reporting.
        }
    }

    private class AuthRejectedException : Exception()

    private companion object {
        const val HELLO_ID = 1L
        const val CONNECT_TIMEOUT_MS = 5_000
        const val MIN_BACKOFF_MS = 500L
        const val MAX_BACKOFF_MS = 8_000L
        const val CHAT_REPLY_REPLAY = 64
        const val SESSION_CONFIG_RETRY_MS = 500L
        const val CHAT_STARTUP_RETRY_MS = 500L
        val nextOutboundId = AtomicLong(2L)
    }
}
