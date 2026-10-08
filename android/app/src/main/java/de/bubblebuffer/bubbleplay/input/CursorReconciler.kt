package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.protocol.CursorState

/** Sequence authority for predicted touch coordinates; no elapsed-time fallback. */
class CursorReconciler {
    var lastSent: Int? = null; private set
    var lastAck: Int? = null; private set
    var pending = false; private set
    var synced = false; private set

    fun reset() { lastSent = null; lastAck = null; pending = false; synced = false }

    fun noteSent(sequence: Int) { lastSent = sequence and 0xffff; pending = true }

    /** Whether this sample may replace the local normalized cursor coordinates. */
    fun apply(state: CursorState, touchActive: Boolean = false, localMotionPending: Boolean = false): Boolean {
        var currentAck = false
        if (state.hasAck) {
            val ack = state.ackSequence and 0xffff
            val previous = lastAck
            // A sample older than feedback already observed cannot gain authority.
            if (previous != null && ack != previous && !newer(previous, ack)) return false
            lastAck = ack
            val sent = lastSent
            currentAck = sent == null || ack == sent || newer(sent, ack)
            if (currentAck) pending = false
        }
        // Wheel sequences keep advancing while lastSent tracks only cursor movement.
        // Once movement is acknowledged, fresh host snapshots remain authoritative
        // even if their retained ACK has moved more than half a range beyond lastSent.
        val authoritative = !localMotionPending && (currentAck || (!touchActive && !pending))
        if (!authoritative) return false
        synced = true
        return state.inBounds
    }

    companion object {
        fun newer(last: Int, next: Int): Boolean {
            val delta = (next - last) and 0xffff
            return delta in 1 until 0x8000
        }
    }
}
