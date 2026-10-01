package de.bubblebuffer.bubbleplay.protocol

import de.bubblebuffer.bubbleplay.files.ReceiveCheck
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/** The shared contract `fixtures/file-transfer-contract.json` (also tested from Rust and JS). */
class FileContractTest {
    private val fixture: JSONObject by lazy {
        val dir = System.getProperty("bubbleplay.fixtures") ?: "../../fixtures"
        JSONObject(File(dir, "file-transfer-contract.json").readText())
    }

    private fun hex(s: String) = s.chunked(2).map { it.toInt(16).toByte() }.toByteArray()
    private fun hexOf(b: ByteArray) = b.joinToString("") { "%02x".format(it) }

    @Test
    fun headerTypesAndLimits() {
        assertEquals(FileWire.PROTOCOL, fixture.getInt("protocol"))
        assertEquals("BPF1", fixture.getString("magic"))
        assertEquals(FileWire.HEADER, fixture.getInt("headerBytes"))
        val header = fixture.getJSONObject("header")
        assertEquals(4, header.getInt("type")); assertEquals(5, header.getInt("flags")); assertEquals(6, header.getInt("reserved"))
        assertEquals(8, header.getInt("transferId")); assertEquals(12, header.getInt("seq"))
        assertEquals("little-endian", header.getString("byteOrder"))
        val types = fixture.getJSONObject("types")
        assertEquals(FileWire.OFFER, types.getInt("OFFER")); assertEquals(FileWire.ACCEPT, types.getInt("ACCEPT"))
        assertEquals(FileWire.DATA, types.getInt("DATA")); assertEquals(FileWire.ACK, types.getInt("ACK"))
        assertEquals(FileWire.DONE, types.getInt("DONE")); assertEquals(FileWire.CANCEL, types.getInt("CANCEL"))
        assertEquals(FileWire.RESULT, types.getInt("RESULT")); assertEquals(FileWire.REQUEST, types.getInt("REQUEST"))
        val limits = fixture.getJSONObject("limits")
        assertEquals(FileWire.MAX_CHUNK.toLong(), limits.getLong("maxDataBytes"))
        assertEquals(FileWire.MAX_JSON.toLong(), limits.getLong("maxControlPayloadBytes"))
        assertEquals(FileWire.MAX_MESSAGE.toLong(), limits.getLong("maxMessageBytes"))
        assertEquals(FileWire.WINDOW, limits.getLong("windowBytes"))
        assertEquals(FileWire.ACK_BYTES, limits.getLong("ackEveryBytes"))
        assertEquals(FileWire.ACK_MS, limits.getLong("ackEveryMs"))
        assertEquals(FileWire.MAX_FILE, limits.getLong("maxFileBytes"))
        assertEquals(FileWire.MAX_BATCH.toLong(), limits.getLong("maxBatchFiles"))
        assertEquals(FileWire.MAX_NAME_BYTES.toLong(), limits.getLong("maxNameBytes"))
        assertEquals(FileWire.IDLE_CLOSE_MS, limits.getLong("idleCloseMs"))
    }

    @Test
    fun everyValidPacketDecodesAndReencodes() {
        val packets = fixture.getJSONArray("packets")
        val seen = HashSet<Int>()
        for (i in 0 until packets.length()) {
            val case = packets.getJSONObject(i)
            val name = case.getString("name")
            val bytes = hex(case.getString("hex"))
            val parsed = FileWire.parse(bytes)
            assertNull("$name: ${parsed.error}", parsed.error)
            val p = parsed.packet!!
            assertEquals(name, fixture.getJSONObject("types").getInt(case.getString("type")), p.type)
            assertEquals(name, case.getLong("transferId"), p.transferId)
            assertEquals(name, case.getLong("seq"), p.seq)
            assertEquals(name, case.getString("payloadHex"), hexOf(p.payload))
            assertArrayEquals(name, bytes, FileWire.encode(p.type, p.transferId, p.seq, p.payload))
            seen += p.type
            val decoded = case.opt("decoded") as? JSONObject
            when (p.type) {
                FileWire.OFFER -> {
                    val meta = FileWire.parseMeta(p.payload)!!
                    assertEquals(decoded!!.getString("name"), meta.name)
                    assertEquals(decoded.getLong("size"), meta.size)
                    assertEquals(decoded.getString("mime"), meta.mime)
                    assertEquals(decoded.getLong("mtimeMs"), meta.mtimeMs)
                    assertEquals(decoded.getString("purpose"), meta.purpose)
                    assertEquals(decoded.getLong("batchId"), meta.batchId)
                    assertEquals(decoded.getInt("batchIndex"), meta.batchIndex)
                    assertEquals(decoded.getInt("batchCount"), meta.batchCount)
                    // What we send is what the other side decodes.
                    assertEquals(meta, FileWire.parseMeta(meta.toJson().toString().toByteArray()))
                    assertNull(FileWire.parse(FileWire.encodeJson(FileWire.OFFER, p.transferId, meta.toJson())).error)
                }
                FileWire.ACCEPT -> assertNull(decoded)
                FileWire.DATA -> assertEquals(decoded!!.getString("dataHex"), hexOf(p.payload))
                FileWire.ACK -> {
                    assertEquals(decoded!!.getLong("total"), FileWire.ackTotal(p))
                    assertArrayEquals(bytes, FileWire.ack(p.transferId, FileWire.ackTotal(p)))
                }
                FileWire.DONE -> assertEquals(decoded!!.getString("sha256"), hexOf(p.payload))
                FileWire.CANCEL -> assertEquals(decoded!!.getString("reason"), FileWire.parseCancel(p.payload))
                FileWire.RESULT -> {
                    val (ok, error) = FileWire.parseResult(p.payload)!!
                    assertEquals(decoded!!.getBoolean("ok"), ok)
                    assertEquals(decoded.optString("error").ifEmpty { null }, error)
                    // JSON key order is not significant: compare what our encoding decodes to.
                    val ours = FileWire.encodeJson(FileWire.RESULT, p.transferId, FileWire.resultJson(ok, error))
                    assertEquals(ok to error, FileWire.parseResult(FileWire.parse(ours).packet!!.payload))
                }
                FileWire.REQUEST -> {
                    assertEquals(decoded!!.getLong("offerId") to decoded.getLong("id"), FileWire.parseRequest(p.payload))
                    val ours = FileWire.encodeJson(FileWire.REQUEST, p.transferId, FileWire.requestJson(decoded.getLong("offerId"), decoded.getLong("id")))
                    assertEquals(FileWire.parseRequest(p.payload), FileWire.parseRequest(FileWire.parse(ours).packet!!.payload))
                }
            }
        }
        assertEquals("every packet type has a vector", (FileWire.OFFER..FileWire.REQUEST).toSet(), seen)
    }

    @Test
    fun rejectedPacketsGiveTheSharedReason() {
        val rejected = fixture.getJSONArray("rejected")
        for (i in 0 until rejected.length()) {
            val case = rejected.getJSONObject(i)
            val bytes = if (case.has("hex")) hex(case.getString("hex")) else {
                val header = hex(case.getString("headerHex"))
                header + ByteArray(case.getInt("totalBytes") - header.size) { 0xAB.toByte() }
            }
            assertEquals(case.getString("name"), case.getString("reason"), FileWire.parse(bytes).error)
        }
    }

    @Test
    fun receiverSessionsReachTheSharedOutcome() {
        val sessions = fixture.getJSONArray("receiverSessions")
        for (i in 0 until sessions.length()) {
            val case = sessions.getJSONObject(i)
            val name = case.getString("name")
            val packets = case.getJSONArray("packets")
            var check: ReceiveCheck? = null
            var outcome: String? = null
            for (j in 0 until packets.length()) {
                val p = FileWire.parse(hex(packets.getString(j))).packet
                assertNotNull("$name #$j", p)
                when (p!!.type) {
                    FileWire.OFFER -> check = ReceiveCheck(FileWire.parseMeta(p.payload)!!.size)
                    FileWire.DATA -> outcome = check!!.onData(p.seq, p.payload)
                    FileWire.DONE -> outcome = check!!.onDone(p.payload) ?: "ok"
                }
                if (outcome != null) break
            }
            assertEquals(name, case.getString("outcome"), outcome)
        }
    }

    @Test
    fun signalingOffersAndCapability() {
        val offers = fixture.getJSONObject("signaling").getJSONArray("fileOffer")
        for (i in 0 until offers.length()) {
            val case = offers.getJSONObject(i)
            val offer = FileWire.parseOfferMessage(JSONObject(case.getString("text")))
            assertEquals(case.getString("text").take(80), case.getBoolean("accepted"), offer != null)
        }
        val caps = JSONObject(fixture.getJSONObject("signaling").getJSONObject("hostCapabilities").getString("text"))
        assertTrue(caps.optInt("fileTransfer") >= 1)
    }

    @Test
    fun socketHandshakeTexts() {
        val texts = fixture.getJSONObject("sockets").getJSONArray("text")
        val hello = JSONObject(texts.getJSONObject(0).getString("text"))
        assertEquals("hello", hello.getString("type"))
        assertEquals("file-client", hello.getString("role"))
        assertTrue(fixture.getJSONObject("sockets").getString("client").startsWith("/file-ws?epoch="))
    }
}
