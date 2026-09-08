package ai.diffforge.haider.daemon

import android.Manifest
import android.app.ActivityManager
import android.app.Application
import android.app.ApplicationExitInfo
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.Binder
import android.os.Build
import android.os.Debug
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.os.Process
import android.os.RemoteCallbackList
import android.os.RemoteException
import android.os.SystemClock
import ai.diffforge.haider.R
import org.json.JSONObject
import java.io.FileDescriptor
import java.io.PrintWriter
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledThreadPoolExecutor
import java.util.concurrent.TimeUnit

/** Started lifetime is independent of bindings. Native calls never run on main or Binder threads. */
open class HaiderDaemonService : Service() {
    private val main = Handler(Looper.getMainLooper())
    private val owner = ScheduledThreadPoolExecutor(1) { task ->
        Thread(null, task, "haider-native-owner", 8L * 1024 * 1024)
    }
    private val callbacks = Executors.newSingleThreadExecutor { Thread(it, "haider-binder-snapshots") }
    private val listeners = RemoteCallbackList<IDaemonSnapshotListener>()
    private lateinit var notifications: DaemonNotifications
    private lateinit var rosterObserver: SessionNotificationObserver
    private lateinit var engine: DaemonEngine
    private var rosterSubscription: AutoCloseable? = null
    private var rosterEndpoint: RpcEndpoint? = null
    private var rosterEpoch = 0L
    private var lastMetricsElapsedMs = -10_000L
    private var ownerFailed = false
    // Main-thread confined. Snapshots can still describe initialization or an older action.
    private var pendingStartActions = 0
    @Volatile private var started = false
    @Volatile private var destroyed = false
    @Volatile private var latest = DaemonServiceSnapshot()

    private val binder = object : IHaiderDaemonService.Stub() {
        override fun getSnapshot(): DaemonServiceSnapshot { checkUid(); return latest }
        override fun getRpcEndpoint(): RpcEndpoint? { checkUid(); return latest.rpcEndpoint }
        override fun registerListener(listener: IDaemonSnapshotListener?) {
            checkUid()
            requireNotNull(listener)
            callbacks.execute {
                val cursor = ListenerCursor()
                if (!destroyed && listeners.register(listener, cursor)) sendSnapshot(listener, cursor, latest)
            }
        }
        override fun unregisterListener(listener: IDaemonSnapshotListener?) {
            checkUid()
            requireNotNull(listener)
            callbacks.execute { listeners.unregister(listener) }
        }
        override fun startUserInitiated() { checkUid(); enqueueStartedAction(ACTION_USER_START) }
        override fun stopAndDisable() { checkUid(); enqueueStartedAction(DaemonIntents.STOP_DAEMON) }
        override fun restart() { checkUid(); enqueueStartedAction(DaemonIntents.RESTART_DAEMON) }
        override fun prepareForUpdate() { checkUid(); enqueueStartedAction(ACTION_PREPARE_UPDATE) }
    }

    override fun onCreate() {
        super.onCreate()
        notifications = DaemonNotifications(this).also { it.createChannels() }
        rosterObserver = SessionNotificationObserver(notifications)
        latest = DaemonServiceSnapshot(appVersion = packagedVersion())
        owner.execute {
            ownerWork {
                val paths = createPaths()
                engine = DaemonEngine(createNativeHost(), createVault(paths), createLifecycleStore(), object : DaemonClock {
                    override fun unixMs() = System.currentTimeMillis()
                    override fun elapsedMs() = SystemClock.elapsedRealtime()
                }, latest.appVersion, paths.json, paths.endpoint,
                    JSONObject().put("policy_version", 1).put("name", "android-standalone")
                        .put("default_model", getString(R.string.daemon_default_model))
                        .put("store_synchronous", "normal").toString(), ::publishSnapshot, ::terminateOwnProcess)
                engine.restore(lastProcessExit())
                updateEnvironment()
            }
        }
        owner.scheduleWithFixedDelay({ ownerWork {
            if (started && !destroyed && ::engine.isInitialized) {
                engine.tick()
                if (SystemClock.elapsedRealtime() - lastMetricsElapsedMs >= 10_000) updateEnvironment()
            }
        } }, 500, 500, TimeUnit.MILLISECONDS)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Promotion precedes every possible load/init/start/recovery task on the owner queue.
        val notification = notifications.status(latest)
        try {
            if (Build.VERSION.SDK_INT >= 34) {
                startForeground(DaemonNotifications.STATUS_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
            } else {
                startForeground(DaemonNotifications.STATUS_ID, notification)
            }
        } catch (_: RuntimeException) {
            owner.execute { publishFailure("FOREGROUND_START_DENIED") }
            stopSelf(startId)
            return START_STICKY
        }
        pendingStartActions++
        started = true
        val action = intent?.action
        owner.execute {
            try {
                ownerWork {
                    when (action) {
                        ACTION_USER_START -> engine.startUserInitiated()
                        DaemonIntents.STOP_DAEMON -> engine.stopAndDisable()
                        DaemonIntents.RESTART_DAEMON -> engine.restart()
                        ACTION_PREPARE_UPDATE -> engine.prepareForUpdate()
                        ACTION_PACKAGE_REPLACED -> engine.resumeEnabled(afterReplacement = true)
                        null, ACTION_RESUME_ENABLED -> engine.resumeEnabled()
                    }
                }
            } finally {
                main.post {
                    pendingStartActions--
                    // Reconsider teardown only after every accepted action has completed.
                    finishStoppedStart()
                }
            }
        }
        return START_STICKY
    }
    override fun onBind(intent: Intent?): IBinder = binder
    override fun onUnbind(intent: Intent?): Boolean = false
    /** Read-only shell diagnostics for recovery probes; never binds, starts, or instruments the app. */
    override fun dump(fd: FileDescriptor?, writer: PrintWriter, args: Array<out String>?) {
        val snapshot = latest
        writer.println("HAIDER_DAEMON_STATE " + JSONObject()
            .put("enabled", snapshot.enabled).put("phase", snapshot.phase)
            .put("generation", snapshot.daemonGeneration).put("snapshotSeq", snapshot.snapshotSeq)
            .put("errorCode", snapshot.errorCode ?: JSONObject.NULL)
            .put("errorRetryable", snapshot.errorRetryable)
            .put("restartAttempt", snapshot.restartAttempt)
            .put("nextRetryUnixMs", snapshot.nextRetryUnixMs ?: JSONObject.NULL)
            .put("hasEndpoint", snapshot.rpcEndpoint != null)
            .put("started", started).put("destroyed", destroyed))
    }
    // No onTaskRemoved stop: swiping/killing the UI does not revoke user-enabled service lifetime.

    override fun onDestroy() {
        destroyed = true
        started = false
        owner.execute {
            try {
                closeRoster()
                if (::engine.isInitialized) engine.destroy()
            } finally {
                callbacks.execute { listeners.kill() }
                callbacks.shutdown()
                owner.shutdown()
            }
        }
        super.onDestroy()
    }

    internal open fun createPaths(): DaemonPaths = DaemonPaths.create(this)
    internal open fun createLifecycleStore(): DaemonLifecycleStore = FileDaemonLifecycleStore(this)
    internal open fun createVault(paths: DaemonPaths): VaultDekProvider = VaultDekProvider { use ->
        VaultDekStore.create(this, paths.vaultDirectory).withDek(use)
    }
    protected open fun createNativeHost(): NativeDaemonHost = JniNativeDaemonHost(applicationContext)
    /** Lane 3 supplies its full-RPC View adapter at this seam. Never substitute fake data here. */
    protected open fun createSessionRosterSource(): SessionRosterSource =
        ai.diffforge.haider.transport.rpc.RpcSessionRosterSource(
            java.io.File(filesDir, "haider/ui-cache/notifier"), packagedVersion())

    private fun enqueueStartedAction(action: String) {
        // Execute in our identity, after returning from the Binder call. No native work here.
        main.post {
            if (!destroyed) {
                try { startForegroundService(Intent(this, HaiderDaemonService::class.java).setAction(action).setPackage(packageName)) }
                catch (_: RuntimeException) { owner.execute { publishFailure("FOREGROUND_START_DENIED") } }
            }
        }
    }
    private fun checkUid() {
        if (Binder.getCallingUid() != applicationInfo.uid) throw SecurityException("Same UID required")
    }

    private fun publishSnapshot(snapshot: DaemonServiceSnapshot) {
        if (snapshot.phase != latest.phase) android.util.Log.i("HaiderDaemon",
            "phase=${snapshot.phase} generation=${snapshot.daemonGeneration}")
        latest = snapshot
        if (!destroyed) {
            callbacks.execute {
                val count = listeners.beginBroadcast()
                try {
                    for (index in 0 until count) {
                        val listener = listeners.getBroadcastItem(index)
                        val cursor = listeners.getBroadcastCookie(index) as ListenerCursor
                        sendSnapshot(listener, cursor, snapshot)
                    }
                } finally { listeners.finishBroadcast() }
            }
            main.post { if (started && !destroyed) notifications.updateStatus(latest) }
            if (snapshot.rpcEndpoint != rosterEndpoint) {
                closeRoster()
                snapshot.rpcEndpoint?.let { endpoint ->
                    rosterEndpoint = endpoint
                    val epoch = rosterEpoch
                    try {
                        rosterSubscription = createSessionRosterSource().observe(endpoint) { update ->
                            if (!destroyed) owner.execute {
                                if (epoch == rosterEpoch && !destroyed) rosterObserver.accept(update)
                            }
                        }
                    } catch (_: Exception) { rosterObserver.accept(RosterUpdate.Unavailable) }
                }
            }
            if (!snapshot.enabled && snapshot.phase in setOf("DISABLED", "ERROR") && started) finishStoppedStart()
        }
    }
    private class ListenerCursor(var lastSeq: Long = -1)
    private fun sendSnapshot(listener: IDaemonSnapshotListener, cursor: ListenerCursor, snapshot: DaemonServiceSnapshot) {
        // Registration may observe a newer snapshot than broadcasts already queued on this executor.
        if (snapshot.snapshotSeq <= cursor.lastSeq) return
        try {
            listener.onSnapshot(snapshot)
            cursor.lastSeq = snapshot.snapshotSeq
        } catch (_: RemoteException) { listeners.unregister(listener) }
    }

    private fun closeRoster() {
        rosterEpoch++
        try { rosterSubscription?.close() } catch (_: Exception) { /* No wire text in diagnostics. */ }
        rosterSubscription = null
        rosterEndpoint = null
        rosterObserver.accept(RosterUpdate.Reset)
    }
    private fun finishStoppedStart() {
        main.post {
            // A disabled restore/older Stop cannot revoke a start still queued on the owner.
            if (pendingStartActions == 0 && !latest.enabled && latest.phase in setOf("DISABLED", "ERROR") && !destroyed) {
                started = false
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }
    }
    private fun ownerWork(work: () -> Unit) {
        if (ownerFailed) return
        try { work() } catch (_: Exception) {
            ownerFailed = true
            publishFailure("LIFECYCLE_STORAGE_FAILURE")
        } catch (_: LinkageError) {
            ownerFailed = true
            publishFailure("NATIVE_LIBRARY_UNAVAILABLE")
        }
    }
    private fun publishFailure(code: String) {
        publishSnapshot(latest.copy(phase = "ERROR", rpcEndpoint = null, nextRetryUnixMs = null,
            errorCode = code, errorRetryable = false, snapshotSeq = latest.snapshotSeq + 1))
    }

    private fun updateEnvironment() {
        val network = try {
            val connectivity = getSystemService(ConnectivityManager::class.java)
            val active = connectivity.activeNetwork
            val capabilities = active?.let(connectivity::getNetworkCapabilities)
            if (capabilities?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true) "AVAILABLE" else "UNAVAILABLE"
        } catch (_: RuntimeException) { "UNKNOWN" }
        val notificationPermission = (Build.VERSION.SDK_INT < 33 ||
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) &&
            getSystemService(android.app.NotificationManager::class.java).areNotificationsEnabled()
        val restricted = getSystemService(PowerManager::class.java).let { !it.isIgnoringBatteryOptimizations(packageName) } ||
            (Build.VERSION.SDK_INT >= 28 && getSystemService(ActivityManager::class.java).isBackgroundRestricted)
        val pss = try { Debug.MemoryInfo().also(Debug::getMemoryInfo).totalPss.toLong() * 1024 } catch (_: RuntimeException) { null }
        engine.updateEnvironment(network, notificationPermission, restricted, pss)
        lastMetricsElapsedMs = SystemClock.elapsedRealtime()
    }
    private fun packagedVersion(): String = packageManager.getPackageInfo(packageName, 0).versionName ?: ""
    private fun lastProcessExit(): ProcessExit? {
        if (Build.VERSION.SDK_INT < 30) return null
        return try {
            getSystemService(ActivityManager::class.java).getHistoricalProcessExitReasons(packageName, 0, 16)
                .filter { it.processName == "$packageName:daemon" }.maxByOrNull { it.timestamp }
                ?.let {
                    val userRequested = it.reason == ApplicationExitInfo.REASON_USER_REQUESTED
                    ProcessExit(it.timestamp, userRequested,
                        packageUpdated = Build.VERSION.SDK_INT >= 34 && it.reason == ApplicationExitInfo.REASON_PACKAGE_UPDATED,
                        legacyUserRequested = Build.VERSION.SDK_INT < 34 && userRequested)
                }
        } catch (_: RuntimeException) { null }
    }
    private fun terminateOwnProcess() {
        val processName = if (Build.VERSION.SDK_INT >= 28) Application.getProcessName() else
            getSystemService(ActivityManager::class.java).runningAppProcesses?.firstOrNull { it.pid == Process.myPid() }?.processName
        check(processName == "$packageName:daemon") { "Daemon process required" }
        Process.killProcess(Process.myPid())
    }
    companion object {
        const val ACTION_USER_START = "ai.diffforge.haider.action.START_DAEMON_USER"
        internal const val ACTION_RESUME_ENABLED = "ai.diffforge.haider.action.RESUME_DAEMON_ENABLED"
        internal const val ACTION_PACKAGE_REPLACED = "ai.diffforge.haider.action.DAEMON_PACKAGE_REPLACED"
        private const val ACTION_PREPARE_UPDATE = "ai.diffforge.haider.action.PREPARE_DAEMON_UPDATE"
        fun userStartIntent(context: Context): Intent = Intent(context, HaiderDaemonService::class.java)
            .setPackage(context.packageName).setAction(ACTION_USER_START)
    }
}
