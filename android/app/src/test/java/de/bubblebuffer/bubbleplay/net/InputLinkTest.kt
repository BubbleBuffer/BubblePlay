package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.net.SignalLiveness.Action
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class InputLinkTest {
    private class Sock(val name: String)

    private val epochA = "a".repeat(32)
    private val epochB = "b".repeat(32)

    @Test
    fun inputIsSuppressedUntilAuthenticationAndFirstFrame() {
        val link = InputLink<Sock>()
        val s = Sock("s1")
        val gen = link.connecting(s)
        assertNull(link.inputSocket()) // socket open, no hello yet
        assertFalse(link.presented(gen)) // a frame before hello does not open input
        assertNull(link.inputSocket())
        assertTrue(link.authenticated(gen, epochA))
        assertNull(link.inputSocket()) // hello done, first frame not presented yet
        assertTrue(link.presented(gen))
        assertSame(s, link.inputSocket())
        link.awaitFrame() // desktop transition
        assertNull(link.inputSocket())
    }

    @Test
    fun mediaUpSignalingDownAsksForReconnectAndDropsInput() {
        val link = InputLink<Sock>()
        val gen = link.connecting(Sock("s1"))
        link.authenticated(gen, epochA); link.presented(gen)
        link.mediaUp = true
        assertFalse(link.inputReconnecting)
        assertTrue(link.lost(gen)) // current socket lost: caller releases input and reconnects
        assertNull(link.inputSocket())
        assertTrue(link.inputReconnecting) // video still flows, input does not
        assertFalse(link.lost(gen)) // a second close callback is not a second reconnect
        link.mediaUp = false
        assertFalse(link.inputReconnecting)
    }

    @Test
    fun staleSocketIsIgnored() {
        val link = InputLink<Sock>()
        val old = link.connecting(Sock("old"))
        link.authenticated(old, epochA); link.presented(old)
        val fresh = Sock("fresh")
        val gen = link.connecting(fresh) // reconnect replaced the socket
        assertNull(link.inputSocket()) // never falls back to the old socket
        assertFalse(link.isCurrent(old))
        assertFalse(link.authenticated(old, epochA)) // late hello-ok from the old socket
        assertFalse(link.lost(old)) // late close from the old socket does not kill the new one
        assertTrue(link.isCurrent(gen))
        assertTrue(link.authenticated(gen, epochB))
        assertTrue(link.presented(gen))
        assertSame(fresh, link.inputSocket())
        assertEquals(epochB, link.currentEpoch)
    }

    @Test
    fun resetForgetsTheSocketAndItsCallbacks() {
        val link = InputLink<Sock>()
        val gen = link.connecting(Sock("s"))
        link.authenticated(gen, epochA); link.presented(gen)
        link.reset() // peer-left / stop
        assertNull(link.inputSocket())
        assertFalse(link.signalingUp)
        assertFalse(link.lost(gen))
        assertFalse(link.authenticated(gen, epochA))
    }

    @Test
    fun firstFrameRequestFromAnOlderEpochIsNotReused() {
        // The owner's bug: after a signaling re-hello the old request id was
        // reused, the new host session never armed input, and input did nothing.
        assertTrue(firstFrameRequestReusable("req", epochA, epochA, failed = false))
        assertFalse(firstFrameRequestReusable("req", epochA, epochB, failed = false))
        assertFalse(firstFrameRequestReusable(null, null, epochA, failed = false))
        assertFalse(firstFrameRequestReusable("req", epochA, epochA, failed = true))
    }
}

class SignalLivenessTest {
    private val live = SignalLiveness(heartbeatMs = 15_000, probeAfterMs = 4_000, pongTimeoutMs = 6_000)

    @Test
    fun regularHeartbeatAndPong() {
        live.opened(0)
        assertEquals(Action.NONE, live.check(10_000))
        assertEquals(Action.PING, live.check(15_000))
        assertEquals(Action.NONE, live.check(18_000))
        live.inbound(18_100) // pong
        assertEquals(Action.NONE, live.check(25_000))
        assertEquals(Action.PING, live.check(30_000))
    }

    @Test
    fun unansweredPingMeansDead() {
        live.opened(0)
        assertEquals(Action.PING, live.check(15_000))
        assertEquals(Action.NONE, live.check(21_000))
        assertEquals(Action.DEAD, live.check(21_001))
    }

    @Test
    fun inputOnAQuietSocketProbesEarly() {
        live.opened(0)
        live.inbound(1_000)
        live.input(6_000)
        assertEquals(Action.PING, live.check(6_000)) // 5 s without inbound while the user is typing
        assertEquals(Action.DEAD, live.check(12_500)) // half-open: found in ~6 s, not 40 s
    }

    @Test
    fun inputOnAChattySocketDoesNotProbe() {
        live.opened(0)
        live.inbound(5_000)
        live.input(6_000)
        assertEquals(Action.NONE, live.check(6_000))
        live.sent(7_000)
        assertEquals(Action.NONE, live.check(8_000)) // input marker was consumed; no stale probe later
    }
}
