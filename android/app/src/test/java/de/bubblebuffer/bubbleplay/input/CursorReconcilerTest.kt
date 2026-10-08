package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.protocol.CursorState
import org.junit.Assert.*
import org.junit.Test

class CursorReconcilerTest {
    private fun state(ack: Int = 0, hasAck: Boolean = true, inBounds: Boolean = true) =
        CursorState(ack, 100, 200, hasAck, inBounds, true)

    @Test fun delayedOldFeedbackNeverGainsAuthorityAfter300Milliseconds() {
        val reconciler = CursorReconciler()
        reconciler.noteSent(10)
        // A gesture has ended. Delay is deliberately absent from the contract:
        // the same stale echo is rejected whether it arrives at 301 ms or 30 s.
        repeat(10) { assertFalse(reconciler.apply(state(9), touchActive = false)) }
        assertTrue(reconciler.pending)
        assertTrue(reconciler.apply(state(10)))
        assertFalse(reconciler.pending)
    }

    @Test fun wrapAndHalfRangeUseUnsignedSequenceOrdering() {
        val reconciler = CursorReconciler()
        reconciler.noteSent(65535)
        assertTrue(reconciler.apply(state(65535)))
        reconciler.noteSent(0)
        assertFalse(reconciler.apply(state(65535)))
        assertTrue(reconciler.apply(state(0)))
        assertFalse(reconciler.apply(state(65535)))
        assertFalse(CursorReconciler.newer(0, 32768))
        assertTrue(CursorReconciler.newer(65535, 0))
    }

    @Test fun reconnectResetForgetsPreviousSentAndAcknowledgedEpoch() {
        val reconciler = CursorReconciler()
        reconciler.noteSent(1234)
        reconciler.apply(state(1233))
        reconciler.reset()
        assertNull(reconciler.lastSent)
        assertNull(reconciler.lastAck)
        assertFalse(reconciler.pending)
        assertFalse(reconciler.synced)
        reconciler.noteSent(0)
        assertTrue(reconciler.apply(state(0)))
    }

    @Test fun externalMovementIsAllowedOnlyAfterLocalMovementIsAcknowledged() {
        val reconciler = CursorReconciler()
        assertTrue(reconciler.apply(state(hasAck = false)))
        reconciler.noteSent(12)
        assertFalse(reconciler.apply(state(hasAck = false)))
        assertTrue(reconciler.apply(state(12)))
        assertTrue(reconciler.apply(state(hasAck = false)))
        assertTrue(reconciler.apply(state(12))) // Host movement with unchanged ack.
        assertFalse(reconciler.apply(state(hasAck = false), touchActive = true))
    }

    @Test fun acknowledgedOldSendCannotOverrideMovementStillAwaitingAdmission() {
        val reconciler = CursorReconciler()
        reconciler.noteSent(3)
        assertFalse(reconciler.apply(state(3), localMotionPending = true))
        reconciler.noteSent(4)
        assertFalse(reconciler.apply(state(3)))
        assertTrue(reconciler.apply(state(4)))
    }

    @Test fun outOfBoundsFeedbackAcknowledgesWithoutProvidingCoordinates() {
        val reconciler = CursorReconciler()
        reconciler.noteSent(8)
        assertFalse(reconciler.apply(state(8, inBounds = false)))
        assertFalse(reconciler.pending)
        assertTrue(reconciler.synced)
    }

    @Test fun acknowledgedWheelSequencesAcrossHalfRangeCannotBlockExternalMovement() {
        val reconciler = CursorReconciler()
        reconciler.noteSent(100)
        assertTrue(reconciler.apply(state(100)))
        // Wheel-only sends do not call noteSent, but host feedback preserves
        // their highest ACK. Each observed ACK advances within the wrap window.
        assertTrue(reconciler.apply(state(16_484)))
        assertTrue(reconciler.apply(state(32_868)))
        assertTrue(reconciler.apply(state(49_252)))
        assertTrue(reconciler.apply(state(100))) // Full wrap, still no pending movement.
        assertFalse(reconciler.pending)
        reconciler.noteSent(101)
        assertFalse(reconciler.apply(state(100))) // A real unacknowledged move stays protected.
        assertTrue(reconciler.apply(state(101)))
    }
}
