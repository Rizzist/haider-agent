package ai.diffforge.haider.transport.rpc

import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.JsonObject
import java.io.IOException
import java.security.MessageDigest
import java.util.UUID

/** Bounded process-memory receipts. A response-loss retry retains its command and original coordinates. */
internal class RpcCommands(private val client: RpcClient, private val send: suspend (JsonObject) -> JsonObject = { client.request(it) }) {
    private val mutex = Mutex()
    private val pending = mutableMapOf<String, JsonObject>()
    suspend fun execute(key: String, body: (String) -> JsonObject): JsonObject = mutex.withLock {
        val command = pending[key] ?: run {
            if (pending.size >= 64) throw IOException("unresolved_commands_limit")
            body(UUID.randomUUID().toString()).also { pending[key] = it }
        }
        try { send(command).also { pending.remove(key) } }
        catch (error: RpcRemoteException) { if (!error.retryable && error.code != "restage_required") pending.remove(key); throw error }
    }
}

internal fun operationKey(vararg values: String): String = MessageDigest.getInstance("SHA-256")
    .digest(values.joinToString("") { "${it.length}:$it" }.toByteArray())
    .joinToString("") { "%02x".format(it) }
