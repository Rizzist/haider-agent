package ai.diffforge.haider.daemon

import android.security.keystore.KeyInfo
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.security.KeyStore
import java.security.SecureRandom
import java.util.UUID
import javax.crypto.SecretKey
import javax.crypto.SecretKeyFactory

@RunWith(AndroidJUnit4::class)
class KeystoreInstrumentedTest {
    @Test fun realKeystoreNonExportableWrapTamperAndLoss() {
        // Synthetic fixture alias only. Never inspect or alter the real vault/signing aliases.
        val alias = "haider.test.wrap.${UUID.randomUUID()}"
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        val key = AndroidDekWrappingKey(alias)
        val dek = ByteArray(32).also(SecureRandom()::nextBytes)
        try {
            key.create()
            val secretKey = store.getKey(alias, null) as SecretKey
            assertNull(secretKey.encoded)
            val info = SecretKeyFactory.getInstance(secretKey.algorithm, "AndroidKeyStore")
                .getKeySpec(secretKey, KeyInfo::class.java) as KeyInfo
            assertFalse(info.isUserAuthenticationRequired)
            val wrapped = key.wrap(dek)
            assertEquals(64, wrapped.size)
            val unwrapped = AndroidDekWrappingKey(alias).unwrap(wrapped)
            try { assertArrayEquals(dek, unwrapped) } finally { unwrapped.fill(0) }
            wrapped[wrapped.lastIndex] = (wrapped.last().toInt() xor 1).toByte()
            try { key.unwrap(wrapped); fail("Damaged tag must fail") } catch (_: javax.crypto.AEADBadTagException) { }
            store.deleteEntry(alias)
            assertFalse(key.exists())
            try { key.unwrap(wrapped); fail("Lost key must fail") }
            catch (failure: VaultKeyFailure) { assertEquals("VAULT_KEY_INVALID", failure.code) }
        } finally { dek.fill(0); if (store.containsAlias(alias)) store.deleteEntry(alias) }
    }
}
