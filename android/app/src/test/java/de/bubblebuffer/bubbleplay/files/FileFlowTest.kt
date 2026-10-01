package de.bubblebuffer.bubbleplay.files

import de.bubblebuffer.bubbleplay.protocol.FileWire
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.security.MessageDigest

class FileFlowTest {
    private val chunk = FileWire.MAX_CHUNK

    @Test
    fun windowHoldsAtMostOneMebibyteUnacked() {
        val w = SendWindow()
        var chunks = 0
        while (w.canSend(chunk)) { w.onSent(chunk); chunks++ }
        assertEquals(16, chunks)
        assertEquals(FileWire.WINDOW, w.inFlight)
        assertEquals(0L, w.room())
        assertTrue(w.onAck(256L * 1024))
        assertEquals(256L * 1024, w.room())
        assertTrue(w.canSend(chunk))
        // A smaller tail chunk fits once there is room for it.
        assertTrue(w.canSend(10))
    }

    @Test
    fun windowRejectsBadAcks() {
        val w = SendWindow()
        w.onSent(chunk); w.onSent(chunk)
        assertTrue(w.onAck(chunk.toLong()))
        assertFalse("backwards", w.onAck(chunk - 1L))
        assertFalse("more than sent", w.onAck(3L * chunk))
        assertTrue("repeat is fine", w.onAck(chunk.toLong()))
        assertTrue(w.onAck(2L * chunk))
        assertEquals(0L, w.inFlight)
    }

    @Test(expected = IllegalArgumentException::class)
    fun windowRefusesToOverflow() {
        val w = SendWindow(limit = 100)
        w.onSent(101)
    }

    @Test
    fun receiverAcksEvery256KiB() {
        val a = AckSchedule()
        var acks = 0
        for (i in 0 until 16) if (a.onData(chunk, now = 0)) { a.acked(); acks++ }
        assertEquals(4, acks) // 1 MiB in 64 KiB chunks at one instant: every 4th chunk
        assertEquals(FileWire.WINDOW, a.lastAcked)
    }

    @Test
    fun receiverAcksAfter100msOfQuietData() {
        val a = AckSchedule()
        assertFalse(a.onData(1000, now = 1000))
        assertEquals(1100L, a.dueAt())
        assertFalse(a.due(1099))
        assertTrue(a.due(1100))
        assertEquals(1000L, a.acked())
        assertEquals(-1L, a.dueAt())
        assertFalse(a.due(5000))
        // Slow trickle: data arriving 100 ms after the first unacked byte forces an ACK.
        assertFalse(a.onData(10, now = 2000))
        assertTrue(a.onData(10, now = 2100))
    }

    @Test
    fun windowAndAcksTogetherNeverStall() {
        // Sender with a 1 MiB window against a receiver ACKing per its rules: a 5 MiB file goes through.
        val size = 5L shl 20
        val w = SendWindow()
        val a = AckSchedule()
        var now = 0L
        while (w.sent < size) {
            var sentThisRound = false
            while (w.sent < size && w.canSend(chunk)) {
                w.onSent(chunk); sentThisRound = true
                if (a.onData(chunk, now)) assertTrue(w.onAck(a.acked()))
            }
            now += 100
            if (a.due(now)) assertTrue(w.onAck(a.acked()))
            assertTrue("stalled at ${w.sent}", sentThisRound || w.room() > 0)
        }
        assertEquals(size, w.sent)
    }

    private fun sha(vararg parts: ByteArray) = MessageDigest.getInstance("SHA-256").apply { parts.forEach { update(it) } }.digest()

    @Test
    fun receiveCheckAcceptsAnIntactFile() {
        val a = ByteArray(10) { 1 }
        val b = ByteArray(5) { 2 }
        val c = ReceiveCheck(15)
        assertNull(c.onData(0, a))
        assertNull(c.onData(1, b))
        assertNull(c.onDone(sha(a, b)))
    }

    @Test
    fun receiveCheckCatchesHashSizeAndOrderErrors() {
        val a = ByteArray(10) { 1 }
        ReceiveCheck(10).apply { assertNull(onData(0, a)); assertEquals(ReceiveCheck.ERR_HASH, onDone(ByteArray(32))) }
        ReceiveCheck(11).apply { assertNull(onData(0, a)); assertEquals(ReceiveCheck.ERR_SIZE, onDone(sha(a))) }
        ReceiveCheck(9).apply { assertEquals(ReceiveCheck.ERR_SIZE, onData(0, a)) }
        ReceiveCheck(20).apply { assertEquals(ReceiveCheck.ERR_SEQUENCE, onData(1, a)) }
        ReceiveCheck(20).apply { assertNull(onData(0, a)); assertEquals(ReceiveCheck.ERR_SEQUENCE, onData(0, a)) }
        ReceiveCheck(0).apply { assertNull(onDone(sha())) }
    }
}
