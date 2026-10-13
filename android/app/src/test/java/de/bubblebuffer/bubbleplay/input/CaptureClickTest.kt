package de.bubblebuffer.bubbleplay.input

import org.junit.Assert.assertEquals
import org.junit.Test

/** Game mode: the click that captures the mouse never reaches the PC. */
class CaptureClickTest {
    private val left = 1
    private val right = 2

    @Test fun theCaptureClickIsHiddenUntilReleased() {
        val c = CaptureClick()
        c.captureRequested(left)
        assertEquals(0, c.filter(left))
        assertEquals(0, c.filter(left))
        assertEquals(0, c.filter(0))
        assertEquals("a later press is an ordinary click", left, c.filter(left))
    }

    @Test fun otherButtonsPassWhileTheCaptureClickIsHeld() {
        val c = CaptureClick()
        c.captureRequested(left)
        assertEquals(right, c.filter(left or right))
        assertEquals(0, c.filter(left))
        assertEquals(right, c.filter(right))
    }

    @Test fun resetForgetsTheSwallowedButtons() {
        val c = CaptureClick()
        c.captureRequested(left or right)
        c.reset()
        assertEquals(left or right, c.filter(left or right))
    }

    @Test fun nothingIsSwallowedWithoutACaptureClick() {
        assertEquals(left, CaptureClick().filter(left))
    }
}
