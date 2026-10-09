package de.bubblebuffer.bubbleplay.ui

import org.junit.Assert.*
import org.junit.Test

class FrozenFrameTest {
    @Test fun shownOnResumeWhenSnapshotExistsAndClearedOnFirstFrame() {
        val frame = FrozenFrame<String>()
        val token = frame.captureStarted()!!
        assertFalse("still visible app: nothing over the live video", frame.captured(token, "desktop"))
        assertTrue("surface lost in the background: overlay up for the return", frame.surfaceLost())
        assertTrue(frame.showing)
        assertEquals("desktop", frame.snapshot)
        assertTrue("first presented frame fades it out", frame.livePresented())
        assertFalse(frame.showing)
        assertFalse("later frames do nothing", frame.livePresented())
    }

    @Test fun captureFinishingAfterSurfaceLossShowsIt() {
        val frame = FrozenFrame<String>()
        val token = frame.captureStarted()!!
        assertFalse(frame.surfaceLost())
        assertTrue(frame.captured(token, "desktop"))
    }

    @Test fun failedCaptureFallsBackToBlack() {
        val frame = FrozenFrame<String>()
        frame.captured(frame.captureStarted()!!, "old")
        frame.livePresented()
        val token = frame.captureStarted()!!
        assertFalse(frame.captured(token, null))
        assertFalse("no stale frame from an earlier pause", frame.surfaceLost())
        assertNull(frame.snapshot)
    }

    @Test fun keptAcrossRepeatedPausesUntilALiveFrame() {
        val frame = FrozenFrame<String>()
        frame.captured(frame.captureStarted()!!, "desktop")
        frame.surfaceLost()
        assertNull("the empty surface is not captured over the snapshot", frame.captureStarted())
        assertTrue(frame.surfaceLost())
        assertEquals("desktop", frame.snapshot)
    }

    @Test fun keptAcrossSessionStopDroppedOnDisconnect() {
        val frame = FrozenFrame<String>()
        frame.captured(frame.captureStarted()!!, "desktop")
        frame.surfaceLost()
        // The 30 s background stop ends the Session, not the frame.
        assertTrue(frame.showing)
        frame.clear()
        assertFalse(frame.showing)
        assertNull(frame.snapshot)
    }

    @Test fun lateCaptureAfterClearIsIgnored() {
        val frame = FrozenFrame<String>()
        val token = frame.captureStarted()!!
        frame.clear()
        frame.surfaceLost()
        assertFalse(frame.captured(token, "desktop"))
        assertNull(frame.snapshot)
    }
}
