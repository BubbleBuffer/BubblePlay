package de.bubblebuffer.bubbleplay.ui

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Background survival: the service state the stream screen relies on. */
class KeepAliveTest {
    private class Recorder : KeepAlive.Owner {
        var disconnects = 0
        var ended = 0
        override fun onKeepAliveDisconnect() { disconnects++ }
        override fun onKeepAliveEnded() { ended++ }
    }

    private val owner = Recorder().also { KeepAlive.owner = it }

    @After fun tearDown() {
        KeepAlive.stopped()
        KeepAlive.owner = null
    }

    @Test fun activeFromRequestUntilStopped() {
        KeepAlive.requested()
        assertTrue(KeepAlive.active)
        KeepAlive.started()
        assertTrue(KeepAlive.active)
        KeepAlive.stopped()
        assertFalse(KeepAlive.active)
        assertEquals("a requested stop is not an end", 0, owner.ended)
    }

    @Test fun refusalOrTimeoutTellsTheOwnerOnce() {
        KeepAlive.requested()
        KeepAlive.ended()
        KeepAlive.ended()
        assertFalse(KeepAlive.active)
        assertEquals(1, owner.ended)
    }

    @Test fun anEndAfterAStopIsIgnored() {
        KeepAlive.requested()
        KeepAlive.stopped()
        KeepAlive.ended()
        assertEquals(0, owner.ended)
    }

    @Test fun disconnectFromTheNotification() {
        KeepAlive.requested()
        KeepAlive.disconnectRequested()
        assertFalse(KeepAlive.active)
        assertEquals(1, owner.disconnects)
        assertEquals(0, owner.ended)
    }
}
