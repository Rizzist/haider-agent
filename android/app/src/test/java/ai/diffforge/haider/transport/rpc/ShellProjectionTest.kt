package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.ui.daemon.*
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import java.nio.file.Files
import java.util.Base64

class ShellProjectionTest {
    private fun envelope(seq: Long, payload: kotlinx.serialization.json.JsonObject) = obj(
        "session_id" to "s", "seq" to seq, "run_id" to "run", "worker_generation" to 7,
        "render" to obj("ui" to true), "payload" to payload)
    private fun item(status: String = "in_progress", exit: Int? = null) = obj("type" to "item", "event" to
        if (status == "in_progress") "started" else "completed", "item_id" to "command-item",
        "item" to obj("item" to "command_execution", "call_id" to "command", "command" to "printf fixture",
            "status" to status, "exit_code" to exit, "unreviewed" to "must-not-be-cached"))
    private fun output(bytes: ByteArray, stream: String = "stdout") = obj("type" to "item", "event" to "delta",
        "item_id" to "command-item", "delta" to obj("delta" to "command_output", "stream" to stream,
            "chunk_b64" to Base64.getEncoder().encodeToString(bytes)))

    @Test fun redactedReplayIsDeduplicatedDurableAndPreservesSplitUtf8() {
        val directory = Files.createTempDirectory("shell-cache").toFile()
        try {
            val cache = TranscriptCache(directory)
            assertTrue(cache.apply("s", envelope(1, item())))
            val utf8 = "é\n".toByteArray()
            cache.apply("s", envelope(2, output(utf8.copyOfRange(0, 1))))
            cache.apply("s", envelope(3, output(utf8.copyOfRange(1, utf8.size))))
            assertFalse(cache.apply("s", envelope(3, output("duplicate".toByteArray()))))
            cache.apply("s", envelope(4, output("redacted stderr".toByteArray(), "stderr")))
            assertEquals(ShellExecutionStatus.Reconnecting, ShellProjection.project("s", cache.entries("s"), false).single().status)
            cache.apply("s", envelope(5, item("completed", 7)))
            val command = ShellProjection.project("s", TranscriptCache(directory).entries("s"), true).single()
            assertEquals("é\n", command.text(ShellOutputStream.Stdout))
            assertEquals("redacted stderr", command.text(ShellOutputStream.Stderr))
            assertEquals(ShellExecutionStatus.Completed, command.status)
            assertEquals(7, command.exitCode)
            assertEquals(listOf(2L, 3L, 4L), command.output.map { it.seq })
            assertFalse(directory.walkTopDown().filter { it.isFile }.any { it.readText().contains("must-not-be-cached") })
        } finally { directory.deleteRecursively() }
    }

    @Test fun cancellationAndOutputClippingAreExplicit() {
        val entries = listOf(envelope(1, item()), envelope(2, output(ByteArray(ShellProjection.MAX_OUTPUT_BYTES + 1))),
            envelope(3, obj("type" to "run_state", "state" to "cancelled"))).mapIndexed { i, e ->
            TranscriptCache.Entry(i + 1L, TranscriptCache.displayProjection(e)) }
        val command = ShellProjection.project("s", entries, true).single()
        assertEquals(ShellExecutionStatus.Cancelled, command.status)
        assertTrue(command.outputTruncated)
        assertTrue(command.output.isEmpty())
    }

    @Test fun fakeShellUsesSetShellAndOnlyCompletesWhenDriven() = runBlocking {
        val service = FakeDaemonService()
        val session = service.sessions.value.first().id
        try { service.startShell(session, "first", "printf fixture"); fail("unavailable") }
        catch (_: IllegalStateException) { }
        service.setShell(ShellAvailability(available = true, sessionId = session))
        val ref = service.startShell(session, "first", "printf fixture")
        assertEquals(ref, service.startShell(session, "first", "printf fixture"))
        service.appendShellOutput(ref, "fixture\n")
        assertEquals(ShellExecutionStatus.Running, service.shellExecutions.value[session]!!.single().status)
        service.cancelShell(ref)
        assertEquals(ShellExecutionStatus.Cancelled, service.shellExecutions.value[session]!!.single().status)
        val second = service.startShell(session, "second", "exit 7")
        service.setShellResult(second, ShellExecutionStatus.Completed, exitCode = 7)
        assertEquals(7, service.shellExecutions.value[session]!!.last().exitCode)
    }
}
