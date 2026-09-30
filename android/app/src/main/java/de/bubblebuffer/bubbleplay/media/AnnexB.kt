package de.bubblebuffer.bubbleplay.media

import de.bubblebuffer.bubbleplay.protocol.V4
import java.io.ByteArrayOutputStream

/** Minimal Annex-B helpers: split NAL units and pull parameter sets for MediaCodec csd. */
object AnnexB {
    /** Returns [start, end) payload ranges of each NAL unit (start codes excluded). */
    fun nalUnits(data: ByteArray): List<IntRange> {
        val starts = ArrayList<Pair<Int, Int>>() // (startCodeIndex, payloadIndex)
        var i = 0
        while (i + 2 < data.size) {
            if (data[i].toInt() == 0 && data[i + 1].toInt() == 0) {
                if (data[i + 2].toInt() == 1) { starts += i to i + 3; i += 3; continue }
                if (i + 3 < data.size && data[i + 2].toInt() == 0 && data[i + 3].toInt() == 1) { starts += i to i + 4; i += 4; continue }
            }
            i++
        }
        return starts.mapIndexed { index, (_, payload) ->
            val end = if (index + 1 < starts.size) starts[index + 1].first else data.size
            payload until end
        }
    }

    fun nalType(codec: Int, firstByte: Byte): Int =
        if (codec == V4.CODEC_HEVC) (firstByte.toInt() shr 1) and 0x3f else firstByte.toInt() and 0x1f

    /**
     * Codec-specific data for MediaFormat: HEVC -> [csd-0 = VPS+SPS+PPS];
     * H.264 -> [csd-0 = SPS, csd-1 = PPS]. Each NAL keeps a 4-byte start code.
     * Empty when the access unit carries no parameter sets.
     */
    fun codecSpecificData(codec: Int, au: ByteArray): List<ByteArray> {
        val hevc = codec == V4.CODEC_HEVC
        val sps = ByteArrayOutputStream()
        val pps = ByteArrayOutputStream()
        for (range in nalUnits(au)) {
            if (range.isEmpty()) continue
            val type = nalType(codec, au[range.first])
            val target = when {
                hevc && (type == 32 || type == 33 || type == 34) -> sps
                !hevc && type == 7 -> sps
                !hevc && type == 8 -> pps
                else -> null
            } ?: continue
            target.write(START_CODE)
            target.write(au, range.first, range.last - range.first + 1)
        }
        if (sps.size() == 0) return emptyList()
        return if (hevc || pps.size() == 0) listOf(sps.toByteArray()) else listOf(sps.toByteArray(), pps.toByteArray())
    }

    private val START_CODE = byteArrayOf(0, 0, 0, 1)
}
