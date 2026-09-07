package ai.diffforge.haider.ui.daemon

import ai.diffforge.haider.ui.chat.kind
import ai.diffforge.haider.ui.components.ForgeButtonKind
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class NeedsInputParsingTest {

    @Test
    fun `an unknown kind is tolerated and still renders the daemon's own copy`() {
        val grown = NeedsInput(
            kind = "a_kind_shipped_after_971",
            title = "Something new",
            safeBody = listOf("verbatim"),
        )
        // No enum to fail on: kind is a String precisely because
        // NeedsInputKindWire is #[serde(other)] and may grow.
        assertEquals("a_kind_shipped_after_971", grown.kind)
        assertEquals("Something new", grown.displayTitle)
    }

    @Test
    fun `an empty title falls back to the first safe body line`() {
        val prompt = NeedsInput(
            kind = "question",
            title = "",
            safeBody = listOf("Which branch should I push to?", "second line"),
        )
        assertEquals("Which branch should I push to?", prompt.displayTitle)
    }

    @Test
    fun `button style comes from decision, never from the label`() {
        assertEquals(ForgeButtonKind.Filled, MenuOption("k", "Delete everything", decision = "allow_once").kind())
        assertEquals(ForgeButtonKind.Ghost, MenuOption("k", "Allow", decision = "allow_always").kind())
        assertEquals(ForgeButtonKind.Ghost, MenuOption("k", "Allow", decision = "reject_once").kind())
        assertEquals(ForgeButtonKind.Destructive, MenuOption("k", "Yes please", decision = "reject_always").kind())
        assertEquals(ForgeButtonKind.Ghost, MenuOption("k", "Send it").kind())
    }

    @Test
    fun `a secret prompt is flagged so the card can refuse a text field`() {
        val secret = NeedsInput(kind = "secret", title = "Passphrase?", secretAnswer = true)
        assertTrue(secret.secretAnswer)
        assertTrue(secret.options.isEmpty())
    }

    @Test
    fun `absent optional coordinates stay absent`() {
        val sparse = NeedsInput(kind = "question", title = "?")
        assertEquals(null, sparse.menuId)
        assertEquals(null, sparse.requestSeq)
        assertEquals(null, sparse.workerGeneration)
        assertEquals(null, sparse.sinceMs)
    }
}
