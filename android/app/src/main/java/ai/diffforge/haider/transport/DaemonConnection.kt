package ai.diffforge.haider.transport

import android.content.Context
import ai.diffforge.haider.service.AndroidCapabilityHandler
import ai.diffforge.haider.service.SmsPermissionMonitor
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch

/** Process-wide owner of the configured daemon transport and its stable observable flows. */
object DaemonConnection {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val lifecycleLock = Any()
    private val _state = MutableStateFlow<TransportState>(TransportState.Disconnected)
    private val _chatReplies = MutableSharedFlow<ChatReply>(
        replay = CHAT_REPLY_REPLAY,
        extraBufferCapacity = 64,
    )

    val state: StateFlow<TransportState> = _state.asStateFlow()
    val chatReplies: SharedFlow<ChatReply> = _chatReplies.asSharedFlow()

    private var client: TransportClient? = null
    private var stateForwarder: Job? = null
    private var chatForwarder: Job? = null
    private var generation = 0L

    /** Loads the saved endpoint and starts one client. Calls while a client is active are no-ops. */
    fun start(context: Context) {
        val appContext = context.applicationContext
        SmsPermissionMonitor.start(appContext)
        val config = DaemonConfig.load(appContext) ?: return

        synchronized(lifecycleLock) {
            if (client != null) return
            generation += 1L
            attachLocked(appContext, config, generation)
        }
    }

    /** Stops the current client, reloads configuration, and starts a replacement asynchronously. */
    fun restart(context: Context) {
        val appContext = context.applicationContext
        SmsPermissionMonitor.start(appContext)
        val config = DaemonConfig.load(appContext)
        val detached: Pair<TransportClient?, Long> = synchronized(lifecycleLock) {
            generation += 1L
            val current = detachLocked()
            _state.value = TransportState.Disconnected
            current to generation
        }

        scope.launch(start = CoroutineStart.UNDISPATCHED) {
            detached.first?.stop()
            if (config != null) {
                synchronized(lifecycleLock) {
                    if (client == null && generation == detached.second) {
                        attachLocked(appContext, config, detached.second)
                    }
                }
            }
        }
    }

    /** Detaches immediately and finishes closing the underlying socket asynchronously. */
    fun stop() {
        val current = synchronized(lifecycleLock) {
            generation += 1L
            val detached = detachLocked()
            _state.value = TransportState.Disconnected
            detached
        }
        if (current != null) {
            scope.launch(start = CoroutineStart.UNDISPATCHED) { current.stop() }
        }
    }

    /** Sends a chat turn through the authenticated client and returns its streamed-reply ID. */
    suspend fun sendChat(text: String): Long {
        val current = synchronized(lifecycleLock) { client }
            ?: throw IllegalStateException("Daemon connection has not been started")
        return current.sendChat(text)
    }

    /** Must be called while holding [lifecycleLock]. */
    private fun attachLocked(context: Context, config: DaemonConfig, ownerGeneration: Long) {
        val newClient = TransportClient(
            host = config.host,
            port = config.port,
            token = config.token,
            handler = AndroidCapabilityHandler(context),
        )
        client = newClient
        stateForwarder = scope.launch(start = CoroutineStart.UNDISPATCHED) {
            newClient.state.collect { newState ->
                synchronized(lifecycleLock) {
                    if (client === newClient && generation == ownerGeneration) {
                        _state.value = newState
                    }
                }
            }
        }
        chatForwarder = scope.launch(start = CoroutineStart.UNDISPATCHED) {
            newClient.chatReplies.collect { reply ->
                val isCurrent = synchronized(lifecycleLock) {
                    client === newClient && generation == ownerGeneration
                }
                if (isCurrent) _chatReplies.emit(reply)
            }
        }
        newClient.start()
    }

    /** Must be called while holding [lifecycleLock]. */
    private fun detachLocked(): TransportClient? {
        val current = client
        client = null
        stateForwarder?.cancel()
        chatForwarder?.cancel()
        stateForwarder = null
        chatForwarder = null
        return current
    }

    private const val CHAT_REPLY_REPLAY = 64
}
