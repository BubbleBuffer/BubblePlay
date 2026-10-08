package de.bubblebuffer.bubbleplay.protocol

import org.json.JSONArray
import org.json.JSONObject
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets

/**
 * File transfer v1 (`docs/architecture/file-transfer.md`): the binary framing of
 * the on-demand file socket (`/file-ws`) and the JSON payloads it carries,
 * plus the `file-offer` signaling message. Pure: no Android types, so the
 * shared contract fixture runs on the JVM.
 *
 * Header (16 bytes): "BPF1", type u8, flags u8 (0), reserved u16 (0),
 * transferId u32 LE, seq u32 LE (DATA: chunk index from 0, otherwise 0).
 */
object FileWire {
    const val PROTOCOL = 1
    const val HEADER = 16
    val MAGIC = byteArrayOf('B'.code.toByte(), 'P'.code.toByte(), 'F'.code.toByte(), '1'.code.toByte())

    const val OFFER = 1
    const val ACCEPT = 2
    const val DATA = 3
    const val ACK = 4
    const val DONE = 5
    const val CANCEL = 6
    const val RESULT = 7
    const val REQUEST = 8

    const val MAX_CHUNK = 65_536
    /** The sender keeps at most this much un-ACKed. */
    const val WINDOW = 1L shl 20
    /** The receiver ACKs at least every this many bytes... */
    const val ACK_BYTES = 256L * 1024
    /** ...or this often while data is arriving. */
    const val ACK_MS = 100L
    const val MAX_FILE = 2L shl 30
    const val MAX_BATCH = 64
    const val IDLE_CLOSE_MS = 60_000L
    /** OFFER, CANCEL, RESULT and REQUEST carry small UTF-8 JSON objects. */
    const val MAX_JSON = 4096
    const val MAX_MESSAGE = HEADER + MAX_CHUNK
    const val SHA256 = 32
    /** UTF-8 bytes of a name in OFFER and file-offer. */
    const val MAX_NAME_BYTES = 1024
    const val MAX_MIME_BYTES = 255
    const val MAX_REASON_BYTES = 256
    const val U32_MAX = 0xFFFF_FFFFL

    const val PURPOSE_SAVE = "save"
    const val PURPOSE_PASTE = "paste"

    class Packet(val type: Int, val transferId: Long, val seq: Long, val payload: ByteArray) {
        override fun toString() = "Packet(type=$type, transferId=$transferId, seq=$seq, ${payload.size} bytes)"
    }

    /** A parsed packet, or why it was rejected. */
    class Parsed(val packet: Packet?, val error: String?)

    fun encode(type: Int, transferId: Long, seq: Long = 0, payload: ByteArray = ByteArray(0)): ByteArray {
        require(type in OFFER..REQUEST) { "type $type" }
        require(transferId in 0..U32_MAX && seq in 0..U32_MAX) { "u32 field out of range" }
        val buffer = ByteBuffer.allocate(HEADER + payload.size).order(ByteOrder.LITTLE_ENDIAN)
        buffer.put(MAGIC).put(type.toByte()).put(0).putShort(0)
        buffer.putInt(transferId.toInt()).putInt(seq.toInt())
        buffer.put(payload)
        return buffer.array()
    }

    fun encodeJson(type: Int, transferId: Long, json: JSONObject) = encode(type, transferId, 0, json.toString().toByteArray(StandardCharsets.UTF_8))

    fun ack(transferId: Long, total: Long): ByteArray =
        encode(ACK, transferId, 0, ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(total).array())

    fun parse(bytes: ByteArray): Parsed {
        fun bad(why: String) = Parsed(null, why)
        if (bytes.size < HEADER) return bad("short")
        if (bytes.size > MAX_MESSAGE) return bad("too-large")
        for (i in 0 until 4) if (bytes[i] != MAGIC[i]) return bad("magic")
        val b = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
        val type = bytes[4].toInt() and 0xFF
        if (type !in OFFER..REQUEST) return bad("type")
        if (bytes[5].toInt() != 0) return bad("flags")
        if (b.getShort(6).toInt() != 0) return bad("reserved")
        val transferId = b.getInt(8).toLong() and U32_MAX
        val seq = b.getInt(12).toLong() and U32_MAX
        val payload = bytes.copyOfRange(HEADER, bytes.size)
        if (type != DATA && seq != 0L) return bad("seq")
        when (type) {
            ACCEPT -> if (payload.isNotEmpty()) return bad("payload")
            DATA -> if (payload.isEmpty()) return bad("payload")
            ACK -> if (payload.size != 8) return bad("payload")
            DONE -> if (payload.size != SHA256) return bad("payload")
            else -> {
                if (payload.isEmpty() || payload.size > MAX_JSON) return bad("payload")
                val valid = when (type) {
                    OFFER -> parseMeta(payload) != null
                    CANCEL -> parseCancel(payload) != null
                    RESULT -> parseResult(payload) != null
                    else -> parseRequest(payload) != null
                }
                if (!valid) return bad("json")
            }
        }
        return Parsed(Packet(type, transferId, seq, payload), null)
    }

    /** The ACK's running total (u64; anything past 2^63 reads as negative and fails the window check). */
    fun ackTotal(packet: Packet): Long = ByteBuffer.wrap(packet.payload).order(ByteOrder.LITTLE_ENDIAN).getLong(0)

    private fun utf8Bytes(s: String) = s.toByteArray(StandardCharsets.UTF_8).size

    /** Names: non-empty, at most [MAX_NAME_BYTES] UTF-8 bytes. */
    private fun validName(value: Any?): String? = (value as? String)?.takeIf { it.isNotEmpty() && utf8Bytes(it) <= MAX_NAME_BYTES }

    /** Strict UTF-8 JSON object, or null. */
    fun jsonObject(payload: ByteArray): JSONObject? {
        val text = try {
            StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(payload)).toString()
        } catch (_: CharacterCodingException) { return null }
        return try { JSONObject(text) } catch (_: Exception) { null }
    }

    // ---- OFFER -------------------------------------------------------------------

    data class Meta(
        val name: String,
        val size: Long,
        val mime: String,
        val mtimeMs: Long,
        val purpose: String,
        val batchId: Long,
        val batchIndex: Int,
        val batchCount: Int,
    ) {
        fun toJson(): JSONObject = JSONObject()
            .put("name", name).put("size", size).put("mime", mime).put("mtimeMs", mtimeMs)
            .put("purpose", purpose).put("batchId", batchId).put("batchIndex", batchIndex).put("batchCount", batchCount)
    }

    fun parseMeta(payload: ByteArray): Meta? {
        val o = jsonObject(payload) ?: return null
        val name = validName(o.opt("name")) ?: return null
        val size = integer(o, "size")?.takeIf { it in 0..MAX_FILE } ?: return null
        val mime = (o.opt("mime") as? String)?.takeIf { utf8Bytes(it) <= MAX_MIME_BYTES } ?: return null
        val mtime = integer(o, "mtimeMs")?.takeIf { it >= 0 } ?: return null
        val purpose = (o.opt("purpose") as? String)?.takeIf { it == PURPOSE_SAVE || it == PURPOSE_PASTE } ?: return null
        val batchId = integer(o, "batchId")?.takeIf { it in 0..U32_MAX } ?: return null
        val count = integer(o, "batchCount")?.takeIf { it in 1..MAX_BATCH.toLong() } ?: return null
        val index = integer(o, "batchIndex")?.takeIf { it in 0 until count } ?: return null
        return Meta(name, size, mime, mtime, purpose, batchId, index.toInt(), count.toInt())
    }

    // ---- REQUEST / CANCEL / RESULT ---------------------------------------------------

    fun requestJson(offerId: Long, id: Long): JSONObject = JSONObject().put("offerId", offerId).put("id", id)

    fun parseRequest(payload: ByteArray): Pair<Long, Long>? {
        val o = jsonObject(payload) ?: return null
        val offerId = integer(o, "offerId")?.takeIf { it in 0..U32_MAX } ?: return null
        val id = integer(o, "id")?.takeIf { it in 0..U32_MAX } ?: return null
        return offerId to id
    }

    fun cancelJson(reason: String): JSONObject = JSONObject().put("reason", reason)

    fun parseCancel(payload: ByteArray): String? =
        (jsonObject(payload)?.opt("reason") as? String)?.takeIf { utf8Bytes(it) <= MAX_REASON_BYTES }

    fun resultJson(ok: Boolean, error: String? = null): JSONObject =
        JSONObject().put("ok", ok).apply { if (error != null) put("error", error) }

    /** (ok, error) or null when malformed. */
    fun parseResult(payload: ByteArray): Pair<Boolean, String?>? {
        val o = jsonObject(payload) ?: return null
        val ok = o.opt("ok") as? Boolean ?: return null
        val error = when (val e = o.opt("error")) {
            null, JSONObject.NULL -> null
            is String -> e.takeIf { utf8Bytes(it) <= MAX_REASON_BYTES } ?: return null
            else -> return null
        }
        return ok to error
    }

    // ---- signaling: file-offer ----------------------------------------------------------

    data class Item(val id: Long, val name: String, val size: Long)

    /** A host offer; empty [items] withdraws the previous one. */
    data class Offer(val offerId: Long, val items: List<Item>, val source: String) {
        val totalBytes: Long get() = items.sumOf { it.size }
    }

    fun parseOfferMessage(msg: JSONObject): Offer? {
        if (msg.optString("type") != "file-offer") return null
        if (integer(msg, "protocol") != PROTOCOL.toLong()) return null
        val offerId = integer(msg, "offerId")?.takeIf { it in 0..U32_MAX } ?: return null
        val source = (msg.opt("source") as? String)?.takeIf { it == "clipboard" || it == "sendto" } ?: return null
        val array = msg.opt("items") as? JSONArray ?: return null
        if (array.length() > MAX_BATCH) return null
        val items = ArrayList<Item>()
        val ids = HashSet<Long>()
        for (i in 0 until array.length()) {
            val item = array.opt(i) as? JSONObject ?: return null
            val id = integer(item, "id")?.takeIf { it in 0..U32_MAX && ids.add(it) } ?: return null
            val name = validName(item.opt("name")) ?: return null
            val size = integer(item, "size")?.takeIf { it in 0..MAX_FILE } ?: return null
            items += Item(id, name, size)
        }
        return Offer(offerId, items, source)
    }

    /** An integral JSON number (no fractions, no strings), or null. */
    private fun integer(o: JSONObject, key: String): Long? = when (val v = o.opt(key)) {
        is Int -> v.toLong()
        is Long -> v
        is Double -> if (v == Math.floor(v) && !v.isInfinite() && Math.abs(v) <= 9.007199254740991E15) v.toLong() else null
        is java.math.BigInteger -> if (v.bitLength() < 63) v.toLong() else null
        is java.math.BigDecimal -> try { v.longValueExact() } catch (_: ArithmeticException) { null }
        else -> null
    }
}
