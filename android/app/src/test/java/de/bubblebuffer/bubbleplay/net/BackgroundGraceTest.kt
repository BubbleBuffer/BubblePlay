package de.bubblebuffer.bubbleplay.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The phone app's background policy: media stops at once, a grace period, then a full disconnect. */
class BackgroundGraceTest {
    private val grace = 300_000L
    private var now = 1_000L

    /** Pending timers by id; cancelled or fired ones are removed. */
    private val timers = LinkedHashMap<Int, Pair<Long, () -> Unit>>()
    private var nextTimer = 0

    /** One media socket at a time, as the session keeps it. */
    private var sockets = 0
    private var opened = 0
    private val events = ArrayList<String>()

    private val host = object : BackgroundGrace.Host {
        override fun stopMedia() { sockets = 0; events += "stop" }
        override fun resumeMedia(backgroundMs: Long) { if (sockets == 0) { sockets = 1; opened++ }; events += "resume $backgroundMs" }
        override fun expired(backgroundMs: Long) { events += "expired $backgroundMs" }
    }

    private val policy = BackgroundGrace(grace, { now }, { delay, task ->
        val id = nextTimer++
        timers[id] = (now + delay) to task
        ({ timers.remove(id) })
    }, host)

    init { sockets = 1; opened = 1 }

    /** Advances the clock and fires due timers, like a handler would. */
    private fun advance(ms: Long) {
        now += ms
        timers.entries.filter { it.value.first <= now }.forEach { e -> timers.remove(e.key)?.second?.invoke() }
    }

    @Test fun backgroundStopsMediaAtOnceAndArmsOneTimer() {
        policy.background()
        assertEquals(0, sockets)
        assertEquals(listOf("stop"), events)
        assertEquals(1, timers.size)
        assertTrue(policy.inBackground)
    }

    @Test fun returnWithinGraceResumesMediaAndDropsTheTimer() {
        policy.background()
        advance(60_000)
        assertTrue(policy.foreground())
        assertEquals(listOf("stop", "resume 60000"), events)
        assertEquals(1, sockets)
        assertTrue(timers.isEmpty())
        advance(grace * 2)
        assertEquals("nothing fires later", 2, events.size)
    }

    @Test fun expiryDisconnectsOnceAndNothingResumesAfterwards() {
        policy.background()
        advance(grace)
        assertEquals(listOf("stop", "expired $grace"), events)
        assertFalse(policy.inBackground)
        assertTrue("a late return resumes nothing", policy.foreground())
        assertEquals(2, events.size)
        assertEquals(0, sockets)
    }

    @Test fun returnAfterTheGraceWithoutAFiredTimerStillDisconnects() {
        policy.background()
        now += grace + 5 // deep sleep: the uptime timer has not fired
        assertFalse(policy.foreground())
        assertEquals(listOf("stop", "expired ${grace + 5}"), events)
        assertTrue(timers.isEmpty())
        assertEquals(0, sockets)
    }

    @Test fun repeatedQuickSwitchesLeakNeitherSocketsNorTimers() {
        repeat(50) {
            policy.background()
            policy.background() // a second stop signal is ignored
            advance(200)
            policy.foreground()
            policy.foreground()
            assertTrue(timers.isEmpty())
            assertEquals(1, sockets)
        }
        assertEquals(51, opened)
        assertEquals(100, events.size)
    }

    @Test fun disconnectDuringTheGraceCancelsTheTimer() {
        policy.background()
        policy.cancel()
        assertTrue(timers.isEmpty())
        advance(grace * 2)
        assertEquals(listOf("stop"), events)
        assertFalse(policy.inBackground)
    }
}
