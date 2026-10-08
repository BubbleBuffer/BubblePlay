package de.bubblebuffer.bubbleplay.net

import org.junit.Assert.*
import org.junit.Test

class MediaRecoveryTest {
    private class Reconnect {
        val link = InputLink<Any>()
        val leg = MediaLeg()
        var signalId = link.connecting(Any())
        var socketPresent = false

        fun hello() {
            assertTrue(link.helloOk(signalId, "a".repeat(32)))
            // Session's hello teardown is unconditional, even after mediaDown cleared the socket.
            leg.invalidate()
            socketPresent = false
            leg.open()
            socketPresent = true
            assertTrue(leg.startHost())
            assertFalse(leg.startHost())
        }
        fun signalDown() { assertTrue(link.lost(signalId)) }
        fun mediaDown() { socketPresent = false; link.mediaUp = false }
        fun reconnectSignal() { signalId = link.connecting(Any()) }
    }

    @Test fun signalingDownThenMediaDownStillStartsNewHost() {
        val session = Reconnect()
        session.hello()
        val oldCallback = session.leg.id
        session.signalDown()
        session.mediaDown()
        assertFalse(session.socketPresent)
        session.reconnectSignal()
        session.hello()
        assertNotEquals(oldCallback, session.leg.id)
    }

    @Test fun mediaDownThenSignalingDownStillStartsNewHost() {
        val session = Reconnect()
        session.hello()
        session.mediaDown()
        session.signalDown()
        session.reconnectSignal()
        session.hello()
    }

    @Test fun helloWithNoPriorMediaInvalidatesQueuedRetry() {
        val session = Reconnect()
        val retryId = session.leg.id
        assertFalse(session.socketPresent)
        session.hello()
        assertNotEquals(retryId, session.leg.id)
    }

    @Test fun everyNewLegStartsHostAndNullTeardownInvalidatesTimers() {
        val leg = MediaLeg()
        val old = leg.open()
        assertTrue(leg.startHost())
        assertFalse(leg.startHost())
        // Signaling down then media down: hello must invalidate even a null socket.
        leg.invalidate()
        assertNotEquals(old, leg.id)
        leg.open()
        assertTrue(leg.startHost())
        // Media down then signaling down, as well as hello before old media down.
        val stale = leg.id
        leg.invalidate()
        leg.open()
        assertNotEquals(stale, leg.id)
        assertTrue(leg.startHost())
        // Media-only reconnect also owns a new start/first-frame handshake.
        leg.open()
        assertTrue(leg.startHost())
    }

    @Test fun missingIdrRetriesThenRestartsOnceAndStops() {
        val recovery = SurfaceRecovery()
        recovery.changed(1, true)
        assertEquals(SurfaceRecovery.Action.KEYFRAME, recovery.missed(1))
        assertEquals(SurfaceRecovery.Action.RESTART, recovery.missed(1))
        assertEquals(SurfaceRecovery.Action.EXHAUSTED, recovery.missed(1))
        assertEquals(SurfaceRecovery.Action.NONE, recovery.missed(1))
    }

    @Test fun staleOutputAndTimersCannotSatisfyRestoredSurface() {
        val recovery = SurfaceRecovery()
        recovery.changed(1, true)
        recovery.changed(2, false)
        recovery.changed(3, true)
        assertFalse(recovery.output(1))
        assertEquals(SurfaceRecovery.Action.NONE, recovery.missed(1))
        assertEquals(SurfaceRecovery.Action.KEYFRAME, recovery.missed(3))
        assertTrue(recovery.output(3))
        assertEquals(SurfaceRecovery.Action.NONE, recovery.missed(3))
    }

    @Test fun staticReconnectFrameBeforeAdmissionOpensInputWithoutAnotherFrame() {
        val frame = FirstFramePresentation()
        frame.start("reconnect", mediaLeg = 2, requireAdmission = true)
        assertFalse(frame.observe("reconnect", 2, 42)) // Media wins the signaling race.
        assertFalse(frame.presented)
        assertTrue(frame.admit("reconnect", 42)) // No second decoded frame is needed.
        assertTrue(frame.presented)
        assertFalse(frame.admit("reconnect", 42)) // Input-ready transition happens once.
        assertFalse(frame.observe("reconnect", 2, 42))
    }

    @Test fun earlyPresentationCannotLeakAcrossRequestsOrMediaLegs() {
        val frame = FirstFramePresentation()
        frame.start("old", 1, true)
        assertFalse(frame.observe("old", 1, 42))
        frame.start("fresh", 2, true)
        assertFalse(frame.observe("fresh", 1, 42)) // Late decoder output from old socket.
        assertFalse(frame.admit("old", 42))
        assertFalse(frame.admit("fresh", 42))
        assertFalse(frame.observe("fresh", 2, 41)) // Wrong desktop generation.
        assertTrue(frame.observe("fresh", 2, 42))
        frame.reset()
        assertFalse(frame.admit("fresh", 42))
    }

    @Test fun admissionBeforeFrameAndOptionalAdmissionStillCompleteOnce() {
        val frame = FirstFramePresentation()
        frame.start("admit-first", 1, true)
        assertFalse(frame.admit("admit-first", 7))
        assertTrue(frame.observe("admit-first", 1, 7))
        assertFalse(frame.observe("admit-first", 1, 7))
        frame.start("optional", 2, false)
        assertTrue(frame.observe("optional", 2, 8))
        assertFalse(frame.admit("optional", 8))
        assertTrue(frame.admitted)
    }

    @Test fun closingMediaForRecoveryDiscardsPreAdmissionPresentation() {
        val leg = MediaLeg()
        val frame = FirstFramePresentation()
        leg.open()
        frame.start("recovering", leg.id, true)
        assertFalse(frame.observe("recovering", leg.id, 42))
        // closeMedia invalidates callbacks and resets the handshake before a retry opens.
        leg.invalidate()
        frame.reset()
        assertFalse(frame.admit("recovering", 42))
        assertFalse(frame.presented)
        leg.open()
        frame.start("fresh", leg.id, true)
        assertFalse(frame.admit("fresh", 42))
        assertTrue(frame.observe("fresh", leg.id, 42))
    }

    @Test fun exhaustedSurfaceBlocksEveryLaterFirstFrameRecoveryCycle() {
        val recovery = SurfaceRecovery()
        val leg = MediaLeg()
        val frame = FirstFramePresentation()
        recovery.changed(4, true)
        leg.open()
        assertEquals(SurfaceRecovery.Action.KEYFRAME, recovery.missed(4))
        assertEquals(SurfaceRecovery.Action.RESTART, recovery.missed(4))
        assertTrue(recovery.canRestartMedia())
        leg.invalidate()
        leg.open()
        frame.start("surface-restart", leg.id, true) // Successful media hello.
        assertEquals(SurfaceRecovery.Action.EXHAUSTED, recovery.missed(4))
        val queuedStartupId = leg.id
        leg.invalidate() // Session closes the failed leg and invalidates queued retries.
        val stoppedId = leg.id
        repeat(10) {
            frame.start("presentation-timeout-$it", leg.id, true)
            // Host failures, first-frame timeouts and media retry callbacks use this same gate.
            if (recovery.canRestartMedia()) leg.open()
            assertEquals(stoppedId, leg.id)
        }
        assertNotEquals(queuedStartupId, leg.id)
        assertTrue(recovery.exhausted)
        assertFalse(recovery.output(4)) // Late output does not reopen the retry budget.
        recovery.changed(4, true) // Duplicate attach also cannot reset exhaustion.
        assertFalse(recovery.canRestartMedia())
        recovery.changed(5, false) // Destroying the surface is not a new usable surface.
        assertFalse(recovery.canRestartMedia())
        recovery.changed(6, true)
        assertTrue(recovery.canRestartMedia())
        assertEquals(SurfaceRecovery.Action.KEYFRAME, recovery.missed(6))
        // The existing user Retry action constructs a fresh Session and policy.
        assertTrue(SurfaceRecovery().canRestartMedia())
    }

    @Test fun surfaceAttachedBeforeSessionStartKeepsWatchdogPending() {
        val recovery = SurfaceRecovery()
        recovery.changed(1, true)
        assertEquals(1L, recovery.waitingGeneration())
        assertTrue(recovery.output(1))
        assertNull(recovery.waitingGeneration())
    }
}
