package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.keys.DeviceId
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.Ed25519Verify

/**
 * The relay proof (crates/bp-device-key/src/proof.rs): Ed25519 over H = SHA-256 of the
 * length-prefixed fields "bubbleplay-relay-v1", role, relay origin, 32-byte nonce, canonical
 * device ID, 32-byte public key. The app signs as [ROLE_VIEWER]; [ROLE_HOST] exists for tests.
 */
object RelayProof {
    const val ROLE_VIEWER = "viewer"
    const val ROLE_HOST = "host"
    private const val LABEL = "bubbleplay-relay-v1"

    fun encoding(role: String, origin: String, nonce: ByteArray, id: String, publicKey: ByteArray): ByteArray =
        encoding(LABEL, role, origin, nonce, id, publicKey)

    /** [encoding] under [label]. The shared fixture carries relay proofs under its own label. */
    internal fun encoding(
        label: String,
        role: String,
        origin: String,
        nonce: ByteArray,
        id: String,
        publicKey: ByteArray,
    ): ByteArray {
        require(role == ROLE_VIEWER || role == ROLE_HOST) { "unknown relay proof role" }
        require(nonce.size == 32) { "relay nonces are 32 bytes" }
        require(publicKey.size == 32) { "Ed25519 public keys are 32 bytes" }
        require(DeviceId.isCanonical(id)) { "device IDs must be canonical" }
        return AuthCrypto.lengthPrefixed(
            AuthCrypto.utf8(label), AuthCrypto.utf8(role), AuthCrypto.utf8(origin),
            nonce, AuthCrypto.utf8(id), publicKey,
        )
    }

    fun hash(role: String, origin: String, nonce: ByteArray, id: String, publicKey: ByteArray): ByteArray =
        AuthCrypto.sha256(encoding(role, origin, nonce, id, publicKey))

    internal fun hash(
        label: String,
        role: String,
        origin: String,
        nonce: ByteArray,
        id: String,
        publicKey: ByteArray,
    ): ByteArray = AuthCrypto.sha256(encoding(label, role, origin, nonce, id, publicKey))

    /** The 64-byte signature for [key] answering [nonce] on the relay at [origin]. */
    fun sign(key: DeviceKey, origin: String, nonce: ByteArray, role: String = ROLE_VIEWER): ByteArray =
        key.sign(hash(role, origin, nonce, key.id, key.publicKey))

    /** Strict verification (see [Ed25519Verify]); also false when [id] is not [publicKey]'s ID. */
    fun verify(
        role: String,
        origin: String,
        nonce: ByteArray,
        id: String,
        publicKey: ByteArray,
        signature: ByteArray,
    ): Boolean = verify(LABEL, role, origin, nonce, id, publicKey, signature)

    /** [verify] under [label], for the shared fixture's relay proofs. */
    internal fun verify(
        label: String,
        role: String,
        origin: String,
        nonce: ByteArray,
        id: String,
        publicKey: ByteArray,
        signature: ByteArray,
    ): Boolean {
        if (nonce.size != 32 || publicKey.size != 32 || signature.size != 64) return false
        if (role != ROLE_VIEWER && role != ROLE_HOST) return false
        if (!DeviceId.isCanonical(id) || DeviceId.fromPublicKey(publicKey) != id) return false
        return Ed25519Verify.verify(publicKey, hash(label, role, origin, nonce, id, publicKey), signature)
    }
}

/** The relay's answer to an admitted viewer hello (relay-rs/core/src/relay/hello.rs viewer_admit). */
data class HelloOk(
    /** Canonical PC ID the relay joined; always equal to the PC the socket was opened for. */
    val pc: String,
    /** 32 lowercase hex chars; the `epoch` query parameter of the media and file sockets. */
    val mediaEpoch: String,
    /** Ticket for the media/file socket hellos; null exactly when [pairing]. */
    val ticket: String?,
    /** True when admitted through a pairing window: only auth-* and pair-* get through. */
    val pairing: Boolean,
    /** The `iceServers` array as compact JSON (unused while Android has no WebRTC). */
    val iceServersJson: String,
)

/** Relay-level signalling for the viewer on `/ws?pc=<id>`, plus the data-socket ticket hellos. */
object ViewerHello {
    const val ROLE_MEDIA_VIEWER = "media-viewer"
    const val ROLE_FILE_VIEWER = "file-viewer"

    /** `{"type":"hello","role":"viewer","id","public","pair"?}`; [pairAdmit] from [PairAdmit.hash]. */
    fun hello(key: DeviceKey, pairAdmit: String? = null): String {
        val m = linkedMapOf<String, Any?>(
            "type" to "hello", "role" to "viewer", "id" to key.id, "public" to AuthCrypto.b64(key.publicKey),
        )
        if (pairAdmit != null) {
            AuthCrypto.unb64(pairAdmit, 32)
            m["pair"] = pairAdmit
        }
        return AuthJson.write(m)
    }

    /** The 32-byte nonce of `{"type":"register-challenge","nonce"}`; throws [AuthJson.Malformed]. */
    fun parseRegisterChallenge(text: String): ByteArray {
        val m = AuthJson.parseObject(text)
        AuthJson.requireExactKeys(m, "type", "nonce")
        if (m["type"] != "register-challenge") throw AuthJson.Malformed("not a register-challenge")
        return AuthCrypto.unb64(AuthJson.string(m, "nonce"), 32)
    }

    /** `{"type":"register-proof","signature"}` answering [nonce] with a viewer-role relay proof. */
    fun registerProof(key: DeviceKey, relayOrigin: String, nonce: ByteArray): String =
        AuthJson.write(
            linkedMapOf(
                "type" to "register-proof",
                "signature" to AuthCrypto.b64(RelayProof.sign(key, relayOrigin, nonce)),
            ),
        )

    /**
     * Parses the viewer's hello-ok: `type`, `pc`, `mediaEpoch`, `iceServers`, then either `ticket`
     * (normal) or `"pairing":true` (pairing window). Unknown keys are ignored (relay-generated, not
     * security relevant); the known ones are checked. Throws [AuthJson.Malformed].
     */
    fun parseHelloOk(text: String, expectedPc: String): HelloOk {
        val m = AuthJson.parseObject(text)
        if (m["type"] != "hello-ok") throw AuthJson.Malformed("not a hello-ok")
        val pc = AuthJson.string(m, "pc")
        if (pc != expectedPc) throw AuthJson.Malformed("hello-ok for another PC")
        val epoch = AuthJson.string(m, "mediaEpoch")
        if (!EPOCH.matches(epoch)) throw AuthJson.Malformed("bad mediaEpoch")
        val ice = m["iceServers"] as? List<*> ?: throw AuthJson.Malformed("iceServers is not an array")
        val pairing = when (val flag = m["pairing"]) {
            null -> false
            true -> true
            else -> throw AuthJson.Malformed("bad pairing flag $flag")
        }
        val ticket = m["ticket"]?.let { it as? String ?: throw AuthJson.Malformed("ticket is not a string") }
        if (pairing != (ticket == null)) throw AuthJson.Malformed("ticket must be present exactly when not pairing")
        if (ticket != null) AuthCrypto.unb64(ticket, 32)
        return HelloOk(pc, epoch, ticket, pairing, AuthJson.write(ice))
    }

    /** `{"type":"hello","role":<media-viewer|file-viewer>,"ticket"}` for a data socket. */
    fun dataHello(role: String, ticket: String): String {
        require(role == ROLE_MEDIA_VIEWER || role == ROLE_FILE_VIEWER) { "not a viewer data role" }
        AuthCrypto.unb64(ticket, 32)
        return AuthJson.write(linkedMapOf("type" to "hello", "role" to role, "ticket" to ticket))
    }

    private val EPOCH = Regex("[0-9a-f]{32}")
}

/** The relay admit hash (decisions item 2). */
object PairAdmit {
    /** b64url(SHA-256("bubbleplay-pair-admit-v1" || canonical PC ID || normalised code)). */
    fun hash(pcId: String, code: String): String = AuthCrypto.b64(AuthCrypto.sha256(bytes(pcId, code)))

    /** The hashed bytes; [pcId] and [code] are canonicalised first (IllegalArgumentException if invalid). */
    fun bytes(pcId: String, code: String): ByteArray {
        val pc = try {
            DeviceId.parse(pcId)
        } catch (e: de.bubblebuffer.bubbleplay.keys.DeviceKeyException) {
            throw IllegalArgumentException("not a PC ID")
        }
        val normalised = requireNotNull(PairingCode.normalise(code)) { "not a pairing code" }
        return AuthCrypto.utf8("bubbleplay-pair-admit-v1" + pc + normalised)
    }
}
