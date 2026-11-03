package de.bubblebuffer.bubbleplay.log

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.atomic.AtomicLong

class CtlPassThroughTest {
    private class Link : CtlLink {
        val sent = ArrayList<String>()
        var accept = true
        var throwOnSend = false
        override var bufferedBytes: Long = 0

        override fun send(json: String): Boolean {
            if (throwOnSend) throw IllegalStateException("boom")
            if (accept) sent.add(json)
            return accept
        }
    }

    private class Recorder : CtlPassThrough.Listener {
        val events = ArrayList<String>()
        override fun onOpened(pcId: String, peerCaps: List<String>) { events.add("open $pcId $peerCaps") }
        override fun onMessage(message: JSONObject) { events.add("msg ${message.getString("type")}/${message.optString("stream")}") }
        override fun onClosed() { events.add("closed") }
    }

    private fun msg(vararg pairs: Pair<String, Any>) = JSONObject().apply { pairs.forEach { (k, v) -> put(k, v) } }

    @Test fun refusesSendsWhileNotConnectedAndReportsNoBacklog() {
        val ctl = CtlPassThrough { 0L }
        assertFalse(ctl.sendRaw("{\"type\":\"x\"}"))
        assertEquals(0L, ctl.bufferedBytes())
        assertFalse(ctl.isOpen())
    }

    @Test fun withoutALogItCreatesNoUploaderAndNeverThrows() {
        BpLog.setCurrent(null)
        var created = 0
        val ctl = CtlPassThrough({ 0L }, { _, _, _ -> created++; error("must not be called") })
        val link = Link()
        ctl.attach("pc-1", link)
        ctl.onEndpoint(msg("type" to "logs-state", "v" to 1, "cursors" to JSONObject()))
        ctl.onEndpoint(msg("type" to "logs-ack", "v" to 1, "stream" to BpLog.SRC, "id" to 1))
        assertEquals(0, created)
        assertTrue(link.sent.isEmpty())
        assertTrue(ctl.sendRaw("{\"type\":\"x\"}"))
        ctl.detach()
    }

    @Test fun anUploaderFactoryThatThrowsDoesNotBreakTheConnection() = withTestLog { _, _ ->
        val ctl = CtlPassThrough({ 0L }, { _, _, _ -> error("boom") })
        val link = Link()
        ctl.attach("pc-1", link)
        assertTrue(ctl.isOpen())
        assertTrue(ctl.sendRaw("{\"type\":\"x\"}"))
    }

    @Test fun routesMessagesByKindAndStream() = withTestLog { _, _ ->
        val ctl = CtlPassThrough { 0L }
        val rec = Recorder()
        ctl.addListener(rec)
        ctl.attach("pc-1", Link())
        ctl.onEndpoint(msg("type" to "caps", "caps" to org.json.JSONArray(listOf("control-aead-v1", CtlPassThrough.CAP))))
        ctl.onEndpoint(msg("type" to "caps", "caps" to org.json.JSONArray(listOf("again")))) // only the first counts
        ctl.onEndpoint(msg("type" to "logs-state", "v" to 1, "cursors" to JSONObject()))
        ctl.onEndpoint(msg("type" to "logs-ack", "v" to 1, "stream" to BpLog.SRC, "id" to 1, "status" to "ok"))
        ctl.onEndpoint(msg("type" to "logs-ack", "v" to 1, "stream" to "app-android", "id" to 1, "status" to "ok"))
        ctl.onEndpoint(msg("type" to "feedback-ack", "v" to 1, "id" to "r", "status" to "ok"))
        ctl.onEndpoint(msg("type" to "host-capabilities"))
        ctl.detach()
        assertEquals(
            listOf(
                "open pc-1 [control-aead-v1, client-logs-v1]",
                "msg logs-state/",
                "msg logs-ack/app-android",
                "msg feedback-ack/",
                "closed",
            ),
            rec.events,
        )
        ctl.onEndpoint(msg("type" to "feedback-ack", "id" to "r")) // after the end: ignored
        assertEquals(5, rec.events.size)
    }

    @Test fun aListenerAddedLateGetsTheOpenConnectionAndItsLogsState() = withTestLog { _, _ ->
        val ctl = CtlPassThrough { 0L }
        ctl.attach("pc-9", Link())
        ctl.onEndpoint(msg("type" to "caps", "caps" to org.json.JSONArray(listOf(CtlPassThrough.CAP))))
        ctl.onEndpoint(msg("type" to "logs-state", "v" to 1, "cursors" to JSONObject()))
        val late = Recorder()
        ctl.addListener(late)
        ctl.addListener(late) // adding twice does not double the events
        assertEquals(listOf("open pc-9 [client-logs-v1]", "msg logs-state/"), late.events)
        ctl.removeListener(late)
        ctl.onEndpoint(msg("type" to "feedback-ack", "id" to "r"))
        assertEquals(2, late.events.size)
    }

    @Test fun aThrowingListenerDoesNotStopTheOthers() = withTestLog { _, _ ->
        val ctl = CtlPassThrough { 0L }
        val rec = Recorder()
        ctl.addListener(object : CtlPassThrough.Listener {
            override fun onMessage(message: JSONObject) = error("boom")
            override fun onOpened(pcId: String, peerCaps: List<String>) = error("boom")
            override fun onClosed() = error("boom")
        })
        ctl.addListener(rec)
        ctl.attach("pc-1", Link())
        ctl.onEndpoint(msg("type" to "caps", "caps" to org.json.JSONArray(listOf("a"))))
        ctl.onEndpoint(msg("type" to "feedback-ack", "id" to "r"))
        ctl.detach()
        assertEquals(listOf("open pc-1 [a]", "msg feedback-ack/", "closed"), rec.events)
    }

    @Test fun sendRawAndBufferFollowTheLinkAndNeverThrow() = withTestLog { _, _ ->
        val ctl = CtlPassThrough { 0L }
        val link = Link()
        ctl.attach("pc-1", link)
        link.bufferedBytes = 4096
        assertEquals(4096L, ctl.bufferedBytes())
        link.accept = false
        assertFalse(ctl.sendRaw("{\"type\":\"x\"}"))
        link.throwOnSend = true
        assertFalse(ctl.sendRaw("{\"type\":\"x\"}"))
        ctl.detach()
        assertFalse(ctl.sendRaw("{\"type\":\"x\"}"))
        assertEquals(0L, ctl.bufferedBytes())
    }

    @Test fun sinceLastInputUsesTheOwnersClock() {
        val now = AtomicLong(1_000)
        val ctl = CtlPassThrough { now.get() }
        assertEquals(Long.MAX_VALUE, ctl.sinceLastInputMs())
        ctl.inputSent()
        now.addAndGet(120)
        assertEquals(120L, ctl.sinceLastInputMs())
        ctl.inputSent()
        assertEquals(0L, ctl.sinceLastInputMs())
    }

    @Test fun eachAttachGetsItsOwnUploaderAndTheOldOneIsClosed() = withTestLog { log, _ ->
        val made = ArrayList<LogUploader>()
        val ctl = CtlPassThrough({ 0L }, { transport, bpLog, pcId -> LogUploader(transport, bpLog, pcId).also { made.add(it) } })
        ctl.attach("pc-1", Link())
        ctl.attach("pc-1", Link()) // a new connection replaces the old one without a detach in between
        ctl.detach()
        assertEquals(2, made.size)
    }
}
