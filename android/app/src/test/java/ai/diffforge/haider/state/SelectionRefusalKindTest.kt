package ai.diffforge.haider.state

import ai.diffforge.haider.ui.chat.CustomModelId
import ai.diffforge.haider.ui.state.SelectionRefusalCodes
import ai.diffforge.haider.ui.state.SelectionRefusalKind
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Which refusals a confirmation can actually fix.
 *
 * The panel offered "Change it anyway" for every refusal, because there was
 * only one refusal in front of it. A custom model id makes `model_unknown`
 * reachable from a tap, and confirming that one sends the identical request
 * with one more field — a button that promises a retry it cannot deliver.
 */
class SelectionRefusalKindTest {

    @Test
    fun `only a cache-epoch refusal is confirmable`() {
        assertEquals(
            SelectionRefusalKind.Confirmable,
            SelectionRefusalCodes.kind(SelectionRefusalCodes.CACHE_EPOCH_CONFIRMATION_REQUIRED),
        )
    }

    @Test
    fun `the daemon's four row refusals are terminal`() {
        listOf(
            SelectionRefusalCodes.MODEL_UNKNOWN,
            SelectionRefusalCodes.PROVIDER_UNAVAILABLE,
            SelectionRefusalCodes.EFFORT_UNSUPPORTED,
            SelectionRefusalCodes.FAST_UNSUPPORTED,
        ).forEach { code ->
            assertEquals(
                "$code must not offer a confirmation",
                SelectionRefusalKind.Terminal,
                SelectionRefusalCodes.kind(code),
            )
        }
    }

    @Test
    fun `a code inside a message is still recognised`() {
        // The facade surfaces `error.message`, which a real transport wraps.
        assertEquals(
            SelectionRefusalKind.Terminal,
            SelectionRefusalCodes.kind("model_unknown: router-deep is not in the inventory"),
        )
        assertEquals(
            SelectionRefusalKind.Confirmable,
            SelectionRefusalCodes.kind("refused (cache_epoch_confirmation_required)"),
        )
    }

    @Test
    fun `matching is on a whole token, not a substring`() {
        // `model_unknown_thing` is not `model_unknown`, and a longer code that
        // merely contains a known one must not inherit its classification.
        assertEquals(
            SelectionRefusalKind.Unrecognised,
            SelectionRefusalCodes.kind("model_unknown_thing"),
        )
    }

    @Test
    fun `an unknown refusal is never promoted to confirmable`() {
        // A lost connection, a newer daemon: nothing is claimed, and consent is
        // not offered for a refusal this client cannot read.
        assertEquals(SelectionRefusalKind.Unrecognised, SelectionRefusalCodes.kind("selection_refused"))
        assertEquals(SelectionRefusalKind.Unrecognised, SelectionRefusalCodes.kind(""))
        assertEquals(SelectionRefusalKind.Unrecognised, SelectionRefusalCodes.kind("connection lost"))
    }

    @Test
    fun `the codes are spelled as the daemon sends them`() {
        assertEquals("model_unknown", SelectionRefusalCodes.MODEL_UNKNOWN)
        assertEquals("provider_unavailable", SelectionRefusalCodes.PROVIDER_UNAVAILABLE)
        assertEquals("effort_unsupported", SelectionRefusalCodes.EFFORT_UNSUPPORTED)
        assertEquals("fast_unsupported", SelectionRefusalCodes.FAST_UNSUPPORTED)
        assertEquals(
            "cache_epoch_confirmation_required",
            SelectionRefusalCodes.CACHE_EPOCH_CONFIRMATION_REQUIRED,
        )
    }

    @Test
    fun `a custom model id refuses only what is this client's to refuse`() {
        assertTrue(CustomModelId.ok("llama3.1:8b"))
        assertTrue(CustomModelId.ok("  router-deep  "))
        // Emptiness and control bytes; everything else is the daemon's call.
        assertFalse(CustomModelId.ok(""))
        assertFalse(CustomModelId.ok("   "))
        assertFalse(CustomModelId.ok("router\ndeep"))
        assertFalse(CustomModelId.ok("router" + 9.toChar() + "deep"))
        // A space is NOT refused: the daemon owns which ids exist, and a
        // client-side guess about id shape is how a valid passthrough id
        // gets blocked before it ever reaches session.select_model.
        assertTrue(CustomModelId.ok("router deep"))
    }
}
