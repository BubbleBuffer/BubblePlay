package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import de.bubblebuffer.bubbleplay.keys.StrictBase64Url
import java.io.ByteArrayOutputStream
import java.security.MessageDigest
import java.security.SecureRandom
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/** Source of the handshake's random bytes; tests inject fixed vectors. */
fun interface AuthRandom {
    /** Fills [out] completely. */
    fun nextBytes(out: ByteArray)

    companion object {
        /** The platform CSPRNG. */
        fun secure(): AuthRandom {
            val random = SecureRandom()
            return AuthRandom { random.nextBytes(it) }
        }
    }
}

internal object AuthCrypto {
    fun utf8(text: String): ByteArray = text.toByteArray(Charsets.UTF_8)

    fun sha256(vararg parts: ByteArray): ByteArray {
        val digest = MessageDigest.getInstance("SHA-256")
        for (part in parts) digest.update(part)
        return digest.digest()
    }

    fun hmacSha256(key: ByteArray, vararg parts: ByteArray): ByteArray {
        val mac = Mac.getInstance("HmacSHA256")
        // The JDK refuses empty keys; HMAC pads short keys with zeros, so 32 zero bytes is the same key.
        mac.init(SecretKeySpec(if (key.isEmpty()) ByteArray(32) else key, "HmacSHA256"))
        for (part in parts) mac.update(part)
        return mac.doFinal()
    }

    /** RFC 5869 HKDF-SHA256 with a 32-byte output. An empty salt means HashLen zero bytes. */
    fun hkdf32(ikm: ByteArray, salt: ByteArray, info: ByteArray): ByteArray {
        val prk = hmacSha256(if (salt.isEmpty()) ByteArray(32) else salt, ikm)
        try {
            return hmacSha256(prk, info, byteArrayOf(1))
        } finally {
            prk.fill(0)
        }
    }

    /** Each field as uint32 big-endian length || bytes (phase 0 transcript and relay proof). */
    fun lengthPrefixed(vararg fields: ByteArray): ByteArray {
        val out = ByteArrayOutputStream()
        for (field in fields) {
            val n = field.size
            out.write(n ushr 24)
            out.write(n ushr 16)
            out.write(n ushr 8)
            out.write(n)
            out.write(field)
        }
        return out.toByteArray()
    }

    fun b64(bytes: ByteArray): String = StrictBase64Url.encode(bytes)

    /** Strict unpadded base64url of exactly [size] bytes; throws [AuthJson.Malformed] otherwise. */
    fun unb64(text: String, size: Int? = null): ByteArray = try {
        StrictBase64Url.decode(text, size)
    } catch (e: DeviceKeyException) {
        throw AuthJson.Malformed("bad base64url")
    }
}
