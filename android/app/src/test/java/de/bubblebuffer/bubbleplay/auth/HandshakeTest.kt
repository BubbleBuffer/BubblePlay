package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.keys.DeviceId
import de.bubblebuffer.bubbleplay.keys.Ed25519Verify
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class HandshakeTest {
    private val v = handshakeVectors
    private val messages = v.getJSONObject("messages")
    private val pc = v.getJSONObject("pc")

    @Test
    fun authHelloMatchesTheFixture() {
        assertJsonEquals(messages.getJSONObject("authHello"), fixtureHandshake().authHello)
        assertJsonEquals(
            pairingVectors.getJSONObject("messages").getJSONObject("authHello"),
            fixtureHandshake(pairingMode()).authHello,
        )
    }

    @Test
    fun wssCapsAreControlAeadPlusPairing() {
        val normal = Handshake.start(viewerKey(), "https://example.test", "1", normalMode(), fixtureRandom())
        assertEquals(listOf("control-aead-v1"), normal.caps)
        assertEquals(JSONArray(listOf("control-aead-v1")).toString(), JSONObject(normal.authHello).getJSONArray("caps").toString())
        val pairing = Handshake.start(viewerKey(), "https://example.test", "1", pairingMode(), fixtureRandom())
        assertEquals(listOf("control-aead-v1", "pair-v1"), pairing.caps)
    }

    @Test
    fun transcriptAndSignedBytesMatchTheFixture() {
        val handshake = fixtureHandshake()
        val transcript = handshake.transcriptFor(
            pc.getString("id"), b64(pc.getString("public")), b64(pc.getString("nonce")), b64(pc.getString("ephemeral")),
        )
        assertEquals(v.getString("transcriptHex"), transcript.toHex())
        val hash = AuthCrypto.sha256(transcript)
        assertEquals(v.getString("transcriptHashHex"), hash.toHex())
        assertEquals(v.getString("pcSignedBytesHex"), Handshake.pcSignedBytes(hash).toHex())
        assertEquals(v.getString("viewerSignedBytesHex"), Handshake.viewerSignedBytes(hash, b64(v.getString("pcSignature"))).toHex())
        assertTrue(Ed25519Verify.verify(b64(pc.getString("public")), Handshake.pcSignedBytes(hash), b64(v.getString("pcSignature"))))
    }

    @Test
    fun authHostVerifiesAndYieldsFinishAndControlKeys() {
        val result = fixtureHandshake().onAuthHost(authHostText())
        assertJsonEquals(messages.getJSONObject("authFinish"), result.authFinish)
        assertEquals(v.getString("viewerSignature"), JSONObject(result.authFinish).getString("signature"))
        assertEquals(pc.getString("id"), result.pcId)
        assertArrayEquals(b64(pc.getString("public")), result.pcPublic)
        assertEquals(v.getString("transcriptHashHex"), result.transcriptHash.toHex())
        val keys = v.getJSONObject("controlKeys")
        assertEquals(keys.getString("viewerToPcHex"), result.controlKeys.viewerToPc.toHex())
        assertEquals(keys.getString("pcToViewerHex"), result.controlKeys.pcToViewer.toHex())
    }

    @Test
    fun sharedSecretAndKeyScheduleMatchTheFixture() {
        val viewerPrivate = org.bouncycastle.crypto.params.X25519PrivateKeyParameters(hex(v.getJSONObject("viewer").getString("ephemeralPrivateHex")), 0)
        assertEquals(v.getJSONObject("viewer").getString("ephemeral"), b64(viewerPrivate.generatePublicKey().encoded))
        val shared = ByteArray(32)
        viewerPrivate.generateSecret(org.bouncycastle.crypto.params.X25519PublicKeyParameters(b64(pc.getString("ephemeral")), 0), shared, 0)
        assertEquals(v.getString("sharedSecretHex"), shared.toHex())
        val keys = ControlKeys.derive(shared, hex(v.getString("transcriptHashHex")))
        assertEquals(v.getJSONObject("controlKeys").getString("viewerToPcHex"), keys.viewerToPc.toHex())
        assertEquals(v.getJSONObject("controlKeys").getString("pcToViewerHex"), keys.pcToViewer.toHex())
    }

    @Test
    fun pairingModeLearnsTheKey() {
        val result = fixtureHandshake(pairingMode()).onAuthHost(authHostText())
        assertArrayEquals(b64(pc.getString("public")), result.pcPublic)
        assertJsonEquals(messages.getJSONObject("authFinish"), result.authFinish)
    }

    @Test
    fun badSignatureFails() {
        val sig = b64(v.getString("pcSignature")).also { it[5] = (it[5].toInt() xor 1).toByte() }
        assertFailure(AuthFailure.BAD_SIGNATURE, authHostWith { put("signature", b64(sig)) })
        // A valid signature over a different nonce no longer covers the transcript.
        val nonce = b64(pc.getString("nonce")).also { it[0] = (it[0].toInt() xor 1).toByte() }
        assertFailure(AuthFailure.BAD_SIGNATURE, authHostWith { put("nonce", b64(nonce)) })
        assertFailure(AuthFailure.BAD_SIGNATURE, authHostWith { put("signature", b64(ByteArray(64))) })
    }

    @Test
    fun idPublicMismatchFails() {
        assertFailure(AuthFailure.ID_MISMATCH, authHostWith { put("id", "47Z3-3QX1-AJH6-2RKB") })
        assertFailure(AuthFailure.MALFORMED, authHostWith { put("id", "77vh-7m56-8gjk-y12j") })
    }

    @Test
    fun pinnedKeyMismatchIsPcKeyChanged() {
        val other = viewerKey().use { it.publicKey }
        val mode = Handshake.Mode.Normal(DeviceId.fromPublicKey(other), other)
        assertFailure(AuthFailure.PC_KEY_CHANGED, authHostText(), mode)
    }

    @Test
    fun pairingModeRefusesAnotherPc() {
        val mode = Handshake.Mode.Pairing("47Z3-3QX1-AJH6-2RKB", pairingVectors.getString("code"))
        assertFailure(AuthFailure.WRONG_PC, authHostText(), mode)
    }

    @Test
    fun extraOrMissingKeysAndBadFieldsAreMalformed() {
        assertFailure(AuthFailure.MALFORMED, authHostWith { put("caps", JSONArray()) })
        assertFailure(AuthFailure.MALFORMED, authHostWith { remove("ephemeral") })
        assertFailure(AuthFailure.MALFORMED, authHostWith { remove("signature") })
        assertFailure(AuthFailure.MALFORMED, authHostWith { put("type", "auth-hello") })
        assertFailure(AuthFailure.MALFORMED, authHostWith { put("nonce", "AAAA") })
        assertFailure(AuthFailure.MALFORMED, authHostWith { put("public", pc.getString("public") + "=") })
        assertFailure(AuthFailure.MALFORMED, authHostWith { put("ephemeral", 5) })
        assertFailure(AuthFailure.MALFORMED, "not json")
    }

    @Test
    fun lowOrderEphemeralFailsKeyAgreement() {
        // The PC signs a transcript with an all-zero (low-order) ephemeral; agreement must refuse it.
        val zero = ByteArray(32)
        val handshake = fixtureHandshake()
        val transcript = handshake.transcriptFor(pc.getString("id"), b64(pc.getString("public")), b64(pc.getString("nonce")), zero)
        val signature = pcKey().use { it.sign(Handshake.pcSignedBytes(AuthCrypto.sha256(transcript))) }
        val text = authHostWith { put("ephemeral", b64(zero)); put("signature", b64(signature)) }
        val failure = assertThrows<AuthException> { handshake.onAuthHost(text) }
        assertEquals(AuthFailure.KEY_AGREEMENT, failure.failure)
    }

    @Test
    fun authHostIsHandledOnce() {
        val handshake = fixtureHandshake()
        handshake.onAuthHost(authHostText())
        val failure = assertThrows<AuthException> { handshake.onAuthHost(authHostText()) }
        assertEquals(AuthFailure.UNEXPECTED_MESSAGE, failure.failure)
    }

    @Test
    fun strictVerifyRefusesSmallOrderKeys() {
        val identity = ByteArray(32).also { it[0] = 1 }
        assertFalse(Ed25519Verify.verify(identity, ByteArray(0), ByteArray(64).also { it[0] = 1 }))
        assertFalse(Ed25519Verify.verify(ByteArray(31), ByteArray(0), ByteArray(64)))
    }

    @Test
    fun timeoutIsTenSeconds() {
        assertEquals(10_000L, Handshake.AUTH_HOST_TIMEOUT_MS)
        assertEquals(10_000L, ViewerAuth.AUTH_HOST_TIMEOUT_MS)
    }

    private fun assertFailure(expected: AuthFailure, text: String, mode: Handshake.Mode = normalMode()) {
        val failure = assertThrows<AuthException> { fixtureHandshake(mode).onAuthHost(text) }
        assertEquals(expected, failure.failure)
    }
}
