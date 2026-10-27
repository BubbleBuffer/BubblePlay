package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.Fixtures
import de.bubblebuffer.bubbleplay.auth.ControlKeys
import de.bubblebuffer.bubbleplay.auth.CtlChannel
import de.bubblebuffer.bubbleplay.auth.Handshake
import de.bubblebuffer.bubbleplay.auth.ViewerAuth
import de.bubblebuffer.bubbleplay.auth.authHostText
import de.bubblebuffer.bubbleplay.auth.b64
import de.bubblebuffer.bubbleplay.auth.contract
import de.bubblebuffer.bubbleplay.auth.fixtureRandom
import de.bubblebuffer.bubbleplay.auth.handshakeVectors
import de.bubblebuffer.bubbleplay.auth.hex
import de.bubblebuffer.bubbleplay.auth.normalMode
import de.bubblebuffer.bubbleplay.auth.pcId
import de.bubblebuffer.bubbleplay.auth.pcKey
import de.bubblebuffer.bubbleplay.auth.pcPublic
import de.bubblebuffer.bubbleplay.auth.viewerKey
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.security.MessageDigest

class SessionWireTest {
    /** Fixture keys/nonces, with the relay's opaque hex epoch and a freshly signed transcript. */
    private class Script(val epoch: String = "f".repeat(32)) {
        val sent = mutableListOf<String>()
        var rejectType: String? = null
        val viewer = viewerKey()
        val origin = handshakeVectors.getString("relayOrigin")
        val wire = SessionWire(viewer, origin, pcId(), pcPublic(), "Phone\n\t\u0000" + "x".repeat(90),
            send = { text ->
                val accepted = JSONObject(text).getString("type") != rejectType
                if (accepted) sent.add(text)
                accepted
            }, random = fixtureRandom())
        private val transcript = Handshake.start(viewer, origin, epoch, normalMode(), fixtureRandom()).transcriptFor(
            pcId(), pcPublic(), b64(handshakeVectors.getJSONObject("pc").getString("nonce")),
            b64(handshakeVectors.getJSONObject("pc").getString("ephemeral")),
        )
        private val hash = MessageDigest.getInstance("SHA-256").digest(transcript)
        val pcChannel: CtlChannel = ControlKeys.derive(hex(handshakeVectors.getString("sharedSecretHex")), hash)
            .let { keys -> keys.pcChannel().also { keys.wipe() } }
        val authHost: String = JSONObject(authHostText()).put("signature", pcKey().use {
            b64(it.sign(Handshake.pcSignedBytes(hash)))
        }).toString()

        fun challenge() {
            assertTrue(wire.start())
            val vector = Fixtures.objects(contract.getJSONObject("relayProof").getJSONArray("vectors"))
                .first { it.getString("name") == "viewer" }
            wire.receive(JSONObject().put("type", "register-challenge").put("nonce", vector.getString("nonce")).toString())
        }

        fun helloOk() = JSONObject().put("type", "hello-ok").put("pc", pcId()).put("mediaEpoch", epoch)
            .put("ticket", b64(ByteArray(32) { 5 })).put("iceServers", JSONArray())

        fun admit() {
            challenge()
            val event = wire.receive(helloOk().toString())
            assertTrue(event is SessionWire.Event.Admitted)
            assertEquals(epoch, (event as SessionWire.Event.Admitted).hello.mediaEpoch)
        }

        fun authenticate() {
            admit()
            assertEquals(SessionWire.Event.Authenticated, wire.receive(authHost))
        }

        fun pcCaps(): String = pcChannel.seal("caps", """{"type":"caps","caps":["control-aead-v1"]}""".toByteArray())
    }

    @Test
    fun signedHelloChallengeProofPrecedeThePinnedHandshakeAndFirstCaps() {
        val s = Script()
        s.admit()
        val hello = JSONObject(s.sent[0])
        assertEquals("viewer", hello.getString("role"))
        assertEquals(s.viewer.id, hello.getString("id"))
        val proof = JSONObject(s.sent[1])
        val vector = Fixtures.objects(contract.getJSONObject("relayProof").getJSONArray("vectors"))
            .first { it.getString("name") == "viewer" }
        assertEquals("register-proof", proof.getString("type"))
        assertEquals(vector.getString("signature"), proof.getString("signature"))
        assertEquals("auth-hello", JSONObject(s.sent[2]).getString("type"))
        assertFalse(s.wire.sendJson("""{"type":"media-control","action":"start"}"""))
        assertFalse(s.wire.sendJson("""{"type":"ping"}"""))
        assertFalse(s.wire.sendInput(byteArrayOf(1)))
        assertEquals(3, s.sent.size)

        assertEquals(SessionWire.Event.Authenticated, s.wire.receive(s.authHost))
        assertEquals("auth-finish", JSONObject(s.sent[3]).getString("type"))
        assertEquals(0, JSONObject(s.sent[4]).getInt("seq"))
        val first = s.pcChannel.open(s.sent[4])
        assertEquals("caps", first.kind)
        val caps = JSONObject(first.text())
        assertEquals("control-aead-v1", caps.getJSONArray("caps").getString(0))
        assertEquals(64, caps.getString("name").length)
        assertFalse(caps.getString("name").any { Character.isISOControl(it) })
    }

    @Test
    fun allApplicationSendersUseCtlAndBinaryInputKeepsItsExactBytes() {
        val s = Script().apply { authenticate() }
        s.pcChannel.open(s.sent.last()) // viewer caps, seq 0
        val types = listOf("media-control", "first-frame", "clipboard", "secure-action", "file-control", "stats", "caps")
        for (type in types) {
            val message = JSONObject().put("type", type).put("marker", type).toString()
            assertTrue(s.wire.sendJson(message))
            val frame = JSONObject(s.sent.last())
            assertEquals("ctl", frame.getString("type"))
            val opened = s.pcChannel.open(frame.toString())
            assertEquals(type, opened.kind)
            assertEquals(message, opened.text())
        }
        val input = byteArrayOf(1, 0, 0xff.toByte(), 13, 10)
        assertTrue(s.wire.sendInput(input))
        val opened = s.pcChannel.open(s.sent.last())
        assertEquals("input", opened.kind)
        assertArrayEquals(input, opened.plaintext)
        assertTrue(s.wire.sendJson("""{"type":"ping"}"""))
        assertEquals("ping", JSONObject(s.sent.last()).getString("type"))
    }

    @Test
    fun plaintextEndpointMessagesAreDroppedAndCtlCursorIsDecodedOnlyAfterPcCaps() {
        val s = Script().apply { authenticate() }
        val endpoints = listOf("host-capabilities", "input-capabilities", "desktop-state", "clipboard", "monitor-list", "first-frame", "cursor", "caps")
        for (type in endpoints) assertEquals(SessionWire.Event.Ignored, s.wire.receive("""{"type":"$type"}"""))
        assertTrue(s.wire.receive(s.pcCaps()) is SessionWire.Event.Endpoint)
        val clipboard = """{"type":"clipboard","text":"test"}"""
        assertEquals(SessionWire.Event.Endpoint(clipboard), s.wire.receive(s.pcChannel.seal("clipboard", clipboard.toByteArray())))
        val bytes = byteArrayOf(0x80.toByte(), 0, 1, 0, 2, 0, 0, 0)
        val cursor = s.wire.receive(s.pcChannel.seal("cursor", bytes)) as SessionWire.Event.Cursor
        assertArrayEquals(bytes, cursor.bytes)
        for (type in listOf("ping", "pong", "peer-left", "peer-joined", "error", "pair-closed")) {
            assertTrue(s.wire.receive("""{"type":"$type"}""") is SessionWire.Event.Relay)
        }
    }

    @Test
    fun replayGapAndBadTagEndTheControlSession() {
        for (failure in listOf("replay", "gap", "tag")) {
            val s = Script().apply { authenticate() }
            val caps = s.pcCaps()
            val frame = when (failure) {
                "replay" -> { s.wire.receive(caps); caps }
                "gap" -> JSONObject(caps).put("seq", 1).toString()
                else -> JSONObject(caps).apply {
                    val data = b64(getString("data"))
                    data[0] = (data[0].toInt() xor 1).toByte()
                    put("data", b64(data))
                }.toString()
            }
            val result = s.wire.receive(frame) as SessionWire.Event.Failed
            assertEquals(SessionProblem.CONTROL_FAILED, result.problem)
            assertFalse(s.wire.sendInput(byteArrayOf(1)))
            assertFalse(s.wire.sendJson("""{"type":"media-control"}"""))
            assertEquals(SessionWire.Event.Ignored, s.wire.receive(caps))
        }
    }

    @Test
    fun missingMandatoryCapsShowThePcRefusalEvenAsTheFirstControlMessage() {
        for (code in listOf("cap-missing", "dtls-bind-required")) {
            val s = Script().apply { authenticate() }
            val refusal = JSONObject().put("type", "error").put("code", code).put("message", "Required feature missing")
            val result = s.wire.receive(s.pcChannel.seal("error", refusal.toString().toByteArray())) as SessionWire.Event.Failed
            assertEquals(SessionProblem.CAP_REFUSED, result.problem)
            assertEquals("Required feature missing", result.detail)
        }
    }

    @Test
    fun policyClosesStopAndOrdinaryDropsReconnect() {
        assertEquals(SessionProblem.RELAY_REFUSED, SessionProblem.closed(1008, admitted = false))
        assertEquals(SessionProblem.DEVICE_REMOVED, SessionProblem.closed(1008, admitted = true))
        for (code in listOf(null, 1000, 1001, 1006, 1011, 401, 403)) {
            assertNull(SessionProblem.closed(code, admitted = false))
            assertNull(SessionProblem.closed(code, admitted = true))
        }
    }

    @Test
    fun keyChangeAndAuthenticationTimeoutAreTerminalAndNeverSendApplicationControl() {
        val changed = Script().apply { admit() }
        val otherPc = JSONObject(changed.authHost).put("id", changed.viewer.id).put("public", b64(changed.viewer.publicKey))
        val result = changed.wire.receive(otherPc.toString()) as SessionWire.Event.Failed
        assertEquals(SessionProblem.PC_KEY_CHANGED, result.problem)
        assertFalse(changed.wire.sendJson("""{"type":"media-control"}"""))
        assertEquals(3, changed.sent.size)

        val timeout = Script().apply { admit() }
        assertEquals(10_000L, ViewerAuth.AUTH_HOST_TIMEOUT_MS)
        assertEquals(SessionProblem.AUTH_TIMEOUT, (timeout.wire.authHostTimedOut() as SessionWire.Event.Failed).problem)
        assertEquals(SessionWire.Event.Ignored, timeout.wire.receive(timeout.authHost))
        val ready = Script().apply { authenticate() }
        assertEquals(SessionWire.Event.Ignored, ready.wire.authHostTimedOut())
    }

    @Test
    fun failedTransportSendCannotPublishAuthenticationOrCaps() {
        for (type in listOf("register-proof", "auth-hello", "auth-finish", "ctl")) {
            val s = Script()
            if (type == "register-proof") {
                s.wire.start()
                s.rejectType = type
                assertEquals(SessionWire.Event.Disconnected,
                    s.wire.receive("""{"type":"register-challenge","nonce":"${b64(ByteArray(32))}"}"""))
            } else if (type == "auth-hello") {
                s.challenge()
                s.rejectType = type
                assertEquals(SessionWire.Event.Disconnected, s.wire.receive(s.helloOk().toString()))
            } else {
                s.admit()
                s.rejectType = type
                assertEquals(SessionWire.Event.Disconnected, s.wire.receive(s.authHost))
            }
            assertFalse(s.wire.sendInput(byteArrayOf(1)))
        }
    }

    @Test
    fun everyNewConnectionRestartsWithCapsAtSequenceZero() {
        for (epoch in listOf("a".repeat(32), "b".repeat(32))) {
            val s = Script(epoch).apply { authenticate() }
            assertEquals(0, JSONObject(s.sent.last()).getInt("seq"))
            assertEquals("caps", s.pcChannel.open(s.sent.last()).kind)
            assertTrue(s.wire.receive(s.pcCaps()) is SessionWire.Event.Endpoint)
        }
    }

    @Test
    fun admissionCannotSkipTheRelayChallengeOrBecomePairing() {
        val skipped = Script()
        skipped.wire.start()
        assertEquals(SessionProblem.AUTH_FAILED, (skipped.wire.receive(skipped.helloOk().toString()) as SessionWire.Event.Failed).problem)
        val pairing = Script().apply { challenge() }
        val hello = pairing.helloOk().apply { remove("ticket"); put("pairing", true) }
        assertEquals(SessionProblem.AUTH_FAILED, (pairing.wire.receive(hello.toString()) as SessionWire.Event.Failed).problem)
        assertFalse(pairing.wire.admitted)
    }
}
