package de.bubblebuffer.bubbleplay.media

/**
 * Decision logic of the host-sound receiver, free of Android types so it runs
 * in JVM tests. Low latency with no A/V sync: audio never delays video.
 */
object AudioPolicy {
    private const val U32 = 0xffff_ffffL
    const val SAMPLE_RATE = 48_000

    /**
     * Frames queued in the AudioTrack: written minus played. playbackHeadPosition
     * is an unsigned 32-bit counter that wraps, so the difference is taken modulo 2^32.
     */
    fun queuedFrames(writtenFrames: Long, playbackHead: Int): Long =
        (writtenFrames - (playbackHead.toLong() and U32)) and U32

    fun framesToMs(frames: Long): Long = frames * 1000 / SAMPLE_RATE
}

/**
 * Drop policy: once more than [highMs] is queued, incoming packets are dropped
 * until the queue is back under [lowMs].
 */
class AudioDropPolicy(private val highMs: Long = 150, private val lowMs: Long = 60) {
    var dropping = false
        private set

    /** True when the packet arriving with [queuedMs] queued must be dropped. */
    fun shouldDrop(queuedMs: Long): Boolean {
        dropping = if (dropping) queuedMs >= lowMs else queuedMs > highMs
        return dropping
    }

    fun reset() {
        dropping = false
    }
}

/** Tracks the BPA1 sequence. The u32 wrap 4294967295 -> 0 is not a gap. */
class AudioSequence {
    private var last = -1L

    /** True when [seq] does not follow the previous packet. The first packet is never a gap. */
    fun observe(seq: Long): Boolean {
        val gap = last >= 0 && seq != ((last + 1) and 0xffff_ffffL)
        last = seq
        return gap
    }

    fun reset() {
        last = -1L
    }
}
