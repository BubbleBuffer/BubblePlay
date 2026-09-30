package de.bubblebuffer.bubbleplay.protocol

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class V4Test {
    private val contract = Fixtures.json("wss-v4-contract.json")
    private val vectors = Fixtures.resource("v4-client-vectors.json")

    @Test
    fun headerConstantsMatchContract() {
        assertEquals(contract.getInt("headerBytes"), V4.HEADER_BYTES)
        assertEquals(contract.getInt("maxPayloadBytes"), V4.MAX_PAYLOAD_BYTES)
    }

    @Test
    fun everyFixturePacketParsesOrIsRejected() {
        val packets = Fixtures.objects(contract.getJSONArray("packets"))
        assertTrue(packets.isNotEmpty())
        for (p in packets) {
            val name = p.getString("name")
            val bytes = Fixtures.hex(p.getString("hex"))
            val frame = V4.parse(bytes)
            if (!p.getBoolean("accepted")) {
                assertNull("$name must be rejected", frame)
                continue
            }
            assertNotNull("$name must parse", frame)
            frame!!
            assertEquals(name, p.getBoolean("key"), frame.key)
            assertEquals(name, p.getBoolean("refine"), frame.refine)
            assertEquals(name, p.getInt("codec"), frame.codec)
            assertEquals(name, p.getLong("generation"), frame.generation)
            assertEquals(name, p.getLong("seq"), frame.seq)
            assertEquals(name, java.lang.Long.parseUnsignedLong(p.getString("captureTsUs")), frame.captureTsUs)
            assertEquals(name, p.getInt("width"), frame.width)
            assertEquals(name, p.getInt("height"), frame.height)
            assertArrayEquals(name, Fixtures.hex(p.getString("payloadHex")), frame.data)
            // Round trip: encoding the parsed frame gives the fixture bytes back.
            assertArrayEquals("$name re-encodes", bytes, V4.encode(frame))
        }
    }

    @Test
    fun oversizedAndHeaderOnlyPacketsAreRejected() {
        val ok = Fixtures.hex(Fixtures.objects(contract.getJSONArray("packets")).first { it.getBoolean("accepted") }.getString("hex"))
        assertNull(V4.parse(ok.copyOf(V4.HEADER_BYTES)))
        assertNull(V4.parse(ByteArray(0)))
        assertNull(V4.parse(ByteArray(V4.MAX_PACKET_BYTES + 1).also { ok.copyInto(it, 0, 0, V4.HEADER_BYTES) }))
    }

    @Test
    fun fixtureClientMessagesThatWeEmitAreByteIdentical() {
        // The contract's accepted ack/keyframe/ping texts are exactly what the client must produce.
        for (m in Fixtures.objects(contract.getJSONArray("clientMessages")).filter { it.getBoolean("accepted") }) {
            val text = m.getString("text")
            val msg = JSONObject(text)
            val ours = when (msg.getString("type")) {
                "ack" -> V4.ackMessage(
                    msg.getLong("generation"), msg.getLong("seq"), msg.getDouble("recvTsMs"),
                    if (msg.has("decodedSeq")) msg.getLong("decodedSeq") else null,
                )
                "keyframe" -> V4.keyframeMessage(msg.getString("reason"))
                "ping" -> V4.PING
                else -> error("unexpected $text")
            }
            assertEquals(text, ours)
        }
    }

    @Test
    fun ackMatchesMediaV4Js() {
        for (v in Fixtures.objects(vectors.getJSONArray("acks"))) {
            val decoded = if (v.has("decodedSeq")) v.getLong("decodedSeq") else null
            assertEquals(v.getString("text"), V4.ackMessage(v.getLong("generation"), v.getLong("seq"), v.getDouble("recvTsMs"), decoded))
        }
    }

    @Test
    fun keyframeMatchesMediaV4Js() {
        for (v in Fixtures.objects(vectors.getJSONArray("keyframes"))) {
            val reason = if (v.isNull("reason")) null else v.getString("reason")
            assertEquals(v.getString("text"), V4.keyframeMessage(reason))
        }
    }

    @Test
    fun heartbeatMatchesMediaV4Js() {
        assertEquals(vectors.getString("ping"), V4.PING)
        assertEquals(15_000L, V4.HEARTBEAT_MS)
    }

    @Test
    fun backoffMatchesMediaV4Js() {
        for (v in Fixtures.objects(vectors.getJSONArray("backoff"))) {
            assertEquals("after ${v.getLong("previous")}", v.getLong("next"), V4.nextBackoffMs(v.getLong("previous")))
        }
        var delay = 0L
        val seen = ArrayList<Long>()
        repeat(8) { delay = V4.nextBackoffMs(delay); seen += delay }
        assertEquals(listOf(250L, 500L, 1000L, 2000L, 4000L, 5000L, 5000L, 5000L), seen)
    }

    @Test
    fun receiveGateMatchesMediaV4Js() {
        val gate = V4ReceiveGate()
        for ((i, step) in Fixtures.objects(vectors.getJSONArray("gate")).withIndex()) {
            when {
                step.has("invalidate") -> gate.invalidate(step.getString("invalidate"))
                step.has("reset") -> gate.reset(step.getString("reset"))
                else -> {
                    val frame = V4Frame(step.getBoolean("key"), false, V4.CODEC_HEVC, step.getLong("generation"), step.getLong("seq"), 0, 0, 0, byteArrayOf(1))
                    val verdict = gate.accept(frame)
                    val expected = step.getJSONObject("verdict")
                    assertEquals("step $i decode", expected.getBoolean("decode"), verdict.decode)
                    val reason = if (expected.isNull("requestKeyframe")) null else expected.getString("requestKeyframe")
                    assertEquals("step $i reason", reason, verdict.requestKeyframe)
                }
            }
        }
    }

    @Test
    fun presentationAndStatusPolicy() {
        assertTrue(V4.shouldPresent(0, 0))
        assertTrue(V4.shouldPresent(1, 0))
        assertEquals(false, V4.shouldPresent(3, 20))
        assertTrue(V4.shouldPresent(3, 100))
        assertEquals(V4.Status.RECONNECTING, V4.status(false, true, 0, null, 0))
        assertEquals(V4.Status.STARTING, V4.status(true, false, 0, null, 0))
        assertEquals(V4.Status.PAUSED_STATIC, V4.status(true, false, 3, 0, 1_000))
        assertEquals(V4.Status.CONNECTED, V4.status(true, false, 3, 500, 1_000))
    }
}
