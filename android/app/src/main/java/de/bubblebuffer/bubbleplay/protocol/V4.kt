package de.bubblebuffer.bubbleplay.protocol

/**
 * WSS media protocol v4 — the Kotlin twin of worker/public/media-v4.js and
 * host/src/wss_v4.rs. The contract is fixtures/wss-v4-contract.json.
 *
 * Host -> client: binary frames with a 28-byte big-endian header followed by
 * one Annex-B access unit:
 *   magic u32 "BPV4" | version u8 = 4 | flags u8 (bit0 key, bit1 refine) |
 *   codec u8 (1 HEVC, 2 H.264) | reserved u8 = 0 | generation u32 | seq u32 |
 *   captureTsUs u64 | width u16 | height u16
 * Client -> host: small JSON text on the same socket (ACK, keyframe, ping).
 */
object V4 {
    const val VERSION = 4
    const val MAGIC = 0x42505634
    const val HEADER_BYTES = 28
    const val MAX_PAYLOAD_BYTES = 8 * 1024 * 1024
    const val MAX_PACKET_BYTES = HEADER_BYTES + MAX_PAYLOAD_BYTES
    const val FLAG_KEY = 0x01
    const val FLAG_REFINE = 0x02
    const val CODEC_HEVC = 1
    const val CODEC_H264 = 2
    const val HEARTBEAT_MS = 15_000L
    const val BACKOFF_MIN_MS = 250L
    const val BACKOFF_MAX_MS = 5_000L
    const val KEYFRAME_REQUEST_INTERVAL_MS = 250L
    const val PING = "{\"type\":\"ping\"}"
    private const val KNOWN_FLAGS = FLAG_KEY or FLAG_REFINE

    /** Parses one v4 packet; null for anything the contract rejects. */
    fun parse(packet: ByteArray): V4Frame? {
        if (packet.size <= HEADER_BYTES || packet.size > MAX_PACKET_BYTES) return null
        val flags = u8(packet, 5)
        val codec = u8(packet, 6)
        if (u32(packet, 0) != MAGIC.toLong() || u8(packet, 4) != VERSION ||
            (flags and KNOWN_FLAGS.inv()) != 0 || (codec != CODEC_HEVC && codec != CODEC_H264) ||
            u8(packet, 7) != 0
        ) return null
        return V4Frame(
            key = (flags and FLAG_KEY) != 0,
            refine = (flags and FLAG_REFINE) != 0,
            codec = codec,
            generation = u32(packet, 8),
            seq = u32(packet, 12),
            captureTsUs = u64(packet, 16),
            width = u16(packet, 24),
            height = u16(packet, 26),
            data = packet.copyOfRange(HEADER_BYTES, packet.size),
        )
    }

    fun encode(frame: V4Frame): ByteArray {
        val payload = frame.data
        require(payload.isNotEmpty() && payload.size <= MAX_PAYLOAD_BYTES) { "v4 payload size out of range" }
        val out = ByteArray(HEADER_BYTES + payload.size)
        put32(out, 0, MAGIC.toLong())
        out[4] = VERSION.toByte()
        out[5] = ((if (frame.key) FLAG_KEY else 0) or (if (frame.refine) FLAG_REFINE else 0)).toByte()
        out[6] = frame.codec.toByte()
        out[7] = 0
        put32(out, 8, frame.generation)
        put32(out, 12, frame.seq)
        for (i in 0 until 8) out[16 + i] = (frame.captureTsUs ushr (56 - 8 * i)).toByte()
        put16(out, 24, frame.width)
        put16(out, 26, frame.height)
        payload.copyInto(out, HEADER_BYTES)
        return out
    }

    /**
     * Per-frame ACK, byte-identical to media-v4.js ackMessageV4:
     * recvTsMs is rounded to 0.1 ms and printed the way JSON.stringify does.
     */
    fun ackMessage(generation: Long, seq: Long, recvTsMs: Double, decodedSeq: Long? = null): String {
        val sb = StringBuilder(96)
        sb.append("{\"type\":\"ack\",\"generation\":").append(generation and 0xffff_ffffL)
        sb.append(",\"seq\":").append(seq and 0xffff_ffffL)
        sb.append(",\"recvTsMs\":").append(tenths(recvTsMs))
        if (decodedSeq != null && decodedSeq in 0..0xffff_ffffL) sb.append(",\"decodedSeq\":").append(decodedSeq)
        return sb.append('}').toString()
    }

    /** Keyframe request, byte-identical to media-v4.js keyframeMessageV4. */
    fun keyframeMessage(reason: String?): String {
        val source = if (reason.isNullOrEmpty()) "receiver" else reason
        val sanitized = buildString(source.length) {
            for (ch in source) append(if (ch.isLetterOrDigitAscii() || ch in "_.:-") ch else '-')
        }.take(64).ifEmpty { "receiver" }
        return "{\"type\":\"keyframe\",\"reason\":\"$sanitized\"}"
    }

    /** Reconnect backoff: 250 ms doubling to 5 s. */
    fun nextBackoffMs(previousMs: Long): Long =
        if (previousMs < BACKOFF_MIN_MS) BACKOFF_MIN_MS else minOf(BACKOFF_MAX_MS, previousMs * 2)

    /**
     * Presentation policy (media-v4.js shouldPresentDecodedV4): every frame is
     * decoded, but while several are still queued the intermediate outputs are
     * not painted. One frame is painted at least every [maxSkipMs].
     */
    fun shouldPresent(decodeQueueSize: Int, msSinceLastPresent: Long, maxQueued: Int = 1, maxSkipMs: Long = 100): Boolean =
        decodeQueueSize <= maxQueued || msSinceLastPresent >= maxSkipMs

    enum class Status { CONNECTING, RECONNECTING, STARTING, PAUSED_STATIC, CONNECTED }

    /** media-v4.js wssV4StatusLabel: a static desktop is never "waiting". */
    fun status(socketOpen: Boolean, reconnecting: Boolean, framesPresented: Long, lastFrameAtMs: Long?, nowMs: Long, staticAfterMs: Long = 1_000): Status {
        if (!socketOpen) return if (reconnecting) Status.RECONNECTING else Status.CONNECTING
        if (framesPresented == 0L) return Status.STARTING
        if (lastFrameAtMs != null && nowMs - lastFrameAtMs >= staticAfterMs) return Status.PAUSED_STATIC
        return Status.CONNECTED
    }

    private fun Char.isLetterOrDigitAscii() = this in 'A'..'Z' || this in 'a'..'z' || this in '0'..'9'

    /** JSON.stringify(Math.max(0, Math.round(x * 10) / 10)) without float formatting surprises. */
    internal fun tenths(value: Double): String {
        if (value.isNaN() || value <= 0.0) return "0"
        val t = Math.round(value * 10.0)
        if (t <= 0L) return "0"
        val whole = t / 10
        val frac = t % 10
        return if (frac == 0L) whole.toString() else "$whole.$frac"
    }

    private fun u8(b: ByteArray, i: Int) = b[i].toInt() and 0xff
    private fun u16(b: ByteArray, i: Int) = (u8(b, i) shl 8) or u8(b, i + 1)
    private fun u32(b: ByteArray, i: Int): Long =
        (u8(b, i).toLong() shl 24) or (u8(b, i + 1).toLong() shl 16) or (u8(b, i + 2).toLong() shl 8) or u8(b, i + 3).toLong()
    private fun u64(b: ByteArray, i: Int): Long = (u32(b, i) shl 32) or u32(b, i + 4)
    private fun put16(b: ByteArray, i: Int, v: Int) { b[i] = (v ushr 8).toByte(); b[i + 1] = v.toByte() }
    private fun put32(b: ByteArray, i: Int, v: Long) {
        b[i] = (v ushr 24).toByte(); b[i + 1] = (v ushr 16).toByte(); b[i + 2] = (v ushr 8).toByte(); b[i + 3] = v.toByte()
    }
}

/** One parsed v4 access unit. [captureTsUs] is the raw u64 bit pattern. */
class V4Frame(
    val key: Boolean,
    val refine: Boolean,
    val codec: Int,
    val generation: Long,
    val seq: Long,
    val captureTsUs: Long,
    val width: Int,
    val height: Int,
    val data: ByteArray,
)

/**
 * Receive-side ordering gate (media-v4.js V4ReceiveGate). Every accepted
 * frame must be decoded in order. A sequence gap, a new generation without a
 * keyframe, or a decoder error drops deltas until the next keyframe and asks
 * for one.
 */
class V4ReceiveGate {
    data class Verdict(val decode: Boolean, val requestKeyframe: String?)

    var generation: Long? = null; private set
    var expectedSeq: Long? = null; private set
    var waitingForKey = true; private set
    private var pendingReason: String? = "connect"

    fun reset(reason: String = "connect") {
        generation = null
        expectedSeq = null
        waitingForKey = true
        pendingReason = reason
    }

    fun invalidate(reason: String) {
        waitingForKey = true
        pendingReason = reason
    }

    fun accept(frame: V4Frame): Verdict {
        if (frame.key) {
            generation = frame.generation
            expectedSeq = (frame.seq + 1) and 0xffff_ffffL
            waitingForKey = false
            pendingReason = null
            return Verdict(true, null)
        }
        val reason = when {
            waitingForKey -> pendingReason ?: "awaiting-keyframe"
            frame.generation != generation -> "generation-change"
            frame.seq != expectedSeq -> "sequence-gap"
            else -> null
        }
        if (reason != null) {
            waitingForKey = true
            pendingReason = reason
            return Verdict(false, reason)
        }
        expectedSeq = (frame.seq + 1) and 0xffff_ffffL
        return Verdict(true, null)
    }
}
