package ai.diffforge.haider.ui.daemon

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `WireFrame::MenuAnswer` (frame.rs:5926) is a compare-and-set, and its
 * identity is the whole coordinate set. Round 1 sent `menu_id` alone, dropping
 * the `request_seq` and `worker_generation` the card had already rendered.
 */
class MenuAnswerContractTest {

    private val prompt = NeedsInput(
        kind = "approval",
        title = "Send it?",
        menuId = "menu-9f2",
        requestSeq = 88,
        workerGeneration = 3,
    )

    @Test
    fun `coordinates require every field the frame carries`() {
        assertNotNullCoordinates(MenuCoordinates.of("s", prompt, "c1"))
        // Any missing coordinate means there is nothing to compare and set.
        assertNull(MenuCoordinates.of("s", prompt.copy(menuId = null), "c1"))
        assertNull(MenuCoordinates.of("s", prompt.copy(requestSeq = null), "c1"))
        assertNull(MenuCoordinates.of("s", prompt.copy(workerGeneration = null), "c1"))
        assertNull(MenuCoordinates.of("s", null, "c1"))
    }

    @Test
    fun `the frame matches the frozen field set`() {
        val coordinates = MenuCoordinates.of("session-7", prompt, "command-answer")!!
        val frame = MenuRpcAdapter.menuAnswerFrame(coordinates, "send", 0)
        assertEquals(1, frame.getInt("v"))
        assertEquals("menu_answer", frame.getString("kind"))
        assertEquals("command-answer", frame.getString("command_id"))
        assertEquals("session-7", frame.getString("session_id"))
        assertEquals("menu-9f2", frame.getString("menu_id"))
        assertEquals(88L, frame.getLong("request_seq"))
        assertEquals(3L, frame.getLong("worker_generation"))
        assertEquals("send", frame.getString("option_key"))
        assertEquals(0, frame.getInt("option_index"))
        assertFalse(frame.has("input"))
    }

    @Test
    fun `free text rides as MenuInput text`() {
        val frame = MenuRpcAdapter.menuAnswerFrame(
            MenuCoordinates.of("s", prompt, "c")!!,
            "",
            0,
            MenuAnswerInput.Text("develop"),
        )
        val input = frame.getJSONObject("input")
        assertEquals("text", input.getString("kind"))
        assertEquals("develop", input.getString("text"))
    }

    @Test
    fun `a secret rides as a vault reference and never as text`() {
        val frame = MenuRpcAdapter.menuAnswerFrame(
            MenuCoordinates.of("s", prompt, "c")!!,
            "",
            0,
            MenuAnswerInput.Secret("vaultref-menu-9"),
        )
        val input = frame.getJSONObject("input")
        // frame.rs:5814 — the raw secret must never appear in this frame.
        assertEquals("secret_vault_reference", input.getString("kind"))
        assertEquals("vaultref-menu-9", input.getString("vault_reference"))
        assertFalse(input.has("text"))
        assertFalse(frame.toString().contains("passphrase"))
    }

    @Test
    fun `the facade records the coordinates it was given`() = runTest {
        val service = FakeDaemonService(FakeScenario.InputRequiredHere)
        val row = service.sessions.value.first { it.id == "s-sms" }
        val coordinates = MenuCoordinates.of("s-sms", row.needsInput, "c1")!!
        service.answer(coordinates, "send", 0)
        assertTrue(
            service.calls.contains("menu.answer:s-sms:menu-9f2:88:3:send:0:none"),
        )
    }

    @Test
    fun `a secret answer stages first and sends only the reference`() = runTest {
        val service = FakeDaemonService(FakeScenario.InputRequiredHere)
        val row = service.sessions.value.first { it.id == "s-sms" }
        val coordinates = MenuCoordinates.of("s-sms", row.needsInput, "c1")!!
        val secret = "hunter2-hunter2".toCharArray()
        val reference = service.stageMenuSecret(secret)
        service.answer(coordinates, "reply", 0, MenuAnswerInput.Secret(reference))
        assertTrue(service.calls.contains("vault.stage"))
        assertTrue(service.calls.any { it.endsWith(":secret:$reference") })
        // No recorded call carries the plaintext.
        assertTrue(service.calls.none { it.contains("hunter2") })
    }

    private fun assertNotNullCoordinates(coordinates: MenuCoordinates?) {
        assertTrue("coordinates should resolve from a complete prompt", coordinates != null)
    }
}
