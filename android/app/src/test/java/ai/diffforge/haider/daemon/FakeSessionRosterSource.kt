package ai.diffforge.haider.daemon

internal class FakeSessionRosterSource : SessionRosterSource {
    var endpoint: RpcEndpoint? = null
        private set
    private var observer: ((RosterUpdate) -> Unit)? = null
    override fun observe(endpoint: RpcEndpoint, observer: (RosterUpdate) -> Unit): AutoCloseable {
        this.endpoint = endpoint
        this.observer = observer
        return AutoCloseable { this.observer = null; this.endpoint = null }
    }
    fun emit(update: RosterUpdate) { observer?.invoke(update) }
}
