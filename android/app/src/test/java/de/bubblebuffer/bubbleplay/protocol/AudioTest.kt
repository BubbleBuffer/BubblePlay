package de.bubblebuffer.bubbleplay.protocol

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AudioTest {
    private val contract = Fixtures.json("audio-contract.json")
    private val v4Contract = Fixtures.json("wss-v4-contract.json")

    @Test
    fun headerConstantsMatchContract() {
        assertEquals(contract.getInt("headerBytes"), AudioWire.HEADER_BYTES)
        assertEquals(contract.getInt("maxPayloadBytes"), AudioWire.MAX_PAYLOAD_BYTES)
        assertEquals(java.lang.Long.parseLong(contract.getString("magicHex"), 16), AudioWire.MAGIC)
        assertEquals(contract.getInt("version"), AudioWire.VERSION)
    }

    @Test
    fun everyFixturePacketIsAcceptedOrRejectedAsContracted() {
        val packets = Fixtures.objects(contract.getJSONArray("packets"))
        assertTrue(packets.isNotEmpty())
        for (p in packets) {
            val name = p.getString("name")
            if (p.has("hexPrefix")) continue // built in oversizedPayloadIsRejected
            val bytes = Fixtures.hex(p.getString("hex"))
            val packet = AudioWire.parse(bytes)
            if (!p.getBoolean("accepted")) {
                assertNull("$name must be rejected", packet)
                continue
            }
            assertNotNull("$name must parse", packet)
            packet!!
            assertEquals(name, p.getBoolean("discontinuity"), packet.discontinuity)
            assertEquals(name, p.getLong("seq"), packet.seq)
            assertEquals(name, java.lang.Long.parseUnsignedLong(p.getString("captureTsUs")), packet.captureTsUs)
            assertEquals(name, p.getInt("frameSamples"), packet.frameSamples)
            assertEquals(name, p.getInt("frameSamples") * 1_000_000L / AudioWire.SAMPLE_RATE, packet.durationUs)
            assertEquals(name, p.getInt("codec"), AudioWire.CODEC_OPUS)
            assertEquals(name, p.getInt("channels"), AudioWire.CHANNELS)
            assertEquals(name, p.getInt("sampleRate"), AudioWire.SAMPLE_RATE)
            assertArrayEquals(name, Fixtures.hex(p.getString("payloadHex")), packet.payload)
        }
    }

    @Test
    fun oversizedPayloadIsRejected() {
        val p = Fixtures.objects(contract.getJSONArray("packets")).first { it.getString("name") == "oversized-payload" }
        assertFalse(p.getBoolean("accepted"))
        val header = Fixtures.hex(p.getString("hexPrefix"))
        val oversized = header + ByteArray(p.getInt("payloadBytes")) { 0x11 }
        assertNull(AudioWire.parse(oversized))
        // The largest payload the contract allows still parses.
        val largest = header + ByteArray(AudioWire.MAX_PAYLOAD_BYTES) { 0x11 }
        assertNotNull(AudioWire.parse(largest))
    }

    @Test
    fun bpv4PacketIsNotAudio() {
        val v4 = Fixtures.objects(v4Contract.getJSONArray("packets")).first { it.getBoolean("accepted") }
        val bytes = Fixtures.hex(v4.getString("hex"))
        assertFalse(AudioWire.isAudio(bytes))
        assertNull(AudioWire.parse(bytes))
        assertNull(V4.parse(Fixtures.hex(contract.getJSONArray("packets").getJSONObject(0).getString("hex"))))
    }

    @Test
    fun audioMagicIsRecognised() {
        val accepted = Fixtures.hex(Fixtures.objects(contract.getJSONArray("packets")).first { it.getBoolean("accepted") }.getString("hex"))
        assertTrue(AudioWire.isAudio(accepted))
        assertFalse(AudioWire.isAudio(ByteArray(3)))
        assertFalse(AudioWire.isAudio(ByteArray(0)))
    }

    @Test
    fun opusHeadMatchesTheDecoderSetup() {
        val head = AudioWire.opusHead()
        assertEquals(19, head.size)
        assertEquals("OpusHead", String(head, 0, 8, Charsets.US_ASCII))
        // version 1, 2 channels, pre-skip 312 (0x0138) LE, 48000 (0xbb80) LE, gain 0, mapping family 0.
        assertArrayEquals(byteArrayOf(1, 2, 0x38, 0x01, 0x80.toByte(), 0xbb.toByte(), 0, 0, 0, 0, 0), head.copyOfRange(8, 19))
        // 6_500_000 ns = 0x632EA0 and 80_000_000 ns = 0x4C4B400, both as 8-byte little-endian.
        assertArrayEquals(byteArrayOf(0xa0.toByte(), 0x2e, 0x63, 0, 0, 0, 0, 0), AudioWire.longLittleEndian(AudioWire.PRE_SKIP_NS))
        assertArrayEquals(byteArrayOf(0x00, 0xb4.toByte(), 0xc4.toByte(), 0x04, 0, 0, 0, 0), AudioWire.longLittleEndian(AudioWire.SEEK_PRE_ROLL_NS))
        assertEquals(6_500_000L, AudioWire.PRE_SKIP_SAMPLES * 1_000_000_000L / AudioWire.SAMPLE_RATE)
    }

    @Test
    fun startMessageCarriesAudioOptInOnlyWhenDecoderIsAvailable() {
        val start = { JSONObject().put("type", "media-control").put("action", "start").put("protocol", 4) }
        assertEquals(1, AudioWire.optIn(start(), decoderAvailable = true).getInt("audio"))
        assertFalse(AudioWire.optIn(start(), decoderAvailable = false).has("audio"))
    }
}
