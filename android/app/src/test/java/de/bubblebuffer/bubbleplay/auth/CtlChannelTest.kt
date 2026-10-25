package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

class CtlChannelTest {
    private val ctl = phase2.getJSONObject("ctl")
    private val vectors = Fixtures.objects(ctl.getJSONArray("messages"))
    private val keys = handshakeVectors.getJSONObject("controlKeys")
    private val v2p = hex(keys.getString("viewerToPcHex"))
    private val p2v = hex(keys.getString("pcToViewerHex"))

    private fun keys() = ControlKeys(v2p, p2v)

    /** The channel that opens messages travelling in [direction]. */
    private fun receiverFor(direction: String) =
        if (direction == "viewer-to-pc") keys().pcChannel() else keys().viewerChannel()

    private fun senderFor(direction: String) =
        if (direction == "viewer-to-pc") keys().viewerChannel() else keys().pcChannel()

    @Test
    fun sealsEveryVectorInBothDirections() {
        for (direction in listOf("viewer-to-pc", "pc-to-viewer")) {
            val sender = senderFor(direction)
            for (vec in vectors.filter { it.getString("direction") == direction }.sortedBy { it.getInt("seq") }) {
                val envelope = sender.seal(vec.getString("kind"), hex(vec.getString("plaintextHex")))
                assertJsonEquals(vec.getJSONObject("message"), envelope)
                assertEquals(vec.getString("aadUtf8"), vec.getString("kind"))
                assertEquals(String.format("%024x", vec.getInt("seq")), vec.getString("nonceHex"))
            }
        }
    }

    @Test
    fun opensEveryVectorInBothDirections() {
        for (direction in listOf("viewer-to-pc", "pc-to-viewer")) {
            val receiver = receiverFor(direction)
            for (vec in vectors.filter { it.getString("direction") == direction }.sortedBy { it.getInt("seq") }) {
                val opened = receiver.open(vec.getJSONObject("message").toString())
                assertEquals(vec.getString("kind"), opened.kind)
                assertEquals(vec.getString("plaintextHex"), opened.plaintext.toHex())
                if (vec.has("plaintextUtf8")) assertEquals(vec.getString("plaintextUtf8"), opened.text())
            }
        }
    }

    @Test
    fun fixtureFailuresEndTheChannel() {
        for (failure in Fixtures.objects(ctl.getJSONArray("failures"))) {
            val direction = failure.getString("direction")
            val receiver = receiverFor(direction)
            val ordered = vectors.filter { it.getString("direction") == direction }.sortedBy { it.getInt("seq") }
            repeat(failure.getInt("expectedSeq")) { receiver.open(ordered[it].getJSONObject("message").toString()) }
            val expectedReason = when (failure.getString("name")) {
                "replayed-seq", "skipped-seq" -> CtlException.Reason.OUT_OF_ORDER
                else -> CtlException.Reason.AUTH_FAILED
            }
            val thrown = assertThrows<CtlException> { receiver.open(failure.getJSONObject("message").toString()) }
            assertEquals(failure.getString("name"), expectedReason, thrown.reason)
            assertEquals("end-session", failure.getString("expected"))
            assertClosed(receiver, ordered[failure.getInt("expectedSeq")].getJSONObject("message").toString())
        }
    }

    @Test
    fun phaseZeroControlMessagesRoundTrip() {
        val messages = Fixtures.objects(handshakeVectors.getJSONArray("controlMessages"))
        for (direction in listOf("viewer-to-pc", "pc-to-viewer")) {
            val sender = senderFor(direction)
            val receiver = receiverFor(direction)
            for (m in messages.filter { it.getString("direction") == direction }.sortedBy { it.getInt("sequence") }) {
                val envelope = JSONObject(sender.seal(m.getString("type"), m.getString("plaintextUtf8").toByteArray()))
                assertEquals(m.getString("ciphertext"), envelope.getString("data"))
                assertEquals(m.getInt("sequence"), envelope.getInt("seq"))
                assertEquals(m.getString("plaintextUtf8"), receiver.open(envelope.toString()).text())
            }
        }
    }

    @Test
    fun plaintextTypeMustEqualKind() {
        val forged = envelope(p2v, 0, "cursor", """{"type":"clipboard","text":"x"}""".toByteArray())
        assertFails(CtlException.Reason.TYPE_MISMATCH, forged)
        assertFails(CtlException.Reason.TYPE_MISMATCH, envelope(p2v, 0, "cursor", "not json".toByteArray()))
        assertFails(CtlException.Reason.TYPE_MISMATCH, envelope(p2v, 0, "cursor", byteArrayOf(0x7b, 0xff.toByte(), 0x7d)))
        // The sender refuses such plaintexts without closing.
        val sender = keys().viewerChannel()
        assertThrows<IllegalArgumentException> { sender.seal("cursor", """{"type":"clipboard"}""".toByteArray()) }
        assertThrows<IllegalArgumentException> { sender.seal("Bad_Kind", """{"type":"Bad_Kind"}""".toByteArray()) }
        assertJsonEquals(vectors[0].getJSONObject("message"), sender.seal("input", hex(vectors[0].getString("plaintextHex"))))
    }

    @Test
    fun envelopeShapeIsStrict() {
        val good = JSONObject(vectors.first { it.getString("direction") == "pc-to-viewer" }.getJSONObject("message").toString())
        assertFails(CtlException.Reason.MALFORMED, JSONObject(good.toString()).put("extra", 1).toString())
        assertFails(CtlException.Reason.MALFORMED, JSONObject(good.toString()).apply { remove("data") }.toString())
        assertFails(CtlException.Reason.MALFORMED, JSONObject(good.toString()).put("type", "ctl2").toString())
        assertFails(CtlException.Reason.MALFORMED, good.toString().replace("\"seq\":0", "\"seq\":0.0"))
        assertFails(CtlException.Reason.MALFORMED, JSONObject(good.toString()).put("seq", "0").toString())
        assertFails(CtlException.Reason.MALFORMED, JSONObject(good.toString()).put("data", good.getString("data") + "=").toString())
        assertFails(CtlException.Reason.MALFORMED, JSONObject(good.toString()).put("data", "AAAA").toString())
        assertFails(CtlException.Reason.BAD_KIND, JSONObject(good.toString()).put("kind", "Clipboard").toString())
        assertFails(CtlException.Reason.BAD_KIND, JSONObject(good.toString()).put("kind", "a".repeat(33)).toString())
        assertFails(CtlException.Reason.BAD_KIND, JSONObject(good.toString()).put("kind", "").toString())
        assertFails(CtlException.Reason.TOO_LONG, JSONObject(good.toString()).put("data", "A".repeat(CtlChannel.DATA_MAX + 1)).toString())
        assertFails(CtlException.Reason.MALFORMED, "[]")
    }

    @Test
    fun sealAfterFailureIsRefused() {
        val channel = keys().viewerChannel()
        assertThrows<CtlException> { channel.open("{}") }
        val thrown = assertThrows<CtlException> { channel.seal("input", byteArrayOf(1)) }
        assertEquals(CtlException.Reason.CLOSED, thrown.reason)
        assertTrue(channel.isClosed)
    }

    @Test
    fun capsIsTheFirstPcMessageKind() {
        val pc = keys().pcChannel()
        val viewer = keys().viewerChannel()
        val caps = """{"type":"caps","caps":["control-aead-v1"]}"""
        val opened = viewer.open(pc.seal(CtlChannel.KIND_CAPS, caps.toByteArray()))
        assertEquals("caps", opened.kind)
        assertArrayEquals(caps.toByteArray(), opened.plaintext)
    }

    private fun assertFails(reason: CtlException.Reason, envelope: String) {
        val receiver = keys().viewerChannel()
        val thrown = assertThrows<CtlException> { receiver.open(envelope) }
        assertEquals(envelope.take(200), reason, thrown.reason)
        assertClosed(receiver, vectors.first { it.getString("direction") == "pc-to-viewer" }.getJSONObject("message").toString())
    }

    private fun assertClosed(channel: CtlChannel, validNext: String) {
        assertTrue(channel.isClosed)
        val thrown = assertThrows<CtlException> { channel.open(validNext) }
        assertEquals(CtlException.Reason.CLOSED, thrown.reason)
    }

    private fun envelope(key: ByteArray, seq: Long, kind: String, plaintext: ByteArray): String {
        val nonce = ByteArray(12).also { for (i in 0 until 8) it[4 + i] = (seq ushr (56 - 8 * i)).toByte() }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(128, nonce))
        cipher.updateAAD(kind.toByteArray())
        return JSONObject().put("type", "ctl").put("kind", kind).put("seq", seq).put("data", b64(cipher.doFinal(plaintext))).toString()
    }
}
