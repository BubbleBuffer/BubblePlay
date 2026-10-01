package de.bubblebuffer.bubbleplay.protocol

import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

/** Spec-derived checks (`.superpawers/file-transfer.md`); the shared fixture is in FileContractTest. */
class FileWireTest {
    private fun hex(s: String) = s.chunked(2).map { it.toInt(16).toByte() }.toByteArray()

    @Test
    fun headerLayoutIsLittleEndian() {
        val bytes = FileWire.encode(FileWire.DATA, 0x01020304, 0x0A0B0C0D, byteArrayOf(0x7F))
        assertArrayEquals(hex("42504631" + "03" + "00" + "0000" + "04030201" + "0d0c0b0a" + "7f"), bytes)
        val packet = FileWire.parse(bytes).packet!!
        assertEquals(FileWire.DATA, packet.type)
        assertEquals(0x01020304L, packet.transferId)
        assertEquals(0x0A0B0C0DL, packet.seq)
    }

    @Test
    fun u32FieldsUseTheFullRange() {
        val bytes = FileWire.encode(FileWire.DATA, FileWire.U32_MAX, FileWire.U32_MAX, byteArrayOf(1))
        val packet = FileWire.parse(bytes).packet!!
        assertEquals(FileWire.U32_MAX, packet.transferId)
        assertEquals(FileWire.U32_MAX, packet.seq)
    }

    @Test
    fun everyTypeRoundTrips() {
        val json = """{"reason":"x"}""".toByteArray()
        val packets = listOf(
            FileWire.encodeJson(FileWire.OFFER, 7, meta().toJson()),
            FileWire.encode(FileWire.ACCEPT, 7),
            FileWire.encode(FileWire.DATA, 7, 3, ByteArray(FileWire.MAX_CHUNK)),
            FileWire.ack(7, 262_144),
            FileWire.encode(FileWire.DONE, 7, 0, ByteArray(32) { it.toByte() }),
            FileWire.encode(FileWire.CANCEL, 7, 0, json),
            FileWire.encodeJson(FileWire.RESULT, 7, FileWire.resultJson(false, "hash-mismatch")),
            FileWire.encodeJson(FileWire.REQUEST, 7, FileWire.requestJson(9, 2)),
        )
        for ((i, bytes) in packets.withIndex()) {
            val parsed = FileWire.parse(bytes)
            assertNull("type ${i + 1}: ${parsed.error}", parsed.error)
            assertEquals(i + 1, parsed.packet!!.type)
            assertArrayEquals(bytes, FileWire.encode(parsed.packet!!.type, parsed.packet!!.transferId, parsed.packet!!.seq, parsed.packet!!.payload))
        }
        assertEquals(262_144L, FileWire.ackTotal(FileWire.parse(packets[3]).packet!!))
        assertEquals(meta(), FileWire.parseMeta(FileWire.parse(packets[0]).packet!!.payload))
        assertEquals(false to "hash-mismatch", FileWire.parseResult(FileWire.parse(packets[6]).packet!!.payload))
        assertEquals(9L to 2L, FileWire.parseRequest(FileWire.parse(packets[7]).packet!!.payload))
    }

    @Test
    fun rejectsMalformedPackets() {
        fun reject(bytes: ByteArray, why: String) = assertEquals(why, FileWire.parse(bytes).error)
        val good = FileWire.encode(FileWire.ACCEPT, 1)
        reject(good.copyOf(15), "short")
        reject(good.copyOf().also { it[3] = '2'.code.toByte() }, "magic")
        reject(good.copyOf().also { it[4] = 0 }, "type")
        reject(good.copyOf().also { it[4] = 9 }, "type")
        reject(good.copyOf().also { it[5] = 1 }, "flags")
        reject(good.copyOf().also { it[7] = 1 }, "reserved")
        reject(good.copyOf().also { it[12] = 1 }, "seq") // seq is 0 outside DATA
        reject(good + byteArrayOf(0), "payload")
        reject(FileWire.encode(FileWire.DATA, 1, 0, ByteArray(FileWire.MAX_CHUNK)) + byteArrayOf(0), "too-large")
        reject(FileWire.encode(FileWire.DATA, 1, 0, ByteArray(0)), "payload")
        reject(FileWire.encode(FileWire.ACK, 1, 0, ByteArray(7)), "payload")
        reject(FileWire.encode(FileWire.DONE, 1, 0, ByteArray(31)), "payload")
        reject(FileWire.encode(FileWire.OFFER, 1, 0, "not json".toByteArray()), "json")
        reject(FileWire.encode(FileWire.RESULT, 1, 0, "[]".toByteArray()), "json")
        reject(FileWire.encode(FileWire.CANCEL, 1, 0, byteArrayOf('{'.code.toByte(), 0xC3.toByte(), '}'.code.toByte())), "json")
        reject(FileWire.encode(FileWire.CANCEL, 1, 0, ByteArray(0)), "payload")
        reject(FileWire.encode(FileWire.CANCEL, 1, 0, ByteArray(FileWire.MAX_JSON + 1) { ' '.code.toByte() }), "payload")
        reject(FileWire.encode(FileWire.CANCEL, 1, 0, "{}".toByteArray()), "json")
        reject(FileWire.encode(FileWire.RESULT, 1, 0, """{"ok":1}""".toByteArray()), "json")
        reject(FileWire.encode(FileWire.REQUEST, 1, 0, """{"offerId":-1,"id":1}""".toByteArray()), "json")
        reject(FileWire.encodeJson(FileWire.OFFER, 1, meta().toJson().put("name", "a".repeat(1025))), "json")
        assertNull(FileWire.parse(FileWire.encodeJson(FileWire.OFFER, 1, meta().toJson().put("name", "é".repeat(512)))).error)
    }

    private fun meta() = FileWire.Meta("photo.jpg", 1234, "image/jpeg", 1_700_000_000_000, "save", 42, 0, 2)

    @Test
    fun offerMetaIsValidated() {
        fun parse(edit: JSONObject.() -> Unit) = FileWire.parseMeta(meta().toJson().apply(edit).toString().toByteArray())
        assertNotNull(parse {})
        assertNull(parse { put("name", "") })
        assertNull(parse { put("size", FileWire.MAX_FILE + 1) })
        assertNotNull(parse { put("size", FileWire.MAX_FILE) })
        assertNull(parse { put("size", -1) })
        assertNull(parse { put("size", 1.5) })
        assertNull(parse { put("size", "12") })
        assertNull(parse { put("purpose", "open") })
        assertNull(parse { put("batchCount", 65) })
        assertNull(parse { put("batchIndex", 2) })
        assertNull(parse { remove("mime") })
    }

    @Test
    fun fileOfferMessage() {
        val msg = JSONObject("""{"type":"file-offer","protocol":1,"offerId":5,"source":"clipboard",
            "items":[{"id":1,"name":"a.txt","size":3},{"id":2,"name":"b.png","size":2048}]}""")
        val offer = FileWire.parseOfferMessage(msg)!!
        assertEquals(5L, offer.offerId)
        assertEquals(2, offer.items.size)
        assertEquals(2051L, offer.totalBytes)
        val withdrawn = FileWire.parseOfferMessage(JSONObject("""{"type":"file-offer","protocol":1,"offerId":6,"source":"sendto","items":[]}"""))!!
        assertEquals(0, withdrawn.items.size)
        assertNull(FileWire.parseOfferMessage(JSONObject(msg.toString()).put("protocol", 2)))
        assertNull(FileWire.parseOfferMessage(JSONObject(msg.toString()).put("source", "disk")))
        val dup = JSONObject(msg.toString())
        dup.getJSONArray("items").getJSONObject(1).put("id", 1)
        assertNull(FileWire.parseOfferMessage(dup))
        val many = JSONObject(msg.toString()).put("items", org.json.JSONArray((0..64).map { JSONObject().put("id", it).put("name", "f$it").put("size", 1) }))
        assertNull(FileWire.parseOfferMessage(many))
    }
}
