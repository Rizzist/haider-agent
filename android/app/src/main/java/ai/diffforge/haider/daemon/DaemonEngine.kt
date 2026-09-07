package ai.diffforge.haider.daemon

import org.json.JSONObject

internal interface DaemonClock {
    fun unixMs(): Long
    fun elapsedMs(): Long
}
internal data class ProcessExit(
    val unixMs: Long,
    val userRequested: Boolean,
    val packageUpdated: Boolean = false,
    val legacyUserRequested: Boolean = false,
)

/** Single-owner state machine. Getters use the service's immutable published copy, never this owner. */
internal class DaemonEngine(
    private val host: NativeDaemonHost,
    private val vault: VaultDekProvider,
    private val store: DaemonLifecycleStore,
    private val clock: DaemonClock,
    private val appVersion: String,
    private val pathsJson: String,
    private val endpoint: String,
    private val policyJson: String,
    private val publish: (DaemonServiceSnapshot) -> Unit,
    private val terminateDaemonProcess: () -> Unit,
) {
    private var persisted = store.load()
    var snapshot = DaemonServiceSnapshot(appVersion = appVersion)
        private set
    private var nativeOwned = false
    private var startupElapsedMs = 0L
    private var retryElapsedMs: Long? = null
    private var awaitingReplacement = false
    private var nativeDrainElapsedMs: Long? = null

    fun restore(exit: ProcessExit?) {
        val updatePending = persisted.enabled && persisted.updateUntilUnixMs?.let {
            it > clock.unixMs() && it - clock.unixMs() <= UPDATE_WINDOW_MS
        } == true
        if (exit != null && exit.unixMs > persisted.lastExitUnixMs) {
            val expectedUpdate = exit.packageUpdated || (exit.legacyUserRequested && updatePending)
            if (expectedUpdate) {
                save(persisted.copy(active = false, lastExitUnixMs = exit.unixMs))
            } else if (exit.userRequested && exit.unixMs >= persisted.lastUserStartUnixMs) {
                save(persisted.copy(enabled = false, active = false, crashes = emptyList(),
                    latchedError = null, retryAtUnixMs = null, updateUntilUnixMs = null,
                    lastExitUnixMs = exit.unixMs))
            } else {
                save(persisted.copy(lastExitUnixMs = exit.unixMs))
            }
        }
        // On pre-ExitInfo devices, the protected replacement marker is the only expected-exit proof.
        if (updatePending && exit == null && persisted.active) save(persisted.copy(active = false))
        if (persisted.active && persisted.enabled && persisted.latchedError == null) {
            unexpectedExit("INTERNAL")
        } else {
            emit(snapshot.copy(enabled = persisted.enabled,
                phase = if (persisted.latchedError != null) "ERROR" else if (persisted.enabled) "RESTARTING" else "DISABLED",
                errorCode = persisted.latchedError, errorRetryable = persisted.latchedError == "CRASH_LOOP",
                restartAttempt = persisted.crashes.size,
                nextRetryUnixMs = persisted.retryAtUnixMs))
            persisted.retryAtUnixMs?.let {
                retryElapsedMs = clock.elapsedMs() + (it - clock.unixMs()).coerceIn(0, MAX_BACKOFF_MS)
            }
        }
    }

    fun startUserInitiated() {
        if (nativeOwned && snapshot.phase in setOf("STARTING", "RECOVERING", "READY")) return
        save(persisted.copy(enabled = true, crashes = emptyList(), latchedError = null,
            retryAtUnixMs = null, updateUntilUnixMs = null, lastUserStartUnixMs = clock.unixMs()))
        awaitingReplacement = false
        retryElapsedMs = null
        beginStart()
    }

    fun resumeEnabled(afterReplacement: Boolean = false) {
        if (!persisted.enabled || persisted.latchedError != null || nativeOwned) return
        if (afterReplacement) {
            // Consume the one-shot marker; it can never itself enable a disabled app.
            save(persisted.copy(updateUntilUnixMs = null))
            awaitingReplacement = false
        } else if (persisted.updateUntilUnixMs?.let { it > clock.unixMs() } == true) {
            awaitingReplacement = true
            return
        }
        if (retryElapsedMs == null || clock.elapsedMs() >= requireNotNull(retryElapsedMs)) beginStart()
    }

    fun restart() {
        save(persisted.copy(enabled = true, crashes = emptyList(), latchedError = null,
            retryAtUnixMs = null, updateUntilUnixMs = null, lastUserStartUnixMs = clock.unixMs()))
        awaitingReplacement = false
        retryElapsedMs = null
        emit(snapshot.copy(enabled = true, phase = "STOPPING", rpcEndpoint = null, nextRetryUnixMs = null))
        if (drain()) beginStart()
    }

    fun stopAndDisable() {
        // This durable write must complete before shutdown, even when the service is recovering.
        save(persisted.copy(enabled = false, retryAtUnixMs = null, updateUntilUnixMs = null,
            crashes = emptyList(), latchedError = null))
        retryElapsedMs = null
        awaitingReplacement = false
        emit(snapshot.copy(enabled = false, phase = "STOPPING", rpcEndpoint = null, nextRetryUnixMs = null))
        if (drain()) emit(snapshot.copy(phase = "DISABLED", errorCode = null, errorRetryable = false,
            restartAttempt = 0, startedAtElapsedRealtimeMs = null))
    }

    fun prepareForUpdate() {
        save(persisted.copy(updateUntilUnixMs = if (persisted.enabled) clock.unixMs() + UPDATE_WINDOW_MS else null,
            retryAtUnixMs = null))
        retryElapsedMs = null
        awaitingReplacement = persisted.enabled
        emit(snapshot.copy(enabled = persisted.enabled, phase = "STOPPING", rpcEndpoint = null, nextRetryUnixMs = null))
        if (drain()) emit(snapshot.copy(phase = if (persisted.enabled) "RESTARTING" else "DISABLED",
            startedAtElapsedRealtimeMs = null))
    }

    fun tick() {
        if (awaitingReplacement) {
            if (persisted.updateUntilUnixMs?.let { it <= clock.unixMs() } == true) {
                save(persisted.copy(updateUntilUnixMs = null))
                awaitingReplacement = false
                resumeEnabled()
            }
            return
        }
        if (snapshot.phase == "RESTARTING" && persisted.enabled && persisted.latchedError == null) {
            resumeEnabled()
            return
        }
        if (!nativeOwned || snapshot.phase !in setOf("STARTING", "RECOVERING", "READY", "STOPPING")) return
        val observation = try {
            NativeObservation.parse(host.observe(), endpoint)
        } catch (_: Exception) {
            fail("NATIVE_PROTOCOL_INVALID", false)
            return
        } catch (_: LinkageError) {
            fail("NATIVE_LIBRARY_UNAVAILABLE", false)
            return
        }
        when (observation.phase) {
            "Starting", "Recovering" -> {
                if (clock.elapsedMs() - startupElapsedMs >= STARTUP_TIMEOUT_MS) {
                    fail("INTERNAL", true)
                } else {
                    emit(snapshot.copy(phase = if (observation.phase == "Starting") "STARTING" else "RECOVERING",
                        daemonGeneration = observation.generation, rpcEndpoint = null))
                }
            }
            "Ready" -> emit(snapshot.copy(phase = "READY", daemonGeneration = observation.generation,
                rpcEndpoint = RpcEndpoint(requireNotNull(observation.endpoint), 1, observation.generation),
                nextRetryUnixMs = null, errorCode = null, errorRetryable = false))
            "Draining" -> {
                val began = nativeDrainElapsedMs ?: clock.elapsedMs().also { nativeDrainElapsedMs = it }
                emit(snapshot.copy(phase = "STOPPING", rpcEndpoint = null))
                if (clock.elapsedMs() - began >= 7_000) fail("INTERNAL", true)
            }
            "Failed" -> fail(observation.errorCode ?: "INTERNAL", observation.retryable)
            "Stopped" -> fail("INTERNAL", true)
        }
    }

    fun updateEnvironment(network: String, notificationsGranted: Boolean, batteryRestricted: Boolean, pssBytes: Long?) {
        emit(snapshot.copy(network = network, notificationsGranted = notificationsGranted,
            batteryRestricted = batteryRestricted, pssBytes = pssBytes))
    }

    /** Framework destruction is not a user Stop: retain opt-in but release the runtime. */
    fun destroy() {
        retryElapsedMs = null
        if (nativeOwned) {
            emit(snapshot.copy(phase = "STOPPING", rpcEndpoint = null))
            drain()
        }
    }

    private fun beginStart() {
        if (nativeOwned || !persisted.enabled || persisted.latchedError != null) return
        retryElapsedMs = null
        save(persisted.copy(active = true, retryAtUnixMs = null, updateUntilUnixMs = null))
        startupElapsedMs = clock.elapsedMs()
        nativeDrainElapsedMs = null
        emit(snapshot.copy(enabled = true, phase = "STARTING", rpcEndpoint = null, nativeVersion = "",
            wireProtocol = 0, daemonGeneration = 0, nextRetryUnixMs = null,
            errorCode = null, errorRetryable = false, restartAttempt = persisted.crashes.size,
            startedAtElapsedRealtimeMs = startupElapsedMs))
        try {
            val rawVersion = host.version()
            require(rawVersion.length <= 4096)
            val version = JSONObject(rawVersion)
            if (version.exactLong("jni_version") != 1L || version.exactLong("wire_protocol") != 1L ||
                version.getString("abi") !in setOf("arm64-v8a", "x86_64") ||
                version.getString("build_id").isBlank()) {
                latch("NATIVE_PROTOCOL_INVALID")
                return
            }
            if (version.getString("daemon_version") != appVersion) {
                latch("NATIVE_VERSION_MISMATCH")
                return
            }
            emit(snapshot.copy(nativeVersion = appVersion, wireProtocol = 1))
            // A non-OK init/start may retain context/runtime ownership. Drain it before any retry.
            nativeOwned = true
            val initStatus = host.init(pathsJson)
            if (initStatus != NativeStatus.OK) {
                fail(NativeStatus.code(initStatus), initStatus == NativeStatus.INTERNAL)
                return
            }
            val startStatus = vault.withDek { dek ->
                try { host.start(dek, policyJson) } finally { dek.fill(0) }
            }
            if (startStatus != NativeStatus.OK) {
                fail(NativeStatus.code(startStatus), startStatus == NativeStatus.INTERNAL)
            }
            // OK is acceptance only; the next nonblocking observation establishes recovery/Ready.
        } catch (failure: VaultKeyFailure) {
            fail(failure.code, false)
        } catch (_: LinkageError) {
            fail("NATIVE_LIBRARY_UNAVAILABLE", false)
        } catch (_: Exception) {
            fail("INTERNAL", true)
        }
    }

    private fun fail(code: String, retryable: Boolean) {
        emit(snapshot.copy(phase = "STOPPING", rpcEndpoint = null, errorCode = code, errorRetryable = retryable))
        if (!drain()) return
        if (retryable) unexpectedExit(code) else latch(code)
    }

    private fun drain(): Boolean {
        if (nativeOwned) {
            val status = try { host.shutdown(false, 7_000) } catch (_: Exception) {
                NativeStatus.SHUTDOWN_TIMEOUT
            } catch (_: LinkageError) { NativeStatus.SHUTDOWN_TIMEOUT }
            if (status != NativeStatus.OK && status != NativeStatus.SHUTDOWN_FORCED) {
                // Completion is unproven. Do not release context or permit another runtime.
                latch("SHUTDOWN_TIMEOUT", retainActive = true)
                terminateDaemonProcess()
                return false
            }
            try {
                host.release()
            } catch (_: Exception) {
                latch("SHUTDOWN_TIMEOUT", retainActive = true)
                terminateDaemonProcess()
                return false
            } catch (_: LinkageError) {
                latch("SHUTDOWN_TIMEOUT", retainActive = true)
                terminateDaemonProcess()
                return false
            }
            nativeOwned = false
        }
        save(persisted.copy(active = false))
        return true
    }

    private fun unexpectedExit(code: String) {
        val now = clock.unixMs()
        // Retain future timestamps on clock rollback; clock changes must not defeat the latch.
        val crashes = (persisted.crashes.filter { now - it < CRASH_WINDOW_MS } + now).takeLast(3)
        if (crashes.size >= 3) {
            save(persisted.copy(crashes = crashes, active = false))
            latch("CRASH_LOOP")
            return
        }
        val delay = if (crashes.size == 1) 1_000L else 5_000L
        save(persisted.copy(active = false, crashes = crashes, retryAtUnixMs = now + delay))
        retryElapsedMs = clock.elapsedMs() + delay
        emit(snapshot.copy(enabled = persisted.enabled, phase = "RESTARTING", rpcEndpoint = null,
            restartAttempt = crashes.size, nextRetryUnixMs = now + delay, errorCode = code,
            errorRetryable = true, startedAtElapsedRealtimeMs = null))
    }

    private fun latch(code: String, retainActive: Boolean = false) {
        save(persisted.copy(active = retainActive, latchedError = code, retryAtUnixMs = null))
        retryElapsedMs = null
        emit(snapshot.copy(enabled = persisted.enabled, phase = "ERROR", rpcEndpoint = null,
            restartAttempt = persisted.crashes.size, nextRetryUnixMs = null,
            // Retryable means explicit user restart; the latch still prohibits automatic retry.
            errorCode = code, errorRetryable = code == "CRASH_LOOP", startedAtElapsedRealtimeMs = null))
    }

    private fun save(next: PersistedLifecycle) {
        store.save(next)
        persisted = next
    }
    private fun emit(next: DaemonServiceSnapshot) {
        if (next == snapshot) return
        snapshot = next.copy(snapshotSeq = snapshot.snapshotSeq + 1)
        publish(snapshot)
    }
    companion object {
        const val CRASH_WINDOW_MS = 600_000L
        const val MAX_BACKOFF_MS = 30_000L
        const val STARTUP_TIMEOUT_MS = 120_000L
        const val UPDATE_WINDOW_MS = 120_000L
    }
}
