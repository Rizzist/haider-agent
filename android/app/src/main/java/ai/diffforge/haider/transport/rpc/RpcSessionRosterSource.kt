package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.daemon.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import java.io.File

/** The foreground service's independent View-only connection; no transcript attachment. */
class RpcSessionRosterSource(private val cacheDirectory: File, private val version: String) : SessionRosterSource {
    override fun observe(endpoint: RpcEndpoint, observer: (RosterUpdate) -> Unit): AutoCloseable {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val client = RpcClient(scope)
        val roster = SessionRosterRepository(client, scope, TranscriptCache(cacheDirectory))
        var baseline = false
        scope.launch {
            combine(roster.ready, roster.sessions, client.state, client.connectionEpochs) { _, rows, _, _ -> rows }
                .collect { rows ->
                    if (!roster.isReady()) {
                        baseline = false
                        observer(RosterUpdate.Reset)
                    } else {
                        val mapped = rows.map { row ->
                            val input = row.needsInput?.let(RpcUiMapping::needsInput)
                            NotificationSession(row.sessionId, row.headSeq, row.workerGeneration, row.runId,
                                row.runState, if (input?.menuId != null && input.requestSeq != null && input.workerGeneration != null)
                                    NotificationInput(input.menuId, input.requestSeq, input.workerGeneration) else null)
                        }
                        observer(if (baseline) RosterUpdate.Changes(mapped) else RosterUpdate.Baseline(mapped))
                        baseline = true
                    }
                }
        }
        val connection = StandaloneRpcConnection(scope, MutableStateFlow(RpcTarget(endpoint.path,
            endpoint.wireProtocol, endpoint.daemonGeneration, version)), client, control = false)
        return AutoCloseable { connection.close(); roster.close(); scope.cancel() }
    }
}
