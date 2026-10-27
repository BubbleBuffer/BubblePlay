package de.bubblebuffer.bubbleplay.auth

import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/** The two control keys of one handshake (decisions item 6). */
class ControlKeys(viewerToPc: ByteArray, pcToViewer: ByteArray) {
    init {
        require(viewerToPc.size == 32 && pcToViewer.size == 32) { "control keys are 32 bytes" }
    }

    internal val viewerToPc = viewerToPc.copyOf()
    internal val pcToViewer = pcToViewer.copyOf()

    /** The viewer's channel: seals with viewer-to-pc, opens with pc-to-viewer. */
    fun viewerChannel(): CtlChannel = CtlChannel(sendKey = viewerToPc, receiveKey = pcToViewer)

    /** The PC's channel (the mirror image; used by tests and tools). */
    fun pcChannel(): CtlChannel = CtlChannel(sendKey = pcToViewer, receiveKey = viewerToPc)

    fun wipe() {
        viewerToPc.fill(0)
        pcToViewer.fill(0)
    }

    companion object {
        /** k_v2p / k_p2v = HKDF-SHA256(ikm = X25519 shared, salt = H, info = "bubbleplay-ctl-v1 <direction>"). */
        fun derive(sharedSecret: ByteArray, transcriptHash: ByteArray): ControlKeys {
            val v2p = AuthCrypto.hkdf32(sharedSecret, transcriptHash, AuthCrypto.utf8("bubbleplay-ctl-v1 viewer-to-pc"))
            val p2v = AuthCrypto.hkdf32(sharedSecret, transcriptHash, AuthCrypto.utf8("bubbleplay-ctl-v1 pc-to-viewer"))
            try {
                return ControlKeys(v2p, p2v)
            } finally {
                v2p.fill(0)
                p2v.fill(0)
            }
        }
    }
}

/** A decrypted ctl message. For binary kinds "input" and "cursor" [plaintext] is the raw binary frame; otherwise UTF-8 JSON. */
class CtlMessage(val kind: String, val plaintext: ByteArray) {
    /** The JSON text (not meaningful for binary kinds). */
    fun text(): String = String(plaintext, Charsets.UTF_8)
}

/** Why a ctl channel failed. Any of these ends the session. */
class CtlException(val reason: Reason, message: String) : Exception(message) {
    enum class Reason {
        /** Not a ctl envelope with exactly type, kind, seq, data in their formats. */
        MALFORMED,
        /** kind outside `[a-z0-9-]{1,32}`. */
        BAD_KIND,
        /** data longer than [CtlChannel.DATA_MAX] characters. */
        TOO_LONG,
        /** seq is not exactly the next expected one (gap or repeat). */
        OUT_OF_ORDER,
        /** The AES-GCM tag did not verify (wrong key, wrong kind as AAD, tampered data). */
        AUTH_FAILED,
        /** A JSON plaintext that is not an object whose type equals kind. */
        TYPE_MISMATCH,
        /** The send sequence would pass 2^53 - 1. */
        EXHAUSTED,
        /** An earlier failure ended this channel. */
        CLOSED,
    }
}

/**
 * The per-direction AES-256-GCM control channel (decisions items 6 and 15).
 *
 * Nonce = 4 zero bytes || seq as u64 big-endian, AAD = UTF-8 kind, data = b64url(ct || tag).
 * Each direction's seq starts at 0 and must be exactly next. The first failure of [open] or
 * [seal] closes the channel for good: every later call throws [CtlException.Reason.CLOSED], and
 * the session must end. The PC's first message is kind "caps" (its full caps list); this class
 * treats it like any other kind.
 */
class CtlChannel(sendKey: ByteArray, receiveKey: ByteArray) {
    private val sendKey = sendKey.copyOf()
    private val receiveKey = receiveKey.copyOf()
    private var sendSeq = 0L
    private var receiveSeq = 0L
    private var failed: CtlException? = null

    init {
        require(sendKey.size == 32 && receiveKey.size == 32) { "control keys are 32 bytes" }
    }

    val isClosed: Boolean @Synchronized get() = failed != null

    /**
     * Seals [plaintext] as kind [kind] and returns the envelope JSON
     * `{"type":"ctl","kind","seq","data"}`. For kinds other than "input" and "cursor", [plaintext] must be a UTF-8
     * JSON object whose type equals [kind] (IllegalArgumentException otherwise; the channel stays open).
     */
    @Synchronized
    fun seal(kind: String, plaintext: ByteArray): String {
        failed?.let { throw CtlException(CtlException.Reason.CLOSED, "control channel closed: ${it.reason}") }
        require(KIND.matches(kind)) { "bad ctl kind" }
        if (kind != KIND_INPUT && kind != KIND_CURSOR) require(jsonTypeOf(plaintext) == kind) { "plaintext type must equal kind" }
        if (sendSeq > MAX_SEQ) fail(CtlException.Reason.EXHAUSTED, "send sequence exhausted")
        val data = AuthCrypto.b64(crypt(Cipher.ENCRYPT_MODE, sendKey, sendSeq, kind, plaintext))
        require(data.length <= DATA_MAX) { "ctl message too large" }
        val envelope = AuthJson.write(linkedMapOf("type" to "ctl", "kind" to kind, "seq" to sendSeq, "data" to data))
        sendSeq++
        return envelope
    }

    /** Opens one received envelope; throws [CtlException] (and closes the channel) on any failure. */
    @Synchronized
    fun open(envelope: String): CtlMessage {
        failed?.let { throw CtlException(CtlException.Reason.CLOSED, "control channel closed: ${it.reason}") }
        val m = try {
            AuthJson.parseObject(envelope).also { AuthJson.requireExactKeys(it, "type", "kind", "seq", "data") }
        } catch (e: AuthJson.Malformed) {
            fail(CtlException.Reason.MALFORMED, "not a ctl envelope")
        }
        if (m["type"] != "ctl") fail(CtlException.Reason.MALFORMED, "not a ctl envelope")
        val kind = m["kind"] as? String ?: fail(CtlException.Reason.MALFORMED, "kind is not a string")
        if (!KIND.matches(kind)) fail(CtlException.Reason.BAD_KIND, "bad ctl kind")
        val seq = m["seq"] as? Long ?: fail(CtlException.Reason.MALFORMED, "seq is not an integer")
        val data = m["data"] as? String ?: fail(CtlException.Reason.MALFORMED, "data is not a string")
        if (data.length > DATA_MAX) fail(CtlException.Reason.TOO_LONG, "ctl data too long")
        if (seq != receiveSeq) fail(CtlException.Reason.OUT_OF_ORDER, "expected seq $receiveSeq, got $seq")
        val sealed = try {
            AuthCrypto.unb64(data)
        } catch (e: AuthJson.Malformed) {
            fail(CtlException.Reason.MALFORMED, "data is not base64url")
        }
        if (sealed.size < TAG_BYTES) fail(CtlException.Reason.MALFORMED, "data shorter than the tag")
        val plaintext = try {
            crypt(Cipher.DECRYPT_MODE, receiveKey, seq, kind, sealed)
        } catch (e: AEADBadTagException) {
            fail(CtlException.Reason.AUTH_FAILED, "ctl tag did not verify")
        }
        if (kind != KIND_INPUT && kind != KIND_CURSOR && jsonTypeOf(plaintext) != kind) {
            plaintext.fill(0)
            fail(CtlException.Reason.TYPE_MISMATCH, "plaintext type does not equal kind")
        }
        receiveSeq++
        return CtlMessage(kind, plaintext)
    }

    /** Ends the channel from outside (session over); wipes the keys. */
    @Synchronized
    fun close() {
        if (failed == null) failed = CtlException(CtlException.Reason.CLOSED, "closed")
        sendKey.fill(0)
        receiveKey.fill(0)
    }

    private fun fail(reason: CtlException.Reason, message: String): Nothing {
        val failure = CtlException(reason, message)
        failed = failure
        sendKey.fill(0)
        receiveKey.fill(0)
        throw failure
    }

    private fun crypt(mode: Int, key: ByteArray, seq: Long, kind: String, input: ByteArray): ByteArray {
        val nonce = ByteArray(12)
        for (i in 0 until 8) nonce[4 + i] = (seq ushr (56 - 8 * i)).toByte()
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(mode, SecretKeySpec(key, "AES"), GCMParameterSpec(TAG_BYTES * 8, nonce))
        cipher.updateAAD(AuthCrypto.utf8(kind))
        return cipher.doFinal(input)
    }

    /** The `type` of a JSON object plaintext, or null when it is not valid UTF-8 JSON with a string type. */
    private fun jsonTypeOf(plaintext: ByteArray): String? {
        val text = try {
            Charsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(plaintext))
                .toString()
        } catch (e: CharacterCodingException) {
            return null
        }
        return try {
            AuthJson.parseObject(text)["type"] as? String
        } catch (e: AuthJson.Malformed) {
            null
        }
    }

    companion object {
        const val CAP = "control-aead-v1"
        const val KIND_INPUT = "input"
        const val KIND_CURSOR = "cursor"
        /** The PC's first ctl message carries its caps under this kind (decisions item 16). */
        const val KIND_CAPS = "caps"
        /** Longest accepted `data` in characters (relay `CTL_DATA_MAX`). */
        const val DATA_MAX = 263_168
        /** Largest seq: 2^53 - 1 (JSON-safe integer). */
        const val MAX_SEQ = 9_007_199_254_740_991L
        private const val TAG_BYTES = 16
        private val KIND = Regex("[a-z0-9-]{1,32}")
    }
}
