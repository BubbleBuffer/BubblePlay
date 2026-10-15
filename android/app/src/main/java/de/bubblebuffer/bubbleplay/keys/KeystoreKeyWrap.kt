package de.bubblebuffer.bubbleplay.keys

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Android-only envelope: 12-byte nonce || AES-GCM ciphertext and 16-byte tag.
 *
 * The wrapping key is non-exportable. Losing it makes the local file unreadable;
 * callers must import a portable backup rather than silently replace the identity.
 * Requires Android Keystore and therefore cannot be exercised by JVM unit tests.
 */
class KeystoreKeyWrap {
    fun wrap(portableBytes: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        // Android Keystore generates the nonce; caller-supplied IVs are forbidden
        // for encryption by its randomized-encryption requirement.
        cipher.init(Cipher.ENCRYPT_MODE, wrappingKey(create = true))
        val nonce = cipher.iv
        check(nonce.size == NONCE_BYTES) { "Unexpected Keystore GCM nonce size" }
        return nonce + cipher.doFinal(portableBytes)
    }

    fun unwrap(envelope: ByteArray): ByteArray {
        require(envelope.size >= NONCE_BYTES + TAG_BYTES) { "Truncated device-key envelope" }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(
            Cipher.DECRYPT_MODE,
            wrappingKey(create = false),
            GCMParameterSpec(TAG_BYTES * 8, envelope, 0, NONCE_BYTES),
        )
        return cipher.doFinal(envelope, NONCE_BYTES, envelope.size - NONCE_BYTES)
    }

    private fun wrappingKey(create: Boolean): SecretKey = synchronized(keyLock) {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        if (store.containsAlias(ALIAS)) {
            return@synchronized store.getKey(ALIAS, null) as? SecretKey
                ?: error("Device-key wrapping alias is not an AES key")
        }
        check(create) { "Device-key wrapping key is missing; import a portable backup" }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec.Builder(
                ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setKeySize(256)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setRandomizedEncryptionRequired(true)
                .build(),
        )
        generator.generateKey()
    }

    companion object {
        private const val ALIAS = "bubbleplay-device-key-wrap"
        private const val NONCE_BYTES = 12
        private const val TAG_BYTES = 16
        private val keyLock = Any()
    }
}
