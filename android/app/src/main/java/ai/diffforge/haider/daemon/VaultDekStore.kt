package ai.diffforge.haider.daemon

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import java.io.File
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** The wrapping key never leaves Android Keystore. Only the short-lived DEK reaches JNI. */
internal interface DekWrappingKey {
    fun exists(): Boolean
    fun create()
    fun wrap(dek: ByteArray): ByteArray
    fun unwrap(blob: ByteArray): ByteArray
}

internal class AndroidDekWrappingKey(private val alias: String = DEFAULT_ALIAS) : DekWrappingKey {
    private val store by lazy { KeyStore.getInstance("AndroidKeyStore").apply { load(null) } }
    override fun exists() = store.containsAlias(alias)
    override fun create() {
        check(!exists())
        KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
            init(KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setKeySize(256).setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setRandomizedEncryptionRequired(true).setUserAuthenticationRequired(false).build())
        }.generateKey()
    }
    private fun key() = store.getKey(alias, null) as? SecretKey ?: throw VaultKeyFailure("VAULT_KEY_INVALID")
    override fun wrap(dek: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key())
        cipher.updateAAD(AAD)
        check(cipher.iv.size == 12)
        return MAGIC + cipher.iv + cipher.doFinal(dek)
    }
    override fun unwrap(blob: ByteArray): ByteArray {
        if (blob.size != 64 || !blob.copyOfRange(0, 4).contentEquals(MAGIC)) {
            throw VaultKeyFailure("VAULT_WRAP_CORRUPT")
        }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, blob, 4, 12))
        cipher.updateAAD(AAD)
        return cipher.doFinal(blob, 16, 48)
    }
    private companion object {
        const val DEFAULT_ALIAS = "haider.android-default.vault-wrap.v1"
        val MAGIC = byteArrayOf(72, 68, 75, 49) // HDK1, Kotlin-private format
        val AAD = "haider-vault-dek-wrap-v1\nandroid-default".toByteArray(Charsets.UTF_8)
    }
}

internal class VaultKeyFailure(val code: String) : Exception(code)
internal fun interface VaultDekProvider {
    /** Implementations must erase the DEK even when [use] throws. */
    fun withDek(use: (ByteArray) -> Int): Int
}

internal interface WrappedDekStorage {
    fun exists(): Boolean
    fun credentialsExist(): Boolean
    fun read(): ByteArray
    fun write(blob: ByteArray)
}

internal class VaultDekStore(
    private val key: DekWrappingKey,
    private val storage: WrappedDekStorage,
    private val random: SecureRandom = SecureRandom(),
) : VaultDekProvider {
    override fun withDek(use: (ByteArray) -> Int): Int {
        var dek: ByteArray? = null
        try {
            if (storage.exists()) {
                if (!key.exists()) throw VaultKeyFailure("VAULT_KEY_INVALID")
                dek = key.unwrap(storage.read())
            } else {
                // Orphaned keys, missing wrap records and existing ciphertext are recovery cases.
                // Never replace a key or wrap a new DEK over an existing vault.
                if (key.exists() || storage.credentialsExist()) throw VaultKeyFailure("VAULT_KEY_INVALID")
                key.create()
                dek = ByteArray(32).also(random::nextBytes)
                storage.write(key.wrap(dek))
            }
            if (dek.size != 32) throw VaultKeyFailure("VAULT_WRAP_CORRUPT")
        } catch (failure: VaultKeyFailure) {
            dek?.fill(0)
            throw failure
        } catch (_: KeyPermanentlyInvalidatedException) {
            dek?.fill(0)
            throw VaultKeyFailure("VAULT_KEY_INVALID")
        } catch (_: AEADBadTagException) {
            dek?.fill(0)
            throw VaultKeyFailure("VAULT_WRAP_CORRUPT")
        } catch (_: Exception) {
            dek?.fill(0)
            throw VaultKeyFailure("VAULT_KEY_UNAVAILABLE")
        } catch (failure: Throwable) {
            dek?.fill(0)
            throw failure
        }
        return try { use(requireNotNull(dek)) } finally { dek.fill(0) }
    }

    companion object {
        fun create(context: Context, vault: File): VaultDekStore {
            val directory = PrivateDaemonFiles.directory(context.noBackupFilesDir, "haider/keys")
            val file = File(directory, "wrapped-vault-dek")
            return VaultDekStore(AndroidDekWrappingKey(), object : WrappedDekStorage {
                override fun exists() = PrivateDaemonFiles.exists(file)
                override fun credentialsExist(): Boolean {
                    if (!PrivateDaemonFiles.exists(vault)) return false
                    // Any unknown vault entry is preserved, including malformed ciphertext.
                    return vault.list()?.isNotEmpty() ?: true
                }
                override fun read() = PrivateDaemonFiles.read(file, 64)
                override fun write(blob: ByteArray) = PrivateDaemonFiles.write(file, blob)
            })
        }
    }
}
