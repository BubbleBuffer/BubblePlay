package de.bubblebuffer.bubbleplay.net

import org.junit.Assert.*
import org.junit.Test

/** Background/resume behaviour of the reconnect loops and the surface watchdog. */
class ResumeReconnectTest {
    @Test fun backoffGrowsAndResetsOnStart() {
        val loop = ReconnectLoop()
        assertEquals(250L, loop.schedule())
        assertTrue(loop.fire())
        assertEquals(500L, loop.schedule())
        assertTrue(loop.fire())
        repeat(6) { loop.schedule(); loop.fire() }
        assertEquals(5_000L, loop.backoffMs)

        loop.background()
        loop.foreground()
        assertEquals(0L, loop.backoffMs)
        assertEquals("first retry after a return waits the minimum", 250L, loop.schedule())
    }

    @Test fun pendingTimerOnStartRetriesImmediately() {
        val loop = ReconnectLoop()
        repeat(5) { loop.schedule(); loop.fire() }
        assertNotNull(loop.schedule()) // a 5 s timer is posted
        assertTrue("onStart cancels the timer and attempts now", loop.foreground())
        assertFalse(loop.scheduled)
        assertEquals(0L, loop.backoffMs)
    }

    @Test fun noReconnectAttemptsWhileStopped() {
        val loop = ReconnectLoop()
        loop.background()
        assertNull("a drop in the background posts no timer", loop.schedule())
        assertTrue(loop.held)
        assertNull(loop.schedule())
        assertEquals("held retries do not grow the backoff", 0L, loop.backoffMs)
        assertTrue("onStart owes exactly one immediate attempt", loop.foreground())
        assertFalse(loop.held)
        assertFalse("nothing else owed", loop.foreground())
    }

    @Test fun timerFiringAfterOnStopIsHeld() {
        val loop = ReconnectLoop()
        assertEquals(250L, loop.schedule())
        loop.background()
        assertFalse("the posted timer does not connect in the background", loop.fire())
        assertTrue(loop.held)
        assertTrue(loop.foreground())
    }

    @Test fun successClearsBackoffAndHold() {
        val loop = ReconnectLoop()
        loop.schedule(); loop.fire(); loop.schedule(); loop.fire()
        loop.succeeded()
        assertEquals(0L, loop.backoffMs)
        assertFalse(loop.foreground())
    }

    @Test fun cancelForgetsOwedRetry() {
        val loop = ReconnectLoop()
        loop.background()
        loop.schedule()
        loop.cancel()
        assertFalse(loop.foreground())
    }

    @Test fun watchdogDoesNotCountWhileSignalingIsDown() {
        val recovery = SurfaceRecovery()
        recovery.changed(1, attached = true)
        repeat(10) {
            assertEquals(SurfaceRecovery.Action.WAIT, recovery.missed(1, signalingUp = false))
        }
        assertFalse(recovery.exhausted)
        assertTrue(recovery.canRestartMedia())
        assertEquals(SurfaceRecovery.Action.KEYFRAME, recovery.missed(1, signalingUp = true))
        assertEquals(SurfaceRecovery.Action.RESTART, recovery.missed(1, signalingUp = true))
        assertEquals(SurfaceRecovery.Action.EXHAUSTED, recovery.missed(1, signalingUp = true))
    }

    @Test fun signalingDropRestartsTheMissCount() {
        val recovery = SurfaceRecovery()
        recovery.changed(1, attached = true)
        assertEquals(SurfaceRecovery.Action.KEYFRAME, recovery.missed(1, signalingUp = true))
        assertEquals(SurfaceRecovery.Action.RESTART, recovery.missed(1, signalingUp = true))
        assertEquals(SurfaceRecovery.Action.WAIT, recovery.missed(1, signalingUp = false))
        assertEquals("a slow host reconnect does not exhaust recovery", SurfaceRecovery.Action.KEYFRAME, recovery.missed(1, signalingUp = true))
        assertFalse(recovery.exhausted)
    }

    @Test fun watchdogIgnoresOtherGenerationsAndOutputSurfaces() {
        val recovery = SurfaceRecovery()
        recovery.changed(2, attached = true)
        assertEquals(SurfaceRecovery.Action.NONE, recovery.missed(1, signalingUp = false))
        assertTrue(recovery.output(2))
        assertEquals(SurfaceRecovery.Action.NONE, recovery.missed(2, signalingUp = false))
    }
}
