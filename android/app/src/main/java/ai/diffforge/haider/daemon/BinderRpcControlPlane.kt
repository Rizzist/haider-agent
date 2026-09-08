package ai.diffforge.haider.daemon

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.IBinder
import ai.diffforge.haider.transport.rpc.RpcControlPlane
import ai.diffforge.haider.ui.daemon.DaemonPhase
import ai.diffforge.haider.ui.daemon.DaemonSnapshotSequencer
import ai.diffforge.haider.ui.daemon.NetworkState
import ai.diffforge.haider.ui.daemon.DaemonServiceSnapshot as UiSnapshot
import ai.diffforge.haider.ui.daemon.RpcEndpoint as UiEndpoint
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import java.io.Closeable

/** One application binding. Every callback is fenced by the actual Binder instance. */
class BinderRpcControlPlane(private val context: Context, scope: CoroutineScope) : RpcControlPlane, Closeable {
    private val job = SupervisorJob(scope.coroutineContext[Job])
    private val owner = CoroutineScope(scope.coroutineContext + job + Dispatchers.Main.immediate)
    private val values = MutableStateFlow<UiSnapshot?>(null)
    override val snapshots = values.asStateFlow()
    private val denied = MutableStateFlow(false)
    override val notificationsPermanentlyDenied = denied.asStateFlow()
    private val sequencer = DaemonSnapshotSequencer()
    private var service: IHaiderDaemonService? = null
    private var listener: IDaemonSnapshotListener? = null
    private var bound = false
    private var closed = false
    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            revoke()
            val remote = IHaiderDaemonService.Stub.asInterface(binder)
            service = remote
            val callback = object : IDaemonSnapshotListener.Stub() {
                override fun onSnapshot(snapshot: DaemonServiceSnapshot) {
                    owner.launch {
                        if (!closed && service?.asBinder() === binder) {
                            val mapped = snapshot.toUiSnapshot()
                            if (sequencer.accept(mapped)) values.value = mapped
                        }
                    }
                }
            }
            listener = callback
            try { remote.registerListener(callback) } catch (_: android.os.RemoteException) { revoke() }
        }
        override fun onServiceDisconnected(name: ComponentName) = revoke()
        override fun onBindingDied(name: ComponentName) {
            revoke()
            if (bound) context.unbindService(this)
            bound = false
            if (!closed) bind()
        }
        override fun onNullBinding(name: ComponentName) = revoke()
    }
    init { owner.launch { bind() } }
    private fun bind() {
        if (!bound && !closed) bound = context.bindService(
            Intent(context, HaiderDaemonService::class.java), connection, Context.BIND_AUTO_CREATE)
    }
    private fun revoke() {
        service = null
        listener = null
        values.value = null
        sequencer.resetBaseline()
    }
    private suspend fun action(action: String) = withContext(Dispatchers.Main.immediate) {
        check(!closed)
        context.startForegroundService(Intent(context, HaiderDaemonService::class.java)
            .setPackage(context.packageName).setAction(action))
        bind()
    }
    override suspend fun start() = action(HaiderDaemonService.ACTION_USER_START)
    override suspend fun stop() = action(DaemonIntents.STOP_DAEMON)
    override suspend fun restart() = action(DaemonIntents.RESTART_DAEMON)
    override suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean) {
        denied.value = !granted && permanentlyDenied
        // The service samples the OS itself. No permission facts are smuggled through Binder.
    }
    override fun close() {
        closed = true
        runCatching { listener?.let { service?.unregisterListener(it) } }
        if (bound) context.unbindService(connection)
        bound = false
        revoke()
        job.cancel()
    }
}

internal fun DaemonServiceSnapshot.toUiSnapshot() = UiSnapshot(
    enabled, DaemonPhase.fromWire(phase), appVersion, nativeVersion, wireProtocol, daemonGeneration,
    rpcEndpoint?.let { UiEndpoint(it.path, it.wireProtocol, it.daemonGeneration) }, restartAttempt,
    nextRetryUnixMs, when (network) {
        "AVAILABLE" -> NetworkState.Available
        "UNAVAILABLE" -> NetworkState.Unavailable
        else -> NetworkState.Unknown
    }, notificationsGranted, batteryRestricted, errorCode, errorRetryable, snapshotSeq,
    startedAtElapsedRealtimeMs, pssBytes,
)
