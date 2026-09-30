package de.bubblebuffer.bubbleplay.protocol

/**
 * Browser -> host input packets (binary, little-endian), identical to
 * worker/public/input.js `packet` and host/src/input.rs. The contract is
 * fixtures/input-contract.json. They travel as binary frames on the
 * authenticated signaling socket (/ws) while media uses WSS v4.
 */
object InputPackets {
    const val BUTTON_LEFT = 1
    const val BUTTON_RIGHT = 2
    const val BUTTON_MIDDLE = 4
    const val BUTTON_X1 = 8
    const val BUTTON_X2 = 16
    const val TEXT_MAX_BYTES = 512

    fun key(vk: Int, up: Boolean) = byteArrayOf(if (up) 1 else 0, vk.toByte(), (vk ushr 8).toByte())
    fun buttons(mask: Int) = byteArrayOf(4, mask.toByte())
    fun releaseAll() = byteArrayOf(6)

    /** Unicode text (type 11); null when empty or over 512 UTF-8 bytes. */
    fun text(value: String): ByteArray? {
        val bytes = value.toByteArray(Charsets.UTF_8)
        if (bytes.isEmpty() || bytes.size > TEXT_MAX_BYTES) return null
        return byteArrayOf(11) + bytes
    }

    fun motion(seq: Int, dx: Int, dy: Int, wheelX: Int = 0, wheelY: Int = 0): ByteArray {
        val d = ByteArray(11)
        d[0] = 7; le16(d, 1, seq); le16(d, 3, i16(dx)); le16(d, 5, i16(dy)); le16(d, 7, i16(wheelX)); le16(d, 9, i16(wheelY))
        return d
    }

    fun absolute(seq: Int, x: Int, y: Int): ByteArray {
        val d = ByteArray(7)
        d[0] = 8; le16(d, 1, seq); le16(d, 3, x); le16(d, 5, y)
        return d
    }

    fun buttonsAt(seq: Int, x: Int, y: Int, mask: Int): ByteArray {
        val d = ByteArray(8)
        d[0] = 9; le16(d, 1, seq); le16(d, 3, x); le16(d, 5, y); d[7] = mask.toByte()
        return d
    }

    fun cursorSync(seq: Int) = byteArrayOf(10, seq.toByte(), (seq ushr 8).toByte())

    /** Physical key: set-1 scan code (0 = use the VK fallback) plus the E0 flag. */
    fun scanKey(scan: Int, extended: Boolean, vk: Int, up: Boolean) = byteArrayOf(
        13, ((if (up) 1 else 0) or (if (extended) 2 else 0)).toByte(),
        scan.toByte(), (scan ushr 8).toByte(), vk.toByte(), (vk ushr 8).toByte(),
    )

    fun clipboardChunk(id: Int, index: Int, count: Int, bytes: ByteArray, paste: Boolean): ByteArray {
        val out = ByteArray(8 + bytes.size)
        out[0] = 12; out[1] = if (paste) 1 else 0
        le16(out, 2, id); le16(out, 4, index); le16(out, 6, count)
        bytes.copyInto(out, 8)
        return out
    }

    private fun i16(v: Int) = v.coerceIn(-32768, 32767)
    private fun le16(b: ByteArray, i: Int, v: Int) { b[i] = v.toByte(); b[i + 1] = (v ushr 8).toByte() }
}

/** Text clipboard in both directions (input.js CLIPBOARD). */
object Clipboard {
    const val MAX_BYTES = 1024 * 1024
    const val CHUNK_BYTES = 8 * 1024
    const val MAX_CHUNKS = 128
    const val HOST_MAX_CHUNKS = 64

    /** Splits UTF-8 text into type-12 packets, or null if empty / over 1 MiB. */
    fun packets(text: String, id: Int, paste: Boolean = true): List<ByteArray>? {
        val bytes = text.toByteArray(Charsets.UTF_8)
        if (bytes.isEmpty() || bytes.size > MAX_BYTES) return null
        val count = (bytes.size + CHUNK_BYTES - 1) / CHUNK_BYTES
        return (0 until count).map { index ->
            val chunk = bytes.copyOfRange(index * CHUNK_BYTES, minOf(bytes.size, (index + 1) * CHUNK_BYTES))
            InputPackets.clipboardChunk(id and 0xffff, index, count, chunk, paste)
        }
    }
}

/** Reassembles host `clipboard` JSON chunks (input.js ClipboardReceiver). */
class ClipboardReceiver {
    private var id: Long? = null
    private var next = 0
    private var count = 0
    private val parts = ArrayList<String>()
    private var chars = 0

    fun reset() { id = null; next = 0; count = 0; parts.clear(); chars = 0 }

    /** Returns the complete text once the last chunk arrives, else null. */
    fun push(protocol: Int?, id: Long?, index: Int?, count: Int?, text: String?): String? {
        if (protocol != 1 || id == null || index == null || count == null || text == null ||
            count < 1 || count > Clipboard.HOST_MAX_CHUNKS || index < 0 || index >= count
        ) { reset(); return null }
        if (index == 0) { reset(); this.id = id; this.count = count }
        else if (id != this.id || count != this.count || index != next) { reset(); return null }
        parts.add(text); next = index + 1; chars += text.length
        if (chars > Clipboard.MAX_BYTES) { reset(); return null }
        if (next < this.count) return null
        val complete = parts.joinToString("")
        reset()
        return complete
    }
}

/** Host -> client cursor state (type 128): u16 ack, u16 x, u16 y, u8 flags. */
data class CursorState(val ackSequence: Int, val x: Int, val y: Int, val hasAck: Boolean, val inBounds: Boolean, val visible: Boolean) {
    companion object {
        fun decode(bytes: ByteArray): CursorState? {
            if (bytes.size != 8 || (bytes[0].toInt() and 0xff) != 128) return null
            fun u16(i: Int) = (bytes[i].toInt() and 0xff) or ((bytes[i + 1].toInt() and 0xff) shl 8)
            val flags = bytes[7].toInt() and 0xff
            return CursorState(u16(1), u16(3), u16(5), flags and 1 != 0, flags and 2 != 0, flags and 4 != 0)
        }
    }
}
