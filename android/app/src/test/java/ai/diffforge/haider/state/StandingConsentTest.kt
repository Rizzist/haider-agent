package ai.diffforge.haider.state

import ai.diffforge.haider.ui.daemon.MenuOption
import ai.diffforge.haider.ui.daemon.NeedsInput
import ai.diffforge.haider.ui.state.PermissionMode
import ai.diffforge.haider.ui.state.StandingConsent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * What Auto is allowed to press (971-V F3).
 *
 * The old adapter simply disabled Auto, so nothing exercised the direction that
 * matters: the failure direction is always "ask".
 */
class StandingConsentTest {

    private fun card(
        kind: String = "permission",
        title: String = "Allow sms.list for the last 20 messages?",
        options: List<MenuOption> = listOf(
            MenuOption("allow", "Allow once", decision = "allow_once"),
            MenuOption("reject", "Don't allow", decision = "reject_once"),
        ),
        secret: Boolean = false,
        menuId: String? = "menu",
        requestSeq: Long? = 5,
        workerGeneration: Long? = 7,
    ) = NeedsInput(
        kind = kind,
        title = title,
        safeBody = emptyList(),
        menuId = menuId,
        requestSeq = requestSeq,
        workerGeneration = workerGeneration,
        options = options,
        secretAnswer = secret,
    )

    @Test
    fun `auto presses the allow-once option the card published`() {
        val choice = StandingConsent.choice(PermissionMode.Auto, card())
        assertEquals(StandingConsent.Choice("allow", 0), choice)
    }

    @Test
    fun `process execution never spends Android standing consent`() {
        for (title in listOf("Allow process_exec?", "Allow ProcessExec: sh -c true?", "Allow exec?", "Allow shell.exec?")) {
            assertNull(StandingConsent.choice(PermissionMode.Auto, card(title = title)))
        }
    }

    @Test
    fun `ask never answers anything`() {
        assertNull(StandingConsent.choice(PermissionMode.Ask, card()))
        assertFalse(StandingConsent.answers(PermissionMode.Ask, card()))
    }

    @Test
    fun `sending a text is never standing consent`() {
        // `sms.send` is deliberately absent from the covered set, and one
        // uncovered name in a card is enough to leave the whole card up.
        assertNull(StandingConsent.choice(PermissionMode.Auto, card(title = "Allow sms.send to Amir?")))
        assertNull(
            StandingConsent.choice(
                PermissionMode.Auto,
                card(
                    title = "Allow sms.list?",
                    options = listOf(MenuOption("allow", "Allow sms.send", decision = "allow_once")),
                ),
            ),
        )
    }

    @Test
    fun `a secret, a question and an unnamed approval are for a person`() {
        assertNull(StandingConsent.choice(PermissionMode.Auto, card(secret = true)))
        assertNull(StandingConsent.choice(PermissionMode.Auto, card(kind = "question")))
        assertNull(StandingConsent.choice(PermissionMode.Auto, card(title = "Are you sure?")))
    }

    @Test
    fun `allow-always is never pressed on someone's behalf`() {
        // A durable grant would outlive the mode the person chose, so a card
        // that offers nothing but `allow_always` waits for them.
        assertNull(
            StandingConsent.choice(
                PermissionMode.Auto,
                card(options = listOf(MenuOption("always", "Always allow", decision = "allow_always"))),
            ),
        )
    }

    @Test
    fun `a card missing a coordinate cannot be answered`() {
        assertNull(StandingConsent.choice(PermissionMode.Auto, card(menuId = null)))
        assertNull(StandingConsent.choice(PermissionMode.Auto, card(requestSeq = null)))
        assertNull(StandingConsent.choice(PermissionMode.Auto, card(workerGeneration = null)))
        assertNull(StandingConsent.choice(PermissionMode.Auto, null))
    }

    @Test
    fun `the index is the card's own, not a recomputed one`() {
        val choice = StandingConsent.choice(
            PermissionMode.Auto,
            card(
                options = listOf(
                    MenuOption("reject", "Don't allow", decision = "reject_once"),
                    MenuOption("allow", "Allow once", decision = "allow_once"),
                ),
            ),
        )
        assertEquals(StandingConsent.Choice("allow", 1), choice)
    }
}
