package de.bubblebuffer.bubbleplay.keys

import java.util.Base64

/**
 * Unpadded base64url as key files write it. Input must use only the URL alphabet, no padding,
 * and canonical trailing bits, so every accepted string has exactly one spelling.
 */
internal object StrictBase64Url {
    private val alphabet = Regex("[A-Za-z0-9_-]+")

    fun encode(bytes: ByteArray): String = Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)

    /** Decodes [text]; when [size] is given, the decoded length must match it. */
    fun decode(text: String, size: Int? = null): ByteArray {
        if (!alphabet.matches(text) || text.length % 4 == 1) malformed("not unpadded base64url")
        val bytes = try {
            Base64.getUrlDecoder().decode(text)
        } catch (e: IllegalArgumentException) {
            malformed("not unpadded base64url")
        }
        try {
            if (encode(bytes) != text) malformed("base64url has non-canonical trailing bits")
            if (size != null && bytes.size != size) malformed("expected $size bytes, found ${bytes.size}")
            return bytes
        } catch (failure: Throwable) {
            bytes.fill(0)
            throw failure
        }
    }
}
