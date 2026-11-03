package de.bubblebuffer.bubbleplay.flutter

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class LatencyStatsTest {
    private var now = 0L
    private val out = ArrayList<LatencyStats.Summary>()
    private val stats = LatencyStats({ now }, 60_000_000_000L) { out += it }

    @Test
    fun nothingIsLoggedInsideTheWindow() {
        stats.record(5_000_000, 0)
        now = 59_000_000_000L
        stats.record(6_000_000, 0)
        assertTrue(out.isEmpty())
    }

    @Test
    fun windowSummaryHasPercentilesAndDroppedDelta() {
        for (ms in 1..100) stats.record(ms * 1_000_000L, if (ms >= 50) 3 else 0)
        now = 60_000_000_000L
        stats.record(200_000_000L, 3)
        val s = out.single()
        assertEquals(101, s.frames)
        assertEquals(3L, s.dropped)
        assertEquals(200.0, s.maxMs, 0.001)
        assertEquals(51.0, s.p50Ms, 0.001)
        assertEquals(false, s.final)
    }

    @Test
    fun finishFlushesTheRestAndEmptyFinishIsSilent() {
        stats.record(2_000_000, 0)
        stats.finish()
        assertEquals(1, out.size)
        assertEquals(true, out[0].final)
        stats.finish()
        assertEquals(1, out.size)
    }
}
