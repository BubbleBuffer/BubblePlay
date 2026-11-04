package de.bubblebuffer.bubbleplay.net

import org.json.JSONObject
import java.util.Base64

/**
 * Client side of `monitor-previews-v1` (docs/architecture/monitor-previews.md): validates a host
 * `monitor-preview` message and holds the latest picture per monitor selector, in memory only.
 * No Android dependency besides org.json; single-threaded (the session thread owns it).
 */
class MonitorPreviews {
    class Picture(val selector: String, val seq: Long, val width: Int, val height: Int, val jpeg: ByteArray)

    private val held = HashMap<String, Picture>()
    /** Highest accepted seq per selector; survives [take] so a stale message cannot come back. */
    private val lastSeq = HashMap<String, Long>()

    /** Validates [msg] and holds it as the latest picture of its monitor. True when it was held. */
    fun onMessage(msg: JSONObject): Boolean {
        if (msg.optString("type") != TYPE) return false
        if ((msg.opt("v") as? Number)?.toLong() != VERSION) return false
        val selector = msg.opt("monitor") as? String ?: return false
        if (selector.isEmpty() || selector.length > MAX_SELECTOR) return false
        val seq = (msg.opt("seq") as? Number)?.toLong() ?: return false
        if (seq < 0) return false
        val w = (msg.opt("w") as? Number)?.toInt() ?: return false
        val h = (msg.opt("h") as? Number)?.toInt() ?: return false
        if (w < 1 || h < 1 || w > MAX_DIMENSION || h > MAX_DIMENSION) return false
        val text = msg.opt("jpeg") as? String ?: return false
        if (text.isEmpty() || text.length > MAX_JPEG_BASE64) return false
        val jpeg = try { Base64.getDecoder().decode(text) } catch (_: IllegalArgumentException) { return false }
        if (!looksLikeJpeg(jpeg)) return false
        val previous = lastSeq[selector]
        if (previous != null && seq < previous) return false
        lastSeq[selector] = seq
        held[selector] = Picture(selector, seq, w, h, jpeg)
        return true
    }

    /** Removes and returns the held picture of [selector] (null when none is held). */
    fun take(selector: String): Picture? = held.remove(selector)

    /** Forgets the held picture of [selector], keeping its seq. */
    fun drop(selector: String) {
        held.remove(selector)
    }

    fun has(selector: String): Boolean = held.containsKey(selector)

    /** The session ended: drop everything, including the seq state. */
    fun clear() {
        held.clear()
        lastSeq.clear()
    }

    companion object {
        const val CAP = "monitor-previews-v1"
        const val TYPE = "monitor-preview"
        private const val VERSION = 1L
        private const val MAX_SELECTOR = 128
        private const val MAX_DIMENSION = 960
        private const val MAX_JPEG_BYTES = 98304
        // Longest base64 text of MAX_JPEG_BYTES bytes, checked before decoding.
        private const val MAX_JPEG_BASE64 = (MAX_JPEG_BYTES + 2) / 3 * 4

        fun looksLikeJpeg(bytes: ByteArray): Boolean =
            bytes.size in 4..MAX_JPEG_BYTES &&
                bytes[0] == 0xFF.toByte() && bytes[1] == 0xD8.toByte() &&
                bytes[bytes.size - 2] == 0xFF.toByte() && bytes[bytes.size - 1] == 0xD9.toByte()
    }
}
