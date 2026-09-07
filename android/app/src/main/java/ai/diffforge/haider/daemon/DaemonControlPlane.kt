package ai.diffforge.haider.daemon

/**
 * The single file that knows the Binder v1 control plane
 * (`state/971-CONTRACTS.md` C2, DRAFT).
 *
 * The contract is still under Astra validation, so the UI never references the
 * AIDL types directly: it depends on [DaemonStatus] and this mapping only. When
 * `IHaiderDaemonService` / `DaemonServiceSnapshot` are frozen (or renamed), the
 * AIDL-generated parcelable is adapted here and nothing else in the UI moves.
 *
 * Binder is control plane only: it carries no transcript, credentials, vault
 * data or RPC frames, and daemon READY is NOT RPC CONNECTED — the data plane
 * keeps its own [DataPlaneState].
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

/** C2 `RpcEndpoint`. The UI never derives the socket path itself. */
data class RpcEndpoint(
    val path: String,
    val wireProtocol: Int,
    val daemonGeneration: Long,
)

/** A Kotlin mirror of the C2 parcelable; every field is always present. */
data class DaemonServiceSnapshot(
    val enabled: Boolean,
    val phase: DaemonPhase,
    val appVersion: String,
    val nativeVersion: String,
    val wireProtocol: Int,
    val daemonGeneration: Long,
    val rpcEndpoint: RpcEndpoint?,
    val restartAttempt: Int,
    val nextRetryUnixMs: Long?,
    val network: NetworkState,
    val notificationsGranted: Boolean,
    val batteryRestricted: Boolean,
    val errorCode: String?,
    val errorRetryable: Boolean,
    val snapshotSeq: Long,
)

/** C2 notification intent actions; navigation hints, never answer authority. */
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

    /** `haider://session/<id>?prompt=<menu_id>` (UI-SPEC 3.7). */
    const val DEEP_LINK_SCHEME = "haider"
    const val DEEP_LINK_HOST_SESSION = "session"
    const val DEEP_LINK_PROMPT = "prompt"
}

/** Where the drawer's environment signals come from, so tests can drive them. */
data class DaemonEnvironment(
    val network: NetworkState = NetworkState.Available,
    val notificationsGranted: Boolean = true,
    val batteryRestricted: Boolean = false,
)

object DaemonSnapshotMapping {
    /**
     * The whole dependency of the UI on C2. `startedAtMs` and `rssBytes` are
     * Android-owned and passed in: the daemon has no uptime or memory field.
     */
    fun toStatus(
        snapshot: DaemonServiceSnapshot,
        sessionCount: Long? = null,
        waitingForRouteCount: Long? = null,
        startedAtMs: Long? = null,
        rssBytes: Long? = null,
        profilePath: String? = null,
        runtimeDir: String? = null,
        pid: Int? = null,
    ): DaemonStatus = when (snapshot.phase) {
        DaemonPhase.Disabled -> DaemonStatus.Stopped
        DaemonPhase.Stopping -> DaemonStatus.Stopped
        DaemonPhase.Starting, DaemonPhase.Recovering -> DaemonStatus.Starting
        DaemonPhase.Restarting -> DaemonStatus.Restarting
        DaemonPhase.Error -> DaemonStatus.Failed(
            reason = snapshot.errorCode ?: "unknown",
            code = snapshot.errorCode,
        )
        DaemonPhase.Ready -> DaemonStatus.Running(
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
                startedAtMs = startedAtMs,
                rssBytes = rssBytes,
            ),
        )
    }

    fun toEnvironment(snapshot: DaemonServiceSnapshot): DaemonEnvironment = DaemonEnvironment(
        network = snapshot.network,
        notificationsGranted = snapshot.notificationsGranted,
        batteryRestricted = snapshot.batteryRestricted,
    )
}
