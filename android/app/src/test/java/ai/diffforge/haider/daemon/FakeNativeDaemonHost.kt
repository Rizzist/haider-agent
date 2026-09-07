package ai.diffforge.haider.daemon

/** Test-only native substitute: never installed in production dependencies. */
internal class FakeNativeDaemonHost : NativeDaemonHost {
    val calls = mutableListOf<String>()
    var versionJson = """{"jni_version":1,"daemon_version":"0.0.971","wire_protocol":1,"build_id":"test","abi":"x86_64"}"""
    @Volatile var observation = """{"jni_version":1,"phase":"Recovering","daemon_generation":41}"""
    var initStatus = NativeStatus.OK
    var startStatus = NativeStatus.OK
    var shutdownStatus = NativeStatus.OK
    @Volatile var passedDek: ByteArray? = null
    var onShutdown: () -> Unit = {}
    override fun version(): String { calls += "version"; return versionJson }
    override fun init(pathsJson: String): Int { calls += "init"; return initStatus }
    override fun start(vaultDek: ByteArray, policyJson: String): Int {
        check(vaultDek.size == 32 && vaultDek.any { it != 0.toByte() })
        calls += "start"
        passedDek = vaultDek
        return startStatus
    }
    override fun observe(): String { calls += "observe"; return observation }
    override fun shutdown(forced: Boolean, deadlineMs: Long): Int {
        check(!forced && deadlineMs == 7_000L)
        calls += "shutdown"
        onShutdown()
        return shutdownStatus
    }
    override fun release() { calls += "release" }
    fun ready() {
        observation = """{"jni_version":1,"phase":"Ready","daemon_generation":41,"endpoint_path":"/private/runtime/h.sock","ready_since_unix_ms":1000}"""
    }
    fun crash() { observation = """{"jni_version":1,"phase":"Stopped","daemon_generation":41}""" }
}
internal class FakeLifecycleStore(var state: PersistedLifecycle = PersistedLifecycle()) : DaemonLifecycleStore {
    val writes = mutableListOf<PersistedLifecycle>()
    override fun load() = state
    override fun save(state: PersistedLifecycle) { writes += state; this.state = state }
}
internal class FakeDaemonClock : DaemonClock {
    var wall = 1_000_000L
    var elapsed = 20_000L
    override fun unixMs() = wall
    override fun elapsedMs() = elapsed
    fun advance(ms: Long) { wall += ms; elapsed += ms }
}
