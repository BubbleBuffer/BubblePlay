package de.bubblebuffer.bubbleplay.e2e

import de.bubblebuffer.bubbleplay.auth.AuthCrypto
import de.bubblebuffer.bubbleplay.auth.AuthJson
import de.bubblebuffer.bubbleplay.auth.ControlKeys
import de.bubblebuffer.bubbleplay.auth.CtlChannel
import de.bubblebuffer.bubbleplay.auth.CtlMessage
import de.bubblebuffer.bubbleplay.auth.Handshake
import de.bubblebuffer.bubbleplay.auth.PairAdmit
import de.bubblebuffer.bubbleplay.auth.Pairing
import de.bubblebuffer.bubbleplay.auth.RelayProof
import de.bubblebuffer.bubbleplay.auth.ViewerHello
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.Ed25519Verify
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import org.bouncycastle.crypto.params.X25519PrivateKeyParameters
import org.bouncycastle.crypto.params.X25519PublicKeyParameters
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import java.security.SecureRandom
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/** What a test socket saw, in order. */
sealed class Frame {
    data class Text(val text: String) : Frame()
    data class Binary(val size: Int) : Frame()
    data class Closed(val code: Int) : Frame()
    data class Failed(val error: Throwable) : Frame()
}

/** The relay's plaintext `peer-joined` for an admitted viewer (decisions item 22). */
data class PeerJoined(val mediaEpoch: String, val pairing: Boolean)

/**
 * A scripted PC on the relay's /host-ws, built from the auth package's primitives (test code).
 * It registers with TOKEN, publishes `admit`, opens pairing windows and runs the PC side of the
 * handshake, pairing and ctl. Every text it receives is kept in [received].
 */
class FakePc(
    private val relay: RelayServer,
    private val http: OkHttpClient,
    val key: DeviceKey,
) : AutoCloseable {
    val id: String = key.id
    private val inbox = LinkedBlockingQueue<Frame>()
    val received = CopyOnWriteArrayList<String>()
    private var socket: WebSocket? = null
    private val random = SecureRandom()

    /** host hello (with [token]) -> register-challenge -> register-proof (role host) -> hello-ok. */
    fun register(token: String? = RelayServer.TOKEN) {
        val request = Request.Builder().url("${relay.origin}/host-ws?pc=$id").build()
        socket = http.newWebSocket(request, object : WebSocketListener() {
            override fun onMessage(webSocket: WebSocket, text: String) {
                received.add(text)
                inbox.add(Frame.Text(text))
            }

            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                inbox.add(Frame.Binary(bytes.size))
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                inbox.add(Frame.Closed(code))
                webSocket.close(code, null)
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                inbox.add(Frame.Failed(t))
            }
        })
        val hello = linkedMapOf<String, Any?>(
            "type" to "hello", "role" to "host", "id" to id, "public" to AuthCrypto.b64(key.publicKey),
        )
        if (token != null) hello["token"] = token
        send(AuthJson.write(hello))
        val nonce = ViewerHello.parseRegisterChallenge(expect("register-challenge").text)
        val signature = RelayProof.sign(key, relay.origin, nonce, RelayProof.ROLE_HOST)
        send(AuthJson.write(linkedMapOf("type" to "register-proof", "signature" to AuthCrypto.b64(signature))))
        val ok = expect("hello-ok").json
        assertEquals(id, ok["pc"])
    }

    /** Publishes the trusted list, then waits until the relay applied it. */
    fun admit(vararg viewerIds: String) {
        send(AuthJson.write(linkedMapOf("type" to "admit", "viewers" to viewerIds.toList(), "remotePairing" to false)))
        sync()
    }

    /** Opens a pairing window for the normalised [code], then waits until the relay applied it. */
    fun pairOpen(code: String) {
        send(AuthJson.write(linkedMapOf("type" to "pair-open", "admit" to PairAdmit.hash(id, code))))
        sync()
    }

    /** The relay handles one socket's frames in order: the pong means everything before it is applied. */
    fun sync() {
        send("{\"type\":\"ping\"}")
        while (true) {
            val message = next()
            if (message.type == "pong") return
            // A viewer that left meanwhile is relay bookkeeping, not a protocol step.
            assertEquals("PC expected pong, got ${message.text}", "peer-left", message.type)
        }
    }

    fun send(text: String) {
        assertTrue("PC send failed: $text", socket!!.send(text))
    }

    /** The next text message; fails on a close, a binary frame or [timeoutMs] of silence. */
    fun next(timeoutMs: Long = WAIT_MS): Received {
        val frame = inbox.poll(timeoutMs, TimeUnit.MILLISECONDS) ?: die("PC: nothing within $timeoutMs ms")
        return when (frame) {
            is Frame.Text -> Received(frame.text, AuthJson.parseObject(frame.text))
            else -> die("PC: expected a text message, got $frame")
        }
    }

    /** The next message, which must have type [type]. */
    fun expect(type: String, timeoutMs: Long = WAIT_MS): Received {
        val message = next(timeoutMs)
        assertEquals("PC expected $type, got ${message.text}", type, message.type)
        return message
    }

    /**
     * Waits for the relay's `peer-joined` (skipping `peer-left` from an earlier viewer). Any
     * other message first, in particular auth-hello, is an error, as on the real host.
     */
    fun expectPeerJoined(): PeerJoined {
        while (true) {
            val message = next()
            if (message.type == "peer-left") continue
            assertEquals("PC expected peer-joined first, got ${message.text}", "peer-joined", message.type)
            val epoch = message.json["mediaEpoch"] as? String ?: die("peer-joined without mediaEpoch")
            val pairing = message.json["pairing"] as? Boolean ?: die("peer-joined without pairing")
            return PeerJoined(epoch, pairing)
        }
    }

    /** No message for [quietMs]. */
    fun expectQuiet(quietMs: Long) {
        val frame = inbox.poll(quietMs, TimeUnit.MILLISECONDS)
        if (frame != null) fail("PC expected silence, got $frame")
    }

    /**
     * Reads the viewer's auth-hello and answers with auth-host signed by [signer] (normally this
     * PC's key; another key models "PC key changed"). The transcript uses [peer]'s exact epoch.
     */
    fun answerAuthHello(peer: PeerJoined, signer: DeviceKey = key): PcHandshake {
        val hello = expect("auth-hello").json
        val viewerId = AuthJson.string(hello, "id")
        val viewerPublic = AuthCrypto.unb64(AuthJson.string(hello, "public"), 32)
        val viewerNonce = AuthCrypto.unb64(AuthJson.string(hello, "nonce"), 32)
        val viewerEphemeral = AuthCrypto.unb64(AuthJson.string(hello, "ephemeral"), 32)
        @Suppress("UNCHECKED_CAST")
        val caps = hello["caps"] as List<String>
        val pcNonce = ByteArray(32).also(random::nextBytes)
        val ephemeral = X25519PrivateKeyParameters(random)
        val pcEphemeral = ephemeral.generatePublicKey().encoded
        val transcript = Handshake.transcriptBytes(
            relay.origin, peer.mediaEpoch, viewerId, viewerPublic, signer.id, signer.publicKey,
            viewerNonce, pcNonce, viewerEphemeral, pcEphemeral,
        )
        val hash = AuthCrypto.sha256(transcript)
        val signature = signer.sign(Handshake.pcSignedBytes(hash))
        send(
            AuthJson.write(
                linkedMapOf(
                    "type" to "auth-host",
                    "id" to signer.id,
                    "public" to AuthCrypto.b64(signer.publicKey),
                    "nonce" to AuthCrypto.b64(pcNonce),
                    "ephemeral" to AuthCrypto.b64(pcEphemeral),
                    "signature" to AuthCrypto.b64(signature),
                ),
            ),
        )
        val shared = ByteArray(32)
        ephemeral.generateSecret(X25519PublicKeyParameters(viewerEphemeral, 0), shared, 0)
        return PcHandshake(this, viewerId, viewerPublic, caps, hash, signature, ControlKeys.derive(shared, hash).pcChannel())
    }

    override fun close() {
        socket?.cancel()
        socket = null
    }

    class Received(val text: String, val json: Map<String, Any?>) {
        val type: String? get() = json["type"] as? String
    }

    companion object {
        const val WAIT_MS = 10_000L
    }
}

/** The PC side of one handshake after auth-host was sent. */
class PcHandshake(
    private val pc: FakePc,
    val viewerId: String,
    private val viewerPublic: ByteArray,
    val viewerCaps: List<String>,
    private val hash: ByteArray,
    private val pcSignature: ByteArray,
    val channel: CtlChannel,
) {
    val transcriptHash: ByteArray get() = hash.copyOf()

    /** The comparison number the PC shows ("482 913"). */
    val sasDisplay: String get() = Pairing.sasDisplay(Pairing.sasNumber(hash))

    /** Reads auth-finish and verifies the viewer's signature over H || PC signature. */
    fun awaitAuthFinish() {
        val finish = pc.expect("auth-finish").json
        val signature = AuthCrypto.unb64(AuthJson.string(finish, "signature"), 64)
        assertTrue(
            "viewer auth-finish signature",
            Ed25519Verify.verify(viewerPublic, Handshake.viewerSignedBytes(hash, pcSignature), signature),
        )
    }

    /**
     * Pairing (decisions item 17): reads the viewer's pair-proof, checks it against [code], then
     * sends the PC's pair-proof (a wrong one when [wrongMac]).
     */
    fun exchangePairProofs(code: String, wrongMac: Boolean = false) {
        val proof = pc.expect("pair-proof").json
        val k = Pairing.pairKey(code)
        val viewerMac = AuthCrypto.unb64(AuthJson.string(proof, "mac"), 32)
        assertTrue("viewer pair-proof mac", Pairing.viewerMac(k, hash).contentEquals(viewerMac))
        val mac = if (wrongMac) Pairing.viewerMac(k, hash) else Pairing.pcMac(k, hash)
        pc.send(AuthJson.write(linkedMapOf("type" to "pair-proof", "mac" to AuthCrypto.b64(mac))))
    }

    /** Seals and sends one ctl message. */
    fun sendCtl(kind: String, plaintext: ByteArray) = pc.send(channel.seal(kind, plaintext))

    fun sendCtlJson(fields: Map<String, Any?>) =
        sendCtl(fields["type"] as String, AuthJson.write(fields).toByteArray(Charsets.UTF_8))

    /** The next message, which must be a ctl envelope that opens on this channel. */
    fun receiveCtl(): CtlMessage {
        val message = pc.expect("ctl")
        return channel.open(message.text)
    }
}
