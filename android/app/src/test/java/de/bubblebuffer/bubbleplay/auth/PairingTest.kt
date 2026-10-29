package de.bubblebuffer.bubbleplay.auth

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingTest {
    private val p = pairingVectors
    private val h = hex(handshakeVectors.getString("transcriptHashHex"))

    @Test
    fun pairKeyAndMacsMatchTheFixture() {
        val k = Pairing.pairKey(p.getString("code"))
        assertEquals(p.getString("pairKeyHex"), k.toHex())
        assertEquals(p.getString("viewerMac"), b64(Pairing.viewerMac(k, h)))
        assertEquals(p.getString("pcMac"), b64(Pairing.pcMac(k, h)))
        assertTrue(Pairing.verifyPcMac(k, h, b64(p.getString("pcMac"))))
        assertFalse(Pairing.verifyPcMac(k, h, b64(p.getString("viewerMac"))))
        assertFalse(Pairing.verifyPcMac(k, h, ByteArray(31)))
        val messages = p.getJSONObject("messages")
        assertEquals(messages.getJSONObject("pairProofViewer").getString("mac"), b64(Pairing.viewerMac(k, h)))
        assertEquals(messages.getJSONObject("pairProofPc").getString("mac"), b64(Pairing.pcMac(k, h)))
    }

    @Test
    fun wrongCodeGivesTheFixtureWrongMac() {
        val wrong = Pairing.pairKey(p.getString("wrongCode"))
        assertEquals(p.getString("wrongCodeViewerMac"), b64(Pairing.viewerMac(wrong, h)))
        assertNotEquals(p.getString("viewerMac"), b64(Pairing.viewerMac(wrong, h)))
        assertFalse(Pairing.verifyPcMac(wrong, h, b64(p.getString("pcMac"))))
    }

    @Test
    fun comparisonNumberMatchesTheFixture() {
        val sasBytes = AuthCrypto.utf8("bubbleplay-pair-sas-v1") + h
        assertEquals(p.getString("sasBytesHex"), sasBytes.toHex())
        assertEquals(p.getLong("sasU32"), Pairing.sasU32(h))
        assertEquals(p.getString("sasNumber"), Pairing.sasNumber(h))
        assertEquals(p.getString("sasDisplay"), Pairing.sasDisplay(Pairing.sasNumber(h)))
    }

    @Test
    fun pairKeyNeedsANormalisedCode() {
        assertThrows<IllegalArgumentException> { Pairing.pairKey("70q1k-9mxr1") }
        assertThrows<IllegalArgumentException> { Pairing.sasDisplay("12345") }
    }
}
