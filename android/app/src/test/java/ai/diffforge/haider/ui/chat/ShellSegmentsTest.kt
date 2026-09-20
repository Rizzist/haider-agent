package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.daemon.ShellOutput
import ai.diffforge.haider.ui.daemon.ShellOutputStream
import java.util.Base64
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * FACADE-SHELL.md's decoding rule: base64 chunks are decoded incrementally
 * with ONE UTF-8 decoder per stream, because the journal splits on byte
 * boundaries and a code point can cross chunks — while the rendered runs keep
 * the durable seq interleaving of stdout and stderr.
 */
class ShellSegmentsTest {

    private fun chunk(seq: Long, stream: ShellOutputStream, bytes: ByteArray) =
        ShellOutput(seq, stream, Base64.getEncoder().encodeToString(bytes))

    private fun chunk(seq: Long, stream: ShellOutputStream, text: String) =
        chunk(seq, stream, text.toByteArray(Charsets.UTF_8))

    @Test
    fun `a code point split across chunks decodes once its tail arrives`() {
        val smile = "🙂".toByteArray(Charsets.UTF_8) // four bytes
        val segments = ShellSegments.of(
            listOf(
                chunk(1, ShellOutputStream.Stdout, "ok ".toByteArray() + smile.copyOfRange(0, 2)),
                chunk(2, ShellOutputStream.Stdout, smile.copyOfRange(2, 4) + "!".toByteArray()),
            ),
        )
        assertEquals(listOf(ShellSegment(ShellOutputStream.Stdout, "ok 🙂!")), segments)
    }

    @Test
    fun `stderr interleaves in seq order with its own attribution`() {
        val segments = ShellSegments.of(
            listOf(
                chunk(1, ShellOutputStream.Stdout, "a\n"),
                chunk(2, ShellOutputStream.Stderr, "E\n"),
                chunk(3, ShellOutputStream.Stdout, "b\n"),
            ),
        )
        assertEquals(
            listOf(
                ShellSegment(ShellOutputStream.Stdout, "a\n"),
                ShellSegment(ShellOutputStream.Stderr, "E\n"),
                ShellSegment(ShellOutputStream.Stdout, "b\n"),
            ),
            segments,
        )
    }

    @Test
    fun `a split code point survives a stderr chunk between its halves`() {
        val smile = "🙂".toByteArray(Charsets.UTF_8)
        val segments = ShellSegments.of(
            listOf(
                chunk(1, ShellOutputStream.Stdout, "ok ".toByteArray() + smile.copyOfRange(0, 2)),
                chunk(2, ShellOutputStream.Stderr, "warn"),
                chunk(3, ShellOutputStream.Stdout, smile.copyOfRange(2, 4)),
            ),
        )
        // The completed code point lands where its final bytes arrived; the
        // stderr run in between keeps its place and its stream.
        assertEquals(
            listOf(
                ShellSegment(ShellOutputStream.Stdout, "ok "),
                ShellSegment(ShellOutputStream.Stderr, "warn"),
                ShellSegment(ShellOutputStream.Stdout, "🙂"),
            ),
            segments,
        )
    }

    @Test
    fun `adjacent same-stream chunks merge into one run`() {
        val segments = ShellSegments.of(
            listOf(
                chunk(1, ShellOutputStream.Stdout, "one "),
                chunk(2, ShellOutputStream.Stdout, "two"),
            ),
        )
        assertEquals(listOf(ShellSegment(ShellOutputStream.Stdout, "one two")), segments)
    }

    @Test
    fun `a tail that never completes becomes replacement characters, not silence`() {
        val smile = "🙂".toByteArray(Charsets.UTF_8)
        val segments = ShellSegments.of(
            listOf(chunk(1, ShellOutputStream.Stdout, "hi".toByteArray() + smile.copyOfRange(0, 2))),
        )
        val only = segments.single()
        assertEquals(ShellOutputStream.Stdout, only.stream)
        val tail = only.text.removePrefix("hi")
        assertTrue("truncated bytes must surface as U+FFFD: '${only.text}'",
            tail.isNotEmpty() && tail.all { it == '�' })
    }

    @Test
    fun `no output decodes to no segments`() {
        assertEquals(emptyList<ShellSegment>(), ShellSegments.of(emptyList()))
    }
}
