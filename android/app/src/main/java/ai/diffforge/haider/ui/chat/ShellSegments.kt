package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.daemon.ShellOutput
import ai.diffforge.haider.ui.daemon.ShellOutputStream
import java.nio.ByteBuffer
import java.nio.CharBuffer
import java.nio.charset.CharsetDecoder
import java.nio.charset.CodingErrorAction
import java.util.Base64

/** One decoded run of terminal output, attributed to a single stream. */
data class ShellSegment(val stream: ShellOutputStream, val text: String)

/**
 * Decodes the journal's interleaved chunk list into displayable runs.
 *
 * The daemon splits output on byte boundaries, so a UTF-8 code point can cross
 * two chunks of the SAME stream — FACADE-SHELL.md requires one continuous
 * decoder per stream. Bytes that end a chunk mid-code-point wait for that
 * stream's next chunk instead of turning into replacement characters, while
 * the emitted runs keep the durable `seq` interleaving of stdout and stderr.
 */
object ShellSegments {

    fun of(output: List<ShellOutput>): List<ShellSegment> {
        val segments = mutableListOf<ShellSegment>()
        val decoders = mutableMapOf<ShellOutputStream, CharsetDecoder>()
        val pending = mutableMapOf<ShellOutputStream, ByteArray>()

        fun append(stream: ShellOutputStream, text: String) {
            if (text.isEmpty()) return
            val last = segments.lastOrNull()
            if (last?.stream == stream) {
                segments[segments.size - 1] = last.copy(text = last.text + text)
            } else {
                segments += ShellSegment(stream, text)
            }
        }

        fun decoder(stream: ShellOutputStream): CharsetDecoder = decoders.getOrPut(stream) {
            Charsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPLACE)
                .onUnmappableCharacter(CodingErrorAction.REPLACE)
        }

        // The projection is already seq-ordered and deduplicated; the sort is a
        // cheap guarantee that this view never depends on that being true.
        output.sortedBy { it.seq }.forEach { chunk ->
            val bytes = (pending.remove(chunk.stream) ?: ByteArray(0)) +
                Base64.getDecoder().decode(chunk.chunkBase64)
            val input = ByteBuffer.wrap(bytes)
            val decoded = CharBuffer.allocate(bytes.size + 1)
            decoder(chunk.stream).decode(input, decoded, false)
            if (input.hasRemaining()) {
                pending[chunk.stream] = bytes.copyOfRange(input.position(), bytes.size)
            }
            decoded.flip()
            append(chunk.stream, decoded.toString())
        }

        // A tail that never completed is genuinely malformed: say so with the
        // replacement character rather than dropping the bytes silently.
        pending.forEach { (stream, rest) ->
            val input = ByteBuffer.wrap(rest)
            val decoded = CharBuffer.allocate(rest.size + 1)
            val tail = decoder(stream)
            tail.decode(input, decoded, true)
            tail.flush(decoded)
            decoded.flip()
            append(stream, decoded.toString())
        }
        return segments
    }
}
