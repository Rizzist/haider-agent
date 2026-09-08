package ai.diffforge.haider.state

import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.transport.SessionSelection
import ai.diffforge.haider.ui.daemon.DaemonInfo
import ai.diffforge.haider.ui.daemon.DaemonStatus
import ai.diffforge.haider.ui.state.ModelChipState
import ai.diffforge.haider.ui.state.ModelChipStateMachine
import ai.diffforge.haider.ui.state.ModelNames
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The 970 chip could sit on "loading…" forever. These pin the deadline that
 * makes that impossible.
 */
class ModelChipStateTest {
    private val running = DaemonStatus.Running(DaemonInfo(version = "0.0.970", generation = 1))
    private val requestedAt = 1_000_000L

    private fun config(model: String = "claude-sonnet-4-5") = SessionConfig(
        catalogRevision = 1,
        catalogAvailable = true,
        unavailableReason = null,
        current = SessionSelection("s", "anthropic", model, "high"),
        providers = emptyList(),
    )

    private fun resolve(
        daemon: DaemonStatus = running,
        config: SessionConfig? = null,
        error: String? = null,
        busy: Boolean = false,
        requestedAtMs: Long? = requestedAt,
        nowMs: Long = requestedAt,
    ) = ModelChipStateMachine.resolve(daemon, config, error, busy, requestedAtMs, nowMs)

    @Test
    fun `a resolved catalog names the model`() {
        val state = resolve(config = config()) as ModelChipState.Resolved
        assertEquals("Sonnet 4.5", state.shortModel)
        assertEquals("claude-sonnet-4-5", state.fullModel)
        assertEquals("high", state.effort)
    }

    @Test
    fun `inside the deadline it is loading`() {
        assertEquals(
            ModelChipState.Loading,
            resolve(nowMs = requestedAt + ModelChipStateMachine.DEADLINE_MS - 1),
        )
    }

    @Test
    fun `the virtual clock past six seconds flips skeleton to error`() {
        assertTrue(
            resolve(nowMs = requestedAt + ModelChipStateMachine.DEADLINE_MS)
                is ModelChipState.Error,
        )
    }

    @Test
    fun `a late success after the deadline still resolves`() {
        assertTrue(
            resolve(
                config = config(),
                nowMs = requestedAt + 10 * ModelChipStateMachine.DEADLINE_MS,
            ) is ModelChipState.Resolved,
        )
    }

    @Test
    fun `no daemon reads as start haider, not as loading`() {
        assertEquals(ModelChipState.DaemonDown, resolve(daemon = DaemonStatus.Stopped))
        assertEquals(ModelChipState.DaemonDown, resolve(daemon = DaemonStatus.Stopping))
        assertEquals(
            ModelChipState.DaemonDown,
            resolve(daemon = DaemonStatus.Failed("boom", null)),
        )
    }

    @Test
    fun `a catalog error reads as retry`() {
        val state = resolve(error = "catalog unavailable") as ModelChipState.Error
        assertEquals("catalog unavailable", state.message)
    }

    @Test
    fun `a change in flight says changing`() {
        assertEquals(ModelChipState.Changing, resolve(config = config(), busy = true))
    }

    @Test
    fun `with nothing in flight the chip offers a retry, not an endless wait`() {
        // There is no request to wait for, so "loading" would be a lie that
        // never resolves. This is the first of the two escapes the verifier
        // found.
        assertTrue(
            resolve(requestedAtMs = null, nowMs = Long.MAX_VALUE / 2) is ModelChipState.Error,
        )
        assertTrue(resolve(requestedAtMs = null, nowMs = requestedAt) is ModelChipState.Error)
    }

    @Test
    fun `a selection that never returns expires like a catalog that never arrives`() {
        // The second escape: selectionBusy used to short-circuit the deadline.
        assertEquals(
            ModelChipState.Changing,
            resolve(busy = true, nowMs = requestedAt + ModelChipStateMachine.DEADLINE_MS - 1),
        )
        assertTrue(
            resolve(busy = true, nowMs = requestedAt + ModelChipStateMachine.DEADLINE_MS)
                is ModelChipState.Error,
        )
    }
}

class ModelNamesTest {
    @Test
    fun `provider prefix and date suffix are stripped`() {
        assertEquals("Sonnet 4.5", ModelNames.short("claude-sonnet-4-5"))
        assertEquals("Opus 4.1", ModelNames.short("anthropic/claude-opus-4-1-20250805"))
        assertEquals("Sonnet 4.5", ModelNames.short("claude-sonnet-4-5-latest"))
    }

    @Test
    fun `vendor initialisms stay upper case`() {
        assertEquals("GPT 5", ModelNames.short("gpt-5"))
        assertEquals("GLM 4.6", ModelNames.short("glm-4-6"))
    }

    @Test
    fun `an unknown shape survives unchanged enough to read`() {
        assertEquals("Something Odd", ModelNames.short("something-odd"))
        assertEquals("", ModelNames.short(null))
        assertEquals("", ModelNames.short(""))
    }

    @Test
    fun `the full id is kept for the secondary line`() {
        assertEquals(
            "anthropic / claude-sonnet-4-5 · high",
            ModelNames.full("anthropic", "claude-sonnet-4-5", "high"),
        )
        assertEquals("anthropic / claude-sonnet-4-5", ModelNames.full("anthropic", "claude-sonnet-4-5", null))
    }

    @Test
    fun `an absent token count stays absent rather than becoming zero`() {
        assertEquals(null, ModelNames.tokens(null))
        assertEquals("0", ModelNames.tokens(0))
        assertEquals("18.4k", ModelNames.tokens(18_400))
        assertEquals("1.2M", ModelNames.tokens(1_200_000))
    }
}
