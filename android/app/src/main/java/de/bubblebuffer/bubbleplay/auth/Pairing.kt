package de.bubblebuffer.bubbleplay.auth

import java.security.MessageDigest

/**
 * Pairing at the PC (decisions items 4, 5, 17). H is the handshake transcript hash.
 * The pair key K = HKDF-SHA256(ikm = normalised code ASCII, salt = empty, info = "bubbleplay-pair-v1").
 */
object Pairing {
    const val CAP = "pair-v1"

    /** K for a normalised [code] (IllegalArgumentException otherwise). The caller should zero it. */
    fun pairKey(code: String): ByteArray {
        require(PairingCode.normalise(code) == code) { "not a normalised pairing code" }
        return AuthCrypto.hkdf32(AuthCrypto.utf8(code), ByteArray(0), AuthCrypto.utf8("bubbleplay-pair-v1"))
    }

    /** HMAC-SHA256(K, "viewer" || H): the viewer's pair-proof mac. */
    fun viewerMac(pairKey: ByteArray, transcriptHash: ByteArray): ByteArray =
        AuthCrypto.hmacSha256(pairKey, AuthCrypto.utf8("viewer"), transcriptHash)

    /** HMAC-SHA256(K, "pc" || H): the PC's pair-proof mac. */
    fun pcMac(pairKey: ByteArray, transcriptHash: ByteArray): ByteArray =
        AuthCrypto.hmacSha256(pairKey, AuthCrypto.utf8("pc"), transcriptHash)

    /** Constant-time check of the PC's mac. */
    fun verifyPcMac(pairKey: ByteArray, transcriptHash: ByteArray, mac: ByteArray): Boolean =
        MessageDigest.isEqual(pcMac(pairKey, transcriptHash), mac)

    /** First 4 bytes of SHA-256("bubbleplay-pair-sas-v1" || H) as an unsigned big-endian u32. */
    fun sasU32(transcriptHash: ByteArray): Long {
        val hash = AuthCrypto.sha256(AuthCrypto.utf8("bubbleplay-pair-sas-v1"), transcriptHash)
        return ((hash[0].toLong() and 0xff) shl 24) or ((hash[1].toLong() and 0xff) shl 16) or
            ((hash[2].toLong() and 0xff) shl 8) or (hash[3].toLong() and 0xff)
    }

    /** The comparison number: [sasU32] mod 10^6, zero-padded to 6 digits, e.g. "830687". */
    fun sasNumber(transcriptHash: ByteArray): String = (sasU32(transcriptHash) % 1_000_000).toString().padStart(6, '0')

    /** "830 687" for "830687". */
    fun sasDisplay(number: String): String {
        require(number.length == 6 && number.all { it in '0'..'9' }) { "not a comparison number" }
        return number.substring(0, 3) + " " + number.substring(3)
    }
}
