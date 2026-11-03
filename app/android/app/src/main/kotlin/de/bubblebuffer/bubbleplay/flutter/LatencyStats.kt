package de.bubblebuffer.bubbleplay.flutter

/**
 * Decode-to-present latency of the stream, kept as per-window samples. One summary line per window
 * (60 s while frames flow) and one at the end of the session; nothing is logged per frame.
 */
internal class LatencyStats(
    private val nowNs: () -> Long = System::nanoTime,
    private val windowNs: Long = 60_000_000_000L,
    private val emit: (Summary) -> Unit,
) {
    data class Summary(
        val p50Ms: Double,
        val p95Ms: Double,
        val maxMs: Double,
        val frames: Int,
        val dropped: Long,
        val final: Boolean,
    )

    private val samples = ArrayList<Long>()
    private var windowStartNs = nowNs()
    private var dropped = 0L
    private var lastSkipped = 0L

    /** One presented frame; [skippedTotal] is the decoder's running count of frames it dropped. */
    @Synchronized
    fun record(latencyNs: Long, skippedTotal: Long) {
        samples += latencyNs
        dropped += (skippedTotal - lastSkipped).coerceAtLeast(0)
        lastSkipped = skippedTotal
        if (nowNs() - windowStartNs >= windowNs) flush(false)
    }

    /** The session ended: log what is left. */
    @Synchronized
    fun finish() = flush(true)

    private fun flush(final: Boolean) {
        windowStartNs = nowNs()
        if (samples.isEmpty() && dropped == 0L) return
        val sorted = samples.sorted()
        val summary = Summary(
            p50Ms = percentile(sorted, 0.50),
            p95Ms = percentile(sorted, 0.95),
            maxMs = (sorted.lastOrNull() ?: 0L) / 1e6,
            frames = sorted.size,
            dropped = dropped,
            final = final,
        )
        samples.clear()
        dropped = 0
        emit(summary)
    }

    private fun percentile(sorted: List<Long>, q: Double): Double {
        if (sorted.isEmpty()) return 0.0
        val index = ((sorted.size - 1) * q).let { Math.ceil(it).toInt() }
        return sorted[index] / 1e6
    }
}
