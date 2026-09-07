package ai.diffforge.haider.ui.daemon

/**
 * The single file that knows the Binder v1 control plane
 * (`docs/android/contracts-v1.md` C2, frozen).
 *
 * The AIDL service, its Parcelables and `NativeDaemon` belong to lane 971-2 in
 * package `ai.diffforge.haider.daemon`. The UI never references those types: it
 * depends on [DaemonStatus] and this mapping, so binding the real Parcelable is
 * a change to this file alone.
 *
 * Binder is control plane only. It carries no transcript, credential material,
 * staged or ready references, authorization URLs, vault data or RPC frames, and
 * daemon READY is NOT RPC CONNECTED — the data plane keeps its own
 * [DataPlaneState].
 */

/** C2 `DaemonServiceSnapshot.phase`. */
enum class DaemonPhase(val wireValue: String) {
    Disabled("DISABLED"),
    Starting("STARTING"),
    Recovering("RECOVERING"),
    Ready("READY"),
    Restarting("RESTARTING"),
    Stopping("STOPPING"),
    Error("ERROR"),
    ;

    companion object {
        fun fromWire(value: String?): DaemonPhase =
            entries.firstOrNull { it.wireValue.equals(value, ignoreCase = true) } ?: Error
    }
}

/** C2 coarse network state. */
enum class NetworkState { Available, Unavailable, Unknown }

/** UI-side data-plane state; deliberately separate from [DaemonPhase]. */
enum class DataPlaneState { Disconnected, Connecting, Connected, ProtocolError }

/**
 * C2 `RpcEndpoint`, in frozen field order. It always describes **h.sock**,
 * never mobile.sock and never an OAuth HTTP listener, and the UI never derives
 * the path itself.
 */
data class RpcEndpoint(
    val path: String,
    val wireProtocol: Int,
    val daemonGeneration: Long,
)

/**
 * A Kotlin mirror of the C2 Parcelable **in its frozen field order**, including
 * the two appended service-owned metrics. Every field exists in every snapshot.
 */
data class DaemonServiceSnapshot(
    val enabled: Boolean,
    val phase: DaemonPhase,
    val appVersion: String,
    /** Empty until successfully queried. */
    val nativeVersion: String,
    /** Zero until known, otherwise 1. */
    val wireProtocol: Int,
    /** Zero until known. This is the *daemon* generation, never a worker generation. */
    val daemonGeneration: Long,
    /** Non-null only in READY. */
    val rpcEndpoint: RpcEndpoint?,
    val restartAttempt: Int,
    val nextRetryUnixMs: Long?,
    val network: NetworkState,
    val notificationsGranted: Boolean,
    val batteryRestricted: Boolean,
    val errorCode: String?,
    val errorRetryable: Boolean,
    /** Increasing within ONE Binder service instance only. */
    val snapshotSeq: Long,
    /** Appended in the freeze: service-owned monotonic start time. */
    val startedAtElapsedRealtimeMs: Long?,
    /** Appended in the freeze: service-owned PSS sample, not labelled RSS. */
    val pssBytes: Long?,
)

/**
 * `snapshotSeq` is per Binder instance, so it must never be compared across
 * instances: on Binder death the endpoint and connection authority are cleared
 * and the baseline is reset (C2).
 */
class DaemonSnapshotSequencer {
    private var lastSeq: Long = -1L

    /** True when the snapshot is newer than what has already been applied. */
    fun accept(snapshot: DaemonServiceSnapshot): Boolean {
        if (snapshot.snapshotSeq <= lastSeq) return false
        lastSeq = snapshot.snapshotSeq
        return true
    }

    /** Called on Binder death, before reconnecting. */
    fun resetBaseline() {
        lastSeq = -1L
    }
}

/** C2 notification intents: explicit, package-scoped and immutable. */
object DaemonIntents {
    const val OPEN_SESSION = "ai.diffforge.haider.action.OPEN_SESSION"
    const val OPEN_INPUT = "ai.diffforge.haider.action.OPEN_INPUT"
    const val STOP_DAEMON = "ai.diffforge.haider.action.STOP_DAEMON"
    const val RESTART_DAEMON = "ai.diffforge.haider.action.RESTART_DAEMON"
    const val OPEN_DAEMON_STATUS = "ai.diffforge.haider.action.OPEN_DAEMON_STATUS"

    const val EXTRA_SESSION_ID = "session_id"
    const val EXTRA_HEAD_SEQ = "head_seq"
    const val EXTRA_MENU_ID = "menu_id"
    const val EXTRA_REQUEST_SEQ = "request_seq"
    const val EXTRA_WORKER_GENERATION = "worker_generation"

    /**
     * The one BROWSABLE deep link in 971: `haider://oauth/return`.
     *
     * It carries no query, fragment, authorization code, state, token, flow id
     * or ready reference. Custom schemes can be claimed by another app, so this
     * is a navigation hint only: it opens Settings and nothing else. It never
     * starts a disabled daemon, creates a flow, or commits an account.
     */
    const val DEEP_LINK_SCHEME = "haider"
    const val OAUTH_RETURN_HOST = "oauth"
    const val OAUTH_RETURN_PATH = "/return"
}

/** Where the drawer's environment signals come from, so tests can drive them. */
data class DaemonEnvironment(
    val network: NetworkState = NetworkState.Available,
    val notificationsGranted: Boolean = true,
    val batteryRestricted: Boolean = false,
)

object DaemonSnapshotMapping {
    /**
     * The whole dependency of the UI on C2.
     *
     * READY maps to Running only when the caller can vouch for the data plane:
     * JNI Ready and RPC Connected are distinct, so [dataPlane] gates it and a
     * READY service with a disconnected data plane still reads as Starting.
     */
    fun toStatus(
        snapshot: DaemonServiceSnapshot,
        dataPlane: DataPlaneState = DataPlaneState.Connected,
        sessionCount: Long? = null,
        waitingForRouteCount: Long? = null,
        profilePath: String? = null,
        runtimeDir: String? = null,
        pid: Int? = null,
    ): DaemonStatus = when (snapshot.phase) {
        DaemonPhase.Disabled -> DaemonStatus.Stopped
        DaemonPhase.Stopping -> DaemonStatus.Stopping
        DaemonPhase.Starting, DaemonPhase.Recovering -> DaemonStatus.Starting
        DaemonPhase.Restarting -> DaemonStatus.Restarting
        DaemonPhase.Error -> DaemonStatus.Failed(
            reason = snapshot.errorCode ?: "unknown",
            code = snapshot.errorCode,
        )
        DaemonPhase.Ready -> if (dataPlane != DataPlaneState.Connected) {
            DaemonStatus.Starting
        } else {
            DaemonStatus.Running(
                DaemonInfo(
                    version = snapshot.nativeVersion,
                    generation = snapshot.daemonGeneration,
                    pid = pid,
                    socketPath = snapshot.rpcEndpoint?.path,
                    ready = true,
                    sessionCount = sessionCount,
                    waitingForRouteCount = waitingForRouteCount,
                    profilePath = profilePath,
                    runtimeDir = runtimeDir,
                    startedAtElapsedRealtimeMs = snapshot.startedAtElapsedRealtimeMs,
                    pssBytes = snapshot.pssBytes,
                ),
            )
        }
    }

    fun toEnvironment(snapshot: DaemonServiceSnapshot): DaemonEnvironment = DaemonEnvironment(
        network = snapshot.network,
        notificationsGranted = snapshot.notificationsGranted,
        batteryRestricted = snapshot.batteryRestricted,
    )
}
