package de.bubblebuffer.bubbleplay.keys

import java.security.MessageDigest

/**
 * Device IDs: the first 80 bits of SHA-256 over the raw 32-byte Ed25519 public key, written as
 * 16 Crockford base32 characters in four hyphenated groups, e.g. `B7QK-2M9X-H4PD-R8TN`.
 */
object DeviceId {
    private const val ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
    private val canonical = Regex("[0-9A-HJKMNP-TV-Z]{4}(?:-[0-9A-HJKMNP-TV-Z]{4}){3}")

    /** Canonical ID for a 32-byte public key. */
    fun fromPublicKey(publicKey: ByteArray): String {
        require(publicKey.size == 32) { "Ed25519 public keys are 32 bytes" }
        val digest = MessageDigest.getInstance("SHA-256").digest(publicKey)
        val text = StringBuilder(19)
        for (group in 0 until 16) {
            var value = 0
            for (bit in 0 until 5) {
                val index = group * 5 + bit
                value = (value shl 1) or ((digest[index / 8].toInt() ushr (7 - index % 8)) and 1)
            }
            if (group > 0 && group % 4 == 0) text.append('-')
            text.append(ALPHABET[value])
        }
        return text.toString()
    }

    /**
     * Normalises typed input: case, hyphens anywhere, and O as 0, I and L as 1 are accepted.
     * Throws [DeviceKeyException] with MALFORMED for anything else.
     */
    fun parse(input: String): String {
        val folded = input.filter { it != '-' }.uppercase()
            .replace('O', '0').replace('I', '1').replace('L', '1')
        if (folded.length != 16 || folded.any { it !in ALPHABET }) malformed("not a device ID")
        return folded.chunked(4).joinToString("-")
    }

    /** True only for the exact canonical spelling that key files store. */
    fun isCanonical(id: String): Boolean = canonical.matches(id)
}
