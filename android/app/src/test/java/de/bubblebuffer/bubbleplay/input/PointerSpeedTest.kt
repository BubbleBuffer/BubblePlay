package de.bubblebuffer.bubbleplay.input

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class PointerSpeedTest {
    @Test
    fun defaultLevelKeepsTheOriginalGain() {
        assertEquals(1.6f, PointerSpeed.gain(PointerSpeed.DEFAULT), 1e-6f)
    }

    @Test
    fun eachStepIsAQuarterFasterAndTheRangeIsBounded() {
        for (level in PointerSpeed.MIN until PointerSpeed.MAX) {
            assertEquals(1.25f, PointerSpeed.gain(level + 1) / PointerSpeed.gain(level), 1e-5f)
        }
        assertEquals(0.655f, PointerSpeed.gain(PointerSpeed.MIN), 0.001f)
        assertEquals(3.906f, PointerSpeed.gain(PointerSpeed.MAX), 0.001f)
        assertTrue(PointerSpeed.gain(PointerSpeed.MIN) > 0f)
    }

    @Test
    fun outOfRangeLevelsClamp() {
        assertEquals(PointerSpeed.gain(PointerSpeed.MIN), PointerSpeed.gain(-3), 0f)
        assertEquals(PointerSpeed.gain(PointerSpeed.MAX), PointerSpeed.gain(99), 0f)
        assertEquals(PointerSpeed.MIN, PointerSpeed.clamp(0))
        assertEquals(PointerSpeed.MAX, PointerSpeed.clamp(10))
    }
}
