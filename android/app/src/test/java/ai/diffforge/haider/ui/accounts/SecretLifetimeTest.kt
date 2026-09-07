package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The finding was not "the wipe is missing" but "the wipe is unreachable": a
 * `wipe()` written after a suspending call never runs when that call is
 * cancelled, which is exactly what happens when the user taps back mid-save.
 */
class SecretLifetimeTest {

    @Test
    fun `use wipes its copy on success`() {
        val buffer = SecretBuffer()
        buffer.set("sk-live-abcdefgh9999")
        var seen: CharArray? = null
        buffer.use { seen = it }
        assertTrue("the copy survived the block", seen!!.all { it == ' ' })
    }

    @Test
    fun `use wipes its copy when the block throws`() {
        val buffer = SecretBuffer()
        buffer.set("sk-live-abcdefgh9999")
        var seen: CharArray? = null
        runCatching {
            buffer.use {
                seen = it
                error("provider refused")
            }
        }
        assertTrue("a thrown block left the secret in memory", seen!!.all { it == ' ' })
    }

    @Test
    fun `use wipes its copy when the block is cancelled`() = runTest {
        val buffer = SecretBuffer()
        buffer.set("sk-live-abcdefgh9999")
        var seen: CharArray? = null
        runCatching {
            buffer.use {
                seen = it
                throw CancellationException("user tapped back")
            }
        }
        assertTrue("cancellation left the secret in memory", seen!!.all { it == ' ' })
    }

    @Test
    fun `wipe is idempotent and safe from onDispose`() {
        val buffer = SecretBuffer()
        buffer.set("sk-live-abcdefgh9999")
        buffer.wipe()
        buffer.wipe()
        assertTrue(buffer.isEmpty)
        assertEquals(0, buffer.length)
    }

    @Test
    fun `replacing the value wipes what was there`() {
        val buffer = SecretBuffer()
        buffer.set("first-secret-value")
        val before = buffer.copy()
        buffer.set("second-secret-value")
        assertFalse(String(before).contains("second"))
        buffer.use { assertEquals("second-secret-value", String(it)) }
    }

    @Test
    fun `the buffer refuses to print itself`() {
        val buffer = SecretBuffer()
        buffer.set("sk-live-abcdefgh9999")
        assertFalse(buffer.toString().contains("sk-live"))
        assertTrue(buffer.toString().contains("REDACTED"))
    }

    @Test
    fun `the masked hint shows four characters at most`() {
        val buffer = SecretBuffer()
        buffer.set("sk-live-abcdefgh9999")
        assertEquals("••••9999", buffer.maskedHint())
        buffer.set("ab")
        assertEquals("••", buffer.maskedHint())
    }

    @Test
    fun `a cancelled save leaves no account and no live secret`() = runTest {
        val repository = FakeAccountsRepository()
        val buffer = SecretBuffer()
        buffer.set("sk-live-abcdefgh9999")
        val before = repository.snapshot.value.accounts.size
        var copy: CharArray? = null
        runCatching {
            buffer.use {
                copy = it
                throw CancellationException("navigated away")
            }
        }
        buffer.wipe()
        assertTrue(copy!!.all { it == ' ' })
        assertTrue(buffer.isEmpty)
        assertEquals(before, repository.snapshot.value.accounts.size)
    }
}
