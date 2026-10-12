package de.bubblebuffer.bubbleplay.input

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PointerWheelTest {
    @Test
    fun mouseWheelIsAPointerSource() {
        assertTrue(PointerWheel.isPointerSource(PointerWheel.SOURCE_MOUSE))
    }

    @Test
    fun touchpadScrollIsAPointerSource() {
        // A touchpad reports SOURCE_TOUCHPAD only, so it must not need the mouse bits.
        assertTrue(PointerWheel.isPointerSource(PointerWheel.SOURCE_TOUCHPAD))
    }

    @Test
    fun touchscreenIsNotAPointerSource() {
        assertFalse(PointerWheel.isPointerSource(0x1002))
    }

    @Test
    fun notchesBecomeWheelDeltaUnits() {
        assertEquals(120f, PointerWheel.units(1f), 0f)
        assertEquals(-240f, PointerWheel.units(-2f), 0f)
        assertEquals(30f, PointerWheel.units(0.25f), 1e-6f)
    }
}
