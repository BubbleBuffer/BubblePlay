package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.Fixtures
import de.bubblebuffer.bubbleplay.log.allText
import de.bubblebuffer.bubbleplay.log.logLines
import de.bubblebuffer.bubbleplay.log.withTestLog
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class ViewerAuthTest {
    private val pairMessages = pairingVectors.getJSONObject("messages")
    private val pcProof = pairMessages.getJSONObject("pairProofPc").toString()
    private val pairDone = pairMessages.getJSONObject("pairDone").toString()
    private val pairDenied = pairMessages.getJSONObject("pairDenied").toString()

    private fun normal() = ViewerAuth(fixtureHandshake())

    private fun pairing() = ViewerAuth(fixtureHandshake(pairingMode()))

    private fun assertFailed(reason: AuthFailure, step: AuthStep) {
        assertEquals(AuthEvent.Failed(reason), step.event)
        assertTrue(step.send.isEmpty())
    }

    @Test
    fun normalModeAuthenticates() {
        val auth = normal()
        val hello = auth.start()
        assertNull(hello.event)
        assertEquals(1, hello.send.size)
        assertJsonEquals(handshakeVectors.getJSONObject("messages").getJSONObject("authHello"), hello.send[0])

        val step = auth.onMessage(authHostText())
        assertEquals(1, step.send.size)
        assertJsonEquals(handshakeVectors.getJSONObject("messages").getJSONObject("authFinish"), step.send[0])
        val event = step.event as AuthEvent.Authenticated
        assertEquals(pcId(), event.pcId)
        assertArrayEquals(pcPublic(), event.pcPublic)

        // The channel is the viewer's: it opens the PC's vectors and seals the viewer's.
        val vectors = Fixtures.objects(phase2.getJSONObject("ctl").getJSONArray("messages"))
        for (vec in vectors.filter { it.getString("direction") == "pc-to-viewer" }) {
            assertEquals(vec.getString("plaintextHex"), event.channel.open(vec.getJSONObject("message").toString()).plaintext.toHex())
        }
        val first = vectors.first { it.getString("direction") == "viewer-to-pc" }
        assertJsonEquals(first.getJSONObject("message"), event.channel.seal(first.getString("kind"), hex(first.getString("plaintextHex"))))

        // Anything after the end, including a repeated auth-host, fails.
        assertFailed(AuthFailure.UNEXPECTED_MESSAGE, auth.onMessage(authHostText()))
    }

    @Test
    fun pairingModeAllowed() {
        val auth = pairing()
        assertJsonEquals(pairMessages.getJSONObject("authHello"), auth.start().send.single())
        val step = auth.onMessage(authHostText())
        assertNull(step.event)
        assertEquals(2, step.send.size)
        assertJsonEquals(handshakeVectors.getJSONObject("messages").getJSONObject("authFinish"), step.send[0])
        assertJsonEquals(pairMessages.getJSONObject("pairProofViewer"), step.send[1])

        val number = auth.onMessage(pcProof)
        assertTrue(number.send.isEmpty())
        assertEquals(AuthEvent.PairNumber(pairingVectors.getString("sasNumber"), pairingVectors.getString("sasDisplay")), number.event)

        val done = auth.onMessage(pairDone).event as AuthEvent.Paired
        assertEquals(pcId(), done.pcId)
        assertArrayEquals(pcPublic(), done.pcPublic)
        assertFailed(AuthFailure.UNEXPECTED_MESSAGE, auth.onMessage(pairDone))
    }

    @Test
    fun pairingModeDenied() {
        val auth = pairing()
        auth.start()
        auth.onMessage(authHostText())
        auth.onMessage(pcProof)
        assertSame(AuthEvent.Denied, auth.onMessage(pairDenied).event)

        val early = pairing()
        early.start()
        early.onMessage(authHostText())
        assertSame(AuthEvent.Denied, early.onMessage(pairDenied).event)
    }

    @Test
    fun wrongPcMacFailsWithoutANumber() {
        val auth = pairing()
        auth.start()
        auth.onMessage(authHostText())
        val wrong = JSONObject().put("type", "pair-proof").put("mac", pairingVectors.getString("viewerMac")).toString()
        assertFailed(AuthFailure.PAIR_MAC_MISMATCH, auth.onMessage(wrong))
        assertFailed(AuthFailure.UNEXPECTED_MESSAGE, auth.onMessage(pcProof))
    }

    @Test
    fun wrongCodeOnTheViewerFailsAgainstTheRealPcMac() {
        val mode = Handshake.Mode.Pairing(pcId(), pairingVectors.getString("wrongCode"))
        val auth = ViewerAuth(fixtureHandshake(mode))
        auth.start()
        val step = auth.onMessage(authHostText())
        assertEquals(pairingVectors.getString("wrongCodeViewerMac"), JSONObject(step.send[1]).getString("mac"))
        assertFailed(AuthFailure.PAIR_MAC_MISMATCH, auth.onMessage(pcProof))
    }

    @Test
    fun outOfOrderMessagesFail() {
        // Before start.
        assertFailed(AuthFailure.UNEXPECTED_MESSAGE, normal().onMessage(authHostText()))
        // Pairing messages before auth-host.
        for (early in listOf(pcProof, pairDone, pairDenied)) {
            val auth = pairing()
            auth.start()
            assertFailed(AuthFailure.UNEXPECTED_MESSAGE, auth.onMessage(early))
        }
        // pair-done before the PC's proof.
        pairing().apply {
            start()
            onMessage(authHostText())
            assertFailed(AuthFailure.UNEXPECTED_MESSAGE, onMessage(pairDone))
        }
        // A second PC proof after the number.
        pairing().apply {
            start()
            onMessage(authHostText())
            onMessage(pcProof)
            assertFailed(AuthFailure.UNEXPECTED_MESSAGE, onMessage(pcProof))
        }
        // Pairing messages in normal mode.
        normal().apply {
            start()
            assertFailed(AuthFailure.UNEXPECTED_MESSAGE, onMessage(pcProof))
        }
        normal().apply {
            start()
            onMessage(authHostText())
            assertFailed(AuthFailure.UNEXPECTED_MESSAGE, onMessage(pairDone))
        }
        // Other types and a second start.
        normal().apply {
            start()
            assertFailed(AuthFailure.UNEXPECTED_MESSAGE, onMessage("""{"type":"ctl","kind":"caps","seq":0,"data":"AAAA"}"""))
        }
        normal().apply {
            start()
            assertFailed(AuthFailure.UNEXPECTED_MESSAGE, start())
        }
    }

    @Test
    fun failuresAreLoggedWithTheirCauseAndNoSecrets() = withTestLog { log, dir ->
        normal().apply {
            start()
            onMessage(authHostWith { put("signature", b64(ByteArray(64))) })
        }
        pairing().apply {
            start()
            onMessage(authHostText())
            onMessage("""{"type":"pair-proof","mac":"AAAA"}""")
        }
        log.flush()
        val lines = dir.logLines().filter { it.getString("msg") == "viewer auth failed" }
        assertEquals(2, lines.size)
        assertEquals(pcId(), lines[0].getString("pc"))
        assertEquals("BAD_SIGNATURE", lines[0].getJSONObject("fields").getString("failure"))
        assertEquals("normal", lines[0].getJSONObject("fields").getString("mode"))
        assertEquals("MALFORMED", lines[1].getJSONObject("fields").getString("failure"))
        assertEquals("pairing", lines[1].getJSONObject("fields").getString("mode"))
        assertFalse(dir.allText().contains(pairingVectors.getString("code")))
    }

    @Test
    fun malformedAndFailedAuthHostEndTheSession() {
        normal().apply {
            start()
            assertFailed(AuthFailure.MALFORMED, onMessage("{"))
            assertFailed(AuthFailure.UNEXPECTED_MESSAGE, onMessage(authHostText()))
        }
        normal().apply {
            start()
            assertFailed(AuthFailure.BAD_SIGNATURE, onMessage(authHostWith { put("signature", b64(ByteArray(64))) }))
        }
        val other = viewerKey().use { it.publicKey }
        ViewerAuth(fixtureHandshake(Handshake.Mode.Normal(de.bubblebuffer.bubbleplay.keys.DeviceId.fromPublicKey(other), other))).apply {
            start()
            assertFailed(AuthFailure.PC_KEY_CHANGED, onMessage(authHostText()))
        }
        pairing().apply {
            start()
            onMessage(authHostText())
            assertFailed(AuthFailure.MALFORMED, onMessage("""{"type":"pair-proof","mac":"AAAA"}"""))
        }
        pairing().apply {
            start()
            onMessage(authHostText())
            onMessage(pcProof)
            assertFailed(AuthFailure.MALFORMED, onMessage("""{"type":"pair-done","x":1}"""))
        }
    }
}
