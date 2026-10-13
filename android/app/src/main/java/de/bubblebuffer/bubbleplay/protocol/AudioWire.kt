package de.bubblebuffer.bubbleplay.protocol

import org.json.JSONObject

/**
 * Host sound (`fixtures/audio-contract.json`): one Opus packet per binary
 * message on the media socket, behind the 28-byte BPA1 header. Pure: no Android
 * types, so the contract fixture runs on the JVM.
 *
 * Header, big-endian: magic u32 "BPA1" | version u8 = 1 | flags u8 (bit0
 * discontinuity) | codec u8 (1 Opus) | channels u8 (2) | sampleRate u32 (48000) |
 * seq u32 | captureTsUs u64 | frameSamples u16 (480) | reserved u16 (0).
 */
object AudioWire {
    const val MAGIC = 0x42504131L
    const val VERSION = 1
    const val HEADER_BYTES = 28
    const val MAX_PAYLOAD_BYTES = 1500
    const val FLAG_DISCONTINUITY = 0x01
    const val CODEC_OPUS = 1
    const val CHANNELS = 2
    const val SAMPLE_RATE = 48_000
    const val FRAME_SAMPLES = 480
    /** Opus pre-skip of the decoder's OpusHead, in samples at 48 kHz (6.5 ms). */
    const val PRE_SKIP_SAMPLES = 312
    const val PRE_SKIP_NS = 6_500_000L
    /** Seek pre-roll the Opus decoder expects (80 ms). */
    const val SEEK_PRE_ROLL_NS = 80_000_000L

    /** True when the packet carries the BPA1 magic. The media socket also carries BPV4 video. */
    fun isAudio(packet: ByteArray): Boolean = packet.size >= 4 && u32(packet, 0) == MAGIC

    /** Parses one BPA1 packet; null for anything the contract rejects. */
    fun parse(packet: ByteArray): AudioPacket? {
        if (packet.size <= HEADER_BYTES || packet.size > HEADER_BYTES + MAX_PAYLOAD_BYTES) return null
        if (u32(packet, 0) != MAGIC || u8(packet, 4) != VERSION) return null
        val flags = u8(packet, 5)
        if ((flags and FLAG_DISCONTINUITY.inv()) != 0) return null
        if (u8(packet, 6) != CODEC_OPUS || u8(packet, 7) != CHANNELS) return null
        if (u32(packet, 8) != SAMPLE_RATE.toLong() || u16(packet, 24) != FRAME_SAMPLES || u16(packet, 26) != 0) return null
        return AudioPacket(
            discontinuity = (flags and FLAG_DISCONTINUITY) != 0,
            seq = u32(packet, 12),
            captureTsUs = u64(packet, 16),
            payload = packet.copyOfRange(HEADER_BYTES, packet.size),
        )
    }

    /** Adds `"audio":1` to a protocol-4 `media-control` start message, only when this device can decode Opus. */
    fun optIn(start: JSONObject, decoderAvailable: Boolean): JSONObject =
        if (decoderAvailable) start.put("audio", 1) else start

    /** The OpusHead packet (csd-0): "OpusHead", version 1, 2 channels, pre-skip, 48 kHz, gain 0, mapping family 0. */
    fun opusHead(): ByteArray {
        val out = ByteArray(19)
        "OpusHead".toByteArray(Charsets.US_ASCII).copyInto(out, 0)
        out[8] = 1
        out[9] = CHANNELS.toByte()
        putLe(out, 10, PRE_SKIP_SAMPLES.toLong(), 2)
        putLe(out, 12, SAMPLE_RATE.toLong(), 4)
        // Output gain (16 bits) and mapping family stay 0.
        return out
    }

    /** An 8-byte little-endian long, the form the decoder takes csd-1 and csd-2 in. */
    fun longLittleEndian(value: Long): ByteArray = ByteArray(8).also { putLe(it, 0, value, 8) }

    private fun putLe(b: ByteArray, at: Int, value: Long, bytes: Int) {
        for (i in 0 until bytes) b[at + i] = (value ushr (8 * i)).toByte()
    }

    private fun u8(b: ByteArray, i: Int) = b[i].toInt() and 0xff
    private fun u16(b: ByteArray, i: Int) = (u8(b, i) shl 8) or u8(b, i + 1)
    private fun u32(b: ByteArray, i: Int): Long =
        (u8(b, i).toLong() shl 24) or (u8(b, i + 1).toLong() shl 16) or (u8(b, i + 2).toLong() shl 8) or u8(b, i + 3).toLong()
    private fun u64(b: ByteArray, i: Int): Long = (u32(b, i) shl 32) or u32(b, i + 4)
}

/** One accepted BPA1 packet. [captureTsUs] is the raw u64 bit pattern. */
class AudioPacket(
    val discontinuity: Boolean,
    val seq: Long,
    val captureTsUs: Long,
    val payload: ByteArray,
)
