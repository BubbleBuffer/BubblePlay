package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.keys.DeviceId
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.Ed25519Verify
import org.bouncycastle.crypto.params.X25519PrivateKeyParameters
import org.bouncycastle.crypto.params.X25519PublicKeyParameters

/** Why authentication or pairing failed. Each is a session end with a user-visible reason. */
enum class AuthFailure {
    /** A message with the wrong type, keys or field formats. */
    MALFORMED,
    /** A message out of order, repeated, or after the exchange ended. */
    UNEXPECTED_MESSAGE,
    /** auth-host's ID is not the ID of its public key. */
    ID_MISMATCH,
    /** auth-host names a different PC than the one this session is for (pairing mode). */
    WRONG_PC,
    /** The PC's key is not the pinned key ("PC key changed"). */
    PC_KEY_CHANGED,
    /** The PC's transcript signature did not verify. */
    BAD_SIGNATURE,
    /** X25519 gave an all-zero secret (low-order ephemeral key). */
    KEY_AGREEMENT,
    /** The PC's pair-proof mac did not verify (wrong code or a relay in the middle). */
    PAIR_MAC_MISMATCH,
}

class AuthException(val failure: AuthFailure, message: String) : Exception(message)

/** The result of a verified auth-host. */
class HandshakeResult(
    /** The auth-finish JSON to send. */
    val authFinish: String,
    /** Canonical PC ID and its Ed25519 public key (learned in pairing mode, equal to the pin otherwise). */
    val pcId: String,
    pcPublic: ByteArray,
    transcriptHash: ByteArray,
    val controlKeys: ControlKeys,
) {
    private val pcPublicBytes = pcPublic.copyOf()
    private val hash = transcriptHash.copyOf()
    val pcPublic: ByteArray get() = pcPublicBytes.copyOf()
    /** H = SHA-256(transcript). */
    val transcriptHash: ByteArray get() = hash.copyOf()
}

/**
 * The viewer side of the phase 0 handshake over `/ws` (docs/architecture/device-keys.md,
 * "Handshake" and "Encodings for implementers"). WSS mode only: no auth-bind.
 *
 * [start] draws the 32-byte nonce, then the 32-byte X25519 ephemeral private key, from [AuthRandom]
 * in that order, and builds auth-hello. [onAuthHost] verifies the PC and returns auth-finish and
 * the control keys. One instance serves one handshake; [onAuthHost] succeeds at most once.
 */
class Handshake private constructor(
    private val key: DeviceKey,
    private val relayOrigin: String,
    private val sessionEpoch: String,
    val mode: Mode,
    private val nonce: ByteArray,
    private val ephemeralPrivate: ByteArray,
    val caps: List<String>,
) {
    /** Who the PC must be. */
    sealed class Mode {
        abstract val pcId: String

        /** Normal session: the PC must present [pcPublic] (the pinned key). */
        class Normal(override val pcId: String, pcPublic: ByteArray) : Mode() {
            internal val pcPublic = pcPublic.copyOf()

            init {
                require(pcPublic.size == 32 && DeviceId.fromPublicKey(pcPublic) == pcId) { "pin does not match its ID" }
            }
        }

        /** Pairing window: the PC must be [pcId]; its key is learned. [code] is the normalised pairing code. */
        class Pairing(override val pcId: String, val code: String) : Mode() {
            init {
                require(DeviceId.isCanonical(pcId)) { "PC IDs must be canonical" }
                require(PairingCode.normalise(code) == code) { "not a normalised pairing code" }
            }
        }
    }

    private val ephemeralPublic: ByteArray = X25519PrivateKeyParameters(ephemeralPrivate, 0).generatePublicKey().encoded
    private var used = false

    val isPairing: Boolean get() = mode is Mode.Pairing

    /** The auth-hello JSON `{type, id, public, nonce, ephemeral, caps}`. */
    val authHello: String = AuthJson.write(
        linkedMapOf(
            "type" to "auth-hello",
            "id" to key.id,
            "public" to AuthCrypto.b64(key.publicKey),
            "nonce" to AuthCrypto.b64(nonce),
            "ephemeral" to AuthCrypto.b64(ephemeralPublic),
            "caps" to caps,
        ),
    )

    /**
     * Verifies auth-host `{type, id, public, nonce, ephemeral, signature}` (exact keys): canonical
     * ID derived from the public key, the expected PC (pin or pairing link), strict Ed25519 over
     * "bubbleplay-auth-host-v1" || H. Then signs auth-finish and derives the control keys.
     * Throws [AuthException]; a second call throws UNEXPECTED_MESSAGE.
     */
    @Synchronized
    fun onAuthHost(text: String): HandshakeResult {
        if (used) throw AuthException(AuthFailure.UNEXPECTED_MESSAGE, "auth-host already handled")
        used = true
        try {
            return verify(text)
        } finally {
            ephemeralPrivate.fill(0)
        }
    }

    private fun verify(text: String): HandshakeResult {
        val host = try {
            val m = AuthJson.parseObject(text)
            AuthJson.requireExactKeys(m, "type", "id", "public", "nonce", "ephemeral", "signature")
            if (m["type"] != "auth-host") throw AuthJson.Malformed("not auth-host")
            Host(
                id = AuthJson.string(m, "id"),
                public = AuthCrypto.unb64(AuthJson.string(m, "public"), 32),
                nonce = AuthCrypto.unb64(AuthJson.string(m, "nonce"), 32),
                ephemeral = AuthCrypto.unb64(AuthJson.string(m, "ephemeral"), 32),
                signature = AuthCrypto.unb64(AuthJson.string(m, "signature"), 64),
            )
        } catch (e: AuthJson.Malformed) {
            throw AuthException(AuthFailure.MALFORMED, "auth-host: ${e.message}")
        }
        if (!DeviceId.isCanonical(host.id)) throw AuthException(AuthFailure.MALFORMED, "auth-host ID is not canonical")
        if (DeviceId.fromPublicKey(host.public) != host.id) {
            throw AuthException(AuthFailure.ID_MISMATCH, "auth-host ID does not match its key")
        }
        when (mode) {
            is Mode.Normal -> if (!host.public.contentEquals(mode.pcPublic)) {
                throw AuthException(AuthFailure.PC_KEY_CHANGED, "PC key changed")
            }
            is Mode.Pairing -> if (host.id != mode.pcId) {
                throw AuthException(AuthFailure.WRONG_PC, "auth-host is from another PC")
            }
        }
        val transcriptHash = AuthCrypto.sha256(transcript(host))
        val pcSigned = pcSignedBytes(transcriptHash)
        if (!Ed25519Verify.verify(host.public, pcSigned, host.signature)) {
            throw AuthException(AuthFailure.BAD_SIGNATURE, "auth-host signature did not verify")
        }
        val shared = ByteArray(32)
        try {
            val agreed = try {
                X25519PrivateKeyParameters(ephemeralPrivate, 0)
                    .generateSecret(X25519PublicKeyParameters(host.ephemeral, 0), shared, 0)
                true
            } catch (e: IllegalStateException) {
                false
            }
            if (!agreed || shared.all { it == 0.toByte() }) {
                throw AuthException(AuthFailure.KEY_AGREEMENT, "X25519 agreement failed")
            }
            val viewerSignature = key.sign(viewerSignedBytes(transcriptHash, host.signature))
            val finish = AuthJson.write(linkedMapOf("type" to "auth-finish", "signature" to AuthCrypto.b64(viewerSignature)))
            return HandshakeResult(finish, host.id, host.public, transcriptHash, ControlKeys.derive(shared, transcriptHash))
        } finally {
            shared.fill(0)
        }
    }

    private class Host(
        val id: String,
        val public: ByteArray,
        val nonce: ByteArray,
        val ephemeral: ByteArray,
        val signature: ByteArray,
    )

    private fun transcript(host: Host): ByteArray = AuthCrypto.lengthPrefixed(
        AuthCrypto.utf8("bubbleplay-auth-v1"),
        AuthCrypto.utf8(relayOrigin),
        AuthCrypto.utf8(sessionEpoch),
        AuthCrypto.utf8(key.id),
        key.publicKey,
        AuthCrypto.utf8(host.id),
        host.public,
        nonce,
        host.nonce,
        ephemeralPublic,
        host.ephemeral,
    )

    companion object {
        /** The viewer waits at most this long for auth-host; the runner enforces it (decisions item 16). */
        const val AUTH_HOST_TIMEOUT_MS = 10_000L

        /**
         * Starts a handshake. [relayOrigin] and [sessionEpoch] go into the transcript verbatim as
         * UTF-8. Caps are `control-aead-v1`, then [extraCaps], then `pair-v1` in pairing mode.
         */
        fun start(
            key: DeviceKey,
            relayOrigin: String,
            sessionEpoch: String,
            mode: Mode,
            random: AuthRandom = AuthRandom.secure(),
            extraCaps: List<String> = emptyList(),
        ): Handshake {
            val caps = buildList {
                add(CtlChannel.CAP)
                addAll(extraCaps)
                if (mode is Mode.Pairing) add(Pairing.CAP)
            }
            require(caps.size <= 32 && caps.all { CAP_FORMAT.matches(it) } && caps.toSet().size == caps.size) {
                "bad caps"
            }
            val nonce = ByteArray(32).also(random::nextBytes)
            val ephemeral = ByteArray(32).also(random::nextBytes)
            return Handshake(key, relayOrigin, sessionEpoch, mode, nonce, ephemeral, caps)
        }

        /** "bubbleplay-auth-host-v1" || H: what the PC signs. */
        fun pcSignedBytes(transcriptHash: ByteArray): ByteArray =
            AuthCrypto.utf8("bubbleplay-auth-host-v1") + transcriptHash

        /** "bubbleplay-auth-viewer-v1" || H || PC signature: what the viewer signs. */
        fun viewerSignedBytes(transcriptHash: ByteArray, pcSignature: ByteArray): ByteArray =
            AuthCrypto.utf8("bubbleplay-auth-viewer-v1") + transcriptHash + pcSignature

        private val CAP_FORMAT = Regex("[a-z0-9-]{1,64}")
    }

    /** The transcript for [host] fields, exposed for tests that check the phase 0 bytes. */
    internal fun transcriptFor(
        pcId: String,
        pcPublic: ByteArray,
        pcNonce: ByteArray,
        pcEphemeral: ByteArray,
    ): ByteArray = transcript(Host(pcId, pcPublic, pcNonce, pcEphemeral, ByteArray(64)))
}
