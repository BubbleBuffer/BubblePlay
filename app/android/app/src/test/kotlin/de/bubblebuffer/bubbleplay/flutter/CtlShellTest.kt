package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.log.CtlLink
import de.bubblebuffer.bubbleplay.log.CtlPassThrough
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The shell side of the ctl bridge, over a real [CtlPassThrough] with a fake link. */
class CtlShellTest {
    private class FakeLink : CtlLink {
        val sent = mutableListOf<String>()
        var accept = true
        override var bufferedBytes: Long = 0
        override fun send(json: String): Boolean {
            if (accept) sent.add(json)
            return accept
        }
    }

    private val events = mutableListOf<Pair<String, String>>()
    private val shell = CtlShell { kind, json -> events.add(kind to json) }
    private var clock = 1_000L

    private fun passThrough() = CtlPassThrough({ clock })

    private fun open(ctl: CtlPassThrough, link: FakeLink, pc: String = "pc-1", caps: List<String> = listOf("client-logs-v1")) {
        ctl.attach(pc, link)
        ctl.onEndpoint(JSONObject().put("type", "caps").put("caps", org.json.JSONArray(caps)))
    }

    @Test
    fun openedMessageAndClosedAreForwarded() {
        val ctl = passThrough()
        shell.attach(ctl)
        val link = FakeLink()
        open(ctl, link)
        ctl.onEndpoint(JSONObject().put("type", "logs-state").put("cursors", JSONObject()))
        ctl.onEndpoint(JSONObject().put("type", "feedback-ack").put("id", "r1"))
        ctl.detach()

        assertEquals(listOf("opened", "message", "message", "closed"), events.map { it.first })
        val opened = JSONObject(events[0].second)
        assertEquals("pc-1", opened.getString("pcId"))
        assertEquals("client-logs-v1", opened.getJSONArray("peerCaps").getString(0))
        assertEquals("logs-state", JSONObject(events[1].second).getString("type"))
        assertEquals("feedback-ack", JSONObject(events[2].second).getString("type"))
        assertEquals("{}", events[3].second)
    }

    @Test
    fun sendDelegatesAndReportsRefusal() {
        val ctl = passThrough()
        assertFalse(shell.send("{\"type\":\"x\"}"))
        shell.attach(ctl)
        assertFalse("no connection yet", shell.send("{\"type\":\"x\"}"))
        val link = FakeLink()
        open(ctl, link)
        assertTrue(shell.send("{\"type\":\"logs\"}"))
        assertEquals(listOf("{\"type\":\"logs\"}"), link.sent)
        link.accept = false
        assertFalse(shell.send("{\"type\":\"logs2\"}"))
    }

    @Test
    fun stateReflectsTheConnection() {
        val ctl = passThrough()
        shell.attach(ctl)
        var state = shell.state()
        assertFalse(state.open)
        assertEquals("", state.pcId)
        assertTrue(state.peerCaps.isEmpty())

        val link = FakeLink()
        open(ctl, link)
        link.bufferedBytes = 1234
        state = shell.state()
        assertTrue(state.open)
        assertEquals(1234L, state.bufferedBytes)
        assertEquals("pc-1", state.pcId)
        assertEquals(listOf("client-logs-v1"), state.peerCaps)
        assertEquals(Long.MAX_VALUE, state.sinceLastInputMs)

        ctl.inputSent()
        clock += 300
        assertEquals(300L, shell.state().sinceLastInputMs)

        ctl.detach()
        assertFalse(shell.state().open)
    }

    @Test
    fun detachOnSessionEndStopsForwardingAndClosesDart() {
        val ctl = passThrough()
        shell.attach(ctl)
        open(ctl, FakeLink())
        events.clear()

        shell.attach(null)
        assertEquals(listOf("closed"), events.map { it.first })
        events.clear()
        // The old session is no longer followed, and sending through it is refused.
        ctl.onEndpoint(JSONObject().put("type", "feedback-ack"))
        assertTrue(events.isEmpty())
        assertFalse(shell.send("{\"type\":\"x\"}"))
        assertFalse(shell.state().open)
    }

    @Test
    fun aReplacedSessionSwitchesOver() {
        val first = passThrough()
        shell.attach(first)
        open(first, FakeLink(), pc = "pc-1")
        events.clear()

        val second = passThrough()
        val link = FakeLink()
        shell.attach(second)
        assertEquals(listOf("closed"), events.map { it.first })
        open(second, link, pc = "pc-2")
        assertEquals("opened", events.last().first)
        assertEquals("pc-2", JSONObject(events.last().second).getString("pcId"))
        assertTrue(shell.send("{\"type\":\"x\"}"))
        assertEquals(1, link.sent.size)

        events.clear()
        first.detach()
        assertTrue("the replaced session is silent", events.isEmpty())
    }

    @Test
    fun attachingToAnOpenConnectionAnnouncesItAtOnce() {
        val ctl = passThrough()
        open(ctl, FakeLink())
        shell.attach(ctl)
        assertEquals(listOf("opened"), events.map { it.first })
    }
}
