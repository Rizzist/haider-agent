package ai.diffforge.haider.daemon

import org.junit.Assert.*
import org.junit.Test
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

class VaultDekStoreTest {
    private class Storage : WrappedDekStorage {
        var blob: ByteArray? = null
        var credentials = false
        var writes = 0
        override fun exists() = blob != null
        override fun credentialsExist() = credentials
        override fun read() = blob!!.clone()
        override fun write(blob: ByteArray) { this.blob = blob; writes++ }
    }
    private class WrappingKey : DekWrappingKey {
        var key: SecretKey? = null
        var creates = 0
        var fail = false
        override fun exists() = key != null
        override fun create() { creates++; key = KeyGenerator.getInstance("AES").apply { init(256) }.generateKey() }
        override fun wrap(dek: ByteArray): ByteArray {
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.ENCRYPT_MODE, key)
            return cipher.iv + cipher.doFinal(dek)
        }
        override fun unwrap(blob: ByteArray): ByteArray {
            if (fail) throw VaultKeyFailure("VAULT_KEY_INVALID")
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, blob, 0, 12))
            return cipher.doFinal(blob, 12, blob.size - 12)
        }
    }
    private val storage = Storage()
    private val key = WrappingKey()
    private fun vault() = VaultDekStore(key, storage, SecureRandom())

    @Test fun generatesOnceRoundTripsAndErasesEveryBorrow() {
        var borrowed: ByteArray? = null
        var expected: ByteArray? = null
        assertEquals(17, vault().withDek { borrowed = it; expected = it.clone(); 17 })
        assertEquals(1, key.creates)
        assertEquals(1, storage.writes)
        assertTrue(borrowed!!.all { it == 0.toByte() })
        vault().withDek { assertArrayEquals(expected, it); borrowed = it; 0 }
        assertTrue(borrowed!!.all { it == 0.toByte() })
        assertFalse(storage.blob!!.asList().windowed(32).any { it == expected!!.asList() })
        expected!!.fill(0)
    }
    @Test fun consumerFailureStillErasesDekAndIsNotRelabelledKeyFailure() {
        var borrowed: ByteArray? = null
        val failure = IllegalStateException("synthetic consumer failure")
        try { vault().withDek { borrowed = it; throw failure }; fail() }
        catch (actual: IllegalStateException) { assertSame(failure, actual) }
        assertTrue(borrowed!!.all { it == 0.toByte() })
    }
    @Test fun lostKeyCannotRegenerateOverExistingWrappedDek() {
        vault().withDek { 0 }
        key.key = null
        assertFailure("VAULT_KEY_INVALID")
        assertEquals(1, key.creates)
        assertEquals(1, storage.writes)
    }
    @Test fun lostWrappingFileCannotReplaceExistingVaultOrOrphanedKey() {
        storage.credentials = true
        assertFailure("VAULT_KEY_INVALID")
        assertEquals(0, key.creates)
        storage.credentials = false; key.create()
        assertFailure("VAULT_KEY_INVALID")
        assertEquals(1, key.creates)
        assertEquals(0, storage.writes)
    }
    @Test fun wrongKeyDamagedTagAndInvalidationFailClosedWithoutRewriting() {
        vault().withDek { 0 }
        key.create()
        assertFailure("VAULT_WRAP_CORRUPT")
        assertEquals(1, storage.writes)
        key.fail = true
        assertFailure("VAULT_KEY_INVALID")
        key.fail = false
        storage.blob!![storage.blob!!.lastIndex] = (storage.blob!!.last().toInt() xor 1).toByte()
        assertFailure("VAULT_WRAP_CORRUPT")
        assertEquals(1, storage.writes)
    }
    private fun assertFailure(code: String) {
        try { vault().withDek { fail("No DEK may be released"); 0 }; fail("Expected fail-closed result") }
        catch (failure: VaultKeyFailure) { assertEquals(code, failure.code) }
    }
}
