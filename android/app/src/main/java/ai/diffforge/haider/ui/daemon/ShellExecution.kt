package ai.diffforge.haider.ui.daemon

/** Exact accepted coordinates. Cancellation must never borrow a newer session run. */
data class ShellExecutionRef(
    val sessionId: String,
    val commandId: String,
    val runId: String,
    val itemId: String,
    val workerGeneration: Long,
)

enum class ShellExecutionStatus { Running, Completed, Cancelled, Error, Reconnecting }
enum class ShellOutputStream { Stdout, Stderr }

/** Already-redacted journal bytes. Decode each stream continuously, not one UTF-8 chunk at a time. */
data class ShellOutput(val seq: Long, val stream: ShellOutputStream, val chunkBase64: String)

/** Full replacement projection; output is ordered/deduplicated by durable sequence. */
data class ShellExecution(
    val ref: ShellExecutionRef,
    val command: String,
    val status: ShellExecutionStatus,
    val output: List<ShellOutput> = emptyList(),
    val outputTruncated: Boolean = false,
    val exitCode: Int? = null,
    val error: String? = null,
) {
    /** Safe display helper preserving multibyte characters across journal chunks. */
    fun text(stream: ShellOutputStream): String {
        val bytes = java.io.ByteArrayOutputStream()
        output.filter { it.stream == stream }.forEach { bytes.write(java.util.Base64.getDecoder().decode(it.chunkBase64)) }
        return bytes.toString(Charsets.UTF_8.name())
    }
}
