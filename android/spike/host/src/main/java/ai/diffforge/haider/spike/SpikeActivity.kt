package ai.diffforge.haider.spike

import ai.diffforge.haider.daemon.NativeDaemon
import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.Bundle
import android.os.Process
import android.system.Os
import android.util.Log
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONObject
import java.io.DataInputStream
import java.io.File
import java.nio.ByteBuffer
import java.util.concurrent.Executors
import java.util.UUID

/** A disposable diagnostic host, deliberately independent of android/app. */
class SpikeActivity : Activity() {
    private val owner = Executors.newSingleThreadExecutor { runnable ->
        Thread(null, runnable, "spike-native-owner", 8L * 1024 * 1024)
    }
    private lateinit var status: TextView
    private lateinit var detail: TextView
    private var socket: LocalSocket? = null
    private var endpoint: File? = null
    private var mobileSocket: LocalSocket? = null
    private var vaultChecked = false
    private var lastGeneration = 0L
    private var observations = 0
    private var probeRunId = ""
    private var cycleRunId: String? = null
    private var completedCycles = 0
    private var nativeIdentity: String? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(24, 64, 24, 32)
        }
        status = TextView(this).apply { textSize = 22f; text = "Haider embedding spike — Idle" }
        detail = TextView(this).apply { textSize = 13f }
        content.addView(status)
        fun button(label: String, action: () -> Unit) {
            content.addView(Button(this).apply {
                text = label
                setOnClickListener { owner.execute { guarded(action) } }
            })
        }
        button("Start + Hello / Welcome") { startProbe() }
        button("Observe + inventory + mobile + TLS") { checkReady() }
        button("Inject TLS probe failure") { tlsProbe(injectFailure = true) }
        button("Shutdown + release") { stopProbe() }
        button("Timeout + retain paused owner") { timeoutProbe() }
        button("Forced shutdown + release") { forcedProbe() }
        button("Run 100 lifecycle cycles") { cycles() }
        content.addView(ScrollView(this).apply { addView(detail) },
            LinearLayout.LayoutParams(-1, 0, 1f))
        setContentView(content)
        if (intent.getBooleanExtra("auto", false)) owner.execute { guarded { startProbe() } }
    }

    private fun guarded(action: () -> Unit) {
        try { action() } catch (error: Throwable) {
            if (cycleRunId != null) writeCycleResult("failed", false, error.javaClass.simpleName)
            File(filesDir, "spike-observe.json").delete()
            File(filesDir, "spike-welcome.json").delete()
            // This test contains no account/provider inputs or credentials.
            report("FAIL ${error.javaClass.simpleName}: ${error.message}")
            Log.e(TAG, "probe failed", error)
            try { socket?.close() } catch (_: Throwable) { }
            socket = null
            try { mobileSocket?.close() } catch (_: Throwable) { }
            mobileSocket = null
            try { NativeDaemon.nativeRelease() } catch (_: Throwable) { }
        }
    }

    private fun writeProbeRecord(name: String, attempt: String, outcome: String,
                                 result: JSONObject? = null, failure: String? = null) {
        val record = JSONObject().put("probe", name).put("attempt_id", attempt)
            .put("spike_run_id", probeRunId).put("status", outcome).put("passed", outcome == "passed")
            .put("native_identity", nativeIdentity?.let { JSONObject(it) } ?: JSONObject.NULL)
            .put("daemon_generation", JSONObject(NativeDaemon.nativeObserve()).getLong("daemon_generation"))
        if (result != null) record.put("result", result)
        if (failure != null) record.put("failure", failure)
        File(filesDir, "spike-$name.json").writeText(record.toString(2))
    }

    private fun recordedProbe(name: String, action: () -> JSONObject) {
        val attempt = UUID.randomUUID().toString()
        writeProbeRecord(name, attempt, "running")
        try {
            writeProbeRecord(name, attempt, "passed", action())
        } catch (error: Throwable) {
            writeProbeRecord(name, attempt, "failed", failure = error.javaClass.simpleName)
            throw error
        }
    }

    private fun paths(): JSONObject {
        val root = File(filesDir, "haider").canonicalFile
        val store = File(root, "profiles/default")
        val runtime = File(root, "runtime/android-default")
        val all = linkedMapOf(
            "store_dir" to store, "runtime_dir" to runtime,
            "logs_dir" to File(root, "logs"),
            "workspace_dir" to File(store, "workspace"),
            "tmp_dir" to File(runtime, "tmp"),
        )
        return JSONObject().put("profile_id", "android-default").also { json ->
            all.forEach { (key, file) ->
                check(file.mkdirs() || file.isDirectory)
                Os.chmod(file.path, 448) // 0700
                json.put(key, file.canonicalPath)
            }
            File(filesDir, "spike-paths.json").writeText(json.toString(2))
        }
    }

    private fun startProbe() {
        probeRunId = UUID.randomUUID().toString()
        File(filesDir, "spike-observe.json").delete()
        File(filesDir, "spike-welcome.json").delete()
        nativeIdentity = NativeDaemon.nativeVersion()
        for (name in listOf("vault", "mobile", "inventory", "tls")) {
            writeProbeRecord(name, UUID.randomUUID().toString(), "not_run")
        }
        check(socket == null) { "Stop the active probe first" }
        report("LOAD run_id=$probeRunId $nativeIdentity")
        val config = paths().toString()
        if (lastGeneration == 0L) negativeCases(config)
        check(NativeDaemon.nativeInit(applicationContext, config) == 0) { "nativeInit failed" }
        check(NativeDaemon.nativeInit(applicationContext, config) == 0) { "init not idempotent" }
        if (!vaultChecked) wrongDekProbe(config)
        val key = ByteArray(32) { 42 } // Synthetic bytes, never a real vault key.
        val result = NativeDaemon.nativeStart(key, POLICY)
        check(key.all { it == 0.toByte() }) { "Java key array not cleared" }
        check(result == 0) { "nativeStart=$result" }
        val overlappingKey = ByteArray(32) { 42 }
        check(NativeDaemon.nativeStart(overlappingKey, POLICY) == 1) { "double start accepted" }
        check(overlappingKey.all { it == 0.toByte() })
        report("START status=$result; DEK cleared; duplicate start rejected")
        val deadline = android.os.SystemClock.elapsedRealtime() + 30000
        var snapshot: JSONObject
        do {
            snapshot = JSONObject(NativeDaemon.nativeObserve())
            if (snapshot.getString("phase") == "Failed") error(snapshot.toString())
            if (snapshot.getString("phase") == "Ready") break
            check(!snapshot.has("endpoint_path")) { "endpoint before Ready" }
            check(android.os.SystemClock.elapsedRealtime() < deadline) { "Ready deadline: $snapshot" }
            Thread.sleep(50)
        } while (true)
        snapshot.put("spike_run_id", probeRunId)
        File(filesDir, "spike-observe.json").writeText(snapshot.toString(2))
        endpoint = File(snapshot.getString("endpoint_path"))
        val runtime = endpoint!!.parentFile!!
        val dirStat = Os.stat(runtime.path)
        val socketStat = Os.stat(endpoint!!.path)
        check(dirStat.st_mode and 511 == 448) { "runtime mode is not 0700" }
        check(socketStat.st_mode and 511 == 384) { "socket mode is not 0600" }
        check(dirStat.st_uid == Process.myUid() && socketStat.st_uid == Process.myUid())
        report("READY $snapshot; runtime=0700 socket=0600 uid=${Process.myUid()}")
        val connection = LocalSocket()
        socket = connection
        connection.connect(LocalSocketAddress(endpoint!!.path, LocalSocketAddress.Namespace.FILESYSTEM))
        // LocalSocket creates its descriptor lazily in connect().
        connection.soTimeout = 10000
        val peer = connection.peerCredentials
        check(peer.uid == Process.myUid()) { "wrong peer UID" }
        check(peer.pid == Process.myPid()) { "daemon is not embedded in this host process" }
        val hello = assets.open("hello.json").use { it.readBytes() }
        val prefix = ByteBuffer.allocate(4).putInt(hello.size).array()
        // Deliberately fragment the prefix; the Rust streaming decoder must
        // handle reads that do not coincide with frame boundaries.
        connection.outputStream.write(prefix, 0, 2)
        connection.outputStream.flush()
        connection.outputStream.write(prefix, 2, 2)
        connection.outputStream.write(hello)
        connection.outputStream.flush()
        val welcome = readFrame(connection)
        check(welcome.getString("kind") == "welcome") { "$welcome" }
        check(welcome.getInt("v") == 1 && welcome.getInt("protocol") == 1)
        check(welcome.getInt("frame_limit") == 8388608)
        check(welcome.getString("lifecycle_phase") == "ready")
        check(welcome.getString("daemon_version") == JSONObject(NativeDaemon.nativeVersion()).getString("daemon_version"))
        val generation = welcome.getLong("daemon_generation")
        check(generation > lastGeneration) { "generation did not advance" }
        check(generation == snapshot.getLong("daemon_generation")) { "observe/welcome generation mismatch" }
        // Every View connection receives this baseline after Welcome, even
        // with no resident session. Consume it before the later drain notice.
        val binding = readFrame(connection)
        check(binding.getInt("v") == 1 && binding.getString("kind") == "resident_session_binding") {
            "missing resident binding baseline"
        }
        val workerGeneration = binding.getLong("worker_generation")
        check(workerGeneration > 0) { "invalid binding worker generation" }
        check(binding.isNull("session_id") && binding.isNull("binding_token")) { "fixture is not unbound" }
        report("BINDING baseline unbound=true worker_generation=$workerGeneration")
        lastGeneration = generation
        welcome.put("spike_run_id", probeRunId)
        welcome.put("binding_worker_generation", workerGeneration)
        File(filesDir, "spike-welcome.json").writeText(welcome.toString(2))
        report("WELCOME daemon_version=${welcome.getString("daemon_version")} lifecycle_phase=ready daemon_generation=$generation peer_pid=${peer.pid} peer_uid=${peer.uid}")
    }

    private fun negativeCases(config: String) {
        val uninitializedKey = ByteArray(32) { 42 }
        check(NativeDaemon.nativeStart(uninitializedKey, POLICY) == 2) { "start before init accepted" }
        check(uninitializedKey.all { it == 0.toByte() })
        val throwing = object : ContextWrapper(applicationContext) {
            override fun getApplicationContext(): Context = throw IllegalStateException("spike-context-exception")
        }
        check(NativeDaemon.nativeInit(throwing, config) == 7) { "Java exception did not map to INTERNAL" }
        check(JSONObject(NativeDaemon.nativeVersion()).getInt("jni_version") == 1)
        check(NativeDaemon.nativeInit(applicationContext, "{}") == 3) { "missing paths accepted" }
        val tooLong = JSONObject(config)
        val runtime = File(filesDir.canonicalFile, "r".repeat(87 - filesDir.canonicalPath.length - 1))
        check(runtime.path.toByteArray(Charsets.UTF_8).size == 87)
        check(File(runtime, "tmp").mkdirs() || File(runtime, "tmp").isDirectory)
        tooLong.put("runtime_dir", runtime.path).put("tmp_dir", File(runtime, "tmp").path)
        check(NativeDaemon.nativeInit(applicationContext, tooLong.toString()) == 3) {
            "non-C3 runtime layout was accepted"
        }
        check(!File(runtime, "h.sock").exists())
        check(NativeDaemon.nativeInit(applicationContext, config) == 0)
        val differentContext = object : ContextWrapper(applicationContext) {
            override fun getApplicationContext(): Context = this
        }
        check(NativeDaemon.nativeInit(differentContext, config) == 10) { "different context adopted" }
        val malformedKey = ByteArray(33) { 42 }
        check(NativeDaemon.nativeStart(malformedKey, POLICY) == 5)
        check(malformedKey.all { it == 0.toByte() })
        val badPolicyKey = ByteArray(32) { 42 }
        check(NativeDaemon.nativeStart(badPolicyKey, "{}") == 4)
        check(badPolicyKey.all { it == 0.toByte() })
        check(NativeDaemon.nativeShutdown(false, -1) == 10)
        report("NEGATIVE PASS different_context=10 negative_deadline=10 throwing_context=7 process_survived=true uninitialized=2 missing_paths=3 non_c3_layout=3 invalid_key=5 bad_policy=4")
    }

    private fun checkReady() {
        val snapshot = JSONObject(NativeDaemon.nativeObserve())
        check(snapshot.getString("phase") == "Ready")
        val expected = setOf("jni_version", "phase", "daemon_generation", "ready_since_unix_ms", "endpoint_path")
        check(snapshot.keys().asSequence().toSet() == expected) { "unexpected observe keys" }
        val begin = android.os.SystemClock.elapsedRealtimeNanos()
        repeat(100) { check(JSONObject(NativeDaemon.nativeObserve()).getString("phase") == "Ready") }
        val elapsed = (android.os.SystemClock.elapsedRealtimeNanos() - begin) / 1000000
        check(elapsed < 1000) { "observe blocked for $elapsed ms" }
        observations++
        if (mobileSocket == null) mobileProbe()
        inventoryProbe()
        if (cycleRunId == null) tlsProbe()
        report("CONTRACT PASS observation=$observations observe_100_ms=$elapsed inventory=true mobile=true")
    }

    private fun stopProbe() {
        val result = NativeDaemon.nativeShutdown(false, 7000)
        check(result == 0) { "shutdown=$result" }
        socket?.let { connection ->
            var draining: JSONObject
            do { draining = readFrame(connection) } while (draining.getString("kind") != "server_draining")
            check(connection.inputStream.read() == -1) { "no EOF" }
            connection.close()
        }
        socket = null
        mobileSocket?.let { connection ->
            check(connection.inputStream.read() == -1) { "mobile socket has no EOF" }
            connection.close()
        }
        mobileSocket = null
        check(!File(endpoint!!.parentFile, "mobile.sock").exists()) { "stale mobile.sock" }
        check(endpoint?.exists() != true) { "stale h.sock after graceful shutdown" }
        val snapshot = NativeDaemon.nativeObserve()
        check(JSONObject(snapshot).getString("phase") == "Stopped")
        NativeDaemon.nativeRelease()
        NativeDaemon.nativeRelease() // Safe after any state, including released.
        report("STOPPED status=$result ServerDraining/EOF socket_removed=true released=true $snapshot")
    }

    private fun timeoutProbe() = recordedProbe("timeout") {
        // The verifier has attached a tracer to ONLY haider-android-owner and
        // observed its stopped state. Java/the daemon's worker threads remain
        // live. No production native test hook or timing race supplies the hold.
        val before = JSONObject(NativeDaemon.nativeObserve())
        check(before.getString("phase") == "Ready") { "start a fresh native owner first" }
        check(NativeDaemon.nativeShutdown(true, 1) == 8) { "owner was not held" }
        val releaseBegin = android.os.SystemClock.elapsedRealtime()
        NativeDaemon.nativeRelease() // Its bounded 7s wait must also time out.
        val releaseElapsed = android.os.SystemClock.elapsedRealtime() - releaseBegin
        check(releaseElapsed in 6500..9000) { "release wait was $releaseElapsed ms" }
        val retained = JSONObject(NativeDaemon.nativeObserve())
        check(retained.getString("phase") == "Failed" && retained.getString("error_code") == "SHUTDOWN_TIMEOUT")
        check(retained.getLong("daemon_generation") == before.getLong("daemon_generation"))
        check(!retained.has("endpoint_path"))
        val key = ByteArray(32) { 42 }
        check(NativeDaemon.nativeStart(key, POLICY) == 1) { "timeout allowed a second runtime" }
        check(key.all { it == 0.toByte() }) { "rejected restart retained Java key bytes" }
        check(NativeDaemon.nativeInit(applicationContext, paths().toString()) == 0) { "context was released under a live owner" }
        val differentContext = object : android.content.ContextWrapper(applicationContext) {
            override fun getApplicationContext(): android.content.Context = this
        }
        check(NativeDaemon.nativeInit(differentContext, paths().toString()) == 10) { "retained application context was replaced" }
        report("TIMEOUT PASS status=8 release_retained=true restart=1 array_cleared=true $retained")
        JSONObject().put("observation", retained).put("release_retained", true)
            .put("restart_status", 1).put("java_array_cleared", true)
            .put("release_elapsed_ms", releaseElapsed).put("different_context_status", 10)
    }

    private fun forcedProbe() = recordedProbe("forced") {
        // If the timeout probe preceded this, the verifier detaches its tracer
        // first. This observes the actual forced outcome of the retained join.
        check(NativeDaemon.nativeShutdown(true, 7000) == 9) { "native owner did not report actual forced shutdown" }
        socket?.close()
        socket = null
        mobileSocket?.close()
        mobileSocket = null
        check(endpoint?.exists() != true)
        check(!File(checkNotNull(endpoint).parentFile, "mobile.sock").exists())
        val terminal = JSONObject(NativeDaemon.nativeObserve())
        check(terminal.getString("phase") == "Stopped")
        check(!terminal.has("endpoint_path"))
        NativeDaemon.nativeRelease()
        NativeDaemon.nativeRelease()
        report("FORCED PASS status=9 socket_removed=true released=true $terminal")
        JSONObject().put("status", 9).put("observation", terminal).put("socket_removed", true)
    }

    private fun cycles() {
        cycleRunId = UUID.randomUUID().toString()
        completedCycles = 0
        writeCycleResult("running", false)
        check(socket == null) { "Stop the active probe first" }
        val start = android.os.SystemClock.elapsedRealtime()
        repeat(100) { index ->
            if (index + 1 == intent.getIntExtra("fail_cycle_at", 0)) error("Injected cycle failure")
            startProbe()
            checkReady()
            stopProbe()
            completedCycles = index + 1
            writeCycleResult("running", false)
            report("CYCLE ${index + 1}/100 PASS")
        }
        val summary = writeCycleResult("completed", true)
            .put("elapsed_ms", android.os.SystemClock.elapsedRealtime() - start)
        File(filesDir, "spike-cycles.json").writeText(summary.toString(2))
        report("100 CYCLES PASS $summary")
        cycleRunId = null
    }

    private fun writeCycleResult(state: String, passed: Boolean, error: String? = null): JSONObject {
        val summary = JSONObject().put("run_id", cycleRunId).put("state", state)
            .put("cycles", completedCycles).put("passed", passed)
            .put("native_identity", nativeIdentity).put("error", error)
            .put("route_observations", observations).put("last_generation", lastGeneration)
        File(filesDir, "spike-cycles.json").writeText(summary.toString(2))
        return summary
    }

    private fun readFrame(connection: LocalSocket): JSONObject {
        val input = DataInputStream(connection.inputStream)
        val size = input.readInt()
        check(size in 1..8388608) { "invalid frame length $size" }
        val bytes = ByteArray(size)
        input.readFully(bytes)
        return JSONObject(String(bytes, Charsets.UTF_8))
    }

    private fun writeFrame(connection: LocalSocket, frame: JSONObject) {
        val bytes = frame.toString().toByteArray(Charsets.UTF_8)
        connection.outputStream.write(ByteBuffer.allocate(4).putInt(bytes.size).array())
        connection.outputStream.write(bytes)
        connection.outputStream.flush()
    }

    private fun rpc(body: JSONObject): JSONObject {
        val connection = checkNotNull(socket)
        val id = UUID.randomUUID().toString()
        writeFrame(connection, JSONObject().put("v", 1).put("kind", "request").put("request_id", id).put("body", body))
        while (true) {
            val frame = readFrame(connection)
            if (frame.optString("kind") == "response" && frame.optString("request_id") == id) return frame.getJSONObject("body")
        }
    }

    private fun wrongDekProbe(config: String) = recordedProbe("vault") {
        // Independent JCA encoder: all bytes belong exclusively to this disposable
        // package. This fixture is never an actual account or signing credential.
        val alias = "synthetic-dek-probe".toByteArray(Charsets.UTF_8)
        val vault = File(JSONObject(config).getString("store_dir"), "vault")
        check(vault.mkdirs() || vault.isDirectory)
        Os.chmod(vault.path, 448)
        val entry = File(vault, alias.joinToString("") { "%02x".format(it) } + ".vault")
        val cipher = javax.crypto.Cipher.getInstance("AES/GCM/NoPadding")
        val key = ByteArray(32) { 42 }
        val nonce = ByteArray(12).also { java.security.SecureRandom().nextBytes(it) }
        cipher.init(javax.crypto.Cipher.ENCRYPT_MODE, javax.crypto.spec.SecretKeySpec(key, "AES"), javax.crypto.spec.GCMParameterSpec(128, nonce))
        cipher.updateAAD("HAV1".toByteArray() + ByteBuffer.allocate(4).putInt(alias.size).array() + alias)
        val record = "HAV1".toByteArray() + nonce + cipher.doFinal("synthetic-only-probe".toByteArray())
        key.fill(0)
        java.io.FileOutputStream(entry).use { it.write(record); it.fd.sync() }
        Os.chmod(entry.path, 384)
        // A successful native recovery with the correct key is the positive
        // control. A directory-open failure must never masquerade as AEAD rejection.
        val correct = ByteArray(32) { 42 }
        check(NativeDaemon.nativeStart(correct, POLICY) == 0)
        check(correct.all { it == 0.toByte() })
        val correctDeadline = android.os.SystemClock.elapsedRealtime() + 30000
        var correctReady: JSONObject
        do {
            correctReady = JSONObject(NativeDaemon.nativeObserve())
            check(correctReady.getString("phase") != "Failed") { "correct DEK failed: $correctReady" }
            if (correctReady.getString("phase") == "Ready") break
            check(android.os.SystemClock.elapsedRealtime() < correctDeadline) { "correct DEK Ready timeout" }
            Thread.sleep(20)
        } while (true)
        check(correctReady.getLong("daemon_generation") > 0)
        check(entry.readBytes().contentEquals(record)) { "correct DEK changed ciphertext" }
        check(NativeDaemon.nativeShutdown(false, 7000) == 0)
        NativeDaemon.nativeRelease()
        check(NativeDaemon.nativeInit(applicationContext, config) == 0)
        val wrong = ByteArray(32) { 43 }
        check(NativeDaemon.nativeStart(wrong, POLICY) == 0)
        check(wrong.all { it == 0.toByte() })
        val deadline = android.os.SystemClock.elapsedRealtime() + 15000
        var failure: JSONObject
        do {
            failure = JSONObject(NativeDaemon.nativeObserve())
            check(failure.getString("phase") != "Ready") { "wrong DEK reached Ready" }
            if (failure.getString("phase") == "Failed") break
            check(android.os.SystemClock.elapsedRealtime() < deadline) { "wrong DEK timeout" }
            Thread.sleep(20)
        } while (true)
        check(failure.getString("error_code") == "VAULT_KEY_INVALID")
        check(!failure.has("endpoint_path"))
        check(!File(JSONObject(config).getString("runtime_dir"), "h.sock").exists())
        check(entry.readBytes().contentEquals(record)) { "wrong DEK changed ciphertext" }
        NativeDaemon.nativeRelease()
        check(NativeDaemon.nativeInit(applicationContext, config) == 0)
        vaultChecked = true
        report("VAULT PASS correct_dek_ready=true wrong_dek=VAULT_KEY_INVALID ciphertext_preserved=true array_cleared=true")
        JSONObject().put("correct_dek_ready", correctReady).put("wrong_dek", failure).put("ciphertext_preserved", true)
            .put("java_array_cleared", true).put("synthetic_only", true)
    }

    private fun mobileProbe() = recordedProbe("mobile") {
        val path = File(endpoint!!.parentFile, "mobile.sock")
        val stat = Os.stat(path.path)
        check(stat.st_mode and 511 == 384 && stat.st_uid == Process.myUid())
        val connection = LocalSocket()
        connection.connect(LocalSocketAddress(path.path, LocalSocketAddress.Namespace.FILESYSTEM))
        connection.soTimeout = 10000
        check(connection.peerCredentials.uid == Process.myUid())
        writeFrame(connection, JSONObject().put("id", 1).put("body", JSONObject().put("type", "hello").put("apkVersion", "971-1-diagnostic")))
        val hello = readFrame(connection)
        check(hello.getInt("id") == 1 && hello.getJSONObject("body").getString("type") == "authOk")
        check(hello.getJSONObject("body").getJSONArray("capabilities").length() == 7)
        mobileSocket = connection
        report("MOBILE PASS token_free=true peer_uid=${connection.peerCredentials.uid} mode=0600")
        hello
    }

    private fun inventoryProbe() = recordedProbe("inventory") {
        val create = rpc(JSONObject().put("method", "session.create").put("command_id", UUID.randomUUID().toString())
            .put("cwd", paths().getString("workspace_dir")).put("provider", "anthropic").put("model", "claude-opus-5").put("max_tokens", 4096))
        check(create.getString("method") == "session.create") { "synthetic session creation failed: $create" }
        val inventory = rpc(JSONObject().put("method", "tools.inventory").put("session_id", create.getString("session_id")))
        check(inventory.getString("method") == "tools.inventory") { "$inventory" }
        val tools = inventory.getJSONObject("inventory").getJSONArray("tools")
        val names = (0 until tools.length()).map { tools.getJSONObject(it).getJSONObject("manifest").getString("name") }.toSet()
        val excluded = setOf("process_exec", "exec", "task_output", "task_kill", "workflow_author", "computer", "peer_list", "peer_send", "ssh_list", "ssh_shell")
        check(names.intersect(excluded).isEmpty()) { "excluded tools in inventory" }
        check(names.containsAll(setOf("fs_read", "fs_write", "fs_edit", "fs_path", "monitor", "spawn_subagent")))
        report("INVENTORY PASS count=${names.size} excluded=0")
        inventory
    }

    private fun tlsProbe(injectFailure: Boolean = false) = recordedProbe("tls") {
        check(!injectFailure) { "synthetic failure after a previous successful probe" }
        // No API key is supplied. A typed 401/403 outcome requires the daemon's
        // Rust HTTP client to complete HTTPS through the emulator's system proxy.
        val result = rpc(JSONObject().put("method", "provider.models_probe").put("provider", "synthetic-tls-probe")
            .put("origin", "https://api.openai.com").put("api_family", "openai_chat_completions").put("keyless", true))
        check(result.getString("method") == "error" && result.optJSONObject("data")?.optString("failure") == "unauthorized") { "TLS probe inconclusive: $result" }
        report("TLS PASS provider.models_probe=unauthorized HTTPS completed without a credential")
        result
    }

    private fun report(message: String) {
        Log.i(TAG, message)
        File(filesDir, "spike-events.log").appendText("$message\n")
        runOnUiThread {
            status.text = when {
                message.startsWith("FAIL") -> "FAIL — see diagnostics"
                message.startsWith("TIMEOUT PASS") -> "Timed out — owner retained"
                message.startsWith("FORCED PASS") -> "Forced stop — owner released"
                message.startsWith("100 CYCLES PASS") -> "100 lifecycle cycles PASS"
                message.startsWith("STOPPED") -> "Stopped — EOF + socket removed"
                message.startsWith("WELCOME") -> "Ready — Hello / Welcome PASS"
                else -> status.text
            }
            detail.text = (message + "\n\n" + detail.text).take(6000)
        }
    }

    companion object {
        private const val TAG = "HaiderSpike"
        private const val POLICY = "{\"policy_version\":1,\"name\":\"android-standalone\",\"default_model\":\"claude-opus-5\",\"store_synchronous\":\"normal\"}"
    }
}
