package de.bubblebuffer.bubbleplay.e2e

import de.bubblebuffer.bubbleplay.Fixtures
import de.bubblebuffer.bubbleplay.auth.AuthJson
import de.bubblebuffer.bubbleplay.auth.CtlChannel
import de.bubblebuffer.bubbleplay.auth.pcKey
import de.bubblebuffer.bubbleplay.auth.viewerKey
import de.bubblebuffer.bubbleplay.log.BpLog
import de.bubblebuffer.bubbleplay.log.CtlPassThrough
import de.bubblebuffer.bubbleplay.log.LogLevel
import de.bubblebuffer.bubbleplay.log.withTestLog
import de.bubblebuffer.bubbleplay.net.SessionWire
import okhttp3.OkHttpClient
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * The Android end of docs/architecture/client-log-upload.md against the real relay and a scripted PC that
 * plays the host side of fixtures/client-log-upload-contract.json: the real [SessionWire] and the real
 * [CtlPassThrough] with its [de.bubblebuffer.bubbleplay.log.LogUploader] (real threads, real clock), wired
 * by [ViewerWire] exactly as Session wires them (attach on Authenticated, endpoint messages in, detach on end).
 */
class LogUploadE2eTest {
    @get:Rule val relay = RelayServer()

    private val http = OkHttpClient.Builder().readTimeout(0, TimeUnit.MILLISECONDS).build()
    private val closers = mutableListOf<AutoCloseable>()
    private val contract = Fixtures.json("client-log-upload-contract.json")

    @After fun tearDown() {
        closers.asReversed().forEach { runCatching { it.close() } }
        http.dispatcher.executorService.shutdown()
        http.connectionPool.evictAll()
    }

    private fun <T : AutoCloseable> T.closing(): T = also { closers.add(it) }

    private fun fixture(name: String): JSONObject {
        val all = contract.getJSONArray("messages")
        for (i in 0 until all.length()) {
            if (all.getJSONObject(i).getString("name") == name) return JSONObject(all.getJSONObject(i).getJSONObject("json").toString())
        }
        error("no fixture $name")
    }

    private class Listener : CtlPassThrough.Listener {
        val messages = LinkedBlockingQueue<JSONObject>()
        val opened = LinkedBlockingQueue<Pair<String, List<String>>>()
        val closed = LinkedBlockingQueue<Boolean>()
        override fun onOpened(pcId: String, peerCaps: List<String>) { opened.add(pcId to peerCaps) }
        override fun onMessage(message: JSONObject) { messages.add(message) }
        override fun onClosed() { closed.add(true) }
        fun next(): JSONObject = messages.poll(10, TimeUnit.SECONDS) ?: error("listener: no message within 10 s")
    }

    private class Conn(val viewer: ViewerWire, val handshake: PcHandshake, val ctlCaps: List<String>)

    private inner class Rig {
        val viewerKey = viewerKey()
        val pc = FakePc(relay, http, pcKey()).closing()
        val ctl = CtlPassThrough { System.nanoTime() / 1_000_000 }

        init {
            pc.register()
            pc.admit(viewerKey.id)
        }

        /** Connects a viewer, runs the handshake, reads the viewer's caps and sends the PC's caps (with the capability). */
        fun connect(pcAdvertises: Boolean = true): Conn {
            val viewer = ViewerWire(relay, http, viewerKey, pc.id, pc.key.publicKey, "E2E Pixel", ctl).closing()
            viewer.start()
            val peer = pc.expectPeerJoined()
            viewer.expect<SessionWire.Event.Admitted>()
            val handshake = pc.answerAuthHello(peer)
            handshake.awaitAuthFinish()
            viewer.expect<SessionWire.Event.Authenticated>()
            val caps = AuthJson.parseObject(handshake.receiveCtl().text())
            @Suppress("UNCHECKED_CAST")
            val sent = caps["caps"] as List<String>
            val pcCaps = if (pcAdvertises) listOf(CtlChannel.CAP, CtlPassThrough.CAP) else listOf(CtlChannel.CAP)
            handshake.sendCtlJson(linkedMapOf("type" to "caps", "caps" to pcCaps))
            viewer.expect<SessionWire.Event.Endpoint>()
            return Conn(viewer, handshake, sent)
        }

        fun send(conn: Conn, message: JSONObject) =
            conn.handshake.sendCtl(message.getString("type"), message.toString().toByteArray(Charsets.UTF_8))

        /** The fixture's logs-state with [cursors] for the Android core stream. */
        fun state(conn: Conn, cursor: String? = null) {
            val state = fixture("logs-state")
            val cursors = JSONObject()
            if (cursor != null) cursors.put(BpLog.SRC, cursor)
            send(conn, state.put("cursors", cursors))
        }

        fun ack(conn: Conn, batch: JSONObject, stream: String = BpLog.SRC, cursor: String = batch.getString("to")) =
            send(conn, fixture("logs-ack-ok").put("stream", stream).put("id", batch.getLong("id")).put("cursor", cursor))

        /** The next ctl message from the viewer, which must be a logs-batch. */
        fun batch(conn: Conn): JSONObject {
            val message = conn.handshake.receiveCtl()
            assertEquals("logs-batch", message.kind)
            return JSONObject(message.text())
        }
    }

    private fun write(log: BpLog, pcId: String?, count: Int, prefix: String = "line") {
        repeat(count) { log.log(LogLevel.INFO, "E2e", "$prefix $it", pcId) }
        log.flush()
    }

    private fun keysOf(o: JSONObject): Set<String> = o.keys().asSequence().toSet()

    // Only the lines this test wrote: the core logs its own handshake lines too (for example "auth-host verified").
    private fun lines(batch: JSONObject) = (0 until batch.getJSONArray("lines").length())
        .map { batch.getJSONArray("lines").getString(it) }
        .filter { JSONObject(it).getString("target") == "E2e" }

    @Test fun capabilityIsAdvertisedInHelloAndCaps() = withTestLog { _, _ ->
        val rig = Rig()
        val conn = rig.connect()
        assertEquals(listOf(CtlChannel.CAP, CtlPassThrough.CAP), conn.handshake.viewerCaps)
        assertEquals(listOf(CtlChannel.CAP, CtlPassThrough.CAP), conn.ctlCaps)
        assertEquals(contract.getString("capability"), CtlPassThrough.CAP)
    }

    @Test fun historyAndLiveBatchesRoundTripAgainstTheContractFixture() = withTestLog { log, _ ->
        val rig = Rig()
        write(log, rig.pc.id, 2, "history")
        write(log, null, 1, "no-pc")
        write(log, "some-other-pc", 1, "other-pc") // not related to this PC: never uploaded
        val conn = rig.connect()
        rig.state(conn)

        val first = rig.batch(conn)
        assertEquals(keysOf(fixture("logs-batch")), keysOf(first))
        assertEquals(1, first.getInt("v"))
        assertEquals(BpLog.SRC, first.getString("stream"))
        assertEquals(1L, first.getLong("id"))
        assertEquals("", first.getString("from"))
        assertFalse(first.getBoolean("live"))
        val sent = lines(first).map(::JSONObject)
        assertEquals(listOf("history 0", "history 1", "no-pc 0"), sent.map { it.getString("msg") })
        assertTrue(sent.all { it.getString("src") == BpLog.SRC })
        assertTrue("to is the cursor of the last line", first.getString("to").isNotEmpty())

        rig.ack(conn, first)
        write(log, rig.pc.id, 2, "live")
        val second = rig.batch(conn)
        assertEquals(2L, second.getLong("id"))
        assertEquals("the uploader advanced to the acked cursor", first.getString("to"), second.getString("from"))
        assertTrue(second.getBoolean("live"))
        assertEquals(listOf("live 0", "live 1"), lines(second).map { JSONObject(it).getString("msg") })

        rig.ack(conn, second)
        write(log, rig.pc.id, 1, "more")
        val third = rig.batch(conn)
        assertEquals(second.getString("to"), third.getString("from"))
        assertEquals(3L, third.getLong("id"))
    }

    @Test fun anExistingHostCursorSkipsWhatItAlreadyHas() = withTestLog { log, _ ->
        val rig = Rig()
        write(log, rig.pc.id, 2, "old")
        val cursor = log.readAfter("", rig.pc.id, 10, 64 * 1024).last().cursor
        write(log, rig.pc.id, 1, "new")
        val conn = rig.connect()
        rig.state(conn, cursor)
        val batch = rig.batch(conn)
        assertEquals(cursor, batch.getString("from"))
        assertEquals(listOf("new 0"), lines(batch).map { JSONObject(it).getString("msg") })
    }

    @Test fun otherStreamAcksAndFeedbackAcksGoToTheListenerNotTheUploader() = withTestLog { log, _ ->
        val rig = Rig()
        val listener = Listener()
        rig.ctl.addListener(listener)
        write(log, rig.pc.id, 1, "first")
        val conn = rig.connect()
        val opened = listener.opened.poll(10, TimeUnit.SECONDS)
        assertNotNull(opened)
        assertEquals(rig.pc.id, opened!!.first)
        assertTrue(CtlPassThrough.CAP in opened.second)

        rig.state(conn)
        assertEquals("logs-state", listener.next().getString("type")) // carries the cursors of every stream
        val first = rig.batch(conn)
        assertEquals(1L, first.getLong("id"))
        assertTrue("the batch of the core stream does not reach the listener", listener.messages.isEmpty())

        // An ack of the Dart stream carrying the in-flight id must not advance the uploader.
        write(log, rig.pc.id, 1, "second")
        rig.ack(conn, first, stream = "app-android", cursor = "zzz")
        val foreign = listener.next()
        assertEquals("logs-ack", foreign.getString("type"))
        assertEquals("app-android", foreign.getString("stream"))
        rig.pc.expectQuiet(1200) // still waiting for its own ack: no second batch

        rig.send(conn, fixture("feedback-ack-ok"))
        val feedback = listener.next()
        assertEquals("feedback-ack", feedback.getString("type"))
        assertEquals(fixture("feedback-ack-ok").getString("id"), feedback.getString("id"))

        rig.ack(conn, first) // the right stream: now it advances
        val second = rig.batch(conn)
        assertEquals(first.getString("to"), second.getString("from"))
        assertTrue("core acks are not offered to the listener", listener.messages.isEmpty())
    }

    @Test fun sendRawReachesThePcAndIsRefusedOnceTheConnectionEnded() = withTestLog { _, _ ->
        val rig = Rig()
        val listener = Listener()
        rig.ctl.addListener(listener)
        assertFalse("not authenticated yet", rig.ctl.sendRaw(fixture("feedback-begin").toString()))
        val conn = rig.connect()
        assertTrue(rig.ctl.sendRaw(fixture("feedback-begin").toString()))
        val ctl = conn.handshake.receiveCtl()
        assertEquals("feedback-begin", ctl.kind)
        assertEquals(fixture("feedback-begin").getString("id"), JSONObject(ctl.text()).getString("id"))
        assertEquals(0L, rig.ctl.bufferedBytes())

        conn.viewer.close()
        assertNotNull(listener.closed.poll(10, TimeUnit.SECONDS))
        assertFalse(rig.ctl.sendRaw(fixture("feedback-begin").toString()))
    }

    @Test fun nothingIsUploadedWithin250MsOfInput() = withTestLog { log, _ ->
        val rig = Rig()
        write(log, rig.pc.id, 2)
        val conn = rig.connect()
        rig.ctl.inputSent()
        rig.state(conn)
        var lastInput = 0L
        repeat(10) { // one second of "typing": a stamp every 100 ms
            lastInput = System.nanoTime() / 1_000_000
            rig.ctl.inputSent()
            rig.pc.expectQuiet(100)
        }
        val batch = rig.batch(conn)
        val waited = System.nanoTime() / 1_000_000 - lastInput
        assertTrue("batch came $waited ms after the last input", waited >= 240)
        assertEquals(2, lines(batch).size)
    }

    @Test fun nothingIsUploadedWhileTheSendBufferIsOver16KiB() = withTestLog { log, _ ->
        val rig = Rig()
        write(log, rig.pc.id, 2)
        val conn = rig.connect()
        conn.viewer.bufferedBytes = 20_000
        rig.state(conn)
        rig.pc.expectQuiet(900)
        conn.viewer.bufferedBytes = 16 * 1024
        assertEquals(2, lines(rig.batch(conn)).size)
    }

    @Test fun everyReconnectGetsAFreshUploader() = withTestLog { log, _ ->
        val rig = Rig()
        val listener = Listener()
        rig.ctl.addListener(listener)
        write(log, rig.pc.id, 3)
        val first = rig.connect()
        rig.state(first)
        val batch1 = rig.batch(first) // never acked
        assertEquals(1L, batch1.getLong("id"))

        first.viewer.close()
        assertNotNull(listener.closed.poll(10, TimeUnit.SECONDS))

        val second = rig.connect()
        rig.state(second)
        val batch2 = rig.batch(second)
        assertEquals("a new uploader numbers its batches from 1", 1L, batch2.getLong("id"))
        assertEquals("and resends what the host never acked", lines(batch1), lines(batch2))
        assertEquals(2, listener.opened.size)
    }

    @Test fun aHostWithoutTheCapabilityIsToldNothingAndSendsNoLogs() = withTestLog { log, _ ->
        val rig = Rig()
        write(log, rig.pc.id, 2)
        val listener = Listener()
        rig.ctl.addListener(listener)
        val conn = rig.connect(pcAdvertises = false)
        val opened = listener.opened.poll(10, TimeUnit.SECONDS)
        assertNotNull(opened)
        assertFalse(CtlPassThrough.CAP in opened!!.second) // the Dart side decides from this
        rig.pc.expectQuiet(900) // no logs-state arrives, so no batch leaves
        assertEquals(CtlPassThrough.CAP, conn.ctlCaps.last())
    }
}
