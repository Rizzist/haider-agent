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
        button("Observe route") { checkReady() }
        button("Shutdown + release") { stopProbe() }
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
            try { socket?.close(); socket = null; NativeDaemon.nativeRelease() } catch (_: Throwable) { }
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
        check(socket == null) { "Stop the active probe first" }
        nativeIdentity = NativeDaemon.nativeVersion()
        report("LOAD run_id=$probeRunId $nativeIdentity")
        val config = paths().toString()
        if (lastGeneration == 0L) negativeCases(config)
        check(NativeDaemon.nativeInit(applicationContext, config) == 0) { "nativeInit failed" }
        check(NativeDaemon.nativeInit(applicationContext, config) == 0) { "init not idempotent" }
        val key = ByteArray(32) { 42 } // Synthetic bytes, never a real vault key.
        val result = NativeDaemon.nativeStart(key, POLICY)
        check(key.all { it == 0.toByte() }) { "Java key array not cleared" }
        check(result == 0) { "nativeStart=$result" }
        check(NativeDaemon.nativeStart(ByteArray(32), POLICY) == 1) { "double start accepted" }
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
        check(binding.getLong("worker_generation") == generation) { "binding generation mismatch" }
        check(binding.isNull("session_id") && binding.isNull("binding_token")) { "fixture is not unbound" }
        report("BINDING baseline unbound=true worker_generation=$generation")
        lastGeneration = generation
        welcome.put("spike_run_id", probeRunId)
        File(filesDir, "spike-welcome.json").writeText(welcome.toString(2))
        report("WELCOME daemon_version=${welcome.getString("daemon_version")} lifecycle_phase=ready daemon_generation=$generation peer_pid=${peer.pid} peer_uid=${peer.uid}")
    }

    private fun negativeCases(config: String) {
        check(NativeDaemon.nativeStart(ByteArray(32), POLICY) == 2) { "start before init accepted" }
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
            "108-byte staging socket path was accepted"
        }
        check(!File(runtime, "h.sock").exists())
        check(NativeDaemon.nativeInit(applicationContext, config) == 0)
        val malformedKey = ByteArray(33) { 42 }
        check(NativeDaemon.nativeStart(malformedKey, POLICY) == 5)
        check(malformedKey.all { it == 0.toByte() })
        check(NativeDaemon.nativeStart(ByteArray(32), "{}") == 4)
        report("NEGATIVE PASS throwing_context=7 process_survived=true uninitialized=2 missing_paths=3 staging_108_bytes=3 invalid_key=5 bad_policy=4")
    }

    private fun checkReady() {
        val snapshot = JSONObject(NativeDaemon.nativeObserve())
        check(snapshot.getString("phase") == "Ready")
        check(snapshot.getString("spike_route_status") in listOf("Available", "Unavailable")) { "ConnectivityManager query inconclusive: $snapshot" }
        observations++
        report("ROUTE observation=$observations $snapshot")
    }

    private fun stopProbe() {
        val result = NativeDaemon.nativeShutdown(false, 7000)
        check(result == 0) { "shutdown=$result" }
        socket?.let { connection ->
            val draining = readFrame(connection)
            check(draining.getString("kind") == "server_draining") { "$draining" }
            check(connection.inputStream.read() == -1) { "no EOF" }
            connection.close()
        }
        socket = null
        check(endpoint?.exists() != true) { "stale h.sock after graceful shutdown" }
        val snapshot = NativeDaemon.nativeObserve()
        check(JSONObject(snapshot).getString("phase") == "Stopped")
        NativeDaemon.nativeRelease()
        NativeDaemon.nativeRelease() // Safe after any state, including released.
        report("STOPPED status=$result ServerDraining/EOF socket_removed=true released=true $snapshot")
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
            // Expire the 250 ms route cache so every cycle exercises JNI.
            Thread.sleep(275)
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

    private fun report(message: String) {
        Log.i(TAG, message)
        File(filesDir, "spike-events.log").appendText("$message\n")
        runOnUiThread {
            status.text = when {
                message.startsWith("FAIL") -> "FAIL — see diagnostics"
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
        private const val POLICY = "{\"spike_use_default_dependencies\":true}"
    }
}
